use serde::{Deserialize, Deserializer, Serialize};

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
use std::sync::{Arc, atomic::AtomicBool};

pub const SCHEMA_VERSION: u32 = 1;
pub type CancelFlag = Arc<AtomicBool>;

/// Identity of one published disposable index snapshot. Numeric revisions are local to a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "UncheckedIndexPin"
)]
pub struct IndexPin {
    #[serde(with = "uuid_text")]
    pub index_generation: uuid::Uuid,
    pub index_revision: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UncheckedIndexPin {
    index_generation: String,
    index_revision: u64,
}
impl TryFrom<UncheckedIndexPin> for IndexPin {
    type Error = String;
    fn try_from(value: UncheckedIndexPin) -> Result<Self, Self::Error> {
        let generation =
            uuid::Uuid::parse_str(&value.index_generation).map_err(|e| e.to_string())?;
        if generation.is_nil()
            || generation.get_version_num() != 4
            || generation.to_string() != value.index_generation
        {
            return Err("indexGeneration must be a canonical non-nil UUIDv4".into());
        }
        if value.index_revision > 9_007_199_254_740_991 {
            return Err("indexRevision exceeds the safe integer range".into());
        }
        Ok(Self {
            index_generation: generation,
            index_revision: value.index_revision,
        })
    }
}
mod uuid_text {
    use serde::Serializer;
    pub fn serialize<S: Serializer>(value: &uuid::Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceFile {
    pub path: String,
    pub hash: String,
    pub language: String,
    pub text: String,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SymbolKind {
    Module,
    Function,
    Method,
    Class,
    /// Measured declarations that are not executable graph entries. Keep their #22 IDs,
    /// parent chains and source ranges without misrepresenting them as functions.
    Field,
    Variable,
    Parameter,
    TypeParameter,
    Alias,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SemanticState {
    Fresh,
    Stale,
    #[default]
    Unavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Provenance {
    pub source: String,
    pub semantic: SemanticState,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Symbol {
    pub id: String,
    pub name: String,
    /// Untrusted SCIP display metadata, never a node ID or semantic relation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_label: Option<String>,
    pub kind: SymbolKind,
    pub path: String,
    pub range: SourceRange,
    pub parent: Option<String>,
    pub accessor: bool,
    pub provenance: Provenance,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Resolution {
    Internal,
    External,
    Unresolved,
    Ambiguous,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallSite {
    pub id: String,
    pub caller: String,
    /// Source-witnessed spelling, or explicit null when no expression is provable.
    #[serde(deserialize_with = "required_nullable")]
    pub callee_text: Option<String>,
    pub path: String,
    pub range: SourceRange,
    /// Exact measured callee token when provable; independent from spelling nullability.
    #[serde(deserialize_with = "required_nullable")]
    pub callee_range: Option<SourceRange>,
    pub ordinal: usize,
    pub regions: Vec<String>,
    pub provenance: Provenance,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ControlRegion {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub parent: Option<String>,
    pub owner: String,
    pub path: String,
    pub range: SourceRange,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub path: Option<String>,
    pub code: String,
    pub message: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IndexStats {
    pub files: usize,
    pub symbols: usize,
    pub calls: usize,
    pub regions: usize,
    pub internal: usize,
    pub external: usize,
    pub unresolved: usize,
    pub ambiguous: usize,
    pub parse_error_files: usize,
    pub semantic_state: SemanticState,
    pub changed_files: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Graph {
    pub schema_version: u32,
    pub files: Vec<SourceFile>,
    pub nodes: Vec<Symbol>,
    pub calls: Vec<CallSite>,
    pub regions: Vec<ControlRegion>,
    pub diagnostics: Vec<Diagnostic>,
    pub stats: IndexStats,
}
impl Default for Graph {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            files: vec![],
            nodes: vec![],
            calls: vec![],
            regions: vec![],
            diagnostics: vec![],
            stats: IndexStats::default(),
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IndexProgress {
    pub phase: String,
    pub completed: usize,
    pub total: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    pub workspace_root: String,
    #[serde(default)]
    pub catching_up: bool,
    pub revision: IndexPin,
    pub indexed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_format: Option<String>,
    pub stats: IndexStats,
    pub diagnostics: Vec<Diagnostic>,
    /// The daemon's watcher-health witness for this checkout, on CLI status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watcher: Option<crate::watch::WatcherWitness>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ViewQuery {
    pub seed: String,
    #[serde(default = "default_depth")]
    pub depth: usize,
    #[serde(default = "default_nodes")]
    pub max_nodes: usize,
    #[serde(default = "default_calls")]
    pub max_calls: usize,
    #[serde(default)]
    pub include_callbacks: bool,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
}
fn default_depth() -> usize {
    1
}
fn default_nodes() -> usize {
    40
}
fn default_calls() -> usize {
    200
}
impl ViewQuery {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.seed.is_empty() && self.seed.len() <= 8192,
            "seed must contain 1..8192 bytes"
        );
        anyhow::ensure!(self.depth <= 5, "depth must be at most 5");
        anyhow::ensure!(
            (1..=150).contains(&self.max_nodes),
            "maxNodes must be 1..150"
        );
        anyhow::ensure!(
            (1..=500).contains(&self.max_calls),
            "maxCalls must be 1..500"
        );
        anyhow::ensure!(
            self.exclude_paths.len() <= 100 && self.exclude_paths.iter().all(|x| x.len() <= 1024),
            "too many or oversized excluded path prefixes"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ViewResult {
    pub revision: IndexPin,
    pub query: ViewQuery,
    pub nodes: Vec<Symbol>,
    pub calls: Vec<CallSite>,
    pub regions: Vec<ControlRegion>,
    pub truncated: bool,
    pub omitted_nodes: usize,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableAnchor {
    pub syntax_id: String,
    pub document: crate::native_evidence::DocumentKey,
    pub captured_revision_id: String,
    pub header_hash: String,
    pub sibling_group_hash: String,
    pub sibling_count: usize,
    pub identical_header_count: usize,
}
impl DurableAnchor {
    pub fn validate(&self) -> anyhow::Result<()> {
        fn hash(value: &str) -> bool {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }
        anyhow::ensure!(
            self.syntax_id.len() == 39
                && self.syntax_id.starts_with("sid:v1:")
                && self.syntax_id[7..]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid durable anchor syntaxId"
        );
        anyhow::ensure!(
            !self.document.source_set_id.is_empty() && !self.captured_revision_id.is_empty(),
            "invalid durable anchor document/revision"
        );
        anyhow::ensure!(
            matches!(
                self.document.language.as_str(),
                "java" | "rust" | "python" | "javascript"
            ),
            "invalid durable anchor document language"
        );
        let path = &self.document.path;
        anyhow::ensure!(
            !path.is_empty()
                && !path.starts_with('/')
                && !path.contains('\\')
                && !path.contains('\0')
                && path.split('/').all(|part| !matches!(part, "" | "." | "..")),
            "invalid durable anchor document path"
        );
        anyhow::ensure!(
            hash(&self.header_hash) && hash(&self.sibling_group_hash),
            "invalid durable anchor hash"
        );
        anyhow::ensure!(
            self.sibling_count > 0
                && self.identical_header_count > 0
                && self.identical_header_count <= self.sibling_count,
            "invalid durable anchor counts"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AnchorStatus {
    Attached,
    Orphaned,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AnchorReason {
    None,
    Missing,
    HeaderMismatch,
    GroupChanged,
    UnprovenContinuity,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnchorResult {
    pub status: AnchorStatus,
    pub target_id: Option<String>,
    pub reason: AnchorReason,
}
impl AnchorResult {
    pub fn attached(id: String) -> Self {
        Self {
            status: AnchorStatus::Attached,
            target_id: Some(id),
            reason: AnchorReason::None,
        }
    }
    pub fn orphaned(reason: AnchorReason) -> Self {
        Self {
            status: AnchorStatus::Orphaned,
            target_id: None,
            reason,
        }
    }
}
