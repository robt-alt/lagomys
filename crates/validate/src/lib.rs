#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ops_actions::Registry;
use ops_spec::{Binding, Error, ErrorCode, Path, TargetKind, Type, Value};
pub use ops_syntax::Comparison;
use ops_syntax::{Expression, Instruction, Operand, OperandKind, Script};

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
        arguments: Vec<CheckedOperand>,
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
    let mut results = BTreeMap::new();
    let mut steps = Vec::new();
    let mut calls = 0;
    for statement in script.statements {
        let (instruction, result_type) = check_instruction(
            statement.instruction,
            binding,
            registry,
            &results,
            &mut calls,
        )
        .map_err(|error| error.at(statement.line))?;
        if let Some(destination) = &statement.destination {
            results.insert(destination.clone(), result_type);
        }
        steps.push(Step {
            line: statement.line,
            destination: statement.destination,
            instruction,
        });
    }
    Ok(ValidatedPlan {
        binding,
        registry,
        steps,
    })
}

fn check_instruction(
    instruction: Instruction,
    binding: &Binding,
    registry: &Registry,
    results: &BTreeMap<String, Type>,
    calls: &mut u64,
) -> Result<(CheckedInstruction, Type), Error> {
    let checked = match instruction {
        Instruction::Call { name, arguments } => {
            let signature = registry.resolve(&name, &binding.modules)?;
            signature.check_count(arguments.len())?;
            let arguments = arguments
                .into_iter()
                .zip(&signature.parameters)
                .map(|(argument, parameter)| {
                    let (argument, kind) =
                        check_value(argument, results, None, false, parameter.target, binding)?;
                    let valid = match &argument.kind {
                        CheckedValue::Literal(value) => parameter.value_type.accepts(value),
                        CheckedValue::Target(index) => binding
                            .targets
                            .get(*index)
                            .is_some_and(|target| parameter.value_type.accepts(&target.value())),
                        _ => parameter.value_type.compatible(&kind),
                    };
                    if !valid {
                        return Err(Error::new(
                            ErrorCode::EArgType,
                            format!(
                                "Parameter {} of {name} requires {}",
                                parameter.name,
                                parameter.value_type.name()
                            ),
                        ));
                    }
                    Ok(argument)
                })
                .collect::<Result<Vec<_>, Error>>()?;
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
            *calls += 1;
            if *calls > binding.budget.actions {
                return Err(Error::new(
                    ErrorCode::EBudget,
                    "Script exceeds the action count budget",
                ));
            }
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
        Instruction::Out(values) => {
            let values = values
                .into_iter()
                .map(|value| {
                    check_value(value, results, None, false, None, binding).map(|(value, _)| value)
                })
                .collect::<Result<Vec<_>, Error>>()?;
            (
                CheckedInstruction::Out(values),
                Type::List(Box::new(Type::Any)),
            )
        }
        Instruction::Unknown(name) => {
            return Err(Error::new(
                ErrorCode::EUnknownInstruction,
                format!("Unknown instruction {name}"),
            ));
        }
    };
    Ok(checked)
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
                ));
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
