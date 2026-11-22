use ops_actions::ActionContext;
use ops_spec::{Error, Process, Value};
use serde_json::json;

pub(crate) fn status(target: &Process, context: &ActionContext) -> Result<Value, Error> {
    let mut processes = operating_system(target, context)?;
    processes.sort_unstable_by_key(|process| process.pid);
    Ok(Value::Array(processes.into_iter().map(|process| {
        json!({ "pid": process.pid, "name": process.name, "running": process.running })
    }).collect()))
}

struct Entry {
    pid: u32,
    name: String,
    running: bool,
}

#[cfg(target_os = "linux")]
fn operating_system(target: &Process, context: &ActionContext) -> Result<Vec<Entry>, Error> {
    use std::{fs, io::ErrorKind};

    let mut processes = Vec::new();
    let mut bytes = 0_usize;
    let mut inspect = |pid| -> Result<(), Error> {
        context.remaining()?;
        let path = format!("/proc/{pid}/stat");
        let stat = match fs::read_to_string(&path) {
            Ok(stat) => stat,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::PermissionDenied
                ) =>
            {
                return Ok(())
            }
            Err(error) => return Err(crate::io_error("inspect the bound process", &error)),
        };
        if let Some((head, tail)) = stat.rsplit_once(')') {
            if let Some((_, name)) = head.split_once('(') {
                if matches!(target, Process::Name(expected) if name != expected) {
                    return Ok(());
                }
                bytes = bytes.saturating_add(name.len() + 64);
                if bytes > context.max_bytes {
                    return Err(crate::data_limit());
                }
                processes.push(Entry {
                    pid,
                    name: name.to_owned(),
                    running: !tail.trim_start().starts_with(['Z', 'X', 'x']),
                });
            }
        }
        Ok(())
    };
    match target {
        Process::Pid(pid) => inspect(*pid)?,
        Process::Name(_) => {
            let directory = fs::read_dir("/proc")
                .map_err(|error| crate::io_error("enumerate processes", &error))?;
            for directory_entry in directory {
                context.remaining()?;
                let directory_entry = directory_entry
                    .map_err(|error| crate::io_error("inspect process inventory", &error))?;
                if let Ok(pid) = directory_entry.file_name().to_string_lossy().parse() {
                    inspect(pid)?;
                }
            }
        }
    }
    Ok(processes)
}

#[cfg(target_os = "macos")]
fn operating_system(target: &Process, context: &ActionContext) -> Result<Vec<Entry>, Error> {
    use ops_spec::ErrorCode;
    use std::{
        io::Read,
        path::Path,
        process::{Command, Stdio},
        sync::mpsc,
        time::Duration,
    };

    let mut command = Command::new("/bin/ps");
    match target {
        Process::Pid(pid) => {
            command.arg("-p").arg(pid.to_string());
        }
        Process::Name(_) => {
            command.arg("-ax");
        }
    }
    let mut child = command
        .args(["-o", "pid=,state=,comm="])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| crate::io_error("query processes", &error))?;
    let result = (|| {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::new(ErrorCode::EIo, "Process inventory output is unavailable"))?;
        let (send, receive) = mpsc::sync_channel(1);
        let limit = context.max_bytes;
        std::thread::Builder::new()
            .spawn(move || {
                let mut bytes = Vec::new();
                let read = stdout
                    .take(limit.saturating_add(1) as u64)
                    .read_to_end(&mut bytes);
                let _ = send.send(read.map(|_| bytes));
            })
            .map_err(|error| crate::io_error("read process inventory", &error))?;
        let bytes = receive
            .recv_timeout(context.remaining()?)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => Error::new(
                    ErrorCode::EBudget,
                    "Process inventory exceeded the execution deadline",
                ),
                mpsc::RecvTimeoutError::Disconnected => {
                    Error::new(ErrorCode::EIo, "Process inventory stopped unexpectedly")
                }
            })?
            .map_err(|error| crate::io_error("read process inventory", &error))?;
        if bytes.len() > limit {
            return Err(crate::data_limit());
        }
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| crate::io_error("read process inventory status", &error))?
            {
                break status;
            }
            std::thread::sleep(context.remaining()?.min(Duration::from_millis(5)));
        };
        if !status.success()
            && !matches!(target, Process::Pid(_) if bytes.is_empty() && status.code() == Some(1))
        {
            return Err(Error::new(
                ErrorCode::EIo,
                "System process inventory failed",
            ));
        }
        let mut entries = Vec::new();
        for line in String::from_utf8_lossy(&bytes).lines() {
            let line = line.trim_start();
            let Some((pid, remaining)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            let Some((state, executable)) = remaining.trim_start().split_once(char::is_whitespace)
            else {
                continue;
            };
            let executable = executable.trim_start();
            let name = Path::new(executable)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(executable);
            if matches!(target, Process::Name(expected) if name != expected && executable != expected)
            {
                continue;
            }
            if let Ok(pid) = pid.parse() {
                entries.push(Entry {
                    pid,
                    name: name.to_owned(),
                    running: !state.starts_with('Z'),
                });
            }
        }
        Ok(entries)
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}
