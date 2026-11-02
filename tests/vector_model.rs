//! Discovery properties for vector encoding and the machine-wide cache.
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use trellis::{
    native_ids::digest,
    search_embed::{EmbedError, quantize},
    search_vectors::{InvalidVector, VectorCacheReader, VectorCacheWriter},
};

const N: usize = 1024;

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32)
}

// Independent nearest-even rounding, retaining the specified f64 division by
// max/127 (multiplication by 127/max can differ at a half-step).
fn nearest_even(x: f64) -> i8 {
    let lo = x.floor();
    let fraction = x - lo;
    let rounded = if fraction < 0.5 || (fraction == 0.5 && (lo as i64) % 2 == 0) {
        lo
    } else {
        lo + 1.0
    };
    rounded.clamp(-128.0, 127.0) as i8
}

fn reference(input: &[f32]) -> (Vec<i8>, f64) {
    let max = input[..N]
        .iter()
        .fold(0.0_f64, |a, &b| a.max(f64::from(b).abs()));
    let scale = max / 127.0;
    (
        input[..N]
            .iter()
            .map(|&v| nearest_even(f64::from(v) / scale))
            .collect(),
        scale,
    )
}

fn component() -> impl Strategy<Value = f32> {
    prop_oneof![
        4 => (-1_000_000i32..=1_000_000).prop_map(|n| n as f32 / 1000.0),
        3 => (-254i16..=254).prop_map(|n| f32::from(n) / 2.0),
        2 => prop::sample::select(vec![0.0, -0.0, f32::MIN_POSITIVE, f32::from_bits(1), f32::MAX, -f32::MAX, 127.0, -127.0]),
        1 => any::<u32>().prop_map(f32::from_bits).prop_filter("finite", |v| v.is_finite()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), .. ProptestConfig::default() })]
    #[test]
    fn quantization_reference_and_loss_bound(
        components in prop::collection::vec(component(), 1..80),
        full in any::<bool>(),
        anchor in prop::sample::select(vec![127.0f32, -127.0, f32::MAX, f32::from_bits(1), 1.0]),
    ) {
        let mut input = vec![0.0; if full { 2*N } else { N }];
        input[0] = anchor;
        for (i, v) in components.iter().enumerate() { input[i+1] = *v; }
        if full { input[N] = f32::MAX; }
        let (expected, scale) = reference(&input);
        let actual = quantize(&input).unwrap().values;
        prop_assert_eq!(&actual, &expected);
        prop_assert_eq!(actual.len(), N);
        prop_assert!(actual.iter().any(|v| *v != 0));
        prop_assert_eq!(quantize(&input).unwrap().values, actual.clone());
        for (&x, &q) in input.iter().zip(&actual) {
            // Reconstruct with the source-derived scale; no scale is stored.
            // Include f64 division/multiplication error in the half-step bound.
            let reconstructed = f64::from(q) * scale;
            let error = (f64::from(x) - reconstructed).abs();
            let bound = scale * 0.5 + f64::from(x).abs().max(scale) * f64::EPSILON * 4.0;
            prop_assert!(reconstructed.is_finite() && error <= bound, "x={x:?} q={q} scale={scale} error={error} bound={bound}");
        }
    }
}

#[test]
fn half_steps_neighbors_and_invalid_inputs() {
    let mut input = vec![0.0; N];
    input[0] = 127.0;
    let mut index = 1;
    for n in -126..=126 {
        let tie = n as f32 + 0.5;
        for value in [tie.next_down(), tie, tie.next_up()] {
            input[index] = value;
            index += 1;
        }
    }
    assert_eq!(quantize(&input).unwrap().values, reference(&input).0);
    for len in [0, 1, N - 1, N + 1, 2 * N - 1, 2 * N + 1] {
        assert!(matches!(
            quantize(&vec![1.0; len]),
            Err(EmbedError::InvalidInput(_))
        ));
    }
    for len in [N, 2 * N] {
        assert!(matches!(
            quantize(&vec![0.0; len]),
            Err(EmbedError::InvalidInput(_))
        ));
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for pos in [0, len - 1] {
                let mut v = vec![1.0; len];
                v[pos] = invalid;
                assert!(matches!(quantize(&v), Err(EmbedError::InvalidInput(_))));
            }
        }
    }
    let mut suffix_only = vec![0.0; 2 * N];
    suffix_only[N] = 1.0;
    assert!(matches!(
        quantize(&suffix_only),
        Err(EmbedError::InvalidInput(_))
    ));
}

fn model(id: u8) -> String {
    format!("{id:064x}")
}
fn hash(text: &str) -> String {
    digest(b"trellis.search-card.v1\0", text.as_bytes())
}

// A deliberately local deterministic fake provider. Its output distinguishes
// models and texts, so an incorrect cache hit is observable without inference.
fn fake_embedding(model: u8, text: u8) -> Vec<i8> {
    let mut v = vec![0; N];
    v[0] = model as i8 + 1;
    v[1] = text as i8 + 1;
    v
}

#[derive(Clone, Debug)]
enum Op {
    Fill(u8, u8),
    Edit(u8, u8),
    Reindex,
    Replace(u8, u8, u8),
    Touch(u8, u8),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (0u8..3, 0u8..3).prop_map(|(m,c)| Op::Fill(m,c)),
        3 => (0u8..3, 0u8..4).prop_map(|(c,t)| Op::Edit(c,t)),
        1 => Just(Op::Reindex),
        2 => (0u8..3, 0u8..3, 1u8..50).prop_map(|(m,c,v)| Op::Replace(m,c,v)),
        1 => (0u8..3, 0u8..3).prop_map(|(m,c)| Op::Touch(m,c)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), .. ProptestConfig::default() })]
    #[test]
    fn model_keyed_cache_histories(ops in prop::collection::vec(op(), 1..30)) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.db");
        let mut writer = VectorCacheWriter::open(&path).unwrap();
        let mut expected: BTreeMap<(u8,u8), Vec<i8>> = BTreeMap::new();
        // Three identities deliberately overlap in exact text.
        let mut cards = [0u8, 0, 1];
        let mut calls = 0usize;
        // Guarantee a genuinely competing model pair for the same card hash.
        for m in 0..2 {
            let vector = fake_embedding(m, 0);
            writer.put(&model(m), &hash("card0"), &vector).unwrap();
            expected.insert((m,0), vector);
        }
        for op in ops {
            match op {
                Op::Edit(c,t) => cards[c as usize] = t,
                // A rebuilt index retains the separate machine-wide vector cache.
                Op::Reindex => { drop(writer); writer = VectorCacheWriter::open(&path).unwrap(); },
                Op::Touch(m,c) => writer.touch(&model(m), &hash(&format!("card{}", cards[c as usize]))).unwrap(),
                Op::Replace(m,c,value) => {
                    let t = cards[c as usize]; let vector = vec![value as i8; N];
                    writer.put(&model(m), &hash(&format!("card{t}")), &vector).unwrap();
                    expected.insert((m,t), vector);
                },
                Op::Fill(m,c) => {
                    let t = cards[c as usize]; let h = hash(&format!("card{t}"));
                    let hit = writer.snapshot().unwrap().get(&model(m), &h).unwrap();
                    prop_assert_eq!(&hit, &expected.get(&(m,t)).cloned());
                    if hit.is_none() {
                        calls += 1;
                        let vector = fake_embedding(m,t);
                        writer.put(&model(m), &h, &vector).unwrap();
                        expected.insert((m,t), vector);
                    }
                },
            }
            let snapshot = writer.snapshot().unwrap();
            prop_assert_eq!(snapshot.entry_count(), expected.len() as u64);
            for m in 0..3 { for t in 0..4 {
                let got = snapshot.get(&model(m), &hash(&format!("card{t}"))).unwrap();
                prop_assert_eq!(got, expected.get(&(m,t)).cloned(), "model={} text={}", m, t);
            }}
            drop(snapshot);
        }
        let distinct_collisions: BTreeSet<_> = (0..4).filter(|t| (0..3).filter(|m| expected.contains_key(&(*m,*t))).count() >= 2).collect();
        prop_assert!(!distinct_collisions.is_empty());
        prop_assert!(calls <= expected.len());
        drop(writer);
        let mut reader = VectorCacheReader::open_existing(&path).unwrap().unwrap();
        let snapshot = reader.snapshot().unwrap();
        for ((m,t), vector) in expected {
            prop_assert_eq!(snapshot.get(&model(m), &hash(&format!("card{t}"))).unwrap(), Some(vector));
        }
    }
}

#[test]
fn cache_rejects_invalid_values_without_publishing() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = VectorCacheWriter::open(&dir.path().join("vectors.db")).unwrap();
    for vector in [vec![], vec![0; N], vec![1; N + 1]] {
        assert!(
            writer
                .put(&model(0), &hash("card0"), &vector)
                .unwrap_err()
                .is::<InvalidVector>()
        );
    }
    assert_eq!(writer.snapshot().unwrap().entry_count(), 0);
}

#[test]
fn quantized_bytes_survive_real_cache_round_trip() {
    let mut input = vec![0.0; N];
    input[..7].copy_from_slice(&[127.0, -127.0, -2.5, -1.5, 0.5, 1.5, 2.5]);
    let expected = reference(&input).0;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vectors.db");
    let mut writer = VectorCacheWriter::open(&path).unwrap();
    writer
        .put(
            &model(0),
            &hash("numeric"),
            &quantize(&input).unwrap().values,
        )
        .unwrap();
    drop(writer);
    let mut reader = VectorCacheReader::open_existing(&path).unwrap().unwrap();
    assert_eq!(
        reader
            .snapshot()
            .unwrap()
            .get(&model(0), &hash("numeric"))
            .unwrap(),
        Some(expected)
    );
}

#[cfg(feature = "test-support")]
#[test]
fn published_card_edits_and_restoration_reuse_only_matching_cache_keys() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        indexer::{IndexOptions, index_workspace_bundle},
        store::Store,
    };

    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("cards.rs");
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let leader = store.publication_for_tests().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut cache = VectorCacheWriter::open(&state.path().join("vectors.db")).unwrap();
    let publish = |body: &str| {
        std::fs::write(&source, body).unwrap();
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(root.path().to_owned()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let pin = store
            .publish_native(
                &graph,
                &capture,
                &native,
                &leader,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let cards = store.head_search_cards(Some(pin), None, 16).unwrap().cards;
        assert_eq!(cards.len(), 1);
        cards.into_iter().next().unwrap()
    };
    let first = publish("fn orchard() { let _fruit = \"apple\"; }\n");
    let vector = fake_embedding(0, 0);
    cache.put(&model(0), &first.card_hash, &vector).unwrap();
    let changed = publish("fn orchard() { let _fruit = \"pear\"; }\n");
    assert_eq!(first.syntax_id, changed.syntax_id);
    assert_ne!(first.card_hash, changed.card_hash);
    assert_eq!(
        cache
            .snapshot()
            .unwrap()
            .get(&model(0), &changed.card_hash)
            .unwrap(),
        None
    );
    let restored = publish("fn orchard() { let _fruit = \"apple\"; }\n");
    assert_eq!(first.card_hash, restored.card_hash);
    let snapshot = cache.snapshot().unwrap();
    assert_eq!(
        snapshot.get(&model(0), &restored.card_hash).unwrap(),
        Some(vector)
    );
    assert_eq!(snapshot.get(&model(1), &restored.card_hash).unwrap(), None);
}
