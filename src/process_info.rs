//! Process-group inspection for producer bounds and crash recovery: group
//! membership, resident set and process start time on macOS and Linux.

use libc::pid_t;

/// Whether any process, zombies included, is left in group `pgid`.
pub fn group_exists(pgid: pid_t) -> bool {
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The process's start time in an opaque, stable unit: microseconds on macOS,
/// clock ticks since boot on Linux. For one PID, two reads agree only for the
/// same process, so it tells a recorded process from a reused PID. Different
/// processes may share a value; identity is the PID and start time together.
pub fn start_time(pid: pid_t) -> Option<u64> {
    imp::start_time(pid)
}

/// Every process currently in group `pgid`.
pub fn group_members(pgid: pid_t) -> Vec<pid_t> {
    imp::group_members(pgid)
}

/// Resident-set bytes of one process.
pub fn resident_bytes(pid: pid_t) -> Option<u64> {
    imp::resident_bytes(pid)
}

/// Sum of the resident sets of all processes in group `pgid`. A process that
/// exits between enumeration and sampling counts as zero.
pub fn group_resident_bytes(pgid: pid_t) -> u64 {
    group_members(pgid)
        .into_iter()
        .filter_map(resident_bytes)
        .sum()
}

#[cfg(target_os = "macos")]
mod imp {
    use libc::{c_int, c_void, pid_t};

    // PROC_PGRP_ONLY from <sys/proc_info.h>; the libc crate does not export it.
    const PROC_PGRP_ONLY: u32 = 2;

    pub fn start_time(pid: pid_t) -> Option<u64> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as c_int;
        let read = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        (read == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    pub fn group_members(pgid: pid_t) -> Vec<pid_t> {
        let mut capacity = 256_usize;
        loop {
            let mut pids = vec![0 as pid_t; capacity];
            let bytes = (pids.len() * std::mem::size_of::<pid_t>()) as c_int;
            let written = unsafe {
                libc::proc_listpids(
                    PROC_PGRP_ONLY,
                    pgid as u32,
                    pids.as_mut_ptr().cast::<c_void>(),
                    bytes,
                )
            };
            if written <= 0 {
                return Vec::new();
            }
            let count = written as usize / std::mem::size_of::<pid_t>();
            // A full buffer may have truncated the list.
            if count < capacity {
                pids.truncate(count);
                pids.retain(|pid| *pid > 0);
                return pids;
            }
            capacity *= 4;
        }
    }

    pub fn resident_bytes(pid: pid_t) -> Option<u64> {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as c_int;
        let read = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size)
        };
        (read == size).then_some(info.pti_resident_size)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use libc::pid_t;

    /// Fields of `/proc/<pid>/stat` after the parenthesized command name,
    /// which may itself contain ") ". Index 0 is the state (field 3).
    fn stat_fields(pid: pid_t) -> Option<Vec<String>> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, rest) = stat.rsplit_once(") ")?;
        Some(rest.split(' ').map(str::to_owned).collect())
    }

    pub fn start_time(pid: pid_t) -> Option<u64> {
        // Field 22, clock ticks since boot.
        stat_fields(pid)?.get(19)?.parse().ok()
    }

    pub fn group_members(pgid: pid_t) -> Vec<pid_t> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<pid_t>().ok())
            .filter(|pid| {
                // Field 5 is the process group.
                stat_fields(*pid)
                    .and_then(|fields| fields.get(2)?.parse::<pid_t>().ok())
                    .is_some_and(|group| group == pgid)
            })
            .collect()
    }

    pub fn resident_bytes(pid: pid_t) -> Option<u64> {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let pages: u64 = statm.split(' ').nth(1)?.trim().parse().ok()?;
        let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
        Some(pages * page)
    }
}
