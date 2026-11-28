//! Admitted input identities that storage and the Architecture core share (#1393): the
//! typed fingerprint encoding, a document's semantic-input, producer-projection and
//! syntax-ID digests, and the logical semantic read charges. Storage computes them for one
//! revision; the dependency baseline in `architecture_core::dependencies` (query) compares
//! them. Pure over shared records, no I/O.
use crate::architecture_inputs::{SemanticFreshnessState, SemanticInputState, SemanticTupleState};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Logical admission charges, including repeated proofs and lookup dependencies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SemanticReadCounts {
    /// Admitted fact/alternative rows, before output filtering and representative selection.
    pub rows: usize,
    /// Proof metadata requests, not the number of distinct shared allocations.
    pub metadata: usize,
    /// Lookup rows charged on every metadata request, including cache hits.
    pub lookups: usize,
}

/// Typed, length-delimited SHA encoding. SQL NULL and empty values never alias.
pub(crate) struct Fingerprint(Sha256);

impl Fingerprint {
    pub(crate) fn new(domain: &str) -> Self {
        let mut this = Self(Sha256::new());
        this.text(domain);
        this
    }
    pub(crate) fn integer(&mut self, value: i64) {
        self.0.update([1]);
        self.0.update(value.to_be_bytes());
    }
    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.0.update([2]);
        self.0.update((value.len() as u64).to_be_bytes());
        self.0.update(value);
    }
    pub(crate) fn text(&mut self, value: &str) {
        self.0.update([3]);
        self.bytes(value.as_bytes());
    }
    pub(crate) fn optional<T>(&mut self, value: Option<T>, put: impl FnOnce(&mut Self, T)) {
        self.0.update([4, u8::from(value.is_some())]);
        if let Some(value) = value {
            put(self, value);
        }
    }
    pub(crate) fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

impl SemanticInputState {
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let Self {
            default_producer,
            projection,
            tuple,
            freshness,
        } = self;
        // A projection's current pin and a basis's freshness other than `stale` change
        // only metadata, which each core rebases (#1310); what they decide is hashed.
        let mut hash = Fingerprint::new("architecture:semantic-document:v2");
        hash.optional(default_producer.as_deref(), Fingerprint::text);
        hash.optional(projection.as_ref(), projection_state);
        hash.optional(
            tuple.as_ref(),
            |hash,
             SemanticTupleState {
                 selected,
                 state,
                 payload_id,
             }| {
                hash.integer(i64::from(*selected));
                hash.text(state);
                hash.optional(*payload_id, Fingerprint::integer);
            },
        );
        hash.optional(
            freshness.as_ref(),
            |hash, SemanticFreshnessState { basis_pin, values }| {
                if projection.is_some() {
                    pin(hash, basis_pin);
                }
                hash.integer(values.len() as i64);
                for (basis, freshness) in values {
                    hash.integer(*basis);
                    // Read B admits a row and its basis site unless its basis is stale.
                    hash.integer(i64::from(freshness == "stale"));
                }
            },
        );
        hash.finish()
    }
}

/// A projection's state without its current pin, which each core rebases (#1310).
fn projection_state(hash: &mut Fingerprint, projection: &crate::model::SemanticProjection) {
    let crate::model::SemanticProjection {
        current_pin: _,
        semantic_basis_pin,
        basis_native_revision_id,
        semantic_status,
    } = projection;
    pin(hash, semantic_basis_pin);
    hash.text(basis_native_revision_id);
    hash.text(match semantic_status {
        crate::model::SemanticStatus::OlderBasis => "olderBasis",
        crate::model::SemanticStatus::CatchingUp => "catchingUp",
        crate::model::SemanticStatus::NeedsReimport => "needsReimport",
    });
}

/// A document's projection under one producer, as a projected source of that producer
/// reads it to decide its endpoints' eligibility (#934, #1310).
pub(crate) fn producer_projection_digest(
    projection: Option<&crate::model::SemanticProjection>,
) -> [u8; 32] {
    let mut hash = Fingerprint::new("architecture:producer-projection:v1");
    hash.optional(projection, projection_state);
    hash.finish()
}

fn pin(hash: &mut Fingerprint, pin: &crate::model::IndexPin) {
    let crate::model::IndexPin {
        index_generation,
        index_revision,
    } = pin;
    hash.bytes(index_generation.as_bytes());
    hash.bytes(&index_revision.to_be_bytes());
}

pub(crate) fn sid_digest<'a>(ids: impl Iterator<Item = &'a str>) -> [u8; 32] {
    let ids: std::collections::BTreeSet<_> = ids.collect();
    let mut hash = Fingerprint::new("architecture:sids:v1");
    hash.integer(ids.len() as i64);
    for id in ids {
        hash.text(id);
    }
    hash.finish()
}
