//! The checkout worker's runtime state and startup reconciliation (Decision 0009).
//!
//! One worker owns every write for a checkout. Startup reconciliation (formerly
//! "H") is its first work item: until it commits a validated head, FIFO claims
//! wait and readers see the last validated head with `catchingUp`.

use super::DaemonState;
use crate::indexer::IndexOptions;
use crate::store::topology::WriterSession;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) const STARTUP_BACKOFF_FIRST: Duration = Duration::from_millis(250);
const STARTUP_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Decision 0009 §2. The worker and readers share this one value; it replaces
/// the separate serving-session slot and the registry's phase flags.
pub(crate) enum RuntimeState {
    /// Not yet asked to reconcile. A daemon runtime begins at once; a
    /// standalone fixture scheduler reconciles when work arrives.
    Activating,
    /// Startup reconciliation is due or running (`waiting: false`), or failed
    /// and is waiting out its backoff (`waiting: true`). `attempts` counts
    /// attempts that began capturing the workspace.
    Reconciling { attempts: u32, waiting: bool },
    /// A validated head committed; this session is the only writer.
    Ready(Arc<WriterSession>),
    /// The captured root is missing or replaced, and its accepted requests are
    /// confirmed terminal. Old-root evidence is never served.
    RootLost,
    /// Released. No further work, reads or claims on this runtime.
    Retired,
}

impl RuntimeState {
    pub(crate) fn session(&self) -> Option<&Arc<WriterSession>> {
        match self {
            RuntimeState::Ready(session) => Some(session),
            _ => None,
        }
    }
}

/// Status a reader may show; never an admission decision (Decision 0009 §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimePhase {
    Activating,
    Reconciling { started: bool, waiting: bool },
    Ready,
    RootLost,
    Retired,
}

impl RuntimePhase {
    pub(crate) fn of(state: &RuntimeState) -> Self {
        match state {
            RuntimeState::Activating => RuntimePhase::Activating,
            RuntimeState::Reconciling { attempts, waiting } => RuntimePhase::Reconciling {
                started: *attempts > 0,
                waiting: *waiting,
            },
            RuntimeState::Ready(_) => RuntimePhase::Ready,
            RuntimeState::RootLost => RuntimePhase::RootLost,
            RuntimeState::Retired => RuntimePhase::Retired,
        }
    }
}

impl DaemonState {
    pub(crate) fn runtime_phase(&self) -> RuntimePhase {
        RuntimePhase::of(&self.runtime.lock().unwrap())
    }

    /// Run `f` under the runtime state lock. Registry admission (read leases,
    /// enqueue, release) linearizes with the worker's transitions here.
    pub(crate) fn with_runtime_state<T>(&self, f: impl FnOnce(&mut RuntimeState) -> T) -> T {
        f(&mut self.runtime.lock().unwrap())
    }

    /// Begin mandatory startup reconciliation for a daemon checkout runtime.
    /// `explicit` registered options override the recorded index inputs.
    pub(crate) fn begin_startup_reconciliation(self: &Arc<Self>, explicit: bool) {
        self.explicit_options.store(explicit, Ordering::Release);
        {
            let mut state = self.runtime.lock().unwrap();
            if matches!(*state, RuntimeState::Activating) {
                *state = RuntimeState::Reconciling {
                    attempts: 0,
                    waiting: false,
                };
            }
        }
        self.start_queue_tick();
    }

    /// Make startup reconciliation due again, as a lost session would.
    #[cfg(test)]
    pub(crate) fn mark_startup_due_for_tests(&self) {
        *self.runtime.lock().unwrap() = RuntimeState::Reconciling {
            attempts: 1,
            waiting: false,
        };
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn startup_captures_for_tests(&self) -> u64 {
        self.startup_captures.load(Ordering::Acquire)
    }

    pub fn startup_error(&self) -> Option<String> {
        self.startup_error.lock().unwrap().clone()
    }

    /// Deterministically pause startup reconciliation after old-root settlement
    /// and before it captures the workspace.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_startup_hook_for_tests(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.startup_hook.lock().unwrap() = Some(hook);
    }

    /// Whether the queue tick should run startup reconciliation now. A
    /// standalone scheduler that was never asked to reconcile does so only
    /// when `work_pending` reports accepted work (`None`: undecided, retry).
    pub(super) fn startup_due(
        &self,
        work_pending: impl FnOnce() -> anyhow::Result<Option<bool>>,
    ) -> anyhow::Result<bool> {
        match &*self.runtime.lock().unwrap() {
            RuntimeState::Reconciling { .. } => return Ok(true),
            RuntimeState::Activating => {}
            RuntimeState::Ready(_) | RuntimeState::RootLost | RuntimeState::Retired => {
                return Ok(false);
            }
        }
        if self.store.is_root_replaced() || self.store.is_recreate_pending() {
            return Ok(true);
        }
        Ok(work_pending()?.unwrap_or(false))
    }

    /// The options startup reconciliation captures with. Explicit registered
    /// options win; otherwise recorded inputs, then the FIFO head's, then the
    /// daemon defaults. A recreate without trusted inputs needs the FIFO
    /// head's options (or, for a replaced root, the defaults).
    fn startup_options(&self) -> anyhow::Result<Option<IndexOptions>> {
        let explicit = self.explicit_options.load(Ordering::Acquire);
        // The FIFO head is read first, even when unused: a typed failure to
        // read it defers this attempt rather than reconciling around it.
        let head = self
            .startup_head()?
            .map(|head| head.options(std::path::Path::new(self.store.workspace_root())))
            .transpose()?;
        if self.store.is_recreate_pending() {
            if explicit {
                return Ok(Some(self.options.clone()));
            }
            return Ok(head.or_else(|| self.store.is_root_replaced().then(|| self.options.clone())));
        }
        if explicit {
            return Ok(Some(self.options.clone()));
        }
        Ok(Some(match self.store.recorded_index_options()? {
            Some(recorded) => recorded,
            None => head.unwrap_or_else(|| self.options.clone()),
        }))
    }

    /// The FIFO head as startup reconciliation sees it.
    pub(super) fn startup_head(&self) -> anyhow::Result<Option<crate::store::requests::Request>> {
        #[cfg(test)]
        if self
            .test_activation_head_busy_once
            .swap(false, Ordering::AcqRel)
        {
            return Err(crate::store::topology::StorageBusy.into());
        }
        self.store.earliest_unfinished_request()
    }

    fn startup_attempt(&self) -> anyhow::Result<Arc<WriterSession>> {
        self.store.settle_foreign_root_requests()?;
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.startup_hook.lock().unwrap().take() {
            hook();
        }
        if let RuntimeState::Reconciling { attempts, .. } = &mut *self.runtime.lock().unwrap() {
            *attempts += 1;
        }
        #[cfg(feature = "test-support")]
        self.startup_captures.fetch_add(1, Ordering::AcqRel);
        let options = self.startup_options()?;
        crate::index_coordinator::establish_serving_session(
            &self.store,
            options.as_ref(),
            &Arc::new(AtomicBool::new(false)),
        )
    }

    /// One startup reconciliation attempt; the worker's highest-priority item.
    /// On failure the state waits out a doubling backoff, and only a typed
    /// contention is not reported as a tick failure.
    pub(super) fn run_startup_reconciliation(&self) -> anyhow::Result<()> {
        {
            let mut state = self.runtime.lock().unwrap();
            let attempts = match &*state {
                RuntimeState::Reconciling { attempts, .. } => *attempts,
                RuntimeState::Activating => 0,
                _ => return Ok(()),
            };
            *state = RuntimeState::Reconciling {
                attempts,
                waiting: false,
            };
        }
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.startup_attempt()))
                .unwrap_or_else(|panic| {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_owned())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_owned());
                    Err(anyhow::anyhow!(
                        "startup reconciliation panicked: {message}"
                    ))
                });
        match outcome {
            Ok(session) => {
                self.replace_serving_session(Some(session));
                *self.startup_error.lock().unwrap() = None;
                *self.startup_backoff.lock().unwrap() = STARTUP_BACKOFF_FIRST;
                *self.recovery_retry_after.lock().unwrap() = None;
                Ok(())
            }
            Err(error) => {
                {
                    let mut first = self.startup_error.lock().unwrap();
                    if first.is_none() {
                        *first = Some(format!("{error:#}"));
                    }
                }
                let delay = {
                    let mut backoff = self.startup_backoff.lock().unwrap();
                    let delay = *backoff;
                    *backoff = backoff.saturating_mul(2).min(STARTUP_BACKOFF_MAX);
                    delay
                };
                *self.recovery_retry_after.lock().unwrap() = Some(Instant::now() + delay);
                if let RuntimeState::Reconciling { waiting, .. } =
                    &mut *self.runtime.lock().unwrap()
                {
                    *waiting = true;
                }
                if crate::store::transient_storage_contention(&error) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }
}
