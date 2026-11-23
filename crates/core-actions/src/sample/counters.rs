use std::{cmp::Reverse, time::Duration};

#[derive(Clone, Copy)]
pub(super) struct ProcessCpu {
    pub start_time: u64,
    pub ticks: u64,
}

pub(super) struct ProcessSample {
    pub pid: u32,
    pub name: String,
    pub cpu: ProcessCpu,
    pub memory_bytes: u64,
}

pub(super) fn process_cpu_pct(ticks: u64, clock_hz: f64, elapsed: Duration) -> f64 {
    100.0 * (ticks as f64 / clock_hz) / elapsed.as_secs_f64()
}

pub(super) fn machine_cpu_pct(busy: u64, idle: u64) -> f64 {
    let total = busy as f64 + idle as f64;
    if total == 0.0 {
        return 0.0;
    }
    100.0 * (busy as f64 / total)
}

pub(super) struct RankedProcess {
    pub pid: u32,
    pub name: String,
    pub amount: u64,
    pub start_time: u64,
}

impl RankedProcess {
    pub(super) fn process_key(&self) -> String {
        format!("{}:{}", self.pid, self.start_time)
    }
}

pub(super) struct Rankings {
    pub cpu: Vec<RankedProcess>,
    pub memory: Vec<RankedProcess>,
    limit: usize,
}

impl Rankings {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            cpu: Vec::with_capacity(limit),
            memory: Vec::with_capacity(limit),
            limit,
        }
    }

    pub(super) fn observe(&mut self, before: Option<ProcessCpu>, after: &ProcessSample) {
        if let Some(ticks) = before
            .filter(|before| before.start_time == after.cpu.start_time)
            .and_then(|before| after.cpu.ticks.checked_sub(before.ticks))
        {
            rank(&mut self.cpu, after, ticks, self.limit);
        }
        rank(&mut self.memory, after, after.memory_bytes, self.limit);
    }
}

fn rank(rows: &mut Vec<RankedProcess>, process: &ProcessSample, amount: u64, limit: usize) {
    let key = (Reverse(amount), process.pid);
    let index = rows.partition_point(|row| (Reverse(row.amount), row.pid) < key);
    if index < limit {
        if rows.len() == limit {
            rows.pop();
        }
        rows.insert(
            index,
            RankedProcess {
                pid: process.pid,
                name: process.name.clone(),
                amount,
                start_time: process.cpu.start_time,
            },
        );
    }
}
