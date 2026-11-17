use super::*;

fn revision(revision_id: &str) -> Revision {
    let source_set_id = "source-set:v1:root".to_owned();
    Revision {
        id: revision_id.into(),
        source_set_id: source_set_id.clone(),
        documents: vec![Document {
            key: DocumentKey {
                source_set_id,
                language: "javascript".into(),
                path: "a.js".into(),
            },
            revision_id: revision_id.into(),
            content_hash: hash(b"f();"),
            byte_length: 4,
        }],
        toolchain_hash: "1".repeat(64),
        config_hash: hash(revision_id.as_bytes()),
        dependency_hash: "3".repeat(64),
    }
}

fn check_inventory_registry(rust_inventory: &Value, semantic_inventory: &Value) -> Result<()> {
    ensure!(
        rust_inventory == semantic_inventory,
        "Rust and semantic-contract native extraction inventories differ"
    );
    Ok(())
}

#[test]
fn native_extraction_inventory_matches_semantic_contract_registry() {
    let rust_inventory = json!(EXTRACTION_INPUTS);
    let output = std::process::Command::new("node")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "--input-type=module",
            "-e",
            r#"
import { productionInventoryEntries } from './tools/semantic-contract/native-inventory.mjs';
console.log(JSON.stringify(productionInventoryEntries()));
"#,
        ])
        .output()
        .expect("Node is required by the repository verification suite");
    assert!(
        output.status.success(),
        "semantic inventory lookup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let semantic_inventory: Value = serde_json::from_slice(&output.stdout).unwrap();
    check_inventory_registry(&rust_inventory, &semantic_inventory).unwrap();
    // A stale JS version must not disappear through Rust-driven lookups.
    let mut extra_js_version = semantic_inventory.clone();
    let mut stale = extra_js_version[0].clone();
    stale[1] = json!("native-v3");
    extra_js_version.as_array_mut().unwrap().push(stale);
    assert!(check_inventory_registry(&rust_inventory, &extra_js_version).is_err());
    // The same comparison must reject drift on either side, not just a missing entry.
    for change_rust in [false, true] {
        for change_version in [false, true] {
            let mut rust = rust_inventory.clone();
            let mut semantic = semantic_inventory.clone();
            let changed = if change_rust {
                &mut rust
            } else {
                &mut semantic
            };
            if change_version {
                changed[0][1] = json!("native-unregistered-future");
            } else {
                changed[0][2][0][1] = json!(["config"]);
            }
            assert!(
                check_inventory_registry(&rust, &semantic).is_err(),
                "accepted drift: Rust={change_rust}, version={change_version}"
            );
        }
    }
}

#[test]
fn declared_inventory_authenticates_captured_components_and_refuses_unproven_reuse() {
    let revision = revision("revision:v2:r1");
    for language in LANGUAGES {
        assert!(
            selected_extraction_context_for_inventory(language, &["config"]).is_err(),
            "synthetic nonempty declaration must not reuse an unauthenticated empty context"
        );
    }
    let mut captured = revision.clone();
    captured.config_hash = hash(b"captured config");
    assert_eq!(
        authenticated_extraction_context(
            "rust",
            &["config"],
            &[("config", Some(b"captured config"))],
            &captured,
        )
        .unwrap(),
        extraction_context("rust", &[("config".into(), captured.config_hash.clone())]).unwrap()
    );
    for (inventory, observed) in [
        (vec!["config"], vec![]),
        (vec!["config"], vec![("config", None)]),
        (
            vec!["config"],
            vec![("config", Some(b"stale config".as_slice()))],
        ),
        (
            vec![],
            vec![("config", Some(b"captured config".as_slice()))],
        ),
        (
            vec!["settings"],
            vec![("settings", Some(b"captured config".as_slice()))],
        ),
        (
            vec!["config", "config"],
            vec![("config", Some(b"captured config".as_slice()))],
        ),
    ] {
        assert!(
            authenticated_extraction_context("rust", &inventory, &observed, &captured).is_err(),
            "{inventory:?} {observed:?}"
        );
    }
    let mut forged_digest = captured.clone();
    forged_digest.config_hash = "not-a-captured-digest".into();
    assert!(
        authenticated_extraction_context(
            "rust",
            &["config"],
            &[("config", Some(b"captured config"))],
            &forged_digest,
        )
        .is_err()
    );
}
