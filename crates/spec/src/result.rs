use serde::Serialize;

use crate::{Error, ErrorCode, Value, VERSION};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Success,
    Failed,
    Timeout,
    Pending,
}

#[derive(Clone, Debug, Serialize)]
pub struct Task {
    pub id: Value,
    pub module: String,
    pub poll: String,
}

#[derive(Debug, Serialize)]
pub struct ObservedValue {
    pub expr: String,
    pub value: Value,
}

#[derive(Debug, Serialize)]
pub struct Assertion {
    pub expr: String,
    pub passed: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct Usage {
    pub seconds: f64,
    pub actions: u64,
    pub output_bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct RunResult {
    pub spec: &'static str,
    pub status: Status,
    pub values: Vec<ObservedValue>,
    pub asserts: Vec<Assertion>,
    pub task: Option<Task>,
    pub error: Option<Error>,
    pub evidence: Vec<Value>,
    pub usage: Usage,
}

impl RunResult {
    pub fn new() -> Self {
        Self {
            spec: VERSION,
            status: Status::Success,
            values: Vec::new(),
            asserts: Vec::new(),
            task: None,
            error: None,
            evidence: Vec::new(),
            usage: Usage::default(),
        }
    }

    pub fn fail(&mut self, error: Error) {
        self.status = Status::Failed;
        self.error = Some(error);
    }

    pub fn redact(&mut self, secrets: &[String]) {
        if secrets.is_empty() {
            return;
        }
        for value in &mut self.values {
            sanitize(&mut value.expr, secrets);
            sanitize_value(&mut value.value, secrets);
        }
        for assertion in &mut self.asserts {
            sanitize(&mut assertion.expr, secrets);
        }
        if let Some(error) = &mut self.error {
            sanitize(&mut error.message, secrets);
            if let Some(path) = &mut error.path {
                sanitize(path, secrets);
            }
            if let Some(code) = &mut error.module_code {
                sanitize_value(code, secrets);
            }
        }
        if let Some(task) = &mut self.task {
            sanitize_value(&mut task.id, secrets);
        }
        for evidence in &mut self.evidence {
            sanitize_value(evidence, secrets);
        }
    }

    pub fn encode(&mut self, limit: usize) -> Result<Vec<u8>, serde_json::Error> {
        let bytes = self.encode_counted()?;
        if bytes.len() <= limit {
            return Ok(bytes);
        }
        self.values.clear();
        self.asserts.clear();
        self.evidence.clear();
        self.fail(Error::new(
            ErrorCode::EOutputLimit,
            "Execution result exceeded the output byte budget",
        ));
        let bytes = self.encode_counted()?;
        if bytes.len() > limit && self.task.take().is_some() {
            return self.encode_counted();
        }
        Ok(bytes)
    }

    fn encode_counted(&mut self) -> Result<Vec<u8>, serde_json::Error> {
        self.usage.output_bytes = 0;
        loop {
            let bytes = serde_json::to_vec(&self)?;
            if bytes.len() == self.usage.output_bytes {
                return Ok(bytes);
            }
            self.usage.output_bytes = bytes.len();
        }
    }
}

impl Default for RunResult {
    fn default() -> Self {
        Self::new()
    }
}

fn sanitize(value: &mut String, secrets: &[String]) {
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        if value.contains(secret) {
            *value = value.replace(secret, "[REDACTED]");
        }
    }
}

fn sanitize_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(value) => sanitize(value, secrets),
        Value::Array(list) => list
            .iter_mut()
            .for_each(|item| sanitize_value(item, secrets)),
        Value::Object(fields) => {
            for (mut key, mut entry) in std::mem::take(fields) {
                sanitize(&mut key, secrets);
                sanitize_value(&mut entry, secrets);
                fields.insert(key, entry);
            }
        }
        _ => {}
    }
}
