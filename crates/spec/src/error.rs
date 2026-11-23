use serde::Serialize;

use crate::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
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
    EOutputLimit,
    EIo,
}

#[derive(Clone, Debug, Serialize)]
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
    pub module_code: Option<Value>,
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
        self.module_code = code;
        self
    }
}
