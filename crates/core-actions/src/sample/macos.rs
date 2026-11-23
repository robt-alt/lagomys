use std::{io, mem::size_of};

use ops_actions::ActionContext;
use ops_spec::Error;

use super::counters::{machine_cpu_pct, ProcessCpu, ProcessSample};
use crate::data_limit;

pub(super) const MEMORY_BASIS: &str = "footprint";
pub(super) use ops_sys::{machine_cpu, process_clock_hz};

pub(super) fn cpu_usage(before: [u32; 4], after: [u32; 4]) -> f64 {
    let [user, system, idle, nice] =
        std::array::from_fn(|index| u64::from(after[index].wrapping_sub(before[index])));
    machine_cpu_pct(user + system + nice, idle)
}

pub(super) fn machine_memory() -> io::Result<(u64, u64)> {
    let counters = ops_sys::machine_memory()?;
    let pages =
        u64::from(counters.free) + u64::from(counters.inactive) + u64::from(counters.purgeable);
    Ok((counters.total_bytes, pages * counters.page_size))
}

pub(super) fn visit_processes(
    context: &ActionContext,
    mut observe: impl FnMut(ProcessSample) -> Result<(), Error>,
) -> Result<(), Error> {
    context.remaining()?;
    let count = ops_sys::process_count();
    context.remaining()?;
    let Some(count) = count else { return Ok(()) };
    let capacity = context.max_bytes / size_of::<i32>();
    if capacity == 0 {
        return Err(data_limit());
    }
    let mut pids = vec![0; count.saturating_add(1).min(capacity)];
    loop {
        context.remaining()?;
        let count = ops_sys::list_pids(&mut pids);
        context.remaining()?;
        let Some(count) = count else { return Ok(()) };
        if count < pids.len() {
            pids.truncate(count);
            break;
        }
        if pids.len() == capacity {
            return Err(data_limit());
        }
        pids.resize(pids.len().saturating_mul(2).min(capacity), 0);
    }

    for raw_pid in pids {
        context.remaining()?;
        let Ok(pid) = u32::try_from(raw_pid) else {
            continue;
        };
        let process = ops_sys::process_counters(raw_pid);
        context.remaining()?;
        if let Some(process) = process {
            observe(ProcessSample {
                pid,
                name: process.name,
                cpu: ProcessCpu {
                    start_time: process.start_time,
                    ticks: process.cpu_ticks,
                },
                memory_bytes: process.footprint,
            })?;
        }
    }
    Ok(())
}
