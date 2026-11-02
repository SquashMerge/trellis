use super::batch::{self, scan_artifact};
use super::profiles::{self, Target};
use super::{ProducerName, TrustProfile};
use crate::semantic_batch::descriptor::{
    Generation, GenerationComponent, GenerationOrigin, GenerationTrust,
};
use crate::semantic_batch::limits::BatchLimits;
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

/// The owner-configured artifact copy enforces the same cap at its exact boundary. The source
/// is sparse, but each case writes and hashes a full cap's worth of copy.
#[test]
fn copy_artifact_keeps_the_production_cap_at_its_boundary() {
    let cap = BatchLimits::PRODUCTION.artifact_bytes;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("index.scip");
    std::fs::File::create(&source)
        .unwrap()
        .set_len(cap + 1)
        .unwrap();
    let over = temp.path().join("over");
    std::fs::create_dir(&over).unwrap();
    assert_eq!(
        batch::copy_artifact(&source, &over)
            .unwrap_err()
            .to_string(),
        "limit_exceeded: artifact bytes"
    );
    std::fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_len(cap)
        .unwrap();
    let within = temp.path().join("within");
    std::fs::create_dir(&within).unwrap();
    assert_eq!(batch::copy_artifact(&source, &within).unwrap().1, cap);
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
fn artifact_scan_accepts_utf8_split_at_chunk_boundary() {
    let root = format!("{}é", "x".repeat(64 * 1024 - 1));
    assert_eq!(
        scan_artifact(&artifact(&root, &[])).unwrap().project_root,
        root
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
    let oversized = vec![0_u8; BatchLimits::PRODUCTION.artifact_bytes as usize + 1];
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

    fn frozen_dir(world: &World) -> PathBuf {
        let (roots, identity) = world.store.state_topology();
        fs::canonicalize(&roots.cache)
            .unwrap()
            .join("producers")
            .join(&identity.root_key)
            .join("runs")
            .join(ID)
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

    fn real_artifact(paths: &[&str], invalid_late_enum: bool) -> Vec<u8> {
        use protobuf::{EnumOrUnknown, Message};
        let pin = crate::scip_admission::pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let mut index = scip::types::Index::new();
        let mut metadata = scip::types::Metadata::new();
        metadata.text_document_encoding = EnumOrUnknown::from_i32(1);
        let mut tool = scip::types::ToolInfo::new();
        tool.name = pin.name;
        tool.version = pin.version;
        metadata.tool_info = Some(tool).into();
        index.metadata = Some(metadata).into();
        for (n, path) in paths.iter().enumerate() {
            let mut doc = scip::types::Document::new();
            doc.relative_path = (*path).to_owned();
            doc.language = "javascript".into();
            let mut occurrence = scip::types::Occurrence::new();
            occurrence.range = vec![0, 0, 1];
            occurrence.symbol = format!("pkg d{n}.");
            occurrence.symbol_roles = 1;
            if invalid_late_enum && n + 1 == paths.len() {
                occurrence.syntax_kind = EnumOrUnknown::from_i32(999_999);
            }
            doc.occurrences.push(occurrence);
            index.documents.push(doc);
        }
        index.write_to_bytes().unwrap()
    }

    fn frozen_with(
        world: &World,
        bytes: &[u8],
        before: impl FnOnce(&Path),
    ) -> anyhow::Result<(PathBuf, batch::WrittenBatch)> {
        let dir = frozen_dir(world);
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join(batch::ARTIFACT_PATH), bytes).unwrap();
        before(&dir);
        let pin = crate::scip_admission::pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let generation =
            super::generation(Some((super::Target::Stub, super::TrustProfile::Restricted)));
        let written = batch::write_frozen_batch(
            &world.store,
            &BatchSpec {
                pin: &pin,
                language: "javascript",
                workspace: &world.root,
                dir: &dir,
                generation,
            },
            &BatchLimits::PRODUCTION,
            &mut Default::default(),
            &Arc::new(AtomicBool::new(false)),
        )?;
        Ok((dir, written))
    }

    fn frozen(world: &World, bytes: &[u8]) -> anyhow::Result<(PathBuf, batch::WrittenBatch)> {
        frozen_with(world, bytes, |_| {})
    }

    fn frozen_observed(
        world: &World,
        bytes: &[u8],
        cancel: &crate::model::CancelFlag,
        observer: &dyn Fn(batch::FrozenEvent),
    ) -> anyhow::Result<batch::WrittenBatch> {
        let dir = frozen_dir(world);
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join(batch::ARTIFACT_PATH), bytes).unwrap();
        let pin = crate::scip_admission::pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let generation =
            super::generation(Some((super::Target::Stub, super::TrustProfile::Restricted)));
        batch::write_frozen_batch_observed(
            &world.store,
            &BatchSpec {
                pin: &pin,
                language: "javascript",
                workspace: &world.root,
                dir: &dir,
                generation,
            },
            &BatchLimits::PRODUCTION,
            &mut Default::default(),
            cancel,
            observer,
        )
    }

    #[test]
    fn frozen_batch_validates_real_scip_and_survives_original_inode_write() {
        use std::io::{Read, Write};
        use std::os::unix::fs::MetadataExt;
        use std::process::{Command, Stdio};
        let world = world(&js(&["a", "b"]));
        let bytes = real_artifact(&["src/a.js", "src/b.js"], false);
        let mut child = None;
        let (dir, written) = frozen_with(&world, &bytes, |dir| {
            let mut writer = Command::new("/bin/sh")
                .args([
                    "-c",
                    "exec 3<>\"$1\"; printf R; if IFS= read -r go; then printf modified >&3; fi",
                    "writer",
                ])
                .arg(dir.join(batch::ARTIFACT_PATH))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut ready = [0];
            writer
                .stdout
                .as_mut()
                .unwrap()
                .read_exact(&mut ready)
                .unwrap();
            assert_eq!(ready, [b'R']);
            child = Some(writer);
        })
        .unwrap();
        let descriptor = Descriptor::parse(&fs::read(&written.descriptor).unwrap()).unwrap();
        let path = dir.join(&descriptor.artifacts[0].relative_path);
        assert_ne!(
            fs::metadata(dir.join(batch::ARTIFACT_PATH)).unwrap().ino(),
            fs::metadata(&path).unwrap().ino()
        );
        assert_eq!(fs::metadata(&path).unwrap().nlink(), 1);
        assert_eq!(
            descriptor.metadata.project_root,
            format!(
                "{}/worktree",
                crate::scip_admission::file_uri(&dir).unwrap()
            )
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let mut writer = child.unwrap();
        writer.stdin.as_mut().unwrap().write_all(b"go\n").unwrap();
        assert!(writer.wait().unwrap().success());
        assert_ne!(fs::read(dir.join(batch::ARTIFACT_PATH)).unwrap(), bytes);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        batch::validate_frozen_batch(
            &world.store,
            &world.root,
            &dir,
            &written,
            crate::semantic_batch::descriptor::GenerationOrigin::TrellisRun,
            ID,
            &BatchLimits::PRODUCTION,
            &mut Default::default(),
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(fs::read_dir(dir.join("batch")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("semantic-stage-")
        }));
    }

    #[test]
    fn frozen_batch_marks_missing_target_document_failed() {
        let world = world(&js(&["a", "b"]));
        let (dir, written) = frozen(&world, &real_artifact(&["src/a.js"], false)).unwrap();
        let descriptor = Descriptor::parse(&fs::read(&written.descriptor).unwrap()).unwrap();
        assert_eq!(descriptor.completion, crate::scip_raw::Completion::Partial);
        assert_eq!(
            (
                descriptor.totals.covered_documents,
                descriptor.totals.excluded_documents
            ),
            (1, 1)
        );
        let coverage = descriptor
            .pages
            .iter()
            .find(|page| page.kind == PageKind::Coverage)
            .unwrap();
        assert!(
            rows(&dir, coverage)
                .iter()
                .any(|row| row["relativePath"] == "src/b.js"
                    && row["artifact"].is_null()
                    && row["exclusion"] == "failed")
        );
    }

    #[test]
    fn frozen_batch_preserves_thirty_three_failed_dispositions() {
        let names: Vec<String> = (0..34).map(|n| format!("f{n:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let world = world(&js(&names));
        let (dir, written) = frozen(&world, &real_artifact(&["src/f00.js"], false)).unwrap();
        let descriptor = Descriptor::parse(&fs::read(&written.descriptor).unwrap()).unwrap();
        assert_eq!(descriptor.completion, crate::scip_raw::Completion::Partial);
        assert_eq!(
            (
                descriptor.totals.covered_documents,
                descriptor.totals.excluded_documents
            ),
            (1, 33)
        );
        let failed = descriptor
            .pages
            .iter()
            .filter(|page| page.kind == PageKind::Coverage)
            .flat_map(|page| rows(&dir, page))
            .filter(|row| row["exclusion"] == "failed" && row["artifact"].is_null())
            .count();
        assert_eq!(failed, 33);
    }

    #[test]
    fn frozen_batch_rejects_late_invalid_nested_enum() {
        let world = world(&js(&["a", "b"]));
        let bytes = real_artifact(&["src/a.js", "src/b.js"], true);
        assert_eq!(batch::scan_artifact(&bytes).unwrap().documents.len(), 2);
        let error = frozen(&world, &bytes).unwrap_err();
        assert!(
            error.to_string().starts_with("producer_output_invalid"),
            "{error:#}"
        );
        assert!(
            fs::read_dir(frozen_dir(&world).join("batch"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("semantic-stage-"))
        );
    }

    #[test]
    fn frozen_batch_rejects_duplicate_and_unlisted_documents() {
        use protobuf::Message;
        for duplicate in [true, false] {
            let world = world(&js(&["a"]));
            let mut index =
                scip::types::Index::parse_from_bytes(&real_artifact(&["src/a.js"], false)).unwrap();
            let mut extra = index.documents[0].clone();
            if !duplicate {
                extra.relative_path = "src/extra.js".into();
            }
            index.documents.push(extra);
            let bytes = index.write_to_bytes().unwrap();
            assert!(batch::scan_artifact(&bytes).is_ok());
            let error = frozen(&world, &bytes).unwrap_err();
            assert!(
                error.to_string().starts_with("producer_output_invalid"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn frozen_batch_keeps_other_language_not_requested() {
        let mut files = js(&["a"]);
        files.push(("src/other.py".into(), "def other(): pass\n".into()));
        let world = world(&files);
        let (dir, written) = frozen(&world, &real_artifact(&["src/a.js"], false)).unwrap();
        let descriptor = Descriptor::parse(&fs::read(&written.descriptor).unwrap()).unwrap();
        let coverage = descriptor
            .pages
            .iter()
            .find(|page| page.kind == PageKind::Coverage)
            .unwrap();
        let coverage_rows = rows(&dir, coverage);
        assert!(
            coverage_rows
                .iter()
                .any(|row| row["relativePath"] == "src/other.py"
                    && row["exclusion"] == "notRequested")
        );
    }

    #[test]
    fn frozen_batch_reports_io_failure() {
        let world = world(&js(&["a"]));
        let error = frozen_with(&world, &real_artifact(&["src/a.js"], false), |dir| {
            fs::create_dir(dir.join("batch")).unwrap();
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("producer_failed: frozen batch I/O"),
            "{error:#}"
        );
    }

    #[test]
    fn frozen_batch_reports_role_metadata_and_open_faults_as_operational() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        for (point, name, errno) in [
            ("metadata", "descriptor.json", libc::EIO),
            ("open", "descriptor.json", libc::EIO),
            ("metadata", "source-0000.json", libc::EIO),
            ("metadata", "artifact.scip", libc::EACCES),
            ("open", "source-0000.json", libc::EIO),
            ("open", "source-0000.json", libc::EACCES),
            ("open", "coverage-0000.json", libc::EIO),
            ("open", "artifact.scip", libc::EIO),
        ] {
            let world = world(&js(&["a"]));
            let events = std::cell::RefCell::new(Vec::new());
            crate::scip_admission::fail_role_io(point, name, errno);
            let error = frozen_observed(
                &world,
                &real_artifact(&["src/a.js"], false),
                &Arc::new(AtomicBool::new(false)),
                &|event| events.borrow_mut().push(event),
            )
            .unwrap_err();
            assert!(
                error.to_string().starts_with("producer_failed:"),
                "{point} {name}: {error:#}"
            );
            assert!(
                error.chain().any(|cause| cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.raw_os_error() == Some(errno))),
                "{point} {name}: {error:#}"
            );
            if name == "coverage-0000.json" {
                let events = events.borrow();
                assert!(events.iter().any(|event| matches!(event, E::End(P::CoveragePass, Err(code)) if code.starts_with("producer_failed:"))), "{events:?}");
                assert!(!events.contains(&E::Begin(P::CoverageClosure)));
            }
            assert!(world.store.is_ready_disposition());
        }
    }

    #[test]
    fn frozen_batch_reports_artifact_open_eio_as_operational() {
        let world = world(&js(&["a"]));
        crate::scip_admission::fail_role_io("open", "artifact.scip", libc::EIO);
        let error = frozen(&world, &real_artifact(&["src/a.js"], false)).unwrap_err();
        assert!(
            error.to_string().starts_with("producer_failed:"),
            "{error:#}"
        );
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.raw_os_error() == Some(libc::EIO))),
            "{error:#}"
        );
    }

    #[test]
    fn frozen_batch_role_busy_and_root_loss_keep_their_classes() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        {
            let world = world(&js(&["a"]));
            let events = std::cell::RefCell::new(Vec::new());
            crate::scip_admission::fail_role_io("open", "coverage-0000.json", libc::EBUSY);
            let error = frozen_observed(
                &world,
                &real_artifact(&["src/a.js"], false),
                &Arc::new(AtomicBool::new(false)),
                &|event| events.borrow_mut().push(event),
            )
            .unwrap_err();
            assert!(error.to_string().starts_with("storage_busy:"), "{error:#}");
            assert!(events.borrow().iter().any(|event| matches!(event, E::End(P::CoveragePass, Err(code)) if code.starts_with("storage_busy:"))));
        }

        let world = world(&js(&["a"]));
        let moved = world.root.with_extension("moved");
        let did_move = std::cell::Cell::new(false);
        let events = std::cell::RefCell::new(Vec::new());
        crate::scip_admission::fail_role_io("open", "coverage-0000.json", libc::EIO);
        let result = frozen_observed(
            &world,
            &real_artifact(&["src/a.js"], false),
            &Arc::new(AtomicBool::new(false)),
            &|event| {
                if event == E::Begin(P::CoveragePass) && !did_move.replace(true) {
                    fs::rename(&world.root, &moved).unwrap();
                }
                events.borrow_mut().push(event);
            },
        );
        if did_move.get() {
            fs::rename(&moved, &world.root).unwrap();
        }
        let error = result.unwrap_err();
        assert!(error.to_string().starts_with("root_changed:"), "{error:#}");
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.raw_os_error() == Some(libc::EIO))),
            "{error:#}"
        );
        assert!(events.borrow().iter().any(|event| matches!(event, E::End(P::CoveragePass, Err(code)) if code.starts_with("root_changed:"))));
        assert!(!events.borrow().contains(&E::Begin(P::CoverageClosure)));
    }

    #[test]
    fn frozen_batch_missing_and_invalid_roles_keep_input_failure_classes() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        use std::os::unix::fs::symlink;
        for (name, use_symlink, expected) in [
            ("descriptor.json", false, "invalid_request:"),
            ("source-0000.json", false, "producer_output_invalid:"),
            ("source-0000.json", true, "producer_output_invalid:"),
        ] {
            let world = world(&js(&["a"]));
            let role = frozen_dir(&world).join("batch").join(name);
            let changed = std::cell::Cell::new(false);
            let error = frozen_observed(
                &world,
                &real_artifact(&["src/a.js"], false),
                &Arc::new(AtomicBool::new(false)),
                &|event| {
                    if event == E::End(P::Durability, Ok(())) && !changed.replace(true) {
                        fs::remove_file(&role).unwrap();
                        if use_symlink {
                            symlink("descriptor.json", &role).unwrap();
                        }
                    }
                },
            )
            .unwrap_err();
            assert!(changed.get());
            assert!(
                error.to_string().starts_with(expected),
                "{name} {use_symlink}: {error:#}"
            );
        }
    }

    #[test]
    fn frozen_batch_root_loss_takes_precedence_over_missing_role() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        let world = world(&js(&["a"]));
        let moved = world.root.with_extension("moved");
        let coverage = frozen_dir(&world).join("batch/coverage-0000.json");
        let did_move = std::cell::Cell::new(false);
        let result = frozen_observed(
            &world,
            &real_artifact(&["src/a.js"], false),
            &Arc::new(AtomicBool::new(false)),
            &|event| {
                if event == E::Begin(P::CoveragePass) && !did_move.replace(true) {
                    fs::remove_file(&coverage).unwrap();
                    fs::rename(&world.root, &moved).unwrap();
                }
            },
        );
        if did_move.get() {
            fs::rename(&moved, &world.root).unwrap();
        }
        assert!(did_move.get());
        let error = result.unwrap_err();
        assert!(error.to_string().starts_with("root_changed:"), "{error:#}");
    }

    #[test]
    fn external_role_io_keeps_existing_invalid_request_behavior() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("external.scip");
        fs::write(&file, b"x").unwrap();
        for point in ["metadata", "open"] {
            crate::scip_admission::fail_role_io(point, "external.scip", libc::EIO);
            let error =
                crate::index_coordinator::open_semantic_input(root.path(), &file).unwrap_err();
            assert!(
                error.to_string().starts_with("invalid_request:"),
                "{point}: {error:#}"
            );
        }
        let error = crate::index_coordinator::open_semantic_input(
            root.path(),
            &root.path().join("missing.scip"),
        )
        .unwrap_err();
        assert!(
            error.to_string().starts_with("invalid_request:"),
            "{error:#}"
        );
        let error = crate::index_coordinator::open_semantic_input(
            root.path(),
            &root.path().join("../outside.scip"),
        )
        .unwrap_err();
        assert!(
            error.to_string().starts_with("invalid_request:"),
            "{error:#}"
        );
    }

    #[test]
    fn frozen_batch_reports_real_decode_read_error_as_io() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        let world = world(&js(&["a"]));
        let events = std::cell::RefCell::new(Vec::new());
        crate::semantic_batch::admit::fail_next_decode_read();
        let error = frozen_observed(
            &world,
            &real_artifact(&["src/a.js"], false),
            &Arc::new(AtomicBool::new(false)),
            &|event| events.borrow_mut().push(event),
        )
        .unwrap_err();
        assert!(
            error.to_string().starts_with("producer_failed:"),
            "{error:#}"
        );
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.raw_os_error() == Some(libc::EIO))),
            "{error:#}"
        );
        assert!(events.borrow().iter().any(|event| matches!(event, E::End(P::FullDecode, Err(code)) if code.starts_with("producer_failed:"))));
    }

    #[test]
    fn frozen_batch_reports_truncated_decode_as_invalid_artifact() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        let world = world(&js(&["a"]));
        let batch_dir = frozen_dir(&world).join("batch");
        let result = frozen_observed(
            &world,
            &real_artifact(&["src/a.js"], false),
            &Arc::new(AtomicBool::new(false)),
            &|event| {
                if event == E::Begin(P::FullDecode) {
                    let stage = fs::read_dir(&batch_dir)
                        .unwrap()
                        .map(|entry| entry.unwrap().path())
                        .find(|path| {
                            path.file_name()
                                .unwrap()
                                .to_string_lossy()
                                .starts_with("semantic-stage-")
                        })
                        .unwrap();
                    fs::OpenOptions::new()
                        .write(true)
                        .open(stage.join("artifact-0.scip"))
                        .unwrap()
                        .set_len(4)
                        .unwrap();
                }
            },
        );
        let error = result.unwrap_err();
        assert!(
            error.to_string().starts_with("producer_output_invalid:"),
            "{error:#}"
        );
    }

    #[test]
    fn frozen_batch_preserves_root_loss_class() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        let world = world(&js(&["a"]));
        let moved = world.root.with_extension("moved");
        let did_move = std::cell::Cell::new(false);
        let result = frozen_observed(
            &world,
            &real_artifact(&["src/a.js"], false),
            &Arc::new(AtomicBool::new(false)),
            &|event| {
                if event == E::Begin(P::SnapshotRead) && !did_move.replace(true) {
                    fs::rename(&world.root, &moved).unwrap();
                }
            },
        );
        if did_move.get() {
            fs::rename(&moved, &world.root).unwrap();
        }
        let error = result.unwrap_err();
        assert!(error.to_string().starts_with("root_changed"), "{error:#}");
    }

    #[test]
    fn frozen_stage_faults_do_not_schedule_live_index_recovery() {
        for point in ["stage-source", "stage-artifact"] {
            for kind in ["corrupt", "conversion", "busy"] {
                let world = world(&js(&["a"]));
                assert!(world.store.is_ready_disposition());
                assert!(!world.store.recovery_pending_for_tests());
                crate::semantic_batch::admit::fail_preflight_at(point, kind);
                let error = frozen(&world, &real_artifact(&["src/a.js"], false)).unwrap_err();
                let expected = if kind == "busy" {
                    "storage_busy"
                } else {
                    "producer_failed"
                };
                assert!(
                    error.to_string().starts_with(expected),
                    "{point} {kind}: {error:#}"
                );
                assert!(
                    error.downcast_ref::<rusqlite::Error>().is_some(),
                    "{point} {kind}: {error:#}"
                );
                assert!(
                    world.store.is_ready_disposition(),
                    "{point} {kind}: {error:#}"
                );
                assert!(!world.store.recovery_pending_for_tests(), "{point} {kind}");
                assert!(world.store.index_baseline().is_ok(), "{point} {kind}");
            }
        }
    }

    #[test]
    fn frozen_native_reader_fault_still_schedules_live_index_recovery() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        for point in ["native-source", "native-source-bytes", "native-artifact"] {
            for kind in ["native", "corrupt", "corrupt-wrapped", "conversion", "busy"] {
                let world = world(&js(&["a"]));
                assert!(world.store.is_ready_disposition());
                crate::semantic_batch::admit::fail_preflight_at(point, kind);
                let events = std::cell::RefCell::new(Vec::new());
                let error = frozen_observed(
                    &world,
                    &real_artifact(&["src/a.js"], false),
                    &Arc::new(AtomicBool::new(false)),
                    &|event| events.borrow_mut().push(event),
                )
                .unwrap_err();
                let expected = match kind {
                    "native" => "incompatible_index",
                    "busy" => "storage_busy",
                    _ => "recovery_required",
                };
                assert!(
                    error.to_string().starts_with(expected),
                    "{point} {kind}: {error:#}"
                );
                assert_eq!(
                    world.store.is_ready_disposition(),
                    kind == "busy",
                    "{point} {kind}"
                );
                assert_eq!(
                    world.store.recovery_pending_for_tests(),
                    kind != "busy",
                    "{point} {kind}"
                );
                if matches!(kind, "corrupt" | "corrupt-wrapped") {
                    assert!(
                        matches!(error.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(info, _)) if info.code == rusqlite::ErrorCode::DatabaseCorrupt),
                        "{point}: {error:#}"
                    );
                }
                let events = events.into_inner();
                assert!(events.iter().any(|event| matches!(event, E::End(P::FrozenBatch, Err(code)) if code.starts_with(expected))), "{point} {kind}: {events:?}");
                if point == "native-artifact" {
                    assert!(events.iter().any(|event| matches!(event, E::End(P::FullDecode, Err(code)) if code.starts_with(expected))), "{point} {kind}: {events:?}");
                }
            }
        }
    }

    #[test]
    fn frozen_preflight_real_retained_source_corruption_schedules_recovery() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        let world = world(&js(&["a"]));
        let corrupted = std::cell::Cell::new(false);
        let result = frozen_observed(
            &world,
            &real_artifact(&["src/a.js"], false),
            &Arc::new(AtomicBool::new(false)),
            &|event| {
                if event == E::Begin(P::CoveragePass) && !corrupted.replace(true) {
                    let writer = world.store.publication_for_tests().unwrap();
                    world
                        .store
                        .mcp_replace_validated_source_bytes_for_tests(
                            &writer,
                            "src/a.js",
                            b"function changed(){}\n",
                        )
                        .unwrap();
                }
            },
        );
        assert!(corrupted.get());
        let error = result.unwrap_err();
        assert!(
            error.to_string().starts_with("incompatible_index"),
            "{error:#}"
        );
        assert!(
            error
                .downcast_ref::<crate::store::SelectedIntegrity>()
                .is_some(),
            "{error:#}"
        );
        assert!(!world.store.is_ready_disposition());
        assert!(world.store.recovery_pending_for_tests());
    }

    #[test]
    fn frozen_batch_survives_original_write_during_snapshot_read() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        use protobuf::Message;
        use std::{
            io::{Read, Write},
            process::{Command, Stdio},
        };
        let world = world(&js(&["a"]));
        let mut index =
            scip::types::Index::parse_from_bytes(&real_artifact(&["src/a.js"], false)).unwrap();
        index
            .metadata
            .as_mut()
            .unwrap()
            .tool_info
            .as_mut()
            .unwrap()
            .arguments
            .push("x".repeat(2 * 1024 * 1024));
        let bytes = index.write_to_bytes().unwrap();
        let dir = frozen_dir(&world);
        fs::create_dir_all(dir.join("out")).unwrap();
        let source = dir.join(batch::ARTIFACT_PATH);
        fs::write(&source, &bytes).unwrap();
        const OFFSET: usize = 1536 * 1024;
        assert_eq!(bytes[OFFSET], b'x');
        let mut writer = Command::new("python3")
            .args([
                "-c",
                "import os,sys; fd=os.open(sys.argv[1],os.O_RDWR); print('R',flush=True); sys.stdin.readline(); os.pwrite(fd,b'y',int(sys.argv[2])); os.close(fd)",
            ])
            .arg(&source)
            .arg(OFFSET.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = [0; 2];
        writer
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut ready)
            .unwrap();
        assert_eq!(&ready, b"R\n");
        let writer = std::cell::RefCell::new(Some(writer));
        let decoded = std::cell::Cell::new(false);
        let pin = crate::scip_admission::pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let generation =
            super::generation(Some((super::Target::Stub, super::TrustProfile::Restricted)));
        let written = batch::write_frozen_batch_observed(
            &world.store,
            &BatchSpec {
                pin: &pin,
                language: "javascript",
                workspace: &world.root,
                dir: &dir,
                generation,
            },
            &BatchLimits::PRODUCTION,
            &mut Default::default(),
            &Arc::new(AtomicBool::new(false)),
            &|event| {
                if event == E::End(P::FullDecode, Ok(())) {
                    decoded.set(true);
                }
                if let E::Progress(P::SnapshotRead, count) = event
                    && let Some(mut child) = writer.borrow_mut().take()
                {
                    assert_eq!(count, 1024 * 1024);
                    child.stdin.as_mut().unwrap().write_all(b"go\n").unwrap();
                    assert!(child.wait().unwrap().success());
                }
            },
        )
        .unwrap();
        assert!(writer.borrow().is_none());
        assert!(decoded.get());
        let changed = fs::read(&source).unwrap();
        assert_eq!(changed[OFFSET], b'y');
        assert_eq!(&changed[..OFFSET], &bytes[..OFFSET]);
        assert_eq!(&changed[OFFSET + 1..], &bytes[OFFSET + 1..]);
        let descriptor = Descriptor::parse(&fs::read(written.descriptor).unwrap()).unwrap();
        assert_eq!(
            descriptor.artifacts[0].sha256,
            crate::scip_raw::sha(&changed)
        );
        assert_eq!(
            fs::read(dir.join(&descriptor.artifacts[0].relative_path)).unwrap(),
            changed
        );
    }

    #[test]
    fn frozen_batch_reports_each_sync_failure_before_success() {
        for point in [
            "file",
            "page file",
            "descriptor file",
            "batch directory",
            "generation directory",
            "ancestor directory",
        ] {
            let world = world(&js(&["a"]));
            batch::fail_snapshot_sync(Some(point));
            let result = frozen(&world, &real_artifact(&["src/a.js"], false));
            batch::fail_snapshot_sync(None);
            let error = result.unwrap_err();
            assert!(
                error
                    .to_string()
                    .starts_with("producer_failed: frozen batch I/O"),
                "{point}: {error:#}"
            );
        }
    }

    #[test]
    fn frozen_batch_fails_each_ancestor_sync() {
        let probe = world(&js(&["a"]));
        let base = fs::canonicalize(&probe.store.state_topology().0.cache).unwrap();
        let count = frozen_dir(&probe)
            .ancestors()
            .skip(1)
            .take_while(|path| path.starts_with(&base))
            .count();
        drop(probe);
        for depth in 0..count {
            let world = world(&js(&["a"]));
            batch::fail_snapshot_ancestor(Some(depth));
            let result = frozen(&world, &real_artifact(&["src/a.js"], false));
            batch::fail_snapshot_ancestor(None);
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .starts_with("producer_failed: frozen batch I/O"),
                "ancestor {depth}"
            );
        }
        let world = world(&js(&["a"]));
        batch::fail_snapshot_ancestor(Some(count));
        let result = frozen(&world, &real_artifact(&["src/a.js"], false));
        batch::fail_snapshot_ancestor(None);
        result.expect("must stop syncing at the durable cache base");
    }

    #[test]
    fn frozen_batch_cancels_at_stream_scan_pages_and_sync_boundaries() {
        use batch::{FrozenEvent as E, FrozenPhase as P, FrozenSync as S};
        use std::sync::atomic::Ordering;
        let points = [
            E::Progress(P::SnapshotRead, 0),
            E::Progress(P::SnapshotWrite, 0),
            E::Progress(P::ShallowScan, 0),
            E::Progress(P::BatchPages, 0),
            E::BeforeSync(S::SnapshotFile),
            E::BeforeSync(S::PageFile),
            E::BeforeSync(S::DescriptorFile),
            E::BeforeSync(S::BatchDirectory),
            E::BeforeSync(S::GenerationDirectory),
            E::BeforeSync(S::AncestorDirectory(0)),
        ];
        for point in points {
            let world = world(&js(&["a"]));
            let cancel = Arc::new(AtomicBool::new(false));
            let reached = std::cell::Cell::new(false);
            let hits = std::cell::Cell::new(0);
            let last_bytes = std::cell::Cell::new(0);
            let observer = |event| {
                let hit = match (&point, &event) {
                    (E::Progress(expected, _), E::Progress(actual, _)) => expected == actual,
                    _ => point == event,
                };
                if hit {
                    reached.set(true);
                    hits.set(hits.get() + 1);
                    if let E::Progress(_, count) = event {
                        last_bytes.set(count);
                    }
                    cancel.store(true, Ordering::Relaxed);
                }
            };
            let mut bytes = real_artifact(&["src/a.js"], false);
            if matches!(
                point,
                E::Progress(P::SnapshotRead, _) | E::Progress(P::SnapshotWrite, _)
            ) {
                use protobuf::Message;
                let mut index = scip::types::Index::parse_from_bytes(&bytes).unwrap();
                index
                    .metadata
                    .as_mut()
                    .unwrap()
                    .tool_info
                    .as_mut()
                    .unwrap()
                    .arguments
                    .push("x".repeat(2 * 1024 * 1024));
                bytes = index.write_to_bytes().unwrap();
            }
            let result = frozen_observed(&world, &bytes, &cancel, &observer);
            assert!(reached.get(), "event {point:?}");
            if matches!(
                point,
                E::Progress(P::SnapshotRead, _) | E::Progress(P::SnapshotWrite, _)
            ) {
                assert_eq!(hits.get(), 1, "event {point:?}");
                assert!(last_bytes.get() < bytes.len() as u64, "event {point:?}");
            }
            assert_eq!(
                result.unwrap_err().to_string(),
                "cancelled",
                "event {point:?}"
            );
        }
    }

    #[test]
    fn frozen_batch_can_cancel_within_a_large_scan_field() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        use protobuf::Message;
        use std::sync::atomic::Ordering;
        let world = world(&js(&["a"]));
        let mut index =
            scip::types::Index::parse_from_bytes(&real_artifact(&["src/a.js"], false)).unwrap();
        index.metadata.as_mut().unwrap().project_root = "x".repeat(2 * 1024 * 1024);
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = std::cell::Cell::new(0);
        let result = frozen_observed(
            &world,
            &index.write_to_bytes().unwrap(),
            &cancel,
            &|event| {
                if matches!(event, E::Progress(P::ShallowScan, _)) {
                    progress.set(progress.get() + 1);
                    if progress.get() == 2 {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "cancelled");
        assert_eq!(progress.get(), 2);
    }

    #[test]
    fn frozen_batch_phase_events_record_cancelled_coverage_without_decode() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        use std::sync::atomic::Ordering;
        let world = world(&js(&["a"]));
        let cancel = Arc::new(AtomicBool::new(false));
        let events = std::cell::RefCell::new(Vec::new());
        let result = frozen_observed(
            &world,
            &real_artifact(&["src/a.js"], false),
            &cancel,
            &|event| {
                if event == E::Begin(P::CoveragePass) {
                    cancel.store(true, Ordering::Relaxed);
                }
                events.borrow_mut().push(event);
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "cancelled");
        let events = events.into_inner();
        assert!(events.contains(&E::End(P::CoveragePass, Err("cancelled".into()))));
        assert!(!events.contains(&E::Begin(P::FullDecode)));
    }

    #[test]
    fn frozen_batch_phase_events_record_directory_sync_failure() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        let world = world(&js(&["a"]));
        let events = std::cell::RefCell::new(Vec::new());
        batch::fail_snapshot_sync(Some("generation directory"));
        let result = frozen_observed(
            &world,
            &real_artifact(&["src/a.js"], false),
            &Arc::new(AtomicBool::new(false)),
            &|event| events.borrow_mut().push(event),
        );
        batch::fail_snapshot_sync(None);
        assert!(result.is_err());
        let events = events.into_inner();
        for phase in [P::Durability, P::FrozenBatch] {
            assert!(events.iter().any(|event| matches!(event, E::End(actual, Err(error)) if *actual == phase && error.starts_with("producer_failed:"))), "{phase:?}");
        }
        assert!(!events.contains(&E::Begin(P::FullDecode)));
    }

    #[test]
    fn frozen_batch_reports_real_phase_outcomes_including_decode_failure() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        for invalid in [false, true] {
            let world = world(&js(&["a", "b"]));
            let events = std::cell::RefCell::new(Vec::new());
            let result = frozen_observed(
                &world,
                &real_artifact(&["src/a.js", "src/b.js"], invalid),
                &Arc::new(AtomicBool::new(false)),
                &|event| events.borrow_mut().push(event),
            );
            assert_eq!(result.is_err(), invalid);
            let events = events.into_inner();
            if invalid {
                assert!(!events.contains(&E::Begin(P::CoverageClosure)));
            }
            for phase in [
                P::FrozenBatch,
                P::SnapshotRead,
                P::SnapshotWrite,
                P::ShallowScan,
                P::BatchPages,
                P::Durability,
                P::CoveragePass,
                P::ArtifactPass,
                P::FullDecode,
            ] {
                assert!(events.contains(&E::Begin(phase)), "missing {phase:?}");
                let end = events
                    .iter()
                    .find_map(|event| match event {
                        E::End(actual, status) if *actual == phase => Some(status),
                        _ => None,
                    })
                    .unwrap();
                let fails =
                    invalid && matches!(phase, P::FullDecode | P::ArtifactPass | P::FrozenBatch);
                assert_eq!(end.is_err(), fails, "{phase:?}: {end:?}");
                if fails {
                    assert!(
                        end.as_ref()
                            .unwrap_err()
                            .starts_with("producer_output_invalid")
                    );
                }
            }
        }
    }

    #[test]
    fn frozen_batch_measures_post_decode_coverage_closure_and_failure_in_order() {
        use batch::{FrozenEvent as E, FrozenPhase as P};
        for missing in [false, true] {
            let world = world(&js(&["a", "b"]));
            let events = std::cell::RefCell::new(Vec::new());
            if missing {
                crate::semantic_batch::admit::fail_preflight_at("stage-closure", "remove-document");
            }
            let result = frozen_observed(
                &world,
                &real_artifact(&["src/a.js", "src/b.js"], false),
                &Arc::new(AtomicBool::new(false)),
                &|event| events.borrow_mut().push(event),
            );
            assert_eq!(result.is_err(), missing);
            if let Err(error) = result {
                assert_eq!(
                    error.to_string(),
                    "producer_output_invalid: missing artifact document"
                );
            }
            let selected: Vec<_> = events
                .into_inner()
                .into_iter()
                .filter(|event| {
                    matches!(
                        event,
                        E::Begin(
                            P::CoveragePass | P::ArtifactPass | P::FullDecode | P::CoverageClosure
                        ) | E::End(
                            P::CoveragePass | P::ArtifactPass | P::FullDecode | P::CoverageClosure,
                            _
                        )
                    )
                })
                .collect();
            let status = if missing {
                Err("producer_output_invalid: missing artifact document".to_owned())
            } else {
                Ok(())
            };
            assert_eq!(
                selected,
                vec![
                    E::Begin(P::CoveragePass),
                    E::End(P::CoveragePass, Ok(())),
                    E::Begin(P::ArtifactPass),
                    E::Begin(P::FullDecode),
                    E::End(P::FullDecode, Ok(())),
                    E::Begin(P::CoverageClosure),
                    E::End(P::CoverageClosure, status.clone()),
                    E::End(P::ArtifactPass, status),
                ]
            );
            assert!(world.store.is_ready_disposition());
            assert!(!world.store.recovery_pending_for_tests());
        }
    }

    #[test]
    fn frozen_batch_checks_cancel_before_reading_output() {
        let world = world(&js(&["a"]));
        let dir = frozen_dir(&world);
        fs::create_dir_all(dir.join("out")).unwrap();
        let pin = crate::scip_admission::pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let generation =
            super::generation(Some((super::Target::Stub, super::TrustProfile::Restricted)));
        let cancel = Arc::new(AtomicBool::new(true));
        let error = batch::write_frozen_batch(
            &world.store,
            &BatchSpec {
                pin: &pin,
                language: "javascript",
                workspace: &world.root,
                dir: &dir,
                generation,
            },
            &BatchLimits::PRODUCTION,
            &mut Default::default(),
            &cancel,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        assert!(!dir.join("batch").exists());
    }

    #[test]
    fn frozen_batch_does_not_call_a_symlink_missing() {
        use std::os::unix::fs::symlink;
        let world = world(&js(&["a"]));
        let error = frozen_with(&world, &real_artifact(&["src/a.js"], false), |dir| {
            fs::remove_file(dir.join(batch::ARTIFACT_PATH)).unwrap();
            symlink("elsewhere", dir.join(batch::ARTIFACT_PATH)).unwrap();
        })
        .unwrap_err();
        assert!(
            !error.to_string().starts_with("producer_output_missing"),
            "{error:#}"
        );
    }

    #[test]
    fn frozen_batch_counts_snapshot_against_disk_growth() {
        let world = world(&js(&["a"]));
        let bytes = real_artifact(&["src/a.js"], false);
        let dir = frozen_dir(&world);
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join(batch::ARTIFACT_PATH), bytes).unwrap();
        let pin = crate::scip_admission::pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let generation =
            super::generation(Some((super::Target::Stub, super::TrustProfile::Restricted)));
        let limits = BatchLimits {
            disk_growth_bytes: 1,
            ..BatchLimits::PRODUCTION
        };
        let error = batch::write_frozen_batch(
            &world.store,
            &BatchSpec {
                pin: &pin,
                language: "javascript",
                workspace: &world.root,
                dir: &dir,
                generation,
            },
            &limits,
            &mut Default::default(),
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("limit_exceeded: operation disk growth"),
            "{error:#}"
        );
    }

    #[test]
    fn frozen_batch_preflight_enforces_stage_budget_and_removes_stage_on_error() {
        let world = world(&js(&["a"]));
        let dir = frozen_dir(&world);
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(
            dir.join(batch::ARTIFACT_PATH),
            real_artifact(&["src/a.js"], false),
        )
        .unwrap();
        let pin = crate::scip_admission::pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let generation =
            super::generation(Some((super::Target::Stub, super::TrustProfile::Restricted)));
        let limits = BatchLimits {
            staging_bytes: 1,
            ..BatchLimits::PRODUCTION
        };
        let error = batch::write_frozen_batch(
            &world.store,
            &BatchSpec {
                pin: &pin,
                language: "javascript",
                workspace: &world.root,
                dir: &dir,
                generation,
            },
            &limits,
            &mut Default::default(),
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("limit_exceeded: staging bytes"),
            "{error:#}"
        );
        assert!(fs::read_dir(dir.join("batch")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("semantic-stage-")
        }));
    }
}
