#![forbid(unsafe_code)]

mod binding;
mod contract;
mod error;
mod path;
mod result;

pub use binding::{Binding, Budget, Process, Target, TargetKind};
pub use contract::{ActionSignature, Argument, Parameter, Type};
pub use error::{Error, ErrorCode};
pub use path::{is_name, pick, truthy, Path, Segment};
pub use result::{
    Assertion, LoopContext, ObservedValue, RepeatReason, RepeatResult, RunResult, Status, Task,
    Usage,
};
pub use serde_json::Value;

pub const VERSION: &str = "0.4";
pub const MAX_DATA_BYTES: usize = 4 * 1024 * 1024;
