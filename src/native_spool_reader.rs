//! Storage's view of the full-document spool (`store::spool_reader`).
//!
//! Each method is the inherent method of the same name on `native_evidence::FullDocumentSpool`;
//! `read_raw` pairs its two row reads, and the spool's header is the `decoder`.
//! This file is not an extractor input (`build/extractor_inputs.rs`): it only delegates, and the
//! spool's decoding and checks stay in `native_evidence.rs`, which is.
use crate::{
    model::{Diagnostic, FileExtraction, Graph, IndexStats},
    native_evidence::FullDocumentSpool,
    native_records::Artifact,
    store::spool_reader::{RawSpoolDocument, SpoolDecoder, SpoolReader},
};
use anyhow::Result;

impl SpoolReader for FullDocumentSpool {
    fn header(&self) -> &Artifact {
        FullDocumentSpool::header(self)
    }
    fn len(&self) -> usize {
        FullDocumentSpool::len(self)
    }
    fn stats(&self) -> &IndexStats {
        FullDocumentSpool::stats(self)
    }
    fn diagnostics(&self) -> &[Diagnostic] {
        FullDocumentSpool::diagnostics(self)
    }
    fn read(&self, index: usize) -> Result<(Artifact, Graph)> {
        FullDocumentSpool::read(self, index)
    }
    fn read_extraction(&self, index: usize) -> Result<FileExtraction> {
        FullDocumentSpool::read_extraction(self, index)
    }
    fn read_raw(&self, index: usize) -> RawSpoolDocument {
        RawSpoolDocument {
            document: FullDocumentSpool::read_document_rows(self, index),
            extraction: FullDocumentSpool::read_extraction_row(self, index),
        }
    }
    fn decoder(&self) -> &dyn SpoolDecoder {
        FullDocumentSpool::header(self)
    }
    #[cfg(test)]
    fn corrupt_graph_json_for_tests(&self, index: usize) {
        FullDocumentSpool::corrupt_graph_json_for_tests(self, index)
    }
    #[cfg(test)]
    fn corrupt_extraction_json_for_tests(&self, index: usize) {
        FullDocumentSpool::corrupt_extraction_json_for_tests(self, index)
    }
    #[cfg(test)]
    fn inject_sqlite_corrupt_read_for_tests(&self) {
        FullDocumentSpool::inject_sqlite_corrupt_read_for_tests(self)
    }
}

/// The spool header decodes its documents' rows: decoding needs only the header.
impl SpoolDecoder for Artifact {
    fn decode(&self, index: usize, facts: &[u8], graph: &[u8]) -> Result<(Artifact, Graph)> {
        crate::native_evidence::decode_spool_document(self, index, facts, graph)
    }
    fn decode_extraction(&self, index: usize, payload: &[u8]) -> Result<FileExtraction> {
        crate::native_evidence::decode_spool_extraction(self, index, payload)
    }
}
