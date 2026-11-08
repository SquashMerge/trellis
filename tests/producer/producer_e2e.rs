//! Real scip-typescript (JavaScript mode) and scip-python through the restricted profile.
//! Each producer has a default variant that skips with `SKIP producer_unavailable: <reason>`
//! when it is not provisioned (CI), and a strict `#[ignore]` variant that fails instead and
//! prints an `E2E-EVIDENCE` line on pass. Nothing is installed.
#![cfg(feature = "test-support")]

use crate::real;
use crate::support;
use real::{Availability, Case};

const TYPESCRIPT: Case = Case {
    producer: "scip-typescript",
    fixture: "javascript",
    extension: "js",
    trust: "restricted",
};
const PYTHON: Case = Case {
    producer: "scip-python",
    fixture: "python",
    extension: "py",
    trust: "restricted",
};

#[test]
fn scip_typescript_javascript_restricted_end_to_end() {
    real::run_case(&TYPESCRIPT, Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_typescript_javascript_restricted_end_to_end_strict() {
    assert!(real::run_case(&TYPESCRIPT, Availability::Required).is_some());
}

/// The checkout's root `tsconfig.json` in a mixed run.
#[derive(Clone, Copy, Debug)]
enum RootTsconfig {
    /// The fixture's own.
    Own,
    /// Deleted, so the plan generates its own.
    Generated,
    /// `extends` an npm package (#799), present in the checkout's `node_modules` or not.
    PackageExtends { installed: bool },
}

/// The package's options and the root's own, split as an npm-shared config splits them.
const PACKAGE_BASE: &str =
    r#"{"compilerOptions":{"allowJs":true,"jsx":"preserve","target":"es2022"}}"#;
const EXTENDING_ROOT: &str = r#"{
  "extends": "@fixture/tsconfig",
  "compilerOptions": {"baseUrl": "src", "paths": {"@app/*": ["app/*"]}, "rootDirs": ["src", "generated"]},
}"#;

/// The mixed TypeScript and JavaScript fixture through one real scip-typescript run
/// (Decision 0020): the artifact covers both languages.
fn typescript_mixed(tsconfig: RootTsconfig, availability: Availability) -> Option<String> {
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/typescript-mixed");
    let checkout = support::start_checkout(
        |root| {
            real::copy_tree(&fixture, root);
            match tsconfig {
                RootTsconfig::Own => {}
                RootTsconfig::Generated => {
                    std::fs::remove_file(root.join("tsconfig.json")).unwrap();
                }
                RootTsconfig::PackageExtends { installed } => {
                    std::fs::write(root.join("tsconfig.json"), EXTENDING_ROOT).unwrap();
                    if installed {
                        let package = root.join("node_modules/@fixture/tsconfig");
                        std::fs::create_dir_all(&package).unwrap();
                        std::fs::write(package.join("tsconfig.json"), PACKAGE_BASE).unwrap();
                    }
                }
            }
        },
        &[],
    );
    let plan = availability.resolve(
        "scip-typescript",
        real::plan(&checkout, "scip-typescript", "restricted", &[]),
    )?;
    assert_eq!(
        plan["producer"]["languages"],
        serde_json::json!(["javascript", "typescript"])
    );
    let generated = plan["inputs"]["generatedConfig"].as_array().unwrap();
    let expected = match tsconfig {
        RootTsconfig::Own => 0,
        // The generated default, which also replaces a root whose package is absent.
        RootTsconfig::Generated | RootTsconfig::PackageExtends { installed: false } => 1,
        // The rewritten root and the package's config.
        RootTsconfig::PackageExtends { installed: true } => 2,
    };
    assert_eq!(generated.len(), expected, "{generated:?}");
    let replaced = matches!(tsconfig, RootTsconfig::PackageExtends { .. });
    assert_eq!(
        plan["inputs"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "tsconfig.json"),
        replaced,
        "{}",
        plan["inputs"]["omitted"]
    );
    // `ignored.jsx` is not indexed, so it is in neither count.
    let documents: usize = ["js", "mjs", "cjs", "ts", "tsx", "mts", "cts"]
        .iter()
        .map(|extension| real_documents(&fixture, extension))
        .sum();
    assert_eq!(plan["inputs"]["documents"]["requested"], documents);
    let hash = plan["planSha256"].as_str().unwrap().to_owned();
    let before = support::workspace_tree_digest(&checkout.root);
    let output = real::producer_command(
        &checkout,
        "run",
        "scip-typescript",
        &["--trust", "restricted", "--plan-sha256", &hash],
    )
    .output()
    .unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON on stdout; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(reply["outcome"], "imported", "{reply}");
    let report = &reply["import"]["report"];
    let artifacts = report["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1, "{report}");
    assert_eq!(
        artifacts[0]["languages"],
        serde_json::json!(["javascript", "typescript"])
    );
    assert!(artifacts[0]["claims"].as_u64().unwrap() > 0, "{report}");
    for language in ["javascript", "typescript"] {
        let row = report["languages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["language"] == language)
            .unwrap_or_else(|| panic!("no {language} report: {report}"));
        assert_eq!(row["failed"], 0, "{language}: {row}");
        assert!(row["complete"].as_u64().unwrap() > 0, "{language}: {row}");
    }
    assert_eq!(
        support::workspace_tree_digest(&checkout.root),
        before,
        "the run changed the workspace"
    );
    let line = format!(
        "E2E-EVIDENCE producer=scip-typescript language=typescript rootTsconfig={tsconfig:?} documents={documents} claims={}",
        artifacts[0]["claims"]
    );
    println!("{line}");
    Some(line)
}

fn real_documents(fixture: &std::path::Path, extension: &str) -> usize {
    fn walk(dir: &std::path::Path, extension: &str, count: &mut usize) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, extension, count);
            } else if path.extension().is_some_and(|e| e == extension) {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(fixture, extension, &mut count);
    count
}

#[test]
fn scip_typescript_typescript_restricted_end_to_end() {
    typescript_mixed(RootTsconfig::Own, Availability::SkipIfUnavailable);
    typescript_mixed(RootTsconfig::Generated, Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_typescript_typescript_restricted_end_to_end_strict() {
    assert!(typescript_mixed(RootTsconfig::Own, Availability::Required).is_some());
    assert!(typescript_mixed(RootTsconfig::Generated, Availability::Required).is_some());
}

/// #799: a root `tsconfig.json` that `extends` an npm package runs, whether the package is
/// supplied from the checkout's `node_modules` or the generated tsconfig replaces the root.
#[test]
fn scip_typescript_package_extends_restricted_end_to_end() {
    for installed in [true, false] {
        typescript_mixed(
            RootTsconfig::PackageExtends { installed },
            Availability::SkipIfUnavailable,
        );
    }
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_typescript_package_extends_restricted_end_to_end_strict() {
    for installed in [true, false] {
        assert!(
            typescript_mixed(
                RootTsconfig::PackageExtends { installed },
                Availability::Required
            )
            .is_some()
        );
    }
}

#[test]
fn scip_python_restricted_end_to_end() {
    real::run_case(&PYTHON, Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_python_restricted_end_to_end_strict() {
    assert!(real::run_case(&PYTHON, Availability::Required).is_some());
}

#[test]
fn unavailable_producer_is_typed_skip() {
    let checkout = support::start_checkout(
        |root| real::copy_tree(&real::fixture_dir("javascript"), root),
        &[],
    );
    let not_a_package = checkout.root.join("package.json");
    let forced = || {
        real::plan(
            &checkout,
            "scip-typescript",
            "restricted",
            &["--executable", not_a_package.to_str().unwrap()],
        )
    };
    let reason = forced().unwrap_err();
    assert!(
        reason.contains("not an npm package entry point"),
        "{reason}"
    );
    assert!(
        Availability::SkipIfUnavailable
            .resolve("scip-typescript", forced())
            .is_none()
    );
    let strict =
        std::panic::catch_unwind(|| Availability::Required.resolve("scip-typescript", forced()));
    assert!(strict.is_err(), "the strict variant must fail, not skip");
}

/// One line of 10,001 function declarations: the shape of a committed minified bundle, over
/// the 10,000-symbols-per-document cap under real scip-typescript.
fn minified_bundle() -> String {
    let mut bundle = String::new();
    for n in 0..10_001 {
        bundle.push_str(&format!("function b{n}(){{}}"));
    }
    bundle.push('\n');
    bundle
}

/// #634: a real scip-typescript artifact whose one generated bundle is over the per-document
/// symbol cap imports every other document; the bundle is reported partial with
/// `document_cap_exceeded` and no facts.
fn bundle_over_symbol_cap(availability: Availability) -> Option<()> {
    let checkout = support::start_checkout(
        |root| {
            real::copy_tree(&real::fixture_dir("javascript"), root);
            std::fs::create_dir(root.join("vendor")).unwrap();
            std::fs::write(root.join("vendor/bundle.js"), minified_bundle()).unwrap();
        },
        &[],
    );
    let plan = availability.resolve(
        "scip-typescript",
        real::plan(&checkout, "scip-typescript", "restricted", &[]),
    )?;
    let hash = plan["planSha256"].as_str().unwrap().to_owned();
    let output = real::producer_command(
        &checkout,
        "run",
        "scip-typescript",
        &["--trust", "restricted", "--plan-sha256", &hash],
    )
    .output()
    .unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON on stdout; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(reply["outcome"], "imported", "{reply}");
    let report = &reply["import"]["report"];
    let javascript = report["languages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|language| language["language"] == "javascript")
        .unwrap_or_else(|| panic!("no javascript report: {report}"));
    assert_eq!(
        (
            &javascript["complete"],
            &javascript["partial"],
            &javascript["failed"]
        ),
        (&2.into(), &1.into(), &0.into()),
        "{javascript}"
    );
    let reasons = &javascript["partialReasons"];
    assert_eq!(reasons["capped"], 1, "{reasons}");
    let listed = reasons["listed"].as_array().unwrap();
    let bundle: Vec<_> = listed
        .iter()
        .filter(|entry| entry["path"] == "vendor/bundle.js")
        .collect();
    assert_eq!(bundle.len(), 1, "{reasons}");
    assert_eq!(bundle[0]["diagnostic"], "document_cap_exceeded");
    assert_eq!(bundle[0]["reasons"]["capped"], true);
    assert!(
        listed
            .iter()
            .all(|entry| entry["path"] != "src/a.js" && entry["path"] != "src/b.js"),
        "the other documents import complete: {reasons}"
    );
    let claims: u64 = report["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|artifact| artifact["claims"].as_u64())
        .sum();
    assert!(claims > 0, "the other documents keep their facts: {report}");
    println!("E2E-EVIDENCE producer=scip-typescript bundle=document_cap_exceeded claims={claims}");
    Some(())
}

#[test]
fn scip_typescript_bundle_over_symbol_cap_imports_the_rest() {
    bundle_over_symbol_cap(Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_typescript_bundle_over_symbol_cap_imports_the_rest_strict() {
    assert!(bundle_over_symbol_cap(Availability::Required).is_some());
}

mod omitted_python_context {
    use super::{Availability, real};
    use serde_json::{Value, json};
    use std::{
        ffi::OsString,
        fs,
        os::unix::fs::DirBuilderExt,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        graph_query::{BoundaryReason, GraphQuery, GraphResult},
        indexer::{IndexOptions, index_workspace_bundle},
        model::{CancelFlag, IndexPin, SemanticStatus},
        producer::{TrustProfile, batch, exec, plan, profiles, state::StateDir, worktree},
        semantic_batch::{
            BatchCounters, BatchLimits, LookupBudget, StageInput, StageOwner,
            descriptor::{GenerationOrigin, GenerationTrust},
            stage_batch,
        },
        semantic_evidence::{Freshness, Target},
        semantic_import::{DestinationMutations, join_staged},
        store::Store,
    };

    fn publish_native(store: &Store, options: &IndexOptions, cancel: &CancelFlag) -> IndexPin {
        let expected = store.index_baseline().unwrap();
        let (graph, native, capture) =
            index_workspace_bundle(options, store.root_id(), cancel, |_| {}).unwrap();
        let leader = store.publication_for_tests().unwrap();
        store
            .publish_native(&graph, &capture, &native, &leader, expected, cancel)
            .unwrap()
    }

    fn graph(store: &Store, pin: IndexPin, root: &str) -> GraphResult {
        let response = store.evidence_response().unwrap();
        let answer = response
            .graph_query(
                &GraphQuery {
                    root_syntax_id: root.to_owned(),
                    semantic_producer_id: Some("scip-python".into()),
                    depth: 2,
                    max_nodes: 20,
                    max_calls: 20,
                    include_external: true,
                    include_tests: true,
                },
                Some(pin),
                &mut || Ok(()),
            )
            .unwrap();
        assert_eq!(answer.basis, pin);
        response.finish(answer.outcome.unwrap()).unwrap()
    }

    fn versions(store: &Store, pin: IndexPin) -> Vec<(String, i64)> {
        store
            .with_index_read_for_tests(|db| {
                let mut statement = db.prepare(
                    "SELECT path,vid FROM revision_documents WHERE revision_id=?1 AND language='python' ORDER BY path",
                )?;
                let rows = statement.query_map(
                    [format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision)],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap()
    }

    fn run(availability: Availability) -> Option<()> {
        let temp = tempfile::Builder::new()
            .prefix("python-omitted-context-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        real::copy_tree(&real::fixture_dir("python"), &root);
        let requirements = b"# supplied producer input at P\n";
        fs::write(root.join("requirements.txt"), requirements).unwrap();
        let store = Store::open_for_tests(&temp.path().join("state"), &root).unwrap();
        let options = IndexOptions::new(root.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let captured = publish_native(&store, &options, &cancel);

        let mut environment = serde_json::Map::new();
        for (name, value) in real::real_producer_env() {
            environment.insert(name, json!(value));
        }
        environment.insert("HOME".into(), json!(std::env::var("HOME").unwrap()));
        let request = plan::PlanRequest {
            version: 1,
            producer: "scip-python".into(),
            trust_profile: TrustProfile::Restricted,
            limits: plan::LimitsRequest {
                timeout_secs: plan::DEFAULT_TIMEOUT_SECS,
                memory_mib: plan::DEFAULT_MEMORY_MIB,
            },
            executable: None,
            environment: serde_json::from_value(Value::Object(environment)).unwrap(),
        };
        let prepared = match plan::prepare(&store, &request, None) {
            Ok(prepared) => prepared,
            Err(error) => {
                let error = format!("{error:#}");
                let (_, reason) = error
                    .split_once("producer_unavailable: ")
                    .unwrap_or_else(|| panic!("plan failed without typed unavailability: {error}"));
                availability.resolve("scip-python", Err(reason.to_owned()))?;
                unreachable!("an unavailable required producer must fail, not continue");
            }
        };
        let (roots, identity) = store.state_topology();
        let state = StateDir::open(roots, identity).unwrap();
        let run_id = "0123456789abcdef0123456789abcdef";
        let dir = fs::canonicalize(state.create_run_dir(run_id).unwrap()).unwrap();
        let run_path = dir.to_str().unwrap();
        let planned = prepared.build(&store, None, run_path);
        assert_eq!(planned["trustProfile"], "restricted");
        assert!(
            planned["inputs"]["omitted"]
                .as_array()
                .unwrap()
                .iter()
                .any(|input| input["path"] == "pyproject.toml")
        );
        let rules = profiles::rules(&prepared.row);
        store
            .with_native_reader(captured, |reader| {
                worktree::materialize(reader, &rules, &dir, &cancel)
            })
            .unwrap();
        assert!(!dir.join("worktree/pyproject.toml").exists());
        assert_eq!(
            fs::read(dir.join("worktree/requirements.txt")).unwrap(),
            requirements
        );
        assert_eq!(
            fs::read(dir.join("producer-config/environment.json")).unwrap(),
            b"[]"
        );
        for name in ["home", "tmp", "out", "logs"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(dir.join(name))
                .unwrap();
        }
        let command = profiles::command(
            &prepared.row,
            &prepared.resolution,
            &prepared.declared,
            None,
            &prepared.pin,
            run_path,
        );
        worktree::write_bin_links(&dir, &command.bin_links).unwrap();
        let outcome = exec::spawn_producer(
            &exec::ProducerSpec {
                argv: command.argv.iter().map(OsString::from).collect(),
                cwd: dir.join("worktree"),
                env: command.env,
                artifact: dir.join(batch::ARTIFACT_PATH),
                log_dir: Some(dir.join("logs")),
            },
            &plan::effective_limits(prepared.limits),
            &cancel,
            &mut exec::Hooks::default(),
        )
        .unwrap();
        assert_eq!(outcome.ending, exec::Ending::Exited, "{outcome:?}");
        assert_eq!(outcome.diagnostics.exit.code, Some(0), "{outcome:?}");
        let written = store
            .with_native_reader(captured, |reader| {
                batch::write_batch(
                    reader,
                    &batch::BatchSpec {
                        pin: &prepared.pin,
                        languages: &["python"],
                        workspace: &root,
                        dir: &dir,
                        generation: batch::run_generation(
                            &profiles::run_generation(&prepared.row, &prepared.resolution),
                            GenerationOrigin::TrellisRun,
                            run_id,
                        ),
                        snapshot: true,
                    },
                )
            })
            .unwrap();
        let limits = BatchLimits::PRODUCTION;
        let mut admitted = stage_batch(
            &store,
            &StageInput::generated(&root, GenerationOrigin::TrellisRun, run_id, &dir).unwrap(),
            StageOwner {
                request_id: "omitted-python-context",
                incarnation: "omitted-python-context",
            },
            &written.descriptor,
            &written.descriptor_sha256,
            &limits,
            &mut BatchCounters::default(),
            &cancel,
        )
        .unwrap();
        let generation = admitted.descriptor().generation.as_ref().unwrap();
        assert_eq!(generation.origin, GenerationOrigin::TrellisRun);
        assert_eq!(generation.trust_profile, Some(GenerationTrust::Restricted));
        let joined = join_staged(
            &mut admitted,
            &store,
            captured,
            &limits,
            &mut LookupBudget::new(&limits),
            &mut BatchCounters::default(),
            DestinationMutations::default(),
            &cancel,
        )
        .unwrap();
        let prior = {
            let leader = store.publication_for_tests().unwrap();
            store
                .publish_semantic_batch(&joined, admitted.stage_mut(), captured, &leader, &cancel)
                .unwrap()
        };
        drop(admitted);
        let prior_semantic = store.semantic_at(prior).unwrap();
        assert!(!prior_semantic.provenance.is_empty());
        assert!(
            prior_semantic
                .provenance
                .iter()
                .all(|proof| proof.derived_from.is_some())
        );
        let raw = store.selected_raw_inputs_at(prior).unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].captured_pin, captured);
        assert_eq!(
            raw[0].descriptor.generation.as_ref().unwrap().origin,
            GenerationOrigin::TrellisRun
        );
        let prior_native = store.retained_native_snapshot_at(prior).unwrap();
        let welcome = prior_native
            .0
            .declarations
            .iter()
            .find(|declaration| {
                declaration.document.path == "pkg/b.py"
                    && declaration.name.as_deref() == Some("welcome")
            })
            .expect("real fixture has the welcome declaration");
        let before = graph(&store, prior, &welcome.syntax_id);
        let before_edge = before
            .edges
            .iter()
            .find(|edge| {
                edge.binding.as_ref().is_some_and(|binding| {
                    matches!(binding.declared_target.as_ref(), Some(Target::Internal { document, .. }) if document.path == "pkg/a.py")
                })
            })
            .expect("real Python evidence binds welcome's call to greet");
        let binding = before_edge.binding.as_ref().unwrap();
        let proof = prior_semantic
            .provenance
            .iter()
            .find(|proof| proof.id == binding.provenance_id)
            .unwrap();
        assert_ne!(proof.freshness, Freshness::Stale);
        assert!(matches!(
            binding.declared_target.as_ref(),
            Some(Target::Internal { revision_id, .. })
                if revision_id == &prior_native.0.revision.id
        ));
        let prior_versions = versions(&store, prior);
        assert_eq!(prior_versions.len(), 3);
        let pyproject = fs::read_to_string(root.join("pyproject.toml")).unwrap();
        fs::write(
            root.join("pyproject.toml"),
            pyproject.replace("1.0.0", "1.0.1"),
        )
        .unwrap();
        let native = publish_native(&store, &options, &cancel);
        let current_native = store.retained_native_snapshot_at(native).unwrap();
        assert_ne!(native, prior);
        assert_ne!(current_native.0.revision.id, prior_native.0.revision.id);
        assert_ne!(
            current_native.0.revision.config_hash,
            prior_native.0.revision.config_hash
        );
        assert_ne!(
            current_native.0.revision.dependency_hash,
            prior_native.0.revision.dependency_hash
        );
        assert_eq!(versions(&store, native), prior_versions);
        assert_eq!(current_native.1, prior_native.1);
        println!(
            "B3-RED-READY real=scip-python trust=restricted facts={} samePythonVids=true omittedPyprojectChanged=true",
            prior_semantic.provenance.len()
        );
        for coverage in prior_semantic
            .coverage
            .iter()
            .filter(|coverage| coverage.selected)
        {
            let projection = store
                .semantic_projection_at(native, "scip-python", "python", &coverage.document_path)
                .unwrap()
                .unwrap();
            assert_eq!(
                projection.semantic_status,
                SemanticStatus::OlderBasis,
                "an omitted pyproject edit must not hide unchanged restricted-Python evidence: {}",
                coverage.document_path
            );
            assert_eq!(projection.current_pin, native);
            assert_eq!(projection.semantic_basis_pin, prior);
            assert_eq!(
                projection.basis_native_revision_id,
                prior_native.0.revision.id
            );
        }
        let after = graph(&store, native, &welcome.syntax_id);
        assert_eq!(after.resolved_revision_id, current_native.0.revision.id);
        let edge = after
            .edges
            .iter()
            .find(|edge| edge.call.id == before_edge.call.id)
            .unwrap();
        assert_eq!(edge.binding.as_ref(), Some(binding));
        assert_eq!(edge.to, before_edge.to);
        assert_eq!(edge.historical_freshness, Some(proof.freshness));
        assert_eq!(edge.effective_freshness, Some(Freshness::PossiblyStale));
        let projection = edge.projection.as_ref().unwrap();
        assert_eq!(projection.semantic_status, SemanticStatus::OlderBasis);
        assert_eq!(projection.current_pin, native);
        assert_eq!(projection.semantic_basis_pin, prior);
        assert_eq!(
            projection.basis_native_revision_id,
            prior_native.0.revision.id
        );
        let projected = after
            .projected_provenance
            .iter()
            .find(|projected| projected.evidence.id == proof.id)
            .unwrap();
        assert_eq!(&projected.evidence, proof);
        assert_eq!(&projected.projection, projection);
        assert_eq!(projected.historical_freshness, proof.freshness);
        assert_eq!(projected.effective_freshness, Freshness::PossiblyStale);
        let (source_pin, source) = store.source_at("pkg/b.py", Some(native)).unwrap().unwrap();
        assert_eq!(source_pin, native);
        assert_eq!(
            source.text.as_bytes(),
            prior_native.1.get(&welcome.document).unwrap()
        );
        let (document, bytes) = store
            .native_source_at(native, &welcome.document)
            .unwrap()
            .unwrap();
        assert_eq!(document.revision_id, current_native.0.revision.id);
        assert_eq!(bytes, source.text.as_bytes());
        assert_eq!(store.semantic_at(prior).unwrap(), prior_semantic);
        assert_eq!(
            store.retained_native_snapshot_at(prior).unwrap(),
            prior_native
        );
        let retained_raw = store.selected_raw_inputs_at(native).unwrap();
        assert_eq!(retained_raw[0].batch_id, raw[0].batch_id);
        assert_eq!(retained_raw[0].descriptor, raw[0].descriptor);
        assert_eq!(retained_raw[0].captured_pin, captured);

        // Unlike omitted pyproject, this retained file was supplied to the producer.
        fs::write(
            root.join("requirements.txt"),
            b"# changed supplied input at N2\n",
        )
        .unwrap();
        let included = publish_native(&store, &options, &cancel);
        assert_eq!(versions(&store, included), prior_versions);
        let projection = store
            .semantic_projection_at(included, "scip-python", "python", "pkg/b.py")
            .unwrap()
            .unwrap();
        assert_eq!(projection.semantic_status, SemanticStatus::CatchingUp);
        assert_eq!(projection.semantic_basis_pin, prior);
        let pending = graph(&store, included, &welcome.syntax_id);
        assert!(pending.partial);
        assert!(pending.edges.iter().any(|edge| {
            edge.call.id == before_edge.call.id
                && edge.binding.is_none()
                && edge.boundary_reason == BoundaryReason::CatchingUp
        }));
        assert_eq!(store.semantic_at(prior).unwrap(), prior_semantic);
        assert_eq!(
            store.retained_native_snapshot_at(prior).unwrap(),
            prior_native
        );
        assert_eq!(
            store
                .semantic_projection_at(native, "scip-python", "python", "pkg/b.py")
                .unwrap()
                .unwrap()
                .semantic_status,
            SemanticStatus::OlderBasis
        );
        println!(
            "E2E-EVIDENCE producer=scip-python trustProfile=restricted omitted=pyproject.toml included=requirements.txt basis={prior:?} native={native:?} olderBasis=true effectiveFreshness=possiblyStale includedChange=catchingUp"
        );
        Some(())
    }

    #[test]
    fn scip_python_restricted_omitted_context_keeps_older_basis() {
        run(Availability::SkipIfUnavailable);
    }

    #[test]
    #[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
    fn scip_python_restricted_omitted_context_keeps_older_basis_strict() {
        assert!(run(Availability::Required).is_some());
    }
}
