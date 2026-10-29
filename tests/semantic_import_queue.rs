#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};
use tempfile::Builder;
use trellis::{native_ids, store::topology::TopologyRoots};

#[path = "support/semantic_batch.rs"]
mod semantic_batch_support;
use semantic_batch_support::{BatchFixture, DocSpec};

const WIRE: &str = "122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166";

struct CrashDaemon(std::process::Child);
impl CrashDaemon {
    fn start(mut command: Command, home: &Path) -> Self {
        let mut daemon = Self(
            command
                .arg("daemon")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let data = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/dev.squashmerge.trellis")
        } else {
            home.join(".local/share/trellis")
        };
        let socket = trellis::daemon::SocketPaths::new(&data).socket;
        trellis::testing::wait_until("crash fixture daemon listening", || {
            assert!(daemon.0.try_wait().unwrap().is_none());
            std::os::unix::net::UnixStream::connect(&socket).is_ok()
        });
        daemon
    }

    fn kill_and_wait(&mut self, home: &Path) {
        use std::os::unix::process::ExitStatusExt;
        let elected: u32 = fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(self.0.id(), elected);
        self.0.kill().unwrap();
        // CLI EOF does not prove the daemon has exited.
        // Reap the daemon itself before attempting the recovery mutation.
        assert_eq!(self.0.wait().unwrap().signal(), Some(libc::SIGKILL));
    }
}
impl Drop for CrashDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

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
    let output = cli(home).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Writes a reference-producer batch of hand-built artifacts into the fresh directory
/// `root/<dir>`. Each artifact is `(language, covered path, SCIP bytes)`; `excluded` entries
/// are `(language, path, reason)`. Returns the descriptor path and its SHA-256.
fn write_batch(
    index_db: &Path,
    root: &Path,
    producer: &str,
    artifacts: &[(&str, &str, Vec<u8>)],
    excluded: &[(&str, &str, &'static str)],
    dir: &str,
) -> (std::path::PathBuf, String) {
    let mut fixture = BatchFixture::from_published(index_db, root).reference(producer);
    for (language, path, bytes) in artifacts {
        let ordinal = fixture.artifact(language, vec![DocSpec::new(path)]);
        fixture.override_artifact_raw(ordinal, bytes.clone());
    }
    for (language, path, reason) in excluded {
        fixture.exclude(language, path, reason);
    }
    fixture.write(&root.join(dir))
}

fn two_document_case(change_b: &str) {
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
    let a = "function run(){ target(); }\n";
    let b = "function target(){ return 1; }\n";
    fs::write(root.join("src/A.js"), a).unwrap();
    fs::write(root.join("src/B.js"), b).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let root_string = root.to_str().unwrap();
    let mut crash_daemon = None;
    let indexed = if matches!(
        change_b,
        "ambiguous"
            | "ambiguous_a"
            | "missing_a"
            | "cutoff"
            | "whole_cas"
            | "sql_fault"
            | "crash_claim"
            | "crash_published"
            | "terminal_busy"
            | "report_bound"
            | "near_bound"
            | "publish_busy"
            | "commit_ambiguous"
    ) {
        let mut cmd = cli(&home);
        if change_b == "ambiguous" {
            cmd.env("TRELLIS_TEST_SEMANTIC_DUPLICATE_B_TARGET", "1");
        }
        if change_b == "ambiguous_a" {
            cmd.env("TRELLIS_TEST_SEMANTIC_DUPLICATE_A_REFERENCE", "1");
        }
        if change_b == "missing_a" {
            cmd.env("TRELLIS_TEST_SEMANTIC_MISSING_A_REFERENCE", "1");
        }
        if matches!(change_b, "cutoff" | "whole_cas") {
            cmd.env(
                "TRELLIS_TEST_SEMANTIC_CUTOFF_SOCKET",
                home.join("candidate-cutoff.sock"),
            );
        }
        if change_b == "sql_fault" {
            cmd.env(
                "TRELLIS_TEST_SEMANTIC_SQL_FAULT",
                home.join("sql-fault-once.marker"),
            );
        }
        if change_b == "crash_claim" {
            cmd.env(
                "TRELLIS_TEST_SEMANTIC_CRASH_AFTER_CLAIM_SOCKET",
                home.join("semantic-claimed.sock"),
            );
        }
        if change_b == "crash_published" {
            cmd.env(
                "TRELLIS_TEST_SEMANTIC_CRASH_AFTER_PUBLISH_SOCKET",
                home.join("semantic-published.sock"),
            );
        }
        if change_b == "terminal_busy" {
            cmd.env("TRELLIS_TEST_SEMANTIC_AMBIGUOUS_TERMINAL", "1");
        }
        if change_b == "report_bound" {
            cmd.env("TRELLIS_TEST_SEMANTIC_REPORT_PADDING", "1048576");
        }
        if change_b == "near_bound" {
            cmd.env("TRELLIS_TEST_SEMANTIC_REPORT_PADDING", "1047500");
        }
        if change_b == "publish_busy" {
            cmd.env(
                "TRELLIS_TEST_SEMANTIC_BUSY_ONCE",
                home.join("semantic-busy-once.marker"),
            );
        }
        if change_b == "commit_ambiguous" {
            cmd.env(
                "TRELLIS_TEST_SEMANTIC_AMBIGUOUS_COMMIT",
                home.join("semantic-commit.marker"),
            );
        }
        let output = if matches!(change_b, "crash_claim" | "crash_published") {
            crash_daemon = Some(CrashDaemon::start(cmd, &home));
            cli(&home)
                .args(["index", "--workspace", root_string])
                .output()
                .unwrap()
        } else {
            cmd.args(["index", "--workspace", root_string])
                .output()
                .unwrap()
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    } else {
        run(&home, &["index", "--workspace", root_string])
    };
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
    if change_b == "zero_claims" {
        for document in &mut binary.documents {
            document.occurrences.clear();
            document.symbols.clear();
        }
    }
    let exclusion_reason = match change_b {
        "partial_empty_covered" => Some("notRequested"),
        "partial_not_requested" => Some("notRequested"),
        "partial_unsupported" => Some("unsupported"),
        "partial_failed" => Some("failed"),
        _ => None,
    };
    let partial = exclusion_reason.is_some();
    if partial {
        binary.documents.retain(|d| d.relative_path == "src/A.js");
        if change_b == "partial_empty_covered" {
            binary.documents[0].occurrences.clear();
            binary.documents[0].symbols.clear();
        }
    }
    let bytes = binary.write_to_bytes().unwrap();
    let artifacts = vec![("javascript", "src/A.js", bytes.clone())];
    let mut excluded = vec![];
    if partial {
        excluded.push(("javascript", "src/B.js", exclusion_reason.unwrap()));
    }
    let (descriptor_path, digest) = if partial {
        write_batch(
            &index_db,
            &root,
            "scip-typescript",
            &artifacts,
            &excluded,
            ".trellis-semantic-batch",
        )
    } else {
        let mut fixture =
            BatchFixture::from_published(&index_db, &root).reference("scip-typescript");
        let ordinal = fixture.artifact(
            "javascript",
            vec![DocSpec::new("src/A.js"), DocSpec::new("src/B.js")],
        );
        fixture.override_artifact_raw(ordinal, bytes.clone());
        fixture.write(&root.join(".trellis-semantic-batch"))
    };
    let args = [
        "semantic",
        "import",
        "--workspace",
        root_string,
        "--batch",
        descriptor_path.to_str().unwrap(),
        "--batch-sha256",
        &digest,
    ];
    let captured = if change_b == "crash_published" {
        use std::time::{Duration, Instant};
        let socket = home.join("semantic-published.sock");
        let mut child = cli(&home)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !socket.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(12),
                "semantic publication barrier not reached"
            );
            std::thread::yield_now();
        }
        let published = store.index_baseline().unwrap();
        assert!(published.index_revision > r1.index_revision);
        let original = store.earliest_unfinished_request().unwrap().unwrap();
        assert_eq!(original.state, "running");
        assert!(
            store
                .semantic_batch_report_at(published, &digest)
                .unwrap()
                .is_some()
        );
        crash_daemon.as_mut().unwrap().kill_and_wait(&home);
        let _ = child.wait();
        fs::write(socket.with_extension("done"), b"crash injected").unwrap();
        fs::remove_file(socket).unwrap();
        let mut recovering = cli(&home)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let output = loop {
            if recovering.try_wait().unwrap().is_some() {
                break recovering.wait_with_output().unwrap();
            }
            if start.elapsed() > Duration::from_secs(10) {
                recovering.kill().unwrap();
                panic!(
                    "semantic publication recovery stalled: original={:?} head={:?}",
                    store.request_by_id(&original.id),
                    store.earliest_unfinished_request()
                );
            }
            std::thread::yield_now();
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let committed: Value = serde_json::from_slice(&output.stdout).unwrap();
        let replayed = store.request_by_id(&original.id).unwrap().unwrap();
        assert_eq!(replayed.state, "done");
        assert!(replayed.revision.unwrap().index_revision >= published.index_revision);
        assert!(
            store
                .semantic_batch_report_at(replayed.revision.unwrap(), &digest)
                .unwrap()
                .is_some()
        );
        committed
    } else if change_b == "commit_ambiguous" {
        let before = store.index_baseline().unwrap();
        let first = cli(&home).args(args).output().unwrap();
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert!(home.join("semantic-commit.marker").exists());
        let committed: Value = serde_json::from_slice(&first.stdout).unwrap();
        let after: trellis::model::IndexPin =
            serde_json::from_value(committed["publishedRevision"].clone()).unwrap();
        assert!(
            after.index_revision > before.index_revision,
            "semantic COMMIT and permitted replay may advance the index"
        );
        let original = store
            .request_by_id(committed["requestId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(original.state, "done");
        assert_eq!(original.error_code, None);
        assert_eq!(original.revision, Some(after));
        let selected = store.semantic_at(after).unwrap();
        assert_eq!(
            selected
                .call_bindings
                .iter()
                .filter(|call| call.join.anchor.document.path == "src/A.js")
                .count(),
            1,
            "replay replaces selected producer overlay, never duplicates facts"
        );
        assert_eq!(
            store
                .semantic_batch_report_at(after, &digest)
                .unwrap()
                .unwrap(),
            committed["report"]
        );
        committed
    } else if change_b == "publish_busy" {
        let committed = run(&home, &args);
        assert!(home.join("semantic-busy-once.marker").exists());
        let row = store
            .request_by_id(committed["requestId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "done");
        assert_eq!(row.error_code, None);
        assert_eq!(
            row.revision,
            Some(serde_json::from_value(committed["publishedRevision"].clone()).unwrap())
        );
        committed
    } else if change_b == "near_bound" {
        let output = cli(&home).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let committed: Value = serde_json::from_slice(&output.stdout).unwrap();
        let report = serde_json::to_vec(&committed["report"]).unwrap();
        assert!(
            report.len() < 1024 * 1024 && report.len() > 1024 * 1024 - 1024,
            "{}",
            report.len()
        );
        assert!(
            output.stdout.len() > 1024 * 1024,
            "chunked daemon reply must carry envelope: report {} stdout {}",
            report.len(),
            output.stdout.len()
        );
        assert_eq!(
            store
                .semantic_batch_report_at(
                    serde_json::from_value(committed["publishedRevision"].clone()).unwrap(),
                    &digest
                )
                .unwrap()
                .unwrap(),
            committed["report"]
        );
        return;
    } else if change_b == "report_bound" {
        let before = store.index_baseline().unwrap();
        let prior = store.semantic_at(before).unwrap();
        let output = cli(&home).args(args).output().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("index_failed"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let after = store.index_baseline().unwrap();
        assert!(
            after.index_revision >= before.index_revision,
            "native startup reconciliation may advance before semantic rejection"
        );
        let selected = store.semantic_at(after).unwrap();
        assert_eq!(
            selected, prior,
            "report bound cannot publish any semantic overlay"
        );
        assert!(
            store
                .semantic_batch_report_at(after, &digest)
                .unwrap()
                .is_none()
        );
        assert!(
            selected
                .provenance
                .iter()
                .all(|p| p.producer_id != "scip-typescript")
        );
        assert!(selected.producers.iter().all(|p| p.id != "scip-typescript"));
        return;
    } else if change_b == "terminal_busy" {
        let committed = run(&home, &args);
        let row = store
            .request_by_id(committed["requestId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "done");
        assert_eq!(row.error_code, None);
        assert_eq!(
            row.revision,
            Some(serde_json::from_value(committed["publishedRevision"].clone()).unwrap())
        );
        assert_eq!(
            store
                .semantic_batch_report_at(row.revision.unwrap(), &digest)
                .unwrap()
                .unwrap(),
            committed["report"]
        );
        committed
    } else if change_b == "crash_claim" {
        use std::time::{Duration, Instant};
        let socket = home.join("semantic-claimed.sock");
        let mut child = cli(&home)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !socket.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "semantic claim barrier not reached"
            );
            std::thread::yield_now();
        }
        let claimed = store.earliest_unfinished_request().unwrap().unwrap();
        assert_eq!(claimed.state, "running");
        crash_daemon.as_mut().unwrap().kill_and_wait(&home);
        let _ = child.wait();
        let marker = home.join("semantic-claimed.sock");
        assert!(marker.exists());
        fs::write(marker.with_extension("done"), b"crash injected").unwrap();
        fs::remove_file(marker).unwrap();
        let mut repeated_child = cli(&home)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let output = loop {
            if repeated_child.try_wait().unwrap().is_some() {
                break repeated_child.wait_with_output().unwrap();
            }
            let row = store.request_by_id(&claimed.id).unwrap().unwrap();
            if start.elapsed() > Duration::from_secs(8) {
                eprintln!(
                    "RECOVERY_FIXTURE={} CLAIMED={claimed:?} DAEMON_PID={:?} ORIGINAL_ROW={row:?} NEXT_ROW={:?} MARKER={:?}",
                    temp.path().display(),
                    fs::read_to_string(home.join("daemon.pid")),
                    store.earliest_unfinished_request(),
                    fs::read(home.join("semantic-claimed.done"))
                );
                repeated_child.kill().unwrap();
                let output = repeated_child.wait_with_output().unwrap();
                eprintln!(
                    "REPEATED_STDERR={}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let _ = temp.keep();
                panic!("recovery CLI stuck");
            }
            std::thread::yield_now();
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let repeated: Value = serde_json::from_slice(&output.stdout).unwrap();
        let resumed = store.request_by_id(&claimed.id).unwrap().unwrap();
        assert_eq!(resumed.state, "done");
        assert_ne!(
            resumed.claim_incarnation, claimed.claim_incarnation,
            "recovery must reclaim the same accepted row under a fresh owner"
        );
        assert!(resumed.revision.is_some());
        assert!(
            store
                .semantic_batch_report_at(resumed.revision.unwrap(), &digest)
                .unwrap()
                .is_some()
        );
        assert!(repeated["report"].is_object());
        repeated
    } else if change_b == "sql_fault" {
        let output = cli(&home).args(args).output().unwrap();
        assert!(!output.status.success());
        let rolled_back = store.index_baseline().unwrap();
        assert!(
            store
                .selected_raw_inputs_at(rolled_back)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .semantic_at(rolled_back)
                .unwrap()
                .call_bindings
                .is_empty()
        );
        assert!(
            store
                .semantic_batch_report_at(rolled_back, &digest)
                .unwrap()
                .is_none()
        );
        let failed = String::from_utf8_lossy(&output.stderr);
        assert!(
            failed.contains("storage") || failed.contains("failed") || failed.contains("error"),
            "{failed}"
        );
        run(&home, &args)
    } else {
        run(&home, &args)
    };
    let captured_pin: trellis::model::IndexPin =
        serde_json::from_value(captured["publishedRevision"].clone()).unwrap();
    let captured_semantic = store.semantic_at(captured_pin).unwrap();
    if change_b == "zero_claims" {
        assert!(captured_semantic.provenance.is_empty());
        let selected = store.selected_raw_inputs_at(captured_pin).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(
            store
                .retained_native_snapshot_at(selected[0].captured_pin)
                .unwrap()
                .0
                .revision
                .id,
            native.revision.id
        );
        assert!(selected[0].proofs.is_empty());
        assert_eq!(selected[0].captures[0].binary_bytes, bytes);
        assert_eq!(selected[0].requested_documents.len(), 2);
        return;
    }
    if partial {
        if change_b == "partial_empty_covered" {
            let a = captured_semantic
                .coverage
                .iter()
                .find(|row| row.document_path == "src/A.js")
                .unwrap();
            assert_eq!(a.state, trellis::semantic_evidence::CoverageState::Complete);
            assert!(a.requested && a.selected);
            assert!(a.diagnostic.is_none());
            assert_eq!(captured["report"]["languages"][0]["requested"], 0);
        }
        let b = captured_semantic
            .coverage
            .iter()
            .find(|row| row.document_path == "src/B.js")
            .unwrap();
        let (state, requested, selected, diagnostic) = match exclusion_reason.unwrap() {
            "notRequested" => (
                trellis::semantic_evidence::CoverageState::NotRequested,
                false,
                false,
                None,
            ),
            "unsupported" => (
                trellis::semantic_evidence::CoverageState::Omitted,
                true,
                false,
                Some("excluded_by_producer"),
            ),
            "failed" => (
                trellis::semantic_evidence::CoverageState::Failed,
                true,
                true,
                Some("excluded_failed_by_producer"),
            ),
            _ => unreachable!(),
        };
        assert_eq!(b.state, state);
        assert_eq!((b.requested, b.selected), (requested, selected));
        assert_eq!(b.diagnostic.as_deref(), diagnostic);
        assert!(
            !captured_semantic
                .provenance
                .iter()
                .any(|row| row.document.path == "src/B.js")
        );
        assert!(
            captured["report"].get("documents").is_none(),
            "the report carries no per-document rows"
        );
        return;
    }
    assert!(
        captured_semantic
            .provenance
            .iter()
            .filter(|p| p.producer_id == "scip-typescript")
            .all(|p| p.freshness == trellis::semantic_evidence::Freshness::Fresh)
    );
    let captured_call = captured_semantic
        .call_bindings
        .iter()
        .find(|c| c.join.anchor.document.path == "src/A.js")
        .unwrap();
    let captured_id = captured_call.call_id.clone();
    let captured_proof = captured_call.provenance_id.clone();
    let r1_b_syntax = native
        .declarations
        .iter()
        .find(|d| d.document.path == "src/B.js" && d.name.as_deref() == Some("target"))
        .unwrap()
        .syntax_id
        .clone();
    if change_b == "added" {
        fs::write(root.join("src/C.js"), "function unrelated(){ return 1; }\n").unwrap();
    } else if change_b == "removed" {
        fs::remove_file(root.join("src/B.js")).unwrap();
    } else if change_b == "missing_a_real" {
        fs::write(root.join("src/A.js"), "function run(){ missing(); }\n").unwrap();
    } else if !matches!(
        change_b,
        "same"
            | "sql_fault"
            | "crash_claim"
            | "crash_published"
            | "terminal_busy"
            | "near_bound"
            | "publish_busy"
            | "commit_ambiguous"
            | "whole_cas"
            | "ambiguous_a"
            | "missing_a"
    ) {
        let changed = if matches!(change_b, "unique" | "ambiguous") {
            "function target(){ return 2; }\n"
        } else {
            "function missing(){ return 2; }\n"
        };
        fs::write(root.join("src/B.js"), changed).unwrap();
    } else {
        fs::write(root.join("package.json"), "{\"changed\":true}\n").unwrap();
    }
    let advanced = run(&home, &["index", "--workspace", root_string]);
    let advanced_pin: trellis::model::IndexPin =
        serde_json::from_value(advanced["publishedRevision"].clone()).unwrap();
    let (r2_native, _) = store.retained_native_snapshot_at(advanced_pin).unwrap();
    if change_b == "unique" {
        let r2_b = r2_native
            .declarations
            .iter()
            .find(|d| d.document.path == "src/B.js" && d.name.as_deref() == Some("target"))
            .unwrap();
        assert_eq!(r2_b.syntax_id, r1_b_syntax);
        assert_ne!(r2_native.revision.id, native.revision.id);
    }
    let joined = if matches!(change_b, "cutoff" | "whole_cas") {
        use std::os::unix::net::UnixStream;
        use std::time::{Duration, Instant};
        let socket = home.join("candidate-cutoff.sock");
        let mut command = cli(&home);
        let child = command
            .env("TRELLIS_TEST_SEMANTIC_CUTOFF_SOCKET", &socket)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !socket.with_extension("ready").exists() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "semantic candidate cutoff was not reached"
            );
            std::thread::yield_now();
        }
        let before = store.index_baseline().unwrap();
        assert_eq!(before, advanced_pin);
        if change_b == "cutoff" {
            fs::write(
                root.join("src/A.js"),
                "function run(){ target(); } // changed after candidate\n",
            )
            .unwrap();
        } else {
            fs::write(
                root.join("src/C.js"),
                "function added_late(){ return 2; }\n",
            )
            .unwrap();
        }
        let stream = UnixStream::connect(&socket).unwrap();
        drop(stream);
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        let committed: trellis::model::IndexPin =
            serde_json::from_value(response["publishedRevision"].clone()).unwrap();
        assert!(
            committed.index_revision >= advanced_pin.index_revision + 2,
            "native reconciliation must precede semantic publication"
        );
        assert!(
            store
                .semantic_batch_report_at(advanced_pin, &digest)
                .unwrap()
                .is_none()
        );
        response
    } else if matches!(change_b, "ambiguous" | "ambiguous_a" | "missing_a") {
        let mut command = cli(&home);
        let hook = match change_b {
            "ambiguous" => "TRELLIS_TEST_SEMANTIC_DUPLICATE_B_TARGET",
            "ambiguous_a" => "TRELLIS_TEST_SEMANTIC_DUPLICATE_A_REFERENCE",
            _ => "TRELLIS_TEST_SEMANTIC_MISSING_A_REFERENCE",
        };
        let output = command.env(hook, "1").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    } else {
        run(&home, &args)
    };
    let joined_pin: trellis::model::IndexPin =
        serde_json::from_value(joined["publishedRevision"].clone()).unwrap();
    let selected = store.semantic_at(joined_pin).unwrap();
    let selected_inputs = store.selected_raw_inputs_at(joined_pin).unwrap();
    assert_eq!(selected_inputs.len(), 1);
    let input = &selected_inputs[0];
    assert_eq!(input.producer_id, "scip-typescript");
    assert_eq!(
        store
            .retained_native_snapshot_at(input.captured_pin)
            .unwrap()
            .0
            .revision
            .id,
        native.revision.id
    );
    assert_eq!(input.captures[0].binary_bytes, bytes);
    assert!(!input.proofs.is_empty());
    assert!(
        input
            .requested_documents
            .iter()
            .any(|d| d.path == "src/A.js")
    );
    if !matches!(
        change_b,
        "cutoff" | "missing_a_real" | "missing_a" | "ambiguous_a"
    ) {
        let independent = selected
            .declaration_bindings
            .iter()
            .find(|d| d.join.anchor.document.path == "src/A.js" && d.join.anchor.range.start == 9)
            .unwrap();
        assert_ne!(independent.provenance_id, captured_proof);
    }
    let state_of = |path: &str| {
        serde_json::to_value(
            selected
                .coverage
                .iter()
                .find(|c| c.document_path == path)
                .unwrap()
                .state,
        )
        .unwrap()
    };
    let language_report = &joined["report"]["languages"][0];
    if matches!(change_b, "missing_a_real" | "missing_a" | "ambiguous_a") {
        let a_key = &native
            .revision
            .documents
            .iter()
            .find(|d| d.key.path == "src/A.js")
            .unwrap()
            .key;
        let a_syntax = native
            .declarations
            .iter()
            .find(|d| d.document == *a_key && d.name.as_deref() == Some("run"))
            .unwrap()
            .syntax_id
            .as_str();
        let history = store
            .semantic_history_for(joined_pin, "scip-typescript", a_key, a_syntax)
            .unwrap()
            .expect("previous selected A tuple retained for history");
        assert!(matches!(
            history.coverage.state,
            trellis::semantic_evidence::CoverageState::Complete
                | trellis::semantic_evidence::CoverageState::Partial
        ));
        assert!(
            !history.provenance.is_empty(),
            "history is separate from current failed A selection"
        );
        assert_eq!(
            history.presentation_freshness,
            if change_b == "missing_a_real" {
                trellis::semantic_evidence::Freshness::Stale
            } else {
                trellis::semantic_evidence::Freshness::PossiblyStale
            }
        );
    }
    if change_b == "whole_cas" {
        assert_eq!(state_of("src/A.js"), "complete");
        assert_eq!(
            selected
                .call_bindings
                .iter()
                .filter(|c| c.join.anchor.document.path == "src/A.js")
                .count(),
            1
        );
        let (head, _) = store.retained_native_snapshot_at(joined_pin).unwrap();
        assert_ne!(head.revision.id, r2_native.revision.id);
        assert!(
            head.revision
                .documents
                .iter()
                .any(|d| d.key.path == "src/C.js")
        );
    } else if matches!(
        change_b,
        "cutoff" | "missing_a_real" | "missing_a" | "ambiguous_a"
    ) {
        assert_eq!(state_of("src/A.js"), "failed");
        assert!(
            language_report["skipped"].as_u64().unwrap() > 0,
            "the failed A document skips its requested claims"
        );
        assert!(
            selected
                .call_bindings
                .iter()
                .all(|c| c.join.anchor.document.path != "src/A.js")
        );
        assert!(
            selected
                .declaration_bindings
                .iter()
                .all(|d| d.join.anchor.document.path != "src/A.js")
        );
        assert!(
            selected
                .references
                .iter()
                .all(|r| r.document.path != "src/A.js")
        );
        if change_b == "cutoff" {
            let (head, _) = store.retained_native_snapshot_at(joined_pin).unwrap();
            assert_ne!(head.revision.id, r2_native.revision.id);
        }
    } else if change_b == "removed" {
        assert_eq!(state_of("src/A.js"), "partial");
        assert!(
            selected
                .call_bindings
                .iter()
                .all(|c| c.join.anchor.document.path != "src/A.js")
        );
        assert!(
            selected
                .coverage
                .iter()
                .all(|c| c.document_path != "src/B.js"),
            "removed B has no selected destination document to cover"
        );
    } else if matches!(change_b, "missing" | "ambiguous") {
        assert_eq!(state_of("src/A.js"), "partial");
        assert_eq!(language_report["matched"], 1);
        assert_eq!(language_report["skipped"], 2);
        assert!(
            selected
                .call_bindings
                .iter()
                .all(|c| c.join.anchor.document.path != "src/A.js")
        );
    } else {
        let call = selected
            .call_bindings
            .iter()
            .find(|c| c.join.anchor.document.path == "src/A.js")
            .unwrap();
        assert_eq!(call.call_id, captured_id);
        assert_ne!(call.provenance_id, captured_proof);
        let provenance = selected
            .provenance
            .iter()
            .find(|p| p.id == call.provenance_id)
            .unwrap();
        let derived = provenance.derived_from.as_ref().unwrap();
        assert_eq!(provenance.revision_id, r2_native.revision.id);
        assert_eq!(
            provenance.basis.as_ref().unwrap().revision_id,
            native.revision.id
        );
        let raw = store
            .semantic_raw_proof_at(
                "scip-typescript",
                &native_ids::digest(b"", &bytes),
                &native.revision.source_set_id,
                &native.revision.id,
                &derived.provenance_id,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            raw.record_kind,
            serde_json::to_value(derived.record_kind)
                .unwrap()
                .as_str()
                .unwrap()
        );
        assert_eq!(raw.fact_digest, derived.fact_digest);
        assert_eq!(raw.path, "src/A.js");
        let envelope: Value = serde_json::from_slice(&raw.canonical_bytes).unwrap();
        assert_eq!(envelope["relativePath"], "src/A.js");
        assert_eq!(call.join.anchor.revision_id, r2_native.revision.id);
        assert_eq!(state_of("src/A.js"), "complete");
        if change_b == "unique" {
            assert!(
                matches!(&call.declared_target,Some(trellis::semantic_evidence::Target::Internal{revision_id,syntax_id,..}) if revision_id==&r2_native.revision.id && syntax_id==&r1_b_syntax)
            );
            assert_eq!(state_of("src/B.js"), "failed");
        }
    }
    if let Ok(pid) = fs::read_to_string(home.join("daemon.pid"))
        && let Ok(pid) = pid.parse::<i32>()
    {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

#[test]
fn partial_manifest_excluded_b_is_not_requested_in_selected_store_and_report() {
    two_document_case("partial_not_requested");
}
#[test]
fn partial_manifest_excluded_unsupported_is_omitted_not_native_capability() {
    two_document_case("partial_unsupported");
}
#[test]
fn accepted_zero_claim_capture_is_selected_without_provenance() {
    two_document_case("zero_claims");
}

#[test]
fn partial_manifest_empty_covered_a_remains_complete_without_claims() {
    two_document_case("partial_empty_covered");
}
#[test]
fn partial_manifest_excluded_failed_is_not_complete() {
    two_document_case("partial_failed");
}
#[test]
fn unchanged_a_rejoins_after_native_r2_with_new_proof_and_equal_id() {
    two_document_case("same");
}
#[test]
fn changed_b_unique_same_id_retargets_a_call() {
    two_document_case("unique");
}
#[test]
fn missing_b_target_omits_only_dependent_a_call() {
    two_document_case("missing");
}
#[test]
fn synthetic_nonunique_b_target_omits_only_dependent_a_call() {
    two_document_case("ambiguous");
}
#[test]
fn changed_a_source_fails_entire_document_without_selected_call() {
    two_document_case("missing_a_real");
}

#[test]
fn missing_required_a_reference_candidate_fails_whole_a_document() {
    two_document_case("missing_a");
}

#[test]
fn synthetic_nonunique_required_a_reference_fails_whole_a_document() {
    two_document_case("ambiguous_a");
}

#[test]
fn source_edit_after_semantic_candidate_reconciles_native_before_same_request_commit() {
    two_document_case("cutoff");
}
#[test]
fn semantic_publication_sql_fault_rolls_back_and_retry_commits() {
    two_document_case("sql_fault");
}
#[test]
fn running_semantic_import_reclaims_same_request_after_daemon_crash() {
    two_document_case("crash_claim");
}
#[test]
fn ambiguous_terminal_commit_preserves_semantic_request_and_report() {
    two_document_case("terminal_busy");
}
#[test]
fn oversized_report_rejected_before_semantic_commit() {
    two_document_case("report_bound");
}
#[test]
fn near_bound_valid_report_commits_and_chunked_cli_reply_survives() {
    two_document_case("near_bound");
}
#[test]
fn semantic_publish_busy_requeues_same_accepted_request_and_commits_once() {
    two_document_case("publish_busy");
}
#[test]
fn ambiguous_semantic_publication_commit_replays_atomically_without_duplicate_facts() {
    two_document_case("commit_ambiguous");
}
#[test]
fn crash_after_semantic_publication_before_terminal_replays_atomically() {
    two_document_case("crash_published");
}
#[test]
fn added_unrelated_document_preserves_a_and_b_rejoin() {
    two_document_case("added");
}
#[test]
fn removed_b_document_omits_dependent_a_call_and_destination_b_coverage() {
    two_document_case("removed");
}
#[test]
fn whole_capture_changed_after_candidate_reconciles_before_same_import() {
    two_document_case("whole_cas");
}

#[test]
fn explicit_import_publishes_a_committed_report_through_daemon() {
    let temp = Builder::new()
        .prefix("semanticq-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/A.js"), "function f(){g();}\n").unwrap();
    let root = fs::canonicalize(&root).unwrap();
    let root_string = root.to_str().unwrap();
    let indexed = run(&home, &["index", "--workspace", root_string]);
    let pin: trellis::model::IndexPin =
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
    let store = trellis::store::Store::open(roots, identity).unwrap();
    let (native, _) = store.retained_native_snapshot_at(pin).unwrap();
    let binary = hex::decode(WIRE).unwrap();
    let (descriptor_path, digest) = write_batch(
        &index_db,
        &root,
        "scip-typescript",
        &[("javascript", "src/A.js", binary.clone())],
        &[],
        ".trellis-semantic-batch",
    );
    let descriptor = descriptor_path.display().to_string();
    // Start the daemon through the real CLI, then issue closed version-1 frames directly.
    let started = run(&home, &["status", "--workspace", root_string]);
    assert!(started.is_object());
    let socket = trellis::daemon::SocketPaths::new(&if cfg!(target_os = "macos") {
        home.join("Library/Application Support/dev.squashmerge.trellis")
    } else {
        home.join(".local/share/trellis")
    })
    .socket;
    std::os::unix::fs::symlink(&descriptor_path, root.join("linked.json")).unwrap();
    fs::write(temp.path().join("outside.json"), b"untouched outside").unwrap();
    let invalid = [
        json!({"version":2,"descriptor":"relative.json","descriptorSha256":digest}),
        json!({"version":2,"descriptor":temp.path().join("outside.json"),"descriptorSha256":digest}),
        json!({"version":2,"descriptor":root.join("linked.json"),"descriptorSha256":digest}),
        json!({"version":2,"descriptor":descriptor,"descriptorSha256":digest.to_uppercase()}),
        json!({"version":1,"scip":root.join("artifact.scip"),"manifest":descriptor,
            "profileId":"mac-mini-m5-pro-v1","producerName":"scip-typescript","manifestSha256":digest}),
    ];
    let queue_rows = || -> Vec<(String, String)> {
        let db = rusqlite::Connection::open_with_flags(
            store.request_db_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.prepare("SELECT id,state FROM requests ORDER BY seq")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    for args in invalid {
        let prior_rows = queue_rows();
        use std::os::unix::net::UnixStream;
        use trellis::daemon::protocol;
        let mut stream = UnixStream::connect(&socket).unwrap();
        protocol::write_frame(
            &mut stream,
            &protocol::Request {
                id: 881,
                operation: "semantic_import".into(),
                payload: json!({"workspace":root,"args":args}),
            },
        )
        .unwrap();
        let reply: protocol::Reply = protocol::read_frame(&mut stream).unwrap();
        assert!(
            reply.payload["error"]
                .as_str()
                .is_some_and(|error| error.contains("invalid_request")),
            "{reply:?}"
        );
        assert_eq!(
            queue_rows(),
            prior_rows,
            "invalid direct request must not create a durable row"
        );
        assert!(store.earliest_unfinished_request().unwrap().is_none());
    }
    // A descriptor above the 1 MiB cap must fail after FIFO acceptance.
    let oversized_path = root.join("oversized-descriptor.json");
    let oversized_bytes = vec![b' '; 1024 * 1024 + 1];
    fs::write(&oversized_path, &oversized_bytes).unwrap();
    let prior_rows = queue_rows();
    let oversized = cli(&home)
        .args([
            "semantic",
            "import",
            "--workspace",
            root_string,
            "--batch",
            oversized_path.to_str().unwrap(),
            "--batch-sha256",
            &native_ids::digest(b"", &oversized_bytes),
        ])
        .output()
        .unwrap();
    assert!(!oversized.status.success());
    let stderr = String::from_utf8_lossy(&oversized.stderr);
    assert!(
        stderr.contains("limit_exceeded: request ") && stderr.contains(" failed: descriptor bytes"),
        "{stderr}"
    );
    let after_rows = queue_rows();
    assert_eq!(
        after_rows.len(),
        prior_rows.len() + 1,
        "oversize request must have been accepted"
    );
    assert_eq!(after_rows.last().unwrap().1, "failed");
    let failed_row = store
        .request_by_id(&after_rows.last().unwrap().0)
        .unwrap()
        .unwrap();
    assert_eq!(
        failed_row.error_code.as_deref(),
        Some("limit_exceeded: descriptor bytes")
    );
    assert!(store.earliest_unfinished_request().unwrap().is_none());
    let baseline = store.index_baseline().unwrap();
    assert!(store.semantic_at(baseline).unwrap().provenance.is_empty());
    assert!(
        store
            .semantic_batch_report_at(baseline, &digest)
            .unwrap()
            .is_none()
    );
    let original_components = store.retained_native_components_at(pin).unwrap();
    fs::write(root.join("package.json"), "{\"changed\":true}\n").unwrap();
    let retained_components = store.retained_native_components_at(pin).unwrap();
    assert_eq!(original_components, retained_components);
    let advanced = run(&home, &["index", "--workspace", root_string]);
    assert!(
        advanced["publishedRevision"]["indexRevision"]
            .as_u64()
            .unwrap()
            > pin.index_revision
    );
    let imported = run(
        &home,
        &[
            "semantic",
            "import",
            "--workspace",
            root_string,
            "--batch",
            &descriptor,
            "--batch-sha256",
            &digest,
        ],
    );
    assert!(
        imported["publishedRevision"]["indexRevision"]
            .as_u64()
            .unwrap()
            > pin.index_revision,
        "{imported}"
    );
    assert_eq!(imported["report"]["producerId"], "scip-typescript");
    assert_eq!(imported["report"]["capturedRevision"], native.revision.id);
    assert_eq!(imported["report"]["descriptorSha256"], digest);
    assert_eq!(imported["report"]["languages"][0]["matched"], 1);
    let imported_pin: trellis::model::IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    assert_eq!(
        store
            .semantic_at(imported_pin)
            .unwrap()
            .coverage
            .iter()
            .find(|c| c.document_path == "src/A.js")
            .unwrap()
            .state,
        trellis::semantic_evidence::CoverageState::Complete
    );
    let first_report = &imported["report"];
    let repeated = run(
        &home,
        &[
            "semantic",
            "import",
            "--workspace",
            root_string,
            "--batch",
            &descriptor,
            "--batch-sha256",
            &digest,
        ],
    );
    assert_eq!(first_report["artifacts"], repeated["report"]["artifacts"]);
    assert_eq!(first_report["languages"], repeated["report"]["languages"]);
    let before_bad = store.index_baseline().unwrap();
    let rejected = cli(&home)
        .args([
            "semantic",
            "import",
            "--workspace",
            root_string,
            "--batch",
            &descriptor,
            "--batch-sha256",
            &"0".repeat(64),
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert_eq!(store.index_baseline().unwrap(), before_bad);
    fs::write(root.join("src/A.js"), "function f(){h();}\n").unwrap();
    let changed = run(&home, &["index", "--workspace", root_string]);
    let failed = run(
        &home,
        &[
            "semantic",
            "import",
            "--workspace",
            root_string,
            "--batch",
            &descriptor,
            "--batch-sha256",
            &digest,
        ],
    );
    assert!(
        failed["publishedRevision"]["indexRevision"]
            .as_u64()
            .unwrap()
            > changed["publishedRevision"]["indexRevision"]
                .as_u64()
                .unwrap()
    );
    assert_eq!(failed["report"]["languages"][0]["matched"], 0);
    let failed_pin: trellis::model::IndexPin =
        serde_json::from_value(failed["publishedRevision"].clone()).unwrap();
    assert_eq!(
        store
            .semantic_at(failed_pin)
            .unwrap()
            .coverage
            .iter()
            .find(|c| c.document_path == "src/A.js")
            .unwrap()
            .state,
        trellis::semantic_evidence::CoverageState::Failed
    );
    if let Ok(pid) = fs::read_to_string(home.join("daemon.pid"))
        && let Ok(pid) = pid.parse::<i32>()
    {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

#[test]
fn four_languages_import_through_cli_daemon_and_publish_reports() {
    use protobuf::{EnumOrUnknown, Message};
    for (language, path, producer, source, symbol, encoding) in [
        (
            "java",
            "A.java",
            "scip-java",
            "class Base { void f(int x) {} void f(String x) {} } class Child extends Base { @Override void f(int x) {} } class Cat implements java.lang.Comparable<Cat> { public int compareTo(Cat other) { return 0; } public String toString(){ return null; } }",
            "Base",
            2,
        ),
        (
            "javascript",
            "A.js",
            "scip-typescript",
            "function f(){ f(); }",
            "f",
            2,
        ),
        (
            "python",
            "A.py",
            "scip-python",
            "def f():\n    f()\n",
            "f",
            2,
        ),
        ("rust", "A.rs", "rust-analyzer", "fn f(){ f(); }", "f", 1),
    ] {
        for variant in if language == "java" {
            vec![0, 5]
        } else if matches!(language, "javascript" | "rust") {
            vec![0, 1, 2, 3, 4, 5, 6]
        } else {
            vec![0, 1, 4, 5, 6]
        } {
            let call_witness = variant == 1 || variant == 3;
            let alias_case = variant == 2;
            let ambiguous_call = variant == 3;
            let coordinate_case = variant == 4;
            let type_case = variant == 5;
            let write_case = variant == 6;
            let (source, symbol) = if alias_case {
                if language == "javascript" {
                    ("import {original as local} from 'pkg'; local;", "local")
                } else {
                    (
                        "use pkg::original as local; fn f(){ let _ = local; }",
                        "local",
                    )
                }
            } else if write_case {
                match language {
                    "javascript" => ("let f=0; f=1;", "f"),
                    "python" => ("f=0\nf=1\n", "f"),
                    "rust" => ("fn main(){ let mut f=0; f=1; }", "f"),
                    _ => unreachable!(),
                }
            } else if type_case {
                match language {
                    "java" => ("class Base {} class Child extends Base {}", "Base"),
                    "javascript" => ("class Base {} class Child extends Base {}", "Base"),
                    "python" => ("class Base: pass\nclass Child(Base): pass\n", "Base"),
                    "rust" => ("trait Base {} trait Child: Base {}", "Base"),
                    _ => unreachable!(),
                }
            } else if coordinate_case {
                match language {
                    "javascript" => ("const marker='😀';\r\nfunction f(){ f(); }", "f"),
                    "python" => ("marker='😀'\r\ndef f():\r\n    f()\r\n", "f"),
                    "rust" => ("const MARKER: &str = \"😀\";\r\nfn f(){ f(); }", "f"),
                    _ => unreachable!(),
                }
            } else {
                (source, symbol)
            };
            let variant_started = std::time::Instant::now();
            eprintln!("SEMANTIC_VARIANT_START language={language} variant={variant}");
            let temp = Builder::new()
                .prefix("semantic-language-")
                .tempdir_in("/tmp")
                .unwrap();
            let home = temp.path().join("home");
            fs::create_dir(&home).unwrap();
            let root = temp.path().join("checkout");
            fs::create_dir(&root).unwrap();
            fs::write(root.join(path), source).unwrap();
            let root = fs::canonicalize(root).unwrap();
            let root_string = root.to_str().unwrap();
            let indexed = run(&home, &["index", "--workspace", root_string]);
            let pin: trellis::model::IndexPin =
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
            let store = trellis::store::Store::open(roots, identity).unwrap();
            let start = source.rfind(symbol).unwrap();
            let line = source[..start].bytes().filter(|b| *b == b'\n').count();
            let prefix = source[..start].rsplit('\n').next().unwrap();
            let col = if encoding == 1 {
                prefix.len()
            } else {
                prefix.encode_utf16().count()
            };
            let mut scip = scip::types::Index::new();
            let mut document = scip::types::Document::new();
            document.language = language.into();
            document.relative_path = path.into();
            document.position_encoding =
                EnumOrUnknown::from_i32(if coordinate_case { 0 } else { encoding });
            let mut occurrence = scip::types::Occurrence::new();
            occurrence.range = vec![line as i32, col as i32, (col + symbol.len()) as i32];
            occurrence.symbol = if type_case {
                "global pkg Base#"
            } else if language == "java" {
                "scip-java pkg Base#"
            } else if alias_case {
                "local alias"
            } else {
                "external pkg f."
            }
            .into();
            occurrence.syntax_kind = EnumOrUnknown::from_i32(0);
            if language != "java" && !type_case {
                occurrence.symbol_roles = if write_case {
                    4
                } else if alias_case {
                    0
                } else if call_witness {
                    8
                } else {
                    0
                };
                if call_witness && !alias_case {
                    let close = source[start + symbol.len()..].find(')').unwrap()
                        + start
                        + symbol.len()
                        + 1;
                    let span = if encoding == 1 {
                        close - start
                    } else {
                        source[start..close].encode_utf16().count()
                    };
                    occurrence.enclosing_range = vec![line as i32, col as i32, (col + span) as i32];
                }
                let definition = if alias_case {
                    source.find("local").unwrap()
                } else if write_case {
                    if language == "rust" {
                        source.find("mut f").unwrap() + 4
                    } else {
                        source.find('f').unwrap()
                    }
                } else {
                    match language {
                        "javascript" => source.find("function f").unwrap() + 9,
                        "python" => source.find("def f").unwrap() + 4,
                        "rust" => source.find("fn f").unwrap() + 3,
                        _ => unreachable!(),
                    }
                };
                let def_line = source[..definition].bytes().filter(|b| *b == b'\n').count();
                let def_col = if encoding == 1 {
                    source[..definition].rsplit('\n').next().unwrap().len()
                } else {
                    source[..definition]
                        .rsplit('\n')
                        .next()
                        .unwrap()
                        .encode_utf16()
                        .count()
                };
                let mut declaration = scip::types::Occurrence::new();
                declaration.range = vec![
                    def_line as i32,
                    def_col as i32,
                    (def_col + symbol.len()) as i32,
                ];
                declaration.symbol = if alias_case {
                    "local alias"
                } else {
                    "external pkg f."
                }
                .into();
                declaration.symbol_roles = if alias_case { 1 | 2 } else { 1 };
                declaration.syntax_kind = EnumOrUnknown::from_i32(0);
                document.occurrences.push(declaration);
                let mut info = scip::types::SymbolInformation::new();
                info.symbol = if alias_case {
                    "local alias"
                } else {
                    "external pkg f."
                }
                .into();
                info.kind = EnumOrUnknown::from_i32(if alias_case { 0 } else { 26 });
                document.symbols.push(info);
            }
            if language == "java" && !type_case {
                occurrence.symbol_roles = 8;
                let mut child = scip::types::SymbolInformation::new();
                child.symbol = "scip-java pkg Child#".into();
                child.kind = EnumOrUnknown::from_i32(7);
                let mut edge = scip::types::Relationship::new();
                edge.symbol = "scip-java pkg Base#".into();
                edge.is_reference = true;
                child.relationships.push(edge);
                document.symbols.push(child);
                let mut cat = scip::types::SymbolInformation::new();
                cat.symbol = "scip-java pkg Cat#".into();
                cat.kind = EnumOrUnknown::from_i32(7);
                let mut edge = scip::types::Relationship::new();
                edge.symbol = "scip-java java/lang/Comparable#".into();
                edge.is_reference = true;
                edge.is_implementation = true;
                cat.relationships.push(edge);
                document.symbols.push(cat);
                let mut base = scip::types::SymbolInformation::new();
                base.symbol = "scip-java pkg Base#".into();
                base.kind = EnumOrUnknown::from_i32(7);
                document.symbols.push(base);
                let first = "scip-java pkg Base#f(I).";
                let other = "scip-java pkg Base#f(Ljava/lang/String;).";
                let derived = "scip-java pkg Child#f(I).";
                for (symbol, edges) in [
                    (first, vec![(derived, true, true)]),
                    (other, vec![]),
                    (derived, vec![(first, true, true), (other, true, true)]),
                    (
                        "scip-java pkg Cat#compareTo(Lpkg/Cat;).",
                        vec![("scip-java java/lang/Comparable#compareTo().", true, true)],
                    ),
                    (
                        "scip-java pkg Cat#toString().",
                        vec![("scip-java java/lang/Object#toString().", true, true)],
                    ),
                ] {
                    let mut info = scip::types::SymbolInformation::new();
                    info.symbol = symbol.into();
                    info.kind = EnumOrUnknown::from_i32(26);
                    for (target, reference, implementation) in edges {
                        let mut edge = scip::types::Relationship::new();
                        edge.symbol = target.into();
                        edge.is_reference = reference;
                        edge.is_implementation = implementation;
                        info.relationships.push(edge);
                    }
                    document.symbols.push(info);
                }
                let named = [
                    ("Base", source.find("Base").unwrap(), "scip-java pkg Base#"),
                    (
                        "Child",
                        source.find("Child").unwrap(),
                        "scip-java pkg Child#",
                    ),
                    ("Cat", source.find("Cat").unwrap(), "scip-java pkg Cat#"),
                    (
                        "Comparable",
                        source.find("Comparable").unwrap(),
                        "scip-java java/lang/Comparable#",
                    ),
                    (
                        "f",
                        source.find("f(int").unwrap(),
                        "scip-java pkg Base#f(I).",
                    ),
                    (
                        "f",
                        source.find("f(String").unwrap(),
                        "scip-java pkg Base#f(Ljava/lang/String;).",
                    ),
                    (
                        "f",
                        source.rfind("f(int").unwrap(),
                        "scip-java pkg Child#f(I).",
                    ),
                    (
                        "compareTo",
                        source.find("compareTo").unwrap(),
                        "scip-java pkg Cat#compareTo(Lpkg/Cat;).",
                    ),
                    (
                        "toString",
                        source.find("toString").unwrap(),
                        "scip-java pkg Cat#toString().",
                    ),
                ];
                for (name, start, symbol) in named {
                    let mut item = scip::types::Occurrence::new();
                    item.range = vec![0, start as i32, (start + name.len()) as i32];
                    item.symbol = symbol.into();
                    item.symbol_roles = if name == "Comparable" { 8 } else { 1 };
                    document.occurrences.push(item);
                }
            }
            if type_case {
                let owner = source.find("Child").unwrap();
                for (at, name, symbol) in [
                    (source.find("Base").unwrap(), "Base", "global pkg Base#"),
                    (owner, "Child", "global pkg Child#"),
                ] {
                    let line = source[..at].bytes().filter(|b| *b == b'\n').count();
                    let prefix = source[..at].rsplit('\n').next().unwrap();
                    let col = if encoding == 1 {
                        prefix.len()
                    } else {
                        prefix.encode_utf16().count()
                    };
                    let mut declaration = scip::types::Occurrence::new();
                    declaration.range = vec![line as i32, col as i32, (col + name.len()) as i32];
                    declaration.symbol = symbol.into();
                    declaration.symbol_roles = 1;
                    document.occurrences.push(declaration);
                }
                let mut child = scip::types::SymbolInformation::new();
                child.symbol = "global pkg Child#".into();
                child.kind = EnumOrUnknown::from_i32(7);
                let mut relation = scip::types::Relationship::new();
                relation.symbol = "global pkg Base#".into();
                relation.is_reference = true;
                child.relationships.push(relation);
                document.symbols.push(child);
                let mut base = scip::types::SymbolInformation::new();
                base.symbol = "global pkg Base#".into();
                base.kind = EnumOrUnknown::from_i32(7);
                document.symbols.push(base);
            }
            if ambiguous_call {
                let mut competing = occurrence.clone();
                competing.symbol = "external pkg Other.".into();
                document.occurrences.push(competing);
            }
            document.occurrences.push(occurrence);
            scip.documents.push(document);
            let bytes = scip.write_to_bytes().unwrap();
            let (descriptor_path, digest) = write_batch(
                &index_db,
                &root,
                producer,
                &[(language, path, bytes.clone())],
                &[],
                ".trellis-semantic-batch",
            );
            let descriptor = descriptor_path.display().to_string();
            if coordinate_case {
                let before = store.index_baseline().unwrap();
                let prior = store.semantic_at(before).unwrap();
                for invalid_case in 0..5 {
                    let mut invalid = scip.clone();
                    let invalid_encoding = match invalid_case {
                        0 => 3,
                        1 => {
                            if encoding == 1 {
                                2
                            } else {
                                1
                            }
                        }
                        _ => encoding,
                    };
                    invalid.documents[0].position_encoding =
                        EnumOrUnknown::from_i32(invalid_encoding);
                    if invalid_case >= 2 {
                        let emoji = source.find('😀').unwrap();
                        let prefix = &source[..emoji];
                        let split = if encoding == 1 {
                            prefix.len() + 1
                        } else {
                            prefix.encode_utf16().count() + 1
                        };
                        let occurrence = &mut invalid.documents[0].occurrences[0];
                        occurrence.range = match invalid_case {
                            2 => vec![0, split as i32, (split + 1) as i32],
                            3 => vec![1, 0, 0, 1],
                            _ => {
                                let line = &source[..source.find('\r').unwrap()];
                                let end = if encoding == 1 {
                                    line.len()
                                } else {
                                    line.encode_utf16().count()
                                };
                                vec![0, end as i32, (end + 1) as i32]
                            }
                        };
                    }
                    let invalid_bytes = invalid.write_to_bytes().unwrap();
                    let (bad_descriptor, bad_digest) = write_batch(
                        &index_db,
                        &root,
                        producer,
                        &[(language, path, invalid_bytes)],
                        &[],
                        &format!(".trellis-semantic-bad-{invalid_case}"),
                    );
                    let output = cli(&home)
                        .args([
                            "semantic",
                            "import",
                            "--workspace",
                            root_string,
                            "--batch",
                            bad_descriptor.to_str().unwrap(),
                            "--batch-sha256",
                            &bad_digest,
                        ])
                        .output()
                        .unwrap();
                    assert!(
                        !output.status.success(),
                        "{language}: invalid coordinate case {invalid_case} must reject"
                    );
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    assert!(
                        stderr.contains("invalid_artifact: request "),
                        "{language} case {invalid_case}: {stderr}"
                    );
                    assert!(
                        store.earliest_unfinished_request().unwrap().is_none(),
                        "{language}: rejected request must leave no queued/running row"
                    );
                    let after = store.index_baseline().unwrap();
                    assert!(after.index_revision >= before.index_revision);
                    assert_eq!(
                        store.semantic_at(after).unwrap(),
                        prior,
                        "invalid coordinate cannot publish semantic overlay"
                    );
                    assert!(
                        store
                            .semantic_batch_report_at(after, &bad_digest)
                            .unwrap()
                            .is_none()
                    );
                }
            }
            eprintln!(
                "SEMANTIC_VARIANT_IMPORT language={language} variant={variant} elapsed={:?}",
                variant_started.elapsed()
            );
            let imported = run(
                &home,
                &[
                    "semantic",
                    "import",
                    "--workspace",
                    root_string,
                    "--batch",
                    &descriptor,
                    "--batch-sha256",
                    &digest,
                ],
            );
            let selected_pin: trellis::model::IndexPin =
                serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
            assert!(
                selected_pin.index_revision > pin.index_revision,
                "{language}: {imported}"
            );
            assert_eq!(
                imported["report"]["producerId"], producer,
                "{language}: {imported}"
            );
            assert_eq!(
                serde_json::to_value(
                    store
                        .semantic_at(selected_pin)
                        .unwrap()
                        .coverage
                        .iter()
                        .find(|c| c.document_path == path)
                        .unwrap()
                        .state
                )
                .unwrap(),
                if language == "java" && !type_case {
                    "partial"
                } else {
                    "complete"
                },
                "{language}: {imported}"
            );
            assert_eq!(
                imported["report"]["languages"][0]["matched"],
                if language == "java" && !type_case {
                    14
                } else if type_case && language == "rust" {
                    3
                } else if type_case {
                    4
                } else if ambiguous_call {
                    1
                } else {
                    2
                },
                "{language}: {imported}"
            );
            if ambiguous_call {
                assert_eq!(imported["report"]["languages"][0]["ambiguous"], 2);
            }
            assert_eq!(
                store
                    .semantic_batch_report_at(selected_pin, &digest)
                    .unwrap()
                    .unwrap(),
                imported["report"]
            );
            if write_case {
                let selected = store.semantic_at(selected_pin).unwrap();
                let written = selected
                    .references
                    .iter()
                    .find(|r| r.document.path == path && r.range.start == start)
                    .expect("measured selected assignment reference");
                assert_eq!(
                    written.roles,
                    vec![trellis::semantic_evidence::Role::Write],
                    "{language}: write bit must not add read"
                );
            }
            assert!(
                store
                    .semantic_at(selected_pin)
                    .unwrap()
                    .provenance
                    .iter()
                    .filter(|p| p.producer_id == producer)
                    .all(|p| p.freshness == trellis::semantic_evidence::Freshness::Fresh),
                "{language}: same-revision matched facts fresh"
            );
            if type_case {
                let selected = store.semantic_at(selected_pin).unwrap();
                let reference = selected
                    .references
                    .iter()
                    .find(|r| r.document.path == path && r.range.start == start)
                    .expect("selected native type use");
                assert!(
                    reference
                        .roles
                        .contains(&trellis::semantic_evidence::Role::Type),
                    "{language}: {reference:?}"
                );
                let expected_roles = if language == "javascript" {
                    vec![
                        trellis::semantic_evidence::Role::Read,
                        trellis::semantic_evidence::Role::Type,
                    ]
                } else {
                    vec![trellis::semantic_evidence::Role::Type]
                };
                assert_eq!(
                    reference.roles, expected_roles,
                    "{language}: exact selected type roles"
                );
                if language != "rust" {
                    assert!(
                        selected.type_relationships.iter().any(
                            |r| r.kind == trellis::semantic_evidence::RelationshipKind::Extends
                        ),
                        "{language}: selected direct extends"
                    );
                }
            }
            if language != "java" && !type_case {
                let selected = store.semantic_at(selected_pin).unwrap();
                if !write_case {
                    assert!(
                        selected
                            .declaration_bindings
                            .iter()
                            .any(|b| b.join.anchor.document.path == path),
                        "{language}: missing declaration"
                    );
                }
                assert!(
                    selected.references.iter().any(|r| r.document.path == path),
                    "{language}: missing reference"
                );
                if coordinate_case {
                    assert!(
                        selected
                            .references
                            .iter()
                            .any(|r| r.range.start == start && r.range.end == start + symbol.len()),
                        "{language}: raw0 profile-selected coordinate after supplementary Unicode and CRLF"
                    );
                }
                assert_eq!(
                    selected.call_bindings.len(),
                    if ambiguous_call {
                        2
                    } else {
                        // The use at the exact measured callee is the call, with or
                        // without an enclosing range (#444); the alias and write sites
                        // are not callees.
                        usize::from(!alias_case && !write_case)
                    },
                    "{language}: exact measured callee controls selected binding"
                );
                if ambiguous_call {
                    assert!(selected.call_bindings.iter().all(|c| c.resolution
                        == trellis::semantic_evidence::Resolution::Ambiguous
                        && c.candidates.len() == 2));
                }
                if alias_case {
                    assert!(
                        selected
                            .references
                            .iter()
                            .any(|r| r.range.start == source.find("local").unwrap()
                                && r.roles.contains(&trellis::semantic_evidence::Role::Alias)
                                && r.roles.contains(&trellis::semantic_evidence::Role::Import)),
                        "{language}: exact native alias import roles"
                    );
                }
            }
            if language == "java" && !type_case {
                let selected = store.semantic_at(selected_pin).unwrap();
                assert!(
                    selected.type_relationships.iter().any(|r| r.kind
                        == trellis::semantic_evidence::RelationshipKind::Extends
                        && matches!(
                            r.target,
                            trellis::semantic_evidence::Target::Internal { .. }
                        )),
                    "{selected:#?}"
                );
                assert!(
                    selected.type_relationships.iter().any(|r| r.kind
                        == trellis::semantic_evidence::RelationshipKind::Implements
                        && matches!(
                            r.target,
                            trellis::semantic_evidence::Target::External { .. }
                        )),
                    "{selected:#?}"
                );
                let overrides: Vec<_> = selected
                    .type_relationships
                    .iter()
                    .filter(|r| r.kind == trellis::semantic_evidence::RelationshipKind::Overrides)
                    .collect();
                assert_eq!(
                    overrides.len(),
                    2,
                    "local full-descriptor and external comparable only: {selected:#?}"
                );
                assert!(
                    overrides.iter().any(|r| matches!(
                        r.target,
                        trellis::semantic_evidence::Target::Internal { .. }
                    )),
                    "{selected:#?}"
                );
                assert!(
                    overrides.iter().any(|r| matches!(
                        r.target,
                        trellis::semantic_evidence::Target::External { .. }
                    )),
                    "{selected:#?}"
                );
                assert!(selected.type_relationships.iter().all(|r| !matches!(&r.target,trellis::semantic_evidence::Target::External{symbol} if symbol.symbol.contains("Object#toString"))),"implicit Object endpoint must not be inferred");
            }
            if let Ok(pid) = fs::read_to_string(home.join("daemon.pid"))
                && let Ok(pid) = pid.parse::<i32>()
            {
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            eprintln!(
                "SEMANTIC_VARIANT_END language={language} variant={variant} elapsed={:?}",
                variant_started.elapsed()
            );
        }
    }
}

#[test]
fn replaced_root_fails_old_accepted_semantic_row_before_new_root_claim() {
    use trellis::store::requests::SemanticImportOptions;
    let temp = Builder::new()
        .prefix("semantic-root-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("A.js"), "function f(){}\n").unwrap();
    let root = fs::canonicalize(root).unwrap();
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
    let old_identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let old = trellis::store::Store::open(roots.clone(), old_identity).unwrap();
    fs::write(root.join("descriptor.json"), b"queued descriptor").unwrap();
    let options = SemanticImportOptions {
        version: 2,
        descriptor: root.join("descriptor.json").display().to_string(),
        descriptor_sha256: "a".repeat(64),
    };
    let accepted = old.enqueue_semantic_import(&root, &options).unwrap();
    assert_eq!(accepted.state, "queued");
    fs::rename(&root, temp.path().join("old-checkout")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("A.js"), "function g(){}\n").unwrap();
    fs::write(root.join("descriptor.json"), b"new root descriptor").unwrap();
    assert_eq!(old.fail_changed_root_requests().unwrap(), 1);
    let fresh_identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    drop(old);
    let fresh = trellis::store::Store::open(roots, fresh_identity).unwrap();
    let prior = fresh
        .request_any_root_for_tests(&accepted.id)
        .unwrap()
        .unwrap();
    assert_eq!(prior.state, "failed");
    assert_eq!(prior.error_code.as_deref(), Some("root_changed"));
    let fresh_row = fresh.enqueue_semantic_import(&root, &options).unwrap();
    assert_eq!(fresh_row.state, "queued");
    assert_ne!(fresh_row.id, accepted.id);
    assert_eq!(
        fresh
            .request_any_root_for_tests(&accepted.id)
            .unwrap()
            .unwrap()
            .error_code
            .as_deref(),
        Some("root_changed")
    );
}

#[test]
fn genuine_published_rust_macro_gap_reports_unsupported_without_losing_ordinary() {
    use protobuf::{EnumOrUnknown, Message};
    let source = "fn main() { ordinary(); some_macro!(hidden()); }";
    let temp = Builder::new()
        .prefix("semantic-real-gap-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("A.rs"), source).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let root_string = root.to_str().unwrap();
    let indexed = run(&home, &["index", "--workspace", root_string]);
    let pin: trellis::model::IndexPin =
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
    let store = trellis::store::Store::open(roots, identity).unwrap();
    let (native, _) = store.retained_native_snapshot_at(pin).unwrap();
    let doc = native
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "A.rs")
        .unwrap();
    let reports = native
        .capability_reports
        .iter()
        .find(|r| r.document == doc.key)
        .unwrap();
    assert_eq!(reports.families.len(), 4);
    assert!(reports.families.iter().all(|f| f.available));
    let hidden = source.find("hidden").unwrap();
    let ordinary = source.find("ordinary").unwrap();
    let outside = source.find("fn").unwrap();
    assert!(
        reports
            .families
            .iter()
            .flat_map(|f| &f.gaps)
            .any(|g| g.range.start <= hidden && hidden + 6 <= g.range.end)
    );
    assert!(
        reports
            .families
            .iter()
            .flat_map(|f| &f.gaps)
            .all(|g| !(g.range.start <= ordinary && ordinary + 8 <= g.range.end))
    );
    assert!(
        reports
            .families
            .iter()
            .flat_map(|f| &f.gaps)
            .all(|g| !(g.range.start <= outside && outside + 4 <= g.range.end)),
        "outside claim must not be inside actual gap"
    );
    let mut binary = scip::types::Index::new();
    let mut document = scip::types::Document::new();
    document.relative_path = "A.rs".into();
    document.language = "rust".into();
    document.position_encoding = EnumOrUnknown::from_i32(1);
    for (at, name, symbol, roles) in [
        (
            source.find("main").unwrap(),
            "main",
            "global pkg main().",
            1,
        ),
        (ordinary, "ordinary", "external pkg ordinary().", 0),
        (hidden, "hidden", "external pkg hidden().", 0),
        (outside, "fn", "external pkg fn().", 0),
    ] {
        let mut occurrence = scip::types::Occurrence::new();
        occurrence.range = vec![0, at as i32, (at + name.len()) as i32];
        occurrence.symbol = symbol.into();
        occurrence.symbol_roles = roles;
        document.occurrences.push(occurrence);
    }
    binary.documents.push(document);
    let bytes = binary.write_to_bytes().unwrap();
    let (descriptor_path, digest) = write_batch(
        &index_db,
        &root,
        "rust-analyzer",
        &[("rust", "A.rs", bytes)],
        &[],
        ".trellis-semantic-batch",
    );
    let committed = run(
        &home,
        &[
            "semantic",
            "import",
            "--workspace",
            root_string,
            "--batch",
            descriptor_path.to_str().unwrap(),
            "--batch-sha256",
            &digest,
        ],
    );
    let report = &committed["report"]["languages"][0];
    assert_eq!(report["requested"], 4, "{committed}");
    assert_eq!(report["matched"], 2, "ordinary and main measured");
    assert_eq!(report["unsupported"], 1, "hidden inside genuine native gap");
    assert_eq!(report["skipped"], 1, "unmatched macro outside measured gap");
    let selected_pin: trellis::model::IndexPin =
        serde_json::from_value(committed["publishedRevision"].clone()).unwrap();
    let selected = store.semantic_at(selected_pin).unwrap();
    assert!(
        selected
            .references
            .iter()
            .any(|r| r.range.start == ordinary)
    );
    assert!(selected.references.iter().all(|r| r.range.start != hidden));
    let coverage = selected
        .coverage
        .iter()
        .find(|c| c.document_path == "A.rs")
        .unwrap();
    assert_eq!(
        coverage.state,
        trellis::semantic_evidence::CoverageState::Partial
    );
    assert_eq!(
        coverage.diagnostic.as_deref(),
        Some("unsupported_native_gap")
    );
    assert!(
        coverage
            .observed_roles
            .contains(&trellis::semantic_evidence::Role::Read)
    );
    assert_eq!(
        store
            .semantic_batch_report_at(selected_pin, &digest)
            .unwrap()
            .unwrap(),
        committed["report"]
    );
}

#[test]
fn replacing_one_selected_raw_capture_copies_the_other_producer() {
    use protobuf::{EnumOrUnknown, Message};
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        indexer::{IndexOptions, index_workspace_bundle},
        semantic_batch::{BatchLimits, testing::stage_and_join_admitted},
        semantic_import::DestinationMutations,
    };
    let temp = Builder::new()
        .prefix("semantic-selection-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("A.js"), "function a() {}\n").unwrap();
    fs::write(root.join("B.py"), "def b():\n    pass\n").unwrap();
    let root = fs::canonicalize(root).unwrap();
    let state = temp.path().join("state");
    fs::create_dir(&state).unwrap();
    let store = trellis::store::Store::open_for_tests(&state, &root).unwrap();
    let index_db = trellis::store::topology::TopologyRoots::isolated_for_tests(
        state.join("cache"),
        state.join("data"),
    )
    .index_db(&trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap());
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.clone());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
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
        .unwrap();
    drop(leader);
    let mut pins = Vec::new();
    for (producer_id, path, language, replacement) in [
        ("scip-typescript", "A.js", "javascript", false),
        ("scip-python", "B.py", "python", false),
        ("scip-typescript", "A.js", "javascript", true),
    ] {
        let mut binary = scip::types::Index::new();
        let mut document = scip::types::Document::new();
        document.relative_path = path.into();
        document.language = language.into();
        document.position_encoding = EnumOrUnknown::from_i32(2);
        if replacement {
            document.text = fs::read_to_string(root.join(path)).unwrap();
        }
        binary.documents.push(document);
        let bytes = binary.write_to_bytes().unwrap();
        let (descriptor, digest) = write_batch(
            &index_db,
            &root,
            producer_id,
            &[(language, path, bytes.clone())],
            &[],
            &format!(".trellis-semantic-batch-{}", pins.len()),
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
        let pin = store
            .publish_semantic_batch(
                &joined,
                admitted.stage_mut(),
                joined.destination_pin(),
                &leader,
                &cancel,
            )
            .unwrap();
        pins.push((pin, native_ids::digest(b"", &bytes)));
    }
    let old = store.selected_raw_inputs_at(pins[1].0).unwrap();
    let latest = store.selected_raw_inputs_at(pins[2].0).unwrap();
    assert_eq!(old.len(), 2);
    assert_eq!(latest.len(), 2);
    let old_js = old
        .iter()
        .find(|row| row.producer_id == "scip-typescript")
        .unwrap();
    let latest_js = latest
        .iter()
        .find(|row| row.producer_id == "scip-typescript")
        .unwrap();
    assert_eq!(old_js.captures[0].artifact_hash, pins[0].1);
    assert_eq!(latest_js.captures[0].artifact_hash, pins[2].1);
    assert_ne!(pins[0].1, pins[2].1);
    assert_eq!(
        old.iter()
            .find(|row| row.producer_id == "scip-python")
            .unwrap()
            .captures[0]
            .artifact_hash,
        pins[1].1
    );
    assert_eq!(
        latest
            .iter()
            .find(|row| row.producer_id == "scip-python")
            .unwrap()
            .captures[0]
            .artifact_hash,
        pins[1].1
    );
    assert_eq!(
        store.selected_raw_inputs_at(pins[1].0).unwrap()[0].captures[0].artifact_hash,
        old[0].captures[0].artifact_hash
    );
}
