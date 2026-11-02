use super::GroupStatus;
use super::exec::{after_reap, children_auto_reaped, group_status};

/// Every combination of the final SIGKILL's result, the leader's exit and the listing after
/// the kill wait (#624). Only a signal that reached the group plus a listing of the leader
/// alone proves the group empty; a listed member or a live leader proves survival; anything
/// else is unknown.
#[test]
fn group_status_is_proven_or_unknown() {
    use GroupStatus::{Empty, Survived, Unknown};
    let cases = [
        (Ok(()), true, Some(true), Empty),
        (Ok(()), true, Some(false), Survived),
        (Ok(()), true, None, Unknown),
        (Ok(()), false, Some(true), Survived),
        (Ok(()), false, None, Survived),
        // macOS refuses a group whose only member is the zombie leader.
        (Err(libc::EPERM), true, Some(true), Unknown),
        (Err(libc::EPERM), true, Some(false), Survived),
        (Err(libc::EPERM), true, None, Unknown),
        (Err(libc::ESRCH), true, Some(true), Unknown),
        (Err(libc::EPERM), false, Some(true), Survived),
    ];
    for (kill, exited, only, expected) in cases {
        assert_eq!(
            group_status(kill, exited, only),
            expected,
            "{kill:?} {exited} {only:?}"
        );
    }
}

/// After the reap only an unknown group can change, and only a probe that found no process
/// with the group ID (ESRCH) proves it empty; a probe that succeeded or was refused (EPERM),
/// as for a reused ID, leaves it unknown.
#[test]
fn only_an_esrch_probe_after_the_reap_settles_an_unknown_group() {
    use GroupStatus::{Empty, Survived, Unknown};
    assert_eq!(after_reap(Unknown, true), Empty);
    assert_eq!(after_reap(Unknown, false), Unknown);
    assert_eq!(after_reap(Survived, true), Survived);
    assert_eq!(after_reap(Survived, false), Survived);
    assert_eq!(after_reap(Empty, false), Empty);
}

#[test]
fn ignored_or_no_zombie_sigchld_is_auto_reaping() {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_DFL;
    assert!(!children_auto_reaped(&action));
    action.sa_sigaction = libc::SIG_IGN;
    assert!(children_auto_reaped(&action));
    action.sa_sigaction = libc::SIG_DFL;
    action.sa_flags = libc::SA_NOCLDWAIT;
    assert!(children_auto_reaped(&action));
}
