use std::{
    io::ErrorKind,
    net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs},
    sync::mpsc,
    time::{Duration, Instant},
};

use ops_actions::ActionContext;
use ops_spec::{Error, ErrorCode, Value};
use serde_json::json;

pub(crate) fn probe(
    host: &str,
    port: u16,
    timeout_ms: u64,
    context: &ActionContext,
) -> Result<Value, Error> {
    let start = Instant::now();
    let window = Duration::from_millis(timeout_ms).min(context.remaining()?);
    let Some(deadline) = start.checked_add(window) else {
        return Err(Error::new(
            ErrorCode::EBudget,
            "TCP probe deadline is out of range",
        ));
    };
    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let (send, receive) = mpsc::sync_channel(1);
        let host = host.to_owned();
        std::thread::Builder::new()
            .spawn(move || {
                let addresses = (host.as_str(), port)
                    .to_socket_addrs()
                    .map(|addresses| addresses.collect::<Vec<_>>());
                let _ = send.send(addresses);
            })
            .map_err(|error| crate::io_error("resolve the bound host", &error))?;
        match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(addresses)) => addresses,
            Ok(Err(error)) => return Ok(observation(start, Some(&error.kind().to_string()))),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Ok(observation(start, Some("timed out")))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::new(
                    ErrorCode::EIo,
                    "Host resolution stopped unexpectedly",
                ))
            }
        }
    };

    let mut last_error = "No address found".to_owned();
    for address in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(observation(start, Some("timed out")));
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(_) => return Ok(observation(start, None)),
            Err(error) => {
                last_error = match error.kind() {
                    ErrorKind::TimedOut => "timed out".to_owned(),
                    kind => kind.to_string(),
                };
            }
        }
    }
    Ok(observation(start, Some(&last_error)))
}

fn observation(start: Instant, error: Option<&str>) -> Value {
    json!({ "reachable": error.is_none(), "latency_ms": start.elapsed().as_secs_f64() * 1000.0, "error": error })
}
