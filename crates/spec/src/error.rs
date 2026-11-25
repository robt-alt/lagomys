use serde::{Deserialize, Serialize};

use crate::Value;

pub const MAX_AVAILABLE_FIELDS: usize = 20;
pub const MAX_DETAIL_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    EParse,
    EUnknownInstruction,
    EUnknownAction,
    EModuleMissing,
    ETargetMissing,
    ETargetType,
    EArgCount,
    EArgType,
    ERef,
    EEgress,
    EBudget,
    EFieldMissing,
    EActionFailed,
    ECancelled,
    EOutputLimit,
    EIo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AvailableFields {
    pub names: Vec<String>,
    pub total: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<AvailableFields>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_code: Option<Box<Value>>,
    // Boxed so the common Result<_, Error> path stays small; flatten keeps the wire format flat.
    #[serde(flatten, deserialize_with = "deserialize_diagnostics")]
    pub diagnostics: Option<Box<Diagnostics>>,
}

fn deserialize_diagnostics<'de, D>(deserializer: D) -> Result<Option<Box<Diagnostics>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Flattening presents an empty map when no diagnostic fields are present.
    match Diagnostics::deserialize(deserializer)? {
        Diagnostics {
            action: None,
            expected: None,
            actual: None,
            available: None,
            detail: None,
        } => Ok(None),
        diagnostics => Ok(Some(Box::new(diagnostics))),
    }
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            line: None,
            column: None,
            path: None,
            module_code: None,
            diagnostics: None,
        }
    }

    pub fn at(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    pub fn column(mut self, column: usize) -> Self {
        self.column = Some(column);
        self
    }

    pub fn field(path: impl Into<String>) -> Self {
        let path = path.into();
        let mut error = Self::new(ErrorCode::EFieldMissing, format!("Missing field {path}"));
        error.path = Some(path);
        error
    }

    pub fn module_code(mut self, code: Option<Value>) -> Self {
        self.module_code = code.map(Box::new);
        self
    }

    fn diagnostics_mut(&mut self) -> &mut Diagnostics {
        self.diagnostics.get_or_insert_with(Default::default)
    }

    pub fn action(mut self, name: impl Into<String>) -> Self {
        self.diagnostics_mut().action = Some(name.into());
        self
    }

    pub fn expected(mut self, kind: impl Into<String>) -> Self {
        self.diagnostics_mut().expected = Some(kind.into());
        self
    }

    pub fn actual(mut self, kind: impl Into<String>) -> Self {
        self.diagnostics_mut().actual = Some(kind.into());
        self
    }

    pub fn available(mut self, names: Vec<String>) -> Self {
        let total = names.len();
        self.diagnostics_mut().available = Some(AvailableFields {
            names: names.into_iter().take(MAX_AVAILABLE_FIELDS).collect(),
            total,
        });
        self
    }

    pub fn detail(mut self, text: impl Into<String>) -> Self {
        let mut text = text.into();
        if text.len() > MAX_DETAIL_BYTES {
            let mut end = MAX_DETAIL_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        self.diagnostics_mut().detail = Some(text);
        self
    }
}
