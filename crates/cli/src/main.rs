#![forbid(unsafe_code)]

use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

use ops_actions::Registry;
use ops_core_actions::CoreActions;
use ops_modules::Secrets;
use ops_spec::{Binding, Error, ErrorCode, RunResult, Status, MAX_DATA_BYTES, VERSION};
use ops_syntax::MAX_SOURCE_BYTES;

const USAGE: &str = "Usage: lagomys <script | -> <binding.json> [module.json ...]";

fn main() -> ExitCode {
    let mut arguments = std::env::args_os().skip(1);
    let script = arguments.next();
    let binding = arguments.next();
    if binding.is_none() {
        match script.as_deref().and_then(OsStr::to_str) {
            Some("--help") | Some("-h") => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            Some("--version") | Some("-V") => {
                println!("lagomys {} (spec {VERSION})", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            _ => {
                return emit(
                    failed(Error::new(ErrorCode::EArgCount, USAGE)),
                    MAX_DATA_BYTES,
                    &[],
                )
            }
        }
    }

    let (Some(script), Some(binding)) = (script, binding) else {
        return ExitCode::FAILURE;
    };
    let binding = match input(Path::new(&binding), MAX_DATA_BYTES)
        .and_then(|bytes| Binding::from_json(&bytes))
    {
        Ok(binding) => binding,
        Err(error) => return emit(failed(error), MAX_DATA_BYTES, &[]),
    };
    let secrets = match Secrets::resolve(&binding) {
        Ok(secrets) => secrets,
        Err(error) => return emit(failed(error), binding.budget.output_bytes, &[]),
    };
    let paths = arguments.map(PathBuf::from).collect::<Vec<_>>();
    let result = run(Path::new(&script), &binding, &paths, &secrets).unwrap_or_else(failed);
    emit(result, binding.budget.output_bytes, secrets.values())
}

fn run(
    source: &Path,
    binding: &Binding,
    modules: &[PathBuf],
    secrets: &Secrets,
) -> Result<RunResult, Error> {
    let started = Instant::now();
    let deadline = started
        .checked_add(Duration::from_secs(binding.budget.seconds))
        .ok_or_else(|| Error::new(ErrorCode::EBudget, "Execution time budget is out of range"))?;
    let bytes = if source == Path::new("-") {
        read_limited(io::stdin().lock(), MAX_SOURCE_BYTES, "script")?
    } else {
        input(source, MAX_SOURCE_BYTES)?
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::new(ErrorCode::EParse, "The script must be UTF-8"))?;
    let script = ops_syntax::parse(text)?;
    let mut registry = Registry::default();
    registry.register(None, CoreActions::new())?;
    ops_modules::load(modules, binding, secrets, &mut registry, deadline)?;
    let plan = ops_validate::validate(script, binding, &registry)?;
    Ok(ops_runtime::execute_from(&plan, started))
}

fn input(path: &Path, limit: usize) -> Result<Vec<u8>, Error> {
    let input = File::open(path)
        .map_err(|_| Error::new(ErrorCode::EIo, format!("Unable to open {}", path.display())))?;
    if !input.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(Error::new(
            ErrorCode::EIo,
            format!("Input {} must be a regular file", path.display()),
        ));
    }
    read_limited(input, limit, "input file")
}

fn read_limited(input: impl Read, limit: usize, name: &str) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    input
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::new(ErrorCode::EIo, format!("Unable to read the {name}")))?;
    if bytes.len() > limit {
        return Err(Error::new(
            ErrorCode::EParse,
            format!("The {name} exceeds the size limit"),
        ));
    }
    Ok(bytes)
}

fn failed(error: Error) -> RunResult {
    let mut result = RunResult::new();
    result.fail(error);
    result
}

fn emit(mut result: RunResult, limit: usize, secrets: &[String]) -> ExitCode {
    result.redact(secrets);
    let Ok(bytes) = result.encode(limit) else {
        return ExitCode::FAILURE;
    };
    let mut output = io::stdout().lock();
    if output
        .write_all(&bytes)
        .and_then(|()| output.write_all(b"\n"))
        .is_err()
    {
        return ExitCode::FAILURE;
    }
    if result.status == Status::Success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
