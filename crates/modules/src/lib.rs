#![forbid(unsafe_code)]

mod descriptor;
mod mcp;
mod transport;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};

use ops_actions::Registry;
use ops_spec::{is_name, ActionSignature, Binding, Error, ErrorCode, Value, VERSION};
use serde::Deserialize;

type Credentials = BTreeMap<String, String>;

#[derive(Default)]
pub struct Secrets {
    modules: BTreeMap<String, Credentials>,
    values: Vec<String>,
}

impl Secrets {
    pub fn resolve(binding: &Binding) -> Result<Self, Error> {
        let mut secrets = Self::default();
        for (module, variables) in &binding.credentials {
            if !binding.modules.contains(module) {
                continue;
            }
            let mut credentials = BTreeMap::new();
            for (name, reference) in variables {
                let variable = reference
                    .strip_prefix('$')
                    .filter(|value| is_name(value))
                    .ok_or_else(|| {
                        invalid("Credentials must be environment variable references")
                    })?;
                if !is_name(name) {
                    return Err(invalid("Invalid credential name"));
                }
                let value = std::env::var(variable)
                    .ok()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        invalid(format!(
                            "Credential {name} is unavailable for module {module}"
                        ))
                    })?;
                secrets.values.push(value.clone());
                credentials.insert(name.clone(), value);
            }
            secrets.modules.insert(module.clone(), credentials);
        }
        secrets
            .values
            .sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.values.dedup();
        Ok(secrets)
    }

    pub fn values(&self) -> &[String] {
        &self.values
    }

    fn for_module(&self, module: &str) -> Credentials {
        self.modules.get(module).cloned().unwrap_or_default()
    }
}

#[derive(Deserialize)]
struct Header {
    spec: String,
    module: String,
}

pub fn load(
    paths: &[impl AsRef<Path>],
    binding: &Binding,
    secrets: &Secrets,
    registry: &mut Registry,
    deadline: Instant,
) -> Result<(), Error> {
    let mut manifests = BTreeMap::new();
    for path in paths {
        let path = path.as_ref();
        let bytes = read_manifest(path)?;
        let header: Header = decode(&bytes, "module header")?;
        if !binding.modules.contains(&header.module) {
            continue;
        }
        if header.spec != VERSION || !is_name(&header.module) {
            return Err(invalid("Invalid module name or specification version"));
        }
        if manifests.insert(header.module.clone(), bytes).is_some() {
            return Err(invalid(format!("Duplicate module {}", header.module)));
        }
    }

    for module in &binding.modules {
        if !manifests.contains_key(module) {
            return Err(Error::new(
                ErrorCode::EModuleMissing,
                format!("Module {module} requires a manifest"),
            ));
        }
    }
    for (module, actions) in &binding.allow {
        if let Some(name) = actions.first().filter(|_| !manifests.contains_key(module)) {
            return Err(unknown_mount_action(module, name));
        }
    }
    for (module, bytes) in manifests {
        let value: Value = decode(&bytes, "module manifest")?;
        let credentials = secrets.for_module(&module);
        let allow = binding.allow.get(&module).map(Vec::as_slice);
        if value.get("command").is_some() {
            let manifest = decode(&bytes, "MCP mount")?;
            let mut provider = mcp::McpProvider::mount(&module, manifest, credentials, deadline)?;
            apply_allow(registry, &module, &mut provider.signatures, allow)?;
            registry.register(Some(module), provider)?;
        } else {
            let manifest = decode(&bytes, "HTTP module")?;
            let mut provider =
                descriptor::HttpProvider::new(&module, manifest, credentials, &binding.targets)?;
            apply_allow(registry, &module, &mut provider.signatures, allow)?;
            registry.register(Some(module), provider)?;
        }
    }
    Ok(())
}

fn apply_allow(
    registry: &mut Registry,
    module: &str,
    signatures: &mut Vec<ActionSignature>,
    allow: Option<&[String]>,
) -> Result<(), Error> {
    let Some(allow) = allow else { return Ok(()) };
    let mounted = signatures
        .iter()
        .map(|signature| (signature.name.as_str(), signature))
        .collect::<BTreeMap<_, _>>();
    let allowed = allow.iter().map(String::as_str).collect::<BTreeSet<_>>();
    for name in allow {
        let signature = mounted
            .get(name.as_str())
            .ok_or_else(|| unknown_mount_action(module, name))?;
        if let Some(poll) = &signature.poll {
            if !allowed.contains(poll.as_str()) {
                return Err(Error::new(
                    ErrorCode::EUnknownAction,
                    format!("Allowed action {name} requires poll action {poll} in allow"),
                ));
            }
        }
    }
    let mut filtered = Vec::new();
    signatures.retain(|signature| {
        if allowed.contains(signature.name.as_str()) {
            true
        } else {
            filtered.push(signature.name.clone());
            false
        }
    });
    registry.record_filtered(filtered);
    Ok(())
}

fn unknown_mount_action(module: &str, name: &str) -> Error {
    Error::new(
        ErrorCode::EUnknownAction,
        format!("Allow list for module {module} names unmounted action {name}"),
    )
}

fn read_manifest(path: &Path) -> Result<Vec<u8>, Error> {
    use std::{fs::File, io::Read};
    let file = File::open(path).map_err(|_| {
        Error::new(
            ErrorCode::EIo,
            format!("Unable to open module manifest {}", path.display()),
        )
    })?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(Error::new(
            ErrorCode::EIo,
            "Module manifest must be a regular file",
        ));
    }
    let mut contents = Vec::new();
    let mut reader = file.take(ops_spec::MAX_DATA_BYTES as u64 + 1);
    reader
        .read_to_end(&mut contents)
        .map_err(|_| Error::new(ErrorCode::EIo, "Unable to read module manifest"))?;
    if contents.len() > ops_spec::MAX_DATA_BYTES {
        return Err(invalid("Module manifest exceeds the size limit"));
    }
    Ok(contents)
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    context: &str,
) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|error| {
        invalid(format!(
            "Invalid {context} at line {} column {}",
            error.line(),
            error.column()
        ))
    })
}

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::EArgType, message)
}

pub(crate) fn action_name(module: &str, action: &str) -> Result<String, Error> {
    if let Some((namespace, name)) = action.split_once('.') {
        if is_name(namespace) && is_name(name) {
            return Ok(action.to_owned());
        }
    } else if is_name(action) {
        return Ok(format!("{module}.{action}"));
    }
    Err(invalid(format!("Invalid action name {action}")))
}
