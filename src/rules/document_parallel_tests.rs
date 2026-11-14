use super::*;
use crate::references::DocumentInput;
use std::sync::{Arc, Mutex, atomic::AtomicBool};

fn snapshot(files: &[(&str, &str)]) -> Snapshot {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let (graph, native) = crate::indexer::index_workspace_with_native(
        &crate::indexer::IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let documents = graph
        .files
        .into_iter()
        .map(|file| {
            let key = native
                .revision
                .documents
                .iter()
                .find(|d| d.key.path == file.path)
                .unwrap()
                .key
                .clone();
            let declarations = native
                .declarations
                .iter()
                .filter(|d| d.document == key)
                .cloned()
                .collect();
            let calls = native
                .calls
                .iter()
                .filter(|c| c.document == key)
                .cloned()
                .collect();
            DocumentInput {
                semantic: Default::default(),
                key,
                file,
                declarations,
                calls,
            }
        })
        .collect();
    Snapshot {
        pin: crate::model::IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 4,
        },
        documents,
        manifests: vec![],
        root_tsconfig: None,
        evidence_gaps: Default::default(),
    }
}

fn fixture() -> (Config, Snapshot) {
    let config = parse("version=1\n[roles.child]\nselect={name='Child',extends='Base'}\n[[rules]]\nid='inheritance'\nfor_each='child'\nrequire=[{calls='child'}]\n").unwrap();
    let snapshot = snapshot(&[
        ("A.java", "class Child extends Base { void run() {} }"),
        ("B.java", "class Child extends Base { void run() {} }"),
    ]);
    assert_eq!(snapshot.documents.len(), 2);
    (config, snapshot)
}

#[test]
fn detached_rule_proof_retains_historical_basis_and_revision() {
    let metadata = BTreeMap::from([
        (
            "projection".into(),
            json!({"currentPin":{"indexRevision":4},"semanticBasisPin":{"indexRevision":3},"basisNativeRevisionId":"basis-native","semanticStatus":"olderBasis"}),
        ),
        ("historicalFreshness".into(), json!("fresh")),
        ("freshness".into(), json!("possiblyStale")),
        (
            "basis".into(),
            json!({"artifactHash":"artifact","revisionId":"capture-native"}),
        ),
    ]);
    let proof = semantic_evidence(
        json!({"revisionId":"current-native","kind":"semanticBinding"}),
        &metadata,
    );
    assert_eq!(proof["revisionId"], "basis-native");
    assert_eq!(proof["freshness"], "possiblyStale");
    assert_eq!(proof["historicalFreshness"], "fresh");
    assert_eq!(proof["basis"]["revisionId"], "capture-native");
}

#[test]
fn historical_complete_coverage_never_proves_forbidden_call_absence() {
    let config = parse("version=1\n[roles.all]\nselect={name='*'}\n[[rules]]\nid='no-calls'\nfor_each='all'\nforbid=[{calls='all'}]\n").unwrap();
    let mut snapshot = snapshot(&[("src/lib.rs", "pub fn run() {}")]);
    let doc = &mut snapshot.documents[0];
    doc.semantic.producer_id = Some("producer".into());
    doc.semantic.complete = true;
    doc.semantic.projection = Some(crate::model::SemanticProjection {
        current_pin: snapshot.pin,
        semantic_basis_pin: crate::model::IndexPin {
            index_revision: snapshot.pin.index_revision - 1,
            ..snapshot.pin
        },
        basis_native_revision_id: "basis-native".into(),
        semantic_status: crate::model::SemanticStatus::OlderBasis,
    });
    let base = prepare(&config, &snapshot, &mut || Ok(())).unwrap();
    assert!(!base.declarations.is_empty());
    for row in &base.declarations {
        assert_eq!(row["status"], "unknown");
        assert_eq!(
            row["constraints"][0]["unresolved"][0],
            "historical_semantic_absence_unproven"
        );
        assert_eq!(
            row["constraints"][0]["projection"]["semanticStatus"],
            "olderBasis"
        );
    }
}

/// #556: rows prepared for every document select what a scoped evaluation returns.
#[test]
fn prepared_rows_select_what_a_scoped_evaluation_returns() {
    let snapshot = snapshot(&[
        ("src/lib.rs", "mod a; mod b;"),
        ("src/a.rs", "pub fn a() {}"),
        ("src/b.rs", "pub fn b() { crate::a::a(); }"),
    ]);
    let config = parse(
        "version=1\n[layers.high]\norder=1\npaths=['src/b.rs']\n\
             [layers.low]\norder=2\npaths=['src/a.rs']\n\
             [roles.handler]\nselect={paths=['src/**'],name='*'}\n\
             [roles.target]\nselect={paths=['src/a.rs']}\n\
             [[rules]]\nid='calls'\nfor_each='handler'\nrequire=[{calls='target'}]\n",
    )
    .unwrap();
    let prepared = prepare(&config, &snapshot, &mut || Ok(())).unwrap();
    assert!(!prepared.declarations.is_empty());
    for paths in [
        vec![],
        vec!["src/a.rs".to_owned()],
        vec!["src/b.rs".to_owned()],
        vec!["missing/**".to_owned()],
    ] {
        let refs = crate::references::project(&snapshot, &paths, 5, &mut || Ok(())).unwrap();
        let scoped = evaluate(&config, &snapshot, &refs, &paths, &mut || Ok(())).unwrap();
        let selected = evaluate_prepared(&config, &prepared, &refs, &paths, &mut || Ok(()))
            .unwrap()
            .data;
        assert_eq!(scoped, selected, "{paths:?}");
    }
}

#[test]
fn document_work_overlaps_but_rows_and_counts_are_admitted_in_order() {
    let (config, snapshot) = fixture();
    let documents: Vec<_> = snapshot.documents.iter().collect();
    let targets = Targets::new();
    let evaluation = DocumentEvaluation {
        config: &config,
        targets: &targets,
        inheritance: true,
        source_set: None,
        before_document: None,
    };
    let mut serial_work = Work::new(work::rules_limit(2, 0));
    let serial = evaluation
        .run(&documents, &mut serial_work, 1, &mut || Ok(()))
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let rx = Mutex::new(rx);
    let first = &documents[0].key.path;
    let mut parallel_work = Work::new(serial_work.limit);
    let hook = |doc: &DocumentInput| {
        if &doc.key.path == first {
            rx.lock().unwrap().recv().unwrap();
        } else {
            tx.send(()).unwrap();
        }
    };
    let parallel = DocumentEvaluation {
        before_document: Some(&hook),
        ..evaluation
    }
    .run(&documents, &mut parallel_work, 4, &mut || Ok(()))
    .unwrap();
    assert!(!serial.0.is_empty());
    assert_eq!(
        serde_json::to_vec(&serial).unwrap(),
        serde_json::to_vec(&parallel).unwrap()
    );
    assert_eq!(serial_work.used, parallel_work.used);
    assert!(serial_work.used > 0);

    // Each file fits locally, but the sum exceeds the single request allowance.
    let per_file = evaluate_document(
        &config,
        &targets,
        documents[0],
        true,
        None,
        serial_work.limit,
        &mut || Ok(()),
    )
    .unwrap();
    assert!(per_file.work.used > 0 && per_file.work.used < serial_work.used);
    for workers in [1, 4] {
        let mut work = Work::new(serial_work.used - 1);
        let error = evaluation
            .run(&documents, &mut work, workers, &mut || Ok(()))
            .unwrap_err();
        assert!(error.to_string().starts_with("bounded_work:"));
    }
}

#[derive(Debug)]
struct Deadline;
impl std::fmt::Display for Deadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline_exceeded")
    }
}
impl std::error::Error for Deadline {}

#[test]
fn worker_callback_error_is_preserved_without_rows() {
    let (config, snapshot) = fixture();
    let documents: Vec<_> = snapshot.documents.iter().collect();
    let targets = Targets::new();
    let evaluation = DocumentEvaluation {
        config: &config,
        targets: &targets,
        inheritance: true,
        source_set: None,
        before_document: None,
    };
    let stopped = AtomicBool::new(false);
    let mut callback = || {
        if stopped.load(std::sync::atomic::Ordering::Acquire) {
            Err(Deadline.into())
        } else {
            Ok(())
        }
    };
    let hook = |_: &DocumentInput| {
        stopped.store(true, std::sync::atomic::Ordering::Release);
    };
    let error = DocumentEvaluation {
        before_document: Some(&hook),
        ..evaluation
    }
    .run(
        &documents,
        &mut Work::new(work::rules_limit(2, 0)),
        2,
        &mut callback,
    )
    .unwrap_err();
    assert!(error.downcast_ref::<Deadline>().is_some());
}
