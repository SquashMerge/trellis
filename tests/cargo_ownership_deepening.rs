//! Small generated histories; compare real target assignment with a graph oracle.
use std::collections::BTreeSet;

use serde_json::json;
use trellis::scip_raw::cargo_targets::{CargoTarget, CargoTargetsBuilder};

const FILES: [&str; 7] = [
    "p0/src/lib.rs",
    "p0/src/main.rs",
    "p0/src/a/x.rs",
    "p0/src/b/x.rs",
    "p0/src/shared.rs",
    "p0/src/leaf.rs",
    "p1/src/lib.rs",
];
const SKIPPED: &str =
    "\x01 Trellis did not read this file: it is larger than the config input cap.\n";

#[derive(Clone, Copy, Debug)]
enum Manifest {
    Valid,
    Invalid,
    Skipped,
}

#[derive(Clone, Copy, Debug)]
enum Declaration {
    None,
    InlineA,
    InlineB,
    InlinePath,
    Include,
    InlineInclude,
    Unsupported,
}

#[derive(Clone, Debug)]
struct State {
    manifest: Manifest,
    declarations: [Declaration; 2],
    chain: bool,
    leaf_direct: [bool; 2],
    unread: u8,
}

fn declaration(d: Declaration) -> &'static str {
    match d {
        Declaration::None => "",
        Declaration::InlineA => "mod a { mod x; }\n",
        Declaration::InlineB => "mod b { mod x; }\n",
        Declaration::InlinePath => "mod a { #[path = \"../shared.rs\"] mod x; }\n",
        Declaration::Include => "include!(\"shared.rs\");\n",
        Declaration::InlineInclude => "mod a { include!(\"shared.rs\"); }\n",
        Declaration::Unsupported => "#[path = \"elsewhere\"] mod a { mod x; }\n",
    }
}

fn target(i: usize) -> CargoTarget {
    let (kind, root, package, version) = match i {
        0 => ("lib", FILES[0], "p0", "1.0.0"),
        1 => ("bin", FILES[1], "p0", "1.0.0"),
        _ => ("lib", FILES[6], "p1", "2.0.0"),
    };
    CargoTarget {
        kind: kind.into(),
        root: root.into(),
        package: package.into(),
        version: Some(version.into()),
    }
}

// Generated declarations directly describe graph edges: no parser or production
// path resolver is used by this repeated-scan closure.
fn expected(s: &State) -> Vec<Option<CargoTarget>> {
    let mut reachable: Vec<BTreeSet<usize>> = (0..3)
        .map(|i| BTreeSet::from([if i == 2 { 6 } else { i }]))
        .collect();
    for (i, reached) in reachable.iter_mut().enumerate().take(2) {
        let edge = match s.declarations[i] {
            Declaration::InlineA => Some(2),
            Declaration::InlineB => Some(3),
            Declaration::InlinePath | Declaration::Include | Declaration::InlineInclude => Some(4),
            Declaration::None | Declaration::Unsupported => None,
        };
        if let Some(edge) = edge {
            reached.insert(edge);
        }
        if s.leaf_direct[i] {
            reached.insert(5);
        }
        loop {
            let old = reached.clone();
            if s.chain && old.contains(&4) {
                reached.insert(5);
            }
            if *reached == old {
                break;
            }
        }
    }
    let reached_unread = s.unread != 0
        && matches!(s.manifest, Manifest::Valid)
        && reachable.iter().take(2).any(|r| r.contains(&4));
    (0..FILES.len())
        .map(|file| {
            if matches!(s.manifest, Manifest::Skipped) {
                return None;
            }
            if reached_unread {
                return None;
            }
            if file < 6
                && (matches!(s.manifest, Manifest::Invalid)
                    || s.declarations
                        .iter()
                        .any(|d| matches!(d, Declaration::Unsupported)))
            {
                return None;
            }
            let owners: Vec<_> = reachable
                .iter()
                .enumerate()
                .filter(|(_, paths)| paths.contains(&file))
                .map(|(i, _)| i)
                .collect();
            match owners.as_slice() {
                [i] if *i == 2 || matches!(s.manifest, Manifest::Valid) => Some(target(*i)),
                _ => None,
            }
        })
        .collect()
}

fn actual(s: &State) -> Vec<Option<CargoTarget>> {
    let m0 = match s.manifest {
        Manifest::Valid => "[package]\nname='p0'\nversion='1.0.0'\nedition='2021'\n",
        Manifest::Invalid => "[package",
        Manifest::Skipped => SKIPPED,
    };
    let manifests = [
        ("Cargo.toml", "[workspace]\n"),
        ("p0/Cargo.toml", m0),
        (
            "p1/Cargo.toml",
            "[package]\nname='p1'\nversion='2.0.0'\nedition='2021'\n",
        ),
    ];
    let config: Vec<_> = manifests
        .iter()
        .map(|(path, text)| json!({"path": path, "type": "present", "bytes": hex::encode(text)}))
        .collect();
    let mut builder = CargoTargetsBuilder::from_native_config(&config);
    for (i, file) in FILES.iter().enumerate() {
        let source = if i < 2 {
            format!(
                "{}{}",
                declaration(s.declarations[i]),
                if s.leaf_direct[i] {
                    "include!(\"leaf.rs\");\n"
                } else {
                    ""
                }
            )
        } else if i == 4 && s.unread != 0 {
            format!(
                "// Trellis did not read this file: it is {}.\n\x01\n",
                if s.unread == 1 {
                    "larger than --max-file-bytes"
                } else {
                    "not UTF-8"
                }
            )
        } else if i == 4 && s.chain {
            "include!(\"leaf.rs\");\n".to_string()
        } else {
            String::new()
        };
        builder.add_document(file, source.as_bytes());
    }
    let targets = builder.finish();
    FILES
        .iter()
        .map(|path| targets.target_of(path).cloned())
        .collect()
}

fn check(s: &State, trace: &[u8]) {
    assert_eq!(actual(s), expected(s), "state={s:?}, trace={trace:?}");
}

fn set(s: &mut State, op: u8) {
    match op % 8 {
        0 | 1 => {
            let i = usize::from(op % 8);
            s.declarations[i] = match (op / 8) % 7 {
                0 => Declaration::None,
                1 => Declaration::InlineA,
                2 => Declaration::InlineB,
                3 => Declaration::InlinePath,
                4 => Declaration::Include,
                5 => Declaration::InlineInclude,
                _ => Declaration::Unsupported,
            };
        }
        2 => {
            s.manifest = match (op / 8) % 3 {
                0 => Manifest::Valid,
                1 => Manifest::Invalid,
                _ => Manifest::Skipped,
            }
        }
        3 => s.chain = !s.chain,
        4 => s.leaf_direct[0] = !s.leaf_direct[0],
        5 => s.leaf_direct[1] = !s.leaf_direct[1],
        _ => s.unread = (op / 8) % 3,
    }
}

fn initial() -> State {
    State {
        manifest: Manifest::Valid,
        declarations: [Declaration::None; 2],
        chain: false,
        leaf_direct: [false; 2],
        unread: 0,
    }
}

#[test]
fn positive_collision_and_recovery_controls() {
    let mut s = initial();
    s.declarations = [Declaration::InlineA, Declaration::InlineB];
    let a = actual(&s);
    assert_eq!(a[2], Some(target(0)));
    assert_eq!(a[3], Some(target(1)));
    assert_eq!(a[6], Some(target(2)));
    check(&s, &[]);
    s.declarations = [Declaration::InlineInclude, Declaration::InlinePath];
    s.chain = true;
    let a = actual(&s);
    assert_eq!(a[4], None);
    assert_eq!(a[5], None);
    assert_eq!(a[6], Some(target(2)));
    check(&s, &[]);
    s.manifest = Manifest::Skipped;
    assert!(actual(&s).iter().all(Option::is_none));
    check(&s, &[]);
    s.manifest = Manifest::Valid;
    s.declarations[1] = Declaration::None;
    assert_eq!(actual(&s)[5], Some(target(0)));
    check(&s, &[]);
    s.manifest = Manifest::Invalid;
    assert_eq!(actual(&s)[0], None);
    assert_eq!(actual(&s)[6], Some(target(2)));
    check(&s, &[]);
    s.manifest = Manifest::Valid;
    s.unread = 1;
    assert!(actual(&s).iter().all(Option::is_none));
    check(&s, &[]);
    s.declarations = [Declaration::InlineA, Declaration::None];
    assert_eq!(actual(&s)[2], Some(target(0)));
    assert_eq!(actual(&s)[6], Some(target(2)));
    check(&s, &[]);
}

#[test]
fn exhaustive_small_layout_controls() {
    let declarations = [
        Declaration::None,
        Declaration::InlineA,
        Declaration::InlineB,
        Declaration::InlinePath,
        Declaration::Include,
        Declaration::InlineInclude,
        Declaration::Unsupported,
    ];
    for a in declarations {
        for b in declarations {
            for manifest in [Manifest::Valid, Manifest::Invalid, Manifest::Skipped] {
                for chain in [false, true] {
                    for unread in 0..3 {
                        let s = State {
                            declarations: [a, b],
                            manifest,
                            chain,
                            unread,
                            leaf_direct: [false; 2],
                        };
                        check(&s, &[]);
                    }
                }
            }
        }
    }
}

#[test]
fn generated_edit_histories() {
    let cases = std::env::var("TRELLIS_CARGO_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128usize);
    let mut rng = std::env::var("TRELLIS_CARGO_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20261102u64);
    let mut manifest_counts = [0usize; 3];
    let mut declaration_counts = [0usize; 7];
    let mut shared_collisions = 0usize;
    let mut reached_unread = 0usize;
    for _ in 0..cases {
        let mut s = initial();
        let mut trace = Vec::new();
        check(&s, &trace);
        for _ in 0..24 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let op = rng as u8;
            trace.push(op);
            set(&mut s, op);
            check(&s, &trace);
            manifest_counts[match s.manifest {
                Manifest::Valid => 0,
                Manifest::Invalid => 1,
                Manifest::Skipped => 2,
            }] += 1;
            for d in s.declarations {
                declaration_counts[match d {
                    Declaration::None => 0,
                    Declaration::InlineA => 1,
                    Declaration::InlineB => 2,
                    Declaration::InlinePath => 3,
                    Declaration::Include => 4,
                    Declaration::InlineInclude => 5,
                    Declaration::Unsupported => 6,
                }] += 1;
            }
            let count = s
                .declarations
                .iter()
                .filter(|d| {
                    matches!(
                        d,
                        Declaration::InlinePath | Declaration::Include | Declaration::InlineInclude
                    )
                })
                .count();
            if matches!(s.manifest, Manifest::Valid) && count == 2 {
                shared_collisions += 1;
            }
            if matches!(s.manifest, Manifest::Valid) && s.unread != 0 && count != 0 {
                reached_unread += 1;
            }
        }
    }
    println!(
        "generated observations={} manifests(valid,invalid,skipped)={manifest_counts:?} declarations={declaration_counts:?} shared_collisions={shared_collisions} reached_unread={reached_unread}",
        cases * 24
    );
}
