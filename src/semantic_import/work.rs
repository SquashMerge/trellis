//! Work counters for tests: complexity work under `cfg(test)`, publication work under
//! `test-support`.
#[cfg(feature = "test-support")]
use crate::native_evidence as native;
#[cfg(feature = "test-support")]
use std::collections::BTreeSet;

#[cfg(test)]
#[derive(Default, Clone, Copy, Debug)]
pub(super) struct ComplexityWork {
    pub(super) coordinate_batches: usize,
    pub(super) native_candidates: usize,
    pub(super) native_gap_index_records: usize,
    pub(super) native_gap_queries: usize,
    pub(super) native_gap_query_records: usize,
    pub(super) raw_claims: usize,
    pub(super) raw_lookups: usize,
    pub(super) reference_map_lookups: usize,
    pub(super) method_index_rows: usize,
    pub(super) method_lookup_rows: usize,
    pub(super) java_type_parses: usize,
}
#[cfg(test)]
thread_local! { pub(super) static COMPLEXITY_WORK: std::cell::RefCell<ComplexityWork> = std::cell::RefCell::new(ComplexityWork::default()); }
#[cfg(test)]
pub(super) fn complexity_work(update: impl FnOnce(&mut ComplexityWork)) {
    COMPLEXITY_WORK.with(|work| update(&mut work.borrow_mut()));
}

#[cfg(feature = "test-support")]
#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicationWork {
    destination_index_builds: usize,
    full_fallbacks: usize,
    witnessed_rejoins: usize,
    fallback_reasons: Vec<String>,
    pub(super) original_index_builds: usize,
    pub(super) destination_role_parses: usize,
    pub(super) original_role_parses: usize,
    original_native_rows: usize,
    original_native_documents: Vec<native::DocumentKey>,
    original_source_bytes: usize,
    original_source_documents: Vec<native::DocumentKey>,
    original_raw_proof_bytes: usize,
    original_raw_envelope_parses: usize,
    original_raw_decodes: usize,
    original_source_hash_documents: Vec<native::DocumentKey>,
    parsed_role_site_documents: Vec<native::DocumentKey>,
    parsed_type_documents: Vec<native::DocumentKey>,
    current_target_lookups: Vec<native::DocumentKey>,
    current_target_rows: usize,
    pub(super) destination_joins_on_reused_documents: usize,
    destination_document_rows: usize,
    pub(super) destination_revision: String,
    pub(super) reused_documents: BTreeSet<native::DocumentKey>,
    /// Unchanged documents S copies from their published basis (#669 H1).
    copied_documents: BTreeSet<native::DocumentKey>,
    /// `copied_documents` as the current producer's attempt found it: what earlier producers
    /// copied. A FULL fallback restores it, dropping only the abandoned attempt's copies.
    #[serde(skip)]
    copied_before_attempt: BTreeSet<native::DocumentKey>,
    /// Documents the S publisher actually wrote as copies, from each joined stage (#826).
    published_copies: BTreeSet<native::DocumentKey>,
}
#[cfg(feature = "test-support")]
thread_local! { static PUBLICATION_WORK: std::cell::RefCell<PublicationWork> =
std::cell::RefCell::new(PublicationWork::default()); }
#[cfg(feature = "test-support")]
pub(crate) fn reset_publication_work(revision: &str, reused: BTreeSet<native::DocumentKey>) {
    PUBLICATION_WORK.with(|cell| {
        *cell.borrow_mut() = PublicationWork {
            destination_revision: revision.to_owned(),
            reused_documents: reused,
            ..PublicationWork::default()
        }
    });
}
#[cfg(feature = "test-support")]
pub(crate) fn note_local_target_work(builds: usize, documents: usize, rows: usize) {
    count_publication_work(|work| {
        work.destination_index_builds += builds;
        work.destination_document_rows += documents;
        work.current_target_rows += rows;
    });
}
/// One FULL destination inventory document, measured for a non-witnessed producer join.
#[cfg(feature = "test-support")]
pub(crate) fn note_destination_document() {
    count_publication_work(|work| work.destination_document_rows += 1);
}
/// One complete FULL destination index, built per non-witnessed producer join.
#[cfg(feature = "test-support")]
pub(crate) fn note_destination_index_build() {
    count_publication_work(|work| work.destination_index_builds += 1);
}
/// An unchanged document whose original source association the witnessed join reuses.
#[cfg(feature = "test-support")]
pub(crate) fn note_copied_document(document: &native::DocumentKey) {
    count_publication_work(|work| {
        work.copied_documents.insert(document.clone());
    });
}
/// A producer's S attempt starts: copies noted so far belong to earlier producers.
#[cfg(feature = "test-support")]
pub(crate) fn note_producer_attempt() {
    count_publication_work(|work| work.copied_before_attempt = work.copied_documents.clone());
}
/// The S publisher wrote `document` as a copy of its basis tuple.
#[cfg(feature = "test-support")]
pub(crate) fn note_published_copy(document: &native::DocumentKey) {
    count_publication_work(|work| {
        work.published_copies.insert(document.clone());
    });
}
#[cfg(feature = "test-support")]
pub(crate) fn note_reused_document(document: &native::DocumentKey) {
    count_publication_work(|work| {
        work.reused_documents.insert(document.clone());
    });
}
#[cfg(feature = "test-support")]
pub(crate) fn note_local_witness_outcome(fallback: Option<&str>) {
    count_publication_work(|work| match fallback {
        Some(reason) => {
            work.full_fallbacks += 1;
            work.fallback_reasons.push(reason.to_owned());
            // FULL copies nothing: this producer's abandoned witnessed attempt's copies are
            // not published; earlier producers' copies are (#826).
            work.copied_documents = work.copied_before_attempt.clone();
        }
        None => work.witnessed_rejoins += 1,
    });
}
#[cfg(feature = "test-support")]
pub(crate) fn note_local_target_lookup(document: &native::DocumentKey) {
    count_publication_work(|work| work.current_target_lookups.push(document.clone()));
}
#[cfg(feature = "test-support")]
pub(crate) fn note_selected_original_native_row(doc: &native::DocumentKey) {
    count_publication_work(|work| {
        work.original_native_rows += 1;
        work.original_native_documents.push(doc.clone());
    });
}
#[cfg(feature = "test-support")]
pub(crate) fn note_selected_original_source_bytes(doc: &native::DocumentKey, length: usize) {
    count_publication_work(|work| {
        work.original_source_bytes += length;
        work.original_source_documents.push(doc.clone());
    });
}
#[cfg(feature = "test-support")]
pub(crate) fn note_selected_original_raw_proof_bytes(length: usize) {
    count_publication_work(|work| work.original_raw_proof_bytes += length);
}
#[cfg(feature = "test-support")]
pub(crate) fn note_original_raw_envelope_parse() {
    count_publication_work(|work| work.original_raw_envelope_parses += 1);
}
#[cfg(feature = "test-support")]
pub(crate) fn note_original_raw_decode() {
    count_publication_work(|work| work.original_raw_decodes += 1);
}
#[cfg(feature = "test-support")]
pub(crate) fn note_original_source_hash(doc: &native::DocumentKey) {
    count_publication_work(|work| work.original_source_hash_documents.push(doc.clone()));
}
#[cfg(feature = "test-support")]
pub(crate) fn note_parsed_role_site_document(doc: &native::DocumentKey) {
    count_publication_work(|work| work.parsed_role_site_documents.push(doc.clone()));
}
#[cfg(feature = "test-support")]
pub(crate) fn note_parsed_type_document(doc: &native::DocumentKey) {
    count_publication_work(|work| work.parsed_type_documents.push(doc.clone()));
}
#[cfg(feature = "test-support")]
pub(crate) fn publication_work() -> serde_json::Value {
    PUBLICATION_WORK
        .with(|cell| serde_json::to_value(&*cell.borrow()).expect("work counters serialize"))
}
#[cfg(feature = "test-support")]
pub(crate) fn publication_work_checkpoint() -> PublicationWork {
    PUBLICATION_WORK.with(|cell| cell.borrow().clone())
}
#[cfg(feature = "test-support")]
pub(crate) fn restore_publication_work(checkpoint: PublicationWork) {
    PUBLICATION_WORK.with(|cell| *cell.borrow_mut() = checkpoint);
}
#[cfg(feature = "test-support")]
pub(super) fn count_publication_work(update: impl FnOnce(&mut PublicationWork)) {
    PUBLICATION_WORK.with(|cell| update(&mut cell.borrow_mut()));
}
