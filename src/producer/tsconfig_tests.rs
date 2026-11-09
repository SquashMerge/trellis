//! #799: a root `tsconfig.json` package `extends` is supplied from the checkout's
//! `node_modules`, replaced by the generated tsconfig when the package is not there, and a
//! typed refusal otherwise.
use super::profiles::{self, Generated, Rules, Target};
use super::settings::{Source, TsProject};
use super::tsconfig::{self, CONFIG_UNRESOLVED, MAX_CHAIN_FILES, MAX_FILE_BYTES};
use super::worktree::{self, RetainedInputs, RetainedSource, RootInput};
use super::{ProducerName, TrustProfile};
use anyhow::Result;
use serde_json::{Value, json};
use std::{fs, path::Path, sync::atomic::AtomicBool};

const KY_ROOT: &str = r#"{
  // JSONC, as ky's is.
  "extends": "@sindresorhus/tsconfig",
  "compilerOptions": {"outDir": "distribution",},
  "include": ["source"],
}"#;

fn ts_rules() -> Rules {
    profiles::rules(
        &profiles::row(
            Target::Real(ProducerName::ScipTypescript),
            TrustProfile::Restricted,
        )
        .unwrap(),
    )
}

fn inputs(root: &str) -> RetainedInputs {
    RetainedInputs {
        config: vec![RootInput {
            path: "tsconfig.json".into(),
            bytes: root.as_bytes().to_vec(),
        }],
        dependencies: vec![],
    }
}

fn write(workspace: &Path, path: &str, bytes: &[u8]) {
    let path = workspace.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn apply(root: &str, workspace: &Path) -> Result<Rules> {
    let mut rules = ts_rules();
    tsconfig::apply(&mut rules, &inputs(root), workspace, None)?;
    Ok(rules)
}

fn generated<'a>(rules: &'a Rules, path: &str) -> &'a Generated {
    rules
        .generated
        .iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("no {path}: {:?}", rules.generated))
}

fn json_of(file: &Generated) -> Value {
    serde_json::from_slice(&file.bytes).unwrap()
}

fn refusal(root: &str, workspace: &Path) -> String {
    let error = apply(root, workspace).unwrap_err().to_string();
    assert!(error.starts_with(CONFIG_UNRESOLVED), "{error}");
    error
}

#[test]
fn package_in_node_modules_is_supplied_and_root_extends_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let base = br#"{"compilerOptions": {"strict": true, /* c */ "target": "es2022"}}"#;
    write(
        dir.path(),
        "node_modules/@sindresorhus/tsconfig/tsconfig.json",
        base,
    );
    write(
        dir.path(),
        "node_modules/@sindresorhus/tsconfig/package.json",
        br#"{"name":"@sindresorhus/tsconfig","exports":"./tsconfig.json"}"#,
    );
    let rules = apply(KY_ROOT, dir.path()).unwrap();

    assert_eq!(rules.omit.len(), 1);
    assert_eq!(rules.omit[0].path, "tsconfig.json");
    assert!(rules.omit[0].reason.contains("\"@sindresorhus/tsconfig\""));
    let root = generated(&rules, "worktree/tsconfig.json");
    assert!(!root.default_only, "the rewritten root is not a default");
    let root = json_of(root);
    assert_eq!(
        root["extends"],
        "./node_modules/@sindresorhus/tsconfig/tsconfig.json"
    );
    assert_eq!(root["include"], json!(["source"]));
    assert_eq!(root["compilerOptions"]["outDir"], "distribution");
    // A base with no package `extends` is supplied byte for byte.
    assert_eq!(
        generated(
            &rules,
            "worktree/node_modules/@sindresorhus/tsconfig/tsconfig.json"
        )
        .bytes,
        base
    );
    assert_eq!(rules.generated.len(), 2, "{:?}", rules.generated);
}

#[test]
fn chains_follow_relative_and_nested_package_extends() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "node_modules/a/package.json",
        br#"{"name":"a","tsconfig":"configs/base.json"}"#,
    );
    write(
        dir.path(),
        "node_modules/a/configs/base.json",
        br#"{"extends": ["./strict", "b/tsconfig.json"]}"#,
    );
    write(dir.path(), "node_modules/a/configs/strict.json", b"{}");
    // `b` is found in the root node_modules from inside `a`.
    write(
        dir.path(),
        "node_modules/b/tsconfig.json",
        b"{\"compilerOptions\":{}}",
    );
    let rules = apply(r#"{"extends": ["a", "./local.json"]}"#, dir.path()).unwrap();

    let root = json_of(generated(&rules, "worktree/tsconfig.json"));
    assert_eq!(
        root["extends"],
        json!(["./node_modules/a/configs/base.json", "./local.json"])
    );
    let base = json_of(generated(
        &rules,
        "worktree/node_modules/a/configs/base.json",
    ));
    assert_eq!(
        base["extends"],
        json!(["./strict", "../../b/tsconfig.json"])
    );
    generated(&rules, "worktree/node_modules/a/configs/strict.json");
    generated(&rules, "worktree/node_modules/b/tsconfig.json");
}

#[test]
fn absent_package_falls_back_to_the_generated_tsconfig_and_records_it() {
    let dir = tempfile::tempdir().unwrap();
    for setup in [
        // No node_modules at all.
        None,
        // The package directory holds no config.
        Some(("node_modules/@sindresorhus/tsconfig/readme.md", &b"x"[..])),
    ] {
        if let Some((path, bytes)) = setup {
            write(dir.path(), path, bytes);
        }
        let rules = apply(KY_ROOT, dir.path()).unwrap();
        assert_eq!(rules.omit.len(), 1);
        assert_eq!(rules.omit[0].path, "tsconfig.json");
        assert!(
            rules.omit[0]
                .reason
                .contains("the generated tsconfig.json replaces it"),
            "{}",
            rules.omit[0].reason
        );
        assert!(rules.omit[0].reason.contains("@sindresorhus/tsconfig"));
        // The default-only generated config is kept, and nothing else is added.
        assert_eq!(rules.generated, ts_rules().generated);
    }
}

#[test]
fn fallback_layout_lists_the_omission_and_the_generated_default() {
    let dir = tempfile::tempdir().unwrap();
    let rules = apply(KY_ROOT, dir.path()).unwrap();
    let layout = worktree::layout(&inputs(KY_ROOT), &rules);
    assert!(layout.config.is_empty(), "{:?}", layout.config);
    assert_eq!(layout.omitted.len(), 1);
    assert_eq!(layout.omitted[0].0, "tsconfig.json");
    assert_eq!(layout.generated.len(), 1);
    assert_eq!(layout.generated[0].path, "worktree/tsconfig.json");
}

/// A retained revision holding only the root config.
struct Retained(String);

impl RetainedSource for Retained {
    fn each_document(&self, visit: &mut dyn FnMut(&str, Vec<u8>) -> Result<()>) -> Result<()> {
        visit("source/index.ts", b"export const a = 1;\n".to_vec())
    }

    fn components(&self) -> Result<(Vec<Value>, Vec<Value>)> {
        let bytes = self.0.as_bytes();
        Ok((
            vec![
                json!({"path": "tsconfig.json", "type": "present", "bytes": hex::encode(bytes), "byteLength": bytes.len()}),
            ],
            vec![],
        ))
    }
}

#[test]
fn materialized_worktree_holds_the_supplied_or_generated_config() {
    let workspace = tempfile::tempdir().unwrap();
    let source = Retained(KY_ROOT.to_owned());

    // Absent: the generated default is the worktree's tsconfig.json.
    let rules = apply(KY_ROOT, workspace.path()).unwrap();
    let run = tempfile::tempdir().unwrap();
    worktree::materialize(&source, &rules, run.path(), &AtomicBool::new(false)).unwrap();
    assert_eq!(
        fs::read(run.path().join("worktree/tsconfig.json")).unwrap(),
        profiles::SCIP_TYPESCRIPT_TSCONFIG
    );

    // Present: the rewritten root and the package's config.
    write(
        workspace.path(),
        "node_modules/@sindresorhus/tsconfig/tsconfig.json",
        b"{}",
    );
    let rules = apply(KY_ROOT, workspace.path()).unwrap();
    let run = tempfile::tempdir().unwrap();
    worktree::materialize(&source, &rules, run.path(), &AtomicBool::new(false)).unwrap();
    let root: Value =
        serde_json::from_slice(&fs::read(run.path().join("worktree/tsconfig.json")).unwrap())
            .unwrap();
    assert_eq!(
        root["extends"],
        "./node_modules/@sindresorhus/tsconfig/tsconfig.json"
    );
    assert_eq!(
        fs::read(
            run.path()
                .join("worktree/node_modules/@sindresorhus/tsconfig/tsconfig.json")
        )
        .unwrap(),
        b"{}"
    );
    assert!(run.path().join("worktree/source/index.ts").is_file());
}

#[test]
fn unusable_extends_is_a_typed_refusal_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let error = refusal(r#"{"extends": 7}"#, dir.path());
    assert!(error.contains("extends 7"), "{error}");
    assert!(error.contains("not a string or an array"), "{error}");

    write(dir.path(), "node_modules/bad-json/tsconfig.json", b"{ nope");
    let error = refusal(r#"{"extends": "bad-json"}"#, dir.path());
    assert!(error.contains("extends \"bad-json\""), "{error}");
    assert!(error.contains("is not a JSON object"), "{error}");

    write(
        dir.path(),
        "node_modules/cycle/tsconfig.json",
        br#"{"extends":"./tsconfig.json"}"#,
    );
    assert!(refusal(r#"{"extends": "cycle"}"#, dir.path()).contains("extends itself"));

    write(
        dir.path(),
        "node_modules/escape/tsconfig.json",
        br#"{"extends":"../../tsconfig.json"}"#,
    );
    assert!(refusal(r#"{"extends": "escape"}"#, dir.path()).contains("leaves the checkout's"));

    write(
        dir.path(),
        "node_modules/exported/package.json",
        br#"{"exports":"../elsewhere.json"}"#,
    );
    assert!(refusal(r#"{"extends": "exported"}"#, dir.path()).contains("leaves"));

    let large = vec![b' '; MAX_FILE_BYTES as usize + 1];
    write(dir.path(), "node_modules/large/tsconfig.json", &large);
    assert!(refusal(r#"{"extends": "large"}"#, dir.path()).contains("larger than"));

    assert!(refusal(r#"{"extends": "@scope"}"#, dir.path()).contains("not a valid package"));
}

#[test]
fn a_chain_over_the_file_bound_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for at in 0..=MAX_CHAIN_FILES {
        let next = format!(r#"{{"extends":"./c{}.json"}}"#, at + 1);
        write(
            dir.path(),
            &format!("node_modules/deep/c{at}.json"),
            next.as_bytes(),
        );
    }
    write(
        dir.path(),
        "node_modules/deep/package.json",
        br#"{"tsconfig":"c0.json"}"#,
    );
    let error = refusal(r#"{"extends": "deep"}"#, dir.path());
    assert!(
        error.contains(&format!("more than {MAX_CHAIN_FILES} files")),
        "{error}"
    );
}

#[test]
fn roots_without_a_package_extends_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    for root in [
        r#"{"compilerOptions": {}}"#,
        r#"{"extends": "./base.json"}"#,
        r#"{"extends": ["../outside.json", "/abs.json"]}"#,
    ] {
        let rules = apply(root, dir.path()).unwrap();
        assert_eq!(rules, ts_rules(), "{root}");
    }
    // No root tsconfig.json: nothing to do.
    let mut rules = ts_rules();
    tsconfig::apply(&mut rules, &RetainedInputs::default(), dir.path(), None).unwrap();
    assert_eq!(rules, ts_rules());
}

/// #808 review: conditions are matched in the manifest's order, as TypeScript 5.9.3 matches
/// them (`types` first here gives `strict: true`), not in sorted key order, and a condition
/// whose target is missing passes to the next.
#[test]
fn conditional_exports_keep_the_manifest_order() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "node_modules/base/package.json",
        br#"{"exports":{"types":"./types.json","default":"./default.json"}}"#,
    );
    write(
        dir.path(),
        "node_modules/base/types.json",
        br#"{"compilerOptions":{"strict":true}}"#,
    );
    write(
        dir.path(),
        "node_modules/base/default.json",
        br#"{"compilerOptions":{"strict":false}}"#,
    );
    let rules = apply(r#"{"extends":"base"}"#, dir.path()).unwrap();
    assert_eq!(
        json_of(generated(&rules, "worktree/tsconfig.json"))["extends"],
        "./node_modules/base/types.json"
    );

    // `import` is not a condition of a config lookup; `default` after it still matches.
    write(
        dir.path(),
        "node_modules/base/package.json",
        br#"{"exports":{".":{"import":"./types.json","default":"./default.json"}}}"#,
    );
    let rules = apply(r#"{"extends":"base"}"#, dir.path()).unwrap();
    assert_eq!(
        json_of(generated(&rules, "worktree/tsconfig.json"))["extends"],
        "./node_modules/base/default.json"
    );

    // A matching condition whose file is missing passes to the next one.
    write(
        dir.path(),
        "node_modules/base/package.json",
        br#"{"exports":{"types":"./gone.json","default":"./default.json"}}"#,
    );
    let rules = apply(r#"{"extends":"base"}"#, dir.path()).unwrap();
    assert_eq!(
        json_of(generated(&rules, "worktree/tsconfig.json"))["extends"],
        "./node_modules/base/default.json"
    );
}

fn packages(dir: &Path, count: usize) -> String {
    let mut names = Vec::new();
    for i in 0..count {
        let name = format!("p{i}");
        write(
            dir,
            &format!("node_modules/{name}/package.json"),
            br#"{"tsconfig":"tsconfig.json"}"#,
        );
        write(dir, &format!("node_modules/{name}/tsconfig.json"), b"{}");
        names.push(name);
    }
    json!({ "extends": names }).to_string()
}

/// #808 review: manifests count toward the file bound. Eight packages read 16 files (eight
/// manifests and eight configs); nine read 18.
#[test]
fn the_file_bound_counts_manifests() {
    let dir = tempfile::tempdir().unwrap();
    let at_bound = packages(dir.path(), MAX_CHAIN_FILES / 2);
    let rules = apply(&at_bound, dir.path()).unwrap();
    assert_eq!(rules.generated.len(), 1 + MAX_CHAIN_FILES / 2);

    let over = packages(dir.path(), MAX_CHAIN_FILES / 2 + 1);
    let error = refusal(&over, dir.path());
    assert!(
        error.contains(&format!("reads more than {MAX_CHAIN_FILES} files")),
        "{error}"
    );
}

/// #808 review: an error other than absence is a typed refusal, never the fallback.
#[test]
fn a_permission_error_is_refused_not_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "node_modules/base/tsconfig.json", b"{}");
    let package = dir.path().join("node_modules/base");
    fs::set_permissions(&package, fs::Permissions::from_mode(0o000)).unwrap();
    let result = apply(r#"{"extends":"base"}"#, dir.path());
    fs::set_permissions(&package, fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err().to_string();
    assert!(error.starts_with(CONFIG_UNRESOLVED), "{error}");
    assert!(error.contains("extends \"base\""), "{error}");
    assert!(error.contains("ermission denied"), "{error}");
}

/// #808 review: a config path that is not a regular file is refused, never the fallback.
#[test]
fn a_non_regular_config_is_refused_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("node_modules/base/tsconfig.json")).unwrap();
    let error = refusal(r#"{"extends":"base"}"#, dir.path());
    assert!(error.contains("is not a regular file"), "{error}");

    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("node_modules/base/package.json")).unwrap();
    let error = refusal(r#"{"extends":"base"}"#, dir.path());
    assert!(
        error.contains("package.json is not a regular file"),
        "{error}"
    );
}

/// #808 review: a symlink whose real target is outside the checkout's `node_modules` is
/// refused; one inside it (as pnpm lays packages out) is followed.
#[test]
fn a_symlink_out_of_node_modules_is_refused() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "outside.json", b"{}");
    fs::create_dir_all(dir.path().join("node_modules/base")).unwrap();
    symlink(
        "../../outside.json",
        dir.path().join("node_modules/base/tsconfig.json"),
    )
    .unwrap();
    let error = refusal(r#"{"extends":"base"}"#, dir.path());
    assert!(
        error.contains("resolves outside the checkout's node_modules"),
        "{error}"
    );

    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "node_modules/.pnpm/base@1/node_modules/base/tsconfig.json",
        b"{}",
    );
    symlink(
        ".pnpm/base@1/node_modules/base",
        dir.path().join("node_modules/base"),
    )
    .unwrap();
    let rules = apply(r#"{"extends":"base"}"#, dir.path()).unwrap();
    assert_eq!(
        generated(&rules, "worktree/node_modules/base/tsconfig.json").bytes,
        b"{}"
    );
}

/// #808 review: as in TypeScript 5.9.3, a nearer package directory with no config passes
/// the lookup to the next `node_modules` outward.
#[test]
fn lookup_continues_past_a_package_with_no_config() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "node_modules/a/tsconfig.json",
        br#"{"extends":"base"}"#,
    );
    write(
        dir.path(),
        "node_modules/a/node_modules/base/package.json",
        br#"{"name":"base"}"#,
    );
    write(
        dir.path(),
        "node_modules/base/tsconfig.json",
        br#"{"compilerOptions":{"strict":true}}"#,
    );
    let rules = apply(r#"{"extends":"a"}"#, dir.path()).unwrap();
    assert!(!generated(&rules, "worktree/tsconfig.json").default_only);
    let a = json_of(generated(&rules, "worktree/node_modules/a/tsconfig.json"));
    assert_eq!(a["extends"], "../base/tsconfig.json");
    generated(&rules, "worktree/node_modules/base/tsconfig.json");

    // An inner package whose `exports` names no config stops the lookup, as TypeScript's does.
    write(
        dir.path(),
        "node_modules/a/node_modules/base/package.json",
        br#"{"exports":{"./other":"./other.json"}}"#,
    );
    let rules = apply(r#"{"extends":"a"}"#, dir.path()).unwrap();
    assert_eq!(rules.generated, ts_rules().generated, "fell back");
}

fn retained(files: &[(&str, &[u8])]) -> RetainedInputs {
    RetainedInputs {
        config: files
            .iter()
            .map(|(path, bytes)| RootInput {
                path: (*path).into(),
                bytes: bytes.to_vec(),
            })
            .collect(),
        dependencies: vec![],
    }
}

fn choose(
    files: &[(&str, &[u8])],
    project: Option<TsProject>,
) -> Result<(Rules, super::settings::Setting)> {
    let dir = tempfile::tempdir().unwrap();
    let mut rules = ts_rules();
    let setting = tsconfig::apply(&mut rules, &retained(files), dir.path(), project)?;
    Ok((rules, setting))
}

/// #717: an unparseable root config is the typed refusal, not left to TypeScript.
#[test]
fn an_unparseable_root_config_is_refused() {
    let placeholder = crate::skipped_config::PLACEHOLDER;
    for (path, bytes, expected) in [
        (
            "tsconfig.json",
            &b"{ not json"[..],
            "tsconfig.json is not a JSON object",
        ),
        (
            "tsconfig.json",
            b"[1]",
            "tsconfig.json is not a JSON object",
        ),
        ("jsconfig.json", b"{,", "jsconfig.json is not a JSON object"),
        ("tsconfig.json", placeholder, "over the config input cap"),
    ] {
        let error = choose(&[(path, bytes)], None).unwrap_err().to_string();
        assert!(error.starts_with(CONFIG_UNRESOLVED), "{error}");
        assert!(error.contains(expected), "{expected}: {error}");
    }
    // The override sidesteps a root config it does not use.
    let (rules, setting) = choose(
        &[("tsconfig.json", b"{ not json")],
        Some(TsProject::Generated),
    )
    .unwrap();
    assert_eq!(setting.value, "generated");
    assert_eq!(rules.omit[0].path, "tsconfig.json");
}

/// #717: a root `jsconfig.json` with no `tsconfig.json` is the project, through a generated
/// tsconfig that extends it; a root `tsconfig.json` wins over it.
#[test]
fn a_root_jsconfig_is_the_project_unless_a_tsconfig_is_there() {
    let js: &[u8] = br#"{"compilerOptions": {"checkJs": true}}"#;
    let (rules, setting) = choose(&[("jsconfig.json", js)], None).unwrap();
    assert_eq!(setting.value, "jsconfig.json");
    assert_eq!(setting.source, Source::Project);
    let project = generated(&rules, "worktree/tsconfig.json");
    assert_eq!(project.bytes, tsconfig::JSCONFIG_PROJECT);
    assert!(!project.default_only);
    assert!(rules.omit.is_empty());
    // The worktree holds the checkout's jsconfig.json and the generated tsconfig.json.
    let layout = worktree::layout(&retained(&[("jsconfig.json", js)]), &rules);
    assert_eq!(layout.config[0].path, "jsconfig.json");
    assert_eq!(layout.generated[0].path, "worktree/tsconfig.json");

    let ts: &[u8] = br#"{"compilerOptions": {}}"#;
    let (rules, setting) = choose(&[("tsconfig.json", ts), ("jsconfig.json", js)], None).unwrap();
    assert_eq!(setting.value, "tsconfig.json");
    assert_eq!(rules, ts_rules());
}

/// #717: `--ts-project` replaces the chosen config, and naming an absent one is refused.
#[test]
fn the_project_override_wins() {
    let js: &[u8] = b"{}";
    let ts: &[u8] = b"{}";
    let both = [("tsconfig.json", ts), ("jsconfig.json", js)];
    let (rules, setting) = choose(&both, Some(TsProject::Jsconfig)).unwrap();
    assert_eq!(setting.value, "jsconfig.json");
    assert_eq!(setting.source, Source::Override);
    assert_eq!(rules.omit[0].path, "tsconfig.json");
    assert_eq!(
        generated(&rules, "worktree/tsconfig.json").bytes,
        tsconfig::JSCONFIG_PROJECT
    );
    let layout = worktree::layout(&retained(&both), &rules);
    assert_eq!(layout.omitted[0].0, "tsconfig.json");

    let (rules, setting) = choose(&both, Some(TsProject::Generated)).unwrap();
    assert_eq!(setting.value, "generated");
    let layout = worktree::layout(&retained(&both), &rules);
    assert_eq!(layout.generated[0].path, "worktree/tsconfig.json");
    assert_eq!(
        generated(&rules, "worktree/tsconfig.json").bytes,
        profiles::SCIP_TYPESCRIPT_TSCONFIG
    );

    let error = choose(&[("jsconfig.json", js)], Some(TsProject::Tsconfig))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with(CONFIG_UNRESOLVED), "{error}");
    assert!(error.contains("no root tsconfig.json"), "{error}");
}
