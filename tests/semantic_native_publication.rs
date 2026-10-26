#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};
use tempfile::Builder;
use trellis::{native_ids, store::topology::TopologyRoots};

fn cli(home: &Path) -> Command {
    let mut cmd = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    cmd.env("HOME", home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("daemon.pid"))
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null());
    cmd
}
fn run(home: &Path, args: &[&str]) -> Value {
    use std::io::{Read, Seek};
    let mut stdout = tempfile::tempfile_in(home).unwrap();
    let mut stderr = tempfile::tempfile_in(home).unwrap();
    let mut child = cli(home)
        .args(args)
        .stdout(stdout.try_clone().unwrap())
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .unwrap();
    trellis::testing::wait_until("CLI completion", || child.try_wait().unwrap().is_some());
    let status = child.wait().unwrap();
    let mut diagnostics = String::new();
    stderr.rewind().unwrap();
    stderr.read_to_string(&mut diagnostics).unwrap();
    assert!(status.success(), "command failed: {diagnostics}");
    let mut bytes = Vec::new();
    stdout.rewind().unwrap();
    stdout.read_to_end(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
fn selected_case(change_b: &str) {
    use protobuf::{EnumOrUnknown, Message};
    let temp = Builder::new()
        .prefix("semantic-two-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join(".gitignore"), "artifact.scip\nmanifest.json\n").unwrap();
    let a = "function run(){ target(); }\n";
    let b = "function target(){ return 1; }\n";
    fs::write(root.join("src/A.js"), a).unwrap();
    fs::write(root.join("src/B.js"), b).unwrap();
    if change_b == "two_producers" {
        fs::write(root.join("src/C.py"), "def helper(): return 1\n").unwrap();
    }
    let root = fs::canonicalize(root).unwrap();
    let root_string = root.to_str().unwrap();
    let indexed = run(&home, &["index", "--workspace", root_string]);
    let r1: trellis::model::IndexPin =
        serde_json::from_value(indexed["publishedRevision"].clone()).unwrap();
    let roots = TopologyRoots::isolated_for_tests(
        if cfg!(target_os = "macos") {
            home.join("Library/Caches/dev.squashmerge.trellis")
        } else {
            home.join(".cache/trellis")
        },
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support/dev.squashmerge.trellis")
        } else {
            home.join(".local/share/trellis")
        },
    );
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let store = trellis::store::Store::open(roots, identity).unwrap();
    let (native, _) = store.retained_native_snapshot_at(r1).unwrap();
    let mut binary = scip::types::Index::new();
    for (path, text) in [("src/A.js", a), ("src/B.js", b)] {
        let mut doc = scip::types::Document::new();
        doc.relative_path = path.into();
        doc.language = "javascript".into();
        doc.position_encoding = EnumOrUnknown::from_i32(2);
        let start = text.find("target").unwrap();
        let mut occurrence = scip::types::Occurrence::new();
        occurrence.range = vec![0, start as i32, (start + 6) as i32];
        occurrence.symbol = "pkg target.".into();
        occurrence.symbol_roles = if path.ends_with("A.js") { 0 } else { 1 };
        occurrence.syntax_kind = EnumOrUnknown::from_i32(0);
        if path.ends_with("A.js") {
            occurrence.enclosing_range = vec![0, start as i32, (start + 8) as i32];
            let start = text.find("run").unwrap();
            let mut independent = scip::types::Occurrence::new();
            independent.range = vec![0, start as i32, (start + 3) as i32];
            independent.symbol = "pkg run.".into();
            independent.symbol_roles = 1;
            independent.syntax_kind = EnumOrUnknown::from_i32(0);
            doc.occurrences.push(independent);
            let mut info = scip::types::SymbolInformation::new();
            info.symbol = "pkg run.".into();
            info.kind = EnumOrUnknown::from_i32(26);
            info.display_name = "run".into();
            doc.symbols.push(info);
        } else {
            let mut info = scip::types::SymbolInformation::new();
            info.symbol = "pkg target.".into();
            info.kind = EnumOrUnknown::from_i32(26);
            info.display_name = "target".into();
            doc.symbols.push(info);
        }
        doc.occurrences.push(occurrence);
        binary.documents.push(doc);
    }
    if change_b == "unmatched_both" {
        let mut claim = scip::types::Occurrence::new();
        claim.range = vec![0, 0, 8];
        claim.symbol = "unmatched0#".into();
        claim.symbol_roles = 0;
        binary.documents[0].occurrences.push(claim);
    }
    if matches!(change_b, "duplicate_call_claims" | "contradictory_calls") {
        let mut repeated = binary.documents[0]
            .occurrences
            .iter()
            .find(|occurrence| occurrence.symbol == "pkg target.")
            .unwrap()
            .clone();
        if change_b == "contradictory_calls" {
            repeated.symbol = "external pkg other.".into();
        } else {
            repeated.syntax_kind = EnumOrUnknown::from_i32(1);
        }
        binary.documents[0].occurrences.push(repeated);
    }
    if change_b == "zero_claims" {
        for document in &mut binary.documents {
            document.occurrences.clear();
            document.symbols.clear();
        }
    }
    let bytes = binary.write_to_bytes().unwrap();
    fs::write(root.join("artifact.scip"), &bytes).unwrap();
    let profile: Value = serde_json::from_str(include_str!(
        "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
    ))
    .unwrap();
    let producer = profile["semanticProducers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "scip-typescript")
        .unwrap();
    let documents: Vec<_> = native.revision.documents.iter().map(|d|json!({"language":d.key.language,"relativePath":d.key.path,"contentHash":d.content_hash})).collect();
    let manifest_hash = native_ids::digest(
        b"",
        &native_ids::canonical(&json!(
            native
                .revision
                .documents
                .iter()
                .map(|d| json!({"document":d.key,"contentHash":d.content_hash}))
                .collect::<Vec<_>>()
        )),
    );
    let exclusion_reason = match change_b {
        "partial_empty_covered" => Some("notRequested"),
        "partial_not_requested" => Some("notRequested"),
        "partial_unsupported" => Some("unsupported"),
        "partial_failed" | "caller_failed" => Some("failed"),
        _ => None,
    };
    let partial = exclusion_reason.is_some();
    if partial {
        binary.documents.retain(|d| {
            d.relative_path
                == if change_b == "caller_failed" {
                    "src/B.js"
                } else {
                    "src/A.js"
                }
        });
        if change_b == "partial_empty_covered" {
            binary.documents[0].occurrences.clear();
            binary.documents[0].symbols.clear();
        }
        fs::write(root.join("artifact.scip"), binary.write_to_bytes().unwrap()).unwrap();
    }
    let bytes = fs::read(root.join("artifact.scip")).unwrap();
    let covered_documents: Vec<_> = documents
        .iter()
        .filter(|d| {
            d["language"] == "javascript"
                && (!partial
                    || d["relativePath"]
                        == if change_b == "caller_failed" {
                            "src/B.js"
                        } else {
                            "src/A.js"
                        })
        })
        .cloned()
        .collect();
    let artifact_documents = if change_b == "caller_failed" {
        vec!["src/B.js"]
    } else if partial {
        vec!["src/A.js"]
    } else {
        vec!["src/A.js", "src/B.js"]
    };
    let exclusions = exclusion_reason
        .map(|reason| {
            vec![json!({"language":"javascript","relativePath":if change_b == "caller_failed" { "src/A.js" } else { "src/B.js" },"reason":reason})]
        })
        .unwrap_or_default();
    let manifest = json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":if partial {"partial"} else {"complete"},"decoderVersion":"scip-0.10.0/CapturedScipFactV1/1","profileId":profile["profileId"],
      "producer":{"name":producer["name"],"version":producer["version"],"executableHash":producer["executableHash"],"positionEncoding":2},
      "sourceSet":native.source_set,"sourceRevision":native.revision.id,"sourceManifestHash":manifest_hash,
      "components":{"toolchainHash":native.revision.toolchain_hash,"configHash":native.revision.config_hash,"dependencyHash":native.revision.dependency_hash},
      "metadata":{"protocolVersion":0,"projectRoot":format!("file://{}",root.display()),"textDocumentEncoding":1},
      "sourceDocuments":documents,"documents":covered_documents,"exclusions":exclusions,
      "artifacts":[{"kind":"scipBinary","path":"artifact.scip","hash":native_ids::digest(b"",&bytes),"language":"javascript","documents":artifact_documents}]});
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(root.join("manifest.json"), &manifest_bytes).unwrap();
    let digest = native_ids::digest(b"", &manifest_bytes);
    let artifact_path = root.join("artifact.scip");
    let manifest_path = root.join("manifest.json");
    let args = [
        "semantic",
        "import",
        "--workspace",
        root_string,
        "--scip",
        artifact_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
        "--profile",
        "mac-mini-m5-pro-v1",
        "--producer",
        "scip-typescript",
        "--manifest-sha256",
        &digest,
    ];
    let imported = run(&home, &args);
    let imported = if change_b == "two_producers" {
        let python = profile["semanticProducers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "scip-python")
            .unwrap();
        let mut python_binary = scip::types::Index::new();
        let mut doc = scip::types::Document::new();
        doc.relative_path = "src/C.py".into();
        doc.language = "python".into();
        doc.position_encoding = EnumOrUnknown::from_i32(2);
        let mut definition = scip::types::Occurrence::new();
        definition.range = vec![0, 4, 10];
        definition.symbol = "scip-python pkg helper().".into();
        definition.symbol_roles = 1;
        doc.occurrences.push(definition);
        python_binary.documents.push(doc);
        let bytes = python_binary.write_to_bytes().unwrap();
        fs::write(root.join("python.scip"), &bytes).unwrap();
        let covered: Vec<_> = documents
            .iter()
            .filter(|d| d["language"] == "python")
            .cloned()
            .collect();
        let manifest = json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":"complete","decoderVersion":"scip-0.10.0/CapturedScipFactV1/1","profileId":profile["profileId"],
          "producer":{"name":python["name"],"version":python["version"],"executableHash":python["executableHash"],"positionEncoding":2},
          "sourceSet":native.source_set,"sourceRevision":native.revision.id,"sourceManifestHash":manifest_hash,
          "components":{"toolchainHash":native.revision.toolchain_hash,"configHash":native.revision.config_hash,"dependencyHash":native.revision.dependency_hash},
          "metadata":{"protocolVersion":0,"projectRoot":format!("file://{}",root.display()),"textDocumentEncoding":1},
          "sourceDocuments":documents,"documents":covered,"exclusions":[],
          "artifacts":[{"kind":"scipBinary","path":"python.scip","hash":native_ids::digest(b"",&bytes),"language":"python","documents":["src/C.py"]}]});
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(root.join("python.json"), &bytes).unwrap();
        let digest = native_ids::digest(b"", &bytes);
        run(
            &home,
            &[
                "semantic",
                "import",
                "--workspace",
                root_string,
                "--scip",
                root.join("python.scip").to_str().unwrap(),
                "--manifest",
                root.join("python.json").to_str().unwrap(),
                "--profile",
                "mac-mini-m5-pro-v1",
                "--producer",
                "scip-python",
                "--manifest-sha256",
                &digest,
            ],
        )
    } else {
        imported
    };
    let prior: trellis::model::IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    let prior_semantic = store.semantic_at(prior).unwrap();
    if !matches!(change_b, "zero_claims" | "partial_empty_covered") {
        assert!(
            !prior_semantic.provenance.is_empty(),
            "fixture must select a proven anchor"
        );
    }
    let unchanged: trellis::model::IndexPin = serde_json::from_value(
        run(&home, &["index", "--workspace", root_string])["publishedRevision"].clone(),
    )
    .unwrap();
    assert_eq!(unchanged, prior);
    assert_eq!(
        store.semantic_at(prior).unwrap().provenance,
        prior_semantic.provenance
    );
    if matches!(change_b, "corrupt_capture" | "two_producers") {
        if change_b == "two_producers" {
            assert_eq!(store.selected_raw_inputs_at(prior).unwrap().len(), 2);
        }
        store.with_index_sql_for_tests(|db| {
            db.execute("UPDATE semantic_raw_captures SET binary_bytes=x'00' WHERE producer_id='scip-typescript'", [])?;
            Ok(())
        }).unwrap();
    }
    match change_b {
        "selected_private_stage" => (),
        "caller_failed" => {
            fs::write(root.join("src/C.js"), "function extra(){ return 1; }\n").unwrap()
        }
        "changed_a" => fs::write(
            root.join("src/A.js"),
            "function run(){ target(); return 3; }\n",
        )
        .unwrap(),
        "missing_b" => fs::write(root.join("src/B.js"), "function other(){ return 2; }\n").unwrap(),
        "new_document" => {
            fs::write(root.join("src/C.js"), "function extra(){ return 2; }\n").unwrap()
        }
        _ => fs::write(root.join("src/B.js"), "function target(){ return 2; }\n").unwrap(),
    }
    if matches!(change_b, "cancel_before" | "cancel_after") {
        let pid: i32 = fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let options = trellis::indexer::IndexOptions::new(root.clone());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let baseline = prior;
        assert_eq!(store.selected_raw_inputs_at(baseline).unwrap().len(), 1);
        let prior_native = store.retained_native_snapshot_at(baseline).unwrap();
        let prior_selected = store.selected_raw_inputs_at(baseline).unwrap();
        fs::write(root.join("src/B.js"), "function target(){ return 3; }\n").unwrap();
        let cancelled = cancel.clone();
        if change_b == "cancel_before" {
            store.set_publication_before_semantic_hook_for_tests(move || {
                cancelled.store(true, std::sync::atomic::Ordering::Release)
            });
        } else {
            store.set_publication_before_commit_hook_for_tests(move || {
                cancelled.store(true, std::sync::atomic::Ordering::Release)
            });
        }
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let leader = store.publication_for_tests().unwrap();
        let result =
            store.publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel);
        assert!(result.is_err(), "expected cancellation");
        assert!(
            cancel.load(std::sync::atomic::Ordering::Acquire),
            "cancellation hook was not reached: {result:?}"
        );
        assert_eq!(
            store.retained_native_snapshot_at(baseline).unwrap(),
            prior_native
        );
        assert_eq!(
            format!("{:?}", store.selected_raw_inputs_at(baseline).unwrap()),
            format!("{prior_selected:?}")
        );
        assert_eq!(store.semantic_at(baseline).unwrap(), prior_semantic);
        store
            .with_index_sql_for_tests(|db| {
                let revision: i64 = db.query_row(
                    "SELECT index_revision FROM index_metadata WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(revision, baseline.index_revision as i64);
                let newer: i64 = db.query_row(
                    "SELECT count(*) FROM native_revisions WHERE published_index_revision>?1",
                    [revision],
                    |r| r.get(0),
                )?;
                assert_eq!(newer, 0);
                let next_semantic: i64 = db.query_row(
                    "SELECT count(*) FROM semantic_coverage WHERE revision_id=?1",
                    [format!(
                        "pin:v1:{}:{}",
                        baseline.index_generation,
                        baseline.index_revision + 1
                    )],
                    |r| r.get(0),
                )?;
                assert_eq!(next_semantic, 0);
                Ok(())
            })
            .unwrap();
        return;
    }
    if change_b == "selected_private_stage" {
        let pid: i32 = fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        trellis::testing::wait_until("fixture daemon exit", || {
            !trellis::testing::process_running(pid)
        });
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let options = trellis::indexer::IndexOptions::new(root.clone());
        let capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let stage = store
            .publish_private_stage_native_for_tests(&options, &capture, &cancel)
            .unwrap();
        assert_ne!(stage.pin.index_generation, prior.index_generation);
        assert!(stage.semantic.coverage.is_empty());
        assert_eq!(store.index_baseline().unwrap(), prior);
        assert_eq!(store.semantic_at(prior).unwrap(), prior_semantic);
        assert_eq!(store.selected_raw_inputs_at(prior).unwrap().len(), 1);
        return;
    }
    if change_b == "compatible_direct" {
        let pid: i32 = fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        trellis::testing::wait_until("fixture daemon exit", || {
            !trellis::testing::process_running(pid)
        });
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let options = trellis::indexer::IndexOptions::new(root.clone());
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let leader = store.publication_for_tests().unwrap();
        let next = store
            .publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel)
            .unwrap();
        assert!(next.index_revision > prior.index_revision);
        let snapshot = store.semantic_at(next).unwrap();
        assert!(
            snapshot
                .provenance
                .iter()
                .any(|proof| proof.derived_from.is_some())
        );
        assert_eq!(store.selected_raw_inputs_at(next).unwrap().len(), 1);
        assert_eq!(store.semantic_at(prior).unwrap(), prior_semantic);
        return;
    }
    let index_args = if change_b == "full" {
        vec![
            "index",
            "--workspace",
            root_string,
            "--max-file-bytes",
            "3145728",
        ]
    } else {
        vec!["index", "--workspace", root_string]
    };
    let next: trellis::model::IndexPin =
        serde_json::from_value(run(&home, &index_args)["publishedRevision"].clone()).unwrap();
    assert!(next.index_revision > prior.index_revision);
    let snapshot = store.semantic_at(next).unwrap();
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|row| row.producer_id == "scip-typescript" && row.selected && row.requested),
        "{snapshot:?}"
    );
    if change_b.starts_with("partial_") {
        let a = snapshot
            .coverage
            .iter()
            .find(|row| row.document_path == "src/A.js")
            .unwrap();
        let b = snapshot
            .coverage
            .iter()
            .find(|row| row.document_path == "src/B.js")
            .unwrap();
        assert_eq!(a.state, trellis::semantic_evidence::CoverageState::Complete);
        assert_eq!(
            b.state,
            if matches!(change_b, "partial_unsupported" | "partial_failed") {
                trellis::semantic_evidence::CoverageState::Failed
            } else {
                trellis::semantic_evidence::CoverageState::NotRequested
            },
            "{change_b}: {b:?}"
        );
        assert!(
            snapshot
                .provenance
                .iter()
                .all(|proof| proof.document.path != "src/B.js")
        );
        if change_b == "partial_empty_covered" {
            assert!(snapshot.provenance.is_empty());
        }
    }
    if matches!(change_b, "corrupt_capture" | "two_producers") {
        let failed: Vec<_> = snapshot
            .coverage
            .iter()
            .filter(|row| row.producer_id == "scip-typescript" && row.requested)
            .collect();
        assert!(!failed.is_empty());
        assert!(failed.iter().all(|row| row.state
            == trellis::semantic_evidence::CoverageState::Failed
            && row.diagnostic.is_some()));
        assert!(
            snapshot
                .provenance
                .iter()
                .all(|proof| proof.producer_id != "scip-typescript")
        );
        if change_b == "two_producers" {
            let good: Vec<_> = snapshot
                .coverage
                .iter()
                .filter(|row| row.producer_id == "scip-python" && row.requested)
                .collect();
            assert!(!good.is_empty());
            assert!(
                good.iter()
                    .all(|row| row.state == trellis::semantic_evidence::CoverageState::Complete)
            );
            assert!(snapshot.provenance.iter().any(|proof|
                proof.producer_id == "scip-python" && proof.derived_from.is_some()));
        } else {
            assert!(snapshot.provenance.is_empty());
        }
    } else if !matches!(
        change_b,
        "zero_claims" | "changed_a" | "partial_empty_covered"
    ) {
        assert!(
            !snapshot.provenance.is_empty(),
            "native rejoin should preserve valid A evidence: {snapshot:?}"
        );
    }
    assert_eq!(
        store.selected_raw_inputs_at(next).unwrap().len(),
        if change_b == "two_producers" { 2 } else { 1 }
    );
    if matches!(change_b, "duplicate_call_claims" | "contradictory_calls") {
        let calls: Vec<_> = snapshot
            .call_bindings
            .iter()
            .filter(|call| call.join.anchor.document.path == "src/A.js")
            .collect();
        assert_eq!(calls.len(), 2, "{snapshot:?}");
        let distinct: std::collections::BTreeSet<_> = calls
            .iter()
            .map(|call| {
                let proof = snapshot
                    .provenance
                    .iter()
                    .find(|proof| proof.id == call.provenance_id)
                    .unwrap();
                proof.derived_from.as_ref().unwrap().provenance_id.as_str()
            })
            .collect();
        assert_eq!(distinct.len(), 2);
        assert_eq!(calls[0].join.anchor, calls[1].join.anchor);
        if change_b == "contradictory_calls" {
            assert!(calls.iter().all(|call| call.resolution
                == trellis::semantic_evidence::Resolution::Ambiguous
                && call.candidates.len() == 2));
        }
    }
    if change_b == "unmatched_both" {
        let selected = store.selected_raw_inputs_at(prior).unwrap();
        let unmatched: Vec<_> = selected[0]
            .proofs
            .iter()
            .filter(|(_, proof)| {
                serde_json::from_slice::<Value>(&proof.canonical_bytes).unwrap()["raw"]["symbol"]
                    == "unmatched0#"
            })
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(unmatched.len(), 1);
        assert!(snapshot.provenance.iter().all(|proof| {
            proof
                .derived_from
                .as_ref()
                .is_none_or(|raw| raw.provenance_id != unmatched[0])
        }));
        assert!(
            snapshot
                .coverage
                .iter()
                .filter(|row| row.document_path == "src/A.js")
                .all(|row| row.state == trellis::semantic_evidence::CoverageState::Complete),
            "{snapshot:?}"
        );
        assert!(
            snapshot
                .provenance
                .iter()
                .any(|proof| proof.document.path == "src/A.js" && proof.derived_from.is_some())
        );
    }
    if change_b == "caller_failed" {
        let caller = snapshot
            .coverage
            .iter()
            .find(|row| row.document_path == "src/A.js")
            .unwrap();
        let target = snapshot
            .coverage
            .iter()
            .find(|row| row.document_path == "src/B.js")
            .unwrap();
        assert_eq!(
            caller.state,
            trellis::semantic_evidence::CoverageState::Failed
        );
        assert_eq!(
            target.state,
            trellis::semantic_evidence::CoverageState::Complete
        );
        assert!(
            snapshot
                .provenance
                .iter()
                .all(|proof| proof.document.path != "src/A.js")
        );
        assert!(
            snapshot
                .provenance
                .iter()
                .any(|proof| proof.document.path == "src/B.js" && proof.derived_from.is_some())
        );
    }
    if change_b == "changed_a" {
        assert!(
            snapshot
                .provenance
                .iter()
                .all(|proof| proof.document.path != "src/A.js")
        );
        assert!(
            snapshot
                .provenance
                .iter()
                .any(|proof| proof.document.path == "src/B.js" && proof.derived_from.is_some())
        );
    }
    if change_b == "missing_b" {
        assert!(
            snapshot
                .provenance
                .iter()
                .any(|proof| proof.document.path == "src/A.js" && proof.derived_from.is_some())
        );
        assert!(
            snapshot
                .provenance
                .iter()
                .all(|proof| proof.document.path != "src/B.js")
        );
    }
    if change_b == "regular" {
        let (r2_native, _) = store.retained_native_snapshot_at(next).unwrap();
        let captured_target = native
            .declarations
            .iter()
            .find(|declaration| {
                declaration.document.path == "src/B.js"
                    && declaration.name.as_deref() == Some("target")
            })
            .unwrap();
        let current_target = r2_native
            .declarations
            .iter()
            .find(|declaration| {
                declaration.document.path == "src/B.js"
                    && declaration.name.as_deref() == Some("target")
            })
            .unwrap();
        assert_eq!(captured_target.syntax_id, current_target.syntax_id);
        let captured_proofs: std::collections::BTreeSet<_> = prior_semantic
            .provenance
            .iter()
            .filter_map(|proof| {
                proof
                    .derived_from
                    .as_ref()
                    .map(|raw| raw.provenance_id.as_str())
            })
            .collect();
        let derived: Vec<_> = snapshot
            .provenance
            .iter()
            .filter_map(|proof| proof.derived_from.as_ref().map(|raw| (proof, raw)))
            .collect();
        assert!(!derived.is_empty());
        for (proof, raw) in derived {
            assert!(captured_proofs.contains(raw.provenance_id.as_str()));
            assert_eq!(
                proof.revision_id,
                store
                    .retained_native_snapshot_at(next)
                    .unwrap()
                    .0
                    .revision
                    .id
            );
            assert_eq!(
                proof.freshness,
                trellis::semantic_evidence::Freshness::PossiblyStale
            );
        }
        assert!(
            snapshot
                .call_bindings
                .iter()
                .all(|binding| binding.stale_target != Some(true))
        );
    }
    if !matches!(
        change_b,
        "zero_claims" | "corrupt_capture" | "two_producers" | "changed_a" | "partial_empty_covered"
    ) {
        assert!(
            snapshot
                .provenance
                .iter()
                .any(|proof| proof.derived_from.is_some()),
            "{snapshot:?}"
        );
    }
}

#[test]
fn native_publication_rejoins_accepted_capture_and_unchanged_is_noop() {
    selected_case("regular");
}

#[test]
fn cancellation_before_and_after_semantic_staging_rolls_back_native_pin() {
    selected_case("cancel_before");
    selected_case("cancel_after");
}

#[test]
fn selected_capture_survives_zero_claims_and_new_document() {
    selected_case("zero_claims");
    selected_case("new_document");
}

#[test]
fn full_native_publication_rejoins_accepted_capture() {
    selected_case("full");
}

#[test]
fn native_publication_isolates_corrupt_selected_artifact() {
    selected_case("corrupt_capture");
}

#[test]
fn unmatched_at_both_native_revisions_skips_claim() {
    selected_case("unmatched_both");
}

#[test]
fn duplicate_call_claims_and_contradictory_targets_rejoin_on_native_route() {
    selected_case("duplicate_call_claims");
    selected_case("contradictory_calls");
}

#[test]
fn private_stage_excludes_prior_selected_capture() {
    selected_case("selected_private_stage");
}

#[test]
fn compatible_recovery_baseline_rejoins_selected_capture() {
    selected_case("compatible_direct");
}

#[test]
fn failed_caller_capture_keeps_independent_target_on_native_route() {
    selected_case("caller_failed");
}

#[test]
fn partial_capture_exclusions_survive_rejoin() {
    for scenario in [
        "partial_not_requested",
        "partial_unsupported",
        "partial_failed",
        "partial_empty_covered",
    ] {
        selected_case(scenario);
    }
}

#[test]
fn bad_selected_artifact_does_not_block_other_selected_producer() {
    selected_case("two_producers");
}

#[test]
fn changed_caller_and_missing_target_are_rejoined_from_captured_r1() {
    selected_case("changed_a");
    selected_case("missing_b");
}
