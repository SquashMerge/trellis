//! The metrics memo and the score records it holds (owner ruling 2026-11-17, D4):
//! `MetricsMemo` with its key and the source-only scores it keeps, and the `metrics:v1`
//! score record (`Score`, `Halstead`, `Exits`, `Unavailable`). Plain data and a bounded
//! FIFO: no parser and no I/O. `Store` holds one memo for each store family; the scoring
//! stays in `crate::metrics`, which re-exports these items.
use crate::syntax_bounds::SyntaxGap;
use anyhow::Result;
use serde::Serialize;
use std::collections::VecDeque;

pub const VERSION: &str = "metrics:v1";

/// The source-only scores of one document and how its syntax exceeded the bounds, if it did.
#[derive(Clone)]
pub(crate) struct SourceScores {
    pub(crate) scores: Vec<Score>,
    pub(crate) gap: Option<SyntaxGap>,
}

/// Version, language, content hash and whether the TypeScript grammar is `tsx`: identical
/// bytes parse differently under the two TypeScript grammars.
pub(crate) type MemoKey = (String, String, String, bool);
pub(crate) fn memo_key(source: &SourceInput<'_>) -> MemoKey {
    (
        VERSION.to_owned(),
        source.language.to_owned(),
        source.content_hash.to_owned(),
        source.language == "typescript" && source.path.ends_with(".tsx"),
    )
}
/// The selected pinned source; its language, grammar-selecting path, hash and bytes enter
/// the memo key.
pub struct SourceInput<'a> {
    pub language: &'a str,
    pub path: &'a str,
    pub content_hash: &'a str,
    pub text: &'a str,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Score {
    pub loc: usize,
    pub sloc: usize,
    pub comment_lines: usize,
    pub cyclomatic: usize,
    pub cognitive: usize,
    pub max_nesting: usize,
    pub halstead: Halstead,
    pub maintainability_index: f64,
    pub fan_out: usize,
    pub fan_in: Unavailable,
    pub parameters: usize,
    pub exits: Exits,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Unavailable {
    pub value: Option<f64>,
    pub unavailable: &'static str,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Exits {
    pub returns: usize,
    pub throws: usize,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Halstead {
    pub n1: usize,
    pub n2: usize,
    #[serde(rename = "N1")]
    pub total_operators: usize,
    #[serde(rename = "N2")]
    pub total_operands: usize,
    pub vocabulary: usize,
    pub length: usize,
    pub volume: f64,
    pub difficulty: f64,
    pub effort: f64,
    pub estimated_bugs: f64,
}
/// FIFO memo contains only source-derived scores and gaps, never native identity or calls.
type MemoEntry = (MemoKey, SourceScores, usize);
pub struct MetricsMemo {
    entries: VecDeque<MemoEntry>,
    bytes: usize,
    limit: usize,
}
impl MetricsMemo {
    pub fn new(limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limit,
        }
    }
    fn insert(&mut self, key: MemoKey, value: SourceScores) -> Result<()> {
        let size = key
            .0
            .capacity()
            .saturating_add(key.1.capacity())
            .saturating_add(key.2.capacity())
            .saturating_add(
                value
                    .scores
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Score>()),
            )
            .saturating_add(std::mem::size_of::<MemoEntry>());
        if size <= self.limit.min(64 * 1024 * 1024) {
            while self.bytes.saturating_add(size) > self.limit.min(64 * 1024 * 1024) {
                if let Some((_, _, n)) = self.entries.pop_front() {
                    self.bytes -= n;
                } else {
                    break;
                }
            }
            self.entries.push_back((key, value, size));
            self.bytes += size;
        }
        Ok(())
    }
    /// Source-only scores already recorded for this source, if any.
    pub(crate) fn lookup(&self, source: &SourceInput<'_>) -> Option<SourceScores> {
        let key = memo_key(source);
        self.entries
            .iter()
            .find(|(k, _, _)| *k == key)
            .map(|(_, scores, _)| scores.clone())
    }
    /// Record a document's newly computed scores, unless an equal source was
    /// recorded since they were computed.
    pub(crate) fn commit(&mut self, computed: Option<(MemoKey, SourceScores)>) -> Result<()> {
        match computed {
            Some((key, scores)) if !self.entries.iter().any(|(k, _, _)| *k == key) => {
                self.insert(key, scores)
            }
            _ => Ok(()),
        }
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
