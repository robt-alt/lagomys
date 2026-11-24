#![forbid(unsafe_code)]

mod aggregate;
mod network;
mod process;
mod sample;

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use ops_actions::{ActionContext, ActionProvider};
use ops_spec::{
    ActionSignature, Argument, Error, ErrorCode, Parameter, Target, TargetKind, Type, Value,
};
use serde_json::json;

pub struct CoreActions(Vec<ActionSignature>);

impl CoreActions {
    pub fn new() -> Self {
        let process_records = list_record([
            ("pid", Type::Integer),
            ("name", Type::String),
            ("running", Type::Bool),
        ]);
        let actions = vec![
            signature(
                "file.read",
                vec![
                    Parameter::target("file", TargetKind::File),
                    Parameter::value(
                        "mode",
                        Type::Choices(vec![json!("auto"), json!("lines")]),
                        false,
                    ),
                ],
                Type::Any,
            ),
            signature(
                "dir.list",
                vec![Parameter::target("dir", TargetKind::Dir)],
                list_record([("name", Type::String), ("kind", Type::String)]),
            ),
            signature(
                "proc.status",
                vec![Parameter::target("process", TargetKind::Process)],
                process_records.clone(),
            ),
            signature(
                "proc.list",
                vec![Parameter::target("local", TargetKind::Local)],
                process_records,
            ),
            signature(
                "net.probe",
                vec![
                    Parameter::target("host", TargetKind::Host),
                    Parameter::value(
                        "port",
                        Type::Count {
                            minimum: 1,
                            maximum: 65535,
                        },
                        true,
                    ),
                    Parameter::value(
                        "timeout_ms",
                        Type::Count {
                            minimum: 1,
                            maximum: u64::MAX,
                        },
                        false,
                    ),
                ],
                record([
                    ("reachable", Type::Bool),
                    ("latency_ms", Type::Number),
                    ("error", Type::OneOf(vec![Type::Null, Type::String])),
                ]),
            ),
            signature(
                "sys.sample",
                vec![
                    Parameter::target("local", TargetKind::Local),
                    Parameter::value(
                        "limit",
                        Type::Count {
                            minimum: 1,
                            maximum: 32,
                        },
                        true,
                    ),
                ],
                record([
                    ("sample_ms", Type::Number),
                    (
                        "machine",
                        record([
                            ("cpu_pct", Type::Number),
                            ("memory_total_bytes", Type::Integer),
                            ("memory_available_bytes", Type::Integer),
                        ]),
                    ),
                    (
                        "cpu",
                        list_record([
                            ("pid", Type::Integer),
                            ("process_key", Type::String),
                            ("name", Type::String),
                            ("cpu_pct", Type::Number),
                        ]),
                    ),
                    (
                        "memory",
                        list_record([
                            ("pid", Type::Integer),
                            ("process_key", Type::String),
                            ("name", Type::String),
                            ("bytes", Type::Integer),
                        ]),
                    ),
                    ("memory_basis", Type::String),
                ]),
            ),
            signature(
                "data.stats",
                vec![Parameter::value(
                    "values",
                    Type::List(Box::new(aggregate::observation_type())),
                    true,
                )],
                record(aggregate::stat_fields()),
            ),
            signature(
                "data.group_stats",
                vec![
                    Parameter::value(
                        "rounds",
                        Type::List(Box::new(Type::List(Box::new(Type::Any)))),
                        true,
                    ),
                    Parameter::value("key_field", Type::String, true),
                    Parameter::value("value_field", Type::String, true),
                ],
                Type::List(Box::new(record(
                    std::iter::once(("key", Type::Any)).chain(aggregate::stat_fields()),
                ))),
            ),
        ];
        Self(actions)
    }
}

impl Default for CoreActions {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionProvider for CoreActions {
    fn signatures(&self) -> &[ActionSignature] {
        &self.0
    }

    fn invoke(
        &self,
        action: &str,
        arguments: &[Argument],
        context: &ActionContext,
    ) -> Result<Value, Error> {
        let signature = self
            .0
            .iter()
            .find(|signature| signature.name == action)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::EUnknownAction,
                    format!("Unknown core action {action}"),
                )
            })?;
        signature.check_arguments(arguments)?;
        context.remaining()?;
        match (action, arguments.first()) {
            ("file.read", Some(Argument::Target(Target::File { path }))) => {
                let lines =
                    matches!(arguments.get(1), Some(Argument::Value(mode)) if mode == "lines");
                read_file(path, lines, context)
            }
            ("dir.list", Some(Argument::Target(Target::Dir { path }))) => {
                list_directory(path, context)
            }
            ("proc.status", Some(Argument::Target(Target::Process { process }))) => {
                process::status(process, context)
            }
            ("proc.list", Some(Argument::Target(Target::Local {}))) => process::list(context),
            ("net.probe", Some(Argument::Target(Target::Host { host }))) => {
                let port = unsigned_argument(arguments, 1)
                    .and_then(|value| u16::try_from(value).ok())
                    .ok_or_else(|| {
                        Error::new(ErrorCode::EArgType, "net.probe requires a valid TCP port")
                    })?;
                let timeout = unsigned_argument(arguments, 2).unwrap_or(3000);
                network::probe(host, port, timeout, context)
            }
            ("sys.sample", Some(Argument::Target(Target::Local {}))) => {
                let limit = unsigned_argument(arguments, 1)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| {
                        Error::new(ErrorCode::EArgType, "sys.sample requires an integer limit")
                    })?;
                sample::sample(limit, context)
            }
            ("data.stats", Some(Argument::Value(values))) => aggregate::stats(values, context),
            ("data.group_stats", Some(Argument::Value(rounds))) => {
                let (Some(Argument::Value(key)), Some(Argument::Value(value))) =
                    (arguments.get(1), arguments.get(2))
                else {
                    return Err(Error::new(
                        ErrorCode::EArgType,
                        "group_stats requires two field names",
                    ));
                };
                aggregate::group_stats(rounds, key, value, context)
            }
            _ => Err(Error::new(
                ErrorCode::ETargetType,
                format!("Invalid target for {action}"),
            )),
        }
    }
}

fn unsigned_argument(arguments: &[Argument], index: usize) -> Option<u64> {
    match arguments.get(index) {
        Some(Argument::Value(value)) => value.as_u64(),
        _ => None,
    }
}

fn signature(name: &str, parameters: Vec<Parameter>, output: Type) -> ActionSignature {
    ActionSignature {
        name: name.to_owned(),
        parameters,
        output,
        poll: None,
    }
}

fn record<'a>(fields: impl IntoIterator<Item = (&'a str, Type)>) -> Type {
    Type::Record(
        fields
            .into_iter()
            .map(|(name, kind)| (name.to_owned(), kind))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn list_record<const N: usize>(fields: [(&str, Type); N]) -> Type {
    Type::List(Box::new(record(fields)))
}

fn read_file(path: &Path, lines: bool, context: &ActionContext) -> Result<Value, Error> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| io_error("read the bound file", &error))?;
    if !file
        .metadata()
        .map_err(|error| io_error("inspect the bound file", &error))?
        .is_file()
    {
        return Err(Error::new(
            ErrorCode::EIo,
            "file.read requires a regular file",
        ));
    }
    let contents = read_bounded(file, context)?;
    let text = String::from_utf8(contents)
        .map_err(|_| Error::new(ErrorCode::EIo, "The bound file is not UTF-8"))?;
    if lines {
        Ok(Value::Array(
            text.lines()
                .map(|line| Value::String(line.to_owned()))
                .collect(),
        ))
    } else {
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
}

fn read_bounded(file: File, context: &ActionContext) -> Result<Vec<u8>, Error> {
    let mut input = file.take(context.max_bytes.saturating_add(1) as u64);
    let mut contents = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        context.remaining()?;
        let count = input
            .read(&mut buffer)
            .map_err(|error| io_error("read the bound file", &error))?;
        if count == 0 {
            return Ok(contents);
        }
        if contents.len() + count > context.max_bytes {
            return Err(data_limit());
        }
        contents.extend_from_slice(&buffer[..count]);
    }
}

fn list_directory(path: &Path, context: &ActionContext) -> Result<Value, Error> {
    let mut entries = Vec::new();
    let mut size = 0_usize;
    let directory =
        fs::read_dir(path).map_err(|error| io_error("list the bound directory", &error))?;
    for entry in directory {
        context.remaining()?;
        let entry = entry.map_err(|error| io_error("read a directory entry", &error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        size = size.saturating_add(name.len() + 40);
        if size > context.max_bytes {
            return Err(data_limit());
        }
        let kind = entry
            .file_type()
            .map_err(|error| io_error("inspect a directory entry", &error))?;
        let kind = if kind.is_file() {
            "file"
        } else if kind.is_dir() {
            "dir"
        } else {
            "other"
        };
        entries.push((name, kind));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(Value::Array(
        entries
            .into_iter()
            .map(|(name, kind)| json!({ "name": name, "kind": kind }))
            .collect(),
    ))
}

pub(crate) fn io_error(operation: &str, error: &std::io::Error) -> Error {
    Error::new(
        ErrorCode::EIo,
        format!("Unable to {operation} ({})", error.kind()),
    )
}

pub(crate) fn data_limit() -> Error {
    Error::new(
        ErrorCode::EOutputLimit,
        "Core observation exceeds the runtime data limit",
    )
}
