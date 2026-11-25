//! A catalog's isomorphism digest (#1314): typed, length-delimited values, with each byte
//! position replaced by its rank among the positions the catalog holds in its document.
use super::{SiteResolution, Target, external};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct Encoder {
    positions: BTreeMap<usize, BTreeSet<usize>>,
    ranks: BTreeMap<usize, BTreeMap<usize, usize>>,
    hash: Option<Sha256>,
}

impl Encoder {
    /// Runs `encode` twice: once to collect each document's positions, then to hash them by
    /// rank with every other value. Both passes must encode the same catalog.
    pub(super) fn digest(encode: impl Fn(&mut Self)) -> [u8; 32] {
        let mut encoder = Self::default();
        encode(&mut encoder);
        encoder.ranks = std::mem::take(&mut encoder.positions)
            .into_iter()
            .map(|(document, positions)| {
                (
                    document,
                    positions
                        .into_iter()
                        .enumerate()
                        .map(|(rank, position)| (position, rank))
                        .collect(),
                )
            })
            .collect();
        encoder.hash = Some(Sha256::new());
        encode(&mut encoder);
        encoder.hash.expect("digest pass").finalize().into()
    }
    pub(super) fn word(&mut self, value: usize) {
        if let Some(hash) = &mut self.hash {
            hash.update((value as u64).to_le_bytes());
        }
    }
    pub(super) fn flag(&mut self, value: bool) {
        if let Some(hash) = &mut self.hash {
            hash.update([u8::from(value)]);
        }
    }
    pub(super) fn text(&mut self, value: &str) {
        self.word(value.len());
        if let Some(hash) = &mut self.hash {
            hash.update(value.as_bytes());
        }
    }
    pub(super) fn optional_text(&mut self, value: Option<&str>) {
        self.flag(value.is_some());
        if let Some(value) = value {
            self.text(value);
        }
    }
    pub(super) fn strings(&mut self, values: &[String]) {
        self.word(values.len());
        for value in values {
            self.text(value);
        }
    }
    pub(super) fn indices(&mut self, values: &BTreeSet<usize>) {
        self.word(values.len());
        for value in values {
            self.word(*value);
        }
    }
    pub(super) fn names(&mut self, values: &BTreeSet<String>) {
        self.word(values.len());
        for value in values {
            self.text(value);
        }
    }
    pub(super) fn position(&mut self, document: usize, value: usize) {
        if self.hash.is_some() {
            self.word(self.ranks[&document][&value]);
        } else {
            self.positions.entry(document).or_default().insert(value);
        }
    }
    pub(super) fn declaration(&mut self, declaration: &super::shared::DeclarationId) {
        let super::shared::DeclarationId {
            document,
            start,
            end,
            sid,
        } = declaration;
        self.word(*document);
        self.position(*document, *start);
        self.position(*document, *end);
        self.text(sid);
    }
    pub(super) fn identity(&mut self, identity: &external::ExternalIdentity) {
        let external::ExternalIdentity {
            kind,
            source_set_id,
            language,
            package,
            standard_library,
        } = identity;
        match kind {
            external::ExternalKind::ExternalPackage => self.word(0),
        }
        self.text(source_set_id);
        self.text(language);
        self.text(package);
        self.flag(*standard_library);
    }
    pub(super) fn target(&mut self, target: &Target) {
        match target {
            Target::Native(document) => {
                self.word(0);
                self.word(*document);
            }
            Target::External(identity) => {
                self.word(1);
                self.identity(identity);
            }
        }
    }
    /// A precomputed site resolution, keyed by its source's ranked range in `document`.
    pub(super) fn resolution(
        &mut self,
        (document, start, end): (usize, usize, usize),
        resolution: &SiteResolution,
    ) {
        let SiteResolution {
            targets,
            proof,
            residual,
        } = resolution;
        self.word(document);
        self.position(document, start);
        self.position(document, end);
        self.word(targets.len());
        for target in targets {
            self.target(target);
        }
        self.indices(proof);
        self.optional_text(*residual);
    }
}

/// The snapshot of exactly `files` written under `root` (emptied first), with `manifests` and
/// `root_tsconfig` in the documents' one source set: one root keeps that source set the same
/// across a test's variants.
#[cfg(test)]
pub(super) fn snapshot_in(
    root: &std::path::Path,
    files: &[(&str, &str)],
    manifests: &[(&str, &str)],
    root_tsconfig: Option<&str>,
) -> super::Snapshot {
    use std::sync::{Arc, atomic::AtomicBool};
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(path).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let identity = crate::store::topology::WorkspaceIdentity::discover(Some(root), root).unwrap();
    let (graph, native) = crate::indexer::index_workspace_with_native(
        &crate::indexer::IndexOptions::new(root.to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let documents: Vec<super::DocumentInput> = graph
        .files
        .into_iter()
        .map(|file| {
            let key = native
                .revision
                .documents
                .iter()
                .find(|doc| doc.key.path == file.path)
                .unwrap()
                .key
                .clone();
            super::DocumentInput {
                semantic: Default::default(),
                declarations: native
                    .declarations
                    .iter()
                    .filter(|row| row.document == key)
                    .cloned()
                    .collect(),
                calls: native
                    .calls
                    .iter()
                    .filter(|row| row.document == key)
                    .cloned()
                    .collect(),
                key,
                file,
            }
        })
        .collect();
    let source_set = documents[0].key.source_set_id.clone();
    super::Snapshot {
        pin: crate::model::IndexPin {
            index_generation: uuid::Uuid::nil(),
            index_revision: 1,
        },
        manifests: manifests
            .iter()
            .map(|(path, text)| external::ManifestInput {
                source_set_id: source_set.clone(),
                path: (*path).into(),
                content_hash: Some(hex::encode(Sha256::digest(text.as_bytes()))),
                bytes: Some(text.as_bytes().to_vec()),
                native_revision_id: "native".into(),
                dependency_hash: "dependencies".into(),
            })
            .collect(),
        root_tsconfig: root_tsconfig.map(|text| super::tsconfig::RootTsconfigInput {
            source_set_id: source_set.clone(),
            path: "tsconfig.json".into(),
            bytes: text.as_bytes().to_vec(),
        }),
        documents,
        evidence_gaps: BTreeMap::new(),
        unparsed: BTreeSet::new(),
    }
}
