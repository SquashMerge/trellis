//! The shared test-support "settled" wait (#720). See `crate::testing` for when a
//! test uses it and when it compares against the snapshot it rendered from instead.
use super::{ActiveResources, CheckoutRuntime};
use crate::http::RuntimePhase;
use crate::model::IndexPin;
use std::time::{Duration, Instant};

/// External SQLite contention (another process's index-use lock, for example)
/// publishes no in-process signal, so a contended sample is retried after this
/// bounded wait, as the CLI read path does. Every other wait is for the owner's
/// published state change.
const CONTENTION_RETRY: Duration = Duration::from_millis(20);

/// The component that kept a checkout from settling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleBusy {
    /// Startup reconciliation has not committed: the runtime phase, and the
    /// first error of the current run, if any.
    Startup {
        phase: String,
        error: Option<String>,
    },
    /// The watcher holds an unacknowledged hint, or its first inventory is due.
    Watcher,
    /// Accepted FIFO work has not reached a terminal row.
    Fifo,
    /// Semantic catch-up (S) still has a pending target.
    Semantic,
    /// The selected read was refused, for example `index_not_ready` or contention.
    Read(String),
    /// `catchingUp` was set while every component above was idle.
    CatchingUp,
    /// Every sample was idle, but the owner published a change while it was taken.
    Changing,
}

impl std::fmt::Display for SettleBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettleBusy::Startup { phase, error } => {
                write!(f, "startup reconciliation ({phase}")?;
                if let Some(error) = error {
                    write!(f, ", first error: {error}")?;
                }
                write!(f, ")")
            }
            SettleBusy::Watcher => write!(f, "watcher pending"),
            SettleBusy::Fifo => write!(f, "FIFO pending"),
            SettleBusy::Semantic => write!(f, "semantic catch-up pending target"),
            SettleBusy::Read(error) => write!(f, "selected read refused: {error}"),
            SettleBusy::CatchingUp => write!(f, "catchingUp with every component idle"),
            SettleBusy::Changing => write!(f, "state changed during every idle sample"),
        }
    }
}

/// Why a settled wait ended without settling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotSettled {
    /// `HANG_BACKSTOP` passed with this component still busy.
    Hang(SettleBusy),
    /// The checkout settled, but at a different published head than the one asked for.
    OtherHead {
        expected: IndexPin,
        published: IndexPin,
    },
    /// The runtime closed, retired or lost its root.
    Released(String),
}

impl std::fmt::Display for NotSettled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotSettled::Hang(busy) => write!(
                f,
                "not_settled: {busy} after the {:?} hang backstop",
                crate::testing::HANG_BACKSTOP
            ),
            NotSettled::OtherHead {
                expected,
                published,
            } => write!(
                f,
                "not_settled: idle at {}:{}, not the expected head {}:{}",
                published.index_generation,
                published.index_revision,
                expected.index_generation,
                expected.index_revision
            ),
            NotSettled::Released(reason) => write!(f, "not_settled: checkout released: {reason}"),
        }
    }
}

impl std::error::Error for NotSettled {}

enum Sample {
    Idle(IndexPin),
    Busy { busy: SettleBusy, contended: bool },
    Released(String),
}

impl CheckoutRuntime {
    /// Wait until this checkout's startup reconciliation has committed and its
    /// watcher, FIFO and semantic catch-up (S) are idle, then return the published
    /// head. With `at`, that head must be `at`; an idle checkout at another head
    /// is an error at once, not a wait.
    ///
    /// It waits on the runtime's published state version, which the worker bumps
    /// after every iteration, and never sleeps; only external SQLite contention
    /// is retried after a bounded wait. A sample counts only if no state change
    /// was published while it was taken. Its one bound is `HANG_BACKSTOP`, and
    /// the error names the component still busy. It holds an admission lease, so
    /// the checkout cannot be released while it waits.
    ///
    /// It blocks the calling thread: async code uses [`Self::settled_for_tests`].
    #[doc(hidden)]
    pub fn await_settled_for_tests(&self, at: Option<IndexPin>) -> Result<IndexPin, NotSettled> {
        self.settled_within(at, true)
    }

    /// [`Self::await_settled_for_tests`] on a blocking thread, for async tests: the
    /// checkout's own async helpers keep the caller's runtime.
    #[doc(hidden)]
    pub async fn settled_for_tests(
        self: &std::sync::Arc<Self>,
        at: Option<IndexPin>,
    ) -> Result<IndexPin, NotSettled> {
        let runtime = std::sync::Arc::clone(self);
        tokio::task::spawn_blocking(move || runtime.await_settled_for_tests(at))
            .await
            .expect("the settled wait panicked")
    }

    /// One coherent sample, without waiting for busy work: `Hang` names the busy
    /// component at once. A sample the owner changed under is taken again. For
    /// tests of the settled wait itself.
    #[doc(hidden)]
    pub fn sample_settled_for_tests(&self, at: Option<IndexPin>) -> Result<IndexPin, NotSettled> {
        self.settled_within(at, false)
    }

    fn settled_within(&self, at: Option<IndexPin>, wait: bool) -> Result<IndexPin, NotSettled> {
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
        let resources = self
            .resources()
            .map_err(|error| NotSettled::Released(format!("{error:#}")))?;
        let scheduler = &resources.scheduler;
        let _lease = scheduler
            .admission_lease()
            .map_err(|error| NotSettled::Released(format!("{error:#}")))?;
        loop {
            // Recorded before the sample: a change during it is never missed.
            let seen = scheduler.state_version();
            let (busy, contended) = match self.settle_sample(&resources) {
                Sample::Idle(published) if scheduler.state_version() == seen => {
                    return match at {
                        Some(expected) if expected != published => Err(NotSettled::OtherHead {
                            expected,
                            published,
                        }),
                        _ => Ok(published),
                    };
                }
                // The state changed while it was sampled: sample it again
                // (the wait below returns at once).
                Sample::Idle(_) => (SettleBusy::Changing, false),
                Sample::Released(reason) => return Err(NotSettled::Released(reason)),
                Sample::Busy { busy, contended } => (busy, contended),
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || (!wait && busy != SettleBusy::Changing) {
                return Err(NotSettled::Hang(busy));
            }
            let timeout = if contended {
                CONTENTION_RETRY.min(remaining)
            } else {
                remaining
            };
            scheduler.wait_state_change(seen, timeout);
        }
    }

    fn settle_sample(&self, resources: &ActiveResources) -> Sample {
        let scheduler = &resources.scheduler;
        let read = |error: anyhow::Error| Sample::Busy {
            contended: crate::store::cli_read_retryable_contention(&error)
                || crate::store::transient_storage_contention(&error),
            busy: SettleBusy::Read(format!("{error:#}")),
        };
        match scheduler.runtime_phase() {
            RuntimePhase::Ready => {}
            RuntimePhase::Closing | RuntimePhase::Retired => {
                return Sample::Released(format!("{:?}", scheduler.runtime_phase()));
            }
            phase => {
                return Sample::Busy {
                    busy: SettleBusy::Startup {
                        phase: format!("{phase:?}"),
                        error: scheduler.startup_error(),
                    },
                    contended: false,
                };
            }
        }
        if scheduler.checkout_watch_pending() {
            return Sample::Busy {
                busy: SettleBusy::Watcher,
                contended: false,
            };
        }
        if scheduler.fifo_pending() {
            return Sample::Busy {
                busy: SettleBusy::Fifo,
                contended: false,
            };
        }
        match resources.store.pending_semantic_target() {
            Ok(None) => {}
            Ok(Some(_)) => {
                return Sample::Busy {
                    busy: SettleBusy::Semantic,
                    contended: false,
                };
            }
            Err(error) => return read(error),
        }
        let (response, catching_up) = match self.evidence_response() {
            Ok(admitted) => admitted,
            Err(error) => return read(error),
        };
        if catching_up {
            return Sample::Busy {
                busy: SettleBusy::CatchingUp,
                contended: false,
            };
        }
        match response
            .status()
            .and_then(|status| response.finish(status.revision))
        {
            Ok(pin) => Sample::Idle(pin),
            Err(error) => read(error),
        }
    }
}
