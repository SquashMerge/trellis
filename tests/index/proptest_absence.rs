//! Discovery: a degraded checkout must never resolve a reference the healthy checkout did
//! not. Skipped, broken or over-deep files are evidence gaps, not proof of absence.

use proptest::prelude::*;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::{Arc, atomic::AtomicBool};
use trellis::{
    indexer::{IndexOptions, index_workspace_with_native},
    model::IndexPin,
    references::{self, DocumentInput, Snapshot},
};

/// A snapshot as a store read builds it.
fn snapshot(files: &[(String, Vec<u8>)], max_file_bytes: u64) -> Snapshot {
    let root = tempfile::tempdir().unwrap();
    for (path, bytes) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let mut options = IndexOptions::new(root.path().to_owned());
    options.max_file_bytes = max_file_bytes;
    let (graph, native) = index_workspace_with_native(
        &options,
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let documents = graph
        .files
        .into_iter()
        .map(|file| {
            let key = native
                .revision
                .documents
                .iter()
                .find(|d| d.key.path == file.path)
                .unwrap()
                .key
                .clone();
            let declarations = native
                .declarations
                .iter()
                .filter(|d| d.document == key)
                .cloned()
                .collect();
            let calls = native
                .calls
                .iter()
                .filter(|c| c.document == key)
                .cloned()
                .collect();
            DocumentInput {
                semantic: Default::default(),
                key,
                file,
                declarations,
                calls,
            }
        })
        .collect();
    let gaps = native
        .coverage
        .iter()
        .filter_map(|coverage| {
            let diagnostic = coverage.diagnostic.as_deref()?;
            let reason = if diagnostic.starts_with("source is larger than --max-file-bytes") {
                "oversized"
            } else if diagnostic.starts_with("source is not UTF-8") {
                "not_utf8"
            } else {
                return None;
            };
            let key = native
                .revision
                .documents
                .iter()
                .find(|d| d.key.path == coverage.document_path)?
                .key
                .clone();
            Some((key, reason))
        })
        .collect();
    Snapshot {
        pin: IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 7,
        },
        documents,
        manifests: Vec::new(),
        evidence_gaps: gaps,
    }
}

fn project_with(files: &[(String, Vec<u8>)], only: &[String], cap: u64) -> Value {
    references::project(&snapshot(files, cap), only, 20, &mut || Ok(()))
        .unwrap()
        .data
}

/// Resolved module edges, the evidence a degraded checkout must never add to.
fn resolved(data: &Value) -> BTreeSet<(String, String)> {
    data["production"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            (
                pair["source"]["path"].as_str().unwrap().to_owned(),
                pair["target"]["path"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Compare individual sites on unchanged source files, not just aggregated module pairs.
/// Sites in the altered file have different text and cannot be compared by byte range.
fn resolved_sites(
    data: &Value,
    changed: &str,
) -> BTreeSet<(String, String, String, u64, u64, String)> {
    let mut sites = BTreeSet::new();
    for role in ["production", "tests"] {
        for edge in data[role].as_array().unwrap() {
            let source = edge["source"]["path"].as_str().unwrap();
            if source == changed {
                continue;
            }
            let target = edge["target"]["path"].as_str().unwrap();
            for site in edge["sites"].as_array().unwrap() {
                sites.insert((
                    role.to_owned(),
                    source.to_owned(),
                    target.to_owned(),
                    site["range"]["start"].as_u64().unwrap(),
                    site["range"]["end"].as_u64().unwrap(),
                    site["relationKind"].as_str().unwrap().to_owned(),
                ));
            }
        }
    }
    sites
}

fn partial_paths(data: &Value) -> BTreeSet<String> {
    data["partialFiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap().to_owned())
        .collect()
}

const USE_TEMPLATES: [&str; 11] = [
    "import { run } from './a.js';\nrun();",
    "import { run } from './a';\nrun();",
    "export * from './a.js';",
    "export * from './a';",
    "import { run } from './barrel.js';\nrun();",
    "import { run } from './lib';\nrun();",
    "import * as ns from './a.js';\nns.run();",
    "const { run } = require('./a.js');\nrun();",
    "import { run } from './a/index.js';\nrun();",
    "import { run } from './facade.js';\nrun();",
    "import { run } from './pkg';\nrun();",
];

const PROVIDER_TEMPLATES: [&str; 5] = [
    "export function run() {}\n",
    "export function other() {}\n",
    "export { run } from './b.js';\n",
    "export * from './b.js';\n",
    "",
];

const COMPETITOR_TEMPLATES: [&str; 4] = [
    "export function run() {}\n",
    "export function other() {}\n",
    "export const run = () => {};\n",
    "",
];

const BREAKS: [&str; 6] = [
    "function broken( {\n",
    "const broken = ;\n",
    "export * from ;\n",
    "{ ] ) (\n",
    "function broken() {\n",
    "class {",
];

fn text(value: &str) -> Vec<u8> {
    value.as_bytes().to_vec()
}

fn build(use_kind: usize, a_kind: usize, b_kind: usize, extra: usize) -> Vec<(String, Vec<u8>)> {
    vec![
        ("use.js".to_owned(), text(USE_TEMPLATES[use_kind])),
        ("a.js".to_owned(), text(PROVIDER_TEMPLATES[a_kind])),
        ("b.js".to_owned(), text(PROVIDER_TEMPLATES[b_kind])),
        (
            "barrel.js".to_owned(),
            text("export * from './a.js';\nexport * from './b.js';\n"),
        ),
        ("lib.js".to_owned(), text(COMPETITOR_TEMPLATES[extra % 4])),
        (
            "lib/index.js".to_owned(),
            text(COMPETITOR_TEMPLATES[extra / 4 % 4]),
        ),
        (
            "a/index.js".to_owned(),
            text(COMPETITOR_TEMPLATES[extra / 16 % 4]),
        ),
        (
            "facade.js".to_owned(),
            text("export * from './barrel.js';\n"),
        ),
        (
            "pkg/index.js".to_owned(),
            text(COMPETITOR_TEMPLATES[extra / 32 % 4]),
        ),
    ]
}

#[test]
fn skipped_competing_star_export_cannot_prove_a_name_unambiguous() {
    let healthy = build(4, 0, 0, 0);
    let filter = vec!["barrel.js".to_owned()];
    let cap = 64 * 1024;
    let before = project_with(&healthy, &filter, cap);
    let mut skipped = healthy.clone();
    skipped
        .iter_mut()
        .find(|(path, _)| path == "b.js")
        .unwrap()
        .1 = b"export function run() {}\n\xff\xfe".to_vec();
    let after = project_with(&skipped, &filter, cap);
    assert!(
        after["partialFiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| { file["path"] == "b.js" && file["reason"] == "not_utf8" }),
        "{after}"
    );
    let edge = ("barrel.js".to_owned(), "a.js".to_owned());
    assert!(!resolved(&before).contains(&edge), "{before}");
    assert!(!resolved(&after).contains(&edge), "{after}");
}

#[test]
fn visit_limited_competitor_cannot_prove_a_name_unambiguous() {
    let healthy = build(4, 0, 0, 0);
    let filter = vec!["barrel.js".to_owned()];
    let cap = 4 * 1024 * 1024;
    let before = project_with(&healthy, &filter, cap);
    let mut degraded = healthy.clone();
    let bytes = &mut degraded
        .iter_mut()
        .find(|(path, _)| path == "b.js")
        .unwrap()
        .1;
    bytes.extend_from_slice(b"const wide = [");
    bytes.extend_from_slice("0,".repeat(1_000_001).as_bytes());
    bytes.extend_from_slice(b"];\n");
    let after = project_with(&degraded, &filter, cap);
    assert!(
        after["partialFiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| { file["path"] == "b.js" && file["reason"] == "visit_limit" }),
        "{}",
        after["partialFiles"]
    );
    assert!(
        resolved_sites(&after, "b.js").is_subset(&resolved_sites(&before, "b.js")),
        "visit-limited competitor added a resolved site: {after}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("TRELLIS_PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(40),
        ..ProptestConfig::default()
    })]

    #[test]
    fn a_degraded_checkout_never_adds_a_resolved_edge(
        use_kind in 0usize..USE_TEMPLATES.len(),
        a_kind in 0usize..PROVIDER_TEMPLATES.len(),
        b_kind in 0usize..PROVIDER_TEMPLATES.len(),
        extra in 0usize..64,
        target in 0usize..7,
        mode in prop::sample::select(vec![0usize, 1, 2, 4, 5, 6]),
        detail in 0usize..64,
        filter in 0usize..5,
    ) {
        let files = build(use_kind, a_kind, b_kind, extra);
        let only: Vec<String> = match filter {
            0 => vec![],
            1 => vec!["use.js".into()],
            2 => vec!["barrel.js".into()],
            3 => vec!["a.js".into()],
            _ => vec!["use.js".into(), "barrel.js".into()],
        };
        // A small cap keeps the "oversized" case cheap; every healthy file is far below it.
        let cap = 64 * 1024;
        let healthy = project_with(&files, &only, cap);
        // Degrade one module, never the entry consumer.
        let candidates = [
            "a.js",
            "b.js",
            "barrel.js",
            "lib/index.js",
            "a/index.js",
            "facade.js",
            "pkg/index.js",
        ];
        let degraded_path = candidates[target];
        let mut degraded = files.clone();
        for (path, bytes) in &mut degraded {
            if path != degraded_path {
                continue;
            }
            *bytes = match mode {
                0 => [bytes.as_slice(), b"\n", BREAKS[detail % BREAKS.len()].as_bytes()].concat(),
                1 => {
                    if bytes.is_empty() {
                        text(BREAKS[0])
                    } else {
                        let cut = 1 + detail % bytes.len();
                        format!("{}function broken( {{", String::from_utf8_lossy(&bytes[..cut]))
                            .into_bytes()
                    }
                }
                2 => [bytes.as_slice(), format!(
                    "\nfunction broken() {{ return {}1{}; }}\n",
                    "(".repeat(400),
                    ")".repeat(400)
                ).as_bytes()].concat(),
                4 => [bytes.as_slice(), format!(
                    "\nfunction broken() {{ return {}1{}; }}\n",
                    "[".repeat(400),
                    "]".repeat(400)
                ).as_bytes()].concat(),
                5 => [bytes.as_slice(), b"\n\xff\xfe"].concat(),
                _ => {
                    let mut oversized = bytes.clone();
                    oversized.resize(cap as usize + 16, b' ');
                    oversized
                }
            };
        }
        let data = project_with(&degraded, &only, cap);
        prop_assert!(
            partial_paths(&data).contains(degraded_path),
            "mode {mode} did not register a partial {degraded_path}: healthy={healthy} degraded={data}"
        );
        let expected_reason = match mode {
            2 | 4 => "depth_limit",
            5 => "not_utf8",
            6 => "oversized",
            _ => "syntax_error",
        };
        prop_assert!(
            data["partialFiles"].as_array().unwrap().iter().any(|file| {
                file["path"] == degraded_path && file["reason"] == expected_reason
            }),
            "mode {mode} did not reach {expected_reason} on {degraded_path}: {}",
            data["partialFiles"]
        );
        if std::env::var_os("TRELLIS_PROPTEST_TRACE").is_some() {
            eprintln!(
                "case: healthy={} degraded={} partial={}",
                resolved(&healthy).len(),
                resolved(&data).len(),
                partial_paths(&data).len()
            );
        }
        let gained: Vec<_> = resolved_sites(&data, degraded_path)
            .difference(&resolved_sites(&healthy, degraded_path))
            .cloned()
            .collect();
        prop_assert!(
            gained.is_empty(),
            "degraded checkout gained edges {gained:?} (mode {mode}, filter {filter})\nhealthy={healthy}\ndegraded={data}"
        );
    }
}
