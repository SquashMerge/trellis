//! Production-size requested-allocation acceptance for #375 owner comment 6452070923.
//! Run as a standalone process; this measures allocation payload, never process RSS.
//! The companion daemon unit test proves admission against its private default cap.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    fmt::Write as _,
    mem::size_of,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Result, ensure};
use serde_json::json;
use trellis::search_rank::{SearchCandidate, SearchCandidateKey, SearchMatrix};

struct RequestedAllocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn allocated(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// Every counter operation is allocation-free. System retains allocator ownership;
// these counters observe successful Rust allocation requests, not allocator metadata.
unsafe impl GlobalAlloc for RequestedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forward the caller's valid allocation layout unchanged.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forward the caller's valid allocation layout unchanged.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: pointer and layout come from the matching allocation request.
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forward the live pointer, original layout and requested size.
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if !replacement.is_null() {
            if new_size >= layout.size() {
                allocated(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: RequestedAllocator = RequestedAllocator;

// These are explicit acceptance-fixture constants, not production tuning changes.
// The daemon test uses the real private cap, dimension and fill-batch constants.
const ROWS: usize = 75_614 * 10;
const DIMENSIONS: usize = 1024;
const CAP_BYTES: usize = 2 * 1024 * 1024 * 1024;
const FILL_BATCH: usize = 16;
const FILL_SCRATCH_BYTES: usize = FILL_BATCH * (3 * DIMENSIONS + 256);
const CARDS_PER_VERSION: usize = 10;

fn candidate(row: usize) -> SearchCandidate {
    let mut syntax_id = String::with_capacity(39);
    write!(syntax_id, "sid:v1:{row:032x}").unwrap();
    let mut card_hash = String::with_capacity(64);
    write!(card_hash, "{row:064x}").unwrap();
    SearchCandidate {
        key: SearchCandidateKey {
            version_id: (row / CARDS_PER_VERSION) as i64 + 1,
            syntax_id,
        },
        card_hash,
    }
}

fn within_ten_percent(estimate: usize, measured: usize) -> bool {
    estimate.abs_diff(measured) as u128 * 10 <= measured as u128
}

fn main() -> Result<()> {
    let estimated_resident =
        SearchMatrix::estimated_new_bytes_for_counts(ROWS, ROWS * 39, ROWS * 64);
    let estimated_build =
        SearchMatrix::estimated_new_build_bytes_for_counts(ROWS, ROWS * 39, ROWS * 64);
    // Exact store input admission shape, including simultaneous Vec/Arc row arrays.
    let candidate_charge =
        ROWS * (2 * size_of::<SearchCandidate>() + 39 + 64) + 2 * size_of::<usize>();
    let cold_reservation = candidate_charge + estimated_build + FILL_SCRATCH_BYTES;
    ensure!(
        cold_reservation <= CAP_BYTES,
        "production-size cold reservation exceeds cap"
    );

    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let mut input = Vec::with_capacity(ROWS);
    for row in 0..ROWS {
        input.push(candidate(row));
    }
    for card in [&input[0], &input[ROWS - 1]] {
        ensure!(
            card.key.syntax_id.len() == 39 && card.card_hash.len() == 64,
            "fixture ID width"
        );
    }
    let candidates: Arc<[SearchCandidate]> = input.into();
    let mut matrix = SearchMatrix::new(candidates, &mut || Ok(()))?;
    let measured_missing = LIVE.load(Ordering::Relaxed) - baseline;
    let cold_peak = PEAK.load(Ordering::Relaxed) - baseline;
    let storage_missing = matrix.storage_bytes();
    ensure!(
        matrix.active_count() == ROWS && matrix.slot_count() == ROWS,
        "matrix row count"
    );
    ensure!(matrix.embedded_count() == 0, "new matrix starts missing");
    ensure!(
        within_ten_percent(estimated_resident, measured_missing),
        "missing resident estimate differs by over 10%"
    );
    ensure!(
        within_ten_percent(storage_missing, measured_missing),
        "missing storage charge differs by over 10%"
    );
    ensure!(
        cold_peak <= cold_reservation,
        "measured cold requested-byte peak exceeds reservation"
    );

    // One stack vector is reused. No all-row vector staging or model inference.
    let mut values = [1_i8; DIMENSIONS];
    values[1] = -2;
    for ordinal in 0..ROWS {
        ensure!(
            matrix.insert(ordinal, &values)?,
            "valid fixture vector was rejected"
        );
    }
    let measured_present = LIVE.load(Ordering::Relaxed) - baseline;
    let storage_present = matrix.storage_bytes();
    let embedded = matrix.embedded_count();
    ensure!(embedded == ROWS as u64, "full vector coverage");
    ensure!(
        within_ten_percent(estimated_resident, measured_present),
        "present resident estimate differs by over 10%"
    );
    ensure!(
        within_ten_percent(storage_present, measured_present),
        "present storage charge differs by over 10%"
    );
    ensure!(
        measured_present < CAP_BYTES && measured_missing < CAP_BYTES,
        "resident requested bytes exceed cap"
    );
    drop(matrix);
    let after_drop = LIVE.load(Ordering::Relaxed);
    ensure!(
        after_drop == baseline,
        "matrix/input requested allocations were retained after drop"
    );

    // Capture measurements before JSON serialization or stdout initialization.
    println!(
        "{}",
        json!({
            "acceptance": "issuecomment-6452070923",
            "measurement": "System global allocator successful live requested payload bytes; not RSS",
            "peak_scope": "Rust allocation request boundaries; excludes System realloc implementation internals",
            "rows": ROWS,
            "dimensions": DIMENSIONS,
            "vector_element_bytes": size_of::<i8>(),
            "syntax_id_bytes": 39,
            "card_hash_bytes": 64,
            "cards_per_version": CARDS_PER_VERSION,
            "cap_bytes": CAP_BYTES,
            "cold_reservation_fits_cap": true,
            "admission_proof": "companion daemon test exercises private default-budget admission",
            "estimated_resident_bytes": estimated_resident,
            "estimated_build_bytes": estimated_build,
            "candidate_input_charge_bytes": candidate_charge,
            "fill_scratch_allowance_bytes": FILL_SCRATCH_BYTES,
            "cold_reserved_bytes": cold_reservation,
            "measured_cold_peak_requested_bytes": cold_peak,
            "measured_missing_resident_requested_bytes": measured_missing,
            "measured_present_resident_requested_bytes": measured_present,
            "reported_missing_storage_bytes": storage_missing,
            "reported_present_storage_bytes": storage_present,
            "missing_relative_error": estimated_resident.abs_diff(measured_missing) as f64 / measured_missing as f64,
            "present_relative_error": estimated_resident.abs_diff(measured_present) as f64 / measured_present as f64,
            "embedded": embedded,
            "baseline_requested_bytes": baseline,
            "after_drop_requested_bytes": after_drop,
            "requested_bytes_released": true
        })
    );
    Ok(())
}
