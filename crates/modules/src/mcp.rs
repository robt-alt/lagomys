use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    time::Instant,
};

use ops_actions::{ActionContext, ActionProvider};
use ops_spec::{
    ActionSignature, Argument, Error, ErrorCode, Parameter, TargetKind, Type, Value, VERSION,
};
use serde::Deserialize;
use serde_json::json;

use crate::{invalid, transport::Session, Credentials};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    #[serde(rename = "spec")]
    _spec: String,
    #[serde(rename = "module")]
    _module: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default, deserialize_with = "explicit_mappings")]
    actions: Option<Vec<Mapping>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mapping {
    name: String,
    tool: String,
    #[serde(default)]
    params: Vec<String>,
    #[serde(default)]
    targets: BTreeMap<String, TargetKind>,
    poll: Option<String>,
}

#[derive(Deserialize)]
struct Tool {
    name: String,
    #[serde(rename = "inputSchema")]
    input: Value,
    #[serde(rename = "outputSchema", default)]
    output: Value,
}

pub(crate) struct McpProvider {
    pub(super) signatures: Vec<ActionSignature>,
    tools: BTreeMap<String, String>,
    session: RefCell<Session>,
}

impl McpProvider {
    pub(crate) fn mount(
        module: &str,
        manifest: Manifest,
        credentials: Credentials,
        deadline: Instant,
    ) -> Result<Self, Error> {
        if manifest.command.is_empty() {
            return Err(invalid("MCP mount requires a command"));
        }
        let mut session =
            Session::spawn(OsStr::new(&manifest.command), &manifest.args, &credentials)?;
        let initialized = session.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "lagomys", "version": VERSION}
            }),
            deadline,
        )?;
        if !matches!(
            initialized.get("protocolVersion").and_then(Value::as_str),
            Some("2024-10-07" | "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25")
        ) {
            return Err(invalid(
                "MCP module selected an unsupported protocol version",
            ));
        }
        session.initialized(deadline)?;
        let catalog = discover(&mut session, deadline)?;
        let mappings = match manifest.actions {
            None => automatic_mappings(&catalog, true),
            // An explicitly empty list keeps the existing discovery behavior.
            Some(mappings) if mappings.is_empty() => automatic_mappings(&catalog, false),
            Some(mappings) => mappings,
        };
        let mut signatures = Vec::new();
        let mut tools = BTreeMap::new();
        for mapping in mappings {
            let tool = catalog.get(&mapping.tool).ok_or_else(|| {
                Error::new(
                    ErrorCode::EUnknownAction,
                    format!("MCP module {module} has no tool {}", mapping.tool),
                )
            })?;
            let signature = signature(module, tool, &mapping)?;
            if tools.insert(signature.name.clone(), mapping.tool).is_some() {
                return Err(invalid(format!("Duplicate action {}", signature.name)));
            }
            signatures.push(signature);
        }
        Ok(Self {
            signatures,
            tools,
            session: RefCell::new(session),
        })
    }
}

fn explicit_mappings<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<Mapping>>, D::Error> {
    Vec::deserialize(deserializer).map(Some)
}

fn automatic_mappings(catalog: &BTreeMap<String, Tool>, normalize: bool) -> Vec<Mapping> {
    let mut used = BTreeMap::new();
    catalog
        .keys()
        .map(|tool| Mapping {
            name: if normalize {
                normalized_name(tool, &mut used)
            } else {
                tool.clone()
            },
            tool: tool.clone(),
            params: Vec::new(),
            targets: BTreeMap::new(),
            poll: None,
        })
        .collect()
}

fn normalized_name(tool: &str, used: &mut BTreeMap<String, usize>) -> String {
    let mut base = String::new();
    if tool.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        base.push('_');
    }
    for character in tool.chars() {
        base.push(if character.is_ascii_alphanumeric() {
            character
        } else {
            '_'
        });
    }
    let mut name = base.clone();
    let mut suffix = used.get(&base).copied().unwrap_or(2);
    while used.contains_key(&name) {
        name = format!("{base}_{suffix}");
        suffix += 1;
    }
    if name != base {
        used.insert(base, suffix);
    }
    used.insert(name.clone(), 2);
    name
}

impl ActionProvider for McpProvider {
    fn signatures(&self) -> &[ActionSignature] {
        &self.signatures
    }

    fn invoke(
        &self,
        name: &str,
        arguments: &[Argument],
        context: &ActionContext,
    ) -> Result<Value, Error> {
        let tool = self.tools.get(name).ok_or_else(|| {
            Error::new(
                ErrorCode::EUnknownAction,
                format!("Unknown MCP action {name}"),
            )
        })?;
        let signature = self
            .signatures
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::EUnknownAction,
                    format!("Unknown MCP action {name}"),
                )
            })?;
        let parameters: serde_json::Map<String, Value> = signature
            .parameters
            .iter()
            .zip(arguments)
            .map(|(parameter, value)| (parameter.name.clone(), value.value()))
            .collect();
        let mut session = self
            .session
            .try_borrow_mut()
            .map_err(|_| Error::new(ErrorCode::EActionFailed, "MCP module is busy"))?;
        let response = session.request(
            "tools/call",
            json!({"name": tool, "arguments": parameters}),
            context.deadline,
        )?;
        let value = content(&response)?;
        if response.get("isError").and_then(Value::as_bool) == Some(true) {
            let code = value.get("code").cloned();
            return Err(Error::new(
                ErrorCode::EActionFailed,
                format!("MCP action {name} returned an error"),
            )
            .module_code(code));
        }
        Ok(value)
    }
}

fn discover(session: &mut Session, deadline: Instant) -> Result<BTreeMap<String, Tool>, Error> {
    let mut catalog = BTreeMap::new();
    let mut cursor = None;
    let mut seen = BTreeSet::new();
    loop {
        let parameters = cursor
            .as_ref()
            .map_or_else(|| json!({}), |value| json!({"cursor":value}));
        let result = session.request("tools/list", parameters, deadline)?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("MCP tools/list must return a tool list"))?;
        for value in tools {
            let tool: Tool = serde_json::from_value(value.clone())
                .map_err(|_| invalid("Invalid MCP tool declaration"))?;
            if catalog.insert(tool.name.clone(), tool).is_some() || catalog.len() > 4096 {
                return Err(invalid("Invalid or oversized MCP tool list"));
            }
        }
        cursor = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(next) = &cursor else { break };
        if !seen.insert(next.clone()) || seen.len() >= 256 {
            return Err(invalid("Invalid MCP tool pagination"));
        }
    }
    Ok(catalog)
}

fn signature(module: &str, tool: &Tool, mapping: &Mapping) -> Result<ActionSignature, Error> {
    if tool.input.get("type").and_then(Value::as_str) != Some("object") {
        return Err(invalid(format!(
            "MCP tool {} requires an object input schema",
            tool.name
        )));
    }
    let empty = serde_json::Map::new();
    let properties = match tool.input.get("properties") {
        Some(value) => value
            .as_object()
            .ok_or_else(|| invalid("Invalid MCP tool properties"))?,
        None => &empty,
    };
    let required = required_names(&tool.input)?;
    let order = if mapping.params.is_empty() {
        properties
            .keys()
            .filter(|key| required.contains(key.as_str()))
            .chain(
                properties
                    .keys()
                    .filter(|key| !required.contains(key.as_str())),
            )
            .map(String::as_str)
            .collect::<Vec<_>>()
    } else {
        mapping.params.iter().map(String::as_str).collect()
    };
    let names: BTreeSet<_> = order.iter().copied().collect();
    if order.len() != properties.len()
        || names.len() != order.len()
        || !names.iter().all(|name| properties.contains_key(*name))
        || !required.is_subset(&names)
        || mapping
            .targets
            .keys()
            .any(|name| !names.contains(name.as_str()))
    {
        return Err(invalid(format!(
            "Invalid parameter mapping for MCP tool {}",
            tool.name
        )));
    }
    let parameters = order
        .into_iter()
        .map(|name| {
            Ok(Parameter {
                name: name.to_owned(),
                value_type: schema_type(&properties[name], 0)?,
                required: required.contains(name),
                target: mapping.targets.get(name).copied(),
            })
        })
        .collect::<Result<_, Error>>()?;

    Ok(ActionSignature {
        name: crate::action_name(module, &mapping.name)?,
        parameters,
        output: schema_type(&tool.output, 0)?,
        poll: mapping
            .poll
            .as_ref()
            .map(|name| crate::action_name(module, name))
            .transpose()?,
    })
}

fn required_names(schema: &Value) -> Result<BTreeSet<&str>, Error> {
    match schema.get("required") {
        None => Ok(BTreeSet::new()),
        Some(value) => value
            .as_array()
            .ok_or_else(|| invalid("Invalid required property list"))?
            .iter()
            .map(|name| {
                name.as_str()
                    .ok_or_else(|| invalid("Required properties must be named"))
            })
            .collect(),
    }
}

fn schema_type(schema: &Value, depth: usize) -> Result<Type, Error> {
    if depth > 64 {
        return Err(invalid("MCP schema nesting exceeds the size limit"));
    }
    if let Some(value) = schema.get("enum") {
        return value
            .as_array()
            .filter(|values| !values.is_empty())
            .cloned()
            .map(Type::Choices)
            .ok_or_else(|| invalid("Invalid MCP enum"));
    }
    if let Some(value) = schema.get("const") {
        return Ok(Type::Choices(vec![value.clone()]));
    }
    if let Some(branches) = schema.get("anyOf").or_else(|| schema.get("oneOf")) {
        return branches
            .as_array()
            .ok_or_else(|| invalid("Invalid MCP schema alternatives"))?
            .iter()
            .map(|branch| schema_type(branch, depth + 1))
            .collect::<Result<Vec<_>, Error>>()
            .map(Type::OneOf);
    }
    match schema.get("type") {
        None => Ok(Type::Any),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .map(|kind| {
                declared_type(
                    kind.as_str()
                        .ok_or_else(|| invalid("Invalid MCP type declaration"))?,
                    schema,
                    depth,
                )
            })
            .collect::<Result<Vec<_>, Error>>()
            .map(Type::OneOf),
        Some(Value::String(kind)) => declared_type(kind, schema, depth),
        _ => Err(invalid("Invalid MCP type declaration")),
    }
}

fn declared_type(kind: &str, schema: &Value, depth: usize) -> Result<Type, Error> {
    let kind = match kind {
        "null" => Type::Null,
        "boolean" => Type::Bool,
        "number" => Type::Number,
        "integer" => Type::Integer,
        "string" => Type::String,
        "array" => {
            let item = schema
                .get("items")
                .map(|item| schema_type(item, depth + 1))
                .transpose()?
                .unwrap_or(Type::Any);
            Type::List(Box::new(item))
        }
        "object" => {
            let fields = required_names(schema)?
                .into_iter()
                .map(|name| {
                    let property = schema
                        .get("properties")
                        .and_then(|fields| fields.get(name))
                        .unwrap_or(&Value::Null);
                    Ok((name.to_owned(), schema_type(property, depth + 1)?))
                })
                .collect::<Result<_, Error>>()?;
            Type::Record(fields)
        }
        _ => return Err(invalid(format!("Unsupported MCP type {kind}"))),
    };
    Ok(kind)
}

fn content(response: &Value) -> Result<Value, Error> {
    if let Some(value) = response.get("structuredContent") {
        return Ok(value.clone());
    }
    let content = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            Error::new(
                ErrorCode::EActionFailed,
                "MCP tool result is missing content",
            )
        })?;
    if let [entry] = content.as_slice() {
        if entry.get("type").and_then(Value::as_str) == Some("text") {
            let text = entry.get("text").and_then(Value::as_str).ok_or_else(|| {
                Error::new(ErrorCode::EActionFailed, "MCP text result is invalid")
            })?;
            return Ok(serde_json::from_str(text).unwrap_or_else(|_| Value::from(text)));
        }
    }
    Ok(Value::Array(content.clone()))
}
