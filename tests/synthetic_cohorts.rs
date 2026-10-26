use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::Command,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_with_native},
    model::CancelFlag,
    store::topology::WorkspaceIdentity,
};

#[derive(Deserialize)]
struct Manifest {
    version: u32,
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestFile {
    path: String,
    source_bytes: usize,
}

#[test]
fn large_cohort_density_uses_production_native_declarations() {
    let temp = tempfile::tempdir().unwrap();
    let corpus = temp.path().join("corpus");
    let generated = Command::new("node")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/synthetic-cohorts/generate.mjs"))
        .arg("--out")
        .arg(&corpus)
        .output()
        .expect("run the synthetic cohort generator with Node.js");
    assert!(
        generated.status.success(),
        "generator failed: {}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(corpus.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest.version, 2);
    let large: Vec<_> = manifest
        .files
        .iter()
        .filter(|file| file.path.starts_with("large/"))
        .collect();
    assert_eq!(large.len(), 10_000);
    assert_eq!(
        large.iter().map(|file| file.source_bytes).sum::<usize>(),
        134_217_728
    );

    let mut file_counts = BTreeMap::<&str, usize>::new();
    let mut ordinary_counts = BTreeMap::<&str, usize>::new();
    let mut ordinary_samples = BTreeMap::new();
    let mut roots = Vec::new();
    for file in &large {
        let language = file.path.split('/').nth(1).unwrap();
        *file_counts.entry(language).or_default() += 1;
        if language == "rust"
            && matches!(
                Path::new(&file.path).file_name().unwrap().to_str().unwrap(),
                "lib.rs" | "mod.rs"
            )
        {
            roots.push(*file);
        } else {
            *ordinary_counts.entry(language).or_default() += 1;
            ordinary_samples.entry(language).or_insert(*file);
        }
    }
    assert_eq!(
        file_counts,
        BTreeMap::from([
            ("java", 2500),
            ("javascript", 2500),
            ("python", 2500),
            ("rust", 2500),
        ])
    );
    assert_eq!(roots.len(), 40);

    let sample = tempfile::tempdir().unwrap();
    for file in ordinary_samples
        .values()
        .copied()
        .chain(roots.iter().copied())
    {
        let destination = sample
            .path()
            .join(file.path.strip_prefix("large/").unwrap());
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(corpus.join(&file.path), destination).unwrap();
    }
    let identity = WorkspaceIdentity::discover(Some(sample.path()), sample.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let (graph, native) = index_workspace_with_native(
        &IndexOptions::new(sample.path().to_owned()),
        &identity.record_id,
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(graph.files.len(), ordinary_samples.len() + roots.len());
    assert_eq!(graph.stats.files, graph.files.len());
    assert_eq!(graph.stats.parse_error_files, 0);
    assert!(graph.diagnostics.is_empty(), "{:?}", graph.diagnostics);
    assert_eq!(graph.stats.symbols, graph.nodes.len());
    assert_eq!(graph.nodes.len(), native.declarations.len());
    assert_eq!(native.coverage.len(), graph.files.len());
    assert!(
        native
            .coverage
            .iter()
            .all(|coverage| { coverage.state == "complete" && coverage.diagnostic.is_none() })
    );

    let mut native_file_counts: BTreeMap<_, usize> = graph
        .files
        .iter()
        .map(|file| (file.path.as_str(), 0))
        .collect();
    for declaration in &native.declarations {
        *native_file_counts
            .get_mut(declaration.document.path.as_str())
            .unwrap() += 1;
    }
    // Ordinary files share declaration structure; seeded names and numeric body
    // terms do not add nodes. Count every Rust module root instead of sampling it.
    let mut total_nodes = 0;
    for (language, ordinary) in ordinary_samples {
        let ordinary_nodes = native_file_counts[ordinary.path.strip_prefix("large/").unwrap()];
        assert!(ordinary_nodes > 0, "no native declarations for {language}");
        let root_nodes: usize = roots
            .iter()
            .filter(|root| root.path.split('/').nth(1) == Some(language))
            .map(|root| native_file_counts[root.path.strip_prefix("large/").unwrap()])
            .sum();
        let nodes = ordinary_nodes * ordinary_counts[language] + root_nodes;
        total_nodes += nodes;
        eprintln!(
            "LARGE {language}: {nodes} nodes / {} files = {:.4}",
            file_counts[language],
            nodes as f64 / file_counts[language] as f64
        );
    }
    let mean = total_nodes as f64 / large.len() as f64;
    assert!(
        (115 * large.len()..=120 * large.len()).contains(&total_nodes),
        "LARGE mean {mean:.4} is outside 115–120 ({total_nodes} native nodes)"
    );
}
