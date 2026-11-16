//! The Files directory page. The directory handle and its entries are in `source_dir`.
use crate::model::IndexPin;
use serde::Serialize;

pub use crate::source_dir::{Entry, SCAN_LIMIT, SourceDir, valid_path};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub root: String,
    pub indexed_workspace: String,
    pub path: String,
    pub revision: IndexPin,
    pub items: Vec<Entry>,
    pub next_offset: Option<usize>,
    pub truncated: bool,
}
