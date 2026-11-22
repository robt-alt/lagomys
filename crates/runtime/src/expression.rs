use std::{cmp::Ordering, collections::BTreeMap};

use ops_spec::{truthy, Error, ErrorCode, Value};
use ops_validate::{CheckedExpression, CheckedOperand, CheckedValue, Comparison};

use crate::Stored;

pub(crate) struct Evaluator<'a> {
    results: &'a BTreeMap<String, Stored>,
    row: Option<&'a Value>,
}

impl<'a> Evaluator<'a> {
    pub(crate) fn new(results: &'a BTreeMap<String, Stored>, row: Option<&'a Value>) -> Self {
        Self { results, row }
    }

    pub(crate) fn value(&self, operand: &CheckedOperand) -> Result<Value, Error> {
        match &operand.kind {
            CheckedValue::Literal(value) => Ok(value.clone()),
            CheckedValue::Result(path) => {
                let value = self.results.get(&path.root).ok_or_else(|| {
                    Error::new(ErrorCode::ERef, format!("Unavailable result {}", path.root))
                })?;
                path.lookup(&value.value)
            }
            CheckedValue::Field(path) => {
                let value = self
                    .row
                    .and_then(|row| row.get(&path.root))
                    .ok_or_else(|| Error::field(&path.root))?;
                path.lookup(value)
            }
            CheckedValue::Target(_) => Err(Error::new(
                ErrorCode::ETargetType,
                "A target handle is not a data value",
            )),
        }
    }

    pub(crate) fn evaluate(&self, expression: &CheckedExpression) -> Result<bool, Error> {
        match expression {
            CheckedExpression::Value(value) => Ok(truthy(&self.value(value)?)),
            CheckedExpression::Not(value) => Ok(!self.evaluate(value)?),
            CheckedExpression::And(values) => {
                for value in values {
                    if !self.evaluate(value)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            CheckedExpression::Or(values) => {
                for value in values {
                    if self.evaluate(value)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            CheckedExpression::Compare(operator, left, right) => {
                compare(*operator, &self.value(left)?, &self.value(right)?)
            }
        }
    }
}

fn compare(operator: Comparison, left: &Value, right: &Value) -> Result<bool, Error> {
    let order = match (left, right) {
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        (Value::Number(left), Value::Number(right)) => {
            let integer = |number: &serde_json::Number| {
                number
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| number.as_u64().map(i128::from))
            };
            match (integer(left), integer(right)) {
                (Some(left), Some(right)) => Some(left.cmp(&right)),
                _ => left
                    .as_f64()
                    .zip(right.as_f64())
                    .and_then(|(left, right)| left.partial_cmp(&right)),
            }
        }
        _ => None,
    };
    let equal = order.map_or(left == right, |order| order.is_eq());
    match operator {
        Comparison::Equal => Ok(equal),
        Comparison::NotEqual => Ok(!equal),
        operator => {
            let order = order.ok_or_else(|| {
                Error::new(
                    ErrorCode::EArgType,
                    "Ordering requires two numbers or two strings",
                )
            })?;
            Ok(match operator {
                Comparison::Greater => order == Ordering::Greater,
                Comparison::Less => order == Ordering::Less,
                Comparison::GreaterEqual => order != Ordering::Less,
                Comparison::LessEqual => order != Ordering::Greater,
                Comparison::Equal | Comparison::NotEqual => equal,
            })
        }
    }
}
