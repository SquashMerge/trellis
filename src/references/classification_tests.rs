use super::parallel_tests::snapshot_of;
use super::*;

fn edge<'a>(data: &'a Value, source: &str, target: &str) -> &'a Value {
    data["production"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["source"]["path"] == source && e["target"]["path"] == target)
        .unwrap()
}

/// R1 and R2: a structural callee path is a call, and a site names its owner.
#[test]
fn structural_callee_paths_are_calls() {
    let snapshot = snapshot_of(&[
        (
            "src/lib.rs",
            "mod high;\nuse crate::high::T;\nfn run(_: crate::high::T) { crate::high::f(); }\n",
        ),
        ("src/high.rs", "pub struct T;\npub fn f() -> u8 { 0 }\n"),
    ]);
    let data = project(&snapshot, &[], 20, &mut || Ok(())).unwrap().data;
    let pair = edge(&data, "src/lib.rs", "src/high.rs");
    let sites = pair["sites"].as_array().unwrap();
    let kind = |spelling: &str, line: u64| {
        sites
            .iter()
            .find(|s| s["spelling"] == spelling && s["lines"]["start"] == line)
            .unwrap_or_else(|| panic!("{spelling} on line {line}"))
    };
    assert_eq!(kind("crate::high::T", 2)["relationKind"], "import");
    assert_eq!(kind("crate::high::T", 3)["relationKind"], "reference");
    let call = kind("crate::high::f", 3);
    assert_eq!(call["relationKind"], "call");
    assert_eq!(call["caller"]["name"], "run");
    assert_eq!(call["owner"]["name"], "run");
    assert_eq!(call["owner"]["syntaxId"], call["ownerSyntaxId"]);
    assert_eq!(call["owner"]["stableLookupKey"], "src/lib.rs::run");
    assert_eq!(call["target"], Value::Null);
    assert_eq!(call["targetReason"], "structural_only");
    // Calls come first, and the counts add up to the edge.
    assert_eq!(sites[0]["relationKind"], "call");
    let counts = &pair["kindCounts"];
    assert_eq!(counts["call"], 1);
    assert_eq!(counts["import"], 1);
    assert_eq!(counts["reference"], 1);
    assert_eq!(counts["type"], 0);
    // A site outside any declaration is owned by the file module.
    let module = pair["source"]["sid"].clone();
    let import = kind("crate::high::T", 2);
    assert_eq!(import["owner"]["name"], Value::Null);
    assert_eq!(import["owner"]["stableLookupKey"], Value::Null);
    assert_eq!(import["owner"]["syntaxId"], module);
}

/// R3: the four reasons, and the order in which they apply.
#[test]
fn target_reason_vocabulary() {
    use SemanticOutcome::*;
    assert_eq!(target_reason(true, false, None), "structural_only");
    // No producer wins over any outcome.
    assert_eq!(
        target_reason(true, false, Some(&External)),
        "structural_only"
    );
    assert_eq!(target_reason(true, true, Some(&External)), "external");
    assert_eq!(target_reason(false, true, Some(&External)), "external");
    assert_eq!(target_reason(true, true, Some(&Ambiguous)), "ambiguous");
    assert_eq!(target_reason(true, true, None), "unresolved");
    assert_eq!(target_reason(true, true, Some(&Unresolved)), "unresolved");
    assert_eq!(target_reason(false, true, Some(&Boundary)), "unresolved");
    assert_eq!(
        target_reason(false, true, Some(&Internal(0, "sid".into()))),
        "unresolved"
    );
    assert_eq!(target_reason(false, true, None), "unresolved");
}
