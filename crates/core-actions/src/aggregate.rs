use std::{cmp::Ordering, collections::BTreeMap};

use ops_actions::ActionContext;
use ops_spec::{Error, ErrorCode, Type, Value};
use serde_json::json;

pub(crate) fn observation_type() -> Type {
    Type::OneOf(vec![Type::Number, Type::Null])
}

pub(crate) fn stat_fields() -> [(&'static str, Type); 4] {
    [
        ("mean", observation_type()),
        ("max", observation_type()),
        ("observed_count", Type::Integer),
        ("sample_count", Type::Integer),
    ]
}

pub(crate) fn stats(values: &Value, context: &ActionContext) -> Result<Value, Error> {
    let values = values
        .as_array()
        .ok_or_else(|| invalid("data.stats requires a list"))?;
    let mut stats = Stats::default();
    for value in values {
        context.remaining()?;
        stats.observe(value)?;
    }
    Ok(stats.result(values.len()))
}

pub(crate) fn group_stats(
    rounds: &Value,
    key_field: &Value,
    value_field: &Value,
    context: &ActionContext,
) -> Result<Value, Error> {
    let rounds = rounds
        .as_array()
        .ok_or_else(|| invalid("data.group_stats requires a list of rounds"))?;
    let (Some(key_field), Some(value_field)) = (key_field.as_str(), value_field.as_str()) else {
        return Err(invalid("data.group_stats requires two field names"));
    };
    let mut groups = BTreeMap::<Key<'_>, Group<'_>>::new();
    for (index, round) in rounds.iter().enumerate() {
        context.remaining()?;
        let rows = round
            .as_array()
            .ok_or_else(|| invalid("Each round must be a list"))?;
        for row in rows {
            context.remaining()?;
            let (Some(key), Some(value)) = (row.get(key_field), row.get(value_field)) else {
                continue;
            };
            let order = Key::new(key)?;
            if groups.first_key_value().is_some_and(|(first, _)| {
                matches!(first, Key::Text(_)) != matches!(order, Key::Text(_))
            }) {
                return Err(invalid(
                    "Ordering group keys requires all numbers or all strings",
                ));
            }
            let group = groups.entry(order).or_insert_with(|| Group {
                key,
                last_round: None,
                stats: Stats::default(),
            });
            if group.last_round != Some(index) {
                group.stats.observe(value)?;
                group.last_round = Some(index);
            }
        }
    }
    let mut values = Vec::new();
    let mut bytes = 2_usize;
    for group in groups.into_values() {
        context.remaining()?;
        let mut value = group.stats.result(rounds.len());
        value["key"] = group.key.clone();
        bytes = bytes
            .saturating_add(value.to_string().len())
            .saturating_add(usize::from(!values.is_empty()));
        if bytes > context.max_bytes {
            return Err(crate::data_limit());
        }
        values.push(value);
    }
    Ok(Value::Array(values))
}

fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::EArgType, message)
}

#[derive(Default)]
struct Stats<'a> {
    observed: usize,
    mean: f64,
    max: Option<&'a Value>,
}

impl<'a> Stats<'a> {
    fn observe(&mut self, value: &'a Value) -> Result<(), Error> {
        if value.is_null() {
            return Ok(());
        }
        let number = value
            .as_f64()
            .ok_or_else(|| invalid("Observations must be numbers or null"))?;
        self.observed += 1;
        let count = self.observed as f64;
        // Weight opposite signs separately so that even an extreme difference stays finite.
        self.mean = if self.mean.is_sign_negative() == number.is_sign_negative() {
            self.mean + (number - self.mean) / count
        } else {
            self.mean * ((count - 1.0) / count) + number / count
        };
        if self
            .max
            .is_none_or(|previous| number_key(value) > number_key(previous))
        {
            self.max = Some(value);
        }
        Ok(())
    }

    fn result(&self, sample_count: usize) -> Value {
        json!({
            "mean": (self.observed != 0).then_some(self.mean),
            "max": self.max,
            "observed_count": self.observed,
            "sample_count": sample_count,
        })
    }
}

struct Group<'a> {
    key: &'a Value,
    last_round: Option<usize>,
    stats: Stats<'a>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Key<'a> {
    Number(NumberKey),
    Text(&'a str),
}

impl<'a> Key<'a> {
    fn new(value: &'a Value) -> Result<Self, Error> {
        if let Some(text) = value.as_str() {
            Ok(Self::Text(text))
        } else {
            number_key(value)
                .map(Self::Number)
                .ok_or_else(|| invalid("Group keys must be sortable numbers or strings"))
        }
    }
}

#[derive(Clone, Copy)]
enum NumberKey {
    Integer(i128),
    Float(f64),
}

fn number_key(value: &Value) -> Option<NumberKey> {
    if let Some(number) = value.as_i64() {
        Some(NumberKey::Integer(i128::from(number)))
    } else if let Some(number) = value.as_u64() {
        Some(NumberKey::Integer(i128::from(number)))
    } else {
        value.as_f64().map(NumberKey::Float)
    }
}

impl Ord for NumberKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match (*self, *other) {
            (Self::Integer(left), Self::Integer(right)) => left.cmp(&right),
            (Self::Float(left), Self::Float(right)) => {
                if left == right {
                    Ordering::Equal
                } else {
                    left.total_cmp(&right)
                }
            }
            (Self::Integer(left), Self::Float(right)) => compare_integer(left, right),
            (Self::Float(left), Self::Integer(right)) => compare_integer(right, left).reverse(),
        }
    }
}

impl PartialOrd for NumberKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for NumberKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for NumberKey {}

fn compare_integer(integer: i128, float: f64) -> Ordering {
    // A JSON integer fits in i64 or u64. The cast saturates only beyond those ranges.
    integer.cmp(&(float as i128)).then_with(|| {
        let fraction = float.fract();
        if fraction > 0.0 {
            Ordering::Less
        } else if fraction < 0.0 {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    })
}
