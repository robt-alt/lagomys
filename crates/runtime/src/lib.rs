#![forbid(unsafe_code)]

mod expression;

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use ops_actions::{ActionContext, CancelSignal};
use ops_spec::{
    Argument, Assertion, Error, ErrorCode, LoopContext, ObservedValue, RepeatReason, RepeatResult,
    RunResult, Status, Task, Value, MAX_DATA_BYTES,
};
use ops_validate::{CheckedInstruction, CheckedOperand, CheckedValue, Step, ValidatedPlan};
use serde::Serialize;
use serde_json::json;

use crate::expression::Evaluator;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_STORED_BYTES: usize = 16 * 1024 * 1024;

pub struct ExecuteOptions {
    pub started: Instant,
    pub cancel: Option<CancelSignal>,
}

pub fn execute(plan: &ValidatedPlan<'_>) -> RunResult {
    execute_from(plan, Instant::now())
}

pub fn execute_from(plan: &ValidatedPlan<'_>, started: Instant) -> RunResult {
    execute_with_options(
        plan,
        ExecuteOptions {
            started,
            cancel: None,
        },
    )
}

pub fn execute_with_options(plan: &ValidatedPlan<'_>, options: ExecuteOptions) -> RunResult {
    let ExecuteOptions { started, cancel } = options;
    let mut result = RunResult::new();
    let Some(deadline) = started.checked_add(Duration::from_secs(plan.binding().budget.seconds))
    else {
        result.fail(Error::new(
            ErrorCode::EBudget,
            "Execution time budget is out of range",
        ));
        return result;
    };
    let mut run = Execution {
        plan,
        deadline,
        cancel,
        result,
        values: BTreeMap::new(),
        stored_bytes: 0,
        payload_bytes: 0,
        evidence_names: Vec::new(),
        declared_evidence: BTreeSet::new(),
    };
    if let Err(error) = run.check_cancelled().and_then(|()| run.block(plan.steps())) {
        if let Some(path) = &error.path {
            run.result
                .evidence
                .push(json!({"line": error.line, "path": path}));
        }
        run.result.fail(error);
    }
    if run.result.status != Status::Success {
        for name in run.evidence_names {
            if let Some(entry) = run.values.remove(name) {
                run.result.evidence.push(entry.value);
            }
        }
    }
    run.result.usage.seconds = started.elapsed().as_secs_f64();
    run.result
}

struct Stored {
    value: Value,
    task: Option<Task>,
    bytes: usize,
    collector: bool,
}

impl Stored {
    fn value(value: Value) -> Self {
        Self {
            value,
            task: None,
            bytes: 0,
            collector: false,
        }
    }
}

struct Execution<'a, 'plan> {
    plan: &'a ValidatedPlan<'plan>,
    deadline: Instant,
    cancel: Option<CancelSignal>,
    result: RunResult,
    values: BTreeMap<String, Stored>,
    stored_bytes: usize,
    payload_bytes: usize,
    evidence_names: Vec<&'a str>,
    declared_evidence: BTreeSet<&'a str>,
}

#[derive(PartialEq, Eq)]
enum Flow {
    Next,
    Break,
    Continue,
    Stop,
}

impl<'a> Execution<'a, '_> {
    fn block(&mut self, steps: &'a [Step]) -> Result<Flow, Error> {
        for step in steps {
            let flow = self.step(step).map_err(|error| {
                if error.line.is_some() {
                    error
                } else {
                    error.at(step.line)
                }
            })?;
            if flow != Flow::Next {
                return Ok(flow);
            }
        }
        Ok(Flow::Next)
    }

    fn step(&mut self, step: &'a Step) -> Result<Flow, Error> {
        if !matches!(step.instruction, CheckedInstruction::Delay(_)) {
            self.check_active()?;
        } else {
            self.check_cancelled()?;
        }
        let result = match &step.instruction {
            CheckedInstruction::Call { name, arguments } => {
                self.call(name, arguments, step.line)?
            }
            CheckedInstruction::Where { source, predicate } => {
                let source = into_list(self.value(source)?)?;
                let mut selected = Vec::new();
                for item in source {
                    self.check_active()?;
                    if Evaluator::new(&self.values, Some(&item)).evaluate(predicate)? {
                        selected.push(item);
                    }
                }
                Stored::value(Value::Array(selected))
            }
            CheckedInstruction::Take { source, count } => {
                let mut list = into_list(self.value(source)?)?;
                list.truncate(usize::try_from(*count).unwrap_or(usize::MAX));
                Stored::value(Value::Array(list))
            }
            CheckedInstruction::Wait { source, seconds } => {
                if let Some(task) = self.task(source) {
                    match self.wait(task, *seconds, step.line)? {
                        Some(value) => value,
                        None => return Ok(Flow::Stop),
                    }
                } else {
                    Stored::value(self.value(source)?)
                }
            }
            CheckedInstruction::Assert { predicate, text } => {
                let passed = Evaluator::new(&self.values, None).evaluate(predicate)?;
                let assertion = Assertion {
                    expr: text.clone(),
                    passed,
                };
                self.account(&assertion)?;
                self.result.asserts.push(assertion);
                if !passed {
                    self.result.status = Status::Failed;
                    let evidence = json!({"line": step.line, "expr": text, "passed": false});
                    self.account(&evidence)?;
                    self.result.evidence.push(evidence);
                }
                Stored::value(json!({"expr": text, "passed": passed}))
            }
            CheckedInstruction::Out(operands) => {
                self.output(operands, step.destination.is_some())?
            }
            CheckedInstruction::Return(operands) => {
                self.output(operands, false)?;
                return Ok(Flow::Stop);
            }
            CheckedInstruction::Repeat {
                count,
                context,
                body,
                introduced,
            } => {
                for name in introduced {
                    self.discard(name);
                }
                let outcome = self.repeat(*count, context.as_deref(), body);
                if let Some(name) = context {
                    self.discard(name);
                }
                let Some(result) = outcome? else {
                    return Ok(Flow::Stop);
                };
                if step.destination.is_none() {
                    return Ok(Flow::Next);
                }
                Stored::value(json!(result))
            }
            CheckedInstruction::If {
                predicate,
                then_body,
                else_body,
                introduced,
            } => {
                let matched = Evaluator::new(&self.values, None).evaluate(predicate)?;
                for name in introduced {
                    self.discard(name);
                }
                return self.block(if matched { then_body } else { else_body });
            }
            CheckedInstruction::Break => return Ok(Flow::Break),
            CheckedInstruction::Continue => return Ok(Flow::Continue),
            CheckedInstruction::Delay(milliseconds) => {
                let remaining = self.deadline.saturating_duration_since(Instant::now());
                self.sleep(Duration::from_millis(*milliseconds).min(remaining))?;
                if Instant::now() >= self.deadline {
                    if self.result.status != Status::Failed {
                        self.result.status = Status::Timeout;
                    }
                    return Ok(Flow::Stop);
                }
                return Ok(Flow::Next);
            }
            CheckedInstruction::NewCollector => Stored {
                collector: true,
                ..Stored::value(Value::Array(Vec::new()))
            },
            CheckedInstruction::Collect { name, value } => {
                let value = self.value(value)?;
                self.append(name, value)?;
                return Ok(Flow::Next);
            }
            CheckedInstruction::Evidence(names) => {
                for name in names {
                    if self.declared_evidence.insert(name) {
                        self.evidence_names.push(name);
                    }
                }
                return Ok(Flow::Next);
            }
        };
        if let Some(destination) = &step.destination {
            self.save(destination, result)?;
        }
        Ok(Flow::Next)
    }

    fn repeat(
        &mut self,
        count: u64,
        context: Option<&str>,
        body: &'a [Step],
    ) -> Result<Option<RepeatResult>, Error> {
        for index in 0..count {
            self.check_active()?;
            if let Some(name) = context {
                let current = LoopContext {
                    index,
                    last: index + 1 == count,
                };
                self.save(name, Stored::value(json!(current)))?;
            }
            match self.block(body)? {
                Flow::Next | Flow::Continue => {}
                Flow::Break => {
                    return Ok(Some(RepeatResult {
                        iterations: index + 1,
                        reason: RepeatReason::Break,
                    }))
                }
                Flow::Stop => return Ok(None),
            }
        }
        Ok(Some(RepeatResult {
            iterations: count,
            reason: RepeatReason::Completed,
        }))
    }

    fn output(&mut self, operands: &[CheckedOperand], assigned: bool) -> Result<Stored, Error> {
        let mut values = Vec::new();
        for operand in operands {
            let output = ObservedValue {
                expr: operand.text.clone(),
                value: self.value(operand)?,
            };
            self.account(&output)?;
            if assigned {
                values.push(output.value.clone());
            }
            self.result.values.push(output);
        }
        Ok(Stored::value(Value::Array(values)))
    }

    fn call(
        &mut self,
        name: &str,
        operands: &[Option<CheckedOperand>],
        line: usize,
    ) -> Result<Stored, Error> {
        let mut arguments = Vec::with_capacity(operands.len());
        for operand in operands {
            let Some(operand) = operand else {
                arguments.push(Argument::Omitted);
                continue;
            };
            let argument = match operand.kind {
                CheckedValue::Target(index) => {
                    let target = self.plan.binding().targets.get(index).ok_or_else(|| {
                        Error::new(
                            ErrorCode::ETargetMissing,
                            format!("Missing bound target @{index}"),
                        )
                    })?;
                    Argument::Target(target.clone())
                }
                _ => Argument::Value(self.value(operand)?),
            };
            arguments.push(argument);
        }
        let result = self.invoke(name, &arguments, self.deadline, line)?;
        let (signature, module) = self.plan.registry().signature(name).ok_or_else(|| {
            Error::new(ErrorCode::EUnknownAction, format!("Unknown action {name}"))
        })?;
        let Some(poll) = &signature.poll else {
            return Ok(Stored::value(result));
        };
        let id = result
            .get("id")
            .filter(|id| id.is_string() || id.is_number())
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::EActionFailed,
                    format!("Action {name} returned an invalid task identifier"),
                )
                .action(name)
            })?;
        let task = Task {
            id,
            module: module.unwrap_or_default().to_owned(),
            poll: poll.clone(),
        };
        Ok(Stored {
            value: json!(task),
            task: Some(task),
            bytes: 0,
            collector: false,
        })
    }

    fn wait(&mut self, task: Task, seconds: u64, line: usize) -> Result<Option<Stored>, Error> {
        let limit = Instant::now()
            .checked_add(Duration::from_secs(seconds))
            .map_or(self.deadline, |deadline| deadline.min(self.deadline));
        let argument = Argument::Value(task.id.clone());
        let status = loop {
            if let Err(error) = self.check_cancelled() {
                self.result.task = Some(task);
                return Err(error);
            }
            if Instant::now() >= limit {
                break Status::Timeout;
            }
            let observation =
                match self.invoke(&task.poll, std::slice::from_ref(&argument), limit, line) {
                    Err(error) if error.code == ErrorCode::EBudget && Instant::now() >= limit => {
                        break Status::Timeout;
                    }
                    Err(error) => {
                        self.result.task = Some(task);
                        return Err(error);
                    }
                    Ok(observation) => observation,
                };
            if Instant::now() >= limit {
                break Status::Timeout;
            }
            match observation.get("state").and_then(Value::as_str) {
                Some("success") => {
                    let value = observation
                        .get("result")
                        .ok_or_else(|| invalid_poll(&task))?;
                    return Ok(Some(Stored::value(value.clone())));
                }
                Some("failed") => {
                    if let Some(value) = observation.get("result") {
                        self.account(value)?;
                        self.result.evidence.push(value.clone());
                    }
                    let module_code = observation
                        .get("result")
                        .and_then(|result| result.get("code"))
                        .cloned();
                    return Err(
                        Error::new(ErrorCode::EActionFailed, "Asynchronous task failed")
                            .action(&task.poll)
                            .module_code(module_code),
                    );
                }
                Some("pending") => break Status::Pending,
                Some("running") => {
                    let remaining = limit.saturating_duration_since(Instant::now());
                    if let Err(error) = self.sleep(POLL_INTERVAL.min(remaining)) {
                        self.result.task = Some(task);
                        return Err(error);
                    }
                }
                _ => return Err(invalid_poll(&task)),
            }
        };
        if self.result.status != Status::Failed {
            self.result.status = status;
        }
        self.result.task = Some(task);
        Ok(None)
    }

    fn task(&self, operand: &CheckedOperand) -> Option<Task> {
        if let CheckedValue::Result(path) = &operand.kind {
            if path.segments.is_empty() {
                return self
                    .values
                    .get(&path.root)
                    .and_then(|value| value.task.clone());
            }
        }
        None
    }

    fn value(&self, operand: &CheckedOperand) -> Result<Value, Error> {
        Evaluator::new(&self.values, None).value(operand)
    }

    fn invoke(
        &mut self,
        name: &str,
        arguments: &[Argument],
        deadline: Instant,
        line: usize,
    ) -> Result<Value, Error> {
        let sequence = self.result.usage.actions.saturating_add(1);
        let context = ActionContext::new(
            deadline,
            MAX_DATA_BYTES,
            sequence,
            line,
            self.cancel.clone(),
        );
        context.check_active()?;
        if self.result.usage.actions >= self.plan.binding().budget.actions {
            return Err(Error::new(
                ErrorCode::EBudget,
                "Action count budget exhausted",
            ));
        }
        self.result.usage.actions = sequence;
        let result = self.plan.registry().invoke(name, arguments, &context);
        self.check_cancelled()?;
        let result = result?;
        context.remaining()?;
        Ok(result)
    }

    fn sleep(&self, duration: Duration) -> Result<(), Error> {
        if let Some(cancel) = &self.cancel {
            if cancel.wait_timeout(duration) {
                return Err(cancelled());
            }
        } else {
            std::thread::sleep(duration);
        }
        Ok(())
    }

    fn check_cancelled(&self) -> Result<(), Error> {
        if self.cancel.as_ref().is_some_and(CancelSignal::is_cancelled) {
            return Err(cancelled());
        }
        Ok(())
    }

    fn check_active(&self) -> Result<(), Error> {
        self.check_cancelled()?;
        if Instant::now() < self.deadline {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::EBudget, "Execution deadline reached"))
        }
    }

    fn account(&mut self, payload: &impl Serialize) -> Result<(), Error> {
        let bytes = json_size(payload)?;
        self.payload_bytes = self.payload_bytes.saturating_add(bytes);
        if self.payload_bytes > self.plan.binding().budget.output_bytes {
            return Err(Error::new(
                ErrorCode::EOutputLimit,
                "Execution result exceeded the output byte budget",
            ));
        }
        Ok(())
    }

    fn save(&mut self, destination: &str, mut value: Stored) -> Result<(), Error> {
        let previous = self.values.get(destination).map_or(0, |entry| entry.bytes);
        value.bytes = json_size(&value.value)?;
        let total = self
            .stored_bytes
            .saturating_sub(previous)
            .saturating_add(value.bytes);
        check_storage(total)?;
        self.stored_bytes = total;
        self.values.insert(destination.to_owned(), value);
        Ok(())
    }

    fn discard(&mut self, name: &str) {
        if let Some(entry) = self.values.remove(name) {
            self.stored_bytes -= entry.bytes;
        }
    }

    fn append(&mut self, name: &str, value: Value) -> Result<(), Error> {
        let entry = self.values.get_mut(name).ok_or_else(|| {
            Error::new(
                ErrorCode::ERef,
                format!("Collector {name} has not been created"),
            )
        })?;
        let elements = entry
            .value
            .as_array_mut()
            .filter(|_| entry.collector)
            .ok_or_else(|| Error::new(ErrorCode::EArgType, format!("{name} is not a collector")))?;
        let added = json_size(&value)?.saturating_add(usize::from(!elements.is_empty()));
        let total = self.stored_bytes.saturating_add(added);
        check_storage(total)?;
        elements.push(value);
        entry.bytes += added;
        self.stored_bytes = total;
        Ok(())
    }
}

fn cancelled() -> Error {
    Error::new(ErrorCode::ECancelled, "Execution cancelled")
}

fn check_storage(total: usize) -> Result<(), Error> {
    if total > MAX_STORED_BYTES {
        return Err(Error::new(
            ErrorCode::EBudget,
            "Stored values exceed the runtime data limit",
        ));
    }
    Ok(())
}

fn into_list(value: Value) -> Result<Vec<Value>, Error> {
    if let Value::Array(values) = value {
        Ok(values)
    } else {
        Err(Error::new(
            ErrorCode::EArgType,
            "where and take require a list",
        ))
    }
}

fn invalid_poll(task: &Task) -> Error {
    Error::new(
        ErrorCode::EActionFailed,
        format!("Invalid observation from {}", task.poll),
    )
    .action(&task.poll)
}

fn json_size(value: &impl Serialize) -> Result<usize, Error> {
    struct Counter(usize);

    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| Error::new(ErrorCode::EIo, "Unable to encode an execution value"))?;
    Ok(counter.0)
}
