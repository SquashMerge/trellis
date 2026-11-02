//! Rust partial-source absence: compare individual unchanged sites, including a
//! positive control which establishes that each competing binding matters.
use super::proptest_absence::{project_at, project_with, resolved_sites};
use proptest::prelude::*;
use serde_json::Value;

const CAP: u64 = 16 * 1024;
type Files = Vec<(String, Vec<u8>)>;

fn put(files: &mut Files, path: &str, source: impl AsRef<[u8]>) {
    if let Some((_, bytes)) = files.iter_mut().find(|(name, _)| name == path) {
        *bytes = source.as_ref().to_vec();
    } else {
        files.push((path.into(), source.as_ref().to_vec()));
    }
}

fn contains_site(data: &Value, source: &str, target: &str) -> bool {
    resolved_sites(data, "")
        .iter()
        .any(|(_, s, t, _, _, _)| s == source && t == target)
}

fn glob_files(alias: bool, nested_package: bool) -> Files {
    let mut files = Vec::new();
    put(
        &mut files,
        "src/lib.rs",
        "pub mod p; pub mod q; pub mod provider; mod user;\n",
    );
    put(&mut files, "src/p.rs", "pub fn f() {}\n");
    put(&mut files, "src/provider.rs", "pub fn original() {}\n");
    put(
        &mut files,
        "src/q.rs",
        if alias {
            "pub use crate::provider::{original as f};\n"
        } else {
            "pub fn f() {}\n"
        },
    );
    put(
        &mut files,
        "src/user.rs",
        "use crate::p::*; use crate::q::*; fn run() { f(); }\n",
    );
    if nested_package {
        put(
            &mut files,
            "Cargo.toml",
            "[package]\nname='outer'\nversion='0.1.0'\nedition='2021'\n",
        );
        put(
            &mut files,
            "nested/Cargo.toml",
            "[package]\nname='inner'\nversion='0.1.0'\nedition='2021'\n",
        );
        put(&mut files, "nested/src/lib.rs", "pub fn f() {}\n");
    }
    files
}

fn owner_files(custom_path: bool, nested_package: bool) -> Files {
    let mut files = Vec::new();
    put(&mut files, "src/lib.rs", "mod x; mod y;\n");
    put(&mut files, "src/x.rs", "mod z;\n");
    put(&mut files, "src/x/z.rs", "fn run() { crate::y::f(); }\n");
    put(&mut files, "src/y.rs", "pub fn f() {}\n");
    put(
        &mut files,
        "src/main.rs",
        if custom_path && nested_package {
            "#[path = \"../nested/src/lib.rs\"] mod x; mod y { pub fn f() {} } fn main() {}\n"
        } else if custom_path {
            "#[path = \"x.rs\"] mod x; mod y { pub fn f() {} } fn main() {}\n"
        } else {
            "mod x; mod y { pub fn f() {} } fn main() {}\n"
        },
    );
    if nested_package {
        put(
            &mut files,
            "nested/Cargo.toml",
            "[package]\nname='inner'\nversion='0.1.0'\nedition='2021'\n",
        );
        put(&mut files, "nested/src/lib.rs", "pub fn f() {}\n");
    }
    files
}

fn degraded(files: &Files, path: &str, mode: u8, use_cut: bool) -> (Files, &'static str) {
    let mut out = files.clone();
    let bytes = &mut out.iter_mut().find(|(name, _)| name == path).unwrap().1;
    let reason = match mode {
        0 if path == "src/main.rs" => {
            bytes.insert(0, b'(');
            "syntax_error"
        }
        0 if use_cut => {
            *bytes = b"pub use crate::provider::{original as f;\n".to_vec();
            "syntax_error"
        }
        0 => {
            bytes.extend_from_slice(b"\nfn broken( {\n");
            "syntax_error"
        }
        1 => {
            bytes.extend_from_slice(b"\n// \xff\n");
            "not_utf8"
        }
        2 => {
            bytes.extend_from_slice(b"\n// ");
            bytes.resize(CAP as usize + 1, b'x');
            "oversized"
        }
        _ if use_cut => {
            *bytes = format!(
                "pub use crate::provider::{}original as f{};\n",
                "{".repeat(180),
                "}".repeat(180)
            )
            .into_bytes();
            "depth_limit"
        }
        _ => {
            bytes.extend_from_slice(
                format!(
                    "\nfn deep() {{ let _ = {}0{}; }}\n",
                    "(".repeat(400),
                    ")".repeat(400)
                )
                .as_bytes(),
            );
            "depth_limit"
        }
    };
    (out, reason)
}

fn check(
    files: &Files,
    path: &str,
    mode: u8,
    use_cut: bool,
    filter: &[String],
) -> Result<(), TestCaseError> {
    let root = tempfile::tempdir().unwrap();
    check_at(root.path(), files, path, mode, use_cut, filter)
}

fn check_at(
    root: &std::path::Path,
    files: &Files,
    path: &str,
    mode: u8,
    use_cut: bool,
    filter: &[String],
) -> Result<(), TestCaseError> {
    let before = project_at(root, files, filter, CAP);
    let (changed, reason) = degraded(files, path, mode, use_cut);
    let after = project_at(root, &changed, filter, CAP);
    prop_assert!(
        before["partialFiles"].as_array().unwrap().is_empty(),
        "healthy checkout was partial: {}",
        before["partialFiles"]
    );
    prop_assert_eq!(
        after["partialFiles"].as_array().unwrap().len(),
        1,
        "unexpected partial files: {}",
        after["partialFiles"]
    );
    for data in [&before, &after] {
        for role in ["production", "tests"] {
            for edge in data[role].as_array().unwrap() {
                prop_assert_eq!(
                    edge["omittedSites"].as_u64(),
                    Some(0),
                    "site sample truncated: {}",
                    edge
                );
            }
        }
    }
    prop_assert!(
        after["partialFiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["path"] == path && p["reason"] == reason),
        "expected {path} {reason}, got {}",
        after["partialFiles"]
    );
    let original = resolved_sites(&before, path);
    let observed = resolved_sites(&after, path);
    let gained: Vec<_> = observed.difference(&original).collect();
    prop_assert!(
        gained.is_empty(),
        "path={path} mode={mode} use_cut={use_cut} filter={filter:?} gained={gained:?}\nhealthy={before}\ndegraded={after}"
    );
    Ok(())
}

#[test]
fn competing_globs_have_a_live_unique_provider_control() {
    for alias in [false, true] {
        let files = glob_files(alias, false);
        let before = project_with(&files, &[], CAP);
        assert!(
            !contains_site(&before, "src/user.rs", "src/p.rs"),
            "{before}"
        );
        let mut unique = files.clone();
        put(&mut unique, "src/q.rs", "pub fn other() {}\n");
        let only_one = project_with(&unique, &[], CAP);
        assert!(
            contains_site(&only_one, "src/user.rs", "src/p.rs"),
            "{only_one}"
        );
        for mode in 0..4 {
            check(&files, "src/q.rs", mode, alias, &[]).unwrap();
        }
    }
}

#[test]
fn lost_module_owner_witness() {
    let files = owner_files(false, false);
    let before = project_with(&files, &[], CAP);
    assert!(
        !contains_site(&before, "src/x/z.rs", "src/y.rs"),
        "{before}"
    );
    let mut unique = files.clone();
    put(&mut unique, "src/main.rs", "fn main() {}\n");
    let only_one = project_with(&unique, &[], CAP);
    assert!(
        contains_site(&only_one, "src/x/z.rs", "src/y.rs"),
        "{only_one}"
    );
    let mut broken = files.clone();
    put(
        &mut broken,
        "src/main.rs",
        "(mod x; mod y { pub fn f() {} } fn main() {}\n",
    );
    let after = project_with(&broken, &[], CAP);
    assert!(
        after["partialFiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["path"] == "src/main.rs" && p["reason"] == "syntax_error")
    );
    assert!(!contains_site(&after, "src/x/z.rs", "src/y.rs"), "{after}");
}

#[test]
fn partial_reason_layout_matrix() {
    for nested in [false, true] {
        for filtered in [false, true] {
            for variant in 0..4 {
                let owner = variant >= 2;
                let files = if owner {
                    owner_files(variant == 3, nested)
                } else {
                    glob_files(variant == 1, nested)
                };
                let path = if owner { "src/main.rs" } else { "src/q.rs" };
                let consumer = if owner { "src/x/z.rs" } else { "src/user.rs" };
                let filter = if filtered {
                    vec![consumer.to_owned()]
                } else {
                    vec![]
                };
                for mode in 0..4 {
                    check(&files, path, mode, variant == 1, &filter).unwrap();
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_RUST_ABSENCE_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(24), ..ProptestConfig::default() })]
    #[test]
    fn rust_partial_sources_do_not_add_unchanged_sites(alias in any::<bool>(), nested in any::<bool>(), mode in 0u8..4, filtered in any::<bool>(), owner in any::<bool>(), custom_path in any::<bool>(), noise in 0u8..5, reverse in any::<bool>(), history in proptest::collection::vec(0u8..4, 0..4)) {
        let mut files = if owner { owner_files(custom_path, nested) } else { glob_files(alias, nested) };
        for i in 0..noise {
            put(&mut files, &format!("extra{i}.rs"), format!("pub fn unrelated_{i}() {{}}\n"));
        }
        if reverse { files.reverse(); }
        let path = if owner { "src/main.rs" } else { "src/q.rs" };
        let consumer = if owner { "src/x/z.rs" } else { "src/user.rs" };
        let filter = if filtered { vec![consumer.to_owned()] } else { vec![] };
        let root = tempfile::tempdir().unwrap();
        check_at(root.path(), &files, path, mode, !owner && alias, &filter)?;
        // Restore and degrade the same checkout repeatedly. Each observation is
        // a fresh full in-process index, not an incremental publication.
        for previous in history {
            check_at(root.path(), &files, path, previous, !owner && alias, &filter)?;
        }
    }
}
