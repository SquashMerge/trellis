use super::BatchLimits;

#[test]
fn production_limits_match_approved_policy() {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    let BatchLimits {
        source_documents,
        descriptor_bytes,
        page_bytes,
        page_rows,
        pages,
        total_page_bytes,
        artifacts,
        artifact_bytes,
        total_artifact_bytes,
        candidates,
        wire_fields,
        unit_expansion_bytes,
        document_occurrences,
        document_symbols,
        external_symbols,
        signature_occurrences,
        message_depth,
        relationships_per_artifact,
        staged_rows,
        output_bytes,
        lookup_steps,
        staging_bytes,
        disk_growth_bytes,
        document_cache_bytes,
        cache_bytes,
    } = BatchLimits::PRODUCTION;
    assert_eq!(source_documents, 100_000);
    assert_eq!(descriptor_bytes, MIB);
    assert_eq!(page_bytes, MIB);
    assert_eq!(page_rows, 1_000);
    assert_eq!(pages, 1_024);
    assert_eq!(total_page_bytes, 64 * MIB);
    assert_eq!(artifacts, 64);
    assert_eq!(artifact_bytes, 64 * MIB);
    assert_eq!(total_artifact_bytes, 512 * MIB);
    assert_eq!(candidates, 4_000_000);
    assert_eq!(wire_fields, 64_000_000);
    assert_eq!(unit_expansion_bytes, 128 * MIB);
    assert_eq!(document_occurrences, 350_000);
    assert_eq!(document_symbols, 10_000);
    assert_eq!(external_symbols, 10_000);
    assert_eq!(signature_occurrences, 10_000);
    assert_eq!(message_depth, 16);
    assert_eq!(relationships_per_artifact, 20_000);
    assert_eq!(staged_rows, 8_000_000);
    assert_eq!(output_bytes, 8 * GIB);
    assert_eq!(lookup_steps, 100_000_000);
    assert_eq!(staging_bytes, 8 * GIB);
    assert_eq!(disk_growth_bytes, 32 * GIB);
    assert_eq!(document_cache_bytes, 8 * MIB);
    assert_eq!(cache_bytes, 64 * MIB);
}

#[cfg(feature = "test-support")]
#[test]
fn test_limits_override_named_fields_and_reject_unknown_keys() {
    let parsed: BatchLimits = serde_json::from_str(r#"{"page_rows": 3}"#).unwrap();
    assert_eq!(parsed.page_rows, 3);
    assert_eq!(parsed.pages, BatchLimits::PRODUCTION.pages);
    assert!(serde_json::from_str::<BatchLimits>(r#"{"nope": 1}"#).is_err());
}

#[test]
fn cache_budget_fits_sixty_four_mebibytes() {
    use super::limits::*;
    // The native reader's live PRAGMA is added by the integration test of the same name.
    let kib = |bytes: u64| bytes / 1024;
    let total = STAGE_CACHE_KIB
        + STAGE_ATTACH_CACHE_KIB
        + kib(BatchLimits::PRODUCTION.document_cache_bytes)
        + kib(IO_BUFFER_BYTES * IO_BUFFERS)
        + NATIVE_READER_CACHE_KIB
        + BATCH_WRITER_CACHE_KIB;
    assert!(
        total <= kib(BatchLimits::PRODUCTION.cache_bytes),
        "{total} KiB"
    );
}
