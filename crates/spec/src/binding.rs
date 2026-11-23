use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, ErrorCode, Value, VERSION};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub spec: String,
    pub targets: Vec<Target>,
    pub modules: Vec<String>,
    pub budget: Budget,
    #[serde(default)]
    pub credentials: BTreeMap<String, BTreeMap<String, String>>,
}

impl Binding {
    pub fn from_json(bytes: &[u8]) -> Result<Self, Error> {
        let binding: Self = serde_json::from_slice(bytes).map_err(|error| {
            Error::new(
                ErrorCode::EParse,
                format!(
                    "Invalid execution binding at line {} column {}",
                    error.line(),
                    error.column()
                ),
            )
        })?;
        binding.check_version()?;
        Ok(binding)
    }

    pub fn check_version(&self) -> Result<(), Error> {
        if self.spec == VERSION {
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::EArgType,
                format!("Execution binding must use specification {VERSION}"),
            ))
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub seconds: u64,
    pub actions: u64,
    pub output_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    File,
    Dir,
    Process,
    Host,
    Local,
}

impl TargetKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "dir",
            Self::Process => "process",
            Self::Host => "host",
            Self::Local => "local",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Target {
    File { path: PathBuf },
    Dir { path: PathBuf },
    Process { process: Process },
    Host { host: String },
    Local {},
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Process {
    Pid(u32),
    Name(String),
}

impl Target {
    pub fn kind(&self) -> TargetKind {
        match self {
            Self::File { .. } => TargetKind::File,
            Self::Dir { .. } => TargetKind::Dir,
            Self::Process { .. } => TargetKind::Process,
            Self::Host { .. } => TargetKind::Host,
            Self::Local {} => TargetKind::Local,
        }
    }

    pub fn value(&self) -> Value {
        match self {
            Self::File { path } | Self::Dir { path } => {
                Value::String(path.to_string_lossy().into_owned())
            }
            Self::Process {
                process: Process::Pid(pid),
            } => Value::from(*pid),
            Self::Process {
                process: Process::Name(name),
            }
            | Self::Host { host: name } => Value::String(name.clone()),
            Self::Local {} => Value::Null,
        }
    }
}
