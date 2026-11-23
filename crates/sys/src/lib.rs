#![cfg(target_os = "macos")]
#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    io,
    mem::{offset_of, size_of, size_of_val, MaybeUninit},
    ptr,
};

extern "C" {
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

struct HostPort(libc::mach_port_t);

impl HostPort {
    #[allow(deprecated)]
    fn new() -> Self {
        // SAFETY: mach_host_self takes no arguments and returns a send right held by this task.
        Self(unsafe { libc::mach_host_self() })
    }
}

impl Drop for HostPort {
    #[allow(deprecated)]
    fn drop(&mut self) {
        // SAFETY: release the send right obtained by this HostPort in the current task.
        unsafe { mach_port_deallocate(libc::mach_task_self(), self.0) };
    }
}

pub fn machine_cpu() -> io::Result<[u32; libc::CPU_STATE_MAX as usize]> {
    let mut info = MaybeUninit::<libc::host_cpu_load_info>::uninit();
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    let host = HostPort::new();
    // SAFETY: the output buffer has room for count Mach integers for this flavor.
    let status = unsafe {
        libc::host_statistics64(
            host.0,
            libc::HOST_CPU_LOAD_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if status != libc::KERN_SUCCESS || count != libc::HOST_CPU_LOAD_INFO_COUNT {
        return Err(io::Error::other("Unable to read Mach CPU counters"));
    }
    // SAFETY: success and the full word count initialize every CPU counter.
    Ok(unsafe { info.assume_init() }.cpu_ticks)
}

#[allow(deprecated)]
pub fn process_clock_hz() -> io::Result<f64> {
    let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: timebase is a writable Mach timebase info record.
    let status = unsafe { libc::mach_timebase_info(&mut timebase) };
    if status != libc::KERN_SUCCESS || timebase.numer == 0 || timebase.denom == 0 {
        return Err(io::Error::other("Unable to read the Mach timebase"));
    }
    // The timebase gives nanoseconds per tick as numer / denom.
    Ok(1_000_000_000.0 * f64::from(timebase.denom) / f64::from(timebase.numer))
}

pub struct MemoryCounters {
    pub total_bytes: u64,
    pub page_size: u64,
    pub free: u32,
    pub inactive: u32,
    pub purgeable: u32,
}

pub fn machine_memory() -> io::Result<MemoryCounters> {
    let mut info = MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let host = HostPort::new();
    // SAFETY: the output buffer has room for count Mach integers, including optional VM fields.
    let status = unsafe {
        libc::host_statistics64(
            host.0,
            libc::HOST_VM_INFO64,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    let required_words = (offset_of!(libc::vm_statistics64, purgeable_count)
        + size_of::<libc::natural_t>())
        / size_of::<libc::integer_t>();
    if status != libc::KERN_SUCCESS || (count as usize) < required_words {
        return Err(io::Error::other("Unable to read Mach VM counters"));
    }
    // SAFETY: the returned prefix covers the required fields. The remaining integer fields are zeroed.
    let info = unsafe { info.assume_init() };
    // SAFETY: sysconf takes a constant selector and writes to no caller memory.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = u64::try_from(page_size)
        .ok()
        .filter(|&size| size != 0)
        .ok_or_else(|| io::Error::other("Unable to read the system page size"))?;
    Ok(MemoryCounters {
        total_bytes: physical_memory()?,
        page_size,
        free: info.free_count,
        inactive: info.inactive_count,
        purgeable: info.purgeable_count,
    })
}

fn physical_memory() -> io::Result<u64> {
    let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
    let mut bytes = 0_u64;
    let mut size = size_of::<u64>();
    // SAFETY: the two selectors request a u64 and the output pointer refers to size writable bytes.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&mut bytes as *mut u64).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    if size != size_of::<u64>() {
        return Err(io::Error::other("Invalid physical memory counter size"));
    }
    Ok(bytes)
}

pub fn process_count() -> Option<usize> {
    // SAFETY: a null pointer and zero size ask libproc for the list capacity.
    usize::try_from(unsafe { libc::proc_listallpids(ptr::null_mut(), 0) }).ok()
}

pub fn list_pids(pids: &mut [i32]) -> Option<usize> {
    if pids.is_empty() {
        return Some(0);
    }
    let size = libc::c_int::try_from(size_of_val(pids)).ok()?;
    // SAFETY: pids points to size writable bytes with the alignment of pid_t.
    let count =
        usize::try_from(unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) }).ok()?;
    (count <= pids.len()).then_some(count)
}

pub struct ProcessCounters {
    pub name: String,
    pub start_time: u64,
    pub cpu_ticks: u64,
    pub footprint: u64,
}

pub fn process_counters(pid: libc::pid_t) -> Option<ProcessCounters> {
    let before = process_info(pid)?;
    let mut usage = MaybeUninit::<libc::rusage_info_v0>::uninit();
    // SAFETY: RUSAGE_INFO_V0 writes a rusage_info_v0 into this output buffer.
    let status =
        unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, usage.as_mut_ptr().cast()) };
    if status != 0 {
        return None;
    }
    // Verify the identity again in case the PID was recycled during the separate rusage read.
    let after = process_info(pid)?;
    if (before.pbi_start_tvsec, before.pbi_start_tvusec)
        != (after.pbi_start_tvsec, after.pbi_start_tvusec)
    {
        return None;
    }
    // SAFETY: the successful rusage call filled the V0 record.
    let usage = unsafe { usage.assume_init() };
    let name = after.pbi_name.map(|byte| byte as u8);
    let command = after.pbi_comm.map(|byte| byte as u8);
    let name: &[u8] = if name[0] == 0 { &command } else { &name };
    let name_end = name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(name.len());
    Some(ProcessCounters {
        name: String::from_utf8_lossy(&name[..name_end]).into_owned(),
        start_time: after
            .pbi_start_tvsec
            .checked_mul(1_000_000)?
            .checked_add(after.pbi_start_tvusec)?,
        cpu_ticks: usage.ri_user_time.checked_add(usage.ri_system_time)?,
        footprint: usage.ri_phys_footprint,
    })
}

fn process_info(pid: libc::pid_t) -> Option<libc::proc_bsdinfo> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer has exactly the size of the requested BSD info record.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    // SAFETY: proc_pidinfo reported writing a full record.
    let info = unsafe { info.assume_init() };
    (info.pbi_pid == pid as u32 && info.pbi_status != libc::SZOMB).then_some(info)
}
