//! Checkout lifecycle: pending work, release, client disconnect, the lifecycle clock and
//! daemon shutdown.
use super::{
    BROWSER_IDLE_DELAY, CHECKOUT_RELEASE_DELAY, CheckoutRegistry, DAEMON_IDLE_DELAY, LifecycleTick,
    SelectionError, startup_running,
};
use crate::http;
use crate::store::topology::WorkspaceIdentity;
use std::sync::Arc;
use std::time::Instant;

impl CheckoutRegistry {
    /// Cancel producer owners without holding the registry they may need.
    pub async fn cancel_producers(registry: Arc<tokio::sync::Mutex<Self>>) -> anyhow::Result<()> {
        let runtimes: Vec<_> = registry.lock().await.runtimes.values().cloned().collect();
        tokio::task::spawn_blocking(move || {
            for runtime in runtimes {
                runtime.auto_run.shutdown();
                runtime.producer.shutdown();
                runtime.pickup.shutdown();
            }
        })
        .await?;
        Ok(())
    }

    /// Explicit shutdown bypasses idle grace, not accepted work or retirement.
    /// Call only after closing and draining client/browser ingress.
    pub async fn shutdown(registry: Arc<tokio::sync::Mutex<Self>>) -> anyhow::Result<()> {
        let signal = {
            let mut checked = registry.lock().await;
            for entry in checked.entries.values_mut() {
                entry.sessions.clear();
                entry.browser_until = None;
            }
            checked.lifecycle_signal()
        };
        loop {
            let notified = signal.notified();
            {
                let mut checked = registry.lock().await;
                for key in checked.entries.keys().cloned().collect::<Vec<_>>() {
                    checked
                        .release(&key)
                        .map_err(|error| anyhow::anyhow!("{}", error.reason()))?;
                }
                if checked.runtimes.is_empty() {
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    /// Mark a known checkout busy even without an attached client. The runtime
    /// supplies the actual queue/in-flight state and clears it after work drains.
    pub fn set_pending_work(&mut self, key: &str, pending: bool) -> Result<(), SelectionError> {
        let entry = self
            .entries
            .get_mut(key)
            .ok_or(SelectionError::Unavailable)?;
        entry.external_work = pending;
        entry.pending_work = pending;
        Ok(())
    }

    /// Probe only already-active checkouts. A failed queue probe remains busy;
    /// never activate an inert registered checkout to refresh pending work.
    pub fn refresh_pending_work(&mut self, key: &str) -> Result<bool, SelectionError> {
        let external = self
            .entries
            .get(key)
            .ok_or(SelectionError::Unavailable)?
            .external_work;
        let pending = external
            || match self.runtimes.get(key) {
                Some(runtime) => {
                    let resources = runtime
                        .resources()
                        .map_err(|_| SelectionError::Unavailable)?;
                    // Published state only; no SQLite under the registry lock.
                    // Watcher intent never holds a checkout (Decision 0009 §5).
                    startup_running(resources.scheduler.runtime_phase())
                        || resources.scheduler.leases() != 0
                        || resources.scheduler.fifo_pending()
                }
                None => {
                    return Ok(self
                        .entries
                        .get(key)
                        .ok_or(SelectionError::Unavailable)?
                        .pending_work);
                }
            };
        self.entries
            .get_mut(key)
            .ok_or(SelectionError::Unavailable)?
            .pending_work = pending;
        Ok(pending)
    }

    /// Release is a separate lifecycle decision, never implied by disconnect.
    /// The lifecycle owner calls this only after the idle delay.
    /// Returns true once the checkout is released. A live runtime is asked to
    /// retire; its worker retires it at a boundary (Decision 0009 §5) and
    /// wakes the lifecycle, which calls this again to finish.
    pub fn release(&mut self, key: &str) -> Result<bool, SelectionError> {
        self.refresh_pending_work(key)?;
        let entry = self.entries.get(key).ok_or(SelectionError::Unavailable)?;
        if !entry.sessions.is_empty() || entry.browser_until.is_some() || entry.pending_work {
            return Ok(false);
        }
        if let Some(runtime) = self.runtimes.get(key) {
            let resources = runtime
                .resources()
                .map_err(|_| SelectionError::Unavailable)?;
            match resources.scheduler.request_retire(|_| true) {
                http::RetireOutcome::Retired => self.finish_retired(key),
                http::RetireOutcome::Declined | http::RetireOutcome::Requested => {
                    return Ok(false);
                }
            }
        }
        self.entries.get_mut(key).unwrap().released = true;
        Ok(true)
    }

    /// A live serve control connection keeps the daemon up without attaching
    /// a checkout or extending its watcher lifetime.
    pub fn retain_serve(&mut self, session: u64) {
        self.serve_sessions.insert(session);
        self.idle_exit_at = None;
        self.lifecycle.notify_one();
    }

    /// Called at client EOF/disconnect; explicit worktrees remain attached until then.
    pub fn disconnect(&mut self, session: u64) {
        self.disconnect_at(session, self.clock_override.unwrap_or_else(Instant::now));
    }

    pub fn disconnect_at(&mut self, session: u64, now: Instant) {
        let expired_at = self.expire_browsers(now);
        if let Some(expired_at) = expired_at {
            self.update_exit_deadline(expired_at);
        }
        let mut disconnected = self.serve_sessions.remove(&session);
        for entry in self.entries.values_mut() {
            if entry.sessions.remove(&session) {
                disconnected = true;
                if entry.sessions.is_empty() && entry.browser_until.is_none() {
                    entry.release_at = Some(now + CHECKOUT_RELEASE_DELAY);
                }
            }
        }
        if !disconnected {
            return;
        }
        self.lifecycle.notify_one();
        if self.serve_sessions.is_empty()
            && self
                .entries
                .values()
                .all(|entry| entry.sessions.is_empty() && entry.browser_until.is_none())
        {
            self.idle_exit_at = Some(now + DAEMON_IDLE_DELAY);
        } else {
            self.update_exit_deadline(now);
        }
    }

    /// Browser activity is a virtual client, not an MCP/CLI session. A later
    /// selected request renews its expiry without shortening the release grace.
    pub fn browser_request_at(
        &mut self,
        identity: &WorkspaceIdentity,
        now: Instant,
    ) -> Result<Arc<WorkspaceIdentity>, SelectionError> {
        let attached = self.attach(None, identity)?;
        let entry = self.entries.get_mut(&identity.root_key).unwrap();
        entry.browser_until = Some(now + BROWSER_IDLE_DELAY);
        entry.release_at = None;
        self.idle_exit_at = None;
        self.lifecycle.notify_one();
        Ok(attached)
    }

    /// Deterministic daemon-driver ticks for integration fixtures. Browser
    /// request timestamps still come from production HTTP's real clock.
    #[doc(hidden)]
    pub fn set_clock_override_for_tests(&mut self, now: Instant) {
        self.clock_override = Some(now);
        // Virtual time moved: the lifecycle owner re-evaluates its deadlines.
        self.lifecycle.notify_one();
    }

    /// Advance deterministic lifecycle clocks. Busy resources keep their original
    /// deadlines: once work drains, no extra grace interval is added.
    pub fn advance(&mut self, now: Instant) -> Result<LifecycleTick, SelectionError> {
        let now = self.clock_override.unwrap_or(now);
        // Expired clients disconnect at their deadline, not at this tick.
        let expired_at = self.expire_browsers(now);
        self.update_exit_deadline(expired_at.unwrap_or(now));
        let due: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.release_at.is_some_and(|at| now >= at))
            .map(|(key, _)| key.clone())
            .collect();
        let mut released = Vec::new();
        for key in due {
            if self.release(&key)? {
                self.entries.get_mut(&key).unwrap().release_at = None;
                released.push(key);
            }
        }
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.test_after_due_release.take() {
            hook();
        }
        let mut busy = false;
        for key in self.entries.keys().cloned().collect::<Vec<_>>() {
            busy |= self.refresh_pending_work(&key)?;
        }
        self.forget_dead_roots();
        let exit = self.idle_exit_at.is_some_and(|at| now >= at)
            && !busy
            // Release may defer on startup reconciliation, a watcher, or an active read just before
            // the later pending-work refresh observes that work finish.
            && self.runtimes.is_empty()
            && self
                .entries
                .values()
                .all(|entry| entry.sessions.is_empty() && entry.browser_until.is_none());
        Ok(LifecycleTick { released, exit })
    }

    /// Forget each entry whose root no longer exists once nothing uses it (#1316):
    /// released, with no runtime, sessions, browser grace, pending or external
    /// work, or in-flight read of the lost root. These are the conditions that
    /// already let a replacement retire an entry. A root that returns attaches
    /// as a new entry; its index, if any, is GC's to collect.
    fn forget_dead_roots(&mut self) {
        let dead: Vec<String> =
            self.entries
                .iter()
                .filter(|(key, entry)| {
                    entry.released
                        && !self.runtimes.contains_key(*key)
                        && entry.sessions.is_empty()
                        && entry.browser_until.is_none()
                        && !entry.pending_work
                        && !entry.external_work
                        && entry.lost_reads.as_ref().is_none_or(|reads| {
                            reads.load(std::sync::atomic::Ordering::Acquire) == 0
                        })
                        && super::root_missing(&entry.identity.root)
                })
                .map(|(key, _)| key.clone())
                .collect();
        for key in dead {
            self.entries.remove(&key);
        }
    }

    fn expire_browsers(&mut self, now: Instant) -> Option<Instant> {
        let mut last = None;
        for entry in self.entries.values_mut() {
            if entry.browser_until.is_some_and(|until| now >= until) {
                let until = entry.browser_until.take().unwrap();
                if entry.sessions.is_empty() {
                    entry.release_at = Some(until + CHECKOUT_RELEASE_DELAY);
                    last = Some(last.map_or(until, |previous: Instant| previous.max(until)));
                }
            }
        }
        last
    }

    pub fn idle_exit_deadline(&self) -> Option<Instant> {
        self.idle_exit_at
    }

    /// The next lifecycle deadline still ahead of `now`. A deadline already
    /// passed is pending on work, a lease or a retirement, and each of those
    /// signals the lifecycle when it changes.
    pub fn next_lifecycle_deadline(&self, now: Instant) -> Option<Instant> {
        // Under a test clock, virtual deadlines are not real-time sleeps: the
        // test moves the clock, which signals the lifecycle.
        if self.clock_override.is_some() {
            return None;
        }
        self.entries
            .values()
            .flat_map(|entry| [entry.release_at, entry.browser_until])
            .chain([self.idle_exit_at])
            .flatten()
            .filter(|at| *at > now)
            .min()
    }

    fn update_exit_deadline(&mut self, now: Instant) {
        if !self.serve_sessions.is_empty()
            || self
                .entries
                .values()
                .any(|entry| !entry.sessions.is_empty() || entry.browser_until.is_some())
        {
            self.idle_exit_at = None;
        } else if self.idle_exit_at.is_none() {
            self.idle_exit_at = Some(now + DAEMON_IDLE_DELAY);
        }
    }
}
