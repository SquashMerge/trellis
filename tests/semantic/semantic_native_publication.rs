#![cfg(feature = "test-support")]
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};
use tempfile::Builder;
use trellis::store::topology::TopologyRoots;

use crate::semantic_batch_support;
use semantic_batch_support::{BatchFixture, DocSpec};

/// Writes a reference-producer batch of hand-built artifacts into the fresh directory
/// `root/<dir>`. Each artifact is `(language, covered paths, SCIP bytes)`; `excluded` entries
/// are `(language, path, reason)`. Returns the descriptor path and its SHA-256.
fn write_batch(
    index_db: &Path,
    root: &Path,
    producer: &str,
    artifacts: &[(&str, &[&str], Vec<u8>)],
    excluded: &[(&str, &str, &'static str)],
    dir: &str,
) -> (std::path::PathBuf, String) {
    let fixture = BatchFixture::from_published(index_db, root);
    let mut fixture = if producer == "trellis-synthetic-scip" {
        fixture.synthetic()
    } else {
        fixture.reference(producer)
    };
    for (language, paths, bytes) in artifacts {
        let ordinal = fixture.artifact(
            language,
            paths.iter().map(|path| DocSpec::new(path)).collect(),
        );
        fixture.override_artifact_raw(ordinal, bytes.clone());
    }
    for (language, path, reason) in excluded {
        fixture.exclude(language, path, reason);
    }
    fixture.write(&root.join(dir))
}

fn import_args<'a>(root: &'a str, descriptor: &'a str, sha: &'a str) -> [&'a str; 8] {
    [
        "semantic",
        "import",
        "--workspace",
        root,
        "--batch",
        descriptor,
        "--batch-sha256",
        sha,
    ]
}

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

// These fixtures assert finished semantic facts, not daemon ownership. Release the
// daemon writer, then run the real second phase once; later CLI calls restart it.
fn finish_selected_semantics(
    home: &Path,
    store: trellis::store::Store,
    native_pin: trellis::model::IndexPin,
) -> (trellis::store::Store, trellis::model::IndexPin) {
    let root = std::path::PathBuf::from(store.workspace_root());
    stop_fixture_daemon(home);
    drop(store);
    // Activation owns SQLite journal recovery before any selected-head read.
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
    let pending_pin = store.index_baseline().unwrap();
    assert_eq!(pending_pin.index_generation, native_pin.index_generation);
    assert!(pending_pin.index_revision >= native_pin.index_revision);
    let pending_native = store.retained_native_snapshot_at(pending_pin).unwrap();
    let pending_semantic = store.semantic_at(pending_pin).unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let leader = store.publication_for_tests().unwrap();
    let completed = store
        .complete_pending_semantic_for_tests(&leader, &cancel)
        .unwrap()
        .unwrap_or(pending_pin);
    assert_eq!(store.index_baseline().unwrap(), completed);
    assert_eq!(
        store.retained_native_snapshot_at(pending_pin).unwrap(),
        pending_native
    );
    assert_eq!(store.semantic_at(pending_pin).unwrap(), pending_semantic);
    assert_eq!(
        store.retained_native_snapshot_at(completed).unwrap(),
        pending_native
    );
    drop(leader);
    (store, completed)
}

fn stop_fixture_daemon(home: &Path) {
    let marker = home.join("daemon.pid");
    let Some(pid) = fs::read_to_string(&marker)
        .ok()
        .and_then(|pid| pid.parse::<i32>().ok())
    else {
        return;
    };
    if trellis::testing::signal_daemon_marker(&marker, libc::SIGTERM).unwrap() {
        trellis::testing::wait_until("fixture daemon writer release", || {
            !trellis::testing::process_running(pid)
        });
    }
}

fn selected_case(change_b: &str) {
    use protobuf::{EnumOrUnknown, Message};
    let synthetic = change_b == "oracle_fork_synthetic";
    let producer_name = if synthetic {
        "trellis-synthetic-scip"
    } else {
        "scip-typescript"
    };
    let add_unrequested_document = matches!(
        change_b,
        "corrupt_capture_new_document" | "two_producers_new_document"
    );
    let over_old_cap = change_b == "oracle_fork_over_old_cap";
    let change_b = match change_b {
        "corrupt_capture_new_document" => "corrupt_capture",
        "two_producers_new_document" => "two_producers",
        "oracle_fork_over_old_cap" => "oracle_fork_body",
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
    fs::write(root.join(".gitignore"), ".trellis-semantic-batch*/\n").unwrap();
    let fused_alias = matches!(
        change_b,
        "oracle_fork_fused_target_lost" | "oracle_fork_fused_beside_lost_target"
    );
    let a = if fused_alias {
        "function run(){ target(); }\nconst alias = target;\n"
    } else {
        "function run(){ target(); }\n"
    };
    let b = "function target(){ return 1; }\n";
    fs::write(root.join("src/A.js"), a).unwrap();
    fs::write(root.join("src/B.js"), b).unwrap();
    if matches!(
        change_b,
        "two_producers"
            | "oracle_two_producers"
            | "oracle_fork_two_producers"
            | "oracle_fork_healthy_two_producers"
            | "oracle_fork_staggered_producers"
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
    let index_db = roots.index_db(&identity);
    let mut store = trellis::store::Store::open(roots, identity).unwrap();
    let (native, _) = store.retained_native_snapshot_at(r1).unwrap();
    let mut binary = scip::types::Index::new();
    for (path, text) in [("src/A.js", a), ("src/B.js", b)] {
        let mut doc = scip::types::Document::new();
        doc.relative_path = path.into();
        doc.language = "javascript".into();
        doc.position_encoding = EnumOrUnknown::from_i32(if synthetic { 1 } else { 2 });
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
    if fused_alias {
        // Two claims of one reference site, on a span that is no call: the second proof fuses
        // into the first one's row.
        let symbols: &[&str] = if change_b == "oracle_fork_fused_target_lost" {
            &["pkg target.", "external pkg other."]
        } else {
            // This row stands on no absent target: only the call in `run` loses its own.
            &["external pkg one.", "external pkg other."]
        };
        for symbol in symbols {
            let mut claim = scip::types::Occurrence::new();
            claim.range = vec![1, 14, 20];
            claim.symbol = (*symbol).into();
            claim.symbol_roles = 0;
            binary.documents[0].occurrences.push(claim);
        }
    }
    let exclusion_reason = match change_b {
        "partial_empty_covered" => Some("notRequested"),
        "partial_not_requested" | "oracle_fork_not_requested" => Some("notRequested"),
        "partial_unsupported"
        | "oracle_partial_unsupported"
        | "oracle_fork_partial_unsupported" => Some("unsupported"),
        "partial_failed" | "caller_failed" | "oracle_fork_failed_exclusion" => Some("failed"),
        _ => None,
    };
    let partial = exclusion_reason.is_some();
    let (covered_path, excluded_path) = if change_b == "caller_failed" {
        ("src/B.js", "src/A.js")
    } else {
        ("src/A.js", "src/B.js")
    };
    if partial {
        binary.documents.retain(|d| d.relative_path == covered_path);
        if change_b == "partial_empty_covered" {
            binary.documents[0].occurrences.clear();
            binary.documents[0].symbols.clear();
        }
    }
    if over_old_cap {
        // Presentation text costs no facts, so the artifact is large and the join stays small.
        binary.documents[0].symbols[0].documentation = vec!["d".repeat(1 << 20); 65];
    }
    let bytes = binary.write_to_bytes().unwrap();
    assert!(!over_old_cap || bytes.len() > 64 << 20);
    let covered: &[&str] = if partial {
        &[covered_path]
    } else {
        &["src/A.js", "src/B.js"]
    };
    let excluded: Vec<(&str, &str, &'static str)> = exclusion_reason
        .map(|reason| ("javascript", excluded_path, reason))
        .into_iter()
        .collect();
    let (descriptor_path, digest) = write_batch(
        &index_db,
        &root,
        producer_name,
        &[("javascript", covered, bytes.clone())],
        &excluded,
        ".trellis-semantic-batch",
    );
    let descriptor = descriptor_path.display().to_string();
    let args = import_args(root_string, &descriptor, &digest);
    let mut digests = std::collections::BTreeMap::from([(producer_name, digest.clone())]);
    let imported = run(&home, &args);
    let imported = if matches!(
        change_b,
        "two_producers"
            | "oracle_two_producers"
            | "oracle_fork_two_producers"
            | "oracle_fork_healthy_two_producers"
            | "oracle_fork_staggered_producers"
    ) {
        if change_b == "oracle_fork_staggered_producers" {
            // The next batch captures bytes this one holds as stale.
            fs::write(root.join("src/B.js"), "function target(){ return 2; }\n").unwrap();
            let indexed = run(&home, &["index", "--workspace", root_string]);
            let native_pin = serde_json::from_value(indexed["publishedRevision"].clone()).unwrap();
            (store, _) = finish_selected_semantics(&home, store, native_pin);
        }
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
        let (python_descriptor, python_digest) = write_batch(
            &index_db,
            &root,
            "scip-python",
            &[(
                "python",
                &["src/C.py"],
                python_binary.write_to_bytes().unwrap(),
            )],
            &[],
            ".trellis-semantic-batch-python",
        );
        digests.insert("scip-python", python_digest.clone());
        run(
            &home,
            &import_args(
                root_string,
                &python_descriptor.display().to_string(),
                &python_digest,
            ),
        )
    } else {
        imported
    };
    let mut prior: trellis::model::IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    // Owner and cancellation cases retain their original daemon lifecycle.
    // Their unchanged initial capture needs no semantic rejoin.
    let keep_daemon = matches!(
        change_b,
        "selected_private_stage" | "cancel_before" | "cancel_after"
    );
    if !keep_daemon {
        (store, prior) = finish_selected_semantics(&home, store, prior);
    }
    let mut prior_semantic = store.semantic_at(prior).unwrap();
    let accepted_artifact_hash = store.selected_raw_inputs_at(prior).unwrap()[0].captures[0]
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
    let unchanged = if keep_daemon {
        unchanged
    } else {
        let (reopened, completed) = finish_selected_semantics(&home, store, unchanged);
        store = reopened;
        completed
    };
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
        (store, prior) = finish_selected_semantics(&home, store, prior);
        prior_semantic = store.semantic_at(prior).unwrap();
        assert!(
            prior_semantic
                .provenance
                .iter()
                .any(|proof| proof.document.path == "src/A.js" && proof.derived_from.is_some()),
            "first LOCAL did not preserve selected r1 evidence: {prior_semantic:?}"
        );
    }
    let (fork_home, mut store) = if change_b.starts_with("oracle_fork_") {
        stop_fixture_daemon(&home);
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
        let seeded_native = store
            .publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel)
            .unwrap();
        assert!(seeded_native.index_revision > prior.index_revision);
        assert_eq!(store.index_baseline().unwrap(), seeded_native);
        let seeded = store
            .complete_pending_semantic_for_tests(&leader, &cancel)
            .unwrap()
            .expect("seeded native publication must finish its selected semantic rejoin");
        assert!(seeded.index_revision > seeded_native.index_revision);
        drop(leader);
        let store = if change_b == "oracle_fork_raw_replacement" {
            let mut extra = scip::types::Occurrence::new();
            extra.range = vec![0, 0, 8];
            extra.symbol = "unmatched0#".into();
            extra.symbol_roles = 0;
            binary.documents[0].occurrences.push(extra);
            let bytes = binary.write_to_bytes().unwrap();
            drop(store);
            run(&home, &["index", "--workspace", root_string]);
            let (replacement, replacement_digest) = write_batch(
                &index_db,
                &root,
                "scip-typescript",
                &[("javascript", &["src/A.js", "src/B.js"], bytes)],
                &[],
                ".trellis-semantic-batch-replacement",
            );
            digests.insert(producer_name, replacement_digest.clone());
            let imported = run(
                &home,
                &import_args(
                    root_string,
                    &replacement.display().to_string(),
                    &replacement_digest,
                ),
            );
            prior = serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
            stop_fixture_daemon(&home);
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
            let replacement_store = trellis::store::Store::open(roots, identity).unwrap();
            let (replacement_store, completed) =
                finish_selected_semantics(&home, replacement_store, prior);
            prior = completed;
            replacement_store
        } else {
            prior = seeded;
            store
        };
        prior_semantic = store.semantic_at(prior).unwrap();
        if change_b == "oracle_fork_raw_replacement" {
            assert_ne!(
                store.selected_raw_inputs_at(prior).unwrap()[0].captures[0].artifact_hash,
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
        // Compare independent completed rejoin outputs from the same seed.
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
        let capture_id = store.selected_raw_inputs_at(prior).unwrap()[0].captures[0].capture_id;
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
        stop_fixture_daemon(&home);

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
        let clean_native = store
            .publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel)
            .unwrap();
        assert!(clean_native.index_revision > prior.index_revision);
        assert_eq!(store.index_baseline().unwrap(), clean_native);
        let clean_head = store
            .complete_pending_semantic_for_tests(&leader, &cancel)
            .unwrap()
            .expect("clean native publication must finish its selected semantic rejoin");
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
        let native_pin = store
            .publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel)
            .expect("historical semantic corruption must not block native publication");
        let native_before = store.retained_native_snapshot_at(native_pin).unwrap();
        let error = store
            .complete_pending_semantic_for_tests(&leader, &cancel)
            .expect_err("retained-row corruption must fail the semantic attempt, not a producer");
        assert!(
            format!("{error:#}").contains("missing native header"),
            "{error:#}"
        );
        assert_eq!(store.index_baseline().unwrap(), native_pin);
        assert_eq!(
            store.retained_native_snapshot_at(native_pin).unwrap(),
            native_before
        );
        store.with_index_sql_for_tests(|db| {
            let head: i64 = db.query_row(
                "SELECT index_revision FROM index_metadata WHERE singleton=1", [], |row| row.get(0),
            )?;
            assert_eq!(head, native_pin.index_revision as i64);
            let newer: i64 = db.query_row(
                "SELECT count(*) FROM native_revisions WHERE published_index_revision>?1",
                [native_pin.index_revision as i64], |row| row.get(0),
            )?;
            let failed: i64 = db.query_row(
                "SELECT count(*) FROM semantic_coverage WHERE revision_id=?1 AND state='failed'",
                [format!("pin:v1:{}:{}", native_pin.index_generation, native_pin.index_revision + 1)],
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
        (store, prior) = finish_selected_semantics(&home, store, prior);
        prior_semantic = store.semantic_at(prior).unwrap();
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
        "oracle_deleted_b" | "oracle_fused_target_lost" | "oracle_fused_beside_lost_target" => {
            fs::remove_file(root.join("src/B.js")).unwrap()
        }
        "oracle_not_requested" | "oracle_failed_exclusion" | "oracle_staggered_producers" => {
            fs::write(root.join("src/D.js"), "function extra(){ return 2; }\n").unwrap()
        }
        // Only the capture stat changes: the same bytes, a newer modification time.
        "oracle_touched_b" => fs::OpenOptions::new()
            .write(true)
            .open(root.join("src/B.js"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap(),
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
        stop_fixture_daemon(&home);
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
                for table in [
                    "semantic_raw_batches",
                    "semantic_raw_captures",
                    "semantic_raw_exclusions",
                ] {
                    let rows: i64 =
                        db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })?;
                    assert_eq!(rows, 0, "{table}");
                }
                Ok(())
            })
            .unwrap();
        return;
    }
    if change_b == "bad_candidate" {
        stop_fixture_daemon(&home);
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
        stop_fixture_daemon(&home);
        drop(store);
        {
            let db = rusqlite::Connection::open(&index_db).unwrap();
            db.pragma_update(None, "foreign_keys", "OFF").unwrap();
            db.execute(
                "DELETE FROM semantic_requested_producers WHERE revision_id=?1 AND id='scip-typescript'",
                [format!("pin:v1:{}:{}", prior.index_generation, prior.index_revision)],
            ).unwrap();
        }
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
        assert!(
            trellis::testing::signal_daemon_marker(&home.join("daemon.pid"), libc::SIGKILL)
                .unwrap()
        );
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
        if change_b == "cancel_before" {
            let native_pin = result.expect("native publication does not run the semantic hook");
            assert_eq!(store.index_baseline().unwrap(), native_pin);
            let native_before = store.retained_native_snapshot_at(native_pin).unwrap();
            let selected_before = store.selected_raw_inputs_at(native_pin).unwrap();
            assert_eq!(
                store
                    .complete_pending_semantic_for_tests(&leader, &cancel)
                    .unwrap(),
                None,
                "cancelled semantic attempt must not publish a completion",
            );
            assert!(cancel.load(std::sync::atomic::Ordering::Acquire));
            assert_eq!(store.index_baseline().unwrap(), native_pin);
            assert_eq!(
                store.retained_native_snapshot_at(native_pin).unwrap(),
                native_before
            );
            assert_eq!(
                format!("{:?}", store.selected_raw_inputs_at(native_pin).unwrap()),
                format!("{selected_before:?}"),
            );
            assert_eq!(store.semantic_at(baseline).unwrap(), prior_semantic);
            store
                .with_index_sql_for_tests(|db| {
                    let newer: i64 = db.query_row(
                        "SELECT count(*) FROM native_revisions WHERE published_index_revision>?1",
                        [native_pin.index_revision as i64],
                        |row| row.get(0),
                    )?;
                    let next_semantic: i64 = db.query_row(
                        "SELECT count(*) FROM semantic_coverage WHERE revision_id=?1",
                        [format!(
                            "pin:v1:{}:{}",
                            native_pin.index_generation,
                            native_pin.index_revision + 1
                        )],
                        |row| row.get(0),
                    )?;
                    assert_eq!((newer, next_semantic), (0, 0));
                    Ok(())
                })
                .unwrap();
            return;
        }
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
        stop_fixture_daemon(&home);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let options = trellis::indexer::IndexOptions::new(root.clone());
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let leader = store.publication_for_tests().unwrap();
        let native_pin = store
            .publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &cancel)
            .unwrap();
        assert!(native_pin.index_revision > prior.index_revision);
        assert_eq!(store.index_baseline().unwrap(), native_pin);
        let next = store
            .complete_pending_semantic_for_tests(&leader, &cancel)
            .unwrap()
            .expect("compatible native publication must finish its selected semantic rejoin");
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
    let native_pin: trellis::model::IndexPin =
        serde_json::from_value(run(&home, &index_args)["publishedRevision"].clone()).unwrap();
    let (store, next) = finish_selected_semantics(&home, store, native_pin);
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
    // Phase two currently uses FULL rejoin. These cases retain their semantic
    // correctness and independent-fork parity checks, not the old LOCAL work counts.
    if change_b.starts_with("oracle_fork_") {
        if synthetic {
            // The unchanged caller keeps complete selected synthetic coverage; the edited B
            // document fails on its own bytes.
            let state_of = |path: &str| {
                snapshot
                    .coverage
                    .iter()
                    .find(|row| row.producer_id == producer_name && row.document_path == path)
                    .map(|row| (row.selected, row.state))
            };
            assert_eq!(
                state_of("src/A.js"),
                Some((true, trellis::semantic_evidence::CoverageState::Complete))
            );
            assert_eq!(
                state_of("src/B.js"),
                Some((true, trellis::semantic_evidence::CoverageState::Failed))
            );
        }
        if matches!(
            change_b,
            "oracle_fork_not_requested" | "oracle_fork_failed_exclusion"
        ) {
            let state_of = |path: &str| {
                snapshot
                    .coverage
                    .iter()
                    .find(|row| row.document_path == path)
                    .map(|row| row.state)
                    .unwrap()
            };
            assert_eq!(
                state_of("src/A.js"),
                trellis::semantic_evidence::CoverageState::Complete
            );
            assert_eq!(
                state_of("src/B.js"),
                if change_b == "oracle_fork_not_requested" {
                    trellis::semantic_evidence::CoverageState::NotRequested
                } else {
                    trellis::semantic_evidence::CoverageState::Failed
                }
            );
            assert!(
                snapshot
                    .provenance
                    .iter()
                    .all(|proof| proof.document.path != "src/B.js"),
                "an excluded document holds no facts: {snapshot:?}"
            );
        }
        if change_b == "oracle_fork_touched_b" {
            assert!(
                snapshot
                    .provenance
                    .iter()
                    .any(|proof| proof.document.path == "src/B.js" && proof.derived_from.is_some()),
                "a touched document keeps its raw-linked proofs: {snapshot:?}"
            );
        }
        if change_b == "oracle_fork_restored_b" {
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
    }
    if change_b == "oracle_fork_successive_local" {
        let (current_native, _) = store.retained_native_snapshot_at(next).unwrap();
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
    if change_b == "oracle_fork_healthy_two_producers" {
        let (next_native, _) = store.retained_native_snapshot_at(next).unwrap();
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
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|row| row.producer_id == producer_name && row.selected && row.requested),
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
        stop_fixture_daemon(&home);
        let full: trellis::model::IndexPin =
            serde_json::from_value(run(fork, &index_args)["publishedRevision"].clone()).unwrap();
        stop_fixture_daemon(fork);
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
        let (oracle, full) = finish_selected_semantics(fork, oracle, full);
        assert_eq!(
            full, next,
            "forked completed pins must share the same seeded identity"
        );
        assert_eq!(
            serde_json::to_vec(&snapshot).unwrap(),
            serde_json::to_vec(&oracle.semantic_at(full).unwrap()).unwrap(),
            "forked selected FULL/LOCAL semantic output differs",
        );
        assert_eq!(
            store
                .semantic_batch_report_at(next, &digests[producer_name])
                .unwrap(),
            oracle
                .semantic_batch_report_at(full, &digests[producer_name])
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
                    store
                        .semantic_batch_report_at(next, &digests[producer])
                        .unwrap(),
                    oracle
                        .semantic_batch_report_at(full, &digests[producer])
                        .unwrap(),
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
                | "oracle_fork_staggered_producers"
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
    if change_b == "regular" {
        use trellis::graph_query::{
            BoundaryReason, DEFAULT_DEPTH, DEFAULT_MAX_CALLS, DEFAULT_MAX_NODES, GraphQuery, Visit,
            WarningCode,
        };
        // The rejoined importer binding is attached to its edge by the engine's candidate
        // rule, and its possiblyStale freshness is the published one, not recomputed.
        let (r2_native, _) = store.retained_native_snapshot_at(next).unwrap();
        let run = r2_native
            .declarations
            .iter()
            .find(|declaration| declaration.name.as_deref() == Some("run"))
            .unwrap();
        let answer = store
            .evidence_response()
            .unwrap()
            .graph_query(
                &GraphQuery {
                    root_syntax_id: run.syntax_id.clone(),
                    semantic_producer_id: Some(snapshot.producers[0].id.clone()),
                    depth: DEFAULT_DEPTH,
                    max_nodes: DEFAULT_MAX_NODES,
                    max_calls: DEFAULT_MAX_CALLS,
                    include_external: true,
                    include_tests: true,
                },
                None,
                &mut || Ok(()),
            )
            .unwrap();
        let result = answer.outcome.unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.edges.len(), 1);
        let edge = &result.edges[0];
        assert_eq!(edge.call.spelling.as_deref(), Some("target"));
        assert_eq!(edge.visit, Visit::Boundary);
        assert_eq!(edge.boundary_reason, BoundaryReason::Stale);
        let binding = edge.binding.as_ref().expect("rejoined binding is attached");
        assert_eq!(binding.call_id.as_deref(), Some(edge.call.id.as_str()));
        assert_eq!(binding.stale_target, Some(false));
        let proof = result
            .provenance
            .iter()
            .find(|proof| proof.id == binding.provenance_id)
            .unwrap();
        assert_eq!(
            proof.freshness,
            trellis::semantic_evidence::Freshness::PossiblyStale
        );
        assert!(proof.derived_from.is_some());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.code == WarningCode::StaleEvidence && w.provenance_id.is_none())
        );
        assert!(
            result
                .warnings
                .iter()
                .all(|w| w.code != WarningCode::StaleTarget)
        );
    }
}

#[test]
fn native_publication_rejoins_accepted_capture_and_unchanged_is_noop() {
    selected_case("regular");
}

#[test]
fn cancellation_before_native_commit_rolls_back_native_pin_and_search() {
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

/// Reused claims borrow their original identity index; changed documents retain no claims.
/// Both paths must publish the same complete evidence and fresh IDs as an independent FULL.
#[test]
fn witnessed_rejoin_reused_and_changed_documents_match_forced_full_oracle() {
    selected_case("oracle_fork_body");
    selected_case("oracle_fork_changed_a");
}

#[test]
fn forked_seeded_local_matches_forced_full_oracle() {
    for scenario in [
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
        "oracle_fork_failed_exclusion",
        "oracle_fork_fused_target_lost",
        "oracle_fork_fused_beside_lost_target",
    ] {
        eprintln!("forked oracle scenario: {scenario}");
        selected_case(scenario);
    }
}

#[test]
fn cancelled_semantic_attempt_keeps_native_head_without_failed_producer_publication() {
    selected_case("cancel_before");
}

#[test]
fn restored_captured_document_matches_forced_full_without_in_process_oracle() {
    selected_case("oracle_fork_restored_b");
}

/// #567: a document the artifact does not cover, in the artifact's own language, must not send
/// an edit elsewhere to FULL.
#[test]
fn edit_beside_a_not_requested_document_takes_the_fast_path() {
    selected_case("oracle_fork_not_requested");
}

/// #567: a touched file changes the capture stat, so the head cannot stand, but no document
/// changed: a new daemon's reconciliation publishes on the fast path.
#[test]
fn stat_only_change_after_restart_takes_the_fast_path() {
    selected_case("oracle_fork_touched_b");
}

#[test]
fn synthetic_batch_native_edit_records_synthetic_producer_fallback() {
    selected_case("oracle_fork_synthetic");
}

#[test]
fn two_healthy_producers_share_one_incremental_destination() {
    selected_case("oracle_fork_healthy_two_producers");
}

/// #567: batches of two languages, imported at different revisions, each reuse only what they
/// captured: a document stale for one must not fail the other's witness.
#[test]
fn producers_imported_at_different_revisions_each_take_the_fast_path() {
    selected_case("oracle_fork_staggered_producers");
}

#[test]
fn successive_local_publications_reuse_original_capture_without_unchanged_work() {
    selected_case("oracle_fork_successive_local");
}

/// #555: past the old 64 MiB cap the CLI import selects the artifact, retains it, and a later
/// publication still takes the witnessed fast path instead of silently falling back.
#[test]
fn artifact_over_the_old_cap_imports_and_rejoins_on_the_fast_path() {
    selected_case("oracle_fork_over_old_cap");
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
fn runner_native_body_edit_then_completed_semantics_matches_full_oracle() {
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
phase = sys.argv[4]
spec = importlib.util.spec_from_file_location('runner', repo / 'tools/verify-67-mixed-performance.py')
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
m.EVIDENCE = scratch / 'evidence'
m.EVIDENCE.mkdir(exist_ok=True)
m.BINARY = binary
m.DEADLINE = time.monotonic() + 300
original_env_for = m.env_for
def fixture_env(home):
    env = original_env_for(home)
    env['TRELLIS_TEST_DAEMON_PID_FILE'] = str(home / 'daemon.pid')
    return env
m.env_for = fixture_env
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
(corpus / 'rust').mkdir(exist_ok=True)
(corpus / 'rust/lib.rs').write_text(''.join(f'pub mod g{group:02d};\n' for group in range(4)))
for group in range(4):
    directory = corpus / f'rust/g{group:02d}'
    directory.mkdir(exist_ok=True)
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
    return r2, artifact, anchor, anchor_hash

workspace, home = scratch / 'medium-workspace', scratch / 'home'
context = scratch / 'runner-context.json'
def selected(home):
    with closing(sqlite3.connect(f'file:{m.db_for(home)}?mode=ro', uri=True)) as db:
        generation, revision = db.execute('SELECT index_generation,index_revision FROM index_metadata').fetchone()
        return f'pin:v1:{generation}:{revision}'
if phase == 'seed':
    home.mkdir(exist_ok=True)
    daemon = m.Daemon(binary, home, phase)
    daemon.start()
    revision, artifact, anchor, anchor_hash = seed_small(workspace, daemon)
    context.write_text(json.dumps({'artifact':artifact,'anchor':anchor.as_posix(),'anchorHash':anchor_hash}))
elif phase == 'oracle':
    home = scratch / 'oracle-home'
    base_env = m.env_for
    def full_env(home):
        env = base_env(home)
        env['TRELLIS_TEST_FORCE_FULL_REJOIN'] = '1'
        return env
    m.env_for = full_env
    daemon = m.Daemon(binary, home, phase)
    daemon.start()
    m.command([str(binary), 'index', '--workspace', str(workspace)], env=m.env_for(home))
elif phase == 'edit':
    saved = json.loads(context.read_text())
    artifact, anchor, anchor_hash = saved['artifact'], Path(saved['anchor']), saved['anchorHash']
    prior = selected(home)
    with closing(sqlite3.connect(f'file:{m.db_for(home)}?mode=ro', uri=True)) as db:
        m.selected_proof(db, prior, anchor.as_posix(), artifact)
        capture = db.execute('SELECT captured_pin FROM semantic_revision_raw_selections WHERE revision_id=?', (prior,)).fetchone()[0]
        captured = db.execute('SELECT retained_config,config_hash FROM native_revisions WHERE id=?', (capture,)).fetchone()
        current = db.execute('SELECT retained_config,config_hash FROM native_revisions WHERE id=?', (prior,)).fetchone()
        extra = [entry for entry in json.loads(current[0]) if entry not in json.loads(captured[0])]
        assert extra == [{'path': f'rust/g0{n}/{name}', 'type': 'absent'} for n in (1,2,3) for name in ('.gitignore','.ignore')], extra
        assert captured[1] != current[1]
        links = db.execute('SELECT m.revision_id,d.id,d.extraction_context,d.content_hash FROM revision_documents m JOIN document_versions d ON d.vid=m.vid WHERE m.revision_id IN (?,?) AND m.path=? ORDER BY m.revision_id', (capture,prior,anchor.as_posix())).fetchall()
        assert len(links)==2 and len({link[1:] for link in links}) == 1, links
    daemon = m.Daemon(binary, home, 'edit')
    daemon.start()
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
    with closing(sqlite3.connect(f'file:{m.db_for(home)}?mode=ro', uri=True)) as db:
        m.check_changed_source(db, revision, relative.as_posix(), (workspace / relative).read_bytes())
    assert m.sha_file(workspace / anchor) == anchor_hash

elif phase == 'parity':
    saved = json.loads(context.read_text())
    anchor, artifact = saved['anchor'], saved['artifact']
    for selected_home in [home, scratch / 'oracle-home']:
        revision = selected(selected_home)
        with closing(sqlite3.connect(f'file:{m.db_for(selected_home)}?mode=ro', uri=True)) as db:
            m.selected_proof(db, revision, anchor, artifact)

"#;
    use std::sync::{Arc, atomic::AtomicBool};
    let mut completed_branches = Vec::new();
    for phase in ["seed", "edit", "oracle", "parity"] {
        let output = Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(env!("CARGO_MANIFEST_DIR"))
            .arg(&scratch)
            .arg(env!("CARGO_BIN_EXE_trellis"))
            .arg(phase)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "runner phase {phase}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if phase == "parity" {
            continue;
        }
        let (home, root) = if phase == "oracle" {
            (
                scratch.join("oracle-home"),
                scratch.join("medium-workspace"),
            )
        } else {
            (scratch.join("home"), scratch.join("medium-workspace"))
        };
        stop_fixture_daemon(&home);
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
        let native_pin = store.index_baseline().unwrap();
        let native = store.retained_native_snapshot_at(native_pin).unwrap();
        let pending_semantic = store.semantic_at(native_pin).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let leader = store.publication_for_tests().unwrap();
        let completed = store
            .complete_pending_semantic_for_tests(&leader, &cancel)
            .unwrap()
            .unwrap_or(native_pin);
        assert_eq!(
            store.retained_native_snapshot_at(native_pin).unwrap(),
            native
        );
        assert_eq!(
            store.retained_native_snapshot_at(completed).unwrap(),
            native
        );
        assert_eq!(store.index_baseline().unwrap(), completed);
        assert_eq!(store.semantic_at(native_pin).unwrap(), pending_semantic);
        let semantic = store.semantic_at(completed).unwrap();
        assert!(
            semantic
                .provenance
                .iter()
                .any(|proof| proof.document.path == "javascript/Cmedium0010.js")
        );
        if phase == "seed" {
            drop(leader);
            drop(store);
            copy_seed(&home, &scratch.join("oracle-home"));
        } else {
            let reports = store.with_index_sql_for_tests(|db| {
                let key = format!("pin:v1:{}:{}", completed.index_generation, completed.index_revision);
                Ok(db.prepare("SELECT producer_id,report_json FROM semantic_import_reports WHERE revision_id=?1 ORDER BY producer_id")?
                    .query_map([key], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            }).unwrap();
            assert!(!reports.is_empty(), "phase {phase}: completed pin {completed:?}, native pin {native_pin:?}; metadata {:?}",
                store.with_index_sql_for_tests(|db| {
                    let mut details = Vec::new();
                    for (table, fields) in [
                        ("semantic_revision_raw_selections", "producer_id || ':' || batch_id || ':' || captured_pin"),
                        ("semantic_revision_producers", "id || ':' || version"),
                        ("semantic_requested_producers", "id || ':' || version"),
                        ("semantic_pending_rejoins", "native_revision_id || ':' || selection_identity"),
                        ("semantic_rejoin_completions", "native_revision_id || ':' || selection_identity"),
                        ("semantic_native_origins", "producer_id || ':' || origin_revision_id"),
                        ("semantic_import_reports", "producer_id"),
                    ] {
                        let rows = db.prepare(&format!("SELECT revision_id,{fields} FROM {table} ORDER BY revision_id,2"))?
                            .query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        details.push((table.to_owned(),rows));
                    }
                    Ok(details)
                }).unwrap());
            completed_branches.push((completed, native, semantic, reports));
        }
    }
    assert_eq!(completed_branches.len(), 2);
    assert_eq!(
        completed_branches[0], completed_branches[1],
        "completed native and semantic families, proofs, bases, targets and reports differ from FULL fork"
    );
}

/// Completed role-site facts are checked against an independently seeded FULL rejoin.
#[test]
fn completed_role_site_families_match_independent_full_rejoin() {
    selected_case("oracle_fork_duplicate_call_claims");
    selected_case("oracle_fork_contradictory_calls");
    selected_case("oracle_fork_unmatched_both");
}

/// Real SCIP admission, followed only by native publications: no daemon can complete the join.
fn raw_target_native_first_case(change: &str) {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        graph_query::{BoundaryReason, GraphQuery, Visit},
        model::SemanticAvailability,
        semantic_batch::{BatchLimits, testing::stage_and_join_admitted},
        semantic_evidence::{self as semantic, Target},
        semantic_import::DestinationMutations,
    };
    let temp = Builder::new()
        .prefix("raw-target-native-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join(".gitignore"), ".trellis-semantic-batch*/\n").unwrap();
    let sources = [
        ("src/A.js", "function run(){ target(); safe(); }\n"),
        ("src/B.js", "function target(){ return 1; }\n"),
        ("src/C.js", "function safe(){ return 3; }\n"),
    ];
    for (path, source) in sources {
        fs::write(root.join(path), source).unwrap();
    }
    let root = fs::canonicalize(root).unwrap();
    let state = temp.path().join("state");
    let store = trellis::store::Store::open_for_tests(&state, &root).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let publish_native = || {
        let options = trellis::indexer::IndexOptions::new(root.clone());
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let leader = store.publication_for_tests().unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                &leader,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap()
    };
    publish_native();
    let index_db = TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"))
        .index_db(
            &trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap(),
        );
    let mut docs = Vec::new();
    for (path, source) in sources {
        let name = match path {
            "src/A.js" => "run",
            "src/B.js" => "target",
            _ => "safe",
        };
        let symbol = format!("pkg {name}.");
        let start = source.find(name).unwrap() as i32;
        let mut doc = DocSpec::new(path)
            .occurrence(&[0, start, start + name.len() as i32], &[], &symbol, 1)
            .symbol(&symbol, 26, &[]);
        if path == "src/A.js" {
            for name in ["target", "safe"] {
                let start = source.find(name).unwrap() as i32;
                doc = doc.occurrence(
                    &[0, start, start + name.len() as i32],
                    &[0, start, start + name.len() as i32 + 2],
                    &format!("pkg {name}."),
                    0,
                );
            }
        }
        docs.push(doc);
    }
    let fixture = BatchFixture::from_published(&index_db, &root).reference("scip-typescript");
    let bytes = fixture.scip_bytes("javascript", &docs);
    let (descriptor, digest) = write_batch(
        &index_db,
        &root,
        "scip-typescript",
        &[("javascript", &["src/A.js", "src/B.js", "src/C.js"], bytes)],
        &[],
        ".trellis-semantic-batch-direct",
    );
    let (mut admitted, joined, _) = stage_and_join_admitted(
        &store,
        &root,
        &descriptor,
        &digest,
        &BatchLimits::PRODUCTION,
        DestinationMutations::default(),
        &cancel,
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let original_pin = store
        .publish_semantic_batch(
            &joined,
            admitted.stage_mut(),
            joined.destination_pin(),
            &leader,
            &cancel,
        )
        .unwrap();
    drop(leader);
    drop(admitted);
    let original = store.semantic_at(original_pin).unwrap();
    let (original_native, _) = store.retained_native_snapshot_at(original_pin).unwrap();
    let run = original_native
        .declarations
        .iter()
        .find(|row| row.document.path == "src/A.js" && row.name.as_deref() == Some("run"))
        .unwrap()
        .syntax_id
        .clone();
    let binding_for = |path: &str| {
        original.call_bindings.iter().find(|binding| {
        matches!(&binding.declared_target, Some(Target::Internal { document, .. }) if document.path == path)
    }).unwrap().clone()
    };
    let dependent = binding_for("src/B.js");
    let independent = binding_for("src/C.js");
    assert_eq!(original.call_bindings.len(), 2);
    assert_eq!(dependent.resolution, semantic::Resolution::Resolved);
    assert_eq!(independent.resolution, semantic::Resolution::Resolved);
    assert_ne!(dependent.call_id, independent.call_id);
    assert_ne!(dependent.provenance_id, independent.provenance_id);
    for binding in [&dependent, &independent] {
        let proof = original
            .provenance
            .iter()
            .find(|p| p.id == binding.provenance_id)
            .unwrap();
        assert!(proof.derived_from.is_some());
        assert!(proof.basis.is_some());
        assert_eq!(proof.revision_id, original_native.revision.id);
        assert_eq!(proof.freshness, semantic::Freshness::Fresh);
    }
    let response = store.evidence_response().unwrap();
    let status = response
        .document_semantic_status_at("src/A.js", Some(original_pin))
        .unwrap();
    assert_eq!(status.state, SemanticAvailability::Fresh);
    assert_eq!(status.semantic_origin.unwrap().pin, original_pin);
    response.finish(()).unwrap();
    drop(response);

    match change {
        "deleted" => fs::remove_file(root.join("src/B.js")).unwrap(),
        "ambiguous" => fs::write(
            root.join("src/B.js"),
            "function target(){ return 1; }\nfunction target(){ return 2; }\n",
        )
        .unwrap(),
        "changed" | "restored" => {
            fs::write(root.join("src/B.js"), "function target(){ return 2; }\n").unwrap()
        }
        _ => unreachable!(),
    }
    let first_pin = publish_native();
    let first = store.semantic_at(first_pin).unwrap();
    for restored in [false, true] {
        let pin = if restored {
            if change != "restored" {
                break;
            }
            fs::write(root.join("src/B.js"), sources[1].1).unwrap();
            publish_native()
        } else {
            first_pin
        };
        let selected = store.semantic_at(pin).unwrap();
        let (native, _) = store.retained_native_snapshot_at(pin).unwrap();
        let freshness = if native.revision.id == original_native.revision.id {
            semantic::Freshness::Fresh
        } else {
            semantic::Freshness::PossiblyStale
        };
        assert_eq!(
            fs::read(root.join("src/A.js")).unwrap(),
            sources[0].1.as_bytes()
        );
        let call_ids = |artifact: &trellis::native_evidence::Artifact| {
            artifact
                .calls
                .iter()
                .filter(|call| call.document.path == "src/A.js")
                .map(|call| call.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(call_ids(&native), call_ids(&original_native));
        assert!(selected.call_bindings.contains(&independent));
        let mut proof = original
            .provenance
            .iter()
            .find(|proof| proof.id == independent.provenance_id)
            .unwrap()
            .clone();
        proof.freshness = freshness;
        assert_eq!(
            selected.provenance.iter().find(|p| p.id == proof.id),
            Some(&proof)
        );
        if !restored {
            assert!(
                !selected
                    .call_bindings
                    .iter()
                    .any(|b| b.call_id == dependent.call_id)
            );
            assert!(
                !selected
                    .provenance
                    .iter()
                    .any(|p| p.id == dependent.provenance_id)
            );
            let targets_b = |target: &Target| {
                matches!(target,
                Target::Internal { document, .. } if document.path == "src/B.js")
            };
            assert!(
                !selected
                    .symbols
                    .iter()
                    .any(|s| s.declarations.iter().any(targets_b))
            );
            assert!(
                !selected.references.iter().any(|r| r
                    .declared_target
                    .iter()
                    .chain(&r.candidates)
                    .any(targets_b))
            );
            assert!(
                !selected
                    .type_relationships
                    .iter()
                    .any(|r| targets_b(&r.source) || targets_b(&r.target))
            );
        } else if let Some(binding) = selected
            .call_bindings
            .iter()
            .find(|b| b.call_id == dependent.call_id)
        {
            // Exact restoration may safely expose the old association, never a new claim.
            assert_eq!(binding, &dependent);
            let mut proof = original
                .provenance
                .iter()
                .find(|p| p.id == dependent.provenance_id)
                .unwrap()
                .clone();
            proof.freshness = freshness;
            assert_eq!(
                selected.provenance.iter().find(|p| p.id == proof.id),
                Some(&proof)
            );
        }
        let response = store.evidence_response().unwrap();
        for path in ["src/A.js", "src/C.js"] {
            let status = response
                .document_semantic_status_at(path, Some(pin))
                .unwrap();
            if path == "src/C.js" {
                assert_eq!(status.state, SemanticAvailability::Fresh);
            } else if !restored {
                assert_eq!(status.state, SemanticAvailability::CatchingUp);
            }
            let origin = status.semantic_origin.unwrap();
            assert_eq!(origin.pin, original_pin);
            assert_eq!(origin.revision_id, original_native.revision.id);
        }
        if change == "deleted" {
            assert!(
                !native
                    .revision
                    .documents
                    .iter()
                    .any(|d| d.key.path == "src/B.js")
            );
            let error = response
                .document_semantic_status_at("src/B.js", Some(pin))
                .unwrap_err();
            assert!(error.to_string().contains("not_found"));
        } else {
            assert_eq!(
                response
                    .document_semantic_status_at("src/B.js", Some(pin))
                    .unwrap()
                    .state,
                SemanticAvailability::CatchingUp
            );
        }
        if change == "ambiguous" {
            assert_eq!(native.declarations.iter().filter(|d|
                d.document.path == "src/B.js" && d.name.as_deref() == Some("target")).count(), 2);
        }
        let result = response
            .graph_query(
                &GraphQuery {
                    root_syntax_id: run.clone(),
                    semantic_producer_id: Some("scip-typescript".into()),
                    depth: 3,
                    max_nodes: 20,
                    max_calls: 20,
                    include_external: true,
                    include_tests: true,
                },
                Some(pin),
                &mut || Ok(()),
            )
            .unwrap()
            .outcome
            .unwrap();
        assert_eq!(result.edges.len(), 2);
        assert!(result.partial);
        if restored
            && result
                .edges
                .iter()
                .any(|edge| Some(&edge.call.id) == dependent.call_id.as_ref() && edge.to.is_some())
        {
            let target = result
                .semantic_documents
                .iter()
                .find(|document| document.document.path == "src/B.js")
                .unwrap();
            assert_eq!(
                target.semantic_status.state,
                SemanticAvailability::CatchingUp
            );
        }
        let safe = result
            .edges
            .iter()
            .find(|e| Some(&e.call.id) == independent.call_id.as_ref())
            .unwrap();
        assert_eq!(safe.binding.as_ref().unwrap(), &independent);
        assert!(result.provenance.contains(&proof));
        for edge in &result.edges {
            if freshness != semantic::Freshness::Fresh
                || (!restored && Some(&edge.call.id) == dependent.call_id.as_ref())
            {
                assert_eq!(edge.visit, Visit::Boundary);
                assert_eq!(edge.to, None);
            }
        }
        if !restored {
            let edge = result
                .edges
                .iter()
                .find(|e| Some(&e.call.id) == dependent.call_id.as_ref())
                .unwrap();
            assert!(edge.binding.is_none());
            assert_eq!(edge.boundary_reason, BoundaryReason::MissingEvidence);
            assert_eq!(safe.boundary_reason, BoundaryReason::Stale);
            assert_eq!(result.nodes.len(), 1);
        }
        response.finish(()).unwrap();
        assert_eq!(store.semantic_at(original_pin).unwrap(), original);
        assert_eq!(store.semantic_at(first_pin).unwrap(), first);
    }
}

#[test]
fn raw_import_native_first_changed_target_omits_only_dependent_facts() {
    raw_target_native_first_case("changed");
}

#[test]
fn raw_import_native_first_deleted_target_omits_only_dependent_facts() {
    raw_target_native_first_case("deleted");
}

#[test]
fn raw_import_native_first_ambiguous_target_does_not_rebind() {
    raw_target_native_first_case("ambiguous");
}

#[test]
fn raw_import_native_first_restored_target_keeps_pending_state_and_pins() {
    raw_target_native_first_case("restored");
}
