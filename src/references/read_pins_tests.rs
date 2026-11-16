//! What the references read promises and no other test pinned: its budget, order, sampling
//! and typed exits. Each test names the contract text it holds the read to.
use super::parallel_tests::snapshot_of;
use super::*;

const MODEL: &str = "src/model.rs";
const SERVICE: &str = "src/service.rs";

/// Four Rust files in one crate. `src/service.rs` names `src/model.rs` once structurally
/// and calls a method of it, which only a semantic binding can resolve.
fn crate_snapshot() -> Snapshot {
    snapshot_of(&[
        (
            "src/lib.rs",
            "pub mod extra;\npub mod model;\npub mod service;\n",
        ),
        ("src/extra.rs", "pub fn idle() {}\n"),
        (
            MODEL,
            "pub struct Thing;\nimpl Thing {\n    pub fn go(&self) {}\n}\n",
        ),
        (
            SERVICE,
            "pub fn run(thing: &crate::model::Thing) {\n    thing.go();\n}\n",
        ),
    ])
}

fn position(snapshot: &Snapshot, path: &str) -> usize {
    snapshot
        .documents
        .iter()
        .position(|doc| doc.key.path == path)
        .unwrap_or_else(|| panic!("{path} is in the snapshot"))
}

/// Binds the one call of `src/service.rs` to `Thing::go` of `src/model.rs`, as a selected
/// producer's call binding would, with `metadata` on the binding.
fn bind_the_call(snapshot: &mut Snapshot, metadata: BTreeMap<String, Value>) {
    let model = position(snapshot, MODEL);
    let go = snapshot.documents[model]
        .declarations
        .iter()
        .find(|declaration| declaration.name.as_deref() == Some("go"))
        .expect("Thing::go is declared")
        .syntax_id
        .clone();
    let service = position(snapshot, SERVICE);
    let service = &mut snapshot.documents[service];
    assert_eq!(service.calls.len(), 1, "one measured call");
    let call = &service.calls[0];
    service.semantic = DocumentSemantic {
        producer_id: Some("producer".into()),
        complete: true,
        anchors: vec![SemanticAnchor {
            range: call.callee_range.clone().unwrap_or(call.range.clone()),
            call_id: Some(call.id.clone()),
            type_role: false,
            direct: true,
            outcome: SemanticOutcome::Internal(model, go),
            basis_site: true,
            provenance_id: "binding".into(),
            metadata,
        }],
        ..DocumentSemantic::default()
    };
}

fn read(snapshot: &Snapshot, sample_limit: usize) -> Value {
    project_with_workers(snapshot, &[], sample_limit, 1, &mut || Ok(()))
        .unwrap()
        .data
}

fn rows_from<'a>(data: &'a Value, source: &str) -> Vec<&'a Value> {
    ["production", "tests", "unresolved"]
        .iter()
        .flat_map(|field| data[*field].as_array().unwrap())
        .filter(|row| row["source"]["path"] == source)
        .collect()
}

fn edge<'a>(data: &'a Value, source: &str, target: &str) -> &'a Value {
    data["production"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| edge["source"]["path"] == source && edge["target"]["path"] == target)
        .unwrap_or_else(|| panic!("an edge from {source} to {target} in {data}"))
}

/// The one semantic site of the edge from `src/service.rs` to `src/model.rs`.
fn bound_site(data: &Value) -> &Value {
    let sites: Vec<_> = edge(data, SERVICE, MODEL)["sites"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|site| site["mapping"] == "semantic")
        .collect();
    assert_eq!(sites.len(), 1, "{data}");
    sites[0]
}

/// docs/rules.md, "Semantic bindings (#523)": "Any internal target must be a declaration of
/// this snapshot", and a resolved internal binding maps the site to its target's module.
/// A binding names its target by the document's place in the snapshot, and the read sorts
/// the documents: the target is found, and the answer is the same, in whatever order the
/// snapshot lists them.
#[test]
fn a_semantic_target_is_found_in_any_snapshot_document_order() {
    let mut snapshot = crate_snapshot();
    let mut answers = Vec::new();
    for _ in 0..snapshot.documents.len() {
        bind_the_call(&mut snapshot, BTreeMap::new());
        let data = read(&snapshot, 5);
        let site = bound_site(&data);
        assert_eq!(site["relationKind"], "call", "{site}");
        assert_eq!(site["target"]["document"]["path"], MODEL, "{site}");
        let go = &site["target"]["syntaxId"];
        assert!(
            snapshot.documents[position(&snapshot, MODEL)]
                .declarations
                .iter()
                .any(|d| d.name.as_deref() == Some("go") && *go == json!(d.syntax_id)),
            "{site}"
        );
        assert_eq!(site["targetReason"], Value::Null);
        assert_eq!(site["provenanceId"], "binding");
        assert_eq!(data["counts"]["semanticSites"], 1, "{data}");
        answers.push(data);
        // The next order is not this one's inverse: four documents, moved round by one.
        snapshot.documents.rotate_left(1);
    }
    assert!(answers.windows(2).all(|pair| pair[0] == pair[1]));
}

/// docs/rules.md, `CallerReason`: a site whose binding is marked a semantic boundary has
/// no caller, and says why with `semantic_boundary`. Without the mark, or with it false,
/// the measured call's enclosing function is the caller.
#[test]
fn a_semantic_boundary_site_names_no_caller() {
    let mut snapshot = crate_snapshot();
    for (metadata, boundary) in [
        (BTreeMap::new(), false),
        (
            BTreeMap::from([("semanticBoundary".to_owned(), json!(false))]),
            false,
        ),
        (
            BTreeMap::from([("semanticBoundary".to_owned(), json!(true))]),
            true,
        ),
    ] {
        bind_the_call(&mut snapshot, metadata);
        let data = read(&snapshot, 5);
        let site = bound_site(&data);
        if boundary {
            assert_eq!(site["caller"], Value::Null, "{site}");
            assert_eq!(site["callerReason"], "semantic_boundary", "{site}");
            assert_eq!(site["semanticBoundary"], true, "{site}");
        } else {
            assert_eq!(site["caller"]["name"], "run", "{site}");
            assert_eq!(site["callerReason"], Value::Null, "{site}");
        }
    }
}

/// docs/rules.md, "Work bounds and located failures": a document published with no
/// evidence but its file module (`evidence_budget`, `oversized`, `not_utf8`) "has no
/// sites", and `partialFiles` names it. Its source text is still in the snapshot, so only
/// the read's selection keeps its sites out.
#[test]
fn a_document_with_an_evidence_gap_has_no_sites() {
    let whole = read(&crate_snapshot(), 5);
    assert_eq!(edge(&whole, SERVICE, MODEL)["count"], 1, "{whole}");
    assert_eq!(whole["counts"]["scannedDocuments"], 4);

    let mut snapshot = crate_snapshot();
    let service = position(&snapshot, SERVICE);
    let gap = &mut snapshot.documents[service];
    // What the store publishes for such a document: its file module and nothing else.
    gap.declarations
        .retain(|declaration| declaration.kind == "module" && declaration.ancestors.is_empty());
    gap.calls.clear();
    let key = gap.key.clone();
    snapshot.evidence_gaps.insert(key, "evidence_budget");
    let data = read(&snapshot, 5);
    assert!(rows_from(&data, SERVICE).is_empty(), "{data}");
    assert_eq!(data["counts"]["scannedDocuments"], 3, "{data}");
    let partial = data["partialFiles"].as_array().unwrap();
    assert_eq!(partial.len(), 1, "{data}");
    assert_eq!(partial[0]["path"], SERVICE);
    assert_eq!(partial[0]["reason"], "evidence_budget");
}

/// Three calls, an import and two plain references from `src/low.rs` to `src/high.rs`.
fn sampled_snapshot() -> Snapshot {
    snapshot_of(&[
        ("src/lib.rs", "pub mod high;\npub mod low;\n"),
        ("src/high.rs", "pub struct T;\npub fn f() {}\n"),
        (
            "src/low.rs",
            "use crate::high::T;\npub fn run(_: crate::high::T, _: crate::high::T) {\n    crate::high::f();\n    crate::high::f();\n    crate::high::f();\n}\n",
        ),
    ])
}

/// docs/rules.md, "CLI JSON": a sample limit is 1 to 50 and "neither sample limit changes
/// exact site counts"; R4: the kind counts add up to `count` before sampling; R5: "An
/// edge's sample takes its `call` sites first, then the others, each in the existing
/// order. `omittedSites` is `count` minus the sites returned." So an edge returns exactly
/// `min(limit, count)` sites, and a smaller limit's sites are a prefix of a larger one's
/// (docs/mcp-readonly-pilot-contract.md, amendment #985).
#[test]
fn an_edge_returns_exactly_its_sample_limit_calls_first() {
    let snapshot = sampled_snapshot();
    let all = read(&snapshot, MAX_SAMPLE_LIMIT);
    let all = edge(&all, "src/low.rs", "src/high.rs");
    let every_site = all["sites"].as_array().unwrap();
    assert_eq!(all["count"], 6, "{all}");
    assert_eq!(every_site.len(), 6);
    let kinds: Vec<_> = every_site
        .iter()
        .map(|site| site["relationKind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["call", "call", "call", "import", "reference", "reference"]
    );
    // Each kind in source order.
    let starts = |from: usize, to: usize| -> Vec<u64> {
        every_site[from..to]
            .iter()
            .map(|site| site["range"]["start"].as_u64().unwrap())
            .collect()
    };
    assert!(starts(0, 3).is_sorted(), "{all}");
    assert!(starts(3, 6).is_sorted(), "{all}");
    for limit in 1..=7 {
        let data = read(&snapshot, limit);
        let edge = edge(&data, "src/low.rs", "src/high.rs");
        let sites = edge["sites"].as_array().unwrap();
        let returned = limit.min(6);
        assert_eq!(sites.len(), returned, "limit {limit}: {edge}");
        assert_eq!(sites[..], every_site[..returned], "limit {limit}");
        assert_eq!(edge["count"], 6, "limit {limit}");
        assert_eq!(edge["omittedSites"], 6 - returned, "limit {limit}");
        assert_eq!(
            edge["kindCounts"],
            json!({"call":3,"type":0,"import":1,"reference":2}),
            "limit {limit}"
        );
    }
}

/// docs/architecture-view.md, "Summary and detail reads": a build keeps "only the
/// per-source reference rows (up to 50 sampled sites per edge, calls first)", so what a
/// checkout holds between reads is bounded. The resolved base keeps no more call samples
/// and no more other samples than its sample limit, while its counts stay exact. Selection
/// cuts the answer to the limit again, so only the base shows this bound.
#[test]
fn a_resolved_base_keeps_no_more_samples_than_its_limit() {
    let snapshot = sampled_snapshot();
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    for limit in [1, 2, 3] {
        let base = resolve(&snapshot, Sources::all(), limit, 1, &request).unwrap();
        let mut sites = 0;
        for edge in base
            .sources
            .iter()
            .flatten()
            .flat_map(|source| source.edges.values())
        {
            assert!(edge.calls.len() <= limit, "limit {limit}");
            assert!(edge.others.len() <= limit, "limit {limit}");
            sites += edge.count;
        }
        assert_eq!(sites, 6, "limit {limit}");
    }
}

/// docs/rules.md, "Work bounds and located failures": "The 250,000-site bound applies
/// both to each raw source extraction and to deduplicated selected sites across the
/// request", and the bounds "refuse the entire result rather than silently dropping
/// evidence". With the bound lowered to what this revision has, the read answers; one
/// site fewer and it fails typed, although no single file is over the bound.
///
/// The lowered bound is process-wide, like the other users of this hook: nextest runs
/// each test in its own process.
#[cfg(feature = "test-support")]
#[test]
fn the_site_bound_counts_every_selected_file_of_the_request() {
    let snapshot = four_language_snapshot();
    let whole = project_with_workers(&snapshot, &[], 5, 1, &mut || Ok(())).unwrap();
    let mut by_file: BTreeMap<&str, usize> = BTreeMap::new();
    for ((path, _), coverage) in &whole.coverage {
        *by_file.entry(path).or_default() += coverage.sites;
    }
    let sites: usize = by_file.values().sum();
    assert!(by_file.values().filter(|&&count| count > 0).count() > 1);
    assert!(by_file.values().all(|&count| count < sites));

    set_site_limit_for_tests(sites);
    assert_eq!(read(&snapshot, 5), whole.data);
    set_site_limit_for_tests(sites - 1);
    let error = project_with_workers(&snapshot, &[], 5, 1, &mut || Ok(())).unwrap_err();
    assert_eq!(error.to_string(), "bounded_work: reference site count");
    set_site_limit_for_tests(SITES);
}

/// One file of each language family, each with a reference its catalog resolves.
fn four_language_snapshot() -> Snapshot {
    snapshot_of(&[
        (
            "src/lib.rs",
            "mod provider; pub use provider::value; pub fn run() { value(); }",
        ),
        ("src/provider.rs", "pub fn value() {}"),
        (
            "Use.java",
            "package app; import app.Provider; public class Use { Provider value; }",
        ),
        ("Provider.java", "package app; public class Provider {}"),
        ("use.py", "from provider import value\nvalue()\n"),
        ("provider.py", "def value():\n    return 1\n"),
        ("use.js", "import { value } from './provider.js'; value();"),
        ("provider.js", "export function value() {}"),
    ])
}

/// The budget the four catalogs hand to the rest of a read built under `limit`.
fn catalog_budget(snapshot: &Snapshot, limit: usize) -> Result<Work> {
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    let check = &mut request.worker();
    let admitted = admit_snapshot(snapshot, check)?;
    let trees: Vec<Tree> = admitted
        .docs
        .iter()
        .map(|doc| parse(doc, check))
        .collect::<Result<_>>()?;
    let selected: Vec<usize> = (0..admitted.docs.len()).collect();
    let read = ReadContext {
        snapshot,
        docs: &admitted.docs,
        trees: &trees,
        document_map: &admitted.document_map,
        selected: &selected,
        sample_limit: 5,
        workers: 1,
        request: &request,
    };
    Ok(build_catalogs(read, limit, check)?.rust.work)
}

/// docs/rules.md, "Work bounds and located failures": "Reference work has one
/// request-wide allowance", and exhaustion fails `bounded_work`. The four catalogs are
/// built side by side, each counting for itself; all four counts are then charged to the
/// one allowance the rest of the read goes on charging. So the work they report is the
/// least allowance they can be built under: one unit less fails typed.
///
/// The allowance of a real read is at least 5,000,000 units, which no revision of normal
/// size uses up, so this drives the catalog stage itself under a chosen allowance.
#[test]
fn the_catalogs_charge_one_shared_budget() {
    let snapshot = four_language_snapshot();
    let roomy = catalog_budget(&snapshot, 5_000_000).unwrap();
    assert_eq!(roomy.limit, 5_000_000);
    let needed = roomy.used;
    assert!(needed > 0);
    let exact = catalog_budget(&snapshot, needed).unwrap();
    assert_eq!((exact.used, exact.remaining()), (needed, 0));
    let error = catalog_budget(&snapshot, needed - 1).unwrap_err();
    assert!(error.to_string().starts_with("bounded_work: "), "{error}");
}

/// docs/rules.md, "Work bounds and located failures": the one request-wide allowance.
/// With a selected producer, every declaration of the snapshot is a possible semantic
/// target, and collecting each one is one unit of that allowance: a budget one unit short
/// of the declarations fails typed. Without a producer nothing is collected.
#[test]
fn collecting_semantic_targets_is_charged_per_declaration() {
    let mut snapshot = crate_snapshot();
    let docs = |snapshot: &Snapshot| -> Vec<usize> {
        let mut order: Vec<usize> = (0..snapshot.documents.len()).collect();
        order.sort_by(|&a, &b| snapshot.documents[a].key.cmp(&snapshot.documents[b].key));
        order
    };
    let declarations: usize = snapshot
        .documents
        .iter()
        .map(|doc| doc.declarations.len())
        .sum();
    assert!(declarations > 4, "more than the four file modules");
    let collect = |snapshot: &Snapshot, budget: usize| {
        let docs: Vec<_> = docs(snapshot)
            .into_iter()
            .map(|index| &snapshot.documents[index])
            .collect();
        let mut work = Work::new(budget);
        declared_targets(&docs, &mut work, &mut || Ok(()))
            .map(|(declared, producers)| (declared.len(), producers.len()))
    };
    assert_eq!(collect(&snapshot, 0).unwrap(), (0, 0));
    bind_the_call(&mut snapshot, BTreeMap::new());
    assert_eq!(collect(&snapshot, declarations).unwrap(), (declarations, 1));
    assert_eq!(
        collect(&snapshot, declarations - 1)
            .unwrap_err()
            .to_string(),
        "bounded_work: reference owner lookup"
    );
}

#[derive(Debug)]
struct CallerStopped;
impl std::fmt::Display for CallerStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}
impl std::error::Error for CallerStopped {}

/// docs/mcp-readonly-pilot-contract.md, error classes: "Cancellation overrides all unsent
/// outcomes." Whichever poll of the request's callback reports the stop, the read returns
/// that error of the caller's, never a projection and never an error of its own. Which
/// steps poll, and how often, is not asserted.
#[test]
fn a_stopped_read_returns_the_callers_error_at_every_poll() {
    let mut snapshot = crate_snapshot();
    bind_the_call(&mut snapshot, BTreeMap::new());
    let stopping_at = |stop: usize, workers: usize| {
        let mut polls = 0usize;
        let result = project_with_workers(&snapshot, &[], 5, workers, &mut || {
            polls += 1;
            if polls == stop {
                return Err(CallerStopped.into());
            }
            Ok(())
        });
        (result, polls)
    };
    let (whole, polls) = stopping_at(usize::MAX, 1);
    whole.unwrap();
    assert!(polls > 1);
    for stop in 1..=polls {
        let (result, made) = stopping_at(stop, 1);
        let error = result.expect_err("a stopped read has no answer");
        assert!(error.downcast_ref::<CallerStopped>().is_some(), "{error}");
        assert_eq!(made, stop, "no poll after the stop");
    }
    // The first poll comes before any worker starts, whatever the worker count.
    let (result, made) = stopping_at(1, 2);
    assert!(
        result
            .unwrap_err()
            .downcast_ref::<CallerStopped>()
            .is_some()
    );
    assert_eq!(made, 1);
}

/// docs/rules.md, "CLI JSON" and "Work bounds and located failures": both reads accept
/// 1 to 50 sample sites per edge, and "Reference samples accept at most 50 sites per
/// edge". A limit outside that is the caller's mistake, typed `invalid_request`.
#[test]
fn a_sample_limit_outside_one_to_fifty_is_an_invalid_request() {
    let snapshot = sampled_snapshot();
    assert_eq!(MAX_SAMPLE_LIMIT, 50);
    for limit in [0, MAX_SAMPLE_LIMIT + 1] {
        let error = project(&snapshot, &[], limit, &mut || Ok(())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid_request: reference sample limit",
            "limit {limit}"
        );
    }
    for limit in [1, MAX_SAMPLE_LIMIT] {
        project(&snapshot, &[], limit, &mut || Ok(())).unwrap();
    }
    let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
    for limit in [0, MAX_SAMPLE_LIMIT + 1] {
        let error = base.select(&[], limit, &mut || Ok(())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid_request: reference sample limit",
            "limit {limit}"
        );
    }
}

fn read_error(snapshot: &Snapshot) -> String {
    project(snapshot, &[], 5, &mut || Ok(()))
        .unwrap_err()
        .to_string()
}

fn copy_of(doc: &DocumentInput) -> DocumentInput {
    DocumentInput {
        key: doc.key.clone(),
        file: doc.file.clone(),
        declarations: doc.declarations.clone(),
        calls: doc.calls.clone(),
        semantic: doc.semantic.clone(),
    }
}

/// docs/mcp-readonly-pilot-contract.md, error classes: `store_unavailable` is an
/// "Unsafe/unreadable/invalid store"; the project rule for accidental corruption is a
/// typed failure, never a partial result. A document whose key and stored file disagree
/// on the path or the language is such a snapshot.
#[test]
fn a_document_whose_key_and_file_disagree_is_store_unavailable() {
    for corrupt in [
        |doc: &mut DocumentInput| doc.file.path = "src/other.rs".into(),
        |doc: &mut DocumentInput| doc.file.language = "java".into(),
    ] {
        let mut snapshot = crate_snapshot();
        let service = position(&snapshot, SERVICE);
        corrupt(&mut snapshot.documents[service]);
        assert_eq!(
            read_error(&snapshot),
            "store_unavailable: reference document identity"
        );
    }
}

/// As above: a snapshot that lists one document twice.
#[test]
fn a_document_listed_twice_is_store_unavailable() {
    let mut snapshot = crate_snapshot();
    let service = position(&snapshot, SERVICE);
    let twice = copy_of(&snapshot.documents[service]);
    snapshot.documents.push(twice);
    assert_eq!(
        read_error(&snapshot),
        "store_unavailable: duplicate reference document"
    );
}

/// As above: every document has exactly one file module, which names it in the answer. A
/// document with none, or with two, is not one the indexer published.
#[test]
fn a_document_without_exactly_one_file_module_is_store_unavailable() {
    let file_module = |declaration: &Declaration| {
        declaration.kind == "module" && declaration.ancestors.is_empty()
    };
    let mut none = crate_snapshot();
    let service = position(&none, SERVICE);
    none.documents[service]
        .declarations
        .retain(|declaration| !file_module(declaration));
    assert_eq!(
        read_error(&none),
        "store_unavailable: reference file module"
    );

    let mut two = crate_snapshot();
    let service = position(&two, SERVICE);
    let declarations = &mut two.documents[service].declarations;
    let module = declarations
        .iter()
        .find(|declaration| file_module(declaration))
        .unwrap()
        .clone();
    declarations.push(module);
    assert_eq!(read_error(&two), "store_unavailable: reference file module");
}
