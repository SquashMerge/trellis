//! Role selection and the per-document evaluation of declaration constraints. The
//! tree-sitter base parse and the `DocumentEvaluation` struct stay in `rules.rs`.
use super::{
    Bases, Config, Constraint, DocumentEvaluation, Selector, Targets,
    evaluate::{decorate, semantic_evidence},
    glob_matches, identity, native_evidence, parse_bases,
};
use crate::{
    native_evidence::{Declaration, DeclarationRole},
    references::{parallel_check::RequestCheck, work::Work},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn selection(selector: &Selector, d: &Declaration, bases: Option<&Bases>) -> Option<bool> {
    if !glob_matches(&selector.paths, &d.document.path)
        || (!selector.language.is_empty() && !selector.language.contains(&d.document.language))
        || selector
            .name
            .as_ref()
            .is_some_and(|p| d.name.as_ref().is_none_or(|n| !p.is_match(n)))
    {
        return Some(false);
    }
    let mut unknown = false;
    for (wanted, actual) in [
        (&selector.extends, bases.and_then(|b| b.extends.as_ref())),
        (
            &selector.implements,
            bases.and_then(|b| b.implements.as_ref()),
        ),
    ] {
        if let Some(wanted) = wanted {
            if let Some(actual) = actual {
                if !actual.contains(wanted) {
                    return Some(false);
                }
            } else {
                unknown = true;
            }
        }
    }
    if unknown { None } else { Some(true) }
}
pub(super) fn package_closure(
    packages: &[Value],
    instances: &[Value],
    test_only: &[Value],
    unassigned: &[Value],
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Value>> {
    let mut needed = BTreeSet::new();
    for identity in instances
        .iter()
        .chain(test_only)
        .map(|row| &row["destination"])
        .chain(unassigned)
    {
        check()?;
        ensure!(
            work.charge(1),
            "bounded_work: rules external package closure budget"
        );
        if identity["kind"] == "externalPackage" {
            needed.insert(identity.to_string());
        }
    }
    let mut retained = Vec::new();
    for package in packages {
        check()?;
        ensure!(
            work.charge(1),
            "bounded_work: rules external package closure budget"
        );
        if needed.remove(&package["identity"].to_string()) {
            retained.push(package.clone());
        }
    }
    ensure!(
        needed.is_empty(),
        "store_unavailable: rules external package closure absent"
    );
    Ok(retained)
}
impl DocumentEvaluation<'_> {
    pub(super) fn run(
        &self,
        documents: &[&crate::references::DocumentInput],
        work: &mut Work,
        workers: usize,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<(Vec<Value>, Vec<Value>)> {
        let workers = workers.clamp(1, crate::parallel::MAX_WORKERS);
        let request = RequestCheck::new(check);
        let mut instances = Vec::new();
        let mut test_only = Vec::new();
        for batch in documents.chunks(workers) {
            request.result(request.check())?;
            let remaining = work.remaining();
            let outcomes =
                crate::parallel::map_ordered(batch, workers, &request.stopped, |_, doc| {
                    #[cfg(test)]
                    if let Some(before) = self.before_document {
                        before(doc);
                    }
                    evaluate_document(
                        self.config,
                        self.targets,
                        doc,
                        self.inheritance,
                        self.source_set,
                        remaining,
                        &mut request.worker(),
                    )
                });
            request.result(Ok(()))?;
            for outcome in outcomes {
                let rows =
                    request.result(outcome.context("cancelled: rules worker not started")?)?;
                ensure!(
                    work.charge(rows.work.used),
                    "bounded_work: rules document budget"
                );
                instances.extend(rows.instances);
                test_only.extend(rows.test_only);
                request.result(request.check())?;
            }
        }
        Ok((instances, test_only))
    }
}

pub(super) struct DocumentRows {
    instances: Vec<Value>,
    test_only: Vec<Value>,
    pub(super) work: Work,
}

pub(super) fn evaluate_document(
    config: &Config,
    targets: &Targets<'_>,
    doc: &crate::references::DocumentInput,
    inheritance: bool,
    source_set: Option<&str>,
    limit: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<DocumentRows> {
    let mut work = Work::new(limit);
    let callers = crate::references::callers::Callers::new(doc, check)?;
    let mut instances = Vec::new();
    let mut test_only = Vec::new();
    let bases = if inheritance {
        parse_bases(doc, check)?
    } else {
        BTreeMap::new()
    };
    let mut anchors = BTreeMap::new();
    if !config.rules.is_empty() {
        for anchor in &doc.semantic.anchors {
            ensure!(work.charge(1), "bounded_work: rule constraint budget");
            if let Some(call) = &anchor.call_id {
                anchors.insert(call.as_str(), anchor);
            }
        }
    }
    for d in &doc.declarations {
        check()?;
        for rule in &config.rules {
            ensure!(
                work.charge(1 + config.roles[&rule.role].paths.len()),
                "bounded_work: rules selection budget"
            );
            check()?;
            let selected = selection(&config.roles[&rule.role], d, bases.get(&d.syntax_id));
            if selected == Some(false) {
                continue;
            }
            let mut unresolved = Vec::new();
            if selected.is_none() {
                unresolved.push("selector_relationship_unavailable".to_owned());
            }
            let mut constraints = Vec::new();
            let mut statuses = Vec::new();
            for c in &rule.constraints {
                ensure!(
                    work.charge(
                        1 + if c.kind == "calls" {
                            doc.calls.len()
                        } else {
                            doc.semantic.relationships.len()
                        },
                    ),
                    "bounded_work: rule constraint budget"
                );
                check()?;
                let (status, reason, mut evidence) =
                    constraint(config, targets, &anchors, doc, d, c, &mut work, check)?;
                for site in &mut evidence {
                    callers.annotate(site);
                }
                statuses.push(status);
                constraints.push(json!({"mode":c.mode,"kind":c.kind,"targetRole":c.role,"status":status,"evidence":evidence,"unresolved":reason.into_iter().collect::<Vec<_>>(),"projection":doc.semantic.projection}));
                if let Some(reason) = reason
                    && !unresolved.iter().any(|r| r == reason)
                {
                    unresolved.push(reason.to_owned());
                }
            }
            // A proven deviation wins over uncertainty; conformance needs every constraint.
            let status = if selected != Some(true) || statuses.is_empty() {
                "unknown"
            } else if statuses.contains(&"deviates") {
                "deviates"
            } else if statuses.iter().all(|status| *status == "conforms") {
                "conforms"
            } else {
                "unknown"
            };
            let test = d.role == DeclarationRole::Test;
            if test {
                unresolved.push("test_only".into());
            }
            let mut row = json!({"rule":rule.id,"target":identity(d),"destination":null,"sourceLayer":null,"destinationLayer":null,"status":status,"excepted":false,"exceptionReason":null,
                    "constraints":constraints,"evidence":[native_evidence(d)],"unresolved":unresolved,"referenceCount":0,"projection":doc.semantic.projection});
            ensure!(
                work.charge(config.exceptions.len()),
                "bounded_work: exception match budget"
            );
            decorate(config, source_set, &mut row);
            if test {
                test_only.push(row)
            } else {
                instances.push(row)
            }
        }
    }
    Ok(DocumentRows {
        instances,
        test_only,
        work,
    })
}

/// One semantic constraint of a selected declaration (#523): its status, the reason it is
/// unknown, and its evidence. A call counts when its selected binding is direct (or a
/// constructor) to a declaration the target role selects; a type relationship counts when
/// its target is such a declaration. A required edge is proven by one counted binding; a
/// forbidden one deviates on one. A forbidden call conforms only when every measured call
/// of the declaration is bound, in a document the producer covers completely. Absence of a
/// required edge never establishes a violation.
#[allow(clippy::too_many_arguments)]
fn constraint(
    config: &Config,
    targets: &Targets<'_>,
    anchors: &BTreeMap<&str, &crate::references::SemanticAnchor>,
    doc: &crate::references::DocumentInput,
    d: &Declaration,
    c: &Constraint,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<(&'static str, Option<&'static str>, Vec<Value>)> {
    use crate::references::SemanticOutcome;
    let role = &config.roles[&c.role];
    let bound = doc.semantic.producer_id.is_some();
    let calls = c.kind == "calls";
    let mut evidence = vec![native_evidence(d)];
    let (mut hits, mut uncertain) = (0usize, 0usize);
    let select = |document: &usize, sid: &str| {
        targets
            .get(&(*document, sid))
            .and_then(|target| selection(role, target, None))
    };
    if calls {
        for call in doc
            .calls
            .iter()
            .filter(|call| call.owner_syntax_id == d.syntax_id)
        {
            ensure!(work.charge(1), "bounded_work: rule evidence budget");
            check()?;
            evidence.push(json!({"document":call.document,"revisionId":call.revision_id,"range":call.range,"kind":"call","syntaxId":call.owner_syntax_id,"callId":call.id,"provenanceId":call.provenance_id}));
            if !bound {
                continue;
            }
            let Some(anchor) = anchors.get(call.id.as_str()) else {
                uncertain += 1;
                continue;
            };
            if anchor.metadata.contains_key("projection") {
                evidence.push(semantic_evidence(json!({"document":call.document,"revisionId":call.revision_id,"range":anchor.range,"kind":"semanticBinding","syntaxId":call.owner_syntax_id,"callId":call.id,"provenanceId":anchor.provenance_id}), &anchor.metadata));
            }
            match &anchor.outcome {
                SemanticOutcome::Internal(key, sid) if anchor.direct => match select(key, sid) {
                    Some(true) => {
                        hits += 1;
                        evidence.push(semantic_evidence(json!({"document":call.document,"revisionId":call.revision_id,"range":anchor.range,"kind":"semanticBinding","syntaxId":sid,"callId":call.id,"provenanceId":anchor.provenance_id}), &anchor.metadata));
                    }
                    Some(false) => {}
                    None => uncertain += 1,
                },
                SemanticOutcome::External => {}
                _ => uncertain += 1,
            }
        }
    } else if bound {
        for relationship in doc
            .semantic
            .relationships
            .iter()
            .filter(|r| r.source_syntax_id == d.syntax_id && r.kind == c.kind)
        {
            ensure!(work.charge(1), "bounded_work: rule evidence budget");
            check()?;
            if relationship.metadata.contains_key("projection") {
                evidence.push(semantic_evidence(json!({"document":d.document,"revisionId":d.revision_id,"range":d.range,"kind":"typeRelationship","syntaxId":d.syntax_id,"callId":null,"provenanceId":relationship.provenance_id}), &relationship.metadata));
            }
            let Some((key, sid)) = &relationship.target else {
                uncertain += usize::from(
                    relationship.metadata.get("semanticBoundary").as_deref() == Some(&json!(true)),
                );
                continue;
            };
            match select(key, sid) {
                Some(true) => {
                    hits += 1;
                    evidence.push(semantic_evidence(json!({"document":d.document,"revisionId":d.revision_id,"range":d.range,"kind":"typeRelationship","syntaxId":sid,"callId":null,"provenanceId":relationship.provenance_id}), &relationship.metadata));
                }
                Some(false) => {}
                None => uncertain += 1,
            }
        }
    }
    let (unavailable, incomplete, absent) = if calls {
        (
            "call_binding_unavailable",
            "call_binding_incomplete",
            "required_call_absent",
        )
    } else {
        (
            "type_relationship_unavailable",
            "type_relationship_incomplete",
            "type_relationship_absent",
        )
    };
    let (status, reason) = match (c.mode, bound, hits > 0) {
        (_, false, _) => ("unknown", Some(unavailable)),
        (_, _, false) if doc.semantic.projection.is_some() => {
            ("unknown", Some("historical_semantic_absence_unproven"))
        }
        ("require", _, true) => ("conforms", None),
        ("forbid", _, true) => ("deviates", None),
        ("forbid", _, false)
            if calls
                && uncertain == 0
                && doc.semantic.complete
                && doc.semantic.projection.is_none() =>
        {
            ("conforms", None)
        }
        _ if uncertain > 0 || (calls && !doc.semantic.complete) => ("unknown", Some(incomplete)),
        // Only a forbidden type relationship reaches here: absence is not proof.
        ("forbid", ..) => ("unknown", Some("type_relationship_absence_unproven")),
        _ => ("unknown", Some(absent)),
    };
    Ok((status, reason, evidence))
}
