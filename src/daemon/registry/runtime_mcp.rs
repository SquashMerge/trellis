//! The checkout runtime's MCP call clocks, watcher health and describe admission.
use super::{CheckoutEvidence, CheckoutRuntime, SelectedCheckout};
use crate::http::RuntimePhase;
use std::time::{Duration, Instant};

impl CheckoutRuntime {
    /// The clock MCP call deadlines are checked against: real time (or a
    /// paused test origin), plus virtual time advanced on this checkout.
    pub(crate) fn mcp_clock_now(&self) -> Instant {
        #[cfg(feature = "test-support")]
        let skew = Duration::from_millis(
            self.mcp_clock_skew_ms
                .load(std::sync::atomic::Ordering::Acquire),
        );
        #[cfg(not(feature = "test-support"))]
        let skew = Duration::ZERO;
        #[cfg(feature = "test-support")]
        if let Some(origin) = *self.mcp_call_clock_origin.lock().unwrap() {
            return origin + skew;
        }
        Instant::now() + skew
    }

    /// The optional-hint clock for a call accepted at `accepted`.
    pub(crate) fn mcp_hint_clock_now(&self, accepted: Instant) -> Instant {
        #[cfg(feature = "test-support")]
        if self.mcp_hint_clock_origin.lock().unwrap().is_some() {
            return accepted
                + Duration::from_millis(
                    self.mcp_clock_skew_ms
                        .load(std::sync::atomic::Ordering::Acquire),
                );
        }
        let _ = accepted;
        self.mcp_clock_now()
    }

    /// Read the published watcher health; it never waits for an inventory.
    pub fn mcp_watcher_degraded(&self) -> anyhow::Result<bool> {
        Ok(self.resources()?.scheduler.checkout_watcher_degraded())
    }

    /// Admit W1 through the same selected response path; a missing head is not corruption.
    pub fn mcp_describe(
        &self,
        selected: Option<&SelectedCheckout>,
    ) -> anyhow::Result<(
        Option<CheckoutEvidence>,
        bool,
        &'static str,
        Option<serde_json::Value>,
    )> {
        let admitted = match selected {
            Some(selected) => self.evidence_for_selected(selected),
            None => self.evidence_response(),
        };
        match admitted {
            Ok((response, catching_up)) => Ok((Some(response), catching_up, "ready", None)),
            Err(error)
                if error
                    .downcast_ref::<crate::store::topology::IndexNotReady>()
                    .is_some() =>
            {
                let catching_up = self.catching_up();
                if self.initial_indexing {
                    let started = self.resources().is_ok_and(|resources| {
                        !matches!(
                            resources.scheduler.runtime_phase(),
                            RuntimePhase::Activating
                                | RuntimePhase::Reconciling { started: false, .. }
                        )
                    });
                    let progress =
                        (!started).then(|| serde_json::json!({"completed":0,"total":null}));
                    Ok((None, catching_up, "indexing", progress))
                } else {
                    Ok((None, catching_up, "reconciling", None))
                }
            }
            Err(error) => Err(error),
        }
    }
}
