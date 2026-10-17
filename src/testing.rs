//! Test timing policy. Functional tests never assert speed: a wall-clock bound
//! in a test only detects a hang, and every test uses the same generous
//! backstop. Performance is measured and gated separately. A test that needs
//! progress waits for it; a test that needs ordering or non-blocking behavior
//! proves it with hooks, not elapsed time.

use std::time::{Duration, Instant};

/// The only wall-clock bound a functional test may use for progress. Well
/// above any healthy run on a loaded host, below nextest's 5-minute kill.
pub const HANG_BACKSTOP: Duration = Duration::from_secs(120);

/// Poll `ready` until it holds; panic with `what` only if it never does.
pub fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_BACKSTOP;
    while !ready() {
        assert!(Instant::now() < deadline, "{what} did not happen (hang)");
        std::thread::sleep(Duration::from_millis(10));
    }
}
