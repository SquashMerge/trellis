//! Generated real-projection visit-bound differentials. See docs/rules.md, work bounds.
use super::{has_edge, snapshot};
use std::collections::BTreeSet;
use trellis::references::{self, Snapshot, VisitObservation};

fn sites(data: &serde_json::Value) -> BTreeSet<(String, String, usize, usize, String)> {
    let mut result = BTreeSet::new();
    for role in ["production", "tests"] {
        for edge in data[role].as_array().unwrap() {
            assert_eq!(edge["omittedSites"], 0, "{data}");
            assert_eq!(edge["count"], 1, "one consumer site per owner: {data}");
            assert_eq!(edge["sites"].as_array().unwrap().len(), 1, "{data}");
            let target = edge["target"]["path"].as_str().unwrap();
            for site in edge["sites"].as_array().unwrap() {
                result.insert((
                    role.to_owned(),
                    target.to_owned(),
                    site["range"]["start"].as_u64().unwrap() as usize,
                    site["range"]["end"].as_u64().unwrap() as usize,
                    site["relationKind"].as_str().unwrap().to_owned(),
                ));
            }
        }
    }
    result
}

fn run(
    input: &Snapshot,
    budget: usize,
    workers: usize,
) -> (serde_json::Value, Vec<VisitObservation>) {
    let (projection, counts) = references::project_with_visit_budget(
        input,
        &["use.js".into()],
        50,
        workers,
        budget,
        &mut || Ok(()),
    )
    .unwrap();
    (projection.data, counts)
}

fn target_counts(counts: &[VisitObservation], kind: &str, end: usize) -> Vec<usize> {
    counts
        .iter()
        .filter(|v| v.kind == kind && v.start == 0 && v.end == end)
        .map(|v| v.visits)
        .collect()
}

#[test]
fn generated_reference_visit_budget_differentials() {
    let cases = std::env::var("TRELLIS_VISIT_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24usize);
    let mut rng = 20261102u64;
    let mut random = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut totals = [0usize; 9];
    let mut scan_bounds = (usize::MAX, 0usize);
    let mut walk_bounds = (usize::MAX, 0usize);
    for case in 0..cases {
        let shape = case % 4;
        let reverse = random() % 2 == 0;
        let explicit = (case / 4) % 2 == 1;
        let filler = 100 + (random() as usize % 160);
        let before = random() % 2 == 0;
        let hazard = (case / 8) % 2 == 1;
        let left = if shape == 3 {
            "export { shared } from './origin.js';\n"
        } else {
            "export function shared() {}\n"
        };
        let right_export = match shape {
            0 => "export function other() {}\n",
            1 => "export function shared() {}\n",
            _ => "export { shared } from './origin.js';\n",
        };
        let padding = format!(
            "const padding = [{}];\n{}",
            "0,".repeat(filler),
            if hazard { "module;\n" } else { "" }
        );
        let right = if before {
            format!("{padding}{right_export}")
        } else {
            format!("{right_export}{padding}")
        };
        let mut barrel = "export function marker() {}\n".to_owned();
        for path in if reverse {
            ["right", "left"]
        } else {
            ["left", "right"]
        } {
            barrel.push_str(&format!("export * from './{path}.js';\n"));
        }
        if explicit {
            barrel.push_str("export { shared } from './origin.js';\n");
        }
        let input = snapshot(&[
            ("use.js", "export * from './barrel.js';\n"),
            ("barrel.js", &barrel),
            ("left.js", left),
            ("right.js", &right),
            ("origin.js", "export function shared() {}\n"),
        ]);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&right, None).unwrap();
        assert!(!tree.root_node().has_error());
        let end = tree.root_node().end_byte();
        let (healthy, measured) = run(&input, 1_000_000, 2);
        assert_eq!(healthy["partialFiles"].as_array().unwrap().len(), 0);
        let scans = target_counts(&measured, "scan", end);
        let walks = target_counts(&measured, "walk", end);
        assert!(
            !scans.is_empty() && !walks.is_empty(),
            "case {case} scans={scans:?} walks={walks:?}"
        );
        let scan = *scans.iter().max().unwrap();
        let walk = *walks.iter().max().unwrap();
        assert!(scan > 80 && walk > 80, "{scan} {walk}");
        scan_bounds = (scan_bounds.0.min(scan), scan_bounds.1.max(scan));
        walk_bounds = (walk_bounds.0.min(walk), walk_bounds.1.max(walk));
        let normal =
            references::project_with_workers(&input, &["use.js".into()], 50, 2, &mut || Ok(()))
                .unwrap()
                .data;
        assert_eq!(
            normal, healthy,
            "default path differs from default-budget hook, case {case}"
        );
        assert!(has_edge(&healthy, "use.js", "barrel.js"));
        if explicit {
            assert!(
                has_edge(&healthy, "use.js", "origin.js"),
                "explicit forwarding should select origin: {healthy}"
            );
            totals[8] += 1;
        }
        if !explicit && !hazard {
            if shape == 0 {
                assert!(has_edge(&healthy, "use.js", "left.js"), "{healthy}");
                totals[0] += 1;
            }
            if shape == 1 || shape == 2 {
                assert!(!has_edge(&healthy, "use.js", "left.js"), "{healthy}");
                totals[1] += 1;
            }
            if shape == 3 {
                assert!(has_edge(&healthy, "use.js", "origin.js"), "{healthy}");
                totals[2] += 1;
            }
        }
        if !explicit && hazard {
            assert!(
                !has_edge(&healthy, "use.js", "left.js"),
                "dynamic hazard should make star names unknown: {healthy}"
            );
            totals[6] += 1;
        }
        let budgets = [
            walk - 1,
            walk,
            walk + 1,
            scan - 1,
            scan,
            scan + 1,
            70 + (random() as usize % 60),
        ];
        for budget in budgets {
            let (partial, observed) = run(&input, budget, if case % 2 == 0 { 1 } else { 3 });
            let got = sites(&partial);
            let want = sites(&healthy);
            assert!(
                got.is_subset(&want),
                "case={case} shape={shape} reverse={reverse} explicit={explicit} before={before} filler={filler} budget={budget} scan={scan} walk={walk} gained={:?}\nhealthy={healthy}\npartial={partial}",
                got.difference(&want).collect::<Vec<_>>()
            );
            let partial_rows = partial["partialFiles"].as_array().unwrap();
            let is_partial = partial_rows
                .iter()
                .any(|row| row["path"] == "right.js" && row["reason"] == "visit_limit");
            assert_eq!(
                is_partial,
                budget < scan,
                "case {case} budget={budget} scan={scan}: {partial_rows:?}"
            );
            let actual_scans = target_counts(&observed, "scan", end);
            assert!(
                actual_scans.contains(&scan.min(budget + 1)),
                "case={case} budget={budget} {actual_scans:?}"
            );
            let actual_walks = target_counts(&observed, "walk", end);
            assert!(!actual_walks.is_empty(), "case={case} budget={budget}");
            assert_eq!(
                *actual_walks.iter().max().unwrap(),
                walk.min(budget + 1),
                "case={case} budget={budget} full_walk={walk} actual={actual_walks:?}"
            );
            if actual_walks.contains(&(budget + 1)) {
                totals[3] += 1;
            }
            if is_partial {
                totals[4] += 1;
            } else {
                totals[5] += 1;
            }
            if is_partial
                && !explicit
                && !hazard
                && shape == 0
                && !has_edge(&partial, "use.js", "left.js")
            {
                totals[7] += 1;
            }
        }
    }
    eprintln!(
        "visit cases={cases} unique={} collision={} diamond={} walk_exhaustions={} partial={} whole={} healthy_dynamic={} partial_suppressed_unique={} explicit={} full_scan_range={scan_bounds:?} full_walk_range={walk_bounds:?}",
        totals[0],
        totals[1],
        totals[2],
        totals[3],
        totals[4],
        totals[5],
        totals[6],
        totals[7],
        totals[8]
    );
    assert!(totals.iter().all(|n| *n > 0), "coverage: {totals:?}");
}

#[test]
fn visit_override_is_confined_to_one_projection() {
    let right = format!(
        "export function other() {{}}\nconst padding = [{}];\n",
        "0,".repeat(180)
    );
    let input = snapshot(&[
        ("use.js", "export * from './barrel.js';\n"),
        (
            "barrel.js",
            "export * from './left.js';\nexport * from './right.js';\n",
        ),
        ("left.js", "export function shared() {}\n"),
        ("right.js", &right),
    ]);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    std::thread::scope(|scope| {
        let input_ref = &input;
        let worker = scope.spawn(move || {
            let mut first = true;
            let (low, events) = references::project_with_visit_budget(
                input_ref,
                &["use.js".into()],
                50,
                3,
                80,
                &mut move || {
                    if first {
                        first = false;
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert!(
                low.data["partialFiles"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["path"] == "right.js" && p["reason"] == "visit_limit")
            );
            assert!(events.iter().any(|e| e.kind == "walk" && e.visits == 81));
            let after =
                references::project_with_workers(input_ref, &["use.js".into()], 50, 1, &mut || {
                    Ok(())
                })
                .unwrap();
            assert!(after.data["partialFiles"].as_array().unwrap().is_empty());
            assert!(has_edge(&after.data, "use.js", "left.js"));
        });
        entered_rx.recv().unwrap();
        let normal =
            references::project_with_workers(&input, &["use.js".into()], 50, 3, &mut || Ok(()));
        release_tx.send(()).unwrap();
        let normal = normal.unwrap();
        assert!(normal.data["partialFiles"].as_array().unwrap().is_empty());
        assert!(has_edge(&normal.data, "use.js", "left.js"));
        worker.join().unwrap();
    });
}
