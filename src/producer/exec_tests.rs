#[test]
fn active_run_has_no_reap_before_signal_or_stale_unwind_kill() {
    let exec = include_str!("exec.rs");
    let run = include_str!("run.rs");
    assert!(
        !exec.contains("child.try_wait()"),
        "the active leader must stay unreaped until the last group signal"
    );
    assert_eq!(
        exec.matches("libc::killpg(").count(),
        1,
        "all group signals must use the owning signal gateway"
    );
    assert!(!exec.contains("group_absent"), "no post-reap group probe");
    let cleanup = run.split("impl Drop for RunCleanup").nth(1).unwrap();
    let cleanup = cleanup.split("/// How a run ended").next().unwrap();
    assert!(
        !cleanup.contains("killpg"),
        "run cleanup has no child authority"
    );
}

#[test]
fn group_survival_serializes_unknown_and_absence_distinctly() {
    let mut diagnostics = super::CompleteDiagnostics::default();
    assert_eq!(
        serde_json::to_value(&diagnostics).unwrap()["groupSurvived"],
        serde_json::Value::Null
    );
    assert_eq!(
        serde_json::to_value(&diagnostics).unwrap()["termination"],
        "unknown"
    );
    diagnostics.group_survived = Some(false);
    diagnostics.stdout_bytes = 3;
    diagnostics.stdout_complete = Some(false);
    diagnostics.stderr_complete = Some(true);
    assert_eq!(
        serde_json::to_value(&diagnostics).unwrap()["groupSurvived"],
        false
    );
    let current = serde_json::to_value(&diagnostics).unwrap();
    assert_eq!(current["stdoutBytes"], 3);
    assert_eq!(current["stdoutComplete"], false);
    assert_eq!(current["stderrComplete"], true);
    let mut old_record = serde_json::to_value(&diagnostics).unwrap();
    old_record.as_object_mut().unwrap().remove("termination");
    old_record.as_object_mut().unwrap().remove("stdoutComplete");
    old_record.as_object_mut().unwrap().remove("stderrComplete");
    let decoded: super::Diagnostics = serde_json::from_value(old_record).unwrap();
    assert!(matches!(
        decoded,
        super::Diagnostics::Complete(super::CompleteDiagnostics {
            termination: super::TerminationStatus::Unknown,
            stdout_complete: None,
            stderr_complete: None,
            ..
        })
    ));
}

#[test]
fn artifact_size_serializes_unknown_and_measured_zero_distinctly() {
    let mut diagnostics = super::CompleteDiagnostics::default();
    assert_eq!(
        serde_json::to_value(&diagnostics).unwrap()["artifactBytes"],
        serde_json::Value::Null
    );
    diagnostics.artifact_bytes = Some(0);
    let wire = serde_json::to_value(&diagnostics).unwrap();
    assert_eq!(wire["artifactBytes"], 0);
    let decoded: super::Diagnostics = serde_json::from_value(wire).unwrap();
    assert!(matches!(
        decoded,
        super::Diagnostics::Complete(super::CompleteDiagnostics {
            artifact_bytes: Some(0),
            ..
        })
    ));
}

fn record_fixture() -> (
    tempfile::TempDir,
    super::state::StateDir,
    super::state::RunRecord,
) {
    use super::state::{DaemonIdentity, RunRecord, RunState, StateDir};
    use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let identity = WorkspaceIdentity::discover(Some(&workspace), &workspace).unwrap();
    let state = StateDir::open(&roots, &identity).unwrap();
    let run_id = "a".repeat(32);
    state.create_run_dir(&run_id).unwrap();
    let record = RunRecord {
        version: 1,
        run_id,
        producer: "scip-typescript".into(),
        trust_profile: super::TrustProfile::Restricted,
        plan_sha256: "0".repeat(64),
        source_revision: "revision".into(),
        state: RunState::Failed,
        outcome: Some("producer_failed".into()),
        error_code: Some("producer_failed".into()),
        daemon: DaemonIdentity::current(),
        process: None,
        request_id: None,
        created_at: 1,
        finished_at: Some(2),
        diagnostics: Some(super::Diagnostics::Complete(super::CompleteDiagnostics {
            artifact_bytes: Some(12),
            group_survived: Some(false),
            ..Default::default()
        })),
    };
    (temp, state, record)
}

fn write_record_value(state: &super::state::StateDir, run_id: &str, value: &serde_json::Value) {
    let path = state.run_dir(run_id).unwrap().join("run.json");
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

#[test]
fn required_nullable_complete_diagnostic_keys_missing_are_corrupt() {
    let (_temp, state, record) = record_fixture();
    let valid = serde_json::to_value(&record).unwrap();
    for key in ["artifactBytes", "groupSurvived"] {
        let mut damaged = valid.clone();
        damaged["diagnostics"].as_object_mut().unwrap().remove(key);
        write_record_value(&state, &record.run_id, &damaged);
        let error = state.read_run_record(&record.run_id).unwrap_err();
        assert!(
            error.to_string().starts_with(super::PRODUCER_STATE_CORRUPT),
            "{key}: {error:#}"
        );
    }
}

#[test]
fn required_nullable_partial_group_key_missing_is_corrupt() {
    let (_temp, state, mut record) = record_fixture();
    record.diagnostics = Some(super::Diagnostics::Unknown(super::UnknownDiagnostics {
        termination: super::TerminationStatus::Unknown,
        failure: super::TerminationFailure::Observation,
        group_survived: None,
    }));
    let mut damaged = serde_json::to_value(&record).unwrap();
    damaged["diagnostics"]
        .as_object_mut()
        .unwrap()
        .remove("groupSurvived");
    write_record_value(&state, &record.run_id, &damaged);
    let error = state.read_run_record(&record.run_id).unwrap_err();
    assert!(
        error.to_string().starts_with(super::PRODUCER_STATE_CORRUPT),
        "{error:#}"
    );
}

#[test]
fn required_nullable_records_accept_null_and_legacy_values() {
    let (_temp, state, mut record) = record_fixture();
    let mut old = serde_json::to_value(&record).unwrap();
    for key in ["termination", "stdoutComplete", "stderrComplete"] {
        old["diagnostics"].as_object_mut().unwrap().remove(key);
    }
    write_record_value(&state, &record.run_id, &old);
    let read = state.read_run_record(&record.run_id).unwrap().unwrap();
    assert!(matches!(
        read.diagnostics,
        Some(super::Diagnostics::Complete(super::CompleteDiagnostics {
            artifact_bytes: Some(12),
            group_survived: Some(false),
            stdout_complete: None,
            stderr_complete: None,
            ..
        }))
    ));

    let mut nulls = old.clone();
    nulls["diagnostics"]["artifactBytes"] = serde_json::Value::Null;
    nulls["diagnostics"]["groupSurvived"] = serde_json::Value::Null;
    write_record_value(&state, &record.run_id, &nulls);
    let read = state.read_run_record(&record.run_id).unwrap().unwrap();
    assert!(matches!(
        read.diagnostics,
        Some(super::Diagnostics::Complete(super::CompleteDiagnostics {
            artifact_bytes: None,
            group_survived: None,
            ..
        }))
    ));

    record.diagnostics = Some(super::Diagnostics::Unknown(super::UnknownDiagnostics {
        termination: super::TerminationStatus::Unknown,
        failure: super::TerminationFailure::Signal,
        group_survived: None,
    }));
    state.write_run_record(&record).unwrap();
    assert_eq!(state.read_run_record(&record.run_id).unwrap(), Some(record));
}

// This source guard complements the runtime ownership design. Parse Rust
// syntax, rather than matching text, so comments and string literals are not
// mistaken for process-wide waits or signal handlers.
fn forbidden_process_calls(source: &str, allow_owned_waitid: bool) -> Vec<String> {
    fn compact(node: tree_sitter::Node<'_>, source: &str) -> String {
        if node.kind().contains("comment") {
            return String::new();
        }
        if node.child_count() == 0 {
            return node
                .utf8_text(source.as_bytes())
                .unwrap()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
        }
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .map(|child| compact(child, source))
            .collect()
    }
    fn has_wnowait(node: tree_sitter::Node<'_>, source: &str) -> bool {
        match node.kind() {
            "scoped_identifier" => compact(node, source) == "libc::WNOWAIT",
            "parenthesized_expression" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|child| !child.kind().contains("comment"))
                    .is_some_and(|child| has_wnowait(child, source))
            }
            "binary_expression"
                if node
                    .child_by_field_name("operator")
                    .is_some_and(|op| op.kind() == "|") =>
            {
                node.child_by_field_name("left")
                    .is_some_and(|child| has_wnowait(child, source))
                    || node
                        .child_by_field_name("right")
                        .is_some_and(|child| has_wnowait(child, source))
            }
            _ => false,
        }
    }
    fn visit(
        node: tree_sitter::Node<'_>,
        source: &str,
        allow_owned_waitid: bool,
        errors: &mut Vec<String>,
    ) {
        if node.kind() == "mod_item"
            && node.child_by_field_name("name").is_some_and(|n| {
                let name = n.utf8_text(source.as_bytes()).unwrap();
                name == "tests" || name.ends_with("_tests")
            })
        {
            return;
        }
        if node.kind() == "use_declaration" {
            let text = compact(node, source);
            if text.contains("libc")
                && (text.contains("::*")
                    || text.contains("{*")
                    || ["waitpid", "wait4", "wait3", "waitid", "sigaction", "signal"]
                        .iter()
                        .any(|name| text.contains(name)))
            {
                errors.push(format!(
                    "process API import at line {}",
                    node.start_position().row + 1
                ));
            }
        }
        if node.kind() == "scoped_identifier" {
            let name = compact(node, source);
            if ["::waitpid", "::wait4", "::wait3"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
            {
                errors.push(format!(
                    "uncontrolled wait reference at line {}",
                    node.start_position().row + 1
                ));
            }
            if name.ends_with("::waitid")
                && !node.parent().is_some_and(|parent| {
                    parent.kind() == "call_expression"
                        && parent.child_by_field_name("function") == Some(node)
                })
            {
                errors.push(format!(
                    "aliased waitid at line {}",
                    node.start_position().row + 1
                ));
            }
            if (name.ends_with("::sigaction") || name.ends_with("::signal"))
                && !name.starts_with("tokio::")
                && !node.parent().is_some_and(|parent| {
                    parent.kind() == "scoped_identifier"
                        || parent.kind() == "call_expression"
                            && parent.child_by_field_name("function") == Some(node)
                })
            {
                errors.push(format!(
                    "aliased signal API at line {}",
                    node.start_position().row + 1
                ));
            }
        }
        if node.kind() == "call_expression"
            && let (Some(function), Some(arguments)) = (
                node.child_by_field_name("function"),
                node.child_by_field_name("arguments"),
            )
        {
            let function = compact(function, source);
            let name = function.rsplit("::").next().unwrap_or("");
            let mut cursor = arguments.walk();
            let argument_nodes: Vec<_> = arguments
                .named_children(&mut cursor)
                .filter(|n| !n.kind().contains("comment"))
                .collect();
            let args: Vec<String> = argument_nodes
                .iter()
                .map(|node| compact(*node, source))
                .collect();
            let first = args.first().map(String::as_str).unwrap_or("");
            // Require WNOWAIT as an unconditional bitwise-OR operand. An
            // opaque variable, masking expression or comment is not proof.
            let non_reaping = argument_nodes
                .get(3)
                .is_some_and(|node| has_wnowait(*node, source));
            let mut in_exited = false;
            let mut in_owner = false;
            if name == "waitid" && allow_owned_waitid {
                let mut ancestor = node.parent();
                while let Some(current) = ancestor {
                    if current.kind() == "function_item" {
                        in_exited = current
                            .child_by_field_name("name")
                            .is_some_and(|name| compact(name, source) == "exited");
                    } else if current.kind() == "impl_item" {
                        in_owner =
                            compact(current, source).starts_with("implProcessOpsforOwnedChild{");
                        break;
                    }
                    ancestor = current.parent();
                }
            }
            let broad = matches!(name, "waitpid" | "wait4")
                || name == "waitid"
                    && !(allow_owned_waitid
                        && in_exited
                        && in_owner
                        && function == "libc::waitid"
                        && first == "libc::P_PID"
                        && non_reaping)
                || matches!(name, "wait" | "wait3")
                    && (function.starts_with("libc::") || function.starts_with("nix::sys::wait::"));
            let null_pointer = |argument: &str| {
                argument == "null()"
                    || argument == "null_mut()"
                    || argument.ends_with("::null()")
                    || argument.ends_with("::null_mut()")
                    || (argument.contains("::null::<") || argument.contains("::null_mut::<"))
                        && argument.ends_with(">()")
                        && !argument.contains('{')
            };
            let setter = name == "signal" && !function.starts_with("tokio::")
                || name == "sigaction" && args.get(1).is_some_and(|a| !null_pointer(a));
            if broad || setter {
                errors.push(format!(
                    "{} at line {}",
                    function,
                    node.start_position().row + 1
                ));
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            visit(child, source, allow_owned_waitid, errors);
        }
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut errors = Vec::new();
    visit(tree.root_node(), source, allow_owned_waitid, &mut errors);
    errors
}

#[test]
fn production_has_no_broad_reaper_or_sigchld_setter() {
    fn scan(dir: &std::path::Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && !path
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with("_tests")
            {
                let source = std::fs::read_to_string(&path).unwrap();
                let allow_owned_waitid = path.ends_with("producer/exec.rs");
                let errors = forbidden_process_calls(&source, allow_owned_waitid);
                assert!(errors.is_empty(), "{}: {errors:?}", path.display());
            }
        }
    }
    scan(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
}

#[test]
fn process_guard_understands_comments_and_read_only_sigaction() {
    let safe = r#"fn f() { /* libc::waitpid(-1, p, 0); */ let _ = "libc::waitid(libc::P_ALL, 0, p, 0)"; libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut old); libc::sigaction(libc::SIGCHLD, std::ptr::null::<libc::sigaction>(), &mut old); child.wait(); child.try_wait(); tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()); }"#;
    assert!(forbidden_process_calls(safe, false).is_empty());
    for call in [
        "libc::waitpid(-1, p, 0)",
        "libc::waitpid(-pgid, p, 0)",
        "{ let pid = -1; libc::waitpid(pid, p, 0) }",
        "libc::waitid(libc::P_ALL, 0, p, 0)",
        "libc::waitid(libc::P_PGID, 0, p, 0)",
        "libc::signal(libc::SIGCHLD, handler)",
        "libc::sigaction(libc::SIGCHLD, &action, p)",
        "libc::sigaction(libc::SIGCHLD, if enabled { &action } else { std::ptr::null() }, p)",
    ] {
        assert!(
            !forbidden_process_calls(&format!("fn f() {{ {call}; }}"), false).is_empty(),
            "{call}"
        );
    }
    assert!(
        !forbidden_process_calls(
            "use libc::waitpid as reap; fn f() { reap(-1, p, 0); }",
            false
        )
        .is_empty()
    );
    assert!(
        !forbidden_process_calls(
            "use libc::waitid as wait; fn f() { wait(libc::P_ALL, 0, p, 0); }",
            true
        )
        .is_empty()
    );
    assert!(
        !forbidden_process_calls(
            "fn f() { let reap = libc::waitpid; reap(-1, p, 0); }",
            false
        )
        .is_empty()
    );
    assert!(
        !forbidden_process_calls(
            "use libc as c; fn f() { let reap = c::waitid; reap(c::P_ALL, 0, p, 0); }",
            false
        )
        .is_empty()
    );
    assert!(
        !forbidden_process_calls(
            "use libc as c; fn f() { let setter = c::sigaction; setter(c::SIGCHLD, &action, p); }",
            false
        )
        .is_empty()
    );
    assert!(
        forbidden_process_calls("impl ProcessOps for OwnedChild { fn exited() { libc::waitid(libc::P_PID, pid, p, libc::WEXITED | libc::WNOWAIT); } }", true)
            .is_empty()
    );
    for flags in [
        "0",
        "libc::WEXITED | libc::WNOHANG",
        "flags",
        "libc::WNOWAIT & 0",
        "(libc::WNOWAIT | libc::WEXITED) & 0",
        "!libc::WNOWAIT",
        "libc::WNOWAIT ^ libc::WEXITED",
    ] {
        let source = format!(
            "impl ProcessOps for OwnedChild {{ fn exited() {{ libc::waitid(libc::P_PID, pid, p, {flags}); }} }}"
        );
        assert!(
            !forbidden_process_calls(&source, true).is_empty(),
            "{flags}"
        );
    }
    assert!(forbidden_process_calls("impl ProcessOps for OwnedChild { fn exited() { libc::waitid(libc::P_PID, pid, p, libc::WEXITED | (libc::WNOHANG | libc::WNOWAIT)); } }", true).is_empty());
    assert!(
        !forbidden_process_calls("fn f() { libc::waitid(libc::P_PID, pid, p, 0); }", false)
            .is_empty()
    );
}
