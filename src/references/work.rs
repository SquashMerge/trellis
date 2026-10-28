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
    per_document: usize,
    per_kib: usize,
    ceiling: usize,
) -> usize {
    let kib = (bytes / 1024).saturating_add(usize::from(!bytes.is_multiple_of(1024)));
    5_000_000usize
        .saturating_add(documents.saturating_mul(per_document))
        .saturating_add(kib.saturating_mul(per_kib))
        .min(ceiling)
}
pub(crate) fn reference_limit(documents: usize, bytes: usize) -> usize {
    scaled_limit(documents, bytes, 20_000, 512, REFERENCE_CEILING)
}
pub(crate) fn rules_limit(documents: usize, bytes: usize) -> usize {
    scaled_limit(documents, bytes, 10_000, 256, RULES_CEILING)
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
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_grow_from_full_admission_and_round_aggregate_bytes() {
        assert_eq!(reference_limit(0, 0), 5_000_000);
        assert_eq!(rules_limit(0, 0), 5_000_000);
        assert_eq!(reference_limit(2, 0) - reference_limit(1, 0), 20_000);
        assert_eq!(rules_limit(2, 0) - rules_limit(1, 0), 10_000);
        let utf8 = "é".len();
        assert_eq!(reference_limit(2, utf8 + 1022), reference_limit(2, 1024));
        assert_eq!(reference_limit(2, 1025) - reference_limit(2, 1024), 512);
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
        };
        assert_eq!(snapshot_bytes(&snapshot, &mut || Ok(())).unwrap(), 4);
        assert_eq!(
            snapshot_bytes(&snapshot, &mut || anyhow::bail!("cancelled"))
                .unwrap_err()
                .to_string(),
            "cancelled"
        );
    }
}
