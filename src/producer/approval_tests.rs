use super::approval::{self, Approval, ApprovalRead, ApprovalRequest};
use super::detect::Environment;
use super::plan::LimitsRequest;
use super::state::StateDir;
use super::{ProducerName, TrustProfile};
use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
use serde_json::{Value, json};
use std::fs;
use tempfile::TempDir;

struct World {
    _temp: TempDir,
    roots: TopologyRoots,
    identity: WorkspaceIdentity,
}

fn world() -> World {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("work");
    fs::create_dir(&workspace).unwrap();
    World {
        roots: TopologyRoots::isolated_for_tests(
            temp.path().join("cache"),
            temp.path().join("data"),
        ),
        identity: WorkspaceIdentity::discover(Some(&workspace), &workspace).unwrap(),
        _temp: temp,
    }
}

fn approval(identity: &WorkspaceIdentity) -> Approval {
    Approval {
        producer: ProducerName::RustAnalyzer.as_str().to_owned(),
        trust_profile: TrustProfile::Trusted,
        request: ApprovalRequest {
            limits: LimitsRequest {
                timeout_secs: 1800,
                memory_mib: 8192,
            },
            executable: None,
            environment: Environment {
                path: "/usr/bin".into(),
                home: "/h".into(),
                ..Environment::default()
            },
        },
        template_sha256: "a".repeat(64),
        root_record_id: identity.record_id.clone(),
        root_inode: identity.inode,
        approved_at: 1_762_790_400_000,
    }
}

#[test]
fn a_missing_directory_or_record_reads_absent() {
    let world = world();
    let state = StateDir::open(&world.roots, &world.identity).unwrap();
    assert!(!state.path().join("approvals").exists());
    let read = approval::read(&state, "rust-analyzer", &world.identity);
    assert_eq!(read, ApprovalRead::Absent);
    assert_eq!(read.reason(), Some("no approval record"));
    approval::write(&state, &approval(&world.identity)).unwrap();
    assert_eq!(
        approval::read(&state, "scip-java", &world.identity),
        ApprovalRead::Absent
    );
}

#[test]
fn the_record_has_exactly_the_documented_keys_and_round_trips() {
    let world = world();
    let state = StateDir::open(&world.roots, &world.identity).unwrap();
    let written = approval(&world.identity);
    approval::write(&state, &written).unwrap();
    let bytes = fs::read(state.approval_path("rust-analyzer").unwrap()).unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "approvedAt",
            "producer",
            "request",
            "rootInode",
            "rootRecordId",
            "templateSha256",
            "trustProfile"
        ]
    );
    let mut request: Vec<_> = json["request"].as_object().unwrap().keys().collect();
    request.sort();
    assert_eq!(request, ["environment", "executable", "limits"]);
    assert_eq!(json["request"]["limits"]["memoryMiB"], json!(8192));
    assert_eq!(
        approval::read(&state, "rust-analyzer", &world.identity),
        ApprovalRead::Valid(Box::new(written))
    );
}

#[test]
fn an_unknown_key_a_foreign_producer_or_garbage_is_unreadable() {
    let world = world();
    let state = StateDir::open(&world.roots, &world.identity).unwrap();
    approval::write(&state, &approval(&world.identity)).unwrap();
    let path = state.approval_path("rust-analyzer").unwrap();
    let good: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut extra = good.clone();
    extra["extra"] = json!(1);
    let mut renamed = good.clone();
    renamed["producer"] = json!("scip-java");
    let mut missing = good;
    missing.as_object_mut().unwrap().remove("approvedAt");
    for bytes in [
        serde_json::to_vec(&extra).unwrap(),
        serde_json::to_vec(&renamed).unwrap(),
        serde_json::to_vec(&missing).unwrap(),
        b"not json".to_vec(),
        vec![b' '; 64 * 1024 + 1],
    ] {
        fs::write(&path, bytes).unwrap();
        let read = approval::read(&state, "rust-analyzer", &world.identity);
        assert_eq!(read, ApprovalRead::Unreadable);
        assert_eq!(read.reason(), Some("approval record unreadable"));
    }
}

#[test]
fn another_record_id_or_inode_is_another_root() {
    let world = world();
    let state = StateDir::open(&world.roots, &world.identity).unwrap();
    let mut other = approval(&world.identity);
    other.root_record_id = "00000000-0000-0000-0000-000000000000".into();
    approval::write(&state, &other).unwrap();
    let read = approval::read(&state, "rust-analyzer", &world.identity);
    assert_eq!(read, ApprovalRead::OtherRoot);
    assert_eq!(read.reason(), Some("approval belongs to another root"));

    let mut moved = approval(&world.identity);
    moved.root_inode += 1;
    approval::write(&state, &moved).unwrap();
    assert_eq!(
        approval::read(&state, "rust-analyzer", &world.identity),
        ApprovalRead::OtherRoot
    );
    approval::write(&state, &approval(&world.identity)).unwrap();
    assert!(matches!(
        approval::read(&state, "rust-analyzer", &world.identity),
        ApprovalRead::Valid(_)
    ));
}

#[test]
fn unsafe_state_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;
    let world = world();
    let state = StateDir::open(&world.roots, &world.identity).unwrap();
    approval::write(&state, &approval(&world.identity)).unwrap();
    let dir = state.path().join("approvals");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        approval::read(&state, "rust-analyzer", &world.identity),
        ApprovalRead::Unreadable
    );
}

#[cfg(feature = "test-support")]
mod conversion {
    use super::*;
    use crate::indexer::{IndexOptions, index_workspace_bundle};
    use crate::producer::approval::Refusal;
    use crate::producer::plan::{self, PlanRequest};
    use crate::producer::profiles::Stub;
    use crate::producer::run::SupersedeToken;
    use crate::producer::state::RunOrigin;
    use crate::store::Store;
    use std::{
        path::PathBuf,
        sync::{Arc, atomic::AtomicBool},
    };

    struct Checkout {
        _temp: TempDir,
        root: PathBuf,
        store: Store,
        stub: Stub,
    }

    fn script(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn publish(store: &Store) -> String {
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(store.workspace_root().into()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let leader = store.publication_for_tests().unwrap();
        let expected = store.index_baseline().unwrap();
        store
            .publish_native(&graph, &capture, &native, &leader, expected, &cancel)
            .unwrap();
        store
            .with_baseline_native_reader(|reader| Ok(reader.header()?.revision.id))
            .unwrap()
            .1
            .unwrap()
    }

    fn checkout() -> (Checkout, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap().join("work");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.js"), "function a(){}\n").unwrap();
        let stub_path = root.parent().unwrap().join("stub");
        script(&stub_path);
        let store = Store::open_for_tests(&root.parent().unwrap().join("state"), &root).unwrap();
        let revision = publish(&store);
        let stub = Stub {
            path: stub_path,
            args: Vec::new(),
        };
        (
            Checkout {
                _temp: temp,
                root,
                store,
                stub,
            },
            revision,
        )
    }

    fn request(trust: TrustProfile) -> PlanRequest {
        PlanRequest::parse(&json!({
            "version": 1,
            "producer": crate::producer::profiles::STUB_NAME,
            "trustProfile": trust,
            "limits": {"timeoutSecs": 600, "memoryMiB": 512},
            "executable": null,
            "environment": {"PATH": "/usr/bin", "HOME": "/h"},
        }))
        .unwrap()
    }

    /// The approval a manual run of `request` against `checkout` would leave.
    fn approved(checkout: &Checkout, request: &PlanRequest) -> Approval {
        let prepared = plan::prepare(&checkout.store, request, Some(&checkout.stub)).unwrap();
        approval::record(request, &prepared, checkout.store.state_topology().1)
    }

    #[test]
    fn a_matching_approval_becomes_an_automatic_run_with_the_recorded_trust() {
        let (checkout, revision) = checkout();
        let approved = approved(&checkout, &request(TrustProfile::Restricted));
        assert_eq!(approved.trust_profile, TrustProfile::Restricted);
        let token = SupersedeToken::default();
        let run = approval::run_request(
            &checkout.store,
            &approved,
            Some(&checkout.stub),
            &revision,
            token.clone(),
        )
        .unwrap();
        assert_eq!(run.origin, RunOrigin::Auto);
        assert!(run.supersede.is_some());
        // Trust, limits and settings come from the record alone.
        assert_eq!(run.plan.trust_profile, TrustProfile::Restricted);
        assert_eq!(run.plan.limits, approved.request.limits);
        assert_eq!(run.plan.settings, Default::default());
        let planned = plan::plan(&checkout.store, &run.plan, Some(&checkout.stub)).unwrap();
        assert_eq!(planned["planSha256"], json!(run.plan_sha256));

        // The profile is read from the record, and it is part of the template: a record whose
        // trust was edited no longer matches the hash it carries.
        let edited = Approval {
            trust_profile: TrustProfile::Trusted,
            ..approved
        };
        let refusal = approval::run_request(
            &checkout.store,
            &edited,
            Some(&checkout.stub),
            &revision,
            token,
        )
        .unwrap_err();
        assert_eq!(refusal, Refusal::Template);
    }

    #[test]
    fn a_head_that_moved_refuses_before_the_template_is_compared() {
        let (checkout, revision) = checkout();
        let mut approved = approved(&checkout, &request(TrustProfile::Restricted));
        approved.template_sha256 = "b".repeat(64);
        let refusal = approval::run_request(
            &checkout.store,
            &approved,
            Some(&checkout.stub),
            "revision:v2:older",
            SupersedeToken::default(),
        )
        .unwrap_err();
        assert_eq!(refusal, Refusal::HeadMoved);
        assert_ne!(revision, "revision:v2:older");
    }

    #[test]
    fn a_changed_template_refuses() {
        let (checkout, revision) = checkout();
        let approved = approved(&checkout, &request(TrustProfile::Restricted));
        // Another launcher is another producer setup: the approval no longer covers it.
        let other = checkout.root.parent().unwrap().join("other-stub");
        script(&other);
        let other = Stub {
            path: other,
            args: Vec::new(),
        };
        let refusal = approval::run_request(
            &checkout.store,
            &approved,
            Some(&other),
            &revision,
            SupersedeToken::default(),
        )
        .unwrap_err();
        assert_eq!(refusal, Refusal::Template);
    }

    #[test]
    fn planning_failures_carry_their_typed_code() {
        let (checkout, revision) = checkout();
        let mut approved = approved(&checkout, &request(TrustProfile::Restricted));
        approved.producer = "scip-java".into();
        approved.request.executable = Some("relative".into());
        let refusal = approval::run_request(
            &checkout.store,
            &approved,
            Some(&checkout.stub),
            &revision,
            SupersedeToken::default(),
        )
        .unwrap_err();
        assert_eq!(refusal, Refusal::Failed("invalid_request".into()));
    }
}
