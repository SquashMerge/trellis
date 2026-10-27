//! Per-call search inputs for the shared syntax-evidence wire projection.

pub use crate::search_model::SearchIndexStatus;

/// Ordered store hits and arm membership from the same admitted snapshot.
pub struct SearchResult {
    pub symbols: crate::store::mcp_read::McpFindWithLines,
    pub matched_by: Vec<Vec<String>>,
    pub search_index: SearchIndexStatus,
}

/// Search has a separate public default from the unchanged find-symbols tool.
pub const DEFAULT_LIMIT: usize = 10;
