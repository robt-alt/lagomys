use std::{collections::BTreeMap, fmt::Write};

use curl::easy::{Easy, List};
use ops_actions::{ActionContext, ActionProvider};
use ops_spec::{
    is_name, pick, ActionSignature, Argument, Error, ErrorCode, Parameter, TargetKind, Type, Value,
};
use serde::Deserialize;

use crate::{invalid, Credentials};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    #[serde(rename = "spec")]
    _spec: String,
    #[serde(rename = "module")]
    _module: String,
    actions: Vec<Descriptor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    name: String,
    params: Vec<Param>,
    target: Option<TargetParam>,
    request: Request,
    result: Option<ResultField>,
    credential: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Param {
    name: String,
    #[serde(rename = "type")]
    kind: ParamType,
    required: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ParamType {
    String,
    Number,
    Bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetParam {
    param: String,
    kind: TargetKind,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    method: String,
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultField {
    pick: Option<String>,
}

enum UrlPart {
    Literal(String),
    Parameter(usize),
}

struct Endpoint {
    method: String,
    url: Vec<UrlPart>,
    pick: Option<String>,
    credential: Option<String>,
}

pub(crate) struct HttpProvider {
    signatures: Vec<ActionSignature>,
    endpoints: BTreeMap<String, Endpoint>,
    credentials: Credentials,
}

impl HttpProvider {
    pub(crate) fn new(
        module: &str,
        manifest: Manifest,
        credentials: Credentials,
    ) -> Result<Self, Error> {
        let mut signatures = Vec::new();
        let mut endpoints = BTreeMap::new();
        for action in manifest.actions {
            let name = crate::action_name(module, &action.name)?;
            let mut parameters: Vec<Parameter> = Vec::new();
            for param in action.params {
                if !is_name(&param.name) || parameters.iter().any(|entry| entry.name == param.name)
                {
                    return Err(invalid(format!("Invalid or duplicate parameter in {name}")));
                }
                let kind = match param.kind {
                    ParamType::String => Type::String,
                    ParamType::Number => Type::Number,
                    ParamType::Bool => Type::Bool,
                };
                parameters.push(Parameter::value(param.name, kind, param.required));
            }
            if let Some(target) = action.target {
                let parameter = parameters
                    .iter_mut()
                    .find(|entry| entry.name == target.param)
                    .ok_or_else(|| invalid(format!("Undeclared target parameter in {name}")))?;
                parameter.target = Some(target.kind);
            }
            if !is_http_token(&action.request.method) {
                return Err(invalid(format!("Invalid HTTP method in {name}")));
            }
            let url = template(&action.request.url, &mut parameters)?;
            if let Some(credential) = &action.credential {
                let value = credentials.get(credential).ok_or_else(|| {
                    invalid(format!("Missing credential {credential} for {name}"))
                })?;
                if value.bytes().any(|byte| byte.is_ascii_control()) {
                    return Err(invalid(format!(
                        "Invalid credential {credential} for {name}"
                    )));
                }
            }
            let endpoint = Endpoint {
                method: action.request.method,
                url,
                pick: action.result.and_then(|result| result.pick),
                credential: action.credential,
            };
            if endpoints.insert(name.clone(), endpoint).is_some() {
                return Err(invalid(format!("Duplicate action {name}")));
            }
            signatures.push(ActionSignature {
                name,
                parameters,
                output: Type::Any,
                poll: None,
            });
        }
        Ok(Self {
            signatures,
            endpoints,
            credentials,
        })
    }
}

impl ActionProvider for HttpProvider {
    fn signatures(&self) -> &[ActionSignature] {
        &self.signatures
    }

    fn invoke(
        &self,
        name: &str,
        arguments: &[Argument],
        context: &ActionContext,
    ) -> Result<Value, Error> {
        let endpoint = self.endpoints.get(name).ok_or_else(|| {
            Error::new(
                ErrorCode::EUnknownAction,
                format!("Unknown HTTP action {name}"),
            )
        })?;
        let url = render(&endpoint.url, arguments)?;
        let mut request = Easy::new();
        request.url(&url).map_err(http_error)?;
        request
            .custom_request(&endpoint.method)
            .map_err(http_error)?;
        request.timeout(context.remaining()?).map_err(http_error)?;
        request.follow_location(false).map_err(http_error)?;
        if let Some(credential) = &endpoint.credential {
            let value = self
                .credentials
                .get(credential)
                .ok_or_else(|| invalid("Missing HTTP credential"))?;
            let mut headers = List::new();
            headers
                .append(&format!("Authorization: Bearer {value}"))
                .map_err(http_error)?;
            request.http_headers(headers).map_err(http_error)?;
        }
        let mut body = Vec::new();
        let mut truncated = false;
        let result = {
            let mut transfer = request.transfer();
            transfer
                .write_function(|part| {
                    if body.len().saturating_add(part.len()) > context.max_bytes {
                        truncated = true;
                        return Ok(0);
                    }
                    body.extend_from_slice(part);
                    Ok(part.len())
                })
                .map_err(http_error)?;
            transfer.perform()
        };
        if truncated {
            return Err(Error::new(
                ErrorCode::EOutputLimit,
                "HTTP response exceeds the runtime data limit",
            ));
        }
        if let Err(error) = result {
            if error.is_operation_timedout() {
                return Err(Error::new(
                    ErrorCode::EBudget,
                    "HTTP action exceeded the execution deadline",
                ));
            }
            return Err(http_error(error));
        }
        let response = request.response_code().map_err(http_error)?;
        if !(200..300).contains(&response) {
            return Err(Error::new(
                ErrorCode::EActionFailed,
                format!("HTTP action returned status {response}"),
            )
            .module_code(Some(Value::from(response))));
        }
        let value = serde_json::from_slice(&body).map_err(|_| {
            Error::new(
                ErrorCode::EActionFailed,
                "HTTP action returned an invalid JSON response",
            )
        })?;
        if let Some(path) = &endpoint.pick {
            pick(&value, path)
        } else {
            Ok(value)
        }
    }
}

fn template(text: &str, parameters: &mut [Parameter]) -> Result<Vec<UrlPart>, Error> {
    let (scheme, host_and_path) = text
        .split_once("://")
        .ok_or_else(|| invalid("HTTP URL requires a scheme"))?;
    if !matches!(scheme, "http" | "https")
        || text
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ' || byte == b'\\')
    {
        return Err(invalid("Invalid HTTP URL"));
    }
    let host_size = host_and_path
        .find(['/', '?', '#'])
        .unwrap_or(host_and_path.len());
    let authority_end = scheme.len() + 3 + host_size;
    if host_size == 0 || host_and_path[..host_size].contains('@') {
        return Err(invalid(
            "HTTP URL requires an authority without credentials",
        ));
    }
    let mut parts = Vec::new();
    let mut offset = 0;
    while let Some(relative) = text[offset..].find('{') {
        let open = offset + relative;
        let literal = &text[offset..open];
        if literal.contains('}') {
            return Err(invalid("Unmatched URL template delimiter"));
        }
        parts.push(UrlPart::Literal(literal.to_owned()));
        let (key, _) = text[open + 1..]
            .split_once('}')
            .ok_or_else(|| invalid("Unclosed URL template parameter"))?;
        let index = parameters
            .iter()
            .position(|param| param.name == key)
            .ok_or_else(|| invalid(format!("Undeclared URL parameter {key}")))?;
        let parameter = &mut parameters[index];
        if open < authority_end && parameter.target != Some(TargetKind::Host) {
            return Err(invalid(
                "A variable HTTP authority requires a bound host target",
            ));
        }
        parameter.required = true;
        parts.push(UrlPart::Parameter(index));
        offset = open + key.len() + 2;
    }
    if text[offset..].contains('}') {
        return Err(invalid("Unmatched URL template delimiter"));
    }
    parts.push(UrlPart::Literal(text[offset..].to_owned()));
    Ok(parts)
}

fn render(parts: &[UrlPart], arguments: &[Argument]) -> Result<String, Error> {
    let mut url = String::new();
    for part in parts {
        match part {
            UrlPart::Literal(text) => url.push_str(text),
            UrlPart::Parameter(index) => {
                let value = arguments
                    .get(*index)
                    .ok_or_else(|| invalid("Missing URL parameter"))?
                    .value();
                let text = if let Value::String(text) = value {
                    text
                } else {
                    value.to_string()
                };
                for byte in text.bytes() {
                    if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                        url.push(char::from(byte));
                    } else {
                        let _ = write!(url, "%{byte:02X}");
                    }
                }
            }
        }
    }
    Ok(url)
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
}

fn http_error(error: curl::Error) -> Error {
    Error::new(
        ErrorCode::EActionFailed,
        format!("HTTP transport failed (code {})", error.code()),
    )
}
