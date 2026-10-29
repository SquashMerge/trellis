//! Revision-bound navigation from cached, measured declarations and scoped type evidence.
//! This is line navigation, not token go-to-definition or global name resolution.
use crate::{
    model::{ClassMember, IndexPin, Symbol, SymbolKind},
    native_evidence::Range,
    store::id_codec,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Params, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeSet;

pub const MAX_RESPONSE_BYTES: usize = 512 * 1024;
// Evidence records and source parsing have separate input budgets. Source never leaves this reader.
const INPUT_BYTES: usize = 256 * 1024;
pub(crate) const RECORD_BYTES: i64 = 32 * 1024;
pub(crate) const QUERY_ROWS: usize = 128;
const TOTAL_ROWS: usize = 256;
const TARGETS: usize = 64;
const WARNINGS: usize = 32;

fn compare_enclosing(a: &Symbol, b: &Symbol) -> std::cmp::Ordering {
    (a.range.end_byte - a.range.start_byte, &a.id)
        .cmp(&(b.range.end_byte - b.range.start_byte, &b.id))
}

#[derive(Debug)]
pub struct InvalidRequest(pub &'static str);
impl std::fmt::Display for InvalidRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for InvalidRequest {}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum NavigationRequest {
    Source(SourceSelector),
    Member(MemberSelector),
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceSelector {
    pub expected_revision: IndexPin,
    pub path: String,
    pub line: usize,
    #[serde(default)]
    pub semantic_producer_id: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemberSelector {
    pub expected_revision: IndexPin,
    pub class_id: String,
    pub member_name: String,
    pub start_byte: usize,
    pub end_byte: usize,
}
impl NavigationRequest {
    pub fn expected_revision(&self) -> IndexPin {
        match self {
            Self::Source(s) => s.expected_revision,
            Self::Member(s) => s.expected_revision,
        }
    }
    pub fn validate(&self) -> Result<()> {
        let text = |s: &str, max| !s.is_empty() && s.len() <= max && !s.contains('\0');
        match self {
            Self::Source(s) => ensure!(
                text(&s.path, 8192)
                    && !s.path.contains(['\\', ':'])
                    && !s
                        .path
                        .split('/')
                        .any(|p| p.is_empty() || p == "." || p == "..")
                    && (1..=10_000_000).contains(&s.line)
                    && s.semantic_producer_id
                        .as_deref()
                        .is_none_or(crate::graph_query::valid_producer_id),
                InvalidRequest(
                    "Choose a cached workspace-relative path and a 1-based source line."
                )
            ),
            Self::Member(s) => ensure!(
                text(&s.class_id, 8192)
                    && text(&s.member_name, 2048)
                    && s.start_byte < s.end_byte
                    && s.end_byte <= i64::MAX as usize,
                InvalidRequest("Choose a recorded class member by name and exact byte range.")
            ),
        }
        Ok(())
    }
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigationTarget {
    pub symbol: Symbol,
    pub action: &'static str,
    pub reason: &'static str,
    pub match_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigationResult {
    pub revision: IndexPin,
    pub targets: Vec<NavigationTarget>,
    pub warnings: Vec<String>,
    pub truncated: bool,
    pub require_index: bool,
}
pub(crate) struct Reader<'a> {
    db: &'a Connection,
    revision_key: &'a str,
    result: NavigationResult,
    input_bytes: usize,
    rows: usize,
    output_bytes: usize,
    seen: BTreeSet<(String, &'static str, String)>,
    symbols: Option<&'a [Symbol]>,
}
impl Reader<'_> {
    fn warn(&mut self, message: &str) {
        let message: String = message.chars().take(512).collect();
        if !self.result.warnings.contains(&message) {
            if self.result.warnings.len() < WARNINGS {
                self.result.warnings.push(message);
            } else {
                self.result.truncated = true;
            }
        }
    }
    pub(crate) fn clipped(&mut self) {
        self.result.truncated = true;
        self.warn("Navigation limits reached; some cached evidence or targets were omitted.");
    }
    // Each query narrows by path/owner/id/range in SQL. Oversized records are represented
    // by NULL, never loaded/deserialized. The caller must SELECT a bounded CASE expression.
    pub(crate) fn records<T: DeserializeOwned>(
        &mut self,
        sql: &str,
        values: impl Params,
    ) -> Result<Vec<T>> {
        let mut stmt = self.db.prepare(sql)?;
        let mut rows = stmt.query(values)?;
        let mut result = vec![];
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            self.rows += 1;
            if count > QUERY_ROWS || self.rows > TOTAL_ROWS {
                self.clipped();
                break;
            }
            let payload: Option<String> = row.get(0)?;
            let Some(payload) = payload else {
                self.clipped();
                continue;
            };
            if self.input_bytes + payload.len() > INPUT_BYTES {
                self.clipped();
                break;
            }
            self.input_bytes += payload.len();
            result.push(serde_json::from_str(&payload)?);
        }
        Ok(result)
    }
    // Identical to `records` for derived symbols: one row probe at a time,
    // then per-record and aggregate byte checks in the same SQL order.
    pub(crate) fn symbol_records(
        &mut self,
        symbols: impl IntoIterator<Item = Symbol>,
    ) -> Result<Vec<Symbol>> {
        let mut result = Vec::new();
        for (index, symbol) in symbols.into_iter().enumerate() {
            self.rows += 1;
            if index >= QUERY_ROWS || self.rows > TOTAL_ROWS {
                self.clipped();
                break;
            }
            let payload_len = serde_json::to_vec(&symbol)?.len();
            if payload_len > RECORD_BYTES as usize {
                self.clipped();
                continue;
            }
            if self.input_bytes + payload_len > INPUT_BYTES {
                self.clipped();
                break;
            }
            self.input_bytes += payload_len;
            result.push(symbol);
        }
        Ok(result)
    }
    pub(crate) fn charge(&mut self, bytes: usize) -> bool {
        self.rows += 1;
        if self.rows > TOTAL_ROWS
            || bytes > RECORD_BYTES as usize
            || self.input_bytes + bytes > INPUT_BYTES
        {
            self.clipped();
            return false;
        }
        self.input_bytes += bytes;
        true
    }
    fn symbol(&mut self, id: &str) -> Result<Option<Symbol>> {
        let symbols = self.symbols.unwrap_or(&[]);
        let symbol = symbols.iter().find(|symbol| symbol.id == id).cloned();
        Ok(self.symbol_records(symbol)?.pop())
    }
    fn add(&mut self, symbol: Symbol, reason: &'static str, certainty: &str) -> Result<()> {
        self.add_target(symbol, reason, certainty, None, None)
    }
    pub(crate) fn add_call(
        &mut self,
        symbol: Symbol,
        call: &crate::native_evidence::Call,
    ) -> Result<()> {
        if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
            return Ok(());
        }
        self.add_target(
            symbol,
            "call",
            "resolved",
            Some(call.id.clone()),
            Some(call.range.clone()),
        )
    }
    fn add_target(
        &mut self,
        symbol: Symbol,
        reason: &'static str,
        certainty: &str,
        call_id: Option<String>,
        range: Option<Range>,
    ) -> Result<()> {
        let action = match symbol.kind {
            SymbolKind::Function | SymbolKind::Method => "sequence",
            SymbolKind::Class => {
                if !self.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM classes c JOIN revision_documents d ON d.revision_id=?2 AND d.cid=c.cid WHERE c.id=?1)",
                    params![id_codec::encode_sid(&symbol.id), self.revision_key],
                    |r| r.get::<_, bool>(0),
                )? {
                    return Ok(());
                }
                "class"
            }
            _ => return Ok(()),
        };
        if reason == "enclosing"
            && self
                .result
                .targets
                .iter()
                .any(|t| t.symbol.id == symbol.id && t.reason == "declaration")
        {
            return Ok(());
        }
        if !self.seen.insert((
            symbol.id.clone(),
            reason,
            call_id.clone().unwrap_or_else(|| certainty.into()),
        )) {
            return Ok(());
        }
        let target = NavigationTarget {
            symbol,
            action,
            reason,
            match_kind: certainty.into(),
            call_id,
            range,
        };
        let bytes = serde_json::to_vec(&target)?.len() + 1;
        // Reserve room for warnings and the envelope, including worst-case JSON escaping.
        if self.result.targets.len() >= TARGETS
            || self.output_bytes + bytes > MAX_RESPONSE_BYTES - 100 * 1024
        {
            self.clipped();
            return Ok(());
        }
        self.output_bytes += bytes;
        self.result.targets.push(target);
        Ok(())
    }
    fn source(&mut self, s: &SourceSelector) -> Result<()> {
        // Count lines inside SQLite: do not copy even a large cached source into Rust.
        // Byte spans always come from measured symbols, never Unicode character offsets.
        let lines: Option<i64> = self.db.query_row(
            "SELECT 1+length(v.source_bytes)-length(CAST(replace(CAST(v.source_bytes AS TEXT),char(10),'') AS BLOB)) FROM document_versions v JOIN revision_documents d ON d.revision_id=?2 AND d.vid=v.vid WHERE d.path=?1",
            params![s.path, self.revision_key], |r| r.get(0)).optional()?;
        ensure!(
            lines.is_some_and(|lines| s.line as i64 <= lines),
            InvalidRequest("The path or line is not present in the cached revision.")
        );
        let mut candidates: Vec<Symbol> = self
            .symbols
            .unwrap_or(&[])
            .iter()
            .filter(|node| {
                node.path == s.path
                    && node.range.start_line == s.line
                    && matches!(
                        node.kind,
                        SymbolKind::Class | SymbolKind::Method | SymbolKind::Function
                    )
            })
            .cloned()
            .collect();
        candidates.sort_by(|a, b| a.id.cmp(&b.id));
        let declarations = self.symbol_records(candidates.into_iter().take(129))?;
        for symbol in declarations {
            self.add(symbol, "declaration", "measured")?;
        }
        for class in [true, false] {
            let mut enclosing: Vec<Symbol> = self
                .symbols
                .unwrap_or(&[])
                .iter()
                .filter(|node| {
                    node.path == s.path
                        && (if class {
                            node.kind == SymbolKind::Class
                        } else {
                            matches!(node.kind, SymbolKind::Method | SymbolKind::Function)
                        })
                        && node.range.start_line <= s.line
                        && (node.range.end_line > s.line
                            || (node.range.end_line == s.line && node.range.end_column > 1))
                })
                .cloned()
                .collect();
            enclosing.sort_by(compare_enclosing);
            for symbol in self.symbol_records(enclosing.into_iter().take(1))? {
                self.add(symbol, "enclosing", "measured")?;
            }
        }
        // Calls and declared type names do not prove a navigable declaration.
        Ok(())
    }
    fn member(&mut self, s: &MemberSelector) -> Result<()> {
        // JSON arrays stay in SQLite. Validate exact recorded name + range before returning
        // even one member, including overloads and shared multi-field declaration ranges.
        if self.result.require_index {
            return Ok(());
        }
        let matches: i64 = self.db.query_row(
            "SELECT count(*) FROM (SELECT 1 FROM classes c JOIN revision_documents d ON d.revision_id=?5 AND d.cid=c.cid, json_each(c.payload) a, json_each(a.value) m
             WHERE c.id=?1 AND a.key IN ('fields','methods')
             AND json_extract(m.value,'$.name')=?2
             AND json_extract(m.value,'$.range.startByte')=?3 AND json_extract(m.value,'$.range.endByte')=?4 LIMIT 2)",
            params![id_codec::encode_sid(&s.class_id), s.member_name, s.start_byte as i64, s.end_byte as i64, self.revision_key], |r| r.get(0))?;
        ensure!(
            matches == 1,
            InvalidRequest("The selector does not match one recorded class member.")
        );
        let revision_key = self.revision_key;
        let members: Vec<ClassMember> = self.records(
            "SELECT CASE WHEN length(CAST(m.value AS BLOB))<=?5 THEN m.value END
             FROM classes c JOIN revision_documents d ON d.revision_id=?6 AND d.cid=c.cid, json_each(c.payload) a, json_each(a.value) m
             WHERE c.id=?1 AND a.key IN ('fields','methods')
             AND json_extract(m.value,'$.name')=?2
             AND json_extract(m.value,'$.range.startByte')=?3 AND json_extract(m.value,'$.range.endByte')=?4 LIMIT 2",
            params![id_codec::encode_sid(&s.class_id), s.member_name, s.start_byte as i64, s.end_byte as i64, RECORD_BYTES, revision_key])?;
        let Some(member) = members.first() else {
            self.warn("The recorded member exceeds the navigation evidence budget.");
            return Ok(());
        };
        if let Some(id) = &member.symbol_id
            && let Some(symbol) = self.symbol(id)?
            && matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function)
            && symbol.name == member.name
            && symbol.path == member.path
            && symbol.range == member.range
            && symbol.parent.as_deref() == Some(&s.class_id)
        {
            self.add(symbol, "declaration", "measured")?;
        }
        // The recorded member is a terminal declaration. Its type hint is not
        // a link to another class or method.
        Ok(())
    }
}
pub(crate) fn navigate(
    db: &Connection,
    request: &NavigationRequest,
    revision: IndexPin,
    revision_key: &str,
    symbols: Option<&[Symbol]>,
    calls: &mut dyn FnMut(&mut Reader<'_>, &SourceSelector) -> Result<()>,
) -> Result<NavigationResult> {
    let metadata: Option<bool> = db
        .query_row(
            "SELECT class_truncated FROM native_revisions WHERE id=?1",
            [revision_key],
            |r| r.get(0),
        )
        .optional()?;
    let mut reader = Reader {
        db,
        revision_key,
        result: NavigationResult {
            revision,
            targets: vec![],
            warnings: vec![],
            truncated: metadata.unwrap_or(false),
            require_index: metadata.is_none(),
        },
        input_bytes: 0,
        rows: 0,
        output_bytes: 0,
        seen: BTreeSet::new(),
        symbols,
    };
    if metadata.is_none() {
        reader.warn(crate::class_diagram::INDEX_NOTICE);
    }
    if metadata == Some(true) {
        reader.warn("The cached class projection is incomplete; some declared types or members may be absent.");
    }
    match request {
        NavigationRequest::Source(s) => {
            reader.source(s)?;
            if s.semantic_producer_id.is_some() {
                calls(&mut reader, s)?;
            }
        }
        NavigationRequest::Member(s) => reader.member(s)?,
    }
    if reader.result.targets.is_empty() && reader.result.warnings.is_empty() {
        reader.warn("No indexed navigation target on this source line.");
    }
    Ok(reader.result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Provenance, SemanticState, SourceRange};

    fn symbol(id: usize) -> Symbol {
        Symbol {
            id: format!("sid:v1:{id:032x}"),
            name: "f".into(),
            display_label: None,
            kind: SymbolKind::Function,
            path: "a.js".into(),
            range: SourceRange {
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                end_line: 1,
                start_column: 1,
                end_column: 2,
            },
            parent: None,
            accessor: false,
            provenance: Provenance {
                source: "tree-sitter".into(),
                semantic: SemanticState::Unavailable,
            },
        }
    }

    fn reader<'a>(db: &'a Connection) -> Reader<'a> {
        Reader {
            db,
            revision_key: "pin",
            symbols: None,
            result: NavigationResult {
                revision: IndexPin {
                    index_generation: uuid::Uuid::parse_str("11000000-0000-4000-8000-000000000000")
                        .unwrap(),
                    index_revision: 1,
                },
                targets: vec![],
                warnings: vec![],
                truncated: false,
                require_index: false,
            },
            input_bytes: 0,
            rows: 0,
            output_bytes: 0,
            seen: BTreeSet::new(),
        }
    }

    #[test]
    fn enclosing_equal_span_id_tie_break_is_bytewise_in_both_input_orders() {
        // Synthetic comparator tie-break test, not a main parity fixture.
        let mut smaller = symbol(1);
        smaller.range.start_byte = 10;
        smaller.range.end_byte = 20;
        let mut larger = symbol(2);
        larger.range.start_byte = 30;
        larger.range.end_byte = 40;
        assert_eq!(smaller.range.end_byte - smaller.range.start_byte, 10);
        assert_eq!(larger.range.end_byte - larger.range.start_byte, 10);
        for mut candidates in [
            vec![smaller.clone(), larger.clone()],
            vec![larger.clone(), smaller.clone()],
        ] {
            candidates.sort_by(compare_enclosing);
            assert_eq!(candidates[0].id, smaller.id);
            assert_eq!(candidates[1].id, larger.id);
        }
    }

    #[test]
    fn derived_symbol_record_and_aggregate_byte_boundaries() {
        let db = Connection::open_in_memory().unwrap();
        let mut one = symbol(1);
        let base = serde_json::to_vec(&one).unwrap().len();
        one.name = "x".repeat(RECORD_BYTES as usize - base + one.name.len());
        assert_eq!(
            serde_json::to_vec(&one).unwrap().len(),
            RECORD_BYTES as usize
        );
        let mut read = reader(&db);
        read.input_bytes = INPUT_BYTES - RECORD_BYTES as usize;
        assert_eq!(read.symbol_records([one.clone()]).unwrap().len(), 1);
        assert_eq!(read.input_bytes, INPUT_BYTES);
        one.name.push('x');
        assert!(read.symbol_records([one]).unwrap().is_empty());
        assert!(read.result.truncated);
        let one = symbol(2);
        let size = serde_json::to_vec(&one).unwrap().len();
        let mut read = reader(&db);
        read.input_bytes = INPUT_BYTES - size + 1;
        assert!(read.symbol_records([one]).unwrap().is_empty());
        assert!(read.result.truncated);
    }

    #[test]
    fn derived_symbol_query_and_total_row_boundaries() {
        let db = Connection::open_in_memory().unwrap();
        let mut read = reader(&db);
        assert_eq!(
            read.symbol_records((0..QUERY_ROWS).map(symbol))
                .unwrap()
                .len(),
            QUERY_ROWS
        );
        assert!(!read.result.truncated);
        assert_eq!(
            read.symbol_records((QUERY_ROWS..QUERY_ROWS + 1).map(symbol))
                .unwrap()
                .len(),
            1
        );
        assert!(!read.result.truncated);
        let mut read = reader(&db);
        read.rows = TOTAL_ROWS - 1;
        assert_eq!(read.symbol_records([symbol(1)]).unwrap().len(), 1);
        assert!(!read.result.truncated);
        assert!(read.symbol_records([symbol(2)]).unwrap().is_empty());
        assert!(read.result.truncated);
        let mut read = reader(&db);
        assert_eq!(
            read.symbol_records((0..QUERY_ROWS + 1).map(symbol))
                .unwrap()
                .len(),
            QUERY_ROWS
        );
        assert!(read.result.truncated);
    }
}
