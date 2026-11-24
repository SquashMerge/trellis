//! A catalog's isomorphism digest (#1314): typed, length-delimited values, with each byte
//! position replaced by its rank among the positions the catalog holds in its document.
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
}
