//! Java absence discovery: real indexing and structural projection, with a small
//! independently specified competing-declaration model and a one-file metamorphism.
use proptest::prelude::*;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::{Arc, atomic::AtomicBool};
use trellis::{
    indexer::{IndexOptions, index_workspace_with_native},
    model::IndexPin,
    references::{self, DocumentInput, Snapshot},
};

const CAP: u64 = 65536;
const CHANGED: &str = "odd/Competitor.java";
const CONSUMER: &str = "app/Use.java";
const TARGET: &str = "types/Target.java";
type Files = Vec<(String, Vec<u8>)>;
type Site = (String, String, String, u64, u64, String);

fn project(files: &Files, filter: &[String]) -> Value {
    // Keep generated source trees inside this worktree, even when TMPDIR is shared.
    let root = tempfile::Builder::new()
        .prefix(".java-absence-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    for (path, bytes) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let mut options = IndexOptions::new(root.path().to_owned());
    options.max_file_bytes = CAP;
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
            DocumentInput {
                semantic: Default::default(),
                declarations: native
                    .declarations
                    .iter()
                    .filter(|d| d.document == key)
                    .cloned()
                    .collect(),
                calls: native
                    .calls
                    .iter()
                    .filter(|c| c.document == key)
                    .cloned()
                    .collect(),
                key,
                file,
            }
        })
        .collect();
    let evidence_gaps = native
        .coverage
        .iter()
        .filter_map(|c| {
            let diagnostic = c.diagnostic.as_deref()?;
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
                .find(|d| d.key.path == c.document_path)?
                .key
                .clone();
            Some((key, reason))
        })
        .collect();
    let snapshot = Snapshot {
        pin: IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 7,
        },
        documents,
        manifests: Vec::new(),
        evidence_gaps,
    };
    references::project(&snapshot, filter, 20, &mut || Ok(()))
        .unwrap()
        .data
}

fn sites(data: &Value) -> BTreeSet<Site> {
    let mut result = BTreeSet::new();
    for role in ["production", "tests"] {
        for edge in data[role].as_array().unwrap() {
            let source = edge["source"]["path"].as_str().unwrap();
            if source == CHANGED {
                continue;
            }
            // Keep external descriptors as well as native paths, without generation IDs.
            let target = if let Some(path) = edge["target"]["path"].as_str() {
                path.to_owned()
            } else {
                let mut descriptor = edge["target"].clone();
                if let Some(obj) = descriptor.as_object_mut() {
                    obj.remove("sourceSetId");
                }
                descriptor.to_string()
            };
            for site in edge["sites"].as_array().unwrap() {
                result.insert((
                    role.into(),
                    source.into(),
                    target.clone(),
                    site["range"]["start"].as_u64().unwrap(),
                    site["range"]["end"].as_u64().unwrap(),
                    site["relationKind"].as_str().unwrap().into(),
                ));
            }
            // No sample truncation may conceal a newly resolved site.
            assert_eq!(
                edge["sites"].as_array().unwrap().len() as u64,
                edge["count"].as_u64().unwrap(),
                "{edge}"
            );
        }
    }
    result
}

fn has_target(data: &Value) -> bool {
    sites(data).iter().any(|s| s.1 == CONSUMER && s.2 == TARGET)
}

// Each healthy case contains a real competing declaration. Removing it makes
// the marked wrong target visible, which calibrates the setup independently of
// the production resolver's handling of partial sources.
fn layout(shape: usize, name: usize, static_kind: usize) -> Files {
    let pkg = ["p", "pkg", "alpha"][name];
    let ty = ["Target", "Thing", "Item"][name];
    let (competitor, consumer) = match shape {
        0 => (
            format!("package {pkg}; public class {ty} {{}}\n"),
            format!("package app; import {pkg}.{ty}; class Use {{}}\n"),
        ),
        1 => (
            format!("package {pkg}; public class {ty} {{}}\n"),
            format!("package app; import {pkg}.*; class Use {{}}\n"),
        ),
        _ => (
            format!("package app; class {pkg} {{ static class {ty} {{}} }}\n"),
            format!("package app; class Use {{ {pkg}.{ty} value; }}\n"),
        ),
    };
    let static_import = match static_kind {
        0 => "import static tools.Outer.pkg;",
        1 => "import static tools.Outer.*;",
        _ => "import static tools.Outer.VALUE;",
    };
    [
        (TARGET, format!("package {pkg}; public class {ty} {{}}\n")),
        (CHANGED, competitor),
        (CONSUMER, consumer),
        ("app/StaticUse.java", format!("package app; {static_import} class StaticUse {{ {pkg}.{ty} v; }}\n")),
        ("tools/Outer.java", "package tools; public class Outer { public static class pkg {} public static int VALUE; }\n".into()),
        ("spare/Keep.java", "package spare; public class Keep {}\n".into()),
        ("app/KeepUse.java", "package app; import spare.Keep; class KeepUse {}\n".into()),
    ].into_iter().map(|(p, s)| (p.into(), s.into_bytes())).collect()
}

fn degrade(files: &Files, mode: usize) -> Files {
    let mut result = files.clone();
    let bytes = &mut result.iter_mut().find(|(p, _)| p == CHANGED).unwrap().1;
    match mode {
        0 => bytes.extend_from_slice(b"\nclass Broken { void f( {\n"),
        // Lose the package declaration, retaining the original file in a broken annotation.
        1 => *bytes = [b"@Foo( ".as_slice(), bytes.as_slice()].concat(),
        2 => bytes.extend_from_slice(
            format!(
                "\nclass Deep {{ Object x = {}null{}; }}\n",
                "(".repeat(400),
                ")".repeat(400)
            )
            .as_bytes(),
        ),
        3 => bytes.extend_from_slice(b"\n// non-utf8: \xff\n"),
        _ => bytes.resize(CAP as usize + 1, b' '),
    }
    result
}

fn check(
    shape: usize,
    name: usize,
    static_kind: usize,
    mode: usize,
    filtered: bool,
) -> Result<(), String> {
    let files = layout(shape, name, static_kind);
    let filter = if filtered {
        vec![
            CONSUMER.to_owned(),
            "app/StaticUse.java".into(),
            "app/KeepUse.java".into(),
        ]
    } else {
        vec![]
    };
    let healthy = project(&files, &filter);
    if !healthy["partialFiles"].as_array().unwrap().is_empty()
        || !sites(&healthy)
            .iter()
            .any(|site| site.1 == "app/KeepUse.java" && site.2 == "spare/Keep.java")
    {
        return Err(format!(
            "healthy control missing an independent resolved site: {healthy}"
        ));
    }
    let mut absent = files.clone();
    absent.retain(|(p, _)| p != CHANGED);
    let removed = project(&absent, &filter);
    if has_target(&healthy) || !has_target(&removed) {
        return Err(format!(
            "invalid witness shape={shape} name={name} static={static_kind}: healthy={healthy} removed={removed}"
        ));
    }
    let broken = degrade(&files, mode);
    assert_eq!(files.iter().zip(&broken).filter(|(a, b)| a != b).count(), 1);
    let after = project(&broken, &filter);
    let reason = [
        "syntax_error",
        "syntax_error",
        "depth_limit",
        "not_utf8",
        "oversized",
    ][mode];
    let partial = after["partialFiles"].as_array().unwrap();
    if partial.len() != 1 || partial[0]["path"] != CHANGED || partial[0]["reason"] != reason {
        return Err(format!(
            "wrong partial: mode={mode} expected={reason}: {partial:?}"
        ));
    }
    let before_sites = sites(&healthy);
    let after_sites = sites(&after);
    let gained: Vec<_> = after_sites.difference(&before_sites).collect();
    if !gained.is_empty() {
        return Err(format!(
            "gained sites {gained:?}; shape={shape} name={name} static={static_kind} mode={mode} filtered={filtered}\nhealthy={healthy}\nafter={after}"
        ));
    }
    if std::env::var_os("TRELLIS_JAVA_ABSENCE_TRACE").is_some() {
        eprintln!(
            "java-absence shape={shape} name={name} static={static_kind} mode={mode} filtered={filtered} before={} after={} witness=1",
            before_sites.len(),
            after_sites.len()
        );
    }
    Ok(())
}

#[test]
fn skipped_duplicate_witness() {
    check(0, 0, 2, 3, false).unwrap();
}

#[test]
fn each_competing_shape_and_partial_reason_is_reached() {
    for shape in 0..3 {
        for mode in 0..5 {
            check(shape, shape, mode % 3, mode, mode % 2 == 0).unwrap();
        }
    }
}

#[test]
fn unplaced_java_package_does_not_fall_back_to_jdk() {
    for import in ["javax.swing.Local", "javax.swing.*"] {
        let files: Files = [
            (
                CHANGED,
                "package javax.swing; public class Local {}\n".to_owned(),
            ),
            (
                CONSUMER,
                format!("package app; import {import}; class Use {{}}\n"),
            ),
        ]
        .into_iter()
        .map(|(path, text)| (path.into(), text.into_bytes()))
        .collect();
        // The baseline must really resolve to the in-repository type.
        let healthy = project(&files, &[]);
        assert!(
            healthy["production"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| {
                    edge["source"]["path"] == CONSUMER && edge["target"]["path"] == CHANGED
                }),
            "{healthy}"
        );
        for (mode, expected) in [(1, "syntax_error"), (3, "not_utf8"), (4, "oversized")] {
            let after = project(&degrade(&files, mode), &[]);
            assert_eq!(after["partialFiles"].as_array().unwrap().len(), 1);
            assert_eq!(after["partialFiles"][0]["path"], CHANGED);
            assert_eq!(after["partialFiles"][0]["reason"], expected);
            assert!(
                sites(&after).is_subset(&sites(&healthy)),
                "import={import} mode={mode} after={after}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_JAVA_ABSENCE_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(24), ..ProptestConfig::default() })]
    #[test]
    fn partial_sources_do_not_add_resolved_sites(shape in 0usize..3, name in 0usize..3, static_kind in 0usize..3, mode in 0usize..5, filtered in any::<bool>()) {
        if let Err(error) = check(shape, name, static_kind, mode, filtered) {
            prop_assert!(false, "{error}");
        }
    }
}
