use super::process_info::{
    group_exists, group_members, group_resident_bytes, resident_bytes, start_time,
};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

/// A shell that leads its own group and keeps two sleeping descendants in it.
fn spawn_group() -> Child {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "sleep 60 & sleep 60 & wait"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn().unwrap()
}

struct Group(Child);
impl Drop for Group {
    fn drop(&mut self) {
        unsafe { libc::killpg(self.0.id() as libc::pid_t, libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

#[test]
fn group_rss_counts_descendants() {
    let group = Group(spawn_group());
    let pgid = group.0.id() as libc::pid_t;
    crate::testing::wait_until("shell started both sleepers", || {
        group_members(pgid).len() >= 3
    });
    let members = group_members(pgid);
    assert!(members.contains(&pgid));
    let leader = resident_bytes(pgid).unwrap();
    let total = group_resident_bytes(pgid);
    assert!(leader > 0);
    assert!(
        total > leader,
        "group {total} must include descendants beyond the leader {leader}"
    );
    assert!(group_exists(pgid));
}

#[test]
fn start_time_is_stable_per_process_and_none_when_gone() {
    let group = Group(spawn_group());
    let pid = group.0.id() as libc::pid_t;
    let first = start_time(pid).unwrap();
    assert_eq!(start_time(pid), Some(first));
    // Distinct processes may share a start time (Linux counts clock ticks, so a child
    // started in its parent's tick matches it); a process is identified by the pair of its
    // PID and start time, never by the start time alone.
    let this = std::process::id() as libc::pid_t;
    let own = start_time(this).unwrap();
    assert_eq!(start_time(this), Some(own));
    drop(group);
    assert_eq!(start_time(pid), None);
    crate::testing::wait_until("group emptied", || !group_exists(pid));
}
