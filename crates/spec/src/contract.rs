use std::collections::BTreeMap;

use crate::{Error, ErrorCode, Target, TargetKind, Value};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Type {
    Any,
    Null,
    Bool,
    Number,
    Integer,
    Count { minimum: u64, maximum: u64 },
    String,
    Choices(Vec<Value>),
    List(Box<Type>),
    Record(BTreeMap<String, Type>),
    OneOf(Vec<Type>),
}

impl Type {
    pub fn of(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(_) => Self::Bool,
            Value::Number(number) if number.is_i64() || number.is_u64() => Self::Integer,
            Value::Number(_) => Self::Number,
            Value::String(_) => Self::String,
            Value::Array(_) => Self::List(Box::new(Self::Any)),
            Value::Object(fields) => Self::Record(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), Self::of(value)))
                    .collect(),
            ),
        }
    }

    pub fn accepts(&self, value: &Value) -> bool {
        match self {
            Self::Any => true,
            Self::Null => value.is_null(),
            Self::Bool => value.is_boolean(),
            Self::Number => value.is_number(),
            Self::Integer => value.is_i64() || value.is_u64(),
            Self::Count { minimum, maximum } => value
                .as_u64()
                .is_some_and(|count| (*minimum..=*maximum).contains(&count)),
            Self::String => value.is_string(),
            Self::Choices(choices) => choices.contains(value),
            Self::List(item) => value
                .as_array()
                .is_some_and(|values| values.iter().all(|value| item.accepts(value))),
            Self::Record(fields) => value.as_object().is_some_and(|value| {
                fields
                    .iter()
                    .all(|(field, kind)| value.get(field).is_some_and(|item| kind.accepts(item)))
            }),
            Self::OneOf(types) => types.iter().any(|kind| kind.accepts(value)),
        }
    }

    pub fn compatible(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Any, _) | (_, Self::Any) => true,
            (Self::OneOf(types), kind) => types.iter().any(|item| item.compatible(kind)),
            (kind, Self::OneOf(types)) => types.iter().any(|item| kind.compatible(item)),
            (Self::Choices(choices), kind) | (kind, Self::Choices(choices)) => choices
                .iter()
                .any(|value| kind.compatible(&Self::of(value))),
            (
                Self::Number | Self::Integer | Self::Count { .. },
                Self::Number | Self::Integer | Self::Count { .. },
            ) => true,
            (Self::List(item), Self::List(other)) => item.compatible(other),
            (Self::Record(_), Self::Record(_)) => true,
            _ => self == other,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Any | Self::Choices(_) | Self::OneOf(_) => "value",
            Self::Null => "null",
            Self::Bool => "boolean",
            Self::Number => "number",
            Self::Integer | Self::Count { .. } => "integer",
            Self::String => "string",
            Self::List(_) => "list",
            Self::Record(_) => "object",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Parameter {
    pub name: String,
    pub value_type: Type,
    pub required: bool,
    pub target: Option<TargetKind>,
}

impl Parameter {
    pub fn value(name: impl Into<String>, value_type: Type, required: bool) -> Self {
        Self {
            name: name.into(),
            value_type,
            required,
            target: None,
        }
    }

    pub fn target(name: impl Into<String>, target: TargetKind) -> Self {
        Self {
            name: name.into(),
            value_type: Type::Any,
            required: true,
            target: Some(target),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ActionSignature {
    pub name: String,
    pub parameters: Vec<Parameter>,
    pub output: Type,
    pub poll: Option<String>,
}

impl ActionSignature {
    pub fn check_count(&self, count: usize) -> Result<(), Error> {
        let minimum = self
            .parameters
            .iter()
            .rposition(|parameter| parameter.required)
            .map_or(0, |index| index + 1);
        let maximum = self.parameters.len();
        if (minimum..=maximum).contains(&count) {
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::EArgCount,
                format!(
                    "Action {} expects {minimum} to {maximum} arguments, received {count}",
                    self.name
                ),
            ))
        }
    }

    pub fn check_arguments(&self, arguments: &[Argument]) -> Result<(), Error> {
        self.check_count(arguments.len())?;
        for (parameter, argument) in self.parameters.iter().zip(arguments) {
            match (parameter.target, argument) {
                (Some(kind), Argument::Target(target))
                    if kind == target.kind() && parameter.value_type.accepts(&target.value()) => {}
                (Some(kind), _) => {
                    return Err(Error::new(
                        ErrorCode::ETargetType,
                        format!(
                            "Parameter {} of {} requires a bound {} target",
                            parameter.name,
                            self.name,
                            kind.name()
                        ),
                    ))
                }
                (None, Argument::Value(value)) if parameter.value_type.accepts(value) => {}
                _ => {
                    return Err(Error::new(
                        ErrorCode::EArgType,
                        format!(
                            "Invalid value for parameter {} of {}",
                            parameter.name, self.name
                        ),
                    ))
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub enum Argument {
    Target(Target),
    Value(Value),
}

impl Argument {
    pub fn value(&self) -> Value {
        match self {
            Self::Target(target) => target.value(),
            Self::Value(value) => value.clone(),
        }
    }
}
