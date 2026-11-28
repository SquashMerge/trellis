//! Request-local work accounting, sized from the full admitted snapshot.
use super::Snapshot;
use anyhow::Result;

pub(crate) const REFERENCE_CEILING: usize = 100_000_000;
pub(crate) const RULES_CEILING: usize = 50_000_000;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Work {
    pub(crate) used: usize,
    pub(crate) limit: usize,
}
impl Work {
    pub(crate) fn new(limit: usize) -> Self {
        Self { used: 0, limit }
    }
    pub(crate) fn remaining(self) -> usize {
        self.limit.saturating_sub(self.used)
    }
    pub(crate) fn charge(&mut self, amount: usize) -> bool {
        let Some(used) = self.used.checked_add(amount) else {
            self.used = usize::MAX;
            return false;
        };
        self.used = used;
        used <= self.limit
    }
}

fn scaled_limit(
    documents: usize,
    bytes: usize,
    (base, per_document, per_kib): (usize, usize, usize),
    ceiling: usize,
) -> usize {
    let kib = (bytes / 1024).saturating_add(usize::from(!bytes.is_multiple_of(1024)));
    base.saturating_add(documents.saturating_mul(per_document))
        .saturating_add(kib.saturating_mul(per_kib))
        .min(ceiling)
}
pub(crate) fn reference_limit(documents: usize, bytes: usize) -> usize {
    // Owner ruling 2026-11-27 (#1359, option A): 1.25 times the #373 base and growth terms.
    let limit = scaled_limit(
        documents,
        bytes,
        (6_250_000, 25_000, 640),
        REFERENCE_CEILING,
    );
    #[cfg(feature = "test-support")]
    let limit = limit.min(TEST_REFERENCE_LIMIT.get());
    limit
}

#[cfg(feature = "test-support")]
thread_local! {
    static TEST_REFERENCE_LIMIT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(REFERENCE_CEILING) };
}

/// A captured build budget. Capture on the requester thread, carry it in the build context,
/// and call `apply` on the worker around full computation or each prepare/finish stage.
/// Catalog and file workers receive explicit per-job limits; they do not inherit thread state.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy)]
pub struct ReferenceWorkLimitForTests(usize);

#[cfg(feature = "test-support")]
impl ReferenceWorkLimitForTests {
    pub fn capture() -> Self {
        Self(TEST_REFERENCE_LIMIT.get())
    }

    pub fn apply(self) -> ReferenceWorkLimitGuardForTests {
        ReferenceWorkLimitGuardForTests {
            previous: TEST_REFERENCE_LIMIT.replace(self.0),
            thread: std::marker::PhantomData,
        }
    }
}

/// Restores the applying thread's previous budget, including on unwinding. A guard cannot
/// move to another thread; only the captured value can cross the worker boundary.
#[cfg(feature = "test-support")]
#[must_use]
pub struct ReferenceWorkLimitGuardForTests {
    previous: usize,
    thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(feature = "test-support")]
impl ReferenceWorkLimitGuardForTests {
    pub fn set(&mut self, limit: usize) {
        TEST_REFERENCE_LIMIT.set(limit);
    }
}

#[cfg(feature = "test-support")]
impl Drop for ReferenceWorkLimitGuardForTests {
    fn drop(&mut self) {
        TEST_REFERENCE_LIMIT.set(self.previous);
    }
}

/// Lower this thread's reference budget until the returned guard drops. Production scaling
/// is unchanged. For another worker, explicitly capture and apply `ReferenceWorkLimitForTests`.
#[cfg(feature = "test-support")]
pub fn set_reference_work_limit_for_tests(limit: usize) -> ReferenceWorkLimitGuardForTests {
    ReferenceWorkLimitForTests(limit).apply()
}
pub(crate) fn rules_limit(documents: usize, bytes: usize) -> usize {
    scaled_limit(documents, bytes, (5_000_000, 10_000, 256), RULES_CEILING)
}

// Counting only: admission and source validation remain with their existing owners.
pub(crate) fn snapshot_bytes(
    snapshot: &Snapshot,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<usize> {
    let mut bytes = 0usize;
    for doc in &snapshot.documents {
        check()?;
        bytes = bytes.saturating_add(doc.file.text.len());
    }
    for manifest in &snapshot.manifests {
        check()?;
        bytes = bytes.saturating_add(manifest.bytes.as_ref().map_or(0, Vec::len));
    }
    if let Some(config) = &snapshot.root_tsconfig {
        check()?;
        bytes = bytes.saturating_add(config.bytes.len());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "test-support")]
    #[test]
    fn reference_work_override_is_thread_scoped_propagated_and_restored() {
        let default = reference_limit(1, 0);
        let mut requester = set_reference_work_limit_for_tests(11);
        let captured = ReferenceWorkLimitForTests::capture();
        let (ready, started) = std::sync::mpsc::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            assert_eq!(
                reference_limit(1, 0),
                default,
                "an unrelated worker is unchanged"
            );
            {
                let mut applied = captured.apply();
                assert_eq!(reference_limit(1, 0), 11, "explicitly propagated budget");
                ready.send(()).unwrap();
                resumed.recv().unwrap();
                assert_eq!(
                    reference_limit(1, 0),
                    11,
                    "requester changes are not global"
                );
                applied.set(7);
                {
                    let _nested = set_reference_work_limit_for_tests(3);
                    assert_eq!(reference_limit(1, 0), 3);
                }
                assert_eq!(
                    reference_limit(1, 0),
                    7,
                    "nested scope restores its predecessor"
                );
            }
            assert_eq!(reference_limit(1, 0), default, "worker scope restored");
            assert!(
                std::panic::catch_unwind(|| {
                    let _applied = captured.apply();
                    panic!("unwind the budget scope");
                })
                .is_err()
            );
            assert_eq!(
                reference_limit(1, 0),
                default,
                "unwinding also restores the worker"
            );
        });
        started.recv().unwrap();
        requester.set(17);
        resume.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(
            reference_limit(1, 0),
            17,
            "worker restoration does not change the requester"
        );
        drop(requester);
        assert_eq!(reference_limit(1, 0), default);
    }

    #[test]
    fn limits_grow_from_full_admission_and_round_aggregate_bytes() {
        assert_eq!(reference_limit(0, 0), 6_250_000);
        assert_eq!(rules_limit(0, 0), 5_000_000);
        assert_eq!(reference_limit(2, 0) - reference_limit(1, 0), 25_000);
        assert_eq!(rules_limit(2, 0) - rules_limit(1, 0), 10_000);
        let utf8 = "é".len();
        assert_eq!(reference_limit(2, utf8 + 1022), reference_limit(2, 1024));
        assert_eq!(reference_limit(2, 1025) - reference_limit(2, 1024), 640);
        assert_eq!(rules_limit(2, 1025) - rules_limit(2, 1024), 256);
        assert_eq!(reference_limit(usize::MAX, usize::MAX), REFERENCE_CEILING);
        assert_eq!(rules_limit(usize::MAX, usize::MAX), RULES_CEILING);
    }

    #[test]
    fn charges_enforce_the_actual_limit_and_never_wrap() {
        let mut work = Work::new(2);
        assert!(work.charge(2));
        assert_eq!(work.remaining(), 0);
        assert!(!work.charge(1));
        let mut work = Work {
            used: usize::MAX,
            limit: usize::MAX,
        };
        assert!(!work.charge(1));
    }

    #[test]
    fn absent_manifests_add_no_bytes_and_present_records_count_once() {
        let manifest = super::super::external::ManifestInput {
            source_set_id: "source".into(),
            native_revision_id: "revision".into(),
            dependency_hash: "hash".into(),
            path: "Cargo.toml".into(),
            content_hash: None,
            bytes: None,
        };
        let mut present = manifest.clone();
        present.bytes = Some("é".as_bytes().to_vec());
        let snapshot = Snapshot {
            pin: crate::model::IndexPin {
                index_generation: uuid::Uuid::nil(),
                index_revision: 1,
            },
            documents: vec![super::super::DocumentInput {
                semantic: Default::default(),
                key: crate::native_evidence::DocumentKey {
                    source_set_id: "source".into(),
                    language: "rust".into(),
                    path: "empty.rs".into(),
                },
                file: crate::model::SourceFile {
                    path: "empty.rs".into(),
                    hash: "hash".into(),
                    language: "rust".into(),
                    text: "é".into(),
                },
                declarations: Vec::new(),
                calls: Vec::new(),
            }],
            manifests: vec![manifest, present],
            root_tsconfig: None,
            evidence_gaps: Default::default(),
            unparsed: Default::default(),
        };
        assert_eq!(snapshot_bytes(&snapshot, &mut || Ok(())).unwrap(), 4);
        let mut with_config = Snapshot {
            root_tsconfig: Some(super::super::tsconfig::RootTsconfigInput {
                source_set_id: "source".into(),
                path: "tsconfig.json".into(),
                bytes: b"{}".to_vec(),
            }),
            ..snapshot
        };
        assert_eq!(snapshot_bytes(&with_config, &mut || Ok(())).unwrap(), 6);
        with_config.root_tsconfig = None;
        assert_eq!(snapshot_bytes(&with_config, &mut || Ok(())).unwrap(), 4);
        let snapshot = with_config;
        assert_eq!(
            snapshot_bytes(&snapshot, &mut || anyhow::bail!("cancelled"))
                .unwrap_err()
                .to_string(),
            "cancelled"
        );
    }
}
