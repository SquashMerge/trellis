fn test_pin(revision: u64) -> trellis::model::IndexPin {
    trellis::model::IndexPin {
        index_generation: uuid::Uuid::from_u128(0x00000000000040008000000000000001),
        index_revision: revision,
    }
}
use std::{fs, path::Path, sync::atomic::AtomicBool};
use tempfile::TempDir;
use trellis::dependencies::{Catalog, CatalogOptions};
fn put(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn setup() -> (TempDir, TempDir, CatalogOptions) {
    let workspace = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    put(
        workspace.path(),
        "Cargo.toml",
        "[package]\nname='app'\nversion='1.0.0'\n[dependencies]\nrenamed={package='dep',version='1'}\n",
    );
    put(
        workspace.path(),
        "Cargo.lock",
        "version=4\n[[package]]\nname='app'\nversion='1.0.0'\n[[package]]\nname='dep'\nversion='1.2.3'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n",
    );
    cached(home.path(), "cache", "dep", "1.2.3");
    let options = CatalogOptions {
        cargo_home: Some(home.path().into()),
        rust_library: None,
    };
    (workspace, home, options)
}
fn cached(home: &Path, cache: &str, name: &str, version: &str) {
    let base = format!("registry/src/{cache}/{name}-{version}");
    put(
        home,
        &format!("{base}/Cargo.toml"),
        &format!("[package]\nname='{name}'\nversion='{version}'\n"),
    );
    put(
        home,
        &format!("{base}/src/lib.rs"),
        "pub struct Widget; impl Widget { pub fn make() -> Self { secret(); Widget } } fn secret() {}\n",
    );
}
fn build(root: &Path, options: &CatalogOptions) -> Catalog {
    Catalog::build(root, test_pin(7), options, &AtomicBool::new(false)).unwrap()
}
#[test]
fn exact_locked_candidates_aliases_and_separate_sources() {
    let (ws, home, options) = setup();
    cached(home.path(), "cache", "unrelated", "9.0.0");
    cached(home.path(), "cache", "dep", "2.0.0");
    let c = build(ws.path(), &options);
    assert_eq!(c.packages.len(), 1);
    let p = &c.packages[0];
    assert_eq!(p.name, "dep");
    assert_eq!(p.aliases, ["renamed"]);
    assert_eq!(p.source_state, "present");
    assert_eq!(p.index_state, "complete");
    assert!(p.warnings.iter().any(|w| w.contains("not verified")));
    assert!(c.symbols.iter().any(|s| s.qualified_name == "dep::Widget"));
    let symbol = c.symbols.iter().find(|s| s.name == "Widget").unwrap();
    let source = &c.sources[&symbol.source_ref];
    assert_eq!(source.hash.len(), 64);
    assert_eq!(source.package_id, p.id);
    assert!(
        source
            .directory
            .read_file(&source.path)
            .unwrap()
            .contains("Widget")
    );
    let json = serde_json::to_string(&c).unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&json)
            .unwrap()
            .get("sources")
            .is_none()
    );
    assert!(!json.contains(home.path().to_str().unwrap()));
}
#[test]
fn duplicate_registry_candidates_are_ambiguous() {
    let (ws, home, options) = setup();
    cached(home.path(), "other", "dep", "1.2.3");
    let c = build(ws.path(), &options);
    assert_eq!(c.packages[0].source_state, "ambiguous");
    assert!(c.symbols.is_empty());
}
#[test]
fn missing_lock_does_not_scan_cache_or_guess_version() {
    let (ws, _, options) = setup();
    fs::remove_file(ws.path().join("Cargo.lock")).unwrap();
    let c = build(ws.path(), &options);
    assert_eq!(c.packages[0].version, "unresolved");
    assert!(c.symbols.is_empty());
}
#[test]
fn unsupported_registry_and_config_fail_closed() {
    let (ws, _, options) = setup();
    put(
        ws.path(),
        ".cargo/config.toml",
        "[source.crates-io]\nreplace-with='custom'\n",
    );
    let c = build(ws.path(), &options);
    assert_eq!(c.packages[0].source_state, "blocked");
    assert!(c.symbols.is_empty());
    fs::remove_file(ws.path().join(".cargo/config.toml")).unwrap();
    put(
        ws.path(),
        "Cargo.lock",
        "[[package]]\nname='dep'\nversion='1.2.3'\nsource='git+https://example.test/dep'\n",
    );
    assert_eq!(
        build(ws.path(), &options).packages[0].source_state,
        "blocked"
    );
}
#[test]
fn workspace_paths_allowed_but_parent_paths_blocked() {
    let (ws, _, options) = setup();
    put(
        ws.path(),
        "Cargo.toml",
        "[package]\nname='app'\nversion='1'\n[dependencies]\nlocal={path='vendor/local'}\noutside={path='../outside'}\n",
    );
    put(
        ws.path(),
        "vendor/local/Cargo.toml",
        "[package]\nname='local'\nversion='0.1.0'\n",
    );
    put(ws.path(), "vendor/local/src/lib.rs", "pub struct Local;\n");
    let c = build(ws.path(), &options);
    assert!(c.symbols.iter().any(|s| s.qualified_name == "local::Local"));
    assert_eq!(
        c.packages
            .iter()
            .find(|p| p.name == "outside")
            .unwrap()
            .source_state,
        "blocked"
    );
}
#[test]
fn standard_library_prioritized_and_unavailable_explicit() {
    let (ws, _, mut options) = setup();
    let rust = TempDir::new().unwrap();
    put(
        rust.path(),
        "std/src/lib.rs",
        "pub mod fs {pub struct OpenOptions;}\n",
    );
    options.rust_library = Some(rust.path().into());
    let c = build(ws.path(), &options);
    assert_eq!(c.packages[0].name, "std");
    assert_eq!(c.packages[0].source, "stdlib");
    assert!(
        c.symbols
            .iter()
            .any(|s| s.qualified_name == "std::fs::OpenOptions")
    );
    assert_eq!(
        c.packages
            .iter()
            .find(|p| p.name == "core")
            .unwrap()
            .source_state,
        "missing"
    );
}
#[test]
fn source_hash_and_catalog_identity_change_with_bytes_and_revision() {
    let (ws, home, options) = setup();
    let a = build(ws.path(), &options);
    let b = build(ws.path(), &options);
    assert_eq!(a.id, b.id);
    put(
        home.path(),
        "registry/src/cache/dep-1.2.3/src/lib.rs",
        "pub struct Changed;\n",
    );
    let c = build(ws.path(), &options);
    assert_ne!(a.id, c.id);
    assert_ne!(a.sources.keys().next(), c.sources.keys().next());
    let d = Catalog::build(ws.path(), test_pin(8), &options, &AtomicBool::new(false)).unwrap();
    assert_ne!(c.id, d.id);
}
#[test]
fn oversized_and_excluded_sources_are_honest() {
    let (ws, home, options) = setup();
    put(
        home.path(),
        "registry/src/cache/dep-1.2.3/src/large.rs",
        &"x".repeat(2 * 1024 * 1024 + 1),
    );
    put(
        home.path(),
        "registry/src/cache/dep-1.2.3/src/tests/ignored.rs",
        "pub struct Ignored;",
    );
    let c = build(ws.path(), &options);
    assert_eq!(c.packages[0].index_state, "partial");
    assert!(!c.symbols.iter().any(|s| s.name == "Ignored"));
    assert!(c.warnings.iter().any(|w| w.contains("Partial")));
}
#[test]
fn cancellation_stops_build() {
    let (ws, _, options) = setup();
    assert!(
        Catalog::build(ws.path(), test_pin(7), &options, &AtomicBool::new(true))
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}
#[cfg(unix)]
#[test]
fn symlink_package_manifest_and_sources_never_followed() {
    use std::os::unix::fs::symlink;
    let (ws, home, options) = setup();
    let outside = TempDir::new().unwrap();
    put(outside.path(), "secret.rs", "pub struct Leaked;");
    symlink(
        outside.path().join("secret.rs"),
        home.path().join("registry/src/cache/dep-1.2.3/src/link.rs"),
    )
    .unwrap();
    let c = build(ws.path(), &options);
    assert!(!c.symbols.iter().any(|s| s.name == "Leaked"));
    assert_eq!(c.packages[0].index_state, "partial");
    fs::remove_file(home.path().join("registry/src/cache/dep-1.2.3/Cargo.toml")).unwrap();
    put(
        outside.path(),
        "Cargo.toml",
        "[package]\nname='dep'\nversion='1.2.3'\n",
    );
    symlink(
        outside.path().join("Cargo.toml"),
        home.path().join("registry/src/cache/dep-1.2.3/Cargo.toml"),
    )
    .unwrap();
    assert!(build(ws.path(), &options).symbols.is_empty());
}
#[cfg(unix)]
#[test]
fn source_capability_pins_original_root_after_root_replacement() {
    let (ws, home, options) = setup();
    let c = build(ws.path(), &options);
    let source = c.sources.values().next().unwrap();
    let moved = home.path().with_extension("moved");
    fs::rename(home.path(), &moved).unwrap();
    fs::create_dir(home.path()).unwrap();
    put(home.path(), &source.path, "pub struct Attacker;");
    assert!(
        source
            .directory
            .read_file(&source.path)
            .unwrap()
            .contains("Widget")
    );
    fs::remove_dir_all(moved).unwrap();
}
#[test]
fn per_package_file_bound_is_partial() {
    let (ws, home, options) = setup();
    for n in 0..501 {
        put(
            home.path(),
            &format!("registry/src/cache/dep-1.2.3/src/f{n:04}.rs"),
            &format!("pub struct S{n};"),
        );
    }
    let c = build(ws.path(), &options);
    assert_eq!(c.packages[0].index_state, "partial");
    assert!(c.sources.len() <= 500);
}

#[test]
fn lock_paths_cannot_select_arbitrary_nested_cache_files() {
    let (ws, _, options) = setup();
    put(
        ws.path(),
        "Cargo.lock",
        "[[package]]\nname='nested/dep'\nversion='1.2.3'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n",
    );
    let c = build(ws.path(), &options);
    assert_eq!(
        c.packages
            .iter()
            .find(|p| p.name == "nested/dep")
            .unwrap()
            .source_state,
        "blocked"
    );
    assert!(c.symbols.is_empty());
}
#[test]
fn absent_manifest_returns_explicit_unsupported_catalog() {
    let ws = TempDir::new().unwrap();
    let c = build(ws.path(), &CatalogOptions::default());
    assert!(c.packages.is_empty());
    assert!(c.warnings.iter().any(|w| w.contains("No Cargo.toml")));
}
#[test]
fn package_discovery_bound_is_explicit() {
    let (ws, _, options) = setup();
    let mut lock = String::from("version=4\n");
    for n in 0..1030 {
        lock.push_str(&format!("[[package]]\nname='missing{n}'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n"));
    }
    put(ws.path(), "Cargo.lock", &lock);
    let c = build(ws.path(), &options);
    assert_eq!(c.packages.len(), 1024);
    assert!(
        c.warnings
            .iter()
            .any(|w| w.contains("package discovery limit"))
    );
}
#[test]
#[ignore = "explicit local-source smoke; uses only configured roots"]
fn local_catalog_smoke() {
    let workspace = std::env::var_os("TRELLIS_CATALOG_WORKSPACE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let options = CatalogOptions {
        cargo_home: std::env::var_os("CARGO_HOME").map(Into::into),
        rust_library: std::env::var_os("TRELLIS_RUST_LIBRARY").map(Into::into),
    };
    let c = build(&workspace, &options);
    println!(
        "packages={} symbols={} sources={} present={} partial={} warnings={:?}",
        c.packages.len(),
        c.symbols.len(),
        c.sources.len(),
        c.packages
            .iter()
            .filter(|p| p.source_state == "present")
            .count(),
        c.packages
            .iter()
            .filter(|p| p.index_state == "partial")
            .count(),
        c.warnings
    );
    println!(
        "packagesWithSymbols={}",
        c.packages
            .iter()
            .filter(|p| c.symbols.iter().any(|s| s.package_id == p.id))
            .count()
    );
    println!(
        "OpenOptions={}",
        c.symbols
            .iter()
            .any(|s| s.qualified_name == "std::fs::OpenOptions")
    );
    assert!(c.packages.len() <= 1024);
    assert!(c.symbols.len() <= 50_000);
    assert!(c.sources.len() <= 2000);
}

#[test]
fn metadata_byte_budget_does_not_select_unsearched_cache_match() {
    let (ws, home, options) = setup();
    // Wrong package manifests still consume secure metadata reads and bytes.
    let manifest = format!(
        "[package]\nname='wrong'\nversion='1.2.3'\n#{}",
        "x".repeat(2 * 1024 * 1024 - 128)
    );
    for n in 0..9 {
        put(
            home.path(),
            &format!("registry/src/a{n}/dep-1.2.3/Cargo.toml"),
            &manifest,
        );
    }
    let c = build(ws.path(), &options);
    let p = c.packages.iter().find(|p| p.name == "dep").unwrap();
    assert_eq!(p.source_state, "ambiguous");
    assert!(c.symbols.is_empty());
    assert!(
        c.warnings
            .iter()
            .any(|w| w.contains("metadata discovery budget"))
    );
}
#[test]
fn identity_and_alias_amplification_is_bounded() {
    let (ws, _, options) = setup();
    let mut manifest = String::from("[package]\nname='app'\nversion='1.0.0'\n[dependencies]\n");
    for n in 0..100 {
        manifest.push_str(&format!("alias{n:03}={{package='dep',version='1'}}\n"));
    }
    manifest.push_str(&format!("{}='1'\n", "x".repeat(129)));
    put(ws.path(), "Cargo.toml", &manifest);
    let c = build(ws.path(), &options);
    assert_eq!(c.packages[0].aliases.len(), 64);
    assert!(
        c.packages[0]
            .warnings
            .iter()
            .any(|w| w.contains("alias limit"))
    );
    assert!(c.warnings.iter().any(|w| w.contains("text bound")));
    put(
        ws.path(),
        "Cargo.lock",
        &format!(
            "[[package]]\nname='{}'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n",
            "y".repeat(129)
        ),
    );
    let c = build(ws.path(), &options);
    assert!(c.packages.iter().all(|p| p.name.len() <= 128));
    assert!(c.warnings.iter().any(|w| w.contains("identity text bound")));
}

#[test]
fn per_package_symbol_bound_prioritizes_types_and_continues_other_packages() {
    let (ws, home, options) = setup();
    let mut early = String::new();
    for n in 0..1500 {
        early.push_str(&format!("pub fn early{n}() {{}}\n"));
    }
    let mut final_file = String::new();
    for n in 0..1000 {
        final_file.push_str(&format!("pub fn late{n}() {{}}\n"));
    }
    for n in 0..500 {
        final_file.push_str(&format!("pub struct LastType{n};\n"));
    }
    put(home.path(), "registry/src/cache/dep-1.2.3/src/a.rs", &early);
    put(
        home.path(),
        "registry/src/cache/dep-1.2.3/src/z.rs",
        &final_file,
    );
    cached(home.path(), "cache", "later", "1.0.0");
    let mut lock = fs::read_to_string(ws.path().join("Cargo.lock")).unwrap();
    lock.push_str("\n[[package]]\nname='later'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n");
    put(ws.path(), "Cargo.lock", &lock);
    let c = build(ws.path(), &options);
    let dep = c.packages.iter().find(|p| p.name == "dep").unwrap();
    assert_eq!(
        c.symbols.iter().filter(|s| s.package_id == dep.id).count(),
        2500
    );
    assert_eq!(dep.index_state, "partial");
    assert!(
        dep.warnings
            .iter()
            .any(|w| w.contains("package declaration limit"))
    );
    assert_eq!(
        c.symbols
            .iter()
            .filter(|s| s.name.starts_with("LastType"))
            .count(),
        500
    );
    let later = c.packages.iter().find(|p| p.name == "later").unwrap();
    assert!(c.symbols.iter().any(|s| s.package_id == later.id));
}
