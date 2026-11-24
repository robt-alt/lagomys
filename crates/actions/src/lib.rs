#![forbid(unsafe_code)]

mod cancel;

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use ops_spec::{ActionSignature, Argument, Error, ErrorCode, Type, Value};

pub use cancel::CancelSignal;

pub struct ActionContext {
    pub deadline: Instant,
    pub max_bytes: usize,
    pub sequence: u64,
    pub line: usize,
    cancel: Option<CancelSignal>,
}

impl ActionContext {
    pub fn new(
        deadline: Instant,
        max_bytes: usize,
        sequence: u64,
        line: usize,
        cancel: Option<CancelSignal>,
    ) -> Self {
        Self {
            deadline,
            max_bytes,
            sequence,
            line,
            cancel,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancelSignal::is_cancelled)
    }

    pub fn check_active(&self) -> Result<(), Error> {
        if self.is_cancelled() {
            return Err(Error::new(ErrorCode::ECancelled, "Execution cancelled"));
        }
        self.remaining().map(|_| ())
    }

    pub fn remaining(&self) -> Result<Duration, Error> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| Error::new(ErrorCode::EBudget, "Execution deadline reached"))
    }
}

pub trait ActionProvider {
    fn signatures(&self) -> &[ActionSignature];

    fn invoke(
        &self,
        action: &str,
        arguments: &[Argument],
        context: &ActionContext,
    ) -> Result<Value, Error>;
}

struct Provider {
    module: Option<String>,
    implementation: Box<dyn ActionProvider>,
}

#[derive(Default)]
pub struct Registry {
    providers: Vec<Provider>,
    indices: BTreeMap<String, (usize, usize)>,
    filtered: BTreeSet<String>,
}

impl Registry {
    pub fn register(
        &mut self,
        module: Option<String>,
        implementation: impl ActionProvider + 'static,
    ) -> Result<(), Error> {
        let provider = self.providers.len();
        let mut names = BTreeMap::new();
        for (index, signature) in implementation.signatures().iter().enumerate() {
            if self.indices.contains_key(&signature.name)
                || names
                    .insert(signature.name.clone(), (provider, index))
                    .is_some()
            {
                return Err(Error::new(
                    ErrorCode::EArgType,
                    format!("Duplicate action declaration {}", signature.name),
                ));
            }
        }
        self.indices.extend(names);
        self.providers.push(Provider {
            module,
            implementation: Box::new(implementation),
        });
        Ok(())
    }

    pub fn record_filtered(&mut self, actions: impl IntoIterator<Item = String>) {
        self.filtered.extend(actions);
    }

    pub fn has_module(&self, module: &str) -> bool {
        self.providers
            .iter()
            .any(|entry| entry.module.as_deref() == Some(module))
    }

    pub fn signature(&self, action: &str) -> Option<(&ActionSignature, Option<&str>)> {
        let (provider, index) = self.indices.get(action)?;
        let provider = self.providers.get(*provider)?;
        let signature = provider.implementation.signatures().get(*index)?;
        Some((signature, provider.module.as_deref()))
    }

    pub fn resolve(
        &self,
        action: &str,
        allowed_modules: &[String],
    ) -> Result<&ActionSignature, Error> {
        if let Some((signature, module)) = self.signature(action) {
            if let Some(module) = module {
                if !allowed_modules.iter().any(|allowed| allowed == module) {
                    return Err(Error::new(
                        ErrorCode::EModuleMissing,
                        format!("Module {module} is not bound"),
                    ));
                }
            }
            return Ok(signature);
        }
        if self.filtered.contains(action) {
            return Err(Error::new(
                ErrorCode::EUnknownAction,
                format!("Action {action} is excluded by this run's allow list"),
            ));
        }
        let (namespace, _) = action.split_once('.').ok_or_else(|| unknown(action))?;
        if self.has_module(namespace)
            || self.indices.keys().any(|name| {
                name.split_once('.')
                    .is_some_and(|(prefix, _)| prefix == namespace)
            })
        {
            Err(unknown(action))
        } else {
            Err(Error::new(
                ErrorCode::EModuleMissing,
                format!("No module provides action {action}"),
            ))
        }
    }

    pub fn invoke(
        &self,
        action: &str,
        arguments: &[Argument],
        context: &ActionContext,
    ) -> Result<Value, Error> {
        context.check_active()?;
        let (provider_index, signature_index) =
            self.indices.get(action).ok_or_else(|| unknown(action))?;
        let provider = self
            .providers
            .get(*provider_index)
            .ok_or_else(|| unknown(action))?;
        let signature = provider
            .implementation
            .signatures()
            .get(*signature_index)
            .ok_or_else(|| unknown(action))?;
        signature.check_arguments(arguments)?;
        let value = provider.implementation.invoke(action, arguments, context)?;
        if !signature.output.accepts(&value) {
            return Err(Error::new(
                ErrorCode::EActionFailed,
                format!(
                    "Action {action} executed but its return value does not match the declared output type"
                ),
            )
            .action(action)
            .expected(signature.output.name())
            .actual(Type::of(&value).name()));
        }
        Ok(value)
    }
}

fn unknown(action: &str) -> Error {
    Error::new(
        ErrorCode::EUnknownAction,
        format!("Unknown action {action}"),
    )
}
