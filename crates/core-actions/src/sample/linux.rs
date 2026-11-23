use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader},
};

use ops_actions::ActionContext;
use ops_spec::Error;

use super::counters::{machine_cpu_pct, ProcessCpu, ProcessSample};

pub(super) const MEMORY_BASIS: &str = "rss";
type CpuTicks = [u64; 8];

pub(super) fn machine_cpu() -> io::Result<CpuTicks> {
    let mut input = BufReader::new(File::open("/proc/stat")?);
    let mut line = String::new();
    input.read_line(&mut line)?;
    parse_machine_cpu(&line).ok_or_else(invalid_counters)
}

fn parse_machine_cpu(line: &str) -> Option<CpuTicks> {
    let mut fields = line.split_ascii_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    // guest and guest_nice are already included in user and nice.
    let mut ticks = [0; 8];
    for tick in &mut ticks {
        *tick = fields.next()?.parse().ok()?;
    }
    Some(ticks)
}

pub(super) fn cpu_usage(before: CpuTicks, after: CpuTicks) -> f64 {
    let [user, nice, system, idle, iowait, irq, softirq, steal] =
        std::array::from_fn(|index| after[index].saturating_sub(before[index]));
    let busy = [user, nice, system, irq, softirq, steal]
        .into_iter()
        .fold(0, u64::saturating_add);
    machine_cpu_pct(busy, idle.saturating_add(iowait))
}

pub(super) fn machine_memory() -> io::Result<(u64, u64)> {
    let contents = fs::read_to_string("/proc/meminfo")?;
    parse_machine_memory(&contents).ok_or_else(invalid_counters)
}

fn parse_machine_memory(contents: &str) -> Option<(u64, u64)> {
    let mut total = None;
    let mut available = None;
    for line in contents.lines() {
        let mut fields = line.split_ascii_whitespace();
        let destination = match fields.next() {
            Some("MemTotal:") => &mut total,
            Some("MemAvailable:") => &mut available,
            _ => continue,
        };
        let kibibytes: u64 = fields.next()?.parse().ok()?;
        if fields.next()? != "kB" {
            return None;
        }
        *destination = Some(kibibytes.checked_mul(1024)?);
    }
    Some((total?, available?))
}

fn invalid_counters() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Invalid machine counters in procfs",
    )
}

pub(super) fn process_clock_hz() -> io::Result<f64> {
    let clock_hz = rustix::param::clock_ticks_per_second();
    if clock_hz == 0 {
        return Err(invalid_counters());
    }
    Ok(clock_hz as f64)
}

pub(super) fn visit_processes(
    context: &ActionContext,
    mut observe: impl FnMut(ProcessSample) -> Result<(), Error>,
) -> Result<(), Error> {
    context.remaining()?;
    let directory = fs::read_dir("/proc");
    context.remaining()?;
    if let Ok(directory) = directory {
        let page_size = rustix::param::page_size() as u64;
        for entry in directory {
            context.remaining()?;
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
                continue;
            };
            let stat = fs::read(entry.path().join("stat"));
            context.remaining()?;
            if let Some(process) = stat
                .ok()
                .and_then(|stat| parse_process(pid, &stat, page_size))
            {
                observe(process)?;
            }
        }
    }
    context.remaining()?;
    Ok(())
}

fn parse_process(pid: u32, stat: &[u8], page_size: u64) -> Option<ProcessSample> {
    let name_start = stat.iter().position(|&byte| byte == b'(')? + 1;
    let name_end = stat.iter().rposition(|&byte| byte == b')')?;
    let counters = std::str::from_utf8(stat.get(name_end + 1..)?).ok()?;
    let mut fields = counters.split_ascii_whitespace();
    if matches!(fields.next()?, "Z" | "X" | "x") {
        return None;
    }
    // Fields following the state start at 4. The CPU times are 14 and 15.
    let user_ticks: u64 = fields.nth(10)?.parse().ok()?;
    let system_ticks = fields.next()?.parse().ok()?;
    let start_time = fields.nth(6)?.parse().ok()?; // Field 22.
    let rss: u64 = fields.nth(1)?.parse().ok()?; // Field 24.
    Some(ProcessSample {
        pid,
        name: String::from_utf8_lossy(stat.get(name_start..name_end)?).into_owned(),
        cpu: ProcessCpu {
            start_time,
            ticks: user_ticks.checked_add(system_ticks)?,
        },
        memory_bytes: rss.checked_mul(page_size)?,
    })
}
