//! One document's graph rows, derived from its measured native facts (`project_document`),
//! and the write-time check that a graph's rows are exactly that projection
//! (`validate_native_graph_records`). Pure over model and native records: no capture, I/O
//! or parser. Publication and selected reads project with it, and publication checks with
//! it (owner ruling 2026-11-17, D2). `crate::indexer` re-exports the crate-visible items and
//! keeps `validate_native_graph`, which compares the graph's sources with the capture.
use crate::{model::*, native_records};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, sync::atomic::Ordering};

/// Byte offsets where each line starts, built once per file so every range maps by binary
/// search instead of rescanning the file prefix (#86).
struct LineIndex(Vec<usize>);
impl LineIndex {
    fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self(starts)
    }
    /// 1-based line and 1-based byte column; only `\n` ends a line.
    fn location(&self, text: &str, byte: usize) -> Result<(usize, usize)> {
        ensure!(
            byte <= text.len() && text.is_char_boundary(byte),
            "invalid measured graph range"
        );
        let line = self.0.partition_point(|&start| start <= byte);
        Ok((line, byte - self.0[line - 1] + 1))
    }
}
fn line_indexes<'a>(files: &BTreeMap<&'a str, &SourceFile>) -> BTreeMap<&'a str, LineIndex> {
    files
        .iter()
        .map(|(path, file)| (*path, LineIndex::new(&file.text)))
        .collect()
}
fn measured_range(
    file: &SourceFile,
    lines: &BTreeMap<&str, LineIndex>,
    range: &native_records::Range,
) -> Result<SourceRange> {
    let lines = lines
        .get(file.path.as_str())
        .context("missing measured source lines")?;
    measured_range_text(&file.text, lines, range)
}

fn measured_range_text(
    text: &str,
    lines: &LineIndex,
    range: &native_records::Range,
) -> Result<SourceRange> {
    let (start_line, start_column) = lines.location(text, range.start)?;
    let (end_line, end_column) = lines.location(text, range.end)?;
    ensure!(range.start <= range.end, "inverted measured graph range");
    Ok(SourceRange {
        start_byte: range.start,
        end_byte: range.end,
        start_line,
        start_column,
        end_line,
        end_column,
    })
}
fn graph_kind(kind: &str) -> Result<SymbolKind> {
    // Exhaust the validated #22 declaration vocabulary. Unknown kinds must fail closed,
    // rather than silently inventing a callable graph node.
    Ok(match kind {
        "module" | "namespace" => SymbolKind::Module,
        "type" | "implementation" => SymbolKind::Class,
        "method" | "constructor" => SymbolKind::Method,
        "function" | "anonymousFunction" => SymbolKind::Function,
        "field" => SymbolKind::Field,
        "variable" => SymbolKind::Variable,
        "parameter" => SymbolKind::Parameter,
        "typeParameter" => SymbolKind::TypeParameter,
        "alias" => SymbolKind::Alias,
        "variant" => SymbolKind::Variant,
        _ => anyhow::bail!("unknown native declaration kind: {kind}"),
    })
}
pub(crate) fn graph_provenance() -> Provenance {
    Provenance {
        projection: None,
        source: "native-measuredSyntax".into(),
        semantic: SemanticState::Unavailable,
    }
}

/// Borrowed facts for exactly one document. Group the writer's full artifact by
/// path once; selected SQL readers can borrow their attested per-document rows.
#[derive(Default)]
pub(crate) struct NativeDocument<'a> {
    pub declarations: Vec<&'a native_records::Declaration>,
    pub calls: Vec<&'a native_records::Call>,
    pub control_regions: Vec<&'a native_records::ControlRegion>,
}

/// Writer order for derived calls: path, start byte, end byte, then ID.
/// The ID breaks ties independently of native extraction order.
pub(crate) fn compare_writer_calls(a: &CallSite, b: &CallSite) -> std::cmp::Ordering {
    (&a.path, a.range.start_byte, a.range.end_byte, &a.id).cmp(&(
        &b.path,
        b.range.start_byte,
        b.range.end_byte,
        &b.id,
    ))
}

/// The sole graph-row derivation for both publication and selected reads.
/// No optional SCIP input or other document affects these measured rows.
pub(crate) fn project_document(
    path: &str,
    source: &[u8],
    native: NativeDocument<'_>,
) -> Result<(Vec<Symbol>, Vec<CallSite>, Vec<ControlRegion>)> {
    let text = std::str::from_utf8(source).context("invalid graph source UTF-8")?;
    let lines = LineIndex::new(text);
    let range = |r: &native_records::Range| measured_range_text(text, &lines, r);
    // Build the complete owner map and prepare parent lookup keys once. SQL
    // attestation returns syntax-ID order, which need not be parent-before-child.
    let mut owners: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut prepared = Vec::with_capacity(native.declarations.len());
    for d in native.declarations {
        ensure!(d.document.path == path, "missing graph source");
        let owner_key = (
            serde_json::to_string(&d.ancestors)?,
            serde_json::to_string(&d.key)?,
        );
        ensure!(
            owners.insert(owner_key, d.syntax_id.clone()).is_none(),
            "native declaration key collision"
        );
        let parent_key = if let Some(last) = d.ancestors.last() {
            Some((
                serde_json::to_string(&d.ancestors[..d.ancestors.len() - 1])?,
                serde_json::to_string(last)?,
            ))
        } else {
            None
        };
        prepared.push((d, parent_key));
    }
    let mut nodes = Vec::with_capacity(prepared.len());
    for (d, parent_key) in prepared {
        let parent = if let Some(key) = parent_key {
            Some(
                owners
                    .get(&key)
                    .context("native declaration parent missing")?
                    .clone(),
            )
        } else {
            None
        };
        nodes.push(Symbol {
            id: d.syntax_id.clone(),
            name: d.name.clone().unwrap_or_else(|| {
                if d.kind == "module" {
                    path.to_owned()
                } else {
                    format!("<{}@{}>", d.kind, d.range.start)
                }
            }),
            display_label: None,
            kind: graph_kind(&d.kind)?,
            path: d.document.path.clone(),
            range: range(&d.range)?,
            parent,
            accessor: d.header.modifiers.iter().any(|m| m == "get" || m == "set"),
            provenance: graph_provenance(),
        });
    }
    let mut regions = Vec::with_capacity(native.control_regions.len());
    for r in native.control_regions {
        ensure!(r.document.path == path, "missing region source");
        let label = text
            .get(r.range.start..r.range.end)
            .context("invalid region text")?
            .chars()
            .take(140)
            .collect();
        regions.push(ControlRegion {
            id: r.id.clone(),
            kind: r.kind.clone(),
            label,
            parent: r.parent_id.clone(),
            owner: r.owner_syntax_id.clone(),
            path: r.document.path.clone(),
            range: range(&r.range)?,
        });
    }
    let mut calls = Vec::with_capacity(native.calls.len());
    for c in native.calls {
        ensure!(c.document.path == path, "missing call source");
        calls.push(CallSite {
            id: c.id.clone(),
            caller: c.owner_syntax_id.clone(),
            callee_text: c.spelling.clone(),
            path: c.document.path.clone(),
            range: range(&c.range)?,
            callee_range: c.callee_range.as_ref().map(range).transpose()?,
            ordinal: c.ordinal,
            regions: c.region_ids.clone(),
            provenance: graph_provenance(),
        });
    }
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    regions.sort_by(|a, b| a.id.cmp(&b.id));
    calls.sort_by(compare_writer_calls);
    Ok((nodes, calls, regions))
}

/// Cross-witness only the supplied graph rows against native facts at write
/// time; this never projects a second whole graph. Selected reads project
/// their graph from attested native rows instead (#254).
pub(crate) fn validate_native_graph_records(
    graph: &Graph,
    native: &native_records::Artifact,
    cancel: &CancelFlag,
) -> Result<()> {
    let mut files = BTreeMap::new();
    for file in &graph.files {
        files.insert(file.path.as_str(), file);
    }
    let lines = line_indexes(&files);
    let mut declaration_keys = BTreeMap::new();
    for d in &native.declarations {
        let key = (
            d.document.path.as_str(),
            serde_json::to_string(&d.ancestors)?,
            serde_json::to_string(&d.key)?,
        );
        ensure!(
            declaration_keys.insert(key, d.syntax_id.as_str()).is_none(),
            "native declaration key collision"
        );
    }
    let mut symbols = BTreeMap::new();
    for node in &graph.nodes {
        symbols.insert(node.id.as_str(), node);
    }
    ensure!(
        symbols.len() == native.declarations.len() && graph.nodes.len() == symbols.len(),
        "native_evidence_required: graph declaration cardinality"
    );
    for d in &native.declarations {
        ensure!(!cancel.load(Ordering::Relaxed), "indexing cancelled");
        let f = files
            .get(d.document.path.as_str())
            .context("native declaration source missing")?;
        let symbol = symbols
            .get(d.syntax_id.as_str())
            .context("native_evidence_required: missing measured declaration")?;
        let owner = if let Some(last) = d.ancestors.last() {
            let prefix = serde_json::to_string(&d.ancestors[..d.ancestors.len() - 1])?;
            let last = serde_json::to_string(last)?;
            Some(
                *declaration_keys
                    .get(&(d.document.path.as_str(), prefix, last))
                    .context("native declaration parent missing")?,
            )
        } else {
            None
        };
        ensure!(
            symbol.display_label.is_none(),
            "native graph has no display labels"
        );
        ensure!(
            symbol.name
                == d.name.clone().unwrap_or_else(|| if d.kind == "module" {
                    f.path.clone()
                } else {
                    format!("<{}@{}>", d.kind, d.range.start)
                })
                && symbol.kind == graph_kind(&d.kind)?
                && symbol.path == d.document.path
                && symbol.range == measured_range(f, &lines, &d.range)?
                && symbol.parent.as_deref() == owner
                && symbol.accessor == d.header.modifiers.iter().any(|m| m == "get" || m == "set")
                && symbol.provenance == graph_provenance(),
            "native_evidence_required: graph declaration differs from measured native row"
        );
    }
    let mut regions = BTreeMap::new();
    for region in &graph.regions {
        regions.insert(region.id.as_str(), region);
    }
    ensure!(
        regions.len() == native.control_regions.len() && graph.regions.len() == regions.len(),
        "native_evidence_required: graph region cardinality"
    );
    for r in &native.control_regions {
        ensure!(!cancel.load(Ordering::Relaxed), "indexing cancelled");
        let f = files
            .get(r.document.path.as_str())
            .context("native region source missing")?;
        let region = regions
            .get(r.id.as_str())
            .context("native_evidence_required: missing measured region")?;
        let text = f
            .text
            .get(r.range.start..r.range.end)
            .context("native region text missing")?;
        ensure!(
            region.kind == r.kind
                && region.label == text.chars().take(140).collect::<String>()
                && region.parent == r.parent_id
                && region.owner == r.owner_syntax_id
                && region.path == r.document.path
                && region.range == measured_range(f, &lines, &r.range)?,
            "native_evidence_required: graph region differs from measured native row"
        );
    }
    let mut calls = BTreeMap::new();
    for call in &graph.calls {
        calls.insert(call.id.as_str(), call);
    }
    ensure!(
        calls.len() == native.calls.len() && graph.calls.len() == calls.len(),
        "native_evidence_required: graph call cardinality"
    );
    for c in &native.calls {
        ensure!(!cancel.load(Ordering::Relaxed), "indexing cancelled");
        let f = files
            .get(c.document.path.as_str())
            .context("native call source missing")?;
        let call = calls
            .get(c.id.as_str())
            .context("native_evidence_required: missing measured call")?;
        ensure!(
            call.caller == c.owner_syntax_id
                && call.callee_text == c.spelling
                && call.path == c.document.path
                && call.range == measured_range(f, &lines, &c.range)?
                && call.callee_range
                    == c.callee_range
                        .as_ref()
                        .map(|range| measured_range(f, &lines, range))
                        .transpose()?
                && call.ordinal == c.ordinal
                && call.regions == c.region_ids
                && call.provenance == graph_provenance(),
            "native_evidence_required: graph call differs from measured native row"
        );
    }
    let mut stats = IndexStats {
        files: graph.files.len(),
        symbols: symbols.len(),
        calls: calls.len(),
        regions: regions.len(),
        unresolved: calls.len(),
        ..IndexStats::default()
    };
    let mut diagnostics = Vec::new();
    for c in &native.coverage {
        if c.state != "complete" {
            let recovered = c
                .diagnostic
                .as_deref()
                .is_some_and(|d| d.contains("parser recovered"));
            if recovered {
                stats.parse_error_files += 1;
            }
            diagnostics.push(Diagnostic {
                path: Some(c.document_path.clone()),
                code: if recovered {
                    "parse-error"
                } else {
                    "native-coverage-partial"
                }
                .into(),
                message: c
                    .diagnostic
                    .clone()
                    .unwrap_or_else(|| "Native extraction is incomplete".into()),
            });
        }
    }
    ensure!(
        graph.stats == stats && graph.diagnostics == diagnostics,
        "native_evidence_required: graph diagnostics or statistics differ from native coverage"
    );
    Ok(())
}

#[cfg(test)]
mod line_index_tests {
    use super::LineIndex;

    /// The previous prefix-scan implementation, kept as the equivalence oracle.
    fn prefix_scan(text: &str, byte: usize) -> anyhow::Result<(usize, usize)> {
        anyhow::ensure!(
            byte <= text.len() && text.is_char_boundary(byte),
            "invalid measured graph range"
        );
        let prefix = &text[..byte];
        Ok((
            prefix.bytes().filter(|c| *c == b'\n').count() + 1,
            byte - prefix.rfind('\n').map_or(0, |i| i + 1) + 1,
        ))
    }

    #[test]
    fn line_index_matches_prefix_scan_on_every_offset() {
        let texts = [
            "",
            "a",
            "\n",
            "\n\n",
            "one line, no trailing newline",
            "first\nsecond\nthird\n",
            "crlf\r\nline\r\n\r\nend",
            "lone\rcarriage\rreturns",
            "é\n😀x\n日本語\n\nz",
            "\u{301}combining\n\u{feff}bom",
        ];
        for text in texts {
            let lines = LineIndex::new(text);
            for byte in 0..=text.len() + 2 {
                let expected = prefix_scan(text, byte);
                let actual = lines.location(text, byte);
                match (expected, actual) {
                    (Ok(expected), Ok(actual)) => {
                        assert_eq!(actual, expected, "{text:?} at byte {byte}")
                    }
                    (Err(expected), Err(actual)) => {
                        assert_eq!(
                            actual.to_string(),
                            expected.to_string(),
                            "{text:?} at {byte}"
                        )
                    }
                    (expected, actual) => {
                        panic!("{text:?} at byte {byte}: expected {expected:?}, got {actual:?}")
                    }
                }
            }
        }
    }
}
