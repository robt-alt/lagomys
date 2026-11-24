#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use ops_actions::Registry;
use ops_spec::{
    ActionSignature, Binding, Error, ErrorCode, LoopContext, Parameter, Path, RepeatResult,
    TargetKind, Type, Value,
};
pub use ops_syntax::Comparison;
use ops_syntax::{CallArgument, Expression, Instruction, Operand, OperandKind, Script, Statement};

pub struct ValidatedPlan<'a> {
    binding: &'a Binding,
    registry: &'a Registry,
    steps: Vec<Step>,
}

impl<'a> ValidatedPlan<'a> {
    pub fn binding(&self) -> &'a Binding {
        self.binding
    }

    pub fn registry(&self) -> &'a Registry {
        self.registry
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }
}

#[derive(Debug)]
pub struct Step {
    pub line: usize,
    pub destination: Option<String>,
    pub instruction: CheckedInstruction,
}

#[derive(Debug)]
pub enum CheckedInstruction {
    Call {
        name: String,
        arguments: Vec<Option<CheckedOperand>>,
    },
    Where {
        source: CheckedOperand,
        predicate: CheckedExpression,
    },
    Take {
        source: CheckedOperand,
        count: u64,
    },
    Wait {
        source: CheckedOperand,
        seconds: u64,
    },
    Assert {
        predicate: CheckedExpression,
        text: String,
    },
    Out(Vec<CheckedOperand>),
    Repeat {
        count: u64,
        context: Option<String>,
        body: Vec<Step>,
        introduced: Vec<String>,
    },
    If {
        predicate: CheckedExpression,
        then_body: Vec<Step>,
        else_body: Vec<Step>,
        introduced: Vec<String>,
    },
    Break,
    Continue,
    Return(Vec<CheckedOperand>),
    Delay(u64),
    NewCollector,
    Collect {
        name: String,
        value: CheckedOperand,
    },
    Evidence(Vec<String>),
}

#[derive(Debug)]
pub struct CheckedOperand {
    pub kind: CheckedValue,
    pub text: String,
}

#[derive(Debug)]
pub enum CheckedValue {
    Literal(Value),
    Result(Path),
    Field(Path),
    Target(usize),
}

#[derive(Debug)]
pub enum CheckedExpression {
    Value(CheckedOperand),
    Not(Box<CheckedExpression>),
    And(Vec<CheckedExpression>),
    Or(Vec<CheckedExpression>),
    Compare(Comparison, CheckedOperand, CheckedOperand),
}

pub fn validate<'a>(
    script: Script,
    binding: &'a Binding,
    registry: &'a Registry,
) -> Result<ValidatedPlan<'a>, Error> {
    binding.check_version()?;
    if Instant::now()
        .checked_add(Duration::from_secs(binding.budget.seconds))
        .is_none()
    {
        return Err(Error::new(
            ErrorCode::EBudget,
            "Execution time budget is out of range",
        ));
    }
    for module in &binding.modules {
        if !registry.has_module(module) {
            return Err(Error::new(
                ErrorCode::EModuleMissing,
                format!("Module {module} is not loaded"),
            ));
        }
    }
    for expansion in &script.macro_expansions {
        if !expansion.capture_assigned {
            return Err(Error::new(
                ErrorCode::ERef,
                format!(
                    "Sampling macro body does not assign {}",
                    expansion.capture_name
                ),
            )
            .at(expansion.line));
        }
        if let Some(&line) = script.definitions.get(&expansion.items_name) {
            return Err(Error::new(
                ErrorCode::ERef,
                format!("Sampling macro reserves {}", expansion.items_name),
            )
            .at(line));
        }
    }
    let (steps, _) =
        Validator { binding, registry }.block(script.statements, &mut Scope::default(), false)?;
    Ok(ValidatedPlan {
        binding,
        registry,
        steps,
    })
}

#[derive(Clone, Default)]
struct Scope {
    results: BTreeMap<String, Type>,
    collectors: BTreeSet<String>,
    contexts: BTreeSet<String>,
}

impl Scope {
    fn join(&mut self, left: Scope, right: Scope) -> Vec<String> {
        let mut introduced = Vec::new();
        let names: BTreeSet<_> = left.results.keys().chain(right.results.keys()).collect();
        for name in names {
            let left = left.results.get(name).unwrap_or(&Type::Null);
            let right = right.results.get(name).unwrap_or(&Type::Null);
            if self
                .results
                .insert(name.clone(), join_type(left, right))
                .is_none()
            {
                introduced.push(name.clone());
            }
        }
        self.collectors = left.collectors.union(&right.collectors).cloned().collect();
        introduced
    }
}

fn join_type(left: &Type, right: &Type) -> Type {
    if left == right {
        return left.clone();
    }
    if matches!(left, Type::Any) || matches!(right, Type::Any) {
        return Type::Any;
    }
    let mut members = Vec::new();
    for kind in [left, right] {
        let alternatives = match kind {
            Type::OneOf(types) => types.as_slice(),
            other => std::slice::from_ref(other),
        };
        for item in alternatives {
            if !members.contains(item) {
                members.push(item.clone());
            }
        }
    }
    Type::OneOf(members)
}

#[derive(Clone, Copy, Default)]
struct Bounds {
    actions: u64,
    delay_ms: u64,
}

impl Bounds {
    fn add(self, other: Self) -> Result<Self, Error> {
        Ok(Self {
            actions: self
                .actions
                .checked_add(other.actions)
                .ok_or_else(budget_overflow)?,
            delay_ms: self
                .delay_ms
                .checked_add(other.delay_ms)
                .ok_or_else(budget_overflow)?,
        })
    }

    fn repeat(self, count: u64) -> Result<Self, Error> {
        Ok(Self {
            actions: self
                .actions
                .checked_mul(count)
                .ok_or_else(budget_overflow)?,
            delay_ms: self
                .delay_ms
                .checked_mul(count)
                .ok_or_else(budget_overflow)?,
        })
    }

    fn either(self, other: Self) -> Self {
        Self {
            actions: self.actions.max(other.actions),
            delay_ms: self.delay_ms.max(other.delay_ms),
        }
    }

    fn check(self, binding: &Binding) -> Result<Self, Error> {
        if self.actions > binding.budget.actions {
            return Err(Error::new(
                ErrorCode::EBudget,
                "Worst-case action count exceeds the budget",
            ));
        }
        if u128::from(self.delay_ms) > u128::from(binding.budget.seconds) * 1000 {
            return Err(Error::new(
                ErrorCode::EBudget,
                "Worst-case delay exceeds the execution time budget",
            ));
        }
        Ok(self)
    }
}

fn budget_overflow() -> Error {
    Error::new(ErrorCode::EBudget, "Worst-case path budget overflowed")
}

struct Validator<'a> {
    binding: &'a Binding,
    registry: &'a Registry,
}

impl Validator<'_> {
    fn block(
        &self,
        statements: Vec<Statement>,
        scope: &mut Scope,
        in_repeat: bool,
    ) -> Result<(Vec<Step>, Bounds), Error> {
        let mut steps = Vec::with_capacity(statements.len());
        let mut budget = Bounds::default();
        for statement in statements {
            let Statement {
                line,
                destination,
                instruction,
            } = statement;
            let (instruction, result_type, bounds) = self
                .instruction(instruction, destination.as_deref(), scope, in_repeat)
                .map_err(|error| {
                    if error.line.is_some() {
                        error
                    } else {
                        error.at(line)
                    }
                })?;
            budget = budget
                .add(bounds)
                .and_then(|total| total.check(self.binding))
                .map_err(|error| error.at(line))?;
            if let Some(name) = &destination {
                scope.results.insert(name.clone(), result_type);
                if matches!(instruction, CheckedInstruction::NewCollector) {
                    scope.collectors.insert(name.clone());
                } else {
                    scope.collectors.remove(name);
                }
            }
            steps.push(Step {
                line,
                destination,
                instruction,
            });
        }
        Ok((steps, budget))
    }

    fn instruction(
        &self,
        instruction: Instruction,
        destination: Option<&str>,
        scope: &mut Scope,
        in_repeat: bool,
    ) -> Result<(CheckedInstruction, Type, Bounds), Error> {
        if let Some(name) = destination {
            if scope.contexts.contains(name) {
                return Err(Error::new(ErrorCode::ERef, "Loop context is read-only"));
            }
            if matches!(
                &instruction,
                Instruction::If { .. }
                    | Instruction::Break
                    | Instruction::Continue
                    | Instruction::Return(_)
                    | Instruction::Delay(_)
                    | Instruction::Evidence(_)
            ) {
                return Err(Error::new(
                    ErrorCode::EParse,
                    "This instruction cannot be assigned",
                ));
            }
        }
        let binding = self.binding;
        let registry = self.registry;
        let results = &scope.results;
        let mut bounds = Bounds::default();
        let is_return = matches!(&instruction, Instruction::Return(_));
        let checked = match instruction {
            Instruction::Call { name, arguments } => {
                let signature = registry.resolve(&name, &binding.modules)?;
                let arguments = check_call_arguments(arguments, signature, results, binding)?;
                if let Some(poll) = &signature.poll {
                    let poll_signature = registry.resolve(poll, &binding.modules)?;
                    if registry.signature(&name).and_then(|(_, module)| module)
                        != registry.signature(poll).and_then(|(_, module)| module)
                        || poll_signature.poll.is_some()
                        || poll_signature.parameters.len() != 1
                        || poll_signature.parameters[0].target.is_some()
                    {
                        return Err(Error::new(
                            ErrorCode::EArgType,
                            format!("Invalid poll declaration for {name}"),
                        ));
                    }
                }
                bounds.actions = 1;
                (
                    CheckedInstruction::Call { name, arguments },
                    if signature.poll.is_some() {
                        Type::Any
                    } else {
                        signature.output.clone()
                    },
                )
            }
            Instruction::Where { source, predicate } => {
                let (source, kind) = check_value(source, results, None, false, None, binding)?;
                let element = list_element(&kind)?;
                let predicate = check_expression(predicate, results, Some(&element), binding)?;
                (
                    CheckedInstruction::Where { source, predicate },
                    Type::List(Box::new(element)),
                )
            }
            Instruction::Take { source, count } => {
                let (source, kind) = check_value(source, results, None, false, None, binding)?;
                let element = list_element(&kind)?;
                (
                    CheckedInstruction::Take { source, count },
                    Type::List(Box::new(element)),
                )
            }
            Instruction::Wait { source, seconds } => {
                if seconds > binding.budget.seconds {
                    return Err(Error::new(
                        ErrorCode::EBudget,
                        "Wait limit exceeds the execution time budget",
                    ));
                }
                let (source, _) = check_value(source, results, None, false, None, binding)?;
                if !matches!(&source.kind, CheckedValue::Result(_)) {
                    return Err(Error::new(
                        ErrorCode::EArgType,
                        "wait requires a result reference",
                    ));
                }
                bounds.actions = seconds.div_ceil(2);
                (CheckedInstruction::Wait { source, seconds }, Type::Any)
            }
            Instruction::Assert { predicate, text } => {
                let predicate = check_expression(predicate, results, None, binding)?;
                let fields = BTreeMap::from([
                    ("expr".to_owned(), Type::String),
                    ("passed".to_owned(), Type::Bool),
                ]);
                (
                    CheckedInstruction::Assert { predicate, text },
                    Type::Record(fields),
                )
            }
            Instruction::Out(values) | Instruction::Return(values) => {
                let values = values
                    .into_iter()
                    .map(|value| {
                        check_value(value, results, None, false, None, binding)
                            .map(|(value, _)| value)
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                if is_return {
                    if values.is_empty() {
                        return Err(Error::new(
                            ErrorCode::EArgCount,
                            "return requires at least one value",
                        ));
                    }
                    (CheckedInstruction::Return(values), Type::Null)
                } else {
                    (
                        CheckedInstruction::Out(values),
                        Type::List(Box::new(Type::Any)),
                    )
                }
            }
            Instruction::Repeat {
                count,
                context,
                body,
            } => {
                let count = integer(count, "repeat requires a positive integer count")?;
                if count == 0 {
                    return Err(Error::new(
                        ErrorCode::EArgType,
                        "repeat requires a positive integer count",
                    ));
                }
                let mut body_scope = scope.clone();
                if let Some(name) = &context {
                    if body_scope.results.contains_key(name) {
                        return Err(Error::new(
                            ErrorCode::ERef,
                            format!("Loop context {name} is already defined"),
                        ));
                    }
                    body_scope.contexts.insert(name.clone());
                    body_scope
                        .results
                        .insert(name.clone(), LoopContext::value_type());
                }
                let (body, body_bounds) = self.block(body, &mut body_scope, true)?;
                if let Some(name) = &context {
                    body_scope.results.remove(name);
                }
                let introduced = scope.join(body_scope, scope.clone());
                bounds = body_bounds.repeat(count)?;
                (
                    CheckedInstruction::Repeat {
                        count,
                        context,
                        body,
                        introduced,
                    },
                    RepeatResult::value_type(),
                )
            }
            Instruction::If {
                predicate,
                then_body,
                else_body,
            } => {
                let predicate = check_expression(predicate, results, None, binding)?;
                let mut then_scope = scope.clone();
                let mut else_scope = scope.clone();
                let (then_body, then_bounds) = self.block(then_body, &mut then_scope, in_repeat)?;
                let (else_body, else_bounds) = self.block(else_body, &mut else_scope, in_repeat)?;
                let introduced = scope.join(then_scope, else_scope);
                bounds = then_bounds.either(else_bounds);
                (
                    CheckedInstruction::If {
                        predicate,
                        then_body,
                        else_body,
                        introduced,
                    },
                    Type::Null,
                )
            }
            Instruction::Break if in_repeat => (CheckedInstruction::Break, Type::Null),
            Instruction::Continue if in_repeat => (CheckedInstruction::Continue, Type::Null),
            Instruction::Break | Instruction::Continue => {
                return Err(Error::new(
                    ErrorCode::EParse,
                    "break and continue require an enclosing repeat",
                ));
            }
            Instruction::Delay(value) => {
                let milliseconds =
                    integer(value, "delay requires nonnegative integer milliseconds")?;
                bounds.delay_ms = milliseconds;
                (CheckedInstruction::Delay(milliseconds), Type::Null)
            }
            Instruction::Collect(arguments) => self.collect(arguments, destination, scope)?,
            Instruction::Evidence(names) => {
                if names.is_empty() {
                    return Err(Error::new(
                        ErrorCode::EArgCount,
                        "evidence requires at least one result name",
                    ));
                }
                for name in &names {
                    if !results.contains_key(name) {
                        return Err(Error::new(
                            ErrorCode::ERef,
                            format!("Unknown evidence result {name}"),
                        ));
                    }
                }
                (CheckedInstruction::Evidence(names), Type::Null)
            }
            Instruction::Unknown(name) => {
                return Err(Error::new(
                    ErrorCode::EUnknownInstruction,
                    format!("Unknown instruction {name}"),
                ));
            }
        };
        Ok((checked.0, checked.1, bounds))
    }

    fn collect(
        &self,
        arguments: Vec<Operand>,
        destination: Option<&str>,
        scope: &Scope,
    ) -> Result<(CheckedInstruction, Type), Error> {
        if destination.is_some() {
            if !arguments.is_empty() {
                return Err(Error::new(
                    ErrorCode::EArgCount,
                    "Collector creation takes no arguments",
                ));
            }
            return Ok((
                CheckedInstruction::NewCollector,
                Type::List(Box::new(Type::Any)),
            ));
        }
        let [target, value] = <[Operand; 2]>::try_from(arguments).map_err(|_| {
            Error::new(
                ErrorCode::EArgCount,
                "collect requires a collector name and a value",
            )
        })?;
        let OperandKind::Reference(path) = target.kind else {
            return Err(Error::new(
                ErrorCode::EArgType,
                "collect requires a collector name",
            ));
        };
        if !path.segments.is_empty() {
            return Err(Error::new(
                ErrorCode::EArgType,
                "collect requires a collector name",
            ));
        }
        if !scope.results.contains_key(&path.root) {
            return Err(Error::new(
                ErrorCode::ERef,
                format!("Unknown collector {}", path.root),
            ));
        }
        if !scope.collectors.contains(&path.root) {
            return Err(Error::new(
                ErrorCode::EArgType,
                format!("{} is not a collector", path.root),
            ));
        }
        let (value, _) = check_value(value, &scope.results, None, false, None, self.binding)?;
        Ok((
            CheckedInstruction::Collect {
                name: path.root,
                value,
            },
            Type::Null,
        ))
    }
}

fn check_call_arguments(
    arguments: Vec<CallArgument>,
    signature: &ActionSignature,
    results: &BTreeMap<String, Type>,
    binding: &Binding,
) -> Result<Vec<Option<CheckedOperand>>, Error> {
    let parameters = &signature.parameters;
    let positional_count = arguments
        .iter()
        .filter(|argument| matches!(argument, CallArgument::Positional(_)))
        .count();
    if positional_count > parameters.len() {
        signature.check_count(positional_count)?;
    }

    let mut slots = vec![None; parameters.len()];
    let mut position = 0;
    for argument in arguments {
        let (index, operand) = match argument {
            CallArgument::Positional(operand) => {
                let index = position;
                position += 1;
                (index, operand)
            }
            CallArgument::Named { name, operand } => {
                let index = parameters
                    .iter()
                    .position(|parameter| parameter.name == name)
                    .ok_or_else(|| {
                        Error::new(
                            ErrorCode::EArgType,
                            format!("Unknown parameter {name} for action {}", signature.name),
                        )
                        .action(&signature.name)
                        .available(
                            parameters
                                .iter()
                                .map(|parameter| parameter.name.clone())
                                .collect(),
                        )
                    })?;
                (index, operand)
            }
        };
        let slot = &mut slots[index];
        if slot.is_some() {
            return Err(Error::new(
                ErrorCode::EArgType,
                format!(
                    "Parameter {} for action {} was supplied more than once",
                    parameters[index].name, signature.name
                ),
            )
            .action(&signature.name));
        }
        *slot = Some(operand);
    }
    for (parameter, operand) in parameters.iter().zip(&slots) {
        if parameter.required && operand.is_none() {
            return Err(signature.missing_argument(parameter));
        }
    }
    slots.truncate(
        slots
            .iter()
            .rposition(Option::is_some)
            .map_or(0, |index| index + 1),
    );
    slots
        .into_iter()
        .zip(parameters)
        .map(|(operand, parameter)| {
            operand
                .map(|operand| {
                    check_call_operand(operand, parameter, &signature.name, results, binding)
                })
                .transpose()
        })
        .collect()
}

fn check_call_operand(
    operand: Operand,
    parameter: &Parameter,
    action: &str,
    results: &BTreeMap<String, Type>,
    binding: &Binding,
) -> Result<CheckedOperand, Error> {
    let (argument, kind) = check_value(operand, results, None, false, parameter.target, binding)
        .map_err(|error| error.action(action))?;
    let (valid, actual) = match &argument.kind {
        CheckedValue::Literal(value) => (parameter.value_type.accepts(value), kind.name()),
        CheckedValue::Target(index) => {
            binding
                .targets
                .get(*index)
                .map_or((false, kind.name()), |target| {
                    let value = target.value();
                    (
                        parameter.value_type.accepts(&value),
                        Type::of(&value).name(),
                    )
                })
        }
        _ => (parameter.value_type.compatible(&kind), kind.name()),
    };
    if !valid {
        return Err(Error::new(
            ErrorCode::EArgType,
            format!(
                "Parameter {} of {action} requires {}",
                parameter.name,
                parameter.value_type.name()
            ),
        )
        .action(action)
        .expected(parameter.value_type.name())
        .actual(actual));
    }
    Ok(argument)
}

fn integer(operand: Operand, message: &str) -> Result<u64, Error> {
    if let OperandKind::Literal(value) = operand.kind {
        if let Some(number) = value.as_u64() {
            return Ok(number);
        }
    }
    Err(Error::new(ErrorCode::EArgType, message).column(operand.column))
}

fn list_element(kind: &Type) -> Result<Type, Error> {
    match kind {
        Type::Any => Ok(Type::Any),
        Type::List(item) => Ok((**item).clone()),
        Type::OneOf(types)
            if types
                .iter()
                .any(|item| matches!(item, Type::Any | Type::List(_))) =>
        {
            Ok(Type::Any)
        }
        _ => Err(Error::new(
            ErrorCode::EArgType,
            "where and take require a list",
        )),
    }
}

fn check_expression(
    expression: Expression,
    results: &BTreeMap<String, Type>,
    row: Option<&Type>,
    binding: &Binding,
) -> Result<CheckedExpression, Error> {
    match expression {
        Expression::Value(value) => Ok(CheckedExpression::Value(
            check_value(value, results, row, true, None, binding)?.0,
        )),
        Expression::Not(value) => Ok(CheckedExpression::Not(Box::new(check_expression(
            *value, results, row, binding,
        )?))),
        Expression::And(list) => {
            let values = list
                .into_iter()
                .map(|value| check_expression(value, results, row, binding))
                .collect::<Result<Vec<_>, Error>>()?;
            Ok(CheckedExpression::And(values))
        }
        Expression::Or(list) => {
            let values = list
                .into_iter()
                .map(|value| check_expression(value, results, row, binding))
                .collect::<Result<Vec<_>, Error>>()?;
            Ok(CheckedExpression::Or(values))
        }
        Expression::Compare(operator, left, right) => {
            let (left, left_type) = check_value(left, results, row, true, None, binding)?;
            let (right, right_type) = check_value(right, results, row, false, None, binding)?;
            if !matches!(operator, Comparison::Equal | Comparison::NotEqual) {
                let numbers =
                    Type::Number.compatible(&left_type) && Type::Number.compatible(&right_type);
                let strings =
                    Type::String.compatible(&left_type) && Type::String.compatible(&right_type);
                if !numbers && !strings {
                    return Err(Error::new(
                        ErrorCode::EArgType,
                        "Ordering requires two numbers or two strings",
                    ));
                }
            }
            Ok(CheckedExpression::Compare(operator, left, right))
        }
    }
}

fn check_value(
    operand: Operand,
    results: &BTreeMap<String, Type>,
    row: Option<&Type>,
    allow_dynamic_field: bool,
    target: Option<TargetKind>,
    binding: &Binding,
) -> Result<(CheckedOperand, Type), Error> {
    let fail = |code, message| Error::new(code, message).column(operand.column);
    let (kind, value_type) = match (operand.kind, target) {
        (OperandKind::Target(index), Some(kind)) => {
            let bound = binding.targets.get(index).ok_or_else(|| {
                fail(
                    ErrorCode::ETargetMissing,
                    format!("Missing bound target @{index}"),
                )
            })?;
            if kind != bound.kind() {
                return Err(fail(
                    ErrorCode::ETargetType,
                    format!("Target @{index} must have kind {}", kind.name()),
                )
                .expected(kind.name())
                .actual(bound.kind().name()));
            }
            (CheckedValue::Target(index), Type::Any)
        }
        (OperandKind::Target(_), None) | (_, Some(_)) => {
            return Err(fail(
                ErrorCode::ETargetType,
                "Target handles are valid only in declared target parameters".to_owned(),
            ));
        }
        (OperandKind::Literal(value), None) => {
            let kind = Type::of(&value);
            (CheckedValue::Literal(value), kind)
        }
        (OperandKind::Reference(path), None) => {
            let field_type = row.and_then(|kind| match kind {
                Type::Record(fields) => fields.get(&path.root),
                _ => None,
            });
            if path.segments.is_empty()
                && row.is_some()
                && (allow_dynamic_field || field_type.is_some())
            {
                let kind = field_type.map_or(Type::Any, |field| path.value_type(field));
                (CheckedValue::Field(path), kind)
            } else if let Some(kind) = results.get(&path.root) {
                let kind = path.value_type(kind);
                (CheckedValue::Result(path), kind)
            } else if let Some(field) = field_type {
                let kind = path.value_type(field);
                (CheckedValue::Field(path), kind)
            } else {
                return Err(fail(
                    ErrorCode::ERef,
                    format!(
                        "Unknown reference {}. String values require double quotes",
                        operand.text
                    ),
                ));
            }
        }
    };
    Ok((
        CheckedOperand {
            kind,
            text: operand.text,
        },
        value_type,
    ))
}
