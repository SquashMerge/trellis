use super::test_support::{pending_tiers, rust_semantic};
use super::*;

/// #660: only the answer's pending languages are listed, in #22 order, and `[]` when none.
#[test]
fn semantic_status_lists_the_answers_pending_languages_in_order() {
    use crate::model::SemanticStatus::{CatchingUp, NeedsReimport};
    let tiers = pending_tiers(&[
        ("typescript", CatchingUp, "re-import pending"),
        ("java", NeedsReimport, "re-import required"),
        ("rust", NeedsReimport, "producer profile pin changed"),
    ]);
    assert_eq!(
        semantic_status(&tiers, ["typescript", "rust", "java", "rust"]),
        json!([
            {"language":"java","state":"needsReimport","reason":"re-import required"},
            {"language":"rust","state":"needsReimport","reason":"producer profile pin changed"},
            {"language":"typescript","state":"catchingUp","reason":"re-import pending"},
        ])
    );
    assert_eq!(
        semantic_status(&tiers, ["rust", "python"]),
        json!([{"language":"rust","state":"needsReimport","reason":"producer profile pin changed"}])
    );
    assert_eq!(semantic_status(&tiers, ["python"]), json!([]));
    assert_eq!(semantic_status(&tiers, Vec::<&str>::new()), json!([]));
    assert_eq!(semantic_status(&Tiers::default(), ["rust"]), json!([]));
    assert!(
        crate::mcp::tools::failure(&json!(1), "not_found")
            .get("semanticStatus")
            .is_none()
    );
}
#[test]
fn compact_keeps_projection_and_historical_freshness() {
    let projection = json!({
        "currentPin":{"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":2},
        "semanticBasisPin":{"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1},
        "basisNativeRevisionId":"revision:v2:basis", "semanticStatus":"olderBasis"
    });
    let mut data = json!({"calls":[{
        "binding":{},"semanticBasisRef":0,"freshness":"possiblyStale",
        "historicalFreshness":"fresh","projection":projection
    }],"coverage":[],"provenance":[],"semanticBases":[],
        "semanticDocuments":[{"projection":projection}]});
    compact_data(&mut data, "calls");
    let call = &data["calls"][0];
    assert_eq!(call["freshness"], "possiblyStale");
    assert_eq!(call["historicalFreshness"], "fresh");
    assert_eq!(call["projection"], projection);
    assert_eq!(data["semanticDocuments"][0]["projection"], projection);
    assert!(call.get("binding").is_none());
    assert!(call.get("semanticBasisRef").is_none());
    assert!(data.get("provenance").is_none());
}

#[test]
fn describe_has_five_rows_typescript_last() {
    let basis = json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1});
    let mut rows = [[0; 5]; 5];
    rows[4] = [3, 3, 0, 0, 1];
    let ready = describe(
        &json!(2),
        "project",
        "ready",
        None,
        Some((rows, Tiers::default())),
        Some(basis),
        false,
    );
    let languages: Vec<&str> = ready["data"]["languages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["language"].as_str().unwrap())
        .collect();
    assert_eq!(
        languages,
        ["java", "rust", "python", "javascript", "typescript"]
    );
    assert_eq!(
        ready["data"]["languages"][4]["coverage"],
        json!({"selected":3,"complete":3,"partial":0,"failed":0,"unselected":1})
    );
}

#[test]
fn describe_closed_ready_and_missing_head() {
    let empty = describe(&json!(1), "project", "reconciling", None, None, None, false);
    assert!(empty["evidenceBasis"].is_null());
    assert_eq!(empty["data"]["languages"], json!([]));
    assert_eq!(empty["partial"], true);
    let rows = [[0, 0, 0, 0, 0], [2, 1, 1, 0, 3], [0; 5], [0; 5], [0; 5]];
    let basis = json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1});
    let ready = describe(
        &json!(2),
        "project",
        "ready",
        None,
        Some((rows, Tiers::default())),
        Some(basis.clone()),
        true,
    );
    assert_eq!(ready["evidenceBasis"], basis);
    assert_eq!(ready["data"]["watcherDegraded"], true);
    assert_eq!(ready["data"]["toolVersion"], 30);
    assert_eq!(
        ready["data"]["languages"][1]["coverage"],
        json!({
        "selected":2,"complete":1,"partial":1,"failed":0,"unselected":3})
    );
    assert_eq!(ready["partial"], true);
    assert_eq!(ready["data"]["languages"].as_array().unwrap().len(), 5);
    // Without imported evidence every language is syntax, with no producer.
    for language in ready["data"]["languages"].as_array().unwrap() {
        assert_eq!(language["nativeCoverageTier"], "syntax");
        assert_eq!(language["semantic"]["available"], false);
        assert_eq!(language["semantic"]["producerId"], Value::Null);
        assert_eq!(language["semantic"]["freshness"], "unavailable");
        assert_eq!(language["semantic"]["reimport"], Value::Null);
    }
}

#[test]
fn reimport_is_shared_by_describe_and_scope_without_claiming_evidence() {
    let mut tiers = Tiers::default();
    tiers[1].reimport = Some(crate::model::SemanticReimport::NeedsReimport {
        reason: "producer profile pin changed".into(),
    });
    let rows = [[0; 5]; 5];
    let described = describe(
        &json!(1),
        "project",
        "ready",
        None,
        Some((rows, tiers.clone())),
        None,
        false,
    );
    let scope = language_status(&rows, &tiers);
    let semantic = &described["data"]["languages"][1]["semantic"];
    assert_eq!(
        semantic["reimport"],
        json!({
            "state":"needsReimport", "reason":"producer profile pin changed"
        })
    );
    assert_eq!(scope[1]["semantic"]["reimport"], semantic["reimport"]);
    assert_eq!(semantic["available"], false);
    assert_eq!(semantic["producerId"], Value::Null);
    assert_eq!(semantic["freshness"], "unavailable");
    assert_eq!(described["partial"], false);
    assert!(syntax_only(&tiers, ["rust"]).is_some());
    // Another producer's fresh publication may coexist with retained work.
    tiers[1].producer_id = Some("other-producer".into());
    tiers[1].freshness = "fresh";
    tiers[1].counts = [1, 1, 0, 0, 0];
    let fresh = language_status(&rows, &tiers);
    assert_eq!(fresh[1]["semantic"]["available"], true);
    assert_eq!(fresh[1]["semantic"]["reimport"], semantic["reimport"]);
}

#[test]
fn reimport_describe_and_find_outputs_satisfy_the_closed_catalog() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut tiers = Tiers::default();
    tiers[1].reimport = Some(crate::model::SemanticReimport::NeedsReimport {
        reason: "retained artifact no longer matches captured inputs".into(),
    });
    let basis = json!({
        "indexGeneration":"123e4567-e89b-42d3-a456-426614174000", "indexRevision":1
    });
    let mut rows = [[0; 5]; 5];
    rows[1] = [1, 1, 0, 0, 0];
    let mut described = describe(
        &json!(1),
        "project",
        "ready",
        None,
        Some((rows, tiers.clone())),
        Some(basis),
        false,
    );
    described["workspace"] = json!("/selected");
    described["catchingUp"] = json!(false);
    let found = find(
        &json!(2),
        super::test_support::fixture(&[], rows, false),
        &tiers,
        std::path::Path::new("/selected"),
        false,
        true,
        false,
    )
    .unwrap()
    .unwrap();
    // #715: a zero-hit find shows the searched languages only, here Rust.
    assert_eq!(found["data"]["scopeCoverage"][0]["language"], "rust");
    assert_eq!(
        found["data"]["scopeCoverage"][0]["semantic"]["reimport"],
        described["data"]["languages"][1]["semantic"]["reimport"]
    );
    let script = r#"
const {createRequire} = require('node:module');
const {readFileSync} = require('node:fs');
const local = createRequire(process.cwd() + '/tests/mcp/package.json');
const Ajv = local('ajv/dist/2020.js').default;
const ajv = new Ajv({strict:false});
const catalog = JSON.parse(readFileSync('tests/fixtures/mcp/catalog.json','utf8'));
for (const [name, output] of JSON.parse(readFileSync(0,'utf8'))) {
  const validate = ajv.compile(catalog.find(tool => tool.name === name).outputSchema);
  if (!validate(output)) throw Error(name + ': ' + JSON.stringify(validate.errors));
}
const pin = {indexGeneration:'123e4567-e89b-42d3-a456-426614174000',indexRevision:2};
const projection = {currentPin:pin,semanticBasisPin:{...pin,indexRevision:1},
  basisNativeRevisionId:'revision:v2:historical',semanticStatus:'olderBasis'};
for (const name of ['trellis_outgoing_calls','trellis_incoming_calls']) {
  const defs = catalog.find(tool => tool.name === name).outputSchema.$defs;
  const values = {
    SemanticProjection:projection,
    DocumentSemanticStatus:{producerId:'rust-analyzer',language:'rust',path:'src/lib.rs',projection}
  };
  for (const [definition,value] of Object.entries(values)) {
    const validate = ajv.compile({$defs:defs,$ref:'#/$defs/' + definition});
    if (!validate(value)) throw Error(name + ' valid ' + definition + ': ' + JSON.stringify(validate.errors));
    if (validate({...value,unknownProjectionField:true})) throw Error(name + ' accepted unknown ' + definition + ' field');
    for (const field of defs[definition].required) {
      const missing = {...value}; delete missing[field];
      if (validate(missing)) throw Error(name + ' accepted missing ' + definition + '.' + field);
    }
  }
  const validateDocument = ajv.compile({$defs:defs,$ref:'#/$defs/DocumentSemanticStatus'});
  if (validateDocument({...values.DocumentSemanticStatus,projection:{...projection,unknownProjectionField:true}})) {
    throw Error(name + ' accepted unknown nested projection field');
  }
}
"#;
    let mut child = Command::new("node")
        .args(["-e", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Node is required by tools/verify");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            &serde_json::to_vec(&json!([
                ["trellis_workspace_describe", described],
                ["trellis_find_symbols", found]
            ]))
            .unwrap(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #528: `syntaxOnly` names, in #22 order, only the answer's languages without
/// available semantic evidence, and is dropped when all of them have it.
#[test]
fn syntax_only_names_only_languages_without_available_semantic_evidence() {
    let tiers = rust_semantic("fresh", [3, 3, 0, 0, 0]);
    assert_eq!(syntax_only(&tiers, ["rust"]), None);
    assert_eq!(
        syntax_only(&tiers, ["rust", "javascript", "java", "rust"]).unwrap()["message"],
        "This answer is syntax-derived; no fresh semantic evidence is selected for java, javascript"
    );
    assert_eq!(
        syntax_only(&tiers, Vec::<&str>::new()).unwrap()["message"],
        "This answer is syntax-derived"
    );
    for (freshness, counts) in [
        ("possiblyStale", [3, 3, 0, 0, 0]),
        ("stale", [3, 3, 0, 0, 0]),
        ("unavailable", [2, 0, 0, 2, 0]),
    ] {
        let warning = syntax_only(&rust_semantic(freshness, counts), ["rust"]).unwrap();
        assert_eq!(warning["code"], "syntaxOnly");
        assert_eq!(warning["provenanceId"], Value::Null);
    }
}

/// #528: a fresh selected import reports the semantic tier beside native coverage;
/// partial semantic coverage stays partial, and stale evidence stays syntax.
#[test]
fn describe_names_why_semantic_tuples_are_partial() {
    use crate::model::{GapOccurrences, PartialReasons, SemanticCoverageSummary};
    let mut tiers = rust_semantic("fresh", [355, 300, 55, 0, 0]);
    let mut summary = SemanticCoverageSummary {
        complete: 300,
        partial: 55,
        partial_reasons: PartialReasons {
            native_gaps: 54,
            native_gaps_only: 53,
            capped: 2,
            unsupported_occurrences: GapOccurrences {
                macro_invocation: 900,
                attribute_item: 40,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    summary.summary = summary.describe();
    tiers[1].partial_reasons = Some(summary);
    let described = describe(
        &json!(1),
        "project",
        "ready",
        None,
        Some((
            [[0; 5], [355, 300, 55, 0, 0], [0; 5], [0; 5], [0; 5]],
            tiers.clone(),
        )),
        None,
        false,
    );
    assert_eq!(
        described["data"]["languages"][1]["semantic"]["partialReasons"],
        json!({"nativeGaps":54,"nativeGapsOnly":53,"capped":2,"omitted":0,
            "capturedDiagnostics":0,"unrecorded":0,
            "unsupportedOccurrences":{"macroInvocation":900,"attributeItem":40,
                "macroDefinition":0,"other":0},
            "summary":"partial: 55 · native gaps 54, document cap 2"})
    );
    assert_eq!(
        described["data"]["languages"][0]["semantic"]["partialReasons"],
        Value::Null
    );
    // Only describe carries the reasons; scope coverage keeps its compact shape.
    assert!(
        language_status(&[[0; 5]; 5], &tiers)[1]["semantic"]
            .get("partialReasons")
            .is_none()
    );
}

#[test]
fn describe_reports_the_imported_semantic_tier_per_language() {
    let rows = [
        [1, 1, 0, 0, 0],
        [355, 22, 333, 0, 0],
        [0; 5],
        [0; 5],
        [0; 5],
    ];
    let basis = json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1});
    let fresh = describe(
        &json!(1),
        "project",
        "ready",
        None,
        Some((rows, rust_semantic("fresh", [355, 300, 55, 0, 0]))),
        Some(basis.clone()),
        false,
    );
    let rust = &fresh["data"]["languages"][1];
    // Native coverage is labelled separately from the selected semantic import.
    assert_eq!(rust["nativeCoverageTier"], "syntax");
    assert!(rust.get("evidenceTier").is_none());
    assert_eq!(
        rust["coverage"],
        json!({"selected":355,"complete":22,"partial":333,"failed":0,"unselected":0})
    );
    assert_eq!(
        rust["semantic"],
        json!({"available":true,"producerId":"rust-analyzer","freshness":"fresh","note":null,
            "coverage":{"selected":355,"complete":300,"partial":55,"failed":0,"unselected":0},
            "partialReasons":null,"reimport":null,"documentStatus":{"olderBasis":0,"catchingUp":0,"needsReimport":0}})
    );
    assert_eq!(
        fresh["data"]["languages"][0]["semantic"]["available"],
        false
    );
    assert_eq!(fresh["partial"], true);
    let stale = describe(
        &json!(2),
        "project",
        "ready",
        None,
        Some((rows, rust_semantic("stale", [355, 355, 0, 0, 0]))),
        Some(basis),
        false,
    );
    assert_eq!(
        stale["data"]["languages"][1]["semantic"]["available"],
        false
    );
    assert_eq!(
        stale["data"]["languages"][1]["semantic"]["freshness"],
        "stale"
    );
}
