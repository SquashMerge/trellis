//! Small exhaustive crash schedules against the real SQLite-backed Store.
#![cfg(feature = "test-support")]

use std::{
    fs,
    io::Write,
    path::Path,
    process::Command,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{indexer::IndexOptions, store::Store};

fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn crash(boundary: &str) -> ! {
    println!("CRASH_BOUNDARY {boundary}");
    std::io::stdout().flush().unwrap();
    // SIGABRT terminates this owned child without running Rust destructors.
    std::process::abort()
}

#[test]
fn crash_child() {
    let Some(state) = std::env::var_os("TRELLIS_CRASH_STATE") else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os("TRELLIS_CRASH_ROOT").unwrap());
    let boundary = std::env::var("TRELLIS_CRASH_BOUNDARY").unwrap();
    let store = Store::open_for_tests(Path::new(&state), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    match boundary.as_str() {
        "insert" => {
            store
                .enqueue_request_after_insert_for_tests(&options, None, || crash("insert"))
                .unwrap();
        }
        "enqueue_commit" => {
            store.set_after_enqueue_commit_hook_for_tests(|| crash("enqueue_commit"));
            store.enqueue_request(&options, None).unwrap();
        }
        "publication_precommit" | "publication_committed" => {
            let (before, session) = trellis::index_coordinator::reconcile_workspace(
                &store,
                &options,
                &cancel(),
                |_| {},
            )
            .unwrap();
            let request = store.claim_request(&session).unwrap().unwrap();
            fs::write(root.join("a.js"), "function after_a() {}\n").unwrap();
            fs::write(root.join("b.js"), "function after_b() {}\n").unwrap();
            println!(
                "PRIOR {} {}",
                before.index_generation, before.index_revision
            );
            std::io::stdout().flush().unwrap();
            if boundary == "publication_precommit" {
                store.set_publication_before_commit_hook_for_tests(|| {
                    crash("publication_precommit")
                });
            }
            let coordinator =
                trellis::index_coordinator::IndexJobCoordinator::prepare_with_session(
                    &store,
                    request.expected,
                    session,
                )
                .unwrap();
            let pin = coordinator.run(&options, &cancel(), |_| {}).unwrap();
            println!("COMMITTED {} {}", pin.index_generation, pin.index_revision);
            std::io::stdout().flush().unwrap();
            crash("publication_committed");
        }
        _ => panic!("unknown boundary"),
    }
    panic!("crash hook was not reached");
}

fn run_child(state: &Path, root: &Path, boundary: &str) -> String {
    use std::os::unix::process::ExitStatusExt;
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--nocapture", "--test-threads=1"])
        .env("TRELLIS_CRASH_STATE", state)
        .env("TRELLIS_CRASH_ROOT", root)
        .env("TRELLIS_CRASH_BOUNDARY", boundary)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGABRT),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!("CRASH_BOUNDARY {boundary}")),
        "{stdout}"
    );
    stdout
}

#[test]
fn queue_insert_and_commit_crash_matrix() {
    // Model: prior terminal rows stay byte-for-byte equivalent as typed values;
    // a pre-COMMIT insert vanishes, a post-COMMIT row survives and joins FIFO.
    for boundary in ["insert", "enqueue_commit"] {
        for prefix in ["none", "done", "failed"] {
            for queued in 0..=2 {
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path().join("root");
                let state = temp.path().join("state");
                fs::create_dir(&root).unwrap();
                fs::create_dir(&state).unwrap();
                fs::write(root.join("a.js"), "function before() {}\n").unwrap();
                let options = IndexOptions::new(root.clone());
                let store = Store::open_for_tests(&state, &root).unwrap();
                let (pin, session) = trellis::index_coordinator::reconcile_workspace(
                    &store,
                    &options,
                    &cancel(),
                    |_| {},
                )
                .unwrap();
                let mut terminal = None;
                if prefix != "none" {
                    let row = store.enqueue_request(&options, None).unwrap();
                    let claim = store.claim_request(&session).unwrap().unwrap();
                    assert_eq!(row.id, claim.id);
                    let result = if prefix == "done" {
                        Ok(pin)
                    } else {
                        Err(anyhow::anyhow!("fixture failure"))
                    };
                    store.finish_request(&session, &claim, result).unwrap();
                    terminal = Some(store.request_by_id(&row.id).unwrap().unwrap());
                }
                let mut pending = Vec::new();
                for _ in 0..queued {
                    pending.push(store.enqueue_request(&options, None).unwrap());
                }
                let last = store.current_request().unwrap();
                drop(session);
                drop(store);
                run_child(&state, &root, boundary);
                let reopened = Store::open_for_tests(&state, &root).unwrap();
                if let Some(row) = &terminal {
                    assert_eq!(
                        serde_json::to_value(reopened.request_by_id(&row.id).unwrap().unwrap())
                            .unwrap(),
                        serde_json::to_value(row).unwrap()
                    );
                }
                for row in &pending {
                    assert_eq!(
                        serde_json::to_value(reopened.request_by_id(&row.id).unwrap().unwrap())
                            .unwrap(),
                        serde_json::to_value(row).unwrap()
                    );
                }
                let latest = reopened.current_request().unwrap();
                if boundary == "insert" {
                    assert_eq!(latest.as_ref().map(|r| &r.id), last.as_ref().map(|r| &r.id));
                } else {
                    let row = latest.unwrap();
                    assert_eq!(row.state, "queued");
                    assert!(last.as_ref().is_none_or(|last| row.seq > last.seq));
                    pending.push(row);
                }
                let (recovered_pin, session) = trellis::index_coordinator::reconcile_workspace(
                    &reopened,
                    &options,
                    &cancel(),
                    |_| {},
                )
                .unwrap();
                for expected in &pending {
                    let claim = reopened
                        .claim_request(&session)
                        .unwrap()
                        .expect("accepted request lost");
                    assert_eq!(
                        claim.id, expected.id,
                        "FIFO after {boundary}, prefix={prefix}, queued={queued}"
                    );
                    reopened
                        .finish_request(&session, &claim, Ok(recovered_pin))
                        .unwrap();
                }
                assert!(reopened.claim_request(&session).unwrap().is_none());
                drop(session);
                drop(reopened);
                let final_store = Store::open_for_tests(&state, &root).unwrap();
                for row in &pending {
                    let done = final_store.request_by_id(&row.id).unwrap().unwrap();
                    assert_eq!(done.state, "done");
                    assert_eq!(done.revision, Some(recovered_pin));
                }
                if let Some(row) = &terminal {
                    assert_eq!(
                        serde_json::to_value(final_store.request_by_id(&row.id).unwrap().unwrap())
                            .unwrap(),
                        serde_json::to_value(row).unwrap()
                    );
                }
            }
        }
    }
}

#[test]
fn publication_crash_preserves_coherent_source_and_running_request() {
    for boundary in ["publication_precommit", "publication_committed"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::write(root.join("a.js"), "function before_a() {}\n").unwrap();
        fs::write(root.join("b.js"), "function before_b() {}\n").unwrap();
        let options = IndexOptions::new(root.clone());
        let store = Store::open_for_tests(&state, &root).unwrap();
        let accepted = store.enqueue_request(&options, None).unwrap();
        drop(store);
        let proof = run_child(&state, &root, boundary);
        let prior_line = proof
            .lines()
            .find_map(|line| line.split_once("PRIOR ").map(|(_, value)| value))
            .unwrap();
        let fields: Vec<_> = prior_line.split_whitespace().collect();
        let prior = trellis::model::IndexPin {
            index_generation: fields[0].parse().unwrap(),
            index_revision: fields[1].parse().unwrap(),
        };
        let reopened = Store::open_for_tests(&state, &root).unwrap();
        let selected = reopened.index_baseline().unwrap();
        assert_eq!(selected.index_generation, prior.index_generation);
        if boundary == "publication_precommit" {
            assert_eq!(selected, prior);
        } else {
            let line = proof
                .lines()
                .find_map(|line| line.strip_prefix("COMMITTED "))
                .unwrap();
            let fields: Vec<_> = line.split_whitespace().collect();
            assert_eq!(selected.index_generation.to_string(), fields[0]);
            assert_eq!(selected.index_revision.to_string(), fields[1]);
            assert!(selected.index_revision > prior.index_revision);
        }
        for name in ["a", "b"] {
            let (_, source) = reopened
                .source_at(&format!("{name}.js"), Some(selected))
                .unwrap()
                .unwrap();
            let tag = if boundary == "publication_precommit" {
                "before"
            } else {
                "after"
            };
            assert_eq!(source.text, format!("function {tag}_{name}() {{}}\n"));
            let (_, old) = reopened
                .source_at(&format!("{name}.js"), Some(prior))
                .unwrap()
                .unwrap();
            assert_eq!(old.text, format!("function before_{name}() {{}}\n"));
        }
        assert_eq!(
            reopened.request_by_id(&accepted.id).unwrap().unwrap().state,
            "running"
        );
        let (_, session) =
            trellis::index_coordinator::reconcile_workspace(&reopened, &options, &cancel(), |_| {})
                .unwrap();
        assert_eq!(
            trellis::index_coordinator::drain_requests(&reopened, &session).unwrap(),
            1
        );
        let done = reopened.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(done.state, "done");
        let completed = done.revision.unwrap();
        assert_eq!(completed.index_generation, prior.index_generation);
        assert!(completed.index_revision >= selected.index_revision);
        for name in ["a", "b"] {
            let (_, source) = reopened
                .source_at(&format!("{name}.js"), Some(completed))
                .unwrap()
                .unwrap();
            assert_eq!(source.text, format!("function after_{name}() {{}}\n"));
        }
        drop(session);
        drop(reopened);
        let final_store = Store::open_for_tests(&state, &root).unwrap();
        assert_eq!(
            serde_json::to_value(final_store.request_by_id(&accepted.id).unwrap().unwrap())
                .unwrap(),
            serde_json::to_value(done).unwrap()
        );
        for name in ["a", "b"] {
            let (_, source) = final_store
                .source_at(&format!("{name}.js"), Some(completed))
                .unwrap()
                .unwrap();
            assert_eq!(source.text, format!("function after_{name}() {{}}\n"));
        }
    }
}
