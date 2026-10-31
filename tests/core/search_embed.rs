#[cfg(feature = "test-support")]
use std::sync::atomic::AtomicBool;

use serde::Deserialize;
use trellis::search_embed::{Backend, EmbedError, Embedder, quantize};

#[derive(Deserialize)]
struct Fixture {
    rows: Vec<Row>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    id: String,
    #[cfg(feature = "test-support")]
    kind: String,
    #[cfg(feature = "test-support")]
    text: String,
    reference: Vec<f32>,
    expected_quantized1024: Vec<i8>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!(
        "../fixtures/semantic-search/voyage-reference.json"
    ))
    .unwrap()
}

#[test]
fn saved_voyage_vectors_match_independent_float64_postprocessing() {
    for row in fixture().rows {
        assert_eq!(row.reference.len(), 2048, "{}", row.id);
        assert_eq!(row.expected_quantized1024.len(), 1024, "{}", row.id);
        // Saved low-precision normalized outputs need not have exact f32 unit norm.
        assert!(
            row.reference.iter().all(|value| value.is_finite()),
            "{}",
            row.id
        );
        assert_eq!(
            quantize(&row.reference).unwrap().values,
            row.expected_quantized1024,
            "{}",
            row.id
        );
    }
}

#[test]
fn missing_or_different_model_is_typed_unavailable() {
    let directory = tempfile::tempdir().unwrap();
    assert!(matches!(
        Embedder::load(directory.path(), Backend::Cpu),
        Err(EmbedError::ModelUnavailable(_))
    ));
    std::fs::create_dir(directory.path().join("1_Pooling")).unwrap();
    std::fs::write(directory.path().join("1_Pooling/config.json"), b"{}").unwrap();
    assert!(
        matches!(Embedder::load(directory.path(), Backend::Cpu), Err(EmbedError::ModelUnavailable(message)) if message.contains("SHA-256 mismatch"))
    );
}

#[cfg(not(feature = "metal"))]
#[test]
fn metal_without_feature_does_not_fall_back_to_cpu() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        matches!(Embedder::load(directory.path(), Backend::Metal), Err(EmbedError::ModelUnavailable(message)) if message.contains("Metal support is not enabled"))
    );
}

/// External weights are required; this is never credited as the full-corpus gate.
#[cfg(feature = "test-support")]
#[test]
#[ignore = "requires explicitly provisioned pinned local model; set TRELLIS_PARITY_MODEL_DIR and TRELLIS_PARITY_BACKEND"]
fn pinned_real_model_matches_saved_fixture() {
    use trellis::search_embed::{EmbeddingControl, InputKind};
    let path = std::env::var_os("TRELLIS_PARITY_MODEL_DIR").expect("set TRELLIS_PARITY_MODEL_DIR");
    let backend = match std::env::var("TRELLIS_PARITY_BACKEND").as_deref() {
        Ok("cpu") => Backend::Cpu,
        Ok("metal") => Backend::Metal,
        _ => panic!("set TRELLIS_PARITY_BACKEND to cpu or metal"),
    };
    let embedder = Embedder::load(std::path::Path::new(&path), backend).unwrap();
    let rows = fixture().rows;
    let cancel = AtomicBool::new(false);
    for kind in ["document", "query"] {
        let selected: Vec<_> = rows.iter().filter(|row| row.kind == kind).collect();
        let texts: Vec<_> = selected.iter().map(|row| row.text.clone()).collect();
        let input_kind = if kind == "query" {
            InputKind::Query
        } else {
            InputKind::Document
        };
        let actual = embedder
            .embed_reference(&texts, input_kind, &EmbeddingControl::new(&cancel))
            .unwrap();
        assert_eq!(actual.len(), selected.len());
        for (vector, row) in actual.iter().zip(&selected) {
            let dot: f64 = vector
                .iter()
                .zip(&row.reference)
                .map(|(a, b)| f64::from(*a) * f64::from(*b))
                .sum();
            let a: f64 = vector.iter().map(|v| f64::from(*v).powi(2)).sum();
            let b: f64 = row.reference.iter().map(|v| f64::from(*v).powi(2)).sum();
            let cosine = dot / (a * b).sqrt();
            assert!(cosine >= 0.999, "{} {backend:?} cosine {cosine}", row.id);
        }
        let production = embedder
            .embed(&texts, input_kind, &EmbeddingControl::new(&cancel))
            .unwrap();
        for (reference, product) in actual.iter().zip(production) {
            assert_eq!(quantize(reference).unwrap(), product);
        }
        cancel.store(true, std::sync::atomic::Ordering::Release);
        assert!(matches!(
            embedder.embed(&texts, input_kind, &EmbeddingControl::new(&cancel)),
            Err(EmbedError::Cancelled)
        ));
        cancel.store(false, std::sync::atomic::Ordering::Release);
    }
}
