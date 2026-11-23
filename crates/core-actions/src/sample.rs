mod counters;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::{
    collections::BTreeMap,
    mem::size_of,
    thread,
    time::{Duration, Instant},
};

use ops_actions::ActionContext;
use ops_spec::{Error, ErrorCode, Value};
use serde_json::json;

use self::counters::{process_cpu_pct, ProcessCpu, Rankings};
#[cfg(target_os = "linux")]
use self::linux as platform;
#[cfg(target_os = "macos")]
use self::macos as platform;
use crate::{data_limit, io_error};

const INTERVAL: Duration = Duration::from_millis(250);

pub(crate) fn sample(limit: usize, context: &ActionContext) -> Result<Value, Error> {
    check_window(INTERVAL, context)?;
    let clock_hz = platform::process_clock_hz();
    context.remaining()?;
    let clock_hz = clock_hz.map_err(|error| io_error("read machine CPU clock", &error))?;
    let cpu_before = platform::machine_cpu();
    let started = Instant::now();
    context.remaining()?;
    let cpu_before = cpu_before.map_err(|error| io_error("sample machine CPU", &error))?;
    let mut before = BTreeMap::new();
    let max_processes = context.max_bytes / size_of::<(u32, ProcessCpu)>();
    platform::visit_processes(context, |process| {
        if before.len() == max_processes {
            return Err(data_limit());
        }
        before.insert(process.pid, process.cpu);
        Ok(())
    })?;

    let wait = INTERVAL.saturating_sub(started.elapsed());
    check_window(wait, context)?;
    if !wait.is_zero() {
        thread::sleep(wait);
    }
    context.remaining()?;
    let cpu_after = platform::machine_cpu();
    let elapsed = started.elapsed();
    context.remaining()?;
    let cpu_after = cpu_after.map_err(|error| io_error("sample machine CPU", &error))?;
    let memory = platform::machine_memory();
    context.remaining()?;
    let (total_bytes, available_bytes) =
        memory.map_err(|error| io_error("sample machine memory", &error))?;

    let mut rankings = Rankings::new(limit);
    platform::visit_processes(context, |process| {
        rankings.observe(before.get(&process.pid).copied(), &process);
        Ok(())
    })?;
    let cpu = rankings
        .cpu
        .into_iter()
        .map(|row| {
            json!({
                "pid": row.pid,
                "process_key": row.process_key(),
                "name": row.name,
                "cpu_pct": process_cpu_pct(row.amount, clock_hz, elapsed),
            })
        })
        .collect::<Vec<_>>();
    let memory = rankings
        .memory
        .into_iter()
        .map(|row| json!({ "pid": row.pid, "process_key": row.process_key(), "name": row.name, "bytes": row.amount }))
        .collect::<Vec<_>>();
    let value = json!({
        "sample_ms": elapsed.as_secs_f64() * 1000.0,
        "machine": {
            "cpu_pct": platform::cpu_usage(cpu_before, cpu_after),
            "memory_total_bytes": total_bytes,
            "memory_available_bytes": available_bytes,
        },
        "cpu": cpu,
        "memory": memory,
        "memory_basis": platform::MEMORY_BASIS,
    });
    context.remaining()?;
    if value.to_string().len() > context.max_bytes {
        return Err(data_limit());
    }
    Ok(value)
}

fn check_window(duration: Duration, context: &ActionContext) -> Result<(), Error> {
    if duration >= context.remaining()? {
        return Err(Error::new(
            ErrorCode::EBudget,
            "Execution deadline leaves too little time for the sys.sample interval",
        ));
    }
    Ok(())
}
