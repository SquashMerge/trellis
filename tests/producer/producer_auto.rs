//! The approval of automatic producer runs (#660): a manual run that imports records what it
//! was approved with, in the checkout's private producer state; nothing else writes one.
#![cfg(feature = "test-support")]
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

use crate::producer_run::{STUB, World, reply};
use crate::support::state_dir;

fn approval_path(world: &World) -> PathBuf {
    state_dir(&world.checkout)
        .join("approvals")
        .join(format!("{STUB}.json"))
}

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn manual_imported_run_writes_approval_with_exact_keys() {
    use trellis::store::topology::WorkspaceIdentity;
    let world = World::new();
    world.emitting();
    assert!(!approval_path(&world).exists());

    let output = world.run(&[]);
    let run = reply(&output);
    assert_eq!(run["outcome"], "imported", "{run}");

    let path = approval_path(&world);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    let approval: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut keys: Vec<&str> = approval
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
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
    assert_eq!(approval["producer"], STUB);
    assert_eq!(approval["trustProfile"], run["trustProfile"]);
    let template = approval["templateSha256"].as_str().unwrap();
    assert!(template.len() == 64 && template.bytes().all(|b| b.is_ascii_hexdigit()));
    let mut request: Vec<&str> = approval["request"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    request.sort_unstable();
    assert_eq!(request, ["environment", "executable", "limits"]);
    assert_eq!(approval["request"]["limits"]["timeoutSecs"], 1800);
    assert_eq!(approval["request"]["limits"]["memoryMiB"], 8192);

    let identity =
        WorkspaceIdentity::discover(Some(&world.checkout.root), &world.checkout.root).unwrap();
    assert_eq!(approval["rootRecordId"], identity.record_id);
    assert_eq!(approval["rootInode"], identity.inode);
    let record = world.record(run["runId"].as_str().unwrap());
    assert!(approval["approvedAt"].as_u64().unwrap() >= record["createdAt"].as_u64().unwrap());
}

#[test]
fn failed_manual_run_writes_no_approval() {
    let world = World::new();
    world.mode(serde_json::json!({"stderr": 1, "exit": 3}));
    let output = world.run(&[]);
    assert!(!output.status.success());
    assert_eq!(reply(&output)["outcome"], "producer_failed");
    assert!(!approval_path(&world).exists());
    assert!(!approval_path(&world).parent().unwrap().exists());
}
