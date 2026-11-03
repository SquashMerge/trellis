//! Structural projection model; docs/rules.md "What a reference means".
//! The rendered modules are synthetic and are never executed.
use super::{has_edge, snapshot};
use proptest::prelude::*;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use trellis::references;

#[derive(Clone, Copy, Debug)]
enum Route {
    Absent,
    Own,
    Origin,
    OriginOther,
}

#[derive(Clone, Debug)]
enum Op {
    Left(u8),
    Right(u8),
    Explicit(bool),
    Reverse(bool),
}

fn route(value: u8) -> Route {
    match value % 4 {
        0 => Route::Absent,
        1 => Route::Own,
        2 => Route::Origin,
        _ => Route::OriginOther,
    }
}

// Model declarations as (document, declaration name), not module routes.
// Explicit forwarding settles the name, otherwise distinct surviving identities
// collide; repeated routes to origin's same declaration deduplicate.
fn expected(left: Route, right: Route, explicit: bool) -> BTreeSet<&'static str> {
    let mut targets = BTreeSet::from(["barrel"]);
    let mut candidates = BTreeSet::new();
    for (side, r) in [("left", left), ("right", right)] {
        match r {
            Route::Absent => (),
            Route::Own => {
                candidates.insert((side, "shared"));
            }
            Route::Origin => {
                candidates.insert(("origin", "original"));
            }
            Route::OriginOther => {
                candidates.insert(("origin", "second"));
            }
        }
    }
    if explicit {
        targets.insert("origin");
    } else if candidates.len() == 1 {
        targets.extend(candidates.into_iter().map(|(owner, _)| owner));
    }
    targets
}

fn check(language: &str, left: Route, right: Route, explicit: bool, reverse: bool) {
    let js = language == "js";
    let extension = if js { "js" } else { "py" };
    let branch = |r: Route| match (js, r) {
        (true, Route::Absent) => "export function other() {}\n",
        (true, Route::Own) => "export function shared() {}\n",
        (true, Route::Origin) => "export { original as shared } from './origin.js';\n",
        (true, Route::OriginOther) => "export { second as shared } from './origin.js';\n",
        (false, Route::Absent) => "def other():\n    pass\n",
        (false, Route::Own) => "def shared():\n    pass\n",
        (false, Route::Origin) => "from origin import original as shared\n",
        (false, Route::OriginOther) => "from origin import second as shared\n",
    };
    let mut barrel = if js {
        "export function marker() {}\n".to_owned()
    } else {
        "def marker():\n    pass\n".to_owned()
    };
    for side in if reverse {
        ["right", "left"]
    } else {
        ["left", "right"]
    } {
        if js {
            barrel.push_str(&format!("export * from './{side}.js';\n"));
        } else {
            barrel.push_str(&format!("from {side} import *\n"));
        }
    }
    if explicit {
        barrel.push_str(if js {
            "export { original as shared } from './origin.js';\n"
        } else {
            "from origin import original as shared\n"
        });
    }
    let use_path = format!("use.{extension}");
    let barrel_path = format!("barrel.{extension}");
    let left_path = format!("left.{extension}");
    let right_path = format!("right.{extension}");
    let origin_path = format!("origin.{extension}");
    let source = if js {
        "export * from './barrel.js';\n"
    } else {
        "from barrel import *\n"
    };
    let origin = if js {
        "export function original() {}\nexport function second() {}\n"
    } else {
        "def original():\n    pass\ndef second():\n    pass\n"
    };
    let files = [
        (&*use_path, source),
        (&*barrel_path, barrel.as_str()),
        (&*left_path, branch(left)),
        (&*right_path, branch(right)),
        (&*origin_path, origin),
    ];
    let input = snapshot(&files);
    let data = references::project(&input, std::slice::from_ref(&use_path), 20, &mut || Ok(()))
        .unwrap()
        .data;
    // `other` is independently exported by an absent branch; include its owner.
    let mut want = expected(left, right, explicit);
    if matches!(left, Route::Absent) && !matches!(right, Route::Absent) {
        want.insert("left");
    }
    if matches!(right, Route::Absent) && !matches!(left, Route::Absent) {
        want.insert("right");
    }
    let got: BTreeSet<_> = ["barrel", "left", "right", "origin"]
        .into_iter()
        .filter(|p| has_edge(&data, &use_path, &format!("{p}.{extension}")))
        .collect();
    assert_eq!(
        got, want,
        "{language} left={left:?} right={right:?} explicit={explicit} reverse={reverse}\n{barrel}\n{data}"
    );
    for pair in data["production"].as_array().unwrap() {
        assert_eq!(pair["source"]["path"], use_path, "{data}");
        let target = pair["target"]["path"].as_str().unwrap();
        assert!(
            [&barrel_path, &left_path, &right_path, &origin_path]
                .iter()
                .any(|path| path.as_str() == target),
            "unexpected target: {data}"
        );
        assert_eq!(pair["count"], 1, "one consumer site per owner: {data}");
        assert_eq!(pair["omittedSites"], 0, "{data}");
    }
}

fn replay(ops: &[Op]) {
    let (mut left, mut right, mut explicit, mut reverse) =
        (Route::Origin, Route::Origin, false, false);
    for op in ops {
        match *op {
            Op::Left(v) => left = route(v),
            Op::Right(v) => right = route(v),
            Op::Explicit(v) => explicit = v,
            Op::Reverse(v) => reverse = v,
        }
        for language in ["js", "py"] {
            check(language, left, right, explicit, reverse);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_RESOLUTION_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(24), ..ProptestConfig::default() })]
    #[test]
    fn generated_module_histories(ops in prop::collection::vec(prop_oneof![ (0u8..4).prop_map(Op::Left), (0u8..4).prop_map(Op::Right), any::<bool>().prop_map(Op::Explicit), any::<bool>().prop_map(Op::Reverse)], 1..9)) {
        replay(&ops);
        static HISTORIES: AtomicUsize = AtomicUsize::new(0);
        static STEPS: AtomicUsize = AtomicUsize::new(0);
        let steps = STEPS.fetch_add(ops.len(), Ordering::Relaxed) + ops.len();
        let histories = HISTORIES.fetch_add(1, Ordering::Relaxed) + 1;
        if histories.is_multiple_of(128) {
            eprintln!("resolution-model histories={histories} steps={steps}");
        }
    }
}

#[test]
fn all_small_layouts() {
    for left in 0..4 {
        for right in 0..4 {
            for explicit in [false, true] {
                for reverse in [false, true] {
                    for language in ["js", "py"] {
                        check(language, route(left), route(right), explicit, reverse);
                    }
                }
            }
        }
    }
}

#[test]
fn collision_and_diamond_controls() {
    replay(&[
        Op::Left(1),
        Op::Right(1),
        Op::Explicit(true),
        Op::Explicit(false),
        Op::Left(2),
        Op::Right(2),
        Op::Reverse(true),
    ]);
}
