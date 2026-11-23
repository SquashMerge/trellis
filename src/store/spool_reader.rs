//! The full-document spool as publication reads it.
//!
//! Storage owns this trait and `native_evidence::FullDocumentSpool`, in indexing, implements it by
//! delegation (`src/native_spool_reader.rs`). Publication takes the trait, so `store.rs` does not
//! name the indexing type. Every `SpoolReader` method but `read_raw` and `decoder` is the spool's
//! own method of the same name.
use crate::{
    model::{Diagnostic, FileExtraction, Graph, IndexStats},
    native_records::Artifact,
};
use anyhow::Result;

/// One document's undecoded spool rows (`SpoolReader::read_raw`). A read failure is
/// kept in place, so it is reported in the same order as `read` and `read_extraction`.
pub(crate) struct RawSpoolDocument {
    /// The facts and graph rows, or `read`'s read failure.
    pub(crate) document: Result<(Vec<u8>, Vec<u8>)>,
    /// The class extraction row, or `read_extraction`'s read failure. Publication
    /// consults it only for a class-catalog language, as it calls `read_extraction`.
    pub(crate) extraction: Result<Vec<u8>>,
}

/// Decodes and checks `read_raw`'s rows exactly as `read` and `read_extraction` do.
pub(crate) trait SpoolDecoder: Sync {
    /// `read`'s decoding and checks of one document's facts and graph rows.
    fn decode(&self, index: usize, facts: &[u8], graph: &[u8]) -> Result<(Artifact, Graph)>;
    /// `read_extraction`'s decoding and check of one document's extraction row.
    fn decode_extraction(&self, index: usize, payload: &[u8]) -> Result<FileExtraction>;
}

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
    /// `read` and, for a class-catalog language, `read_extraction`, split in two: the
    /// spool's own SQLite reads, here, and their decoding and checks, which the
    /// returned rows do on any thread with `decoder`. Each read keeps its own result.
    fn read_raw(&self, index: usize) -> RawSpoolDocument;
    /// The decoder of `read_raw`'s rows. It holds no SQLite handle, so it is shared
    /// with worker threads.
    fn decoder(&self) -> &dyn SpoolDecoder;

    // Fault injection for the publisher's test hook, which receives the spool as this trait.

    #[cfg(test)]
    fn corrupt_graph_json_for_tests(&self, index: usize);
    #[cfg(test)]
    fn corrupt_extraction_json_for_tests(&self, index: usize);
    #[cfg(test)]
    fn inject_sqlite_corrupt_read_for_tests(&self);
}
