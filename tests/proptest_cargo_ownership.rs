//! Discovery test: small Cargo crate graphs checked against a fixed-point oracle.
//! `PROPTEST_CASES` controls the run length; proptest persists shrinking seeds.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use serde_json::json;
use trellis::scip_raw::cargo_targets::{CargoTarget, CargoTargetsBuilder};

const UNREAD: [&str; 2] = [
    "// Trellis did not read this file: it is larger than --max-file-bytes.\n\u{1}\n",
    "// Trellis did not read this file: it is not UTF-8.\n\u{1}\n",
];

#[derive(Clone, Debug)]
enum Edge {
    Mod(&'static str),
    Path(String),
}

#[derive(Clone, Debug, Default)]
struct File {
    edges: Vec<Edge>,
    unread: Option<usize>,
}

#[derive(Clone, Debug)]
struct Graph {
    files: BTreeMap<String, File>,
    targets: Vec<(usize, CargoTarget)>,
    versions: [bool; 2],
    duplicate_roots: usize,
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

// Resolve relative source paths by manipulating components, independently of the builder.
fn resolve(base: &str, relative: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop().expect("generated paths stay in workspace");
            }
            _ => parts.push(part),
        }
    }
    parts.join("/")
}

fn relative(from: &str, to: &str) -> String {
    let a: Vec<_> = parent(from).split('/').filter(|p| !p.is_empty()).collect();
    let b: Vec<_> = to.split('/').collect();
    let common = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    std::iter::repeat_n("..", a.len() - common)
        .chain(b[common..].iter().copied())
        .collect::<Vec<_>>()
        .join("/")
}

fn target(package: usize, kind: &str, root: String, version: bool) -> CargoTarget {
    CargoTarget {
        kind: kind.into(),
        root,
        package: format!("pkg{package}"),
        version: version.then(|| format!("0.{}.0", package + 1)),
    }
}

// Scenarios deliberately retain their interaction when proptest shrinks the noise.
// 0 shared module, 1 shared root, 2 ambiguous module, 3 missing module,
// 4 reached unread, 5 unreachable unread, 6 general graph, 7 cross-package
// sharing, 8 partial reach sharing, 9 identical target instances on one root.
fn graph(scenario: u8, choices: Vec<u8>, versions: [bool; 2], skip: usize) -> Graph {
    let mut files = BTreeMap::new();
    for p in 0..2 {
        for name in ["lib.rs", "main.rs", "a.rs", "a/b.rs", "b.rs", "orphan.rs"] {
            files.insert(format!("p{p}/src/{name}"), File::default());
        }
    }
    files.insert("shared/x.rs".into(), File::default());
    let paths: Vec<String> = files.keys().cloned().collect();
    for (i, path) in paths.iter().enumerate() {
        if path.ends_with("orphan.rs") {
            continue;
        }
        let code = choices[i];
        let edge = match code % 10 {
            0..=3 => None,
            4 => Some(Edge::Mod("a")),
            5 => Some(Edge::Mod("b")),
            _ => Some(Edge::Path(
                paths[(usize::from(code) + i) % paths.len()].clone(),
            )),
        };
        if let Some(edge) = edge {
            files.get_mut(path).unwrap().edges.push(edge);
        }
    }
    // Deterministic witnesses are insulated from unrelated generated poison.
    if scenario != 6 {
        for file in files.values_mut() {
            file.edges.clear();
        }
        files
            .get_mut("p1/src/lib.rs")
            .unwrap()
            .edges
            .push(Edge::Mod("b"));
        // Optional edges stay valid and create diamonds, cross-package ownership,
        // and cycles without erasing the scenario's defining interaction.
        if choices[0] % 2 == 1 && scenario != 4 {
            files
                .get_mut("p1/src/main.rs")
                .unwrap()
                .edges
                .push(Edge::Path("shared/x.rs".into()));
        }
        if choices[1] % 2 == 1 && scenario != 4 {
            files
                .get_mut("p0/src/a.rs")
                .unwrap()
                .edges
                .push(Edge::Path("shared/x.rs".into()));
        }
    }
    match scenario {
        0 => {
            for root in ["p0/src/lib.rs", "p0/src/main.rs"] {
                files.get_mut(root).unwrap().edges.push(Edge::Mod("a"));
            }
        }
        1 => files
            .get_mut("p0/src/lib.rs")
            .unwrap()
            .edges
            .push(Edge::Mod("a")),
        2 => {
            files.insert("p0/src/a/mod.rs".into(), File::default());
            files
                .get_mut("p0/src/lib.rs")
                .unwrap()
                .edges
                .push(Edge::Mod("a"));
        }
        3 => files
            .get_mut("p0/src/lib.rs")
            .unwrap()
            .edges
            .push(Edge::Mod("missing")),
        4 => {
            files
                .get_mut("p0/src/lib.rs")
                .unwrap()
                .edges
                .push(Edge::Path("shared/x.rs".into()));
            files.get_mut("shared/x.rs").unwrap().unread = Some(skip);
        }
        5 => files.get_mut("p0/src/orphan.rs").unwrap().unread = Some(skip),
        7 => {
            for root in ["p0/src/lib.rs", "p1/src/lib.rs"] {
                files
                    .get_mut(root)
                    .unwrap()
                    .edges
                    .push(Edge::Path("shared/x.rs".into()));
            }
        }
        8 => {
            for root in ["p0/src/lib.rs", "p1/src/lib.rs"] {
                files
                    .get_mut(root)
                    .unwrap()
                    .edges
                    .push(Edge::Path("shared/x.rs".into()));
            }
            files
                .get_mut("p0/src/main.rs")
                .unwrap()
                .edges
                .push(Edge::Mod("missing"));
        }
        9 => files
            .get_mut("p0/src/lib.rs")
            .unwrap()
            .edges
            .push(Edge::Mod("a")),
        _ => {}
    }
    let mut targets = Vec::new();
    for (p, version) in versions.into_iter().enumerate() {
        targets.push((p, target(p, "lib", format!("p{p}/src/lib.rs"), version)));
        targets.push((p, target(p, "bin", format!("p{p}/src/main.rs"), version)));
    }
    let duplicate_roots = match scenario {
        1 => 1,
        9 => 2,
        _ => 0,
    };
    for _ in 0..duplicate_roots {
        targets.push((0, target(0, "bin", "p0/src/lib.rs".into(), versions[0])));
    }
    Graph {
        files,
        targets,
        versions,
        duplicate_roots,
    }
}

fn render(graph: &Graph) -> CargoTargetsBuilder {
    let mut config = vec![
        json!({"path": "Cargo.toml", "type": "present", "bytes": hex::encode("[workspace]\n")}),
    ];
    for p in 0..2 {
        let version = if graph.versions[p] {
            format!("version = '0.{}.0'\n", p + 1)
        } else {
            String::new()
        };
        let extra: String = if p == 0 {
            (0..graph.duplicate_roots)
                .map(|n| format!("[[bin]]\nname = 'duplicate{n}'\npath = 'src/lib.rs'\n"))
                .collect()
        } else {
            String::new()
        };
        let manifest = format!("[package]\nname = 'pkg{p}'\n{version}edition = '2021'\n{extra}");
        config.push(json!({"path": format!("p{p}/Cargo.toml"), "type": "present", "bytes": hex::encode(manifest)}));
    }
    let mut builder = CargoTargetsBuilder::from_native_config(&config);
    for (path, file) in &graph.files {
        let source = if let Some(skip) = file.unread {
            UNREAD[skip].to_owned()
        } else {
            file.edges
                .iter()
                .enumerate()
                .map(|(i, edge)| match edge {
                    Edge::Mod(name) => format!("mod {name};\n"),
                    Edge::Path(destination) => format!(
                        "#[path = \"{}\"] mod chosen{i};\n",
                        relative(path, destination)
                    ),
                })
                .collect()
        };
        builder.add_document(path, source.as_bytes());
    }
    builder
}

// Repeated full scans to a fixed point, separately for each target instance. The
// state includes the module directory rule, since one file can be reached both ways.
fn expected(graph: &Graph) -> BTreeMap<String, Option<CargoTarget>> {
    let mut visits = Vec::new();
    let mut poisoned = [false; 2];
    let mut workspace_unread = false;
    for (package, target) in &graph.targets {
        let mut states = BTreeSet::from([(target.root.clone(), true)]);
        loop {
            let previous = states.clone();
            for (path, owns_directory) in &previous {
                let file = &graph.files[path];
                if file.unread.is_some() {
                    workspace_unread = true;
                    continue;
                }
                for edge in &file.edges {
                    match edge {
                        Edge::Path(dest) => {
                            if graph.files.contains_key(dest) {
                                states.insert((dest.clone(), true));
                            } else {
                                poisoned[*package] = true;
                            }
                        }
                        Edge::Mod(name) => {
                            let base = if *owns_directory {
                                parent(path)
                            } else {
                                path.strip_suffix(".rs").unwrap()
                            };
                            let direct = resolve(base, &format!("{name}.rs"));
                            let nested = resolve(base, &format!("{name}/mod.rs"));
                            let found: Vec<_> = [direct, nested]
                                .into_iter()
                                .filter(|p| graph.files.contains_key(p))
                                .collect();
                            if found.len() != 1 {
                                poisoned[*package] = true;
                            } else {
                                states.insert((found[0].clone(), found[0].ends_with("/mod.rs")));
                            }
                        }
                    }
                }
            }
            if states == previous {
                break;
            }
        }
        visits.push(states.into_iter().map(|(p, _)| p).collect::<BTreeSet<_>>());
    }
    graph
        .files
        .keys()
        .map(|path| {
            let owners: Vec<_> = visits
                .iter()
                .enumerate()
                .filter(|(_, seen)| seen.contains(path))
                .map(|(i, _)| i)
                .collect();
            let blocked = workspace_unread
                || (0..2).any(|p| poisoned[p] && path.starts_with(&format!("p{p}/")));
            let result = if !blocked && owners.len() == 1 {
                let candidate = &graph.targets[owners[0]].1;
                (graph
                    .targets
                    .iter()
                    .filter(|(_, t)| t.root == candidate.root)
                    .count()
                    == 1)
                    .then(|| candidate.clone())
            } else {
                None
            };
            (path.clone(), result)
        })
        .collect()
}

fn check(graph: &Graph) -> Result<(), TestCaseError> {
    let expected = expected(graph);
    let actual = render(graph).finish();
    for (path, owner) in expected {
        prop_assert_eq!(
            actual.target_of(&path),
            owner.as_ref(),
            "path={}, graph={:#?}",
            path,
            graph
        );
    }
    Ok(())
}

#[test]
fn shared_module_witness() {
    let graph = graph(0, vec![0; 13], [true, false], 0);
    check(&graph).unwrap();
}

#[test]
fn scenario_controls_are_non_vacuous() {
    for scenario in 0..=9 {
        if scenario == 6 {
            continue;
        }
        let graph = graph(scenario, vec![0; 13], [true, false], 1);
        let owners = render(&graph).finish();
        match scenario {
            0 | 1 | 9 => assert_eq!(owners.target_of("p0/src/a.rs"), None),
            2 | 3 => {
                assert_eq!(owners.target_of("p0/src/main.rs"), None);
                assert_eq!(owners.target_of("p1/src/b.rs"), Some(&graph.targets[2].1));
            }
            4 => assert!(
                graph
                    .files
                    .keys()
                    .all(|path| owners.target_of(path).is_none())
            ),
            5 => assert_eq!(owners.target_of("p1/src/b.rs"), Some(&graph.targets[2].1)),
            7 | 8 => {
                assert_eq!(owners.target_of("shared/x.rs"), None);
                assert_eq!(owners.target_of("p1/src/b.rs"), Some(&graph.targets[2].1));
            }
            _ => unreachable!(),
        }
        check(&graph).unwrap();
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(128), ..ProptestConfig::default() })]
    #[test]
    fn cargo_target_graphs_match_fixed_point(
        scenario in prop_oneof![4 => Just(0u8), 2 => Just(1), 2 => Just(2), 2 => Just(3), 2 => Just(4), 2 => Just(5), 8 => Just(6), 3 => Just(7), 3 => Just(8), 2 => Just(9)],
        choices in prop::collection::vec(any::<u8>(), 13),
        versions in any::<[bool; 2]>(),
        skip in 0usize..2,
    ) {
        check(&graph(scenario, choices, versions, skip))?;
    }
}
