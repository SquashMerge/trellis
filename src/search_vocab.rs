//! Search vocabulary that storage, ranking and the embedder share: the candidate rows
//! storage reads and ranking orders, the role filter and exact-match policy, the query
//! budget with its error, the stored vector width and the opt-in phase timer.
//!
//! This file references no layer. Ranking stays in `search_rank.rs`, the embedder in
//! `search_embed`, and model configuration and ownership in `search_model.rs`; each
//! re-exports the names that moved from it.
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use crate::native_records::DeclarationRole;

/// Owner-reviewed Decision 0018 constant; not a runtime tuning parameter.
pub const CANDIDATE_DEPTH: usize = 200;

/// Width of a stored, quantized search vector.
pub(crate) const DIMENSIONS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SearchCandidateKey {
    pub version_id: i64,
    pub syntax_id: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchRole {
    #[default]
    Any,
    Production,
    Test,
}

impl SearchRole {
    pub fn accepts(self, role: DeclarationRole) -> bool {
        match self {
            Self::Any => true,
            Self::Production => role == DeclarationRole::Production,
            Self::Test => role == DeclarationRole::Test,
        }
    }
}

/// Measured declaration metadata selected from the same admitted index head.
#[derive(Clone, Debug, Default)]
pub struct SearchPolicy {
    pub exact: BTreeMap<SearchCandidateKey, DeclarationRole>,
    pub role: SearchRole,
}

impl SearchPolicy {
    pub fn exact_priority(&self, key: &SearchCandidateKey) -> u8 {
        match self.exact.get(key) {
            Some(DeclarationRole::Production) => 0,
            Some(DeclarationRole::Test) => 1,
            None => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchCandidate {
    pub key: SearchCandidateKey,
    pub card_hash: String,
    pub role: DeclarationRole,
}

/// Absolute query cutoff using the request's clock; never mutates global cancellation.
#[derive(Clone)]
pub struct QueryBudget {
    cutoff: Instant,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl QueryBudget {
    pub fn new(cutoff: Instant, clock: Arc<dyn Fn() -> Instant + Send + Sync>) -> Self {
        Self { cutoff, clock }
    }

    pub fn now(&self) -> Instant {
        (self.clock)()
    }

    pub fn remaining(&self) -> std::time::Duration {
        self.cutoff.saturating_duration_since(self.now())
    }

    pub fn check(&self) -> Result<(), EmbedError> {
        if self.now() >= self.cutoff {
            Err(EmbedError::BudgetExceeded)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
pub enum EmbedError {
    ModelUnavailable(String),
    InvalidInput(String),
    Cancelled,
    BudgetExceeded,
    Runtime(String),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelUnavailable(message) => write!(f, "model unavailable: {message}"),
            Self::InvalidInput(message) => write!(f, "invalid embedding input: {message}"),
            Self::Cancelled => f.write_str("embedding cancelled"),
            Self::BudgetExceeded => f.write_str("query embedding budget exceeded"),
            Self::Runtime(message) => write!(f, "embedding runtime: {message}"),
        }
    }
}

impl std::error::Error for EmbedError {}

/// Opt-in diagnostics for a quiet search query; never reads the clock when disabled.
pub(crate) fn profiling_enabled() -> bool {
    tracing::enabled!(target: "trellis::search_profile", tracing::Level::DEBUG)
}

pub(crate) struct SearchPhaseTimer {
    phase: &'static str,
    started: Option<std::time::Instant>,
}

impl SearchPhaseTimer {
    pub(crate) fn start(phase: &'static str) -> Self {
        Self {
            phase,
            started: profiling_enabled().then(std::time::Instant::now),
        }
    }

    pub(crate) fn record(phase: &'static str, elapsed: std::time::Duration) {
        tracing::debug!(
            target: "trellis::search_profile",
            phase,
            elapsed_us = elapsed.as_micros(),
            "search phase"
        );
    }
}

impl Drop for SearchPhaseTimer {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            Self::record(self.phase, started.elapsed());
        }
    }
}
