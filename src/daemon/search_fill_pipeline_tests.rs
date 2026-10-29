use super::*;
use crate::{
    search_embed::{QuantizedVector, SearchEmbedderFactory},
    store::{HeadSearchCard, topology::TopologyRoots},
};
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Clone)]
struct Gate {
    entered: Sender<()>,
    release: Arc<Mutex<Receiver<()>>>,
}
impl Gate {
    fn pause(&self) {
        self.entered.send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .unwrap();
    }
}
fn gate() -> (Gate, Receiver<()>, Sender<()>) {
    let (entered, arrived) = mpsc::channel();
    let (resume, release) = mpsc::channel();
    (
        Gate {
            entered,
            release: Arc::new(Mutex::new(release)),
        },
        arrived,
        resume,
    )
}
fn receive<T>(receiver: &Receiver<T>) -> T {
    receiver
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .unwrap()
}
fn pin() -> IndexPin {
    IndexPin {
        index_generation: uuid::Uuid::nil(),
        index_revision: 1,
    }
}
fn tokenizer() -> DocumentTokenizer {
    use tokenizers::{Tokenizer, TruncationParams};
    let mut vocab = serde_json::Map::from_iter([
        ("[UNK]".into(), serde_json::json!(0)),
        ("a".into(), serde_json::json!(1)),
    ]);
    for index in 0..256 {
        vocab.insert(format!("card{index}"), serde_json::json!(index + 2));
    }
    let mut tokenizer = Tokenizer::from_bytes(
        serde_json::to_vec(&serde_json::json!({
            "version": "1.0", "truncation": null, "padding": null,
            "added_tokens": [], "normalizer": null,
            "pre_tokenizer": {"type": "Whitespace"},
            "post_processor": null, "decoder": null,
            "model": {"type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]"}
        }))
        .unwrap(),
    )
    .unwrap();
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: 512,
            ..Default::default()
        }))
        .unwrap();
    DocumentTokenizer::from_tokenizer(&tokenizer)
}
fn document_vector(document: &PreparedDocument) -> Vec<i8> {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for id in document.token_ids_for_tests() {
        digest.update(id.to_le_bytes());
    }
    let digest = digest.finalize();
    (0..DIMENSIONS)
        .map(|index| (digest[index % digest.len()] % 127 + 1) as i8)
        .collect()
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Documents(Vec<usize>),
    Query(String),
}
struct RecordingModel {
    tokenizer: DocumentTokenizer,
    calls: Arc<Mutex<Vec<Call>>>,
    document_gate: Option<(usize, Gate)>,
}
impl SearchEmbedder for RecordingModel {
    fn embed(
        &self,
        texts: &[String],
        kind: InputKind,
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        control.check()?;
        assert_eq!(kind, InputKind::Query);
        self.calls
            .lock()
            .unwrap()
            .push(Call::Query(texts[0].clone()));
        Ok(texts
            .iter()
            .map(|_| QuantizedVector {
                values: vec![1; DIMENSIONS],
            })
            .collect())
    }
    fn document_tokenizer(&self) -> DocumentTokenizer {
        self.tokenizer.clone()
    }
    fn embed_prepared(
        &self,
        documents: &[PreparedDocument],
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        let nth = {
            let mut calls = self.calls.lock().unwrap();
            let nth = calls
                .iter()
                .filter(|call| matches!(call, Call::Documents(_)))
                .count();
            calls.push(Call::Documents(
                documents.iter().map(PreparedDocument::token_len).collect(),
            ));
            nth
        };
        if let Some((target, gate)) = &self.document_gate
            && *target == nth
        {
            gate.pause();
        }
        control.check()?;
        Ok(documents
            .iter()
            .map(|document| QuantizedVector {
                values: document_vector(document),
            })
            .collect())
    }
}
struct Factory {
    calls: Arc<Mutex<Vec<Call>>>,
    document_gate: Option<(usize, Gate)>,
}
impl SearchEmbedderFactory for Factory {
    fn load(&self, _: &EmbeddingControl<'_>) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
        Ok(Box::new(RecordingModel {
            tokenizer: tokenizer(),
            calls: self.calls.clone(),
            document_gate: self.document_gate.clone(),
        }))
    }
}
struct Harness {
    _directory: tempfile::TempDir,
    service: Arc<SearchService>,
    calls: Arc<Mutex<Vec<Call>>>,
}
fn harness(document_gate: Option<(usize, Gate)>) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let roots = TopologyRoots::isolated_for_tests(
        directory.path().join("cache"),
        directory.path().join("data"),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let service = Arc::new(
        SearchService::with_embedder_for_tests(
            roots,
            Arc::new(Factory {
                calls: calls.clone(),
                document_gate,
            }),
        )
        .unwrap(),
    );
    Harness {
        _directory: directory,
        service,
        calls,
    }
}
struct Pages {
    cards: Vec<HeadSearchCard>,
    read: Option<Sender<usize>>,
    gate: Option<(usize, Gate)>,
    panic_at: Option<usize>,
}
impl FillSource for Pages {
    fn page(&self, expected: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards> {
        assert!(expected.is_none_or(|expected| expected == pin()));
        let start = after.map_or(0, |cursor| {
            usize::from_str_radix(cursor.strip_prefix("sid:v1:").unwrap(), 16).unwrap() + 1
        });
        if let Some(read) = &self.read {
            read.send(start).unwrap();
        }
        if let Some((target, gate)) = &self.gate
            && *target == start
        {
            gate.pause();
        }
        assert_ne!(self.panic_at, Some(start), "injected producer panic");
        Ok(HeadSearchCards {
            pin: pin(),
            cards: self.cards.iter().skip(start).take(BATCH).cloned().collect(),
        })
    }
}
fn pages(count: usize, mixed: bool) -> Pages {
    Pages {
        cards: (0..count)
            .map(|index| HeadSearchCard {
                syntax_id: format!("sid:v1:{index:032x}"),
                card_hash: format!("{index:064x}"),
                card_text: "a ".repeat(if mixed {
                    if index % 2 == 0 { 100 } else { 1 }
                } else {
                    1
                }),
            })
            .collect(),
        read: None,
        gate: None,
        panic_at: None,
    }
}
fn run(
    service: Arc<SearchService>,
    source: Arc<Pages>,
    cancel: Arc<AtomicBool>,
) -> (std::thread::JoinHandle<()>, Receiver<Result<()>>) {
    let (finished, result) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let outcome = LocalBackend(service).run(source.as_ref(), &cancel, None, &|_| {});
        finished.send(outcome.result).unwrap();
    });
    (handle, result)
}
fn query(service: Arc<SearchService>, text: &'static str) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        assert!(
            service
                .query_embedding(
                    text,
                    &Arc::new(AtomicBool::new(false)),
                    Instant::now() + crate::testing::HANG_BACKSTOP,
                    None
                )
                .unwrap()
                .vector
                .is_some()
        );
    })
}

#[test]
fn prepared_windows_sort_real_prompted_lengths_and_use_128_document_slices() {
    let harness = harness(None);
    let mut source = pages(256, true);
    for (index, card) in source.cards.iter_mut().enumerate() {
        card.card_text.push_str(&format!("card{index}"));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let expected = source
        .cards
        .iter()
        .map(|card| {
            let prepared = tokenizer()
                .prepare(
                    std::slice::from_ref(&card.card_text),
                    &EmbeddingControl::new(&cancel),
                )
                .unwrap();
            (card.card_hash.clone(), document_vector(&prepared[0]))
        })
        .collect::<Vec<_>>();
    assert_ne!(expected[0].1, expected[1].1);
    assert_ne!(
        expected[0].1, expected[2].1,
        "same-length texts must differ too"
    );
    let (worker, result) = run(harness.service.clone(), Arc::new(source), cancel);
    receive(&result).unwrap();
    worker.join().unwrap();
    harness
        .service
        .with_vector_snapshot(|snapshot| {
            let snapshot = snapshot.expect("completed fill cache");
            let model_id = model_id_for_card_format(CardFormat::V1.as_str());
            for (hash, vector) in &expected {
                assert_eq!(
                    snapshot.get(&model_id, hash)?.as_ref(),
                    Some(vector),
                    "hash {hash}"
                );
            }
            Ok(())
        })
        .unwrap();
    let calls = harness.calls.lock().unwrap();
    let lengths = calls
        .iter()
        .flat_map(|call| match call {
            Call::Documents(lengths) => lengths.clone(),
            _ => panic!("unexpected query"),
        })
        .collect::<Vec<_>>();
    assert_eq!(lengths.len(), 256);
    assert!(lengths.windows(2).all(|pair| pair[0] <= pair[1]));
    let Call::Documents(first) = &calls[0] else {
        unreachable!()
    };
    assert_eq!(first.len(), DOCUMENT_BATCH_MAX);
    assert!(first[0] < *lengths.last().unwrap());
    for call in calls.iter() {
        let Call::Documents(lengths) = call else {
            unreachable!()
        };
        assert!(lengths.len() <= DOCUMENT_BATCH_MAX);
        assert!(lengths.len() * lengths.iter().max().unwrap() <= DOCUMENT_BATCH_TOKEN_WORK_MAX);
    }
}

#[cfg(feature = "test-support")]
#[test]
fn parity_preparation_uses_production_hash_order_and_slice_bounds() {
    let texts = (0..TOKEN_WINDOW + 1)
        .map(|index| {
            format!(
                "{}card{}",
                "a ".repeat(if index % 2 == 0 { 100 } else { 1 }),
                index % 256
            )
        })
        .collect::<Vec<_>>();
    let cancel = AtomicBool::new(false);
    let windows = prepare_document_windows_for_tests(&texts, tokenizer(), &cancel).unwrap();
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].0.len(), TOKEN_WINDOW);
    let expected = tokenizer()
        .prepare(&texts, &EmbeddingControl::new(&cancel))
        .unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for (ordinals, documents) in windows {
        assert!(
            documents
                .windows(2)
                .all(|pair| pair[0].token_len() <= pair[1].token_len())
        );
        for (&ordinal, document) in ordinals.iter().zip(&documents) {
            assert!(seen.insert(ordinal));
            assert_eq!(
                document.token_ids_for_tests(),
                expected[ordinal].token_ids_for_tests()
            );
        }
        let mut start = 0;
        while start < documents.len() {
            let end = prepared_document_slice_end_for_tests(&documents, start);
            assert!(end > start);
            assert!(end - start <= DOCUMENT_BATCH_MAX);
            assert!(
                (end - start) * documents[end - 1].token_len() <= DOCUMENT_BATCH_TOKEN_WORK_MAX
            );
            start = end;
        }
    }
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        (0..texts.len()).collect::<Vec<_>>()
    );
}

#[test]
fn producer_prepares_a_queued_window_while_current_model_slice_is_paused() {
    let (model_gate, entered, resume) = gate();
    let harness = harness(Some((0, model_gate)));
    let (read, reads) = mpsc::channel();
    let mut source = pages(TOKEN_WINDOW * 2 + 1, false);
    source.read = Some(read);
    let (worker, result) = run(
        harness.service,
        Arc::new(source),
        Arc::new(AtomicBool::new(false)),
    );
    receive(&entered);
    // Reading window three requires window two to have been tokenized and
    // successfully queued while inference remains paused on window one.
    while receive(&reads) != TOKEN_WINDOW * 2 {}
    assert_eq!(harness.calls.lock().unwrap().len(), 1);
    resume.send(()).unwrap();
    receive(&result).unwrap();
    worker.join().unwrap();
}

#[test]
fn fifo_queries_run_before_the_next_prepared_slice() {
    let (model_gate, entered, resume) = gate();
    let harness = harness(Some((0, model_gate)));
    let (worker, result) = run(
        harness.service.clone(),
        Arc::new(pages(256, false)),
        Arc::new(AtomicBool::new(false)),
    );
    receive(&entered);
    let first = query(harness.service.clone(), "first");
    harness.service.wait_for_queued_queries_for_tests(1);
    let second = query(harness.service.clone(), "second");
    harness.service.wait_for_queued_queries_for_tests(2);
    resume.send(()).unwrap();
    first.join().unwrap();
    second.join().unwrap();
    receive(&result).unwrap();
    worker.join().unwrap();
    let calls = harness.calls.lock().unwrap();
    assert!(matches!(calls[0], Call::Documents(_)));
    assert_eq!(calls[1], Call::Query("first".into()));
    assert_eq!(calls[2], Call::Query("second".into()));
    assert!(matches!(calls[3], Call::Documents(_)));
}

#[test]
fn query_runs_while_the_producer_has_not_published_a_window() {
    let (read_gate, entered, resume) = gate();
    let harness = harness(None);
    let mut source = pages(32, false);
    source.gate = Some((BATCH, read_gate));
    let (worker, result) = run(
        harness.service.clone(),
        Arc::new(source),
        Arc::new(AtomicBool::new(false)),
    );
    receive(&entered);
    let query = query(harness.service, "producer wait");
    query.join().unwrap();
    assert_eq!(
        *harness.calls.lock().unwrap(),
        vec![Call::Query("producer wait".into())]
    );
    resume.send(()).unwrap();
    receive(&result).unwrap();
    worker.join().unwrap();
}

#[test]
fn cancellation_drops_a_full_prepared_queue_and_joins_the_producer() {
    let (model_gate, entered, resume) = gate();
    let harness = harness(Some((0, model_gate)));
    let (read, reads) = mpsc::channel();
    let mut source = pages(TOKEN_WINDOW * 3 + 1, false);
    source.read = Some(read);
    let cancel = Arc::new(AtomicBool::new(false));
    let (worker, result) = run(harness.service, Arc::new(source), cancel.clone());
    receive(&entered);
    while receive(&reads) != TOKEN_WINDOW * 2 {}
    // The second window occupies the capacity-one channel; the model still
    // holds the first. Cancellation must release the producer, not await sends.
    cancel.store(true, Ordering::Release);
    resume.send(()).unwrap();
    let error = receive(&result).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<EmbedError>(),
        Some(EmbedError::Cancelled)
    ));
    worker.join().unwrap();
    assert_eq!(harness.calls.lock().unwrap().len(), 1);
}

#[test]
fn producer_panic_returns_a_typed_failure_without_hanging() {
    let harness = harness(None);
    let mut source = pages(32, false);
    source.panic_at = Some(BATCH);
    let (worker, result) = run(
        harness.service,
        Arc::new(source),
        Arc::new(AtomicBool::new(false)),
    );
    let error = receive(&result).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<EmbedError>(),
        Some(EmbedError::Runtime(_))
    ));
    worker.join().unwrap();
}

#[test]
fn current_inference_overlaps_the_previous_put_many_stage() {
    let (write_gate, writing, release_write) = gate();
    let (model_gate, embedding, release_model) = gate();
    let harness = harness(Some((1, model_gate)));
    *harness.service.before_fill_put.lock().unwrap() = Some(Box::new(move || write_gate.pause()));
    let (worker, result) = run(
        harness.service,
        Arc::new(pages(256, false)),
        Arc::new(AtomicBool::new(false)),
    );
    receive(&writing);
    receive(&embedding);
    assert_eq!(harness.calls.lock().unwrap().len(), 2);
    release_write.send(()).unwrap();
    release_model.send(()).unwrap();
    receive(&result).unwrap();
    worker.join().unwrap();
}

#[test]
fn writer_panic_returns_a_typed_failure_after_the_next_slice_is_computed() {
    let (write_gate, writing, release_write) = gate();
    let (model_gate, embedding, release_model) = gate();
    let harness = harness(Some((1, model_gate)));
    *harness.service.before_fill_put.lock().unwrap() = Some(Box::new(move || {
        write_gate.pause();
        panic!("injected writer panic");
    }));
    let (worker, result) = run(
        harness.service.clone(),
        Arc::new(pages(256, false)),
        Arc::new(AtomicBool::new(false)),
    );
    receive(&writing);
    receive(&embedding);
    assert_eq!(harness.calls.lock().unwrap().len(), 2);
    release_write.send(()).unwrap();
    release_model.send(()).unwrap();
    let error = receive(&result).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<EmbedError>(),
        Some(EmbedError::Runtime(_))
    ));
    worker.join().unwrap();
    assert_eq!(harness.calls.lock().unwrap().len(), 2);
    assert_eq!(harness.service.vector_report().unwrap().entries, 0);
}

#[test]
fn cancellation_joins_a_paused_writer_with_a_prepared_window_queued() {
    let (write_gate, writing, release_write) = gate();
    let (model_gate, embedding, release_model) = gate();
    let harness = harness(Some((1, model_gate)));
    *harness.service.before_fill_put.lock().unwrap() = Some(Box::new(move || write_gate.pause()));
    let (read, reads) = mpsc::channel();
    let mut source = pages(TOKEN_WINDOW * 3 + 1, false);
    source.read = Some(read);
    let cancel = Arc::new(AtomicBool::new(false));
    let (worker, result) = run(harness.service, Arc::new(source), cancel.clone());
    receive(&writing);
    receive(&embedding);
    while receive(&reads) != TOKEN_WINDOW * 2 {}
    cancel.store(true, Ordering::Release);
    release_write.send(()).unwrap();
    release_model.send(()).unwrap();
    let error = receive(&result).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<EmbedError>(),
        Some(EmbedError::Cancelled)
    ));
    worker.join().unwrap();
    assert_eq!(harness.calls.lock().unwrap().len(), 2);
}
