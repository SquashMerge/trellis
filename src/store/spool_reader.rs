//! The full-document spool as publication reads it.
//!
//! Storage owns this trait and `native_evidence::FullDocumentSpool`, in indexing, implements it by
//! delegation (`src/native_spool_reader.rs`). Publication takes the trait, so `store.rs` does not
//! name the indexing type. Every method is the spool's own method of the same name.
use crate::{
    model::{Diagnostic, FileExtraction, Graph, IndexStats},
    native_records::Artifact,
};
use anyhow::Result;

/// One extraction's private, repeatable document spool, as publication reads it.
pub(crate) trait SpoolReader {
    /// The captured revision header: the full document list, no per-document facts.
    fn header(&self) -> &Artifact;
    /// How many documents the header lists.
    fn len(&self) -> usize;
    /// The aggregate graph counters of every spooled document.
    fn stats(&self) -> &IndexStats;
    /// The aggregate diagnostics of every spooled document.
    fn diagnostics(&self) -> &[Diagnostic];
    /// One document's facts and graph, by its index in the header.
    fn read(&self, index: usize) -> Result<(Artifact, Graph)>;
    /// One document's class extraction, by its index in the header.
    fn read_extraction(&self, index: usize) -> Result<FileExtraction>;

    // Fault injection for the publisher's test hook, which receives the spool as this trait.

    #[cfg(test)]
    fn corrupt_graph_json_for_tests(&self, index: usize);
    #[cfg(test)]
    fn corrupt_extraction_json_for_tests(&self, index: usize);
    #[cfg(test)]
    fn inject_sqlite_corrupt_read_for_tests(&self);
}
