#[cfg(unix)]
#[tokio::test]
async fn slow_checkout_count_preserves_tracking_and_does_not_starve_later_rows() {
    use crate::{git_status::GitReader, store::topology::WorkspaceIdentity};
    use std::{fs, os::unix::fs::PermissionsExt};

    let temp = tempfile::tempdir().unwrap();
    let mut rows = Vec::new();
    for name in ["slow", "later", "last"] {
        let root = temp.path().join(name);
        git2::Repository::init(&root).unwrap();
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        rows.push(serde_json::json!({
            "rootKey": identity.root_key,
            "workspaceRoot": identity.root,
            "state": "available"
        }));
    }
    let head = serde_json::json!({
        "kind": "branch", "name": "main", "commit": "1111111111111111111111111111111111111111",
        "shortCommit": "1111111"
    });
    let upstream = serde_json::json!({
        "name": "origin/main", "ahead": 2, "behind": 1, "basis": "last_fetch"
    });
    let head_response = serde_json::json!({
        "mode": "head", "result": {"Ok": {"head": head, "upstream": upstream}}
    });
    let summary_response = serde_json::json!({
        "mode": "summary", "result": {
            "state": "available", "head": head, "upstream": upstream,
            "workingTree": {"state": "available", "changedFiles": 3}
        }
    });
    let exe = temp.path().join("reader");
    fs::write(&exe, format!(
        "#!/bin/sh\nif [ \"$5\" = head ]; then printf '%s' '{}'; elif [ \"$3\" = '{}' ]; then exec /bin/sleep 60; else printf '%s' '{}'; fi\n",
        head_response, rows[0]["workspaceRoot"].as_str().unwrap(), summary_response
    )).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    super::enrich_checkout_git_with_reader(&mut rows, &GitReader::new(exe)).await;
    let measured: Vec<_> = rows.iter().map(|row| row["git"].clone()).collect();
    let mut expected: Vec<_> = (0..3)
        .map(|_| {
            serde_json::json!({
                "state": "available", "head": head, "upstream": upstream,
                "workingTree": {"state": "available", "changedFiles": 3}
            })
        })
        .collect();
    expected[0]["workingTree"] = serde_json::json!({
        "state": "unavailable", "reason": "deadline_exceeded"
    });
    assert_eq!(measured, expected);
}
