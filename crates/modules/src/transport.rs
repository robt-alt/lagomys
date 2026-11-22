use std::{
    collections::BTreeMap,
    ffi::OsStr,
    io::{BufRead, BufReader, Read, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    time::Instant,
};

use ops_spec::{Error, ErrorCode, Value, MAX_DATA_BYTES};
use rustix::process::{kill_process_group, Pid, Signal};
use serde_json::json;

pub(crate) struct Session {
    child: Option<Child>,
    requests: SyncSender<Vec<u8>>,
    responses: Receiver<Result<Value, Error>>,
    next_id: u64,
}

impl Session {
    pub(crate) fn spawn(
        command: &OsStr,
        args: &[String],
        credential_env: &BTreeMap<String, String>,
    ) -> Result<Self, Error> {
        let mut process = Command::new(command);
        process
            .args(args)
            .env_clear()
            .envs(credential_env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        if let Some(path) = std::env::var_os("PATH") {
            process.env("PATH", path);
        }
        let mut child = process
            .spawn()
            .map_err(|_| transport("Unable to start the MCP module"))?;
        let result = attach(&mut child);
        match result {
            Ok((requests, responses)) => Ok(Self {
                child: Some(child),
                requests,
                responses,
                next_id: 0,
            }),
            Err(error) => {
                terminate(child);
                Err(error)
            }
        }
    }

    pub(crate) fn request(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, Error> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let result = self
            .send(&request, deadline)
            .and_then(|()| self.receive(id, deadline));
        if result
            .as_ref()
            .is_err_and(|error| error.code == ErrorCode::EBudget)
        {
            if let Some(child) = self.child.take() {
                terminate(child);
            }
        }
        result
    }

    pub(crate) fn initialized(&self, deadline: Instant) -> Result<(), Error> {
        self.send(
            &json!({"jsonrpc":"2.0", "method": "notifications/initialized"}),
            deadline,
        )
    }

    fn send(&self, value: &Value, deadline: Instant) -> Result<(), Error> {
        remaining(deadline)?;
        let mut frame =
            serde_json::to_vec(value).map_err(|_| transport("Unable to encode the MCP request"))?;
        if frame.len() >= MAX_DATA_BYTES {
            return Err(limit());
        }
        frame.push(b'\n');
        self.requests
            .try_send(frame)
            .map_err(|_| transport("MCP module is not receiving requests"))
    }

    fn receive(&self, id: u64, deadline: Instant) -> Result<Value, Error> {
        loop {
            let frame = self
                .responses
                .recv_timeout(remaining(deadline)?)
                .map_err(|error| match error {
                    RecvTimeoutError::Timeout => deadline_error(),
                    RecvTimeoutError::Disconnected => {
                        transport("MCP module exited while responding")
                    }
                })??;
            if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                return Err(transport("MCP response has an invalid JSON-RPC version"));
            }
            if frame.get("id").and_then(Value::as_u64) == Some(id) && frame.get("method").is_none()
            {
                if let Some(error) = frame.get("error") {
                    return Err(transport("MCP module returned an RPC error")
                        .module_code(error.get("code").cloned()));
                }
                return frame
                    .get("result")
                    .cloned()
                    .ok_or_else(|| transport("MCP response is missing result"));
            }
            if frame.get("method").is_none() {
                return Err(transport("MCP response has an unexpected request id"));
            }
            if let Some(server_id) = frame.get("id") {
                self.send(
                    &json!({
                        "jsonrpc": "2.0", "id": server_id,
                        "error": {"code": -32601, "message": "Method not available"}
                    }),
                    deadline,
                )?;
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            terminate(child);
        }
    }
}

type Channels = (SyncSender<Vec<u8>>, Receiver<Result<Value, Error>>);

fn attach(child: &mut Child) -> Result<Channels, Error> {
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| transport("Missing MCP input stream"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| transport("Missing MCP output stream"))?;
    let (requests, write_queue) = mpsc::sync_channel::<Vec<u8>>(4);
    let (read_queue, responses) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .spawn(move || {
            while let Ok(frame) = write_queue.recv() {
                if input
                    .write_all(&frame)
                    .and_then(|()| input.flush())
                    .is_err()
                {
                    return;
                }
            }
        })
        .map_err(|_| transport("Unable to create the MCP writer"))?;
    std::thread::Builder::new()
        .spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let mut frame = Vec::new();
                let response = match reader
                    .by_ref()
                    .take(MAX_DATA_BYTES as u64 + 1)
                    .read_until(b'\n', &mut frame)
                {
                    Ok(0) => break,
                    Ok(count) if count > MAX_DATA_BYTES => Err(limit()),
                    Ok(_) if !frame.ends_with(b"\n") => {
                        Err(transport("Incomplete MCP stdio frame"))
                    }
                    Ok(_) => serde_json::from_slice(&frame)
                        .map_err(|_| transport("Invalid MCP stdio frame")),
                    Err(_) => Err(transport("Unable to read the MCP module response")),
                };
                let failed = response.is_err();
                if read_queue.send(response).is_err() || failed {
                    break;
                }
            }
        })
        .map_err(|_| transport("Unable to create the MCP reader"))?;
    Ok((requests, responses))
}

fn terminate(mut child: Child) {
    // Keep the leader's PID reserved until its process group has been signaled.
    if let Some(group) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) {
        let _ = kill_process_group(group, Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn remaining(deadline: Instant) -> Result<std::time::Duration, Error> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(deadline_error())
    } else {
        Ok(remaining)
    }
}

fn limit() -> Error {
    Error::new(
        ErrorCode::EOutputLimit,
        "MCP frame exceeds the runtime data limit",
    )
}

fn transport(message: &str) -> Error {
    Error::new(ErrorCode::EActionFailed, message)
}

fn deadline_error() -> Error {
    Error::new(
        ErrorCode::EBudget,
        "MCP module exceeded the execution deadline",
    )
}
