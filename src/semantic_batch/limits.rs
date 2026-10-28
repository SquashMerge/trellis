//! Named resource policy for streamed batch import. Every violation is a typed
//! `ScipCaptureError::LimitExceeded("<name>")`, raised before the quantity is expanded.
use serde::Deserialize;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Size of every streaming I/O buffer: artifact copy, decoder reader, blob writer and the
/// source-inventory reader.
pub const IO_BUFFER_BYTES: u64 = MIB;
/// Number of such buffers that count against `cache_bytes`.
pub const IO_BUFFERS: u64 = 4;
/// `PRAGMA cache_size` of the stage connection, in KiB.
pub const STAGE_CACHE_KIB: u64 = 8 * 1024;
/// `PRAGMA stage.cache_size` of the schema the publishing writer attaches, in KiB.
pub const STAGE_ATTACH_CACHE_KIB: u64 = 2 * 1024;
/// `PRAGMA cache_size` of the native reader used by batch staging, in KiB.
pub const NATIVE_READER_CACHE_KIB: u64 = 16 * 1024;
/// `PRAGMA cache_size` the batch publication writer applies, in KiB.
pub const BATCH_WRITER_CACHE_KIB: u64 = 16 * 1024;
/// The stage directory is measured after this many staged rows.
pub const STAGE_CHECK_ROWS: u64 = 10_000;
/// The stage directory is measured after this many artifact bytes are copied.
pub const STAGE_CHECK_COPIED_BYTES: u64 = 16 * MIB;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BatchLimits {
    pub source_documents: u64,
    pub descriptor_bytes: u64,
    pub page_bytes: u64,
    pub page_rows: u64,
    pub pages: u64,
    pub total_page_bytes: u64,
    pub artifacts: u64,
    pub artifact_bytes: u64,
    pub total_artifact_bytes: u64,
    pub candidates: u64,
    pub wire_fields: u64,
    pub unit_expansion_bytes: u64,
    pub document_occurrences: u64,
    pub document_symbols: u64,
    pub external_symbols: u64,
    pub signature_occurrences: u64,
    pub message_depth: u64,
    pub relationships_per_artifact: u64,
    pub staged_rows: u64,
    pub output_bytes: u64,
    pub lookup_steps: u64,
    pub staging_bytes: u64,
    pub disk_growth_bytes: u64,
    pub document_cache_bytes: u64,
    pub cache_bytes: u64,
}

impl BatchLimits {
    pub const PRODUCTION: BatchLimits = BatchLimits {
        source_documents: 100_000,
        descriptor_bytes: MIB,
        page_bytes: MIB,
        page_rows: 1_000,
        pages: 1_024,
        total_page_bytes: 64 * MIB,
        artifacts: 64,
        artifact_bytes: 64 * MIB,
        total_artifact_bytes: 512 * MIB,
        candidates: 4_000_000,
        wire_fields: 64_000_000,
        unit_expansion_bytes: 128 * MIB,
        document_occurrences: 10_000,
        document_symbols: 10_000,
        external_symbols: 10_000,
        signature_occurrences: 10_000,
        message_depth: 16,
        relationships_per_artifact: 20_000,
        staged_rows: 8_000_000,
        output_bytes: 8 * GIB,
        lookup_steps: 100_000_000,
        staging_bytes: 8 * GIB,
        disk_growth_bytes: 32 * GIB,
        document_cache_bytes: 8 * MIB,
        cache_bytes: 64 * MIB,
    };

    /// Production limits with the `TRELLIS_TEST_SEMANTIC_LIMITS` JSON object applied. The
    /// caller reads this once per operation; an unparsable object or unknown key is an error.
    #[cfg(feature = "test-support")]
    pub fn effective() -> Result<BatchLimits, String> {
        match std::env::var("TRELLIS_TEST_SEMANTIC_LIMITS") {
            Ok(text) => serde_json::from_str(&text).map_err(|_| "test limits".to_string()),
            Err(_) => Ok(BatchLimits::PRODUCTION),
        }
    }
}

impl Default for BatchLimits {
    fn default() -> Self {
        BatchLimits::PRODUCTION
    }
}
