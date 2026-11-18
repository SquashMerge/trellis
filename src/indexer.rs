//! Native-only graph projection from one immutable capture and validated #22 evidence.
//! Syntax never supplies a target, candidate edge, dispatch resolution or callback traversal.
use crate::{
    capture::Capture, model::*, native_evidence, store::admitted_capture::AdmittedCapture,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::atomic::Ordering,
};
use tree_sitter::Node;

pub use crate::model::{IndexOptions, ReconcileOptions};
// The graph projection and its write-time check are in the shared `graph_projection`.
#[cfg(test)]
use crate::graph_projection::graph_provenance;
pub(crate) use crate::graph_projection::{
    NativeDocument, compare_writer_calls, project_document, validate_native_graph_records,
};

fn root_id(root: &std::path::Path) -> Result<String> {
    Ok(crate::store::topology::WorkspaceIdentity::discover(Some(root), root)?.record_id)
}

pub fn index_workspace(
    options: &IndexOptions,
    cancel: &CancelFlag,
    progress: impl Fn(IndexProgress) + Sync,
) -> Result<Graph> {
    let (graph, _) = index_workspace_with_capture(options, cancel, progress)?;
    Ok(graph)
}

/// Return the original admitted descriptor for verification at the publication cutoff.
pub fn index_workspace_with_capture(
    options: &IndexOptions,
    cancel: &CancelFlag,
    progress: impl Fn(IndexProgress) + Sync,
) -> Result<(Graph, Capture)> {
    let capture = Capture::admit(options, cancel, &progress)?;
    let root = std::fs::canonicalize(&options.workspace_root)?;
    let native = native_evidence::from_capture(&capture, &root, &root_id(&root)?, cancel)?;
    let graph = project_native(options, &capture, &native, cancel, &progress)?;
    capture.verify(cancel)?;
    Ok((graph, capture))
}

/// Both views are built from the identical validated immutable capture.
pub fn index_workspace_with_native(
    options: &IndexOptions,
    root_id: &str,
    cancel: &CancelFlag,
    progress: impl Fn(IndexProgress) + Sync,
) -> Result<(Graph, native_evidence::Artifact)> {
    let capture = Capture::admit(options, cancel, &progress)?;
    let root = std::fs::canonicalize(&options.workspace_root)?;
    let native = native_evidence::from_capture(&capture, &root, root_id, cancel)?;
    let graph = project_native(options, &capture, &native, cancel, &progress)?;
    capture.verify(cancel)?;
    Ok((graph, native))
}

/// One admission produces the graph and native artifact for the same immutable capture.
pub fn index_workspace_bundle(
    options: &IndexOptions,
    root_id: &str,
    cancel: &CancelFlag,
    progress: impl Fn(IndexProgress) + Sync,
) -> Result<(Graph, native_evidence::Artifact, Capture)> {
    let capture = Capture::admit(options, cancel, &progress)?;
    let root = std::fs::canonicalize(&options.workspace_root)?;
    let native = native_evidence::from_capture(&capture, &root, root_id, cancel)?;
    let graph = project_native(options, &capture, &native, cancel, &progress)?;
    capture.verify(cancel)?;
    Ok((graph, native, capture))
}

/// Decision made before a delta writer may reuse any prior native or graph rows.
/// The old capture is an immutable *admitted* snapshot, not a live filesystem read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapturedChange {
    Unchanged,
    DocumentLocal { path: String },
    FullNative { reason: &'static str },
}

/// An intentionally narrow proof: only a literal in a return expression or a comment
/// inside a function body may change. Every other edit takes the full-native path.
/// This reads only the changed document's source bytes and never queries lookup owners.
fn proved_body_only(old: &SourceFile, new: &SourceFile) -> bool {
    // `Node::parent` walks from the root, so a climb is quadratic in depth. An edit deeper than
    // this takes the full-native path.
    const ANCESTRY_LIMIT: usize = 128;
    if old.path != new.path || old.language != new.language || old.text == new.text {
        return false;
    }
    let language = match old.language.as_str() {
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => crate::indexer_ts::grammar(&old.path),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        _ => return false,
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return false;
    }
    let (Some(before), Some(after)) =
        (parser.parse(&old.text, None), parser.parse(&new.text, None))
    else {
        return false;
    };
    if before.root_node().has_error() || after.root_node().has_error() {
        return false;
    }
    let a = old.text.as_bytes();
    let b = new.text.as_bytes();
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (a_end, b_end) = (a.len() - suffix, b.len() - suffix);
    // One side can be empty for a length-changing edit at the end of a
    // literal/comment. Anchor that side to its preceding byte; the unchanged
    // whole-tree shape and exact bytes outside both leaves still fence it.
    fn changed_leaf<'a>(root: Node<'a>, start: usize, end: usize) -> Option<Node<'a>> {
        // A zero-width side of a length-changing edit can lie at either leaf
        // edge. Inserting `1` before `70000` leaves the old-side edit at the
        // start of the numeric token, where probing only the prior byte sees
        // whitespace, not the measured literal. Check both adjacent leaves;
        // the matched kind, return-body ancestry, identical complete tree
        // shape and unchanged bytes outside the leaf are proved below.
        let probes = if start == end {
            [
                start.checked_sub(1).map(|previous| (previous, start)),
                (start < root.end_byte()).then_some((start, start + 1)),
            ]
        } else {
            [Some((start, end)), None]
        };
        for (probe_start, probe_end) in probes.into_iter().flatten() {
            let Some(mut node) = root.descendant_for_byte_range(probe_start, probe_end) else {
                continue;
            };
            while node.child_count() > 0 {
                let Some(child) = (0..node.child_count())
                    .filter_map(|i| node.child(i))
                    .find(|n| n.start_byte() <= probe_start && n.end_byte() >= probe_end)
                else {
                    break;
                };
                node = child;
            }
            if node.is_named()
                && matches!(
                    node.kind(),
                    "comment"
                        | "number"
                        | "integer"
                        | "integer_literal"
                        | "decimal_integer_literal"
                        | "string"
                        | "string_literal"
                        | "string_fragment"
                        | "raw_string_literal"
                        | "identifier"
                )
            {
                return Some(node);
            }
        }
        None
    }
    let Some(left) = changed_leaf(before.root_node(), prefix, a_end) else {
        return false;
    };
    let Some(right) = changed_leaf(after.root_node(), prefix, b_end) else {
        return false;
    };
    if left.kind() != right.kind() || !left.is_named() {
        return false;
    }
    let comment = left.kind() == "comment";
    let literal = matches!(
        left.kind(),
        "number"
            | "integer"
            | "integer_literal"
            | "decimal_integer_literal"
            | "string"
            | "string_literal"
            | "string_fragment"
            | "raw_string_literal"
    );
    // Returned identifiers are safe only when both names have a preceding
    // syntactic local binding in this function. An imported/global name could
    // change cross-file reference evidence even with the same tree shape.
    fn local_binding(node: Node<'_>, source: &[u8]) -> bool {
        if node.kind() != "identifier" {
            return false;
        }
        let name = &source[node.byte_range()];
        let mut parent = node.parent();
        let mut climbed = 0;
        let function = loop {
            let Some(current) = parent else {
                return false;
            };
            climbed += 1;
            if climbed > ANCESTRY_LIMIT {
                return false;
            }
            if matches!(
                current.kind(),
                "function_declaration"
                    | "function_definition"
                    | "function_item"
                    | "method_declaration"
            ) {
                break current;
            }
            parent = current.parent();
        };
        let Some(body) = function.child_by_field_name("body") else {
            return false;
        };
        fn is_binding(root: Node<'_>, name: &[u8], source: &[u8]) -> bool {
            let field = match root.kind() {
                "variable_declarator" => "name",
                "let_declaration" => "pattern",
                "assignment" => "left",
                _ => "",
            };
            !field.is_empty()
                && root.child_by_field_name(field).is_some_and(|bound| {
                    bound.kind() == "identifier" && &source[bound.byte_range()] == name
                })
        }
        // Iterative: repository nesting has no bound here.
        let mut pending = vec![body];
        while let Some(root) = pending.pop() {
            if root.start_byte() >= node.start_byte() {
                continue;
            }
            if is_binding(root, name, source) {
                return true;
            }
            let mut cursor = root.walk();
            pending.extend(root.children(&mut cursor));
        }
        false
    }
    let returned_identifier =
        left.kind() == "identifier" && local_binding(left, a) && local_binding(right, b);
    if !comment && !literal && !returned_identifier {
        return false;
    }
    fn safe_ancestry(mut node: Node<'_>, comment: bool, language: &str) -> bool {
        let mut body = false;
        let mut function = false;
        let mut returned = false;
        let mut climbed = 0;
        while let Some(parent) = node.parent() {
            climbed += 1;
            if climbed > ANCESTRY_LIMIT {
                return false;
            }
            let kind = parent.kind();
            // Rust's final bare expression is a returned value, but only when
            // the edited leaf itself is the last named child of this exact
            // function body's block. Headers, calls and nested expressions do
            // not gain a new proof from this rule.
            if language == "rust" && kind == "block" {
                let mut cursor = parent.walk();
                returned |= parent
                    .named_children(&mut cursor)
                    .last()
                    .is_some_and(|last| last.id() == node.id())
                    && parent.parent().is_some_and(|owner| {
                        owner.kind() == "function_item"
                            && owner
                                .child_by_field_name("body")
                                .is_some_and(|body| body.id() == parent.id())
                    });
            }
            if kind.contains("import")
                || (kind.contains("export")
                    && !(kind == "export_statement" && body && function && (comment || returned)))
                || (kind.contains("class")
                    && !matches!(
                        kind,
                        "class_body" | "class_declaration" | "class_definition"
                    ))
                || kind.contains("super")
                || kind.contains("call")
                || kind.contains("invocation")
                || (kind.contains("declaration")
                    && !matches!(kind, "class_declaration")
                    && !kind.contains("function")
                    && !kind.contains("method"))
                || kind.contains("parameter")
                || kind.contains("assignment")
                || kind.contains("attribute")
                || kind.contains("decorator")
                || kind.contains("type_annotation")
                || kind == "ERROR"
            {
                return false;
            }
            body |= matches!(kind, "statement_block" | "block");
            function |= kind.contains("function") || kind.contains("method");
            returned |= matches!(kind, "return_statement" | "return_expression");
            node = parent;
        }
        body && function && (comment || returned)
    }
    if !safe_ancestry(left, comment, &old.language) || !safe_ancestry(right, comment, &new.language)
    {
        return false;
    }
    // Identical tree shape is necessary, not sufficient: the exact bytes outside the
    // one measured leaf must also be unchanged, including the enclosing header/scope.
    // Iterative: repository nesting has no bound here.
    fn shape<'tree>(root: Node<'tree>, result: &mut Vec<&'tree str>) {
        let mut cursor = root.walk();
        loop {
            result.push(cursor.node().kind());
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                result.push("/");
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return;
                }
            }
        }
    }
    let mut old_shape = vec![];
    let mut new_shape = vec![];
    shape(before.root_node(), &mut old_shape);
    shape(after.root_node(), &mut new_shape);
    old_shape == new_shape
        && a[..left.start_byte()] == b[..right.start_byte()]
        && a[left.end_byte()..] == b[right.end_byte()..]
}

/// The only scan outside the changed document compares path/language/capture hashes.
/// A prior failed or ambiguous lookup in an unchanged file is deliberately not read.
/// The caller supplies two captures admitted with the same root; admission/config and
/// producer equality must be checked separately by the reuse fingerprint below.
pub fn measure_captured_change(previous: &Capture, current: &Capture) -> CapturedChange {
    measure_captured_change_observed(previous, current, |_| {})
}

/// Classifier candidate callback only. This is NOT a native extraction witness;
/// the separate selected native stage observes only after successful assembly.
pub fn measure_captured_change_observed(
    previous: &Capture,
    current: &Capture,
    mut visit: impl FnMut(&str),
) -> CapturedChange {
    let old_options = previous.reconcile_options();
    let new_options = current.reconcile_options();
    if old_options != new_options {
        return CapturedChange::FullNative {
            reason: "capture admission changed",
        };
    }
    let (Ok(previous_native), Ok(current_native)) = (
        previous.native_input_fingerprints(),
        current.native_input_fingerprints(),
    ) else {
        return CapturedChange::FullNative {
            reason: "native input authentication unavailable",
        };
    };
    if previous_native != current_native {
        return CapturedChange::FullNative {
            reason: "captured native input changed",
        };
    }
    let before: BTreeMap<_, _> = previous
        .files
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();
    let after: BTreeMap<_, _> = current.files.iter().map(|f| (f.path.as_str(), f)).collect();
    if before.keys().ne(after.keys()) {
        return CapturedChange::FullNative {
            reason: "source inventory changed (add/delete/rename)",
        };
    }
    let mut changed = None;
    for (path, old) in before {
        let new = after[path];
        if old.language != new.language {
            return CapturedChange::FullNative {
                reason: "source language changed",
            };
        }
        if old.hash == new.hash {
            continue;
        }
        if changed.is_some() {
            return CapturedChange::FullNative {
                reason: "multiple source documents changed",
            };
        }
        changed = Some((path, old, new));
    }
    match changed {
        None => CapturedChange::Unchanged,
        Some((path, old, new)) => {
            visit(path);
            if proved_body_only(old, new) {
                CapturedChange::DocumentLocal {
                    path: path.to_owned(),
                }
            } else {
                CapturedChange::FullNative {
                    reason: "cross-file effect not proved local",
                }
            }
        }
    }
}

/// Classification for the pinned prior manifest. Prior files and input observations
/// come from one verified SQLite snapshot, never from a live old workspace walk.
pub(crate) fn measure_persisted_change(
    previous: &[SourceFile],
    previous_options: &ReconcileOptions,
    previous_inputs: &BTreeMap<String, crate::capture::CaptureInputObservation>,
    current: &dyn AdmittedCapture,
) -> Result<Option<BTreeSet<String>>> {
    if previous_options != current.reconcile_options() {
        return Ok(None);
    }
    let current_inputs = current.persisted_inputs()?;
    let native_inputs = |inputs: &BTreeMap<String, crate::capture::CaptureInputObservation>| {
        inputs
            .iter()
            .filter(|(key, _)| {
                key.starts_with("config:")
                    || key.starts_with("toolchain:")
                    || key.starts_with("ignore:")
                    || *key == crate::capture::EXTRACTOR_INPUT_KEY
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    if native_inputs(previous_inputs) != native_inputs(&current_inputs) {
        return Ok(None);
    }
    let before: BTreeMap<_, _> = previous.iter().map(|f| (f.path.as_str(), f)).collect();
    let after: BTreeMap<_, _> = current
        .files()
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();
    let changed = before
        .keys()
        .chain(after.keys())
        .filter(|path| match (before.get(**path), after.get(**path)) {
            (Some(old), Some(new)) => old.language != new.language || old.hash != new.hash,
            _ => true,
        })
        .map(|path| (*path).to_owned())
        .collect();
    Ok(Some(changed))
}

/// A diagnostic selected-document measurement for two caller-supplied captures.
/// Publication uses a separate authenticated persisted-manifest path; unproved
/// edits still use the full native fallback.
#[derive(Debug)]
pub struct StagedNativeMeasurement {
    pub decision: CapturedChange,
    pub selected: Option<native_evidence::SelectedDocument>,
}

pub fn measure_captured_native_change(
    previous: &Capture,
    current: &Capture,
    root: &Path,
    root_id: &str,
    cancel: &CancelFlag,
    on_extract: impl FnMut(&native_evidence::DocumentKey),
) -> Result<StagedNativeMeasurement> {
    let decision = measure_captured_change(previous, current);
    let selected = match &decision {
        CapturedChange::DocumentLocal { path } => Some(native_evidence::measure_captured_document(
            current, root, root_id, path, cancel, on_extract,
        )?),
        _ => None,
    };
    Ok(StagedNativeMeasurement { decision, selected })
}

/// Fingerprints are internal reuse *conditions*, not a new public source of authority.
/// The native fingerprint excludes revision-scoped IDs only; it includes authenticated
/// bytes, admission/config/toolchain, producer, coverage roles and measured owners.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentFingerprint {
    pub native: String,
    pub projection: String,
}
impl DocumentFingerprint {
    pub fn reusable_native(&self, other: &Self) -> bool {
        self.native == other.native
    }
    pub fn reusable_projection(&self, other: &Self) -> bool {
        self == other
    }
}

/// Called only with the artifact and graph from one validated captured bundle; a later
/// writer must also attest any stored candidate before comparing these fingerprints.
pub fn measure_document_fingerprint(
    capture: &Capture,
    native: &native_evidence::Artifact,
    graph: &Graph,
    path: &str,
    _options: &IndexOptions,
) -> Result<DocumentFingerprint> {
    let file = capture
        .files
        .iter()
        .find(|f| f.path == path)
        .context("missing captured document")?;
    ensure!(
        hex::encode(Sha256::digest(file.text.as_bytes())) == file.hash
            && capture.hashes.get(path) == Some(&file.hash),
        "captured document hash mismatch"
    );
    let document = native
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == path)
        .context("missing measured native document")?;
    ensure!(
        document.key.language == file.language
            && document.key.source_set_id == native.source_set.id
            && document.content_hash == file.hash
            && document.byte_length == file.text.len()
            && document.revision_id == native.revision.id,
        "native document is not the captured bytes"
    );
    let coverage: Vec<_> = native
        .coverage
        .iter()
        .filter(|v| v.document_path == path)
        .collect();
    ensure!(coverage.len() == 1, "missing or duplicate native coverage");
    let coverage = coverage[0];
    ensure!(
        coverage.language == file.language
            && coverage.source_set_id == native.source_set.id
            && coverage.producer_id == native.producer.id
            && coverage.revision_id == native.revision.id,
        "native coverage ownership mismatch"
    );
    // Do not carry occurrence revision/proof IDs into a reusable version's identity.
    // Every other field, including owner/key/roles and all distinct source facts, stays.
    fn stable_fact<T: Serialize>(fact: &T) -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(fact)?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("revisionId");
            obj.remove("provenanceId");
        }
        Ok(value)
    }
    let declarations = native
        .declarations
        .iter()
        .filter(|d| d.document == document.key)
        .map(stable_fact)
        .collect::<Result<Vec<_>>>()?;
    let calls = native
        .calls
        .iter()
        .filter(|c| c.document == document.key)
        .map(stable_fact)
        .collect::<Result<Vec<_>>>()?;
    let regions = native
        .control_regions
        .iter()
        .filter(|r| r.document == document.key)
        .map(stable_fact)
        .collect::<Result<Vec<_>>>()?;
    let context = crate::native_records::native_extraction_context(
        &native.producer,
        &file.language,
        &native.revision,
    )?;
    let native_value = serde_json::json!({
        "sourceSetId":native.source_set.id,"language":file.language,"path":file.path,
        "contentHash":file.hash,"byteLength":file.text.len(),"extractionContext":context,
        "producer":native.producer,"toolchainHash":native.revision.toolchain_hash,
        "configHash":native.revision.config_hash,"dependencyHash":native.revision.dependency_hash,
        "coverage":stable_fact(coverage)?,"declarations":declarations,"calls":calls,"regions":regions,
    });
    let native_hash = crate::native_ids::digest(
        b"trellis.local-native-fingerprint.v1\0",
        &crate::native_ids::canonical(&native_value),
    );
    let nodes: Vec<_> = graph.nodes.iter().filter(|n| n.path == path).collect();
    let graph_calls: Vec<_> = graph.calls.iter().filter(|c| c.path == path).collect();
    let graph_regions: Vec<_> = graph.regions.iter().filter(|r| r.path == path).collect();
    let projection_value = serde_json::json!({
        "nativeFingerprint":&native_hash,"nodes":nodes,"calls":graph_calls,"regions":graph_regions,
    });
    Ok(DocumentFingerprint {
        native: native_hash,
        projection: crate::native_ids::digest(
            b"trellis.local-graph-fingerprint.v1\0",
            &crate::native_ids::canonical(&projection_value),
        ),
    })
}

pub(crate) fn project_native(
    options: &IndexOptions,
    capture: &Capture,
    native: &native_evidence::Artifact,
    cancel: &CancelFlag,
    progress: &impl Fn(IndexProgress),
) -> Result<Graph> {
    project_native_selected(options, capture, native, cancel, progress, None)
}

/// The fast local path projects only the fully validated changed document.
/// The writer retains prior projection IDs for every unchanged manifest entry.
pub(crate) fn project_native_document(
    options: &IndexOptions,
    capture: &Capture,
    native: &native_evidence::Artifact,
    path: &str,
    cancel: &CancelFlag,
    progress: &impl Fn(IndexProgress),
) -> Result<Graph> {
    ensure!(
        native.declarations.iter().all(|d| d.document.path == path)
            && native.calls.iter().all(|c| c.document.path == path)
            && native
                .control_regions
                .iter()
                .all(|r| r.document.path == path)
            && native.coverage.len() == 1
            && native.coverage[0].document_path == path,
        "selected native projection contains another document"
    );
    project_native_selected(
        options,
        capture,
        native,
        cancel,
        progress,
        Some(&BTreeSet::from([path.to_owned()])),
    )
}

pub(crate) fn project_native_documents(
    options: &IndexOptions,
    capture: &Capture,
    native: &native_evidence::Artifact,
    paths: &BTreeSet<String>,
    cancel: &CancelFlag,
    progress: &impl Fn(IndexProgress),
) -> Result<Graph> {
    project_native_selected(options, capture, native, cancel, progress, Some(paths))
}

fn project_native_selected(
    _options: &IndexOptions,
    capture: &Capture,
    native: &native_evidence::Artifact,
    cancel: &CancelFlag,
    progress: &impl Fn(IndexProgress),
    selected: Option<&BTreeSet<String>>,
) -> Result<Graph> {
    ensure!(!cancel.load(Ordering::Relaxed), "indexing cancelled");
    if selected.is_none() {
        capture.claim_graph_projection()?;
    }
    let selected_files: Vec<_> = capture
        .files
        .iter()
        .filter(|f| selected.is_none_or(|paths| paths.contains(&f.path)))
        .cloned()
        .collect();
    ensure!(
        selected.is_none_or(|paths| selected_files.len() == paths.len()),
        "selected graph source is missing or ambiguous"
    );
    let mut graph = Graph {
        files: selected_files.clone(),
        ..Graph::default()
    };
    let mut by_path: BTreeMap<&str, NativeDocument<'_>> = selected_files
        .iter()
        .map(|file| (file.path.as_str(), NativeDocument::default()))
        .collect();
    for d in &native.declarations {
        by_path
            .get_mut(d.document.path.as_str())
            .context("missing graph source")?
            .declarations
            .push(d);
    }
    for c in &native.calls {
        by_path
            .get_mut(c.document.path.as_str())
            .context("missing call source")?
            .calls
            .push(c);
    }
    for r in &native.control_regions {
        by_path
            .get_mut(r.document.path.as_str())
            .context("missing region source")?
            .control_regions
            .push(r);
    }
    for file in &selected_files {
        let facts = by_path
            .remove(file.path.as_str())
            .context("missing graph source")?;
        let (nodes, calls, regions) = project_document(&file.path, file.text.as_bytes(), facts)?;
        graph.nodes.extend(nodes);
        graph.calls.extend(calls);
        graph.regions.extend(regions);
    }
    for coverage in &native.coverage {
        if coverage.state != "complete" {
            let recovered = coverage
                .diagnostic
                .as_deref()
                .is_some_and(|message| message.contains("parser recovered"));
            if recovered {
                graph.stats.parse_error_files += 1;
            }
            graph.diagnostics.push(Diagnostic {
                path: Some(coverage.document_path.clone()),
                code: if recovered {
                    "parse-error"
                } else {
                    "native-coverage-partial"
                }
                .into(),
                message: coverage
                    .diagnostic
                    .clone()
                    .unwrap_or_else(|| "Native extraction is incomplete".into()),
            });
        }
    }
    graph.nodes.sort_by(|a, b| a.id.cmp(&b.id));
    graph.calls.sort_by(compare_writer_calls);
    graph.regions.sort_by(|a, b| a.id.cmp(&b.id));
    graph.stats.files = graph.files.len();
    graph.stats.symbols = graph.nodes.len();
    graph.stats.calls = graph.calls.len();
    graph.stats.regions = graph.regions.len();
    graph.stats.unresolved = graph.calls.len();
    for i in 0..graph.files.len() {
        ensure!(!cancel.load(Ordering::Relaxed), "indexing cancelled");
        progress(IndexProgress {
            phase: "parse".into(),
            completed: i + 1,
            total: graph.files.len(),
        });
    }
    ensure!(!cancel.load(Ordering::Relaxed), "indexing cancelled");
    progress(IndexProgress {
        phase: "complete".into(),
        completed: graph.files.len(),
        total: graph.files.len(),
    });
    Ok(graph)
}

/// Verify every public graph row against the exact captured native syntax projection.
pub fn validate_native_graph(
    graph: &Graph,
    capture: &dyn AdmittedCapture,
    native: &native_evidence::Artifact,
    cancel: &CancelFlag,
) -> Result<()> {
    ensure!(
        graph.files == capture.files(),
        "native_evidence_required: graph source differs from capture"
    );
    validate_native_graph_records(graph, native, cancel)
}

/// Closed #22 syntax categories measured by the JavaScript adapter.
pub(crate) fn native_js_kind(n: Node<'_>) -> Option<&'static str> {
    Some(match n.kind() {
        "class_declaration" | "class" => "type",
        "method_definition" => "method",
        "function_declaration"
        | "function_expression"
        | "generator_function_declaration"
        | "generator_function" => "function",
        "arrow_function" => "anonymousFunction",
        "variable_declarator" => "variable",
        "field_definition" => "field",
        "formal_parameter" => "parameter",
        "import_specifier"
            if n.child_by_field_name("name")
                .is_some_and(|n| n.kind() == "identifier")
                && n.child_by_field_name("alias")
                    .is_some_and(|n| n.kind() == "identifier") =>
        {
            "alias"
        }
        _ => return None,
    })
}

#[cfg(test)]
mod local_classification_tests {
    use super::proved_body_only;
    use crate::model::SourceFile;

    fn file(language: &str, text: &str) -> SourceFile {
        SourceFile {
            path: format!("selected.{language}"),
            language: language.into(),
            hash: String::new(),
            text: text.into(),
        }
    }

    fn typescript(path: &str, text: &str) -> SourceFile {
        SourceFile {
            path: path.into(),
            ..file("typescript", text)
        }
    }

    #[test]
    fn typescript_body_literal_edits_are_proved_under_both_grammars() {
        for path in ["selected.ts", "selected.tsx"] {
            assert!(
                proved_body_only(
                    &typescript(path, "function work(a: number): number { return 1; }"),
                    &typescript(path, "function work(a: number): number { return 10000; }"),
                ),
                "{path}: a returned literal is local"
            );
            assert!(
                !proved_body_only(
                    &typescript(path, "function work(a: number): number { return 1; }"),
                    &typescript(path, "function work(a: number): string { return 1; }"),
                ),
                "{path}: a header edit needs the full path"
            );
        }
        assert!(
            !proved_body_only(
                &typescript("selected.ts", "function work() { return 1; }"),
                &typescript("selected.tsx", "function work() { return 10000; }"),
            ),
            "a path change is not a body edit"
        );
    }

    #[test]
    fn the_shape_proof_is_iterative() {
        let deep = format!("const v = {}{};", "[".repeat(200_000), "]".repeat(200_000));
        let before = format!("function f() {{ return 1; }}\n{deep}\n");
        let after = format!("function f() {{ return 2; }}\n{deep}\n");
        assert!(proved_body_only(
            &file("javascript", &before),
            &file("javascript", &after)
        ));
    }

    #[test]
    fn length_changing_method_and_function_bodies_are_local_only_with_identical_surface() {
        for (language, before, after) in [
            (
                "javascript",
                "function work() { return 1; }",
                "function work() { return 10000; }",
            ),
            (
                "java",
                "class A { int work() { return 1; } }",
                "class A { int work() { return 10000; } }",
            ),
            (
                "java",
                "public class Cmedium0000 { public static int f0(){return 70000;} }",
                "public class Cmedium0000 { public static int f0(){return 170000;} }",
            ),
            (
                "javascript",
                "export function f0(){return 7000;}",
                "export function f0(){return 17000;}",
            ),
            ("rust", "pub fn f0()->i32{100}", "pub fn f0()->i32{1100}"),
            (
                "python",
                "def work():\n    return 1\n",
                "def work():\n    return 10000\n",
            ),
            (
                "rust",
                "fn work() -> u64 { return 1; }",
                "fn work() -> u64 { return 10000; }",
            ),
        ] {
            assert!(
                proved_body_only(&file(language, before), &file(language, after)),
                "{language}: a single longer literal in the body is local"
            );
            assert!(
                proved_body_only(&file(language, after), &file(language, before)),
                "{language}: a single shorter literal in the body is local"
            );
        }
    }

    #[test]
    fn exported_function_and_rust_tail_changes_still_reject_headers_imports_and_calls() {
        for (language, before, after) in [
            (
                "javascript",
                "export function f0(){return 7000;}",
                "export function renamed(){return 7000;}",
            ),
            (
                "javascript",
                "export function f0(){return old();}",
                "export function f0(){return other();}",
            ),
            (
                "javascript",
                "export function f0(){return 7000;}",
                "import {f0} from './peer.js'; export function f0(){return 7000;}",
            ),
            (
                "rust",
                "pub fn f0()->i32{100}",
                "pub fn renamed()->i32{100}",
            ),
            ("rust", "pub fn f0()->i32{100}", "pub fn f0()->i64{100}"),
            (
                "rust",
                "pub fn f0()->i32{old()}",
                "pub fn f0()->i32{other()}",
            ),
        ] {
            assert!(
                !proved_body_only(&file(language, before), &file(language, after)),
                "{language}: unproved exported/tail surface must take FULL"
            );
        }
    }

    #[test]
    fn noncallee_return_identifiers_change_only_the_measured_document() {
        for (language, before, after) in [
            (
                "javascript",
                "function f() { let local=1, other=2; return local+1; }",
                "function f() { let local=1, other=2; return other+1; }",
            ),
            (
                "java",
                "class A { int f() { int local=1, other=2; return local+1; } }",
                "class A { int f() { int local=1, other=2; return other+1; } }",
            ),
            (
                "python",
                "def f():\n    local=1; other=2\n    return local+1\n",
                "def f():\n    local=1; other=2\n    return other+1\n",
            ),
            (
                "rust",
                "fn f()->i32 { let local=1; let other=2; return local+1; }",
                "fn f()->i32 { let local=1; let other=2; return other+1; }",
            ),
        ] {
            assert!(
                proved_body_only(&file(language, before), &file(language, after)),
                "{language}: returned non-callee identifier is document local"
            );
        }
        assert!(
            !proved_body_only(
                &file(
                    "python",
                    "from a import peer\nfrom b import other\ndef f():\n    return peer\n"
                ),
                &file(
                    "python",
                    "from a import peer\nfrom b import other\ndef f():\n    return other\n"
                )
            ),
            "imported/global return references need FULL fallback"
        );
        assert!(
            !proved_body_only(
                &file("javascript", "function f() { return caller(); }"),
                &file("javascript", "function f() { return other(); }")
            ),
            "callee edits remain FULL fallback even with identical AST shape"
        );
    }

    #[test]
    fn declaration_and_cross_file_edges_never_gain_local_permission() {
        for (language, before, after) in [
            (
                "javascript",
                "function work(a) { return 1; }",
                "function work(abc) { return 1; }",
            ),
            (
                "java",
                "class A extends B { int work() { return 1; } }",
                "class A extends XYZ { int work() { return 1; } }",
            ),
            (
                "python",
                "from a import T\ndef work():\n    return 1\n",
                "from b import T\ndef work():\n    return 1\n",
            ),
            (
                "rust",
                "fn work() -> u64 { return 1; }",
                "fn work() -> i64 { return 1; }",
            ),
            (
                "javascript",
                "function work() { return 1; }",
                "function work() { return called(); }",
            ),
        ] {
            assert!(
                !proved_body_only(&file(language, before), &file(language, after)),
                "{language}: declaration, dependency or tree-shape changes need FULL"
            );
        }
    }

    #[test]
    #[ignore = "AST-only frozen 40-path preflight; never count as benchmark evidence"]
    fn frozen_medium_large_mixed_edits_have_proved_local_ast_body_shape() {
        use sha2::{Digest, Sha256};
        use std::{fs, process::Command};
        let temp = tempfile::tempdir().unwrap();
        let corpus = temp.path().join("frozen");
        let generated = Command::new("node")
            .arg("tools/synthetic-cohorts/generate.mjs")
            .arg("--out")
            .arg(&corpus)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(
            generated.status.success(),
            "pinned generator: {}",
            String::from_utf8_lossy(&generated.stderr)
        );
        assert_eq!(
            hex::encode(Sha256::digest(
                fs::read(corpus.join("manifest.json")).unwrap()
            )),
            "ab4a058fac22dd9f324a476990755a3af07ab32f977f32c977d6eeea79627259"
        );
        let mut paths = 0;
        for size in ["medium", "large"] {
            for ordinal in 0..5 {
                for (language, extension) in [
                    ("java", "java"),
                    ("python", "py"),
                    ("javascript", "js"),
                    ("rust", "rs"),
                ] {
                    let path = if language == "rust" {
                        corpus.join(format!("{size}/rust/g00/C{size}{ordinal:04}.{extension}"))
                    } else {
                        corpus.join(format!("{size}/{language}/C{size}{ordinal:04}.{extension}"))
                    };
                    let before = fs::read_to_string(&path).unwrap();
                    let after = if ordinal >= 3 {
                        let peer = if matches!(language, "java" | "javascript") {
                            "peerValue"
                        } else {
                            "peer_value"
                        };
                        let marker = format!("return local+{peer}");
                        assert!(before.contains(&marker), "{path:?}");
                        before.replacen(&marker, &format!("return {peer}+{peer}"), 1)
                    } else {
                        let marker = match language {
                            "java" => "public static int f0(){return ",
                            "python" => "def f0(): return ",
                            "javascript" => "export function f0(){return ",
                            "rust" => "pub fn f0()->i32{",
                            _ => unreachable!(),
                        };
                        let begin = before.find(marker).unwrap() + marker.len();
                        let end = begin
                            + before[begin..]
                                .bytes()
                                .take_while(u8::is_ascii_digit)
                                .count();
                        let old = &before[begin..end];
                        let value = old.parse::<u64>().unwrap()
                            + if ordinal == 1 {
                                1
                            } else {
                                10_u64.pow(old.len() as u32)
                            };
                        format!("{}{}{}", &before[..begin], value, &before[end..])
                    };
                    assert_ne!(before, after, "{path:?}: real changed body");
                    assert!(
                        proved_body_only(&file(language, &before), &file(language, &after)),
                        "{path:?}: frozen mixed edit AST must be locally proved"
                    );
                    paths += 1;
                }
            }
        }
        assert_eq!(paths, 40);
    }
}

#[cfg(test)]
mod projection_document_tests {
    use super::{NativeDocument, compare_reader_calls, compare_writer_calls, project_document};
    use crate::model::{CallSite, SourceRange};
    use crate::native_evidence::{Declaration, DeclarationRole, DocumentKey, Header, Key, Range};

    fn declaration(path: &str, language: &str, id: &str, child: bool) -> Declaration {
        let parent_key = Key {
            kind: "module".into(),
            name: Some(path.into()),
            signature: None,
            ordinal: 0,
        };
        Declaration {
            role: DeclarationRole::Production,
            syntax_id: id.into(),
            document: DocumentKey {
                source_set_id: "source-set".into(),
                language: language.into(),
                path: path.into(),
            },
            revision_id: "revision".into(),
            kind: if child { "function" } else { "module" }.into(),
            name: child.then(|| "nested".into()),
            lookup_key: None,
            ancestors: if child {
                vec![parent_key.clone()]
            } else {
                vec![]
            },
            key: if child {
                Key {
                    kind: "function".into(),
                    name: Some("nested".into()),
                    signature: None,
                    ordinal: 0,
                }
            } else {
                parent_key
            },
            range: Range { start: 0, end: 3 },
            name_range: None,
            header: Header {
                kind: if child { "function" } else { "module" }.into(),
                name: None,
                modifiers: vec![],
                type_parameters: vec![],
                parameters: vec![],
                result_type: None,
                bases: vec![],
            },
            provenance_id: "native".into(),
        }
    }

    #[test]
    fn child_before_parent_syntax_id_and_input_order_preserve_graph_rows() {
        for (path, language) in [
            ("a.rs", "rust"),
            ("a.js", "javascript"),
            ("a.py", "python"),
            ("A.java", "java"),
        ] {
            // The SQL read's syntax_id order is child first, unlike extraction order.
            let parent = declaration(
                path,
                language,
                "sid:v1:ffffffffffffffffffffffffffffffff",
                false,
            );
            let child = declaration(
                path,
                language,
                "sid:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                true,
            );
            let project = |declarations: Vec<&Declaration>| {
                project_document(
                    path,
                    b"abc",
                    NativeDocument {
                        declarations,
                        calls: vec![],
                        control_regions: vec![],
                    },
                )
                .unwrap()
            };
            let writer = project(vec![&parent, &child]);
            let sql = project(vec![&child, &parent]);
            assert_eq!(
                serde_json::to_vec(&writer).unwrap(),
                serde_json::to_vec(&sql).unwrap()
            );
            assert_eq!(sql.0[0].parent.as_deref(), Some(parent.syntax_id.as_str()));
            assert_eq!(sql.0[0].id, child.syntax_id);
            assert_eq!(sql.0[1].name, path);
        }
    }

    #[test]
    fn synthetic_call_site_sort_tie_breaks_by_bytewise_id_without_native_validation() {
        // This tests DTO comparators directly. These synthetic rows do not claim
        // to be native-validated artifacts or main-produced payload goldens.
        let make_call = |id: &str, end_byte: usize| CallSite {
            id: id.into(),
            caller: "sid:v1:ffffffffffffffffffffffffffffffff".into(),
            callee_text: None,
            path: "a.rs".into(),
            range: SourceRange {
                start_byte: 0,
                end_byte,
                ..SourceRange::default()
            },
            callee_range: None,
            ordinal: 0,
            regions: vec![],
            provenance: super::graph_provenance(),
        };
        let last = make_call("occ:v2:ffffffffffffffffffffffffffffffff", 2);
        let first = make_call("occ:v2:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 2);
        let longest = make_call("occ:v2:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 3);
        let mut writer = [last.clone(), longest.clone(), first.clone()];
        writer.sort_by(compare_writer_calls);
        assert_eq!(
            writer
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec![first.id.as_str(), last.id.as_str(), longest.id.as_str()]
        );
        let mut reader = [last.clone(), longest.clone(), first.clone()];
        reader.sort_by(compare_reader_calls);
        assert_eq!(
            reader
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec![longest.id.as_str(), first.id.as_str(), last.id.as_str()]
        );
    }

    #[test]
    fn duplicate_owner_key_and_missing_parent_are_explicit_failures() {
        let parent = declaration(
            "a.rs",
            "rust",
            "sid:v1:ffffffffffffffffffffffffffffffff",
            false,
        );
        let mut duplicate = parent.clone();
        duplicate.syntax_id = "sid:v1:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into();
        let child = declaration(
            "a.rs",
            "rust",
            "sid:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            true,
        );
        let project = |declarations: Vec<&Declaration>| {
            project_document(
                "a.rs",
                b"abc",
                NativeDocument {
                    declarations,
                    calls: vec![],
                    control_regions: vec![],
                },
            )
            .unwrap_err()
            .to_string()
        };
        assert_eq!(
            project(vec![&parent, &duplicate]),
            "native declaration key collision"
        );
        assert_eq!(project(vec![&child]), "native declaration parent missing");
    }
}
