use super::batch::{self, scan_artifact};
use super::profiles::{self, Target};
use super::{ProducerName, TrustProfile};
use crate::semantic_batch::descriptor::{
    Generation, GenerationComponent, GenerationOrigin, GenerationTrust,
};
use protobuf::Message;

const ID: &str = "0123456789abcdef0123456789abcdef";

fn artifact(project_root: &str, paths: &[&str]) -> Vec<u8> {
    let mut index = scip::types::Index::new();
    let mut metadata = scip::types::Metadata::new();
    metadata.project_root = project_root.into();
    index.metadata = Some(metadata).into();
    for path in paths {
        let mut document = scip::types::Document::new();
        document.relative_path = (*path).into();
        index.documents.push(document);
    }
    index.write_to_bytes().unwrap()
}

#[test]
fn artifact_scan_reads_project_root_and_documents() {
    let scan = scan_artifact(&artifact("file:///w", &["src/a.js", "src/b.js"])).unwrap();
    assert_eq!(scan.project_root, "file:///w");
    assert_eq!(
        scan.documents.into_iter().collect::<Vec<_>>(),
        ["src/a.js", "src/b.js"]
    );
    assert_eq!(
        scan_artifact(&artifact("", &[])).unwrap(),
        Default::default()
    );
}

#[test]
fn artifact_scan_is_bounded() {
    let code = |bytes: &[u8]| scan_artifact(bytes).unwrap_err().to_string();
    let full = artifact("file:///w", &["src/a.js"]);
    for cut in [1, full.len() - 1] {
        assert!(
            code(&full[..cut]).starts_with("producer_output_invalid"),
            "truncation at {cut}"
        );
    }
    let long = "p".repeat(4097);
    assert!(code(&artifact("", &[&long])).starts_with("producer_output_invalid: document path"));
    let many: Vec<String> = (0..100_001).map(|n| format!("d{n}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert!(code(&artifact("", &many)).starts_with("producer_output_invalid: artifact documents"));
    let oversized = vec![0_u8; (64 << 20) + 1];
    assert_eq!(code(&oversized), "limit_exceeded: producer artifact bytes");
    // A document without a path and a non-UTF-8 root are not artifacts Trellis can describe.
    let mut document = scip::types::Document::new();
    document.language = "javascript".into();
    let mut index = scip::types::Index::new();
    index.documents.push(document);
    assert!(code(&index.write_to_bytes().unwrap()).starts_with("producer_output_invalid"));
    assert!(code(&[0x0a, 0x03, 0x1a, 0x01, 0xff]).starts_with("producer_output_invalid"));
}

fn resolution() -> super::detect::Resolution {
    use super::detect::{Exe, Resolution, Verification};
    let exe = |path: &str| Exe {
        path: path.into(),
        sha256: "ab".repeat(32),
        dir: "/bin".into(),
    };
    Resolution {
        launcher: "/opt/launcher".into(),
        verification: Verification {
            method: "testStub",
            observed: String::new(),
        },
        node: Some(exe("/nodes/node")),
        java: Some(exe("/jdk/bin/java")),
        java_home: None,
        rustup: Some(exe("/opt/rustup")),
        python3: Some(exe("/py/python3")),
    }
}

fn generation(table_for: Option<(Target, TrustProfile)>) -> Generation {
    let (origin, table) = match table_for {
        Some((target, trust)) => (
            GenerationOrigin::TrellisRun,
            profiles::run_generation(&profiles::row(target, trust).unwrap(), &resolution()),
        ),
        None => (GenerationOrigin::OwnerPickup, profiles::pickup_generation()),
    };
    batch::run_generation(&table, origin, ID)
}

#[test]
fn generation_table_per_origin_and_producer() {
    use GenerationComponent::{
        Config, DependencyManifests, ExternalInputs, ProducerExecutable, ProducerVersion, Runtime,
        Source,
    };
    for producer in [ProducerName::ScipTypescript, ProducerName::ScipPython] {
        for trust in [TrustProfile::Restricted, TrustProfile::Trusted] {
            let g = generation(Some((Target::Real(producer), trust)));
            assert_eq!(
                g.observed,
                [
                    Source,
                    Config,
                    DependencyManifests,
                    ProducerVersion,
                    Runtime
                ]
            );
            assert_eq!(g.asserted, [ProducerExecutable, ExternalInputs]);
            assert_eq!(g.runtime.len(), 1);
            assert_eq!(g.runtime[0].name, "node");
        }
    }
    for producer in [ProducerName::ScipJava, ProducerName::RustAnalyzer] {
        let g = generation(Some((Target::Real(producer), TrustProfile::Trusted)));
        assert_eq!(
            g.observed,
            [
                Source,
                Config,
                DependencyManifests,
                ProducerVersion,
                ProducerExecutable
            ]
        );
        assert_eq!(g.asserted, [Runtime, ExternalInputs]);
        assert!(g.runtime.is_empty());
    }
    let stub = generation(Some((Target::Stub, TrustProfile::Restricted)));
    assert_eq!(stub.observed, [Source, Config, DependencyManifests]);
    assert!(stub.runtime.is_empty());
    assert_eq!(stub.trust_profile, Some(GenerationTrust::Restricted));
    let pickup = generation(None);
    assert!(pickup.observed.is_empty());
    assert_eq!(pickup.asserted, GenerationComponent::ALL);
    assert_eq!(pickup.trust_profile, None);
    assert_eq!(pickup.origin, GenerationOrigin::OwnerPickup);
}

#[cfg(feature = "test-support")]
mod with_store {
    use super::{ID, artifact, batch};
    use crate::{
        indexer::{IndexOptions, index_workspace_bundle},
        producer::batch::BatchSpec,
        semantic_batch::{
            BatchLimits,
            descriptor::{Descriptor, PageKind, SourceManifestHasher},
        },
        store::Store,
    };
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{Arc, atomic::AtomicBool},
    };

    struct World {
        _state: tempfile::TempDir,
        _workspace: tempfile::TempDir,
        root: PathBuf,
        store: Store,
    }

    fn world(files: &[(String, String)]) -> World {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(workspace.path()).unwrap();
        for (path, text) in files {
            let target = root.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, text).unwrap();
        }
        let store = Store::open_for_tests(state.path(), &root).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(root.clone()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let leader = store.publication_for_tests().unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                &leader,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        drop(leader);
        World {
            _state: state,
            _workspace: workspace,
            root,
            store,
        }
    }

    fn js(names: &[&str]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|name| (format!("src/{name}.js"), format!("function {name}(){{}}\n")))
            .collect()
    }

    /// Writes `bytes` as the pickup artifact and the batch for it; returns the batch directory.
    fn write(world: &World, bytes: &[u8]) -> anyhow::Result<(PathBuf, Descriptor)> {
        let dir = world._state.path().join(format!("pickups-{ID}"));
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join(batch::ARTIFACT_PATH), bytes).unwrap();
        let (pin, language) = batch::pickup_target("scip-typescript").unwrap();
        let head = world.store.index_baseline().unwrap();
        let written = world.store.with_native_reader(head, |reader| {
            batch::write_batch(
                reader,
                &BatchSpec {
                    pin: &pin,
                    language,
                    workspace: &world.root,
                    dir: &dir,
                    generation: batch::pickup_generation(ID),
                },
            )
        })?;
        let descriptor = Descriptor::parse(&fs::read(&written.descriptor).unwrap()).unwrap();
        Ok((dir, descriptor))
    }

    fn rows(
        dir: &Path,
        entry: &crate::semantic_batch::descriptor::PageEntry,
    ) -> Vec<serde_json::Value> {
        let page: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join(&entry.relative_path)).unwrap()).unwrap();
        page["rows"].as_array().unwrap().clone()
    }

    #[test]
    fn descriptor_header_matches_retained_revision() {
        let world = world(&js(&["a", "b"]));
        let (dir, descriptor) = write(&world, &artifact("", &["src/a.js", "src/b.js"])).unwrap();
        let head = world.store.index_baseline().unwrap();
        let header = world
            .store
            .with_native_reader(head, |reader| reader.header())
            .unwrap();
        assert_eq!(descriptor.source_revision, header.revision.id);
        assert_eq!(descriptor.source_set.id, header.source_set.id);
        assert_eq!(descriptor.source_set.root_id, header.source_set.root_id);
        assert_eq!(
            descriptor.components.config_hash,
            header.revision.config_hash
        );
        assert_eq!(
            descriptor.metadata.project_root,
            crate::scip_admission::file_uri(&world.root).unwrap()
        );
        assert_eq!(descriptor.generation, Some(batch::pickup_generation(ID)));
        let expected = world
            .store
            .with_native_reader(head, |reader| {
                let mut hasher = SourceManifestHasher::new(&header.source_set.id);
                let mut inventory = reader.inventory()?;
                while let Some(document) = inventory.next_document()? {
                    hasher.push(
                        &document.key.language,
                        &document.key.path,
                        &document.content_hash,
                    );
                }
                Ok(hasher.finish())
            })
            .unwrap();
        assert_eq!(descriptor.source_manifest_hash, expected);
        let limits = BatchLimits::PRODUCTION;
        descriptor.check_pages(&limits).unwrap();
        descriptor.check_artifacts(&limits).unwrap();
        descriptor.check_paths(&dir).unwrap();
        let bytes = fs::read(dir.join(batch::ARTIFACT_PATH)).unwrap();
        assert_eq!(descriptor.artifacts[0].sha256, crate::scip_raw::sha(&bytes));
        assert_eq!(descriptor.artifacts[0].byte_length, bytes.len() as u64);
        assert_eq!(descriptor.completion, crate::scip_raw::Completion::Complete);
    }

    #[test]
    fn pages_partition_inventory_within_limits() {
        let names: Vec<String> = (0..1050).map(|n| format!("f{n:04}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let world = world(&js(&refs));
        let paths: Vec<String> = names.iter().map(|n| format!("src/{n}.js")).collect();
        let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let (dir, descriptor) = write(&world, &artifact("", &path_refs)).unwrap();
        let limits = BatchLimits::PRODUCTION;
        descriptor.check_pages(&limits).unwrap();
        for kind in [PageKind::Source, PageKind::Coverage] {
            let pages: Vec<_> = descriptor.pages.iter().filter(|p| p.kind == kind).collect();
            assert_eq!(
                pages.iter().map(|p| p.row_count).collect::<Vec<_>>(),
                [1000, 50],
                "{kind:?}"
            );
            for page in &pages {
                assert!(page.byte_length <= limits.page_bytes);
                assert!(page.relative_path.starts_with("batch/"));
            }
            let all: Vec<String> = pages
                .iter()
                .flat_map(|page| rows(&dir, page))
                .map(|row| row["relativePath"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(all, paths, "{kind:?} rows partition the inventory in order");
        }
        assert_eq!(descriptor.totals.source_documents, 1050);
        assert_eq!(descriptor.totals.covered_documents, 1050);
    }

    #[test]
    fn missing_documents_marked_failed() {
        let world = world(&js(&["a", "b", "c"]));
        let (dir, descriptor) = write(&world, &artifact("", &["src/b.js"])).unwrap();
        assert_eq!(descriptor.completion, crate::scip_raw::Completion::Partial);
        assert_eq!(descriptor.totals.covered_documents, 1);
        assert_eq!(descriptor.totals.excluded_documents, 2);
        let coverage = descriptor
            .pages
            .iter()
            .find(|page| page.kind == PageKind::Coverage)
            .unwrap();
        let by_path: Vec<(String, serde_json::Value, serde_json::Value)> = rows(&dir, coverage)
            .into_iter()
            .map(|row| {
                (
                    row["relativePath"].as_str().unwrap().to_owned(),
                    row["artifact"].clone(),
                    row["exclusion"].clone(),
                )
            })
            .collect();
        assert_eq!(
            by_path,
            [
                ("src/a.js".into(), serde_json::Value::Null, "failed".into()),
                ("src/b.js".into(), 0.into(), serde_json::Value::Null),
                ("src/c.js".into(), serde_json::Value::Null, "failed".into()),
            ]
        );
    }

    #[test]
    fn writer_refuses_unadmitted_project_root_and_missing_artifact() {
        let world = world(&js(&["a"]));
        let error = write(&world, &artifact("file:///elsewhere", &["src/a.js"])).unwrap_err();
        assert_eq!(error.to_string(), "producer_output_invalid: project root");
        for spelling in [
            crate::scip_admission::file_uri(&world.root).unwrap(),
            format!("{}/", crate::scip_admission::file_uri(&world.root).unwrap()),
        ] {
            let (_, descriptor) = write_again(&world, &artifact(&spelling, &["src/a.js"]));
            assert_eq!(descriptor.metadata.project_root, spelling);
        }
        let dir = world._state.path().join("empty");
        fs::create_dir_all(&dir).unwrap();
        let (pin, language) = batch::pickup_target("scip-typescript").unwrap();
        let head = world.store.index_baseline().unwrap();
        let error = world
            .store
            .with_native_reader(head, |reader| {
                batch::write_batch(
                    reader,
                    &BatchSpec {
                        pin: &pin,
                        language,
                        workspace: &world.root,
                        dir: &dir,
                        generation: batch::pickup_generation(ID),
                    },
                )
            })
            .unwrap_err();
        assert!(error.to_string().starts_with("producer_output_missing"));
    }

    fn write_again(world: &World, bytes: &[u8]) -> (PathBuf, Descriptor) {
        let dir = world._state.path().join(format!("pickups-{ID}"));
        fs::remove_dir_all(&dir).unwrap();
        write(world, bytes).unwrap()
    }
}
