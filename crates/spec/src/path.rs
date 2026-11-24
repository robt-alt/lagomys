use crate::{Error, ErrorCode, Type, Value};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Segment {
    Field(String),
    Index(usize),
}

#[derive(Clone, Debug)]
pub struct Path {
    pub root: String,
    pub segments: Vec<Segment>,
}

impl Path {
    pub fn lookup(&self, root: &Value) -> Result<Value, Error> {
        Self::lookup_segments(root, &self.segments, &self.root)
    }

    pub fn lookup_segments(
        value: &Value,
        segments: &[Segment],
        prefix: &str,
    ) -> Result<Value, Error> {
        let Some((first, rest)) = segments.split_first() else {
            return Ok(value.clone());
        };
        match first {
            Segment::Field(name) => {
                if let Some(list) = value.as_array() {
                    return list
                        .iter()
                        .enumerate()
                        .map(|(index, element)| {
                            Self::lookup_segments(element, segments, &format!("{prefix}[{index}]"))
                        })
                        .collect();
                }
                let path = format!("{prefix}.{name}");
                let field = value.get(name).ok_or_else(|| {
                    let error = Error::field(&path);
                    match value.as_object() {
                        Some(fields) => error.available(fields.keys().cloned().collect()),
                        None => error,
                    }
                })?;
                Self::lookup_segments(field, rest, &path)
            }
            Segment::Index(index) => {
                let path = format!("{prefix}[{index}]");
                let field = value.get(*index).ok_or_else(|| Error::field(&path))?;
                Self::lookup_segments(field, rest, &path)
            }
        }
    }

    pub fn value_type(&self, root: &Type) -> Type {
        Self::segment_type(root, &self.segments)
    }

    fn segment_type(root: &Type, segments: &[Segment]) -> Type {
        let Some((first, rest)) = segments.split_first() else {
            return root.clone();
        };
        match (root, first) {
            (Type::List(item), Segment::Field(_)) => {
                Type::List(Box::new(Self::segment_type(item, segments)))
            }
            (Type::List(item), Segment::Index(_)) => Self::segment_type(item, rest),
            (Type::Record(fields), Segment::Field(field)) => fields
                .get(field)
                .map_or(Type::Any, |kind| Self::segment_type(kind, rest)),
            _ => Type::Any,
        }
    }
}

pub fn is_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        _ => true,
    }
}

pub fn pick(value: &Value, path: &str) -> Result<Value, Error> {
    let mut segments = Vec::new();
    let mut remaining = path;
    while !remaining.is_empty() && segments.len() < 64 {
        if let Some(index) = remaining.strip_prefix('[') {
            let (number, rest) = index.split_once(']').ok_or_else(|| invalid_path(path))?;
            segments.push(Segment::Index(
                number.parse().map_err(|_| invalid_path(path))?,
            ));
            remaining = rest;
        } else {
            let tail = if segments.is_empty() {
                remaining
            } else {
                remaining
                    .strip_prefix('.')
                    .ok_or_else(|| invalid_path(path))?
            };
            let end = tail.find(['.', '[']).unwrap_or(tail.len());
            let name = &tail[..end];
            if !is_name(name) {
                return Err(invalid_path(path));
            }
            segments.push(Segment::Field(name.to_owned()));
            remaining = &tail[end..];
        }
    }
    if segments.is_empty() || !remaining.is_empty() {
        return Err(invalid_path(path));
    }
    Path::lookup_segments(value, &segments, "$")
}

fn invalid_path(path: &str) -> Error {
    Error::new(
        ErrorCode::EArgType,
        format!("Invalid result field path {path}"),
    )
}
