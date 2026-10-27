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
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("daemon.pid"));
    if home.join("full-oracle").exists() {
        cmd.env("TRELLIS_TEST_LOCAL_FULL_ORACLE", "1");
        cmd.env(
            "TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE",
            home.join("work-counts.json"),
        );
    }
    if home.join("force-full").exists() {
        cmd.env("TRELLIS_TEST_FORCE_FULL_REJOIN", "1");
    }
    if home.join("work-counts").exists() {
        cmd.env(
            "TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE",
            home.join("work-counts.json"),
        );
    }
    cmd.env_remove("XDG_CACHE_HOME")
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
fn copy_seed(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    fs::set_permissions(to, fs::metadata(from).unwrap().permissions()).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_seed(&entry.path(), &target);
        } else if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn selected_case(change_b: &str) {
    use protobuf::{EnumOrUnknown, Message};
    let add_unrequested_document = matches!(
        change_b,
        "corrupt_capture_new_document" | "two_producers_new_document"
    );
    let change_b = match change_b {
        "corrupt_capture_new_document" => "corrupt_capture",
        "two_producers_new_document" => "two_producers",
        other => other,
    };
    let temp = Builder::new()
        .prefix("semantic-two-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    if change_b.starts_with("oracle_") {
        fs::write(home.join("full-oracle"), b"1").unwrap();
    }
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join(".gitignore"), "artifact.scip\nmanifest.json\n").unwrap();
    let a = "function run(){ target(); }\n";
    let b = "function target(){ return 1; }\n";
    fs::write(root.join("src/A.js"), a).unwrap();
    fs::write(root.join("src/B.js"), b).unwrap();
    if matches!(
        change_b,
        "two_producers"
            | "oracle_two_producers"
            | "oracle_fork_two_producers"
            | "oracle_fork_healthy_two_producers"
    ) {
        fs::write(root.join("src/C.py"), "def helper(): return 1\n").unwrap();
    }
    let root = fs::canonicalize(root).unwrap();
    let root_string = root.to_str().unwrap();
    let mut private_stage_daemon = if change_b == "selected_private_stage" {
        let mut child = cli(&home)
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        trellis::testing::wait_until("private-stage daemon election", || {
            assert!(
                child.try_wait().unwrap().is_none(),
                "fixture daemon exited before election"
            );
            fs::read_to_string(home.join("daemon.pid"))
                .is_ok_and(|marker| marker == child.id().to_string())
        });
        Some(child)
    } else {
        None
    };
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
    if matches!(
        change_b,
        "unmatched_both" | "oracle_unmatched_both" | "oracle_fork_unmatched_both"
    ) {
        let mut claim = scip::types::Occurrence::new();
        claim.range = vec![0, 0, 8];
        claim.symbol = "unmatched0#".into();
        claim.symbol_roles = 0;
        binary.documents[0].occurrences.push(claim);
    }
    if matches!(
        change_b,
        "duplicate_call_claims"
            | "contradictory_calls"
            | "oracle_duplicate_call_claims"
            | "oracle_contradictory_calls"
            | "oracle_fork_duplicate_call_claims"
            | "oracle_fork_contradictory_calls"
    ) {
        let mut repeated = binary.documents[0]
            .occurrences
            .iter()
            .find(|occurrence| occurrence.symbol == "pkg target.")
            .unwrap()
            .clone();
        if matches!(
            change_b,
            "contradictory_calls"
                | "oracle_contradictory_calls"
                | "oracle_fork_contradictory_calls"
        ) {
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
        "partial_unsupported"
        | "oracle_partial_unsupported"
        | "oracle_fork_partial_unsupported" => Some("unsupported"),
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
    let imported = if matches!(
        change_b,
        "two_producers"
            | "oracle_two_producers"
            | "oracle_fork_two_producers"
            | "oracle_fork_healthy_two_producers"
    ) {
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
    let mut prior: trellis::model::IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    let mut prior_semantic = store.semantic_at(prior).unwrap();
    let accepted_artifact_hash = store.selected_raw_inputs_at(prior).unwrap()[0]
        .artifact_hash
        .clone();
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
    if change_b == "oracle_fork_successive_local" {
        fs::write(root.join("src/B.js"), "function target(){ return 2; }\n").unwrap();
        prior = serde_json::from_value(
            run(&home, &["index", "--workspace", root_string])["publishedRevision"].clone(),
        )
        .unwrap();
        prior_semantic = store.semantic_at(prior).unwrap();
        assert!(
            prior_semantic
                .provenance
                .iter()
                .any(|proof| proof.document.path == "src/A.js" && proof.derived_from.is_some()),
            "first LOCAL did not preserve selected r1 evidence: {prior_semantic:?}"
        );
    }
    let (fork_home, store) = if change_b.starts_with("oracle_fork_") {
        let pid: i32 = fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        trellis::testing::wait_until("seed daemon exit", || {
            !trellis::testing::process_running(pid)
        });
        if matches!(
            change_b,
            "oracle_fork_restored_b" | "oracle_fork_retained_target"
        ) {
            fs::write(root.join("src/B.js"), "function target(){ return 2; }\n").unwrap();
        }
        if change_b == "oracle_fork_successive_local" {
            fs::write(root.join("src/B.js"), "function target(){ return 3; }\n").unwrap();
        }
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let options = trellis::indexer::IndexOptions::new(root.clone());
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let leader = store.publication_for_tests().unwrap();
        let seeded = store
            .publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel)
            .unwrap();
        assert!(seeded.index_revision > prior.index_revision);
        let store = if change_b == "oracle_fork_raw_replacement" {
            let mut extra = scip::types::Occurrence::new();
            extra.range = vec![0, 0, 8];
            extra.symbol = "unmatched0#".into();
            extra.symbol_roles = 0;
            binary.documents[0].occurrences.push(extra);
            let bytes = binary.write_to_bytes().unwrap();
            fs::write(root.join("artifact.scip"), &bytes).unwrap();
            drop(store);
            let stable = run(&home, &["index", "--workspace", root_string]);
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
            let stabilized = trellis::store::Store::open(roots, identity).unwrap();
            let stable_pin: trellis::model::IndexPin =
                serde_json::from_value(stable["publishedRevision"].clone()).unwrap();
            let (actual, _) = stabilized.retained_native_snapshot_at(stable_pin).unwrap();
            let mut replacement = manifest.clone();
            replacement["artifacts"][0]["hash"] = json!(native_ids::digest(b"", &bytes));
            replacement["sourceRevision"] = json!(actual.revision.id);
            replacement["sourceManifestHash"] = json!(native_ids::digest(
                b"",
                &native_ids::canonical(&json!(
                    actual
                        .revision
                        .documents
                        .iter()
                        .map(|d| json!({"document":d.key,"contentHash":d.content_hash}))
                        .collect::<Vec<_>>()
                ))
            ));
            replacement["components"] = json!({
                "toolchainHash":actual.revision.toolchain_hash,
                "configHash":actual.revision.config_hash,
                "dependencyHash":actual.revision.dependency_hash,
            });
            let bytes = serde_json::to_vec(&replacement).unwrap();
            fs::write(root.join("manifest.json"), &bytes).unwrap();
            let digest = native_ids::digest(b"", &bytes);
            drop(stabilized);
            let imported = run(
                &home,
                &[
                    "semantic",
                    "import",
                    "--workspace",
                    root_string,
                    "--scip",
                    root.join("artifact.scip").to_str().unwrap(),
                    "--manifest",
                    root.join("manifest.json").to_str().unwrap(),
                    "--profile",
                    "mac-mini-m5-pro-v1",
                    "--producer",
                    "scip-typescript",
                    "--manifest-sha256",
                    &digest,
                ],
            );
            prior = serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
            let pid: i32 = fs::read_to_string(home.join("daemon.pid"))
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
            trellis::testing::wait_until("replacement daemon exit", || {
                !trellis::testing::process_running(pid)
            });
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
            trellis::store::Store::open(roots, identity).unwrap()
        } else {
            prior = seeded;
            store
        };
        prior_semantic = store.semantic_at(prior).unwrap();
        if change_b == "oracle_fork_raw_replacement" {
            assert_ne!(
                store.selected_raw_inputs_at(prior).unwrap()[0].artifact_hash,
                accepted_artifact_hash,
                "the replacement must select a distinct authenticated raw artifact",
            );
        }
        assert!(
            prior_semantic
                .provenance
                .iter()
                .any(|proof| proof.derived_from.is_some())
        );
        let fork = temp.path().join("fork-home");
        copy_seed(&home, &fork);
        fs::remove_file(fork.join("full-oracle")).unwrap();
        fs::write(fork.join("force-full"), b"1").unwrap();
        // Compare production LOCAL output with the independent FULL fork. An
        // in-process oracle must not change the branch being compared.
        fs::remove_file(home.join("full-oracle")).unwrap();
        fs::write(home.join("work-counts"), b"1").unwrap();
        drop(store);
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
        (
            Some(fork),
            trellis::store::Store::open(roots, identity).unwrap(),
        )
    } else {
        (None, store)
    };
    if change_b == "oracle_fork_missing_raw_key" {
        let capture_id = store.selected_raw_inputs_at(prior).unwrap()[0].capture_id;
        store
            .with_index_sql_for_tests(|db| {
                assert_eq!(db.execute(
                "DELETE FROM semantic_raw_documents WHERE capture_id=?1 AND path='src/A.js'",
                [capture_id],
            )?,1);
                Ok(())
            })
            .unwrap();
        if let Some(fork) = &fork_home {
            copy_seed(&home, fork);
            let _ = fs::remove_file(fork.join("full-oracle"));
        }
    }
    if change_b == "retained_native_header_corruption" {
        let pid: i32 = fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        trellis::testing::wait_until("fixture daemon exit", || {
            !trellis::testing::process_running(pid)
        });

        let raw = store.selected_raw_inputs_at(prior).unwrap().remove(0);
        let original_key = format!(
            "pin:v1:{}:{}",
            raw.captured_pin.index_generation, raw.captured_pin.index_revision
        );
        let original_vid: i64 = store
            .with_index_sql_for_tests(|db| {
                Ok(db.query_row(
                    "SELECT vid FROM revision_documents WHERE revision_id=?1 AND path='src/B.js'",
                    [&original_key],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        let options = trellis::indexer::IndexOptions::new(root.clone());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let leader = store.publication_for_tests().unwrap();

        // First make the captured B version historical, so current-version
        // publication preflight cannot discover the corrupt row by coincidence.
        fs::write(root.join("src/B.js"), "function target(){ return 2; }\n").unwrap();
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let clean_head = store
            .publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel)
            .unwrap();
        assert!(clean_head.index_revision > prior.index_revision);
        assert!(
            store
                .semantic_at(clean_head)
                .unwrap()
                .provenance
                .iter()
                .any(|p| p.derived_from.is_some())
        );

        store
            .with_index_sql_for_tests(|db| {
                let current_vid: i64 = db.query_row(
                    "SELECT vid FROM revision_documents WHERE revision_id=?1 AND path='src/B.js'",
                    [format!(
                        "pin:v1:{}:{}",
                        clean_head.index_generation, clean_head.index_revision
                    )],
                    |row| row.get(0),
                )?;
                assert_ne!(current_vid, original_vid);
                let children: i64 = db.query_row(
                    "SELECT (SELECT count(*) FROM native_version_header_items WHERE vid=?1)
                      + (SELECT count(*) FROM native_version_parameters WHERE vid=?1)",
                    [original_vid],
                    |row| row.get(0),
                )?;
                assert_eq!(children, 0, "simple B header must have no child rows");
                // No foreign-key disabling: headers refer to declarations, but
                // declarations do not refer back to their required header.
                assert_eq!(
                    db.execute(
                        "DELETE FROM native_version_headers WHERE vid=?1 AND name='target'",
                        [original_vid],
                    )?,
                    1
                );
                Ok(())
            })
            .unwrap();

        fs::write(root.join("src/B.js"), "function target(){ return 3; }\n").unwrap();
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let result =
            store.publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel);
        let error = result
            .expect_err("native retained-row corruption must not publish failed producer output");
        assert!(
            format!("{error:#}").contains("missing native header"),
            "{error:#}"
        );
        store.with_index_sql_for_tests(|db| {
            let head: i64 = db.query_row(
                "SELECT index_revision FROM index_metadata WHERE singleton=1", [], |row| row.get(0),
            )?;
            assert_eq!(head, clean_head.index_revision as i64);
            let newer: i64 = db.query_row(
                "SELECT count(*) FROM native_revisions WHERE published_index_revision>?1",
                [clean_head.index_revision as i64], |row| row.get(0),
            )?;
            let failed: i64 = db.query_row(
                "SELECT count(*) FROM semantic_coverage WHERE revision_id=?1 AND state='failed'",
                [format!("pin:v1:{}:{}", clean_head.index_generation, clean_head.index_revision + 1)],
                |row| row.get(0),
            )?;
            assert_eq!((newer, failed), (0, 0));
            Ok(())
        }).unwrap();
        return;
    }
    if change_b == "oracle_fork_healthy_two_producers" {
        let selected = store.selected_raw_inputs_at(prior).unwrap();
        assert_eq!(selected.len(), 2);
        assert!(selected.iter().all(|input| !input.proofs.is_empty()));
    }
    if matches!(
        change_b,
        "corrupt_capture" | "two_producers" | "oracle_two_producers" | "oracle_fork_two_producers"
    ) {
        if change_b != "corrupt_capture" {
            assert_eq!(store.selected_raw_inputs_at(prior).unwrap().len(), 2);
        }
        store.with_index_sql_for_tests(|db| {
            db.execute("UPDATE semantic_raw_captures SET binary_bytes=x'00' WHERE producer_id='scip-typescript'", [])?;
            Ok(())
        }).unwrap();
        if let Some(fork) = &fork_home {
            copy_seed(&home, fork);
            assert!(!fork.join("full-oracle").exists());
        }
    }
    if matches!(change_b, "oracle_retained_target" | "oracle_restored_b") {
        fs::write(root.join("src/B.js"), "function target(){ return 2; }\n").unwrap();
        prior = serde_json::from_value(
            run(&home, &["index", "--workspace", root_string])["publishedRevision"].clone(),
        )
        .unwrap();
        prior_semantic = store.semantic_at(prior).unwrap();
    }
    if change_b == "oracle_mismatch" {
        use std::collections::BTreeMap;
        use trellis::{scip_admission, semantic_evidence as semantic};
        let pid: i32 = fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        trellis::testing::wait_until("fixture daemon exit", || {
            !trellis::testing::process_running(pid)
        });
        drop(store);
        // Seed a fresh generation with the same direct extractor that will
        // publish LOCAL, rather than inheriting the CLI extractor's identity.
        let state = tempfile::tempdir_in(temp.path()).unwrap();
        let store = trellis::store::Store::open_for_tests(state.path(), &root).unwrap();
        let options = trellis::indexer::IndexOptions::new(root.clone());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let leader = store.publication_for_tests().unwrap();
        let selected = store
            .publish_native(
                &graph,
                &capture,
                &native,
                &leader,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let (r1, sources) = store.retained_native_snapshot_at(selected).unwrap();
        let (toolchain, config, dependencies) =
            store.retained_native_components_at(selected).unwrap();
        let mut direct_manifest = manifest.clone();
        direct_manifest["sourceSet"] = serde_json::to_value(&r1.source_set).unwrap();
        direct_manifest["sourceRevision"] = json!(r1.revision.id);
        direct_manifest["sourceManifestHash"] = json!(native_ids::digest(
            b"",
            &native_ids::canonical(&json!(
                r1.revision
                    .documents
                    .iter()
                    .map(|doc| { json!({"document":doc.key,"contentHash":doc.content_hash}) })
                    .collect::<Vec<_>>()
            ))
        ));
        direct_manifest["components"] = json!({
            "toolchainHash":r1.revision.toolchain_hash,
            "configHash":r1.revision.config_hash,
            "dependencyHash":r1.revision.dependency_hash,
        });
        let documents = r1.revision.documents.iter().map(|doc| {
            json!({"language":doc.key.language,"relativePath":doc.key.path,"contentHash":doc.content_hash})
        }).collect::<Vec<_>>();
        direct_manifest["sourceDocuments"] = json!(documents);
        direct_manifest["documents"] = json!(documents);
        let manifest_bytes = serde_json::to_vec(&direct_manifest).unwrap();
        fs::write(&manifest_path, &manifest_bytes).unwrap();
        let binary_bytes = fs::read(&artifact_path).unwrap();
        let selection = scip_admission::TrustedSelection {
            expected_manifest_hash: native_ids::digest(b"", &manifest_bytes),
            profile_id: "mac-mini-m5-pro-v1".into(),
            producer_name: "scip-typescript".into(),
        };
        let admission = scip_admission::ReferenceProfileAdmission::from_retained_native(
            &root,
            &r1,
            &sources,
            &artifact_path,
            &binary_bytes,
            &manifest_path,
            &manifest_bytes,
            &selection,
            &cancel,
        )
        .unwrap();
        let producer = admission.producer();
        let inventory = scip_admission::decode_reference_profile(&admission, &cancel).unwrap();
        let basis = r1
            .revision
            .documents
            .iter()
            .map(|doc| {
                (
                    doc.key.clone(),
                    semantic::SemanticBasis {
                        producer_id: producer.id.clone(),
                        producer_version: producer.version.clone(),
                        producer_hash: producer.executable_hash.clone(),
                        artifact_hash: inventory.artifact_hash.clone(),
                        language: semantic::Language::Javascript,
                        source_set_id: doc.key.source_set_id.clone(),
                        revision_id: r1.revision.id.clone(),
                        source_manifest_hash: inventory.source_manifest_hash.clone(),
                        toolchain_hash: r1.revision.toolchain_hash.clone(),
                        config_hash: r1.revision.config_hash.clone(),
                        dependency_hash: r1.revision.dependency_hash.clone(),
                        lookup_dependencies: vec![],
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let joined = trellis::semantic_import::rejoin_inventory(
            &inventory,
            &r1,
            &sources,
            &r1,
            &sources,
            producer.clone(),
            &basis,
            admission.admitted_encoding(),
        )
        .unwrap();
        let captured = semantic::CapturedComponents {
            producer_executables: BTreeMap::new(),
            artifacts: BTreeMap::from([(producer.id.clone(), binary_bytes.clone())]),
            sources,
            captured_revision: r1.revision.clone(),
            native_toolchain: toolchain.clone(),
            native_config: config.clone(),
            native_dependencies: dependencies.clone(),
            requested_producers: vec![producer.clone()],
            toolchain: native_ids::canonical(&toolchain),
            config: native_ids::canonical(&json!(config)),
            dependency: native_ids::canonical(&json!(dependencies)),
            native_extraction_components: BTreeMap::new(),
        };
        let raw = trellis::store::SemanticRawCapture {
            producer_id: producer.id,
            source_set_id: r1.revision.source_set_id.clone(),
            captured_revision: r1.revision.id.clone(),
            binary_bytes,
            manifest_bytes,
            inventory,
        };
        let document_hashes = r1
            .revision
            .documents
            .iter()
            .map(|doc| {
                (
                    (doc.key.language.clone(), doc.key.path.clone()),
                    doc.content_hash.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let prior = store
            .publish_semantic_import(
                selected,
                &document_hashes,
                &joined.publication,
                &captured,
                &r1,
                &raw,
                &joined.report,
                &leader,
                &cancel,
            )
            .unwrap();
        let prior_semantic = store.semantic_at(prior).unwrap();
        assert!(!prior_semantic.provenance.is_empty());
        assert!(
            prior_semantic
                .provenance
                .iter()
                .all(|proof| proof.derived_from.is_some())
        );
        for path in ["src/A.js", "src/B.js"] {
            assert!(
                prior_semantic.coverage.iter().any(|row| {
                    row.document_path == path
                        && row.selected
                        && row.requested
                        && row.state == semantic::CoverageState::Complete
                }),
                "direct admission must select complete evidence for {path}: {prior_semantic:?}"
            );
        }
        assert_eq!(store.index_baseline().unwrap(), prior);
        assert_eq!(store.selected_raw_inputs_at(prior).unwrap().len(), 1);
        fs::write(root.join("src/B.js"), "function target(){ return 2; }\n").unwrap();
        let capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let error = store
            .publish_local_oracle_mismatch_for_tests(&options, &capture, &leader, &cancel)
            .expect_err("oracle mismatch must abort publication");
        assert!(
            format!("{error:#}")
                .contains("LOCAL publication differs from independent FULL rejoin oracle"),
            "{error:#}"
        );
        assert_eq!(store.index_baseline().unwrap(), prior);
        assert_eq!(store.semantic_at(prior).unwrap(), prior_semantic);
        store
            .with_index_sql_for_tests(|db| {
                let newer: i64 = db.query_row(
                    "SELECT count(*) FROM native_revisions WHERE published_index_revision>?1",
                    [prior.index_revision as i64],
                    |row| row.get(0),
                )?;
                let failed: i64 = db.query_row(
                    "SELECT count(*) FROM semantic_coverage WHERE revision_id=?1 AND state='failed'",
                    [format!("pin:v1:{}:{}", prior.index_generation, prior.index_revision + 1)],
                    |row| row.get(0),
                )?;
                assert_eq!((newer, failed), (0, 0));
                Ok(())
            })
            .unwrap();
        return;
    }
    let fork_scenario = change_b
        .strip_prefix("oracle_fork_")
        .map(|name| format!("oracle_{name}"));
    match fork_scenario.as_deref().unwrap_or(change_b) {
        "selected_private_stage" => (),
        "oracle_retained_target" => {
            fs::write(root.join("src/C.js"), "function extra(){ return 2; }\n").unwrap()
        }
        "oracle_restored_b" => fs::write(root.join("src/B.js"), b).unwrap(),
        "oracle_successive_local" => {
            fs::write(root.join("src/B.js"), "function target(){ return 4; }\n").unwrap()
        }
        "oracle_deleted_a" => fs::remove_file(root.join("src/A.js")).unwrap(),
        "oracle_deleted_b" => fs::remove_file(root.join("src/B.js")).unwrap(),
        "oracle_moved_a" => fs::rename(root.join("src/A.js"), root.join("src/D.js")).unwrap(),
        "oracle_moved_b" => fs::rename(root.join("src/B.js"), root.join("src/D.js")).unwrap(),
        "oracle_ambiguous_b" => fs::write(
            root.join("src/B.js"),
            "function target(){ return 1; }\nfunction target(){ return 2; }\n",
        )
        .unwrap(),
        "caller_failed" => {
            fs::write(root.join("src/C.js"), "function extra(){ return 1; }\n").unwrap()
        }
        "changed_a" | "oracle_changed_a" => fs::write(
            root.join("src/A.js"),
            "function run(){ target(); return 3; }\n",
        )
        .unwrap(),
        "missing_b" | "oracle_missing_b" => {
            fs::write(root.join("src/B.js"), "function other(){ return 2; }\n").unwrap()
        }
        "new_document" => {
            fs::write(root.join("src/C.js"), "function extra(){ return 2; }\n").unwrap()
        }
        _ => fs::write(root.join("src/B.js"), "function target(){ return 2; }\n").unwrap(),
    }
    if add_unrequested_document {
        fs::write(root.join("src/C.js"), "function extra(){ return 2; }\n").unwrap();
    }
    if change_b == "corrupt_rebaseline" {
        let pid: i32 = fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        trellis::testing::wait_until("fixture daemon exit", || {
            !trellis::testing::process_running(pid)
        });
        store
            .with_index_sql_for_tests(|db| {
                db.execute("UPDATE index_metadata SET stats='not-json'", [])?;
                Ok(())
            })
            .unwrap();
        drop(store);
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
        let rebaselined = trellis::store::Store::open(roots, identity).unwrap();
        let options = trellis::indexer::IndexOptions::new(root.clone());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (next, session) = trellis::index_coordinator::reconcile_workspace(
            &rebaselined,
            &options,
            &cancel,
            |_| {},
        )
        .unwrap();
        session.verify().unwrap();
        assert_ne!(next.index_generation, prior.index_generation);
        assert!(
            rebaselined
                .retained_native_snapshot_at(next)
                .unwrap()
                .0
                .revision
                .documents
                .len()
                >= 2
        );
        assert!(rebaselined.semantic_at(next).unwrap().coverage.is_empty());
        assert!(rebaselined.selected_raw_inputs_at(next).unwrap().is_empty());
        rebaselined
            .with_index_sql_for_tests(|db| {
                let selections: i64 = db.query_row(
                    "SELECT count(*) FROM semantic_revision_raw_selections",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(selections, 0);
                Ok(())
            })
            .unwrap();
        return;
    }
    if change_b == "bad_candidate" {
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
        let (graph, mut native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        native.revision.documents[0].content_hash = "bad-candidate-hash".into();
        let leader = store.publication_for_tests().unwrap();
        assert!(
            store
                .publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel)
                .is_err()
        );
        assert_eq!(store.index_baseline().unwrap(), prior);
        assert_eq!(store.semantic_at(prior).unwrap(), prior_semantic);
        store.with_index_sql_for_tests(|db| {
            let next: i64 = db.query_row("SELECT count(*) FROM native_revisions WHERE published_index_revision>?1", [prior.index_revision as i64], |row| row.get(0))?;
            let failed: i64 = db.query_row("SELECT count(*) FROM semantic_coverage WHERE revision_id=?1 AND state='failed'", [format!("pin:v1:{}:{}", prior.index_generation, prior.index_revision+1)], |row| row.get(0))?;
            assert_eq!((next, failed), (0, 0));
            Ok(())
        }).unwrap();
        return;
    }
    if change_b == "structural_corruption" {
        let pid: i32 = fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        trellis::testing::wait_until("fixture daemon exit", || {
            !trellis::testing::process_running(pid)
        });
        store.with_index_sql_for_tests(|db| {
            db.pragma_update(None, "foreign_keys", "OFF")?;
            db.execute("DELETE FROM semantic_requested_producers WHERE revision_id=?1 AND id='scip-typescript'", [format!("pin:v1:{}:{}", prior.index_generation, prior.index_revision)])?;
            db.pragma_update(None, "foreign_keys", "ON")?;
            Ok(())
        }).unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let options = trellis::indexer::IndexOptions::new(root.clone());
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let leader = store.publication_for_tests().unwrap();
        let error = store
            .publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel)
            .unwrap_err();
        assert!(
            error.to_string().contains("incompatible_index"),
            "{error:#}"
        );
        store.with_index_sql_for_tests(|db| {
            let revision: i64 = db.query_row("SELECT index_revision FROM index_metadata", [], |row| row.get(0))?;
            assert_eq!(revision, prior.index_revision as i64);
            let failed: i64 = db.query_row("SELECT count(*) FROM semantic_coverage WHERE revision_id=?1 AND state='failed'", [format!("pin:v1:{}:{}", prior.index_generation, prior.index_revision + 1)], |row| row.get(0))?;
            assert_eq!(failed, 0);
            Ok(())
        }).unwrap();
        return;
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
        let search_snapshot = |db: &rusqlite::Connection| {
            let cards = db.prepare(
                "SELECT vid,syntax_id,card_text,card_hash,facts_json,terms FROM native_version_search_cards ORDER BY vid,syntax_id",
            )?.query_map([], |row| Ok((
                row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?, row.get::<_, String>(3)?,
                row.get::<_, String>(4)?, row.get::<_, String>(5)?,
            )))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let fts = db
                .prepare("SELECT rowid,vid,syntax_id,terms FROM head_search_fts ORDER BY rowid")?
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, anyhow::Error>((cards, fts))
        };
        let prior_search = (change_b == "cancel_after")
            .then(|| store.with_index_sql_for_tests(search_snapshot).unwrap());
        let changed_b = if change_b == "cancel_after" {
            "function target(){ return \"rollback orchard\"; }\n"
        } else {
            "function target(){ return 3; }\n"
        };
        fs::write(root.join("src/B.js"), changed_b).unwrap();
        let cancelled = cancel.clone();
        if change_b == "cancel_before" {
            store.set_publication_before_semantic_hook_for_tests(move || {
                cancelled.store(true, std::sync::atomic::Ordering::Release)
            });
        } else {
            let audit = store.clone();
            store.set_publication_before_commit_hook_for_tests(move || {
                let writes = audit.take_search_fts_writes_for_tests();
                assert!(
                    writes
                        .iter()
                        .any(|(operation, _)| *operation == rusqlite::ffi::SQLITE_DELETE)
                );
                assert!(
                    writes
                        .iter()
                        .any(|(operation, _)| *operation == rusqlite::ffi::SQLITE_INSERT)
                );
                cancelled.store(true, std::sync::atomic::Ordering::Release)
            });
        }
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        if change_b == "cancel_after" {
            let changed = native
                .search_cards
                .iter()
                .find(|card| card.path == "src/B.js")
                .unwrap();
            let previous = prior_native
                .0
                .search_cards
                .iter()
                .find(|card| card.syntax_id == changed.syntax_id)
                .unwrap();
            assert_ne!(changed.card.text, previous.card.text);
        }
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
        if let Some(prior_search) = prior_search {
            assert_eq!(
                store.with_index_sql_for_tests(search_snapshot).unwrap(),
                prior_search
            );
        }
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
        let mut child = private_stage_daemon.take().unwrap();
        assert_eq!(
            fs::read_to_string(home.join("daemon.pid")).unwrap(),
            child.id().to_string(),
            "CLI commands must have used the elected fixture daemon",
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "fixture daemon exited early"
        );
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        child.wait().unwrap();
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
    let index_args = if matches!(change_b, "full" | "oracle_fork_context") {
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
    if change_b.starts_with("oracle_") {
        assert!(
            snapshot.coverage.iter().all(|row| row
                .diagnostic
                .as_deref()
                .is_none_or(|diagnostic| !diagnostic
                    .contains("differs from independent FULL rejoin oracle"))),
            "{change_b}: {snapshot:?}"
        );
    }
    if change_b.starts_with("oracle_fork_") {
        let work: Value =
            serde_json::from_slice(&fs::read(home.join("work-counts.json")).unwrap()).unwrap();
        let (witnessed, fallbacks) = match change_b {
            "oracle_fork_body"
            | "oracle_fork_changed_a"
            | "oracle_fork_ambiguous_b"
            | "oracle_fork_partial_unsupported"
            | "oracle_fork_successive_local" => (1, 0),
            "oracle_fork_healthy_two_producers" => (2, 0),
            "oracle_fork_missing_b"
            | "oracle_fork_deleted_a"
            | "oracle_fork_deleted_b"
            | "oracle_fork_moved_a"
            | "oracle_fork_moved_b"
            | "oracle_fork_restored_b"
            | "oracle_fork_retained_target"
            | "oracle_fork_unmatched_both"
            | "oracle_fork_duplicate_call_claims"
            | "oracle_fork_contradictory_calls"
            | "oracle_fork_two_producers"
            | "oracle_fork_missing_raw_key"
            | "oracle_fork_raw_replacement" => (0, 1),
            "oracle_fork_context" => (0, 0),
            _ => panic!("missing branch expectation for {change_b}"),
        };
        assert_eq!(work["witnessedRejoins"], witnessed, "{change_b}: {work}");
        assert_eq!(work["fullFallbacks"], fallbacks, "{change_b}: {work}");
        let reasons = work["fallbackReasons"].as_array().unwrap();
        assert_eq!(reasons.is_empty(), fallbacks == 0, "{change_b}: {work}");
        if change_b == "oracle_fork_restored_b" {
            assert!(
                reasons
                    .iter()
                    .any(|reason| reason == "restored_document_requires_measurement"),
                "restoration must request measured rejoin, not fail its source: {work}"
            );
            assert!(
                snapshot
                    .coverage
                    .iter()
                    .any(|row| row.document_path == "src/B.js"
                        && row.selected
                        && row.state == trellis::semantic_evidence::CoverageState::Complete),
                "restored B must regain selected complete evidence: {snapshot:?}"
            );
            assert!(
                snapshot
                    .provenance
                    .iter()
                    .any(|proof| proof.document.path == "src/B.js" && proof.derived_from.is_some()),
                "restored B must regain raw-linked proofs: {snapshot:?}"
            );
        }
        if witnessed > 0 {
            assert_eq!(work["destinationIndexBuilds"], 1, "{change_b}: {work}");
            assert_eq!(work["destinationDocumentRows"], 1, "{change_b}: {work}");
            assert_eq!(
                work["destinationJoinsOnReusedDocuments"], 0,
                "{change_b}: {work}"
            );
            assert_eq!(work["destinationRoleParses"], 0, "{change_b}: {work}");
            assert_eq!(work["originalRoleParses"], 0, "{change_b}: {work}");
            assert_eq!(work["originalRawDecodes"], 0, "{change_b}: {work}");
        } else {
            assert!(
                work["originalRawDecodes"].as_u64().unwrap() > 0,
                "{change_b}: {work}"
            );
        }
    }
    if change_b == "oracle_fork_context" {
        let work: Value =
            serde_json::from_slice(&fs::read(home.join("work-counts.json")).unwrap()).unwrap();
        eprintln!("context invalidation publication work: {work}");
        assert_eq!(work["destinationIndexBuilds"], 1, "{work}");
        assert_eq!(
            work["destinationDocumentRows"], 2,
            "native FULL must index both documents: {work}"
        );
        assert_eq!(
            work["destinationRoleParses"], 2,
            "native FULL must rejoin both source claims: {work}"
        );
        assert_eq!(
            work["fullFallbacks"], 0,
            "native FULL does not enter the LOCAL fallback: {work}"
        );
    }
    if change_b == "oracle_fork_successive_local" {
        let work: Value =
            serde_json::from_slice(&fs::read(home.join("work-counts.json")).unwrap()).unwrap();
        assert_eq!(
            work["fullFallbacks"], 0,
            "second LOCAL must witness the earliest original association: {work}"
        );
        assert_eq!(work["destinationIndexBuilds"], 1, "{work}");
        assert_eq!(work["destinationDocumentRows"], 1, "{work}");
        assert_eq!(work["destinationRoleParses"], 0, "{work}");
        assert_eq!(work["originalRawProofBytes"], 0, "{work}");
        assert_eq!(work["originalRawDecodes"], 0, "{work}");
        assert!(
            work["parsedRoleSiteDocuments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert!(
            work["parsedTypeDocuments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert!(
            work["originalNativeDocuments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert!(
            work["originalSourceDocuments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert!(work["currentTargetRows"].as_u64().unwrap() >= 1, "{work}");
        assert!(
            work["originalSourceHashDocuments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert_eq!(
            work["indexPin"],
            serde_json::to_value(next).unwrap(),
            "{work}"
        );
        let (current_native, _) = store.retained_native_snapshot_at(next).unwrap();
        assert_eq!(
            work["destinationRevision"], current_native.revision.id,
            "{work}"
        );
        assert!(
            snapshot
                .call_bindings
                .iter()
                .any(|call| matches!(&call.declared_target,
            Some(trellis::semantic_evidence::Target::Internal {revision_id,..})
                if revision_id==&current_native.revision.id)),
            "second LOCAL must decide the latest target revision: {snapshot:?}"
        );
    }
    if change_b == "oracle_fork_missing_raw_key" {
        let work: Value =
            serde_json::from_slice(&fs::read(home.join("work-counts.json")).unwrap()).unwrap();
        assert_eq!(work["fullFallbacks"], 1, "{work}");
        assert!(
            work["originalRawProofBytes"].as_u64().unwrap() > 0,
            "{work}"
        );
        assert!(work["originalRawDecodes"].as_u64().unwrap() > 0, "{work}");
        assert_eq!(
            work["indexPin"],
            serde_json::to_value(next).unwrap(),
            "{work}"
        );
        assert!(
            snapshot
                .coverage
                .iter()
                .any(|row| row.document_path == "src/A.js"
                    && row.selected
                    && row.state == trellis::semantic_evidence::CoverageState::Complete),
            "missing-key fallback published incomplete evidence: {snapshot:?}"
        );
    }
    if change_b == "oracle_fork_healthy_two_producers" {
        let work: Value =
            serde_json::from_slice(&fs::read(home.join("work-counts.json")).unwrap()).unwrap();
        assert_eq!(work["destinationIndexBuilds"], 1, "{work}");
        assert_eq!(work["destinationJoinsOnReusedDocuments"], 0, "{work}");
        // Only A and C contain captured source claims; B is a changed native
        // target and must be indexed, not parsed as a destination source claim.
        assert_eq!(work["destinationRoleParses"], 0, "{work}");
        assert_eq!(work["destinationDocumentRows"], 1, "{work}");
        assert_eq!(work["originalRoleParses"], 0, "{work}");
        assert!(work["originalNativeRows"].as_u64().unwrap() <= 2, "{work}");
        assert!(
            work["originalNativeDocuments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert!(
            work["originalSourceBytes"].as_u64().unwrap() <= (b.len() * 2) as u64,
            "{work}"
        );
        assert!(
            work["originalSourceDocuments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert_eq!(work["originalRawProofBytes"], 0, "{work}");
        assert_eq!(work["originalRawEnvelopeParses"], 0, "{work}");
        assert_eq!(work["originalRawDecodes"], 0, "{work}");
        assert!(
            work["parsedRoleSiteDocuments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert!(
            work["parsedTypeDocuments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert!(
            work["originalSourceHashDocuments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert!(
            work["currentTargetLookups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|doc| doc["path"] == "src/B.js"),
            "{work}"
        );
        assert!(
            work["currentTargetLookups"].as_array().unwrap().len() <= 4,
            "{work}"
        );
        assert!(work["currentTargetRows"].as_u64().unwrap() >= 1, "{work}");
        assert!(work["currentTargetRows"].as_u64().unwrap() <= 4, "{work}");
        let (next_native, _) = store.retained_native_snapshot_at(next).unwrap();
        assert_eq!(
            work["destinationRevision"], next_native.revision.id,
            "{work}"
        );
        assert_eq!(
            work["indexPin"],
            serde_json::to_value(next).unwrap(),
            "{work}"
        );
        assert_eq!(
            work["producerIds"],
            json!(["scip-python", "scip-typescript"]),
            "{work}"
        );
        assert_eq!(
            work["reusedDocuments"].as_array().unwrap().len(),
            2,
            "{work}"
        );
        let changed_target = next_native
            .declarations
            .iter()
            .find(|row| row.document.path == "src/B.js" && row.name.as_deref() == Some("target"))
            .unwrap();
        assert_ne!(
            next_native
                .revision
                .documents
                .iter()
                .find(|doc| doc.key.path == "src/B.js")
                .unwrap()
                .content_hash,
            native
                .revision
                .documents
                .iter()
                .find(|doc| doc.key.path == "src/B.js")
                .unwrap()
                .content_hash,
            "the changed B target must be measured from new bytes",
        );
        assert!(
            snapshot.call_bindings.iter().any(|call| {
                matches!(
                    &call.declared_target,
                    Some(trellis::semantic_evidence::Target::Internal {
                        syntax_id,
                        revision_id,
                        ..
                    }) if syntax_id == &changed_target.syntax_id
                        && revision_id == &next_native.revision.id
                )
            }),
            "changed B declaration must resolve the unchanged A source: {snapshot:?}"
        );
    }
    if matches!(change_b, "oracle_body" | "oracle_two_producers") {
        let work: Value =
            serde_json::from_slice(&fs::read(home.join("work-counts.json")).unwrap()).unwrap();
        eprintln!("publication work counts {change_b}: {work}");
        if change_b == "oracle_body" {
            assert_eq!(work["destinationIndexBuilds"], 1, "{work}");
            assert_eq!(work["destinationRoleParses"], 0, "{work}");
            assert_eq!(work["fullFallbacks"], 0, "{work}");
            assert_eq!(work["originalRoleParses"], 0, "{work}");
            assert_eq!(work["originalRawDecodes"], 0, "{work}");
        } else {
            assert_eq!(work["fullFallbacks"], 1, "{work}");
            assert_eq!(
                work["destinationIndexBuilds"], 2,
                "attempted LOCAL work plus FULL fallback: {work}"
            );
            assert!(work["originalRoleParses"].as_u64().unwrap() > 0, "{work}");
        }
        assert_eq!(work["destinationJoinsOnReusedDocuments"], 0, "{work}");
    }
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
    if matches!(
        change_b,
        "corrupt_capture" | "two_producers" | "oracle_two_producers" | "oracle_fork_two_producers"
    ) {
        if add_unrequested_document {
            let added = snapshot
                .coverage
                .iter()
                .find(|row| row.producer_id == "scip-typescript" && row.document_path == "src/C.js")
                .expect("new document must have coverage for the selected producer");
            assert_eq!(
                (added.requested, added.selected, added.state),
                (
                    false,
                    false,
                    trellis::semantic_evidence::CoverageState::NotRequested
                ),
                "new document was never requested by the corrupt capture: {added:?}"
            );
            for path in ["src/A.js", "src/B.js"] {
                let requested = snapshot
                    .coverage
                    .iter()
                    .find(|row| row.producer_id == "scip-typescript" && row.document_path == path)
                    .unwrap();
                assert_eq!(
                    (requested.requested, requested.selected, requested.state),
                    (
                        true,
                        true,
                        trellis::semantic_evidence::CoverageState::Failed
                    ),
                    "requested document must retain failed selected coverage: {requested:?}"
                );
            }
        }
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
        if matches!(
            change_b,
            "two_producers" | "oracle_two_producers" | "oracle_fork_two_producers"
        ) {
            let good: Vec<_> = snapshot
                .coverage
                .iter()
                .filter(|row| row.producer_id == "scip-python" && row.requested)
                .collect();
            assert!(!good.is_empty());
            assert!(good.iter().all(|row| {
                row.selected && row.state == trellis::semantic_evidence::CoverageState::Complete
            }));
            assert!(snapshot.provenance.iter().any(|proof|
                proof.producer_id == "scip-python" && proof.derived_from.is_some()));
        } else {
            assert!(snapshot.provenance.is_empty());
        }
    } else if !matches!(
        change_b,
        "zero_claims"
            | "changed_a"
            | "partial_empty_covered"
            | "oracle_changed_a"
            | "oracle_fork_changed_a"
            | "oracle_fork_deleted_a"
            | "oracle_fork_deleted_b"
            | "oracle_fork_moved_a"
            | "oracle_fork_moved_b"
            | "oracle_fork_ambiguous_b"
            | "oracle_deleted_a"
            | "oracle_deleted_b"
            | "oracle_moved_a"
            | "oracle_moved_b"
            | "oracle_ambiguous_b"
    ) {
        assert!(
            !snapshot.provenance.is_empty(),
            "native rejoin should preserve valid A evidence: {snapshot:?}"
        );
    }
    if let Some(fork) = &fork_home {
        let pid: i32 = fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        trellis::testing::wait_until("local branch daemon exit", || {
            !trellis::testing::process_running(pid)
        });
        let full: trellis::model::IndexPin =
            serde_json::from_value(run(fork, &index_args)["publishedRevision"].clone()).unwrap();
        assert_eq!(
            full, next,
            "forked FULL and LOCAL pins must share the same seeded identity"
        );
        let roots = TopologyRoots::isolated_for_tests(
            if cfg!(target_os = "macos") {
                fork.join("Library/Caches/dev.squashmerge.trellis")
            } else {
                fork.join(".cache/trellis")
            },
            if cfg!(target_os = "macos") {
                fork.join("Library/Application Support/dev.squashmerge.trellis")
            } else {
                fork.join(".local/share/trellis")
            },
        );
        let identity =
            trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let oracle = trellis::store::Store::open(roots, identity).unwrap();
        assert_eq!(
            serde_json::to_vec(&snapshot).unwrap(),
            serde_json::to_vec(&oracle.semantic_at(full).unwrap()).unwrap(),
            "forked selected FULL/LOCAL semantic output differs",
        );
        assert_eq!(
            store
                .semantic_import_report_at(next, "scip-typescript")
                .unwrap(),
            oracle
                .semantic_import_report_at(full, "scip-typescript")
                .unwrap(),
            "forked selected FULL/LOCAL report differs",
        );
        if change_b == "oracle_fork_healthy_two_producers" {
            for producer in ["scip-typescript", "scip-python"] {
                assert!(
                    snapshot.coverage.iter().any(|row| {
                        row.producer_id == producer
                            && row.selected
                            && row.requested
                            && row.state == trellis::semantic_evidence::CoverageState::Complete
                    }),
                    "missing healthy selected coverage for {producer}: {snapshot:?}"
                );
                assert!(
                    snapshot.provenance.iter().any(|proof| {
                        proof.producer_id == producer && proof.derived_from.is_some()
                    }),
                    "missing selected proof for {producer}: {snapshot:?}"
                );
                assert_eq!(
                    store.semantic_import_report_at(next, producer).unwrap(),
                    oracle.semantic_import_report_at(full, producer).unwrap(),
                    "forked selected FULL/LOCAL report differs for {producer}",
                );
            }
        }
    }
    assert_eq!(
        store.selected_raw_inputs_at(next).unwrap().len(),
        if matches!(
            change_b,
            "two_producers"
                | "oracle_two_producers"
                | "oracle_fork_two_producers"
                | "oracle_fork_healthy_two_producers"
        ) {
            2
        } else {
            1
        }
    );
    if matches!(
        change_b,
        "duplicate_call_claims"
            | "contradictory_calls"
            | "oracle_duplicate_call_claims"
            | "oracle_contradictory_calls"
            | "oracle_fork_duplicate_call_claims"
            | "oracle_fork_contradictory_calls"
    ) {
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
        if matches!(
            change_b,
            "contradictory_calls"
                | "oracle_contradictory_calls"
                | "oracle_fork_contradictory_calls"
        ) {
            assert!(calls.iter().all(|call| call.resolution
                == trellis::semantic_evidence::Resolution::Ambiguous
                && call.candidates.len() == 2));
        }
    }
    if matches!(
        change_b,
        "unmatched_both" | "oracle_unmatched_both" | "oracle_fork_unmatched_both"
    ) {
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
            prior_semantic
                .provenance
                .iter()
                .any(|proof| proof.document.path == "src/A.js")
        );
        assert!(!prior_semantic.call_bindings.is_empty());
        assert_eq!(store.semantic_at(prior).unwrap(), prior_semantic);
        assert!(
            snapshot
                .provenance
                .iter()
                .all(|proof| proof.document.path != "src/A.js")
        );
        assert!(
            snapshot
                .call_bindings
                .iter()
                .all(|binding| binding.join.anchor.document.path != "src/A.js")
        );
        assert!(
            snapshot
                .provenance
                .iter()
                .any(|proof| proof.document.path == "src/B.js" && proof.derived_from.is_some())
        );
    }
    if change_b == "missing_b" {
        let a = snapshot
            .coverage
            .iter()
            .find(|row| row.document_path == "src/A.js")
            .unwrap();
        assert_eq!(
            a.state,
            trellis::semantic_evidence::CoverageState::Partial,
            "{snapshot:?}"
        );
        assert!(a.selected && a.requested);
        assert!(a.diagnostic.is_some());
        assert!(
            a.observed_roles
                .contains(&trellis::semantic_evidence::Role::Definition)
        );
        assert!(
            snapshot
                .provenance
                .iter()
                .any(|proof| proof.document.path == "src/A.js" && proof.derived_from.is_some())
        );
        assert!(
            snapshot
                .call_bindings
                .iter()
                .all(|binding| binding.join.anchor.document.path != "src/A.js")
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
        let retargeted: Vec<_> = snapshot
            .call_bindings
            .iter()
            .filter(|binding| binding.join.anchor.document.path == "src/A.js")
            .collect();
        assert_eq!(
            retargeted.len(),
            1,
            "expected unique rejoined A call: {snapshot:?}"
        );
        let call = retargeted[0];
        assert_eq!(
            call.resolution,
            trellis::semantic_evidence::Resolution::Resolved
        );
        assert_eq!(call.stale_target, Some(false));
        match call.declared_target.as_ref().expect("verified target") {
            trellis::semantic_evidence::Target::Internal {
                syntax_id,
                revision_id,
                ..
            } => {
                assert_eq!(syntax_id, &current_target.syntax_id);
                assert_eq!(revision_id, &r2_native.revision.id);
            }
            other => panic!("expected a verified internal target, found {other:?}"),
        }
        let call_proof = snapshot
            .provenance
            .iter()
            .find(|proof| proof.id == call.provenance_id)
            .unwrap();
        let raw = call_proof
            .derived_from
            .as_ref()
            .expect("retargeted call requires one raw link");
        let inputs = store.selected_raw_inputs_at(next).unwrap();
        let captured_raw = inputs[0]
            .proofs
            .iter()
            .find(|(id, _)| id == &raw.provenance_id)
            .unwrap();
        assert_eq!(captured_raw.1.fact_digest, raw.fact_digest);
        assert_eq!(captured_raw.1.record_kind, "occurrence");
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
fn corrupt_index_rebaseline_discards_prior_selected_capture() {
    selected_case("corrupt_rebaseline");
}

#[test]
fn bad_shared_native_candidate_rolls_back_without_producer_failure() {
    selected_case("bad_candidate");
}

#[test]
fn retained_native_header_corruption_is_not_a_failed_producer() {
    selected_case("retained_native_header_corruption");
}

#[test]
fn structural_selection_corruption_rolls_back_without_failed_producer_rows() {
    selected_case("structural_corruption");
}

#[test]
fn native_publication_isolates_corrupt_selected_artifact() {
    selected_case("corrupt_capture");
}

#[test]
fn corrupt_selected_capture_does_not_request_new_documents() {
    selected_case("corrupt_capture_new_document");
    selected_case("two_producers_new_document");
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

#[test]
fn forked_seeded_local_matches_forced_full_oracle() {
    for scenario in [
        "oracle_fork_body",
        "oracle_fork_changed_a",
        "oracle_fork_missing_b",
        "oracle_fork_deleted_a",
        "oracle_fork_deleted_b",
        "oracle_fork_moved_a",
        "oracle_fork_moved_b",
        "oracle_fork_ambiguous_b",
        "oracle_fork_retained_target",
        "oracle_fork_unmatched_both",
        "oracle_fork_duplicate_call_claims",
        "oracle_fork_contradictory_calls",
        "oracle_fork_partial_unsupported",
        "oracle_fork_two_producers",
    ] {
        eprintln!("forked oracle scenario: {scenario}");
        selected_case(scenario);
    }
}

#[test]
fn in_process_oracle_mismatch_aborts_without_failed_producer_publication() {
    selected_case("oracle_mismatch");
}

#[test]
fn restored_captured_document_matches_forced_full_without_in_process_oracle() {
    selected_case("oracle_fork_restored_b");
}

#[test]
fn two_healthy_producers_share_one_incremental_destination() {
    selected_case("oracle_fork_healthy_two_producers");
}

#[test]
fn successive_local_publications_reuse_original_capture_without_unchanged_work() {
    selected_case("oracle_fork_successive_local");
}

#[test]
fn missing_admitted_document_key_uses_same_transaction_full_fallback() {
    selected_case("oracle_fork_missing_raw_key");
}

#[test]
fn raw_replacement_uses_new_captured_associations() {
    selected_case("oracle_fork_raw_replacement");
}

#[test]
fn context_invalidation_uses_full_oracle() {
    selected_case("oracle_fork_context");
}

#[test]
fn incremental_local_matches_independent_full_oracle() {
    for scenario in [
        "oracle_body",
        "oracle_changed_a",
        "oracle_missing_b",
        "oracle_retained_target",
        "oracle_restored_b",
        "oracle_deleted_a",
        "oracle_deleted_b",
        "oracle_moved_a",
        "oracle_moved_b",
        "oracle_ambiguous_b",
        "oracle_unmatched_both",
        "oracle_duplicate_call_claims",
        "oracle_contradictory_calls",
        "oracle_partial_unsupported",
        "oracle_two_producers",
    ] {
        selected_case(scenario);
    }
}

#[test]
fn runner_body_edit_reuses_captured_anchor_across_unrelated_ignore_inventory() {
    let temp = Builder::new()
        .prefix("semantic-runner-body-")
        .tempdir_in("/tmp")
        .unwrap();
    let scratch = fs::canonicalize(temp.path()).unwrap();
    let script = r#"
import importlib.util, json, shutil, sqlite3, sys, time
from pathlib import Path
from contextlib import closing
repo, scratch, binary = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
spec = importlib.util.spec_from_file_location('runner', repo / 'tools/verify-67-mixed-performance.py')
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
m.EVIDENCE = scratch / 'evidence'
m.EVIDENCE.mkdir()
m.BINARY = binary
m.DEADLINE = time.monotonic() + 300
# Keep the runner's document topology, not its performance-sized method bodies.
corpus = scratch / 'tiny-corpus'
for language, suffix in [('java','java'), ('javascript','js'), ('python','py')]:
    for ordinal in range(250):
        name = f'Cmedium{ordinal:04d}'
        if language == 'java':
            source = f'public class {name} {{ public static int f0(){{return 40000;}} public static int f1(int n){{if(n>0) return f0(); return 0;}} }}\n'
        elif language == 'javascript':
            source = 'export function f0(){return 4000;}\nexport function f1(n){if(n>0) return f0(); return 0;}\n'
        else:
            source = 'def f0(): return 4000\ndef f1(n):\n    if n > 0: return f0()\n    return 0\n'
        dest = corpus / language / f'{name}.{suffix}'
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(source)
# The five module roots and 245 leaves retain exactly 250 Rust documents.
(corpus / 'rust').mkdir()
(corpus / 'rust/lib.rs').write_text(''.join(f'pub mod g{group:02d};\n' for group in range(4)))
for group in range(4):
    directory = corpus / f'rust/g{group:02d}'
    directory.mkdir()
    ordinals = range(245 * group // 4, 245 * (group + 1) // 4)
    (directory / 'mod.rs').write_text(''.join(f'pub mod Cmedium{ordinal:04d};\n' for ordinal in ordinals))
    for ordinal in ordinals:
        (directory / f'Cmedium{ordinal:04d}.rs').write_text('pub fn f0()->i32{600}\npub fn f1(n:i32)->i32{if n>0 {return f0();} 0}\n')
assert len(list(corpus.rglob('*.*'))) == 1000

def seed_small(workspace, daemon, edited=None):
    anchor = Path('javascript/Cmedium0010.js')
    sentinels = [anchor, Path('java/Cmedium0010.java'),
                 Path('python/Cmedium0010.py'), Path('rust/g00/Cmedium0010.rs')]
    workspace.mkdir()
    for relative in sentinels:
        dest = workspace / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(corpus / relative, dest)
    anchor_hash = m.sha_file(workspace / anchor)
    def publish():
        output = m.command([str(binary), 'index', '--workspace', str(workspace)], env=m.env_for(daemon.home))
        pin = json.loads(output)['publishedRevision']
        return f"pin:v1:{pin['indexGeneration']}:{pin['indexRevision']}"
    r1 = publish()
    with closing(sqlite3.connect(f'file:{m.db_for(daemon.home)}?mode=ro', uri=True)) as db:
        assert db.execute('SELECT count(*) FROM revision_documents WHERE revision_id=?', (r1,)).fetchone()[0] == 4
    artifact, imported = m.import_fixture(binary, workspace, daemon, 'medium', r1)
    shutil.copytree(corpus, workspace, dirs_exist_ok=True)
    if edited is not None:
        for relative, source in edited.items():
            shutil.copyfile(source, workspace / relative)
    r2 = publish()
    assert r2 not in (r1, imported)
    assert m.sha_file(workspace / anchor) == anchor_hash
    with closing(sqlite3.connect(f'file:{m.db_for(daemon.home)}?mode=ro', uri=True)) as db:
        assert db.execute('SELECT count(*) FROM revision_documents WHERE revision_id=?', (r2,)).fetchone()[0] == 1000
        m.selected_proof(db, r2, anchor.as_posix(), artifact)
    return r2, artifact, anchor, anchor_hash
workspace, home = scratch / 'medium-workspace', scratch / 'home'
home.mkdir()
daemon = m.Daemon(binary, home, 'warm')
try:
    daemon.start()
    prior, artifact, anchor, anchor_hash = seed_small(workspace, daemon)
    with closing(sqlite3.connect(f'file:{m.db_for(home)}?mode=ro', uri=True)) as db:
        capture = db.execute('SELECT captured_pin FROM semantic_revision_raw_selections WHERE revision_id=?', (prior,)).fetchone()[0]
        r1 = db.execute('SELECT retained_config,config_hash FROM native_revisions WHERE id=?', (capture,)).fetchone()
        r2 = db.execute('SELECT retained_config,config_hash FROM native_revisions WHERE id=?', (prior,)).fetchone()
        original = json.loads(r1[0]); current = json.loads(r2[0])
        extra = [entry for entry in current if entry not in original]
        assert extra == [{'path': f'rust/g0{n}/{name}', 'type': 'absent'} for n in (1,2,3) for name in ('.gitignore','.ignore')], extra
        assert r1[1] != r2[1]
        links = db.execute('SELECT m.revision_id,d.id,d.extraction_context,d.content_hash FROM revision_documents m JOIN document_versions d ON d.vid=m.vid WHERE m.revision_id IN (?,?) AND m.path=? ORDER BY m.revision_id', (capture,prior,anchor.as_posix())).fetchall()
        assert len(links)==2 and len({link[1:] for link in links}) == 1, links
    language, relative, kind = m.planned_edit('medium', 'body', 0)
    assert (language,relative.as_posix(),kind)==('java','java/Cmedium0000.java','grow')
    before = m.request_rows(home)
    generation, number = prior.rsplit(':', 1)
    wall = time.time_ns() / 1_000_000
    old, new = m.edit_leaf(workspace / relative, language, kind)
    assert (old,new)==('40000','140000'), (old,new)
    _, output, _, _, ended_wall = m.command([str(binary), 'index', '--workspace', str(workspace)], env=m.env_for(home), timed=True)
    pin = json.loads(output)['publishedRevision']
    revision = f"pin:v1:{pin['indexGeneration']}:{pin['indexRevision']}"
    assert revision == f'{generation}:{int(number)+1}'
    after = m.request_rows(home)
    added = set(after) - set(before)
    assert len(added) == 1, added
    request = added.pop()
    state, request_generation, request_revision, incarnation, error = after[request]
    assert (state, request_generation, request_revision, error) == ('done', pin['indexGeneration'], pin['indexRevision'], None)
    deadline = min(m.DEADLINE - 10, time.monotonic() + 5)
    while True:
        completed = [marker for marker in m.markers(daemon.path)
                     if marker['event'] == 'index_request_complete' and marker['detail'].get('request') == request]
        if completed:
            break
        assert time.monotonic() < deadline, request
        time.sleep(.01)
    assert len(completed) == 1 and completed[0]['incarnation'] == incarnation, completed
    complete = completed[0]['detail']
    assert (complete['generation'], int(complete['revision'])) == (request_generation, request_revision)
    observed = m.sample_publication(daemon.path, generation=request_generation, revision=request_revision,
        incarnation=incarnation, request=request, request_mode=complete['mode'], start_wall_ms=wall, cli_end_wall_ms=ended_wall)
    assert observed['publication_mode']=='local', observed
    with closing(sqlite3.connect(f'file:{m.db_for(home)}?mode=ro', uri=True)) as db:
        m.selected_proof(db, revision, anchor.as_posix(), artifact)
        m.check_changed_source(db, revision,relative.as_posix(),(workspace / relative).read_bytes())
    assert m.sha_file(workspace / anchor)==anchor_hash
    work = json.loads((scratch / 'work-counts.json').read_text())
    assert work['witnessedRejoins']==1 and work['fullFallbacks']==0, work
    assert work['originalNativeRows']==0 and work['originalSourceBytes']==0 and work['originalRawProofBytes']==0, work
    assert work['destinationDocumentRows']==996, work
    assert len(work['reusedDocuments'])==4 and work['destinationRoleParses']==0, work
    with closing(sqlite3.connect(f'file:{m.db_for(home)}?mode=ro', uri=True)) as db:
        local_digest = m.parity_digest(db, revision, workspace)
        local_reports = db.execute('SELECT report_json FROM semantic_import_reports ORDER BY rowid').fetchall()
finally:
    daemon.stop()

oracle_home, oracle_root = scratch / 'oracle-home', scratch / 'oracle-workspace'
oracle_home.mkdir()
oracle = m.Daemon(binary, oracle_home, 'oracle')
try:
    oracle.start()
    oracle_pin, oracle_artifact, oracle_anchor, oracle_hash = seed_small(
        oracle_root, oracle, edited={relative.as_posix(): workspace / relative})
    assert oracle_hash == anchor_hash and oracle_anchor == anchor
    with closing(sqlite3.connect(f'file:{m.db_for(oracle_home)}?mode=ro', uri=True)) as db:
        m.selected_proof(db, oracle_pin, oracle_anchor.as_posix(), oracle_artifact)
        oracle_digest = m.parity_digest(db, oracle_pin, oracle_root)
        oracle_reports = db.execute('SELECT report_json FROM semantic_import_reports ORDER BY rowid').fetchall()
    assert local_digest == oracle_digest, [name for name in local_digest if local_digest[name] != oracle_digest[name]]
    assert local_digest['semantic_import_reports'] == oracle_digest['semantic_import_reports']
    assert len(local_reports) == len(oracle_reports) == 1, (local_reports, oracle_reports)
finally:
    oracle.stop()
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(env!("CARGO_MANIFEST_DIR"))
        .arg(&scratch)
        .arg(env!("CARGO_BIN_EXE_trellis"))
        .env(
            "TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE",
            scratch.join("work-counts.json"),
        )
        .env("TRELLIS_TEST_LOCAL_FULL_ORACLE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "runner-shaped local publication failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
