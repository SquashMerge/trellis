//! Admitted input identities and the full-index dependency baseline.
use serde::Serialize;

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

pub(crate) use staged::*;

mod staged {
    use super::SemanticReadCounts;
    use crate::architecture_inputs::{
        ArchitectureFingerprints, DocumentFingerprint, ManifestInput, NativeFingerprint,
        RootTsconfigInput, SemanticFreshnessState, SemanticInputState, SemanticTupleState,
        Snapshot,
    };
    use crate::native_records::DocumentKey;
    use crate::references::{ReferenceBase, SourceChargePlan};
    use anyhow::{Result, ensure};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::Arc,
    };

    /// Positions always address the complete sorted document inventory, never a compact
    /// dirty list. This keeps retained endpoints valid across every eligible build.
    #[derive(Default, Debug)]
    pub(crate) struct ChangedDocuments {
        pub(crate) native: BTreeSet<DocumentKey>,
        pub(crate) dirty: BTreeSet<DocumentKey>,
        pub(crate) patched: BTreeSet<DocumentKey>,
        pub(crate) reused: BTreeSet<DocumentKey>,
    }

    pub(crate) fn changed_documents(
        snapshot: &Snapshot,
        fingerprints: &ArchitectureFingerprints,
        base: Option<&ArchitectureDependencies>,
    ) -> Result<std::result::Result<ChangedDocuments, crate::architecture_core::FullReason>> {
        use crate::architecture_core::FullReason;
        let Some(base) = base else {
            return Ok(Err(FullReason::NoBase));
        };
        let order: Vec<_> = fingerprints.documents.keys().cloned().collect();
        if order.as_slice() != base.order.as_ref() {
            return Ok(Err(FullReason::DocumentSetChanged));
        }
        ensure!(
            base.documents.len() == order.len(),
            "incomplete architecture base"
        );
        if manifest_digest(&snapshot.manifests) != base.manifests {
            return Ok(Err(FullReason::ManifestsChanged));
        }
        if root_config_digest(snapshot.root_tsconfig.as_ref()) != base.root_config {
            return Ok(Err(FullReason::RootTsconfigChanged));
        }
        if fingerprints.producers != base.producers {
            return Ok(Err(FullReason::ProducerSetChanged));
        }
        if fingerprints.selections != base.selections {
            return Ok(Err(FullReason::RawSelectionsChanged));
        }
        let mut changed = ChangedDocuments::default();
        let mut sid = BTreeSet::new();
        // Per document, the producers whose projection of it changed.
        let mut projections: Vec<BTreeSet<&str>> = Vec::with_capacity(order.len());
        // Revision-scoped metadata does not make a source Dirty: rejoined provenance IDs,
        // non-stale freshness values and a projection's current pin are rebased per core from
        // the stable inputs the source retains (#1310). Only what Read B admits or resolves
        // differently is in the semantic fingerprint.
        for (i, key) in order.iter().enumerate() {
            let current = &fingerprints.documents[key];
            let old = &base.documents[i].fingerprint;
            if current.native != old.native {
                changed.native.insert(key.clone());
                changed.dirty.insert(key.clone());
                if current.sids != old.sids {
                    sid.insert(u32::try_from(i)?);
                }
            }
            if current.semantic != old.semantic {
                changed.dirty.insert(key.clone());
            }
            let (now, then) = (&current.producer_projections, &old.producer_projections);
            projections.push(
                now.keys()
                    .chain(then.keys())
                    .filter(|producer| now.get(*producer) != then.get(*producer))
                    .map(String::as_str)
                    .collect(),
            );
        }
        for (key, document) in order.iter().zip(&base.documents) {
            // A projected source reads each endpoint's eligibility from that endpoint's
            // projection under the source's own producer at the current pin (#934). So it is
            // Dirty when that projection changes for a retained endpoint (#1310), whatever the
            // endpoint's default producer is.
            let producer = fingerprints.documents[key]
                .projected
                .then(|| fingerprints.semantic_states.get(key))
                .flatten()
                .and_then(|state| state.default_producer.as_deref());
            let eligibility_changed = producer.is_some_and(|producer| {
                document.references.eligibility.iter().any(|&endpoint| {
                    projections
                        .get(endpoint as usize)
                        .is_some_and(|changed| changed.contains(producer))
                })
            });
            if eligibility_changed
                || document
                    .references
                    .endpoints
                    .iter()
                    .any(|endpoint| sid.contains(endpoint))
            {
                changed.dirty.insert(key.clone());
            }
        }
        // A language with a catalog digest is covered by it: the build plan compares the
        // digests once the changed sets are known (#1314).
        if changed
            .native
            .iter()
            .any(|key| !crate::references::catalog_digested(&key.language))
        {
            return Ok(Err(FullReason::NonRustChanged));
        }
        if changed.dirty.len() > order.len() / 2 {
            return Ok(Err(FullReason::DirtyMajority));
        }
        for (key, document) in order.iter().zip(&base.documents) {
            if changed.dirty.contains(key) {
                continue;
            }
            let patch = document.references.endpoints.iter().any(|&endpoint| {
                order
                    .get(endpoint as usize)
                    .is_some_and(|key| changed.native.contains(key))
            });
            if patch {
                changed.patched.insert(key.clone());
            } else {
                changed.reused.insert(key.clone());
            }
        }
        Ok(Ok(changed))
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

    pub(crate) fn manifest_digest(manifests: &[ManifestInput]) -> [u8; 32] {
        let mut entries: Vec<_> = manifests.iter().collect();
        entries.sort_by_key(|entry| (&entry.source_set_id, &entry.path));
        let mut hash = Fingerprint::new("architecture:manifests:v1");
        hash.integer(entries.len() as i64);
        for ManifestInput {
            source_set_id,
            path,
            content_hash,
            bytes,
            // Provenance remains in current package descriptors and their Work, not this gate.
            native_revision_id: _,
            dependency_hash: _,
        } in entries
        {
            hash.text(source_set_id);
            hash.text(path);
            hash.optional(content_hash.as_deref(), Fingerprint::text);
            hash.optional(bytes.as_deref(), Fingerprint::bytes);
        }
        hash.finish()
    }

    pub(crate) fn root_config_digest(config: Option<&RootTsconfigInput>) -> [u8; 32] {
        let mut hash = Fingerprint::new("architecture:root-config:v1");
        hash.optional(
            config,
            |hash,
             RootTsconfigInput {
                 source_set_id,
                 path,
                 bytes,
             }| {
                hash.text(source_set_id);
                hash.text(path);
                hash.bytes(bytes);
            },
        );
        hash.finish()
    }

    #[derive(Clone)]
    pub(crate) struct ArchitectureDependencies {
        pub(crate) order: Arc<[DocumentKey]>,
        pub(crate) documents: Vec<Arc<DocumentDependencies>>,
        pub(crate) rust_catalog: [u8; 32],
        pub(crate) python_catalog: [u8; 32],
        pub(crate) javascript_catalog: [u8; 32],
        pub(crate) manifests: [u8; 32],
        pub(crate) root_config: [u8; 32],
        pub(crate) producers: [u8; 32],
        pub(crate) selections: [u8; 32],
        pub(crate) global_semantic: SemanticReadCounts,
    }

    #[derive(Clone)]
    pub(crate) struct DocumentDependencies {
        pub(crate) fingerprint: Arc<DocumentFingerprint>,
        pub(crate) references: Arc<ReferenceDependencies>,
        pub(crate) semantic: SemanticReadCounts,
    }

    #[derive(Clone)]
    pub(crate) struct ReferenceDependencies {
        pub(crate) work: usize,
        pub(crate) sites: usize,
        pub(crate) endpoints: Arc<[u32]>,
        /// A projected source's semantic endpoint documents, whose eligibility it reads.
        pub(crate) eligibility: Arc<[u32]>,
        pub(crate) plan: Option<Arc<SourceChargePlan>>,
    }

    impl ArchitectureDependencies {
        /// Reuse immutable subparts independently: fresh memo warmth can change only q_s
        /// while the source's fingerprint, request plan and endpoint array remain shared.
        pub(crate) fn share_unchanged(&mut self, base: &Self) {
            if self.order.as_ref() != base.order.as_ref() {
                return;
            }
            self.order = base.order.clone();
            for (current, old) in self.documents.iter_mut().zip(&base.documents) {
                let document = Arc::make_mut(current);
                if document.fingerprint == old.fingerprint {
                    document.fingerprint = old.fingerprint.clone();
                }
                let references = Arc::make_mut(&mut document.references);
                if references.endpoints == old.references.endpoints {
                    references.endpoints = old.references.endpoints.clone();
                }
                if references.eligibility == old.references.eligibility {
                    references.eligibility = old.references.eligibility.clone();
                }
                let same_plan = match (&references.plan, &old.references.plan) {
                    (None, None) => true,
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    _ => false,
                };
                if references.work == old.references.work
                    && references.sites == old.references.sites
                    && references.endpoints == old.references.endpoints
                    && references.eligibility == old.references.eligibility
                    && same_plan
                {
                    document.references = old.references.clone();
                }
                if Arc::ptr_eq(&document.fingerprint, &old.fingerprint)
                    && Arc::ptr_eq(&document.references, &old.references)
                    && document.semantic == old.semantic
                {
                    *current = old.clone();
                }
            }
        }

        pub(crate) fn full(
            snapshot: &Snapshot,
            fingerprints: ArchitectureFingerprints,
            counts: &BTreeMap<DocumentKey, SemanticReadCounts>,
            global_semantic: SemanticReadCounts,
            references: &ReferenceBase,
        ) -> Result<Self> {
            let ArchitectureFingerprints {
                documents: fingerprints,
                semantic_states: _,
                producers,
                selections,
                native_revision: _,
            } = fingerprints;
            let mut order: Vec<_> = snapshot
                .documents
                .iter()
                .map(|doc| doc.key.clone())
                .collect();
            order.sort();
            ensure!(
                order.windows(2).all(|pair| pair[0] != pair[1]),
                "duplicate architecture dependency key"
            );
            u32::try_from(order.len())?;
            let facts = references.source_facts();
            ensure!(
                facts.len() == order.len(),
                "incomplete architecture source facts"
            );
            ensure!(
                fingerprints.len() == order.len() && counts.len() == order.len(),
                "incomplete architecture dependency inputs"
            );
            let mut documents = Vec::with_capacity(order.len());
            for (key, fact) in order.iter().zip(facts) {
                ensure!(
                    fact.endpoints
                        .iter()
                        .all(|&endpoint| (endpoint as usize) < order.len()),
                    "invalid architecture endpoint"
                );
                ensure!(
                    fact.endpoints.windows(2).all(|pair| pair[0] < pair[1])
                        && fact.eligibility.windows(2).all(|pair| pair[0] < pair[1])
                        && fact
                            .eligibility
                            .iter()
                            .all(|&endpoint| (endpoint as usize) < order.len()),
                    "unordered architecture endpoints"
                );
                let fingerprint = fingerprints
                    .get(key)
                    .ok_or_else(|| anyhow::anyhow!("missing architecture fingerprint"))?;
                documents.push(Arc::new(DocumentDependencies {
                    fingerprint: Arc::new(fingerprint.clone()),
                    references: Arc::new(ReferenceDependencies {
                        work: fact.work,
                        sites: fact.sites,
                        endpoints: fact.endpoints.clone().into(),
                        eligibility: fact.eligibility.clone().into(),
                        plan: fact.plan.clone(),
                    }),
                    semantic: *counts
                        .get(key)
                        .ok_or_else(|| anyhow::anyhow!("missing architecture semantic counts"))?,
                }));
            }
            Ok(Self {
                order: order.into(),
                documents,
                rust_catalog: references.rust_catalog_digest(),
                python_catalog: references.python_catalog_digest(),
                javascript_catalog: references.javascript_catalog_digest(),
                manifests: manifest_digest(&snapshot.manifests),
                root_config: root_config_digest(snapshot.root_tsconfig.as_ref()),
                producers,
                selections,
                global_semantic,
            })
        }

        pub(crate) fn canonical(&self) -> Value {
            let Self {
                order,
                documents,
                rust_catalog,
                python_catalog,
                javascript_catalog,
                manifests,
                root_config,
                producers,
                selections,
                global_semantic,
            } = self;
            let documents: Vec<_> = documents.iter().map(|document| {
                let DocumentDependencies { fingerprint, references, semantic } = document.as_ref();
                let DocumentFingerprint { native, semantic: semantic_fingerprint, sids, projected, producer_projections } = fingerprint.as_ref();
                let NativeFingerprint { version_id, coverage_requested, coverage_selected, coverage_state, coverage_diagnostic } = native;
                let ReferenceDependencies { work, sites, endpoints, eligibility, plan } = references.as_ref();
                json!({"native": {"version_id": version_id, "coverage_requested": coverage_requested, "coverage_selected": coverage_selected,
                    "coverage_state": coverage_state, "coverage_diagnostic": coverage_diagnostic},
                    "semanticFingerprint": semantic_fingerprint, "sids": sids, "projected": projected, "producerProjections": producer_projections,
                    "references": {"work": work, "sites": sites, "endpoints": endpoints.as_ref(), "eligibility": eligibility.as_ref(), "plan": plan.as_deref()}, "semantic": canonical_counts(semantic)})
            }).collect();
            json!({"order": order.as_ref(), "documents": documents, "rustCatalog": rust_catalog,
                "pythonCatalog": python_catalog,
                "javascriptCatalog": javascript_catalog,
                "manifests": manifests, "rootConfig": root_config, "producers": producers,
                "selections": selections, "globalSemantic": canonical_counts(global_semantic)})
        }
    }

    fn canonical_counts(counts: &SemanticReadCounts) -> Value {
        let SemanticReadCounts {
            rows,
            metadata,
            lookups,
        } = counts;
        json!({"rows": rows, "metadata": metadata, "lookups": lookups})
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::architecture_inputs::ManifestInput;

    #[test]
    fn manifest_gate_fields() {
        let manifest = ManifestInput {
            source_set_id: "source".into(),
            path: "Cargo.toml".into(),
            content_hash: Some("hash".into()),
            bytes: Some(b"[package]".to_vec()),
            native_revision_id: "native".into(),
            dependency_hash: "dependencies".into(),
        };
        let baseline = manifest_digest(std::slice::from_ref(&manifest));
        for mutate in [
            (|entry: &mut ManifestInput| entry.native_revision_id.push('2'))
                as fn(&mut ManifestInput),
            |entry| entry.dependency_hash.push('2'),
        ] {
            let mut provenance = manifest.clone();
            mutate(&mut provenance);
            assert_eq!(
                baseline,
                manifest_digest(&[provenance]),
                "provenance is not parser input"
            );
        }
        let mut changed = manifest.clone();
        changed.bytes = Some(b"[workspace]".to_vec());
        assert_ne!(
            baseline,
            manifest_digest(&[changed]),
            "captured bytes are parser input"
        );
        for mutate in [
            (|entry: &mut ManifestInput| entry.source_set_id.push('2')) as fn(&mut ManifestInput),
            |entry| entry.path.push('2'),
            |entry| entry.content_hash = None,
            |entry| entry.content_hash = Some(String::new()),
            |entry| entry.bytes = None,
            |entry| entry.bytes = Some(Vec::new()),
        ] {
            let mut changed = manifest.clone();
            mutate(&mut changed);
            assert_ne!(baseline, manifest_digest(&[changed]));
        }
        assert_ne!(baseline, manifest_digest(&[]));
        assert_ne!(
            baseline,
            manifest_digest(&[manifest.clone(), manifest.clone()])
        );
        let absent = ManifestInput {
            content_hash: None,
            bytes: None,
            ..manifest.clone()
        };
        assert_ne!(manifest_digest(&[]), manifest_digest(&[absent]));
        let nested = ManifestInput {
            path: "tools/Cargo.toml".into(),
            ..manifest.clone()
        };
        assert_eq!(
            manifest_digest(&[manifest.clone(), nested.clone()]),
            manifest_digest(&[nested, manifest])
        );
    }

    #[test]
    fn root_config_and_sid_digest_fields() {
        use crate::architecture_inputs::RootTsconfigInput;
        let config = RootTsconfigInput {
            source_set_id: "source".into(),
            path: "tsconfig.json".into(),
            bytes: b"{}".to_vec(),
        };
        let baseline = root_config_digest(Some(&config));
        assert_ne!(baseline, root_config_digest(None));
        for mutate in [
            (|config: &mut RootTsconfigInput| config.source_set_id.push('2'))
                as fn(&mut RootTsconfigInput),
            |config| config.path = "jsconfig.json".into(),
            |config| config.bytes.clear(),
        ] {
            let mut changed = config.clone();
            mutate(&mut changed);
            assert_ne!(baseline, root_config_digest(Some(&changed)));
        }
        assert_eq!(
            sid_digest(["b", "a", "a"].into_iter()),
            sid_digest(["a", "b"].into_iter())
        );
        assert_ne!(sid_digest(["a"].into_iter()), sid_digest([].into_iter()));
        assert_ne!(
            sid_digest(["ab", "c"].into_iter()),
            sid_digest(["a", "bc"].into_iter())
        );
    }

    #[test]
    fn semantic_fingerprint_components_and_absence() {
        use crate::architecture_inputs::{
            SemanticFreshnessState, SemanticInputState, SemanticTupleState,
        };
        use crate::model::{IndexPin, SemanticProjection, SemanticStatus};
        use std::collections::BTreeMap;
        let pin = IndexPin {
            index_generation: uuid::Uuid::from_u128(1),
            index_revision: 2,
        };
        let state = SemanticInputState {
            default_producer: Some("producer".into()),
            projection: Some(SemanticProjection {
                current_pin: pin,
                semantic_basis_pin: IndexPin {
                    index_revision: 1,
                    ..pin
                },
                basis_native_revision_id: "native".into(),
                semantic_status: SemanticStatus::OlderBasis,
            }),
            tuple: Some(SemanticTupleState {
                selected: true,
                state: "complete".into(),
                payload_id: Some(7),
            }),
            freshness: Some(SemanticFreshnessState {
                basis_pin: pin,
                values: BTreeMap::from([(1, "fresh".into()), (2, "possiblyStale".into())]),
            }),
        };
        let baseline = state.fingerprint();
        for mutate in [
            (|state: &mut SemanticInputState| state.default_producer = None)
                as fn(&mut SemanticInputState),
            |state| state.default_producer = Some(String::new()),
            |state| state.default_producer = Some("other".into()),
            |state| state.projection = None,
            |state| {
                state
                    .projection
                    .as_mut()
                    .unwrap()
                    .semantic_basis_pin
                    .index_generation = uuid::Uuid::from_u128(2)
            },
            |state| {
                state
                    .projection
                    .as_mut()
                    .unwrap()
                    .semantic_basis_pin
                    .index_revision += 1
            },
            |state| {
                state
                    .projection
                    .as_mut()
                    .unwrap()
                    .basis_native_revision_id
                    .push('2')
            },
            |state| state.projection.as_mut().unwrap().semantic_status = SemanticStatus::CatchingUp,
            |state| {
                state.projection.as_mut().unwrap().semantic_status = SemanticStatus::NeedsReimport
            },
            |state| state.tuple = None,
            |state| state.tuple.as_mut().unwrap().selected = false,
            |state| state.tuple.as_mut().unwrap().state = "partial".into(),
            |state| state.tuple.as_mut().unwrap().payload_id = None,
            |state| state.tuple.as_mut().unwrap().payload_id = Some(8),
            |state| state.freshness = None,
            |state| {
                state.freshness.as_mut().unwrap().basis_pin.index_generation =
                    uuid::Uuid::from_u128(2)
            },
            |state| state.freshness.as_mut().unwrap().basis_pin.index_revision += 1,
            |state| state.freshness.as_mut().unwrap().values.clear(),
            |state| {
                state.freshness.as_mut().unwrap().values.remove(&1);
            },
            |state| {
                state
                    .freshness
                    .as_mut()
                    .unwrap()
                    .values
                    .insert(3, "fresh".into());
            },
            |state| {
                state
                    .freshness
                    .as_mut()
                    .unwrap()
                    .values
                    .insert(2, "stale".into());
            },
        ] {
            let mut changed = state.clone();
            mutate(&mut changed);
            assert_ne!(baseline, changed.fingerprint(), "{changed:?}");
        }
        // Revision-scoped metadata that each core rebases is not semantic state (#1310): a
        // projection's current pin, and a freshness value that stays other than `stale`.
        for mutate in [
            (|state: &mut SemanticInputState| {
                state
                    .projection
                    .as_mut()
                    .unwrap()
                    .current_pin
                    .index_generation = uuid::Uuid::from_u128(2)
            }) as fn(&mut SemanticInputState),
            |state| {
                state
                    .projection
                    .as_mut()
                    .unwrap()
                    .current_pin
                    .index_revision += 1
            },
            |state| {
                state
                    .freshness
                    .as_mut()
                    .unwrap()
                    .values
                    .insert(1, "possiblyStale".into());
            },
            |state| {
                state
                    .freshness
                    .as_mut()
                    .unwrap()
                    .values
                    .insert(2, "fresh".into());
            },
        ] {
            let mut changed = state.clone();
            mutate(&mut changed);
            assert_eq!(baseline, changed.fingerprint(), "{changed:?}");
        }
        let mut empty = state.clone();
        empty.freshness.as_mut().unwrap().values.clear();
        let empty_digest = empty.fingerprint();
        empty.freshness = None;
        assert_ne!(
            empty_digest,
            empty.fingerprint(),
            "an absent map is not an empty one"
        );
        let mut reordered = state.clone();
        reordered.freshness.as_mut().unwrap().values =
            [(2, "possiblyStale".into()), (1, "fresh".into())]
                .into_iter()
                .collect();
        assert_eq!(baseline, reordered.fingerprint());
        let mut catching_up = state.clone();
        catching_up.projection.as_mut().unwrap().semantic_status = SemanticStatus::CatchingUp;
        let mut needs_import = catching_up.clone();
        needs_import.projection.as_mut().unwrap().semantic_status = SemanticStatus::NeedsReimport;
        assert_ne!(catching_up.fingerprint(), needs_import.fingerprint());
    }
}
