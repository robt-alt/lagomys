#![forbid(unsafe_code)]

mod expression;

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use ops_actions::ActionContext;
use ops_spec::{
    Argument, Assertion, Error, ErrorCode, ObservedValue, RunResult, Status, Task, Value,
    MAX_DATA_BYTES,
};
use ops_validate::{CheckedInstruction, CheckedOperand, CheckedValue, Step, ValidatedPlan};
use serde::Serialize;
use serde_json::json;

use crate::expression::Evaluator;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_STORED_BYTES: usize = 16 * 1024 * 1024;

pub fn execute(plan: &ValidatedPlan<'_>) -> RunResult {
    execute_from(plan, Instant::now())
}

pub fn execute_from(plan: &ValidatedPlan<'_>, started: Instant) -> RunResult {
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
        result,
        values: BTreeMap::new(),
        stored_bytes: 0,
        payload_bytes: 0,
    };
    for step in plan.steps() {
        match run.step(step) {
            Ok(Some(value)) => {
                if let Some(destination) = &step.destination {
                    if let Err(error) = run.save(destination, value) {
                        run.result.fail(error.at(step.line));
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(error) => {
                if let Some(path) = &error.path {
                    run.result
                        .evidence
                        .push(json!({"line": step.line, "path": path}));
                }
                run.result.fail(error.at(step.line));
                break;
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
}

impl Stored {
    fn value(value: Value) -> Self {
        Self {
            value,
            task: None,
            bytes: 0,
        }
    }
}

struct Execution<'a, 'plan> {
    plan: &'a ValidatedPlan<'plan>,
    deadline: Instant,
    result: RunResult,
    values: BTreeMap<String, Stored>,
    stored_bytes: usize,
    payload_bytes: usize,
}

impl Execution<'_, '_> {
    fn step(&mut self, step: &Step) -> Result<Option<Stored>, Error> {
        self.check_time()?;
        let result = match &step.instruction {
            CheckedInstruction::Call { name, arguments } => self.call(name, arguments)?,
            CheckedInstruction::Where { source, predicate } => {
                let source = into_list(self.value(source)?)?;
                let mut selected = Vec::new();
                for item in source {
                    self.check_time()?;
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
                    return self.wait(task, *seconds);
                }
                Stored::value(self.value(source)?)
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
                let mut values = Vec::new();
                for operand in operands {
                    let output = ObservedValue {
                        expr: operand.text.clone(),
                        value: self.value(operand)?,
                    };
                    self.account(&output)?;
                    if step.destination.is_some() {
                        values.push(output.value.clone());
                    }
                    self.result.values.push(output);
                }
                Stored::value(Value::Array(values))
            }
        };
        Ok(Some(result))
    }

    fn call(&mut self, name: &str, operands: &[CheckedOperand]) -> Result<Stored, Error> {
        let mut arguments = Vec::with_capacity(operands.len());
        for operand in operands {
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
        let result = self.invoke(name, &arguments, self.deadline)?;
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
        })
    }

    fn wait(&mut self, task: Task, seconds: u64) -> Result<Option<Stored>, Error> {
        let limit = Instant::now()
            .checked_add(Duration::from_secs(seconds))
            .map_or(self.deadline, |deadline| deadline.min(self.deadline));
        let argument = Argument::Value(task.id.clone());
        let status = loop {
            if Instant::now() >= limit {
                break Status::Timeout;
            }
            let observation = match self.invoke(&task.poll, std::slice::from_ref(&argument), limit)
            {
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
                            .module_code(module_code),
                    );
                }
                Some("pending") => break Status::Pending,
                Some("running") => {
                    let remaining = limit.saturating_duration_since(Instant::now());
                    std::thread::sleep(POLL_INTERVAL.min(remaining));
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
    ) -> Result<Value, Error> {
        let context = ActionContext {
            deadline,
            max_bytes: MAX_DATA_BYTES,
        };
        context.remaining()?;
        if self.result.usage.actions >= self.plan.binding().budget.actions {
            return Err(Error::new(
                ErrorCode::EBudget,
                "Action count budget exhausted",
            ));
        }
        self.result.usage.actions += 1;
        let result = self.plan.registry().invoke(name, arguments, &context)?;
        context.remaining()?;
        Ok(result)
    }

    fn check_time(&self) -> Result<(), Error> {
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
        if total > MAX_STORED_BYTES {
            return Err(Error::new(
                ErrorCode::EBudget,
                "Stored values exceed the runtime data limit",
            ));
        }
        self.stored_bytes = total;
        self.values.insert(destination.to_owned(), value);
        Ok(())
    }
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
