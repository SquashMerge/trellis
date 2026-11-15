//! Test-support counters of the retained native reads storage performs, per thread. Storage
//! notes each read; tests read and reset them through `crate::semantic_batch::testing`.
use std::cell::Cell;

thread_local! {
    static NATIVE_DOCUMENT_LOADS: Cell<u64> = const { Cell::new(0) };
}

pub fn reset_native_document_loads() {
    NATIVE_DOCUMENT_LOADS.with(|loads| loads.set(0));
}

pub fn native_document_loads() -> u64 {
    NATIVE_DOCUMENT_LOADS.with(Cell::get)
}

pub(crate) fn note_native_document_load() {
    NATIVE_DOCUMENT_LOADS.with(|loads| loads.set(loads.get() + 1));
}

thread_local! {
    static WHOLE_SNAPSHOT_READS: Cell<u64> = const { Cell::new(0) };
}

/// Called by `retained_native_snapshot_from_db`: a whole-revision read happened.
pub fn note_whole_snapshot_read() {
    WHOLE_SNAPSHOT_READS.with(|reads| reads.set(reads.get() + 1));
}

/// Whole-revision snapshot reads on this thread since `stage_batch` last started.
pub fn whole_snapshot_reads() -> u64 {
    WHOLE_SNAPSHOT_READS.with(Cell::get)
}

pub(crate) fn reset_whole_snapshot_reads() {
    WHOLE_SNAPSHOT_READS.with(|reads| reads.set(0));
}
