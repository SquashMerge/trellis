//! Exercise the real fill read/inference/write path with multiple synthetic IDs.
use super::*;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicUsize;

static HISTORIES: AtomicUsize = AtomicUsize::new(0);
static COLLISIONS: AtomicUsize = AtomicUsize::new(0);
static REEMBEDS: AtomicUsize = AtomicUsize::new(0);
static REUSES: AtomicUsize = AtomicUsize::new(0);

fn model(id: u8) -> String {
    format!("{id:064x}")
}
fn card_hash(text: u8) -> String {
    crate::native_ids::digest(
        b"trellis.search-card.v1\0",
        format!("card{text}").as_bytes(),
    )
}
fn vector(model: u8, text: u8) -> Vec<i8> {
    let mut v = vec![0; DIMENSIONS];
    v[0] = model as i8 + 1;
    v[1] = text as i8 + 1;
    v
}

struct Model {
    id: u8,
    calls: Arc<Mutex<usize>>,
}
impl SearchEmbedder for Model {
    fn embed(
        &self,
        _: &[String],
        _: InputKind,
        _: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        unreachable!("documents use prepared inference")
    }
    fn document_tokenizer(&self) -> DocumentTokenizer {
        tokenizer()
    }
    fn embed_prepared(
        &self,
        docs: &[PreparedDocument],
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        control.check()?;
        *self.calls.lock().unwrap() += docs.len();
        Ok(docs
            .iter()
            .map(|d| {
                let text = (0..4)
                    .find(|n| d.token_ids_for_tests().contains(&(n + 2)))
                    .unwrap() as u8;
                QuantizedVector {
                    values: vector(self.id, text),
                }
            })
            .collect())
    }
}

#[derive(Clone, Debug)]
enum Op {
    Fill(u8, u8),
    Edit(u8, u8),
    WrongDim(u8, u8),
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(16),
        .. ProptestConfig::default()
    })]
    #[test]
    fn fill_model_histories(ops in prop::collection::vec(prop_oneof![
        5 => (0u8..3, 0u8..3).prop_map(|(m,c)| Op::Fill(m,c)),
        3 => (0u8..3, 0u8..4).prop_map(|(c,t)| Op::Edit(c,t)),
        1 => (0u8..3, 0u8..3).prop_map(|(m,c)| Op::WrongDim(m,c)),
    ], 1..20)) {
        let harness = harness(None);
        let service = harness.service;
        service.with_vector_writer(|_| Ok(())).unwrap();
        let backend = LocalBackend(service.clone());
        let calls = Arc::new(Mutex::new(0));
        let cancel = AtomicBool::new(false);
        let mut expected = BTreeMap::<(u8,u8), Vec<i8>>::new();
        let mut cards = [0u8, 0, 1];
        let mut expected_calls = 0;
        let mut reuses = 0;
        let mut reembeds = 0;
        let mut collisions = BTreeSet::new();
        // Every history encounters a real cross-model collision before random work.
        let seed = [Op::Fill(0,0), Op::Fill(1,1), Op::Fill(0,1), Op::WrongDim(2,2), Op::Fill(2,2)];
        for op in seed.into_iter().chain(ops) {
            match op {
                Op::Edit(c,t) => cards[c as usize] = t,
                Op::WrongDim(m,c) => {
                    let t = cards[c as usize];
                    service.with_vector_writer(|writer| writer.put(&model(m), &card_hash(t), &[7])).unwrap();
                    expected.insert((m,t), vec![7]);
                },
                Op::Fill(m,c) => {
                    let t = cards[c as usize];
                    let source = Pages {
                        cards: vec![HeadSearchCard { syntax_id: "sid:v1:00000000000000000000000000000000".into(), card_hash: card_hash(t), card_text: format!("card{t}") }],
                        read: None, gate: None, panic_at: None,
                    };
                    let page = source.page(None, None).unwrap();
                    let mut reader = service.fill_reader().unwrap().unwrap();
                    let missing = LocalBackend::missing(Some(&mut reader), &model(m), &page).unwrap();
                    prop_assert_eq!(&missing, &vec![expected.get(&(m,t)).is_none_or(|v| v.len() != DIMENSIONS)]);
                    if missing[0] {
                        if expected.contains_key(&(m,t)) { reembeds += 1; }
                        expected_calls += 1;
                        let fake = Model { id: m, calls: calls.clone() };
                        backend.fill_loaded(&source, &cancel, &fake, &model(m), (page, missing), Some(&mut reader)).unwrap();
                        expected.insert((m,t), vector(m,t));
                    } else {
                        reuses += 1;
                    }
                    prop_assert_eq!(*calls.lock().unwrap(), expected_calls);
                },
            }
            service.with_vector_snapshot(|snapshot| {
                let snapshot = snapshot.unwrap();
                for m in 0..3 { for t in 0..4 {
                    assert_eq!(snapshot.get(&model(m), &card_hash(t))?, expected.get(&(m,t)).cloned(), "model={m} text={t}");
                }}
                Ok(())
            }).unwrap();
            for t in 0..4 {
                let vectors: Vec<_> = (0..3).filter_map(|m| expected.get(&(m,t))).filter(|v| v.len() == DIMENSIONS).collect();
                if vectors.iter().enumerate().any(|(i, a)| vectors.iter().skip(i+1).any(|b| a != b)) {
                    collisions.insert(t);
                }
            }
        }
        prop_assert!(!collisions.is_empty());
        COLLISIONS.fetch_add(collisions.len(), Ordering::Relaxed);
        REEMBEDS.fetch_add(reembeds, Ordering::Relaxed);
        REUSES.fetch_add(reuses, Ordering::Relaxed);
        let completed = HISTORIES.fetch_add(1, Ordering::Relaxed) + 1;
        let configured = std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(16);
        if completed == configured {
            eprintln!("coverage: {completed} histories, {} distinct-per-history cross-model card collisions, {} cache reuses, {} dimension-mismatch re-embeddings", COLLISIONS.load(Ordering::Relaxed), REUSES.load(Ordering::Relaxed), REEMBEDS.load(Ordering::Relaxed));
        }
    }
}
