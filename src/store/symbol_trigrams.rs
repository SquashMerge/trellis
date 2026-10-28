//! The trigram candidate prefilter for symbol search (Decision 0013).
//!
//! Symbol search keeps its literal rule: SQLite's ASCII-only `lower`, then
//! `instr` on the name or ID. The FTS5 tables `native_declaration_trigrams`
//! (W2) and `graph_node_trigrams` (the browser and CLI search) hold a
//! `lower`ed copy of the **name** only, written by triggers on the source
//! rows, under `tokenize='trigram case_sensitive 1'`. IDs are not indexed: a
//! query that could occur in an ID adds the rows a covering scan of the ID
//! finds. The tables never score or order, and every candidate is still
//! checked with the literal rule, so the answer is the literal scan's answer.
//!
//! Every literal match is a candidate: if `lower(q)` occurs in `lower(name)`,
//! each of its character trigrams occurs there consecutively, and the copy
//! is indexed under the same `lower` with no further folding. If `lower(q)`
//! occurs only in the ID, the query has the shape [`can_match_an_id`]
//! accepts, and the ID scan finds the row.
use super::storage_result;
use anyhow::Result;
use rusqlite::Connection;

/// The MATCH expression for `query`, or `None` when the query takes the
/// literal scan. The choice depends on the query alone and is made before
/// FTS5 runs. A trigram table can't represent a query of fewer than three
/// Unicode characters (it forms no trigram), or one containing U+0000 (an
/// FTS5 string literal ends at the NUL). Every other query becomes one FTS5
/// string literal of its ASCII-lowered text, with `"` doubled, so FTS5
/// operators and column filters in the query are plain text.
pub(super) fn match_expression(query: &str) -> Option<String> {
    #[cfg(test)]
    if FORCE_LITERAL.get() {
        return None;
    }
    if query.chars().count() < 3 || query.contains('\0') {
        return None;
    }
    Some(format!(
        "\"{}\"",
        query.to_ascii_lowercase().replace('"', "\"\"")
    ))
}

/// Whether `lower(query)` can occur in some ID, decided from the query alone.
/// Every stored ID, native and graph, is a minted SyntaxId: `sid:v1:`
/// followed by 32 lowercase hex digits. A string occurs in one exactly when
/// it is a substring of `sid:v1:`, or a suffix of `sid:v1:` (possibly
/// empty) followed by 1–32 hex digits.
pub(super) fn can_match_an_id(query: &str) -> bool {
    const PREFIX: &str = "sid:v1:";
    let lowered = query.to_ascii_lowercase();
    PREFIX.contains(lowered.as_str())
        || (0..=PREFIX.len()).any(|start| {
            lowered.strip_prefix(&PREFIX[start..]).is_some_and(|hex| {
                (1..=32).contains(&hex.len())
                    && hex
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        })
}

/// One search's candidate rows and how to find them.
struct Search<'a> {
    columns: &'a str,
    source: &'a str,
    names: &'a str,
    ids: &'a str,
    joins: &'a [&'a str],
    name: &'a str,
    id: &'a str,
    query_parameter: usize,
    limit_parameter: usize,
    match_parameter: usize,
}

impl Search<'_> {
    fn hits(
        &self,
        db: &Connection,
        query: &str,
        limit: i64,
        values: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<(String, String)>> {
        let Some(expression) = match_expression(query) else {
            return ranked(db, &self.sql(self.source), values).map(unranked);
        };
        let id_shaped = can_match_an_id(query);
        let mut values = values.to_vec();
        values.push(&expression);
        // Open FTS even on misses, so corruption never becomes an empty answer.
        // Keep small candidate sets on the FTS-driven join-and-sort path.
        // This is a global (retained-history) count, not revision density.
        const ORDERED_THRESHOLD: i64 = 4096;
        let threshold = ORDERED_THRESHOLD.max(limit);
        let probe_limit = threshold.saturating_add(1);
        let count: i64 = storage_result(db.query_row(
            &format!(
                "SELECT count(*) FROM (SELECT rowid FROM {names} WHERE {names} MATCH ?1 LIMIT ?2)",
                names = self.names,
            ),
            rusqlite::params![expression, probe_limit],
            |row| row.get(0),
        ))?;
        if limit > 0 && count > threshold {
            #[cfg(test)]
            ORDERED_SEARCHES.set(ORDERED_SEARCHES.get() + 1);
            return self
                .ordered_hits(db, query, limit, id_shaped, &values)
                .map(unranked);
        }
        let alias = self.source.rsplit(' ').next().unwrap_or_default();
        let table = self.source.split(' ').next().unwrap_or_default();
        let names = format!(
            "(SELECT rowid AS r FROM {names} WHERE {names} MATCH ?{p}) c
                CROSS JOIN {table} {alias} ON {alias}.rowid=c.r",
            names = self.names,
            p = self.match_parameter,
        );
        let hits = ranked(db, &self.sql(&names), &values)?;
        if !id_shaped || names_decide(&hits, limit) {
            return Ok(unranked(hits));
        }
        #[cfg(test)]
        ID_SCANS.set(ID_SCANS.get() + 1);
        let both = format!(
            "(SELECT rowid AS r FROM {names} WHERE {names} MATCH ?{p} UNION {ids}) c
                CROSS JOIN {table} {alias} ON {alias}.rowid=c.r",
            names = self.names,
            ids = self.ids,
            p = self.match_parameter,
        );
        ranked(db, &self.sql(&both), &values).map(unranked)
    }

    fn literal(&self) -> String {
        format!(
            "instr(lower({name}),lower(?{q}))>0 OR {id}",
            name = self.name,
            q = self.query_parameter,
            id = self.id_match(),
        )
    }

    fn id_match(&self) -> String {
        format!(
            "instr('sid:v1:'||lower(hex({id})),lower(?{q}))>0",
            id = self.id,
            q = self.query_parameter,
        )
    }

    fn joins(&self, ordered: bool) -> String {
        let keyword = if ordered { "CROSS JOIN" } else { "JOIN" };
        self.joins
            .iter()
            .map(|join| format!("{keyword} {join}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn sql(&self, from: &str) -> String {
        format!(
            "SELECT {columns} FROM {from} {joins} WHERE {literal}
                ORDER BY 3,{name} COLLATE BINARY,{id} LIMIT ?{limit}",
            columns = self.columns,
            joins = self.joins(false),
            literal = self.literal(),
            name = self.name,
            id = self.id,
            limit = self.limit_parameter,
        )
    }

    fn ordered_sql(&self, tier: i64, upper: bool, id_shaped: bool) -> String {
        let table = self.source.split(' ').next().unwrap_or_default();
        let alias = self.source.rsplit(' ').next().unwrap_or_default();
        let name = self.name;
        let q = self.query_parameter;
        let membership = format!(
            "EXISTS(SELECT 1 FROM {names} WHERE rowid={alias}.rowid AND {names} MATCH ?{p})",
            names = self.names,
            p = self.match_parameter,
        );
        let (index, predicate) = match tier {
            0 => (
                "folded",
                format!("lower({name})=lower(?{q}) AND {membership}"),
            ),
            1 => {
                // The folded range narrows the input, but lower(name) order
                // is not binary name order. SQLite sorts these prefix hits.
                let end = if upper {
                    format!("AND lower({name}) < ?{}", self.match_parameter + 1)
                } else {
                    String::new()
                };
                (
                    "folded",
                    format!(
                        "lower({name})>=lower(?{q}) {end}
                     AND lower({name})<>lower(?{q}) AND instr(lower({name}),lower(?{q}))=1
                     AND {membership}"
                    ),
                )
            }
            _ => {
                // CASE, unlike NOT(prefix), also keeps unnamed ID-only hits.
                let candidate = if id_shaped {
                    format!(
                        "((instr(lower({name}),lower(?{q}))>0 AND {membership}) OR {})",
                        self.id_match()
                    )
                } else {
                    format!("instr(lower({name}),lower(?{q}))>0 AND {membership}")
                };
                (
                    "name",
                    format!(
                        "CASE WHEN lower({name})=lower(?{q}) THEN 0
                     WHEN instr(lower({name}),lower(?{q}))=1 THEN 1 ELSE 2 END=2 AND {candidate}"
                    ),
                )
            }
        };
        format!(
            "SELECT {columns} FROM {source} INDEXED BY {table}_search_{index} {joins}
             WHERE {predicate} ORDER BY {name} COLLATE BINARY,{id} LIMIT ?{limit}",
            columns = self.columns,
            source = self.source,
            // Keep the ordered source outermost; sparse/literal queries retain
            // their ordinary joins and SQLite's original planning freedom.
            joins = self.joins(true),
            id = self.id,
            limit = self.limit_parameter,
        )
    }

    fn ordered_hits(
        &self,
        db: &Connection,
        query: &str,
        limit: i64,
        id_shaped: bool,
        values: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<(String, String, i64)>> {
        let upper = prefix_end(&query.to_ascii_lowercase());
        let mut hits = Vec::new();
        for tier in 0..=2 {
            let remaining = limit - hits.len() as i64;
            if remaining <= 0 {
                break;
            }
            #[cfg(test)]
            if tier == 2 && id_shaped {
                ID_SCANS.set(ID_SCANS.get() + 1);
            }
            let mut values = values.to_vec();
            values[self.limit_parameter - 1] = &remaining;
            if tier == 1 && upper.is_some() {
                values.push(&upper);
            }
            hits.extend(ranked(
                db,
                &self.ordered_sql(tier, upper.is_some(), id_shaped),
                &values,
            )?);
        }
        Ok(hits)
    }
}

/// Exclusive BINARY bound for valid UTF-8 strings starting with `prefix`.
fn prefix_end(prefix: &str) -> Option<String> {
    for (at, ch) in prefix.char_indices().rev() {
        let mut next = u32::from(ch) + 1;
        if next == 0xd800 {
            next = 0xe000;
        }
        if let Some(ch) = char::from_u32(next) {
            let mut end = prefix[..at].to_owned();
            end.push(ch);
            return Some(end);
        }
    }
    None
}

/// The ID-scan shortcut. Not a fallback: it runs after the names query has
/// succeeded and only decides whether a second, larger query could change
/// the answer. A row the ID scan adds matches through its ID only, so its
/// name neither equals nor starts with the query: it ranks 2. Hits come
/// ordered by rank. If the names query already filled all `limit` rows with
/// rank-0 or rank-1 hits, every added row would sort after them and fall
/// beyond the LIMIT, so the answer is the same without the scan.
fn names_decide(hits: &[(String, String, i64)], limit: i64) -> bool {
    i64::try_from(hits.len()).is_ok_and(|n| n >= limit) && hits.last().is_some_and(|h| h.2 <= 1)
}

fn unranked(hits: Vec<(String, String, i64)>) -> Vec<(String, String)> {
    hits.into_iter().map(|(a, b, _)| (a, b)).collect()
}

/// W2's ranked `(version_id, syntax_id)` hits in `revision`, at most
/// `lookahead` of them. SQLite `lower` folds ASCII only, and `instr` treats
/// the query as literal bytes, not a pattern.
pub(super) fn w2_hits(
    db: &Connection,
    revision: &str,
    query: &str,
    lookahead: i64,
) -> Result<Vec<(String, String)>> {
    w2_search().hits(db, query, lookahead, &[&revision, &query, &lookahead])
}

fn w2_search() -> Search<'static> {
    Search {
        columns: "v.id,d.syntax_id,CASE WHEN lower(d.name)=lower(?2) THEN 0
            WHEN instr(lower(d.name),lower(?2))=1 THEN 1 ELSE 2 END",
        source: "native_version_declarations d",
        names: "native_declaration_trigrams",
        ids: "SELECT d.rowid FROM revision_documents m
            JOIN native_version_declarations d ON d.vid=m.vid
            WHERE m.revision_id=?1 AND instr('sid:v1:'||lower(hex(d.syntax_id)),lower(?2))>0",
        joins: &[
            "revision_documents m ON m.vid=d.vid AND m.revision_id=?1",
            "document_versions v ON v.vid=d.vid",
        ],
        name: "d.name",
        id: "d.syntax_id",
        query_parameter: 2,
        limit_parameter: 3,
        match_parameter: 4,
    }
}

/// The browser and CLI search's ranked `(path, id)` graph-node hits in
/// the admitted head, at most `limit` of them, under the same literal rule.
pub(super) fn graph_hits(
    db: &Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<(String, String)>> {
    graph_search().hits(db, query, limit, &[&query, &limit])
}

fn graph_search() -> Search<'static> {
    Search {
        columns: "m.path,n.id,CASE WHEN lower(n.name)=lower(?1) THEN 0 WHEN instr(lower(n.name),lower(?1))=1 THEN 1 ELSE 2 END",
        source: "graph_nodes n",
        names: "graph_node_trigrams",
        ids: "SELECT n.rowid FROM revision_documents m
            JOIN graph_nodes n ON n.gid=m.gid
            WHERE m.revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1)
            AND instr('sid:v1:'||lower(hex(n.id)),lower(?1))>0",
        joins: &["revision_documents m ON m.revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) AND m.gid=n.gid"],
        name: "n.name",
        id: "n.id",
        query_parameter: 1,
        limit_parameter: 2,
        match_parameter: 3,
    }
}

/// Reconfirm FTS corruption only during exceptional recovery under exclusive
/// index use. The default check reads FTS internals, not captured source content.
pub(super) fn verify_integrity(db: &Connection) -> Result<()> {
    let transaction = storage_result(db.unchecked_transaction())?;
    for table in ["native_declaration_trigrams", "graph_node_trigrams"] {
        storage_result(transaction.execute(
            &format!("INSERT INTO {table}({table}) VALUES('integrity-check')"),
            [],
        ))?;
    }
    storage_result(transaction.rollback())?;
    Ok(())
}

/// Run one search, selecting `(a, b, rank)`. SQLite contention is the typed,
/// retried `storage_busy`; every other error, from FTS5 or not, fails the
/// read. Nothing here retries the literal scan after FTS5 has run.
fn ranked(
    db: &Connection,
    sql: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<Vec<(String, String, i64)>> {
    let raw: Vec<(String, Vec<u8>, i64)> = storage_result((|| {
        db.prepare(sql)?
            .query_map(values, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    })())?;
    raw.into_iter()
        .map(|(first, id, rank)| Ok((first, super::id_codec::decode_sid(&id)?, rank)))
        .collect()
}

#[cfg(test)]
thread_local! {
    static FORCE_LITERAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ID_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ORDERED_SEARCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many ID scans this thread has run, so a test can see the shortcut.
#[cfg(test)]
pub(crate) fn id_scans() -> usize {
    ID_SCANS.get()
}

/// Run `read` with the prefilter off on this thread, so a test can compare
/// the trigram path with the literal scan for the same query.
#[cfg(test)]
pub(crate) fn with_literal_scan<T>(read: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FORCE_LITERAL.set(false);
        }
    }
    FORCE_LITERAL.set(true);
    let _reset = Reset;
    read()
}

#[cfg(test)]
impl super::Store {
    /// Overwrite both trigram tables' FTS5 structure records with garbage.
    pub(crate) fn corrupt_symbol_trigrams_for_tests(&self) -> Result<()> {
        let db = self.cache_write()?;
        for table in ["native_declaration_trigrams", "graph_node_trigrams"] {
            anyhow::ensure!(
                db.execute(
                    &format!("UPDATE {table}_data SET block=x'ffffffffffffffff' WHERE id=10"),
                    [],
                )? == 1,
                "trigram structure record absent"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranking_fixture() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE native_version_declarations(vid INTEGER,syntax_id BLOB,name TEXT);
             CREATE INDEX native_version_declarations_search_name ON native_version_declarations(name COLLATE BINARY,syntax_id,vid);
             CREATE INDEX native_version_declarations_search_folded ON native_version_declarations(lower(name),name COLLATE BINARY,syntax_id,vid);
             CREATE TABLE document_versions(vid INTEGER PRIMARY KEY,id TEXT);
             CREATE TABLE revision_documents(revision_id TEXT,vid INTEGER,gid INTEGER,path TEXT);
             CREATE INDEX revision_documents_version ON revision_documents(vid,revision_id);
             CREATE INDEX revision_documents_graph ON revision_documents(gid,revision_id);
             CREATE TABLE index_metadata(singleton INTEGER,index_generation TEXT,index_revision INTEGER);
             INSERT INTO index_metadata VALUES(1,'test',1);
             INSERT INTO document_versions VALUES(1,'selected'),(2,'retained');
             INSERT INTO revision_documents VALUES('pin:v1:test:1',1,1,'a.js');
             CREATE TABLE graph_nodes(gid INTEGER,id BLOB,name TEXT);
             CREATE INDEX graph_nodes_search_name ON graph_nodes(name COLLATE BINARY,id,gid);
             CREATE INDEX graph_nodes_search_folded ON graph_nodes(lower(name),name COLLATE BINARY,id,gid);
             CREATE VIRTUAL TABLE native_declaration_trigrams USING fts5(name,content='',columnsize=0,tokenize='trigram case_sensitive 1');
             CREATE VIRTUAL TABLE graph_node_trigrams USING fts5(name,content='',columnsize=0,tokenize='trigram case_sensitive 1');"
        ).unwrap();
        db
    }

    fn insert_ranked(db: &Connection, rowid: i64, owner: i64, name: Option<&str>) {
        let id = super::super::id_codec::encode_sid(&format!("sid:v1:{rowid:032x}")).unwrap();
        db.execute(
            "INSERT INTO native_version_declarations(rowid,vid,syntax_id,name) VALUES(?1,?2,?3,?4)",
            rusqlite::params![rowid, owner, id, name],
        )
        .unwrap();
        db.execute(
            "INSERT INTO native_declaration_trigrams(rowid,name) VALUES(?1,lower(?2))",
            rusqlite::params![rowid, name],
        )
        .unwrap();
        if let Some(name) = name {
            db.execute(
                "INSERT INTO graph_nodes(rowid,gid,id,name) VALUES(?1,?2,?3,?4)",
                rusqlite::params![rowid, owner, id, name],
            )
            .unwrap();
            db.execute(
                "INSERT INTO graph_node_trigrams(rowid,name) VALUES(?1,lower(?2))",
                rusqlite::params![rowid, name],
            )
            .unwrap();
        }
    }

    #[test]
    fn ordered_tiers_equal_the_literal_rule_including_retained_and_unnamed_id_hits() {
        let db = ranking_fixture();
        for (at, name) in [
            Some("OCA"),
            Some("oca"),
            Some("OCAz"),
            Some("ocaa"),
            Some("a_oca"),
            Some("a_oca"),
            Some("abc"),
            Some("abcTail"),
            Some("a_abc"),
            None,
        ]
        .into_iter()
        .enumerate()
        {
            insert_ranked(&db, at as i64 + 1, 1, name);
        }
        for at in 20..40 {
            insert_ranked(&db, at, 2, Some("000_oca_abc"));
        }
        for query in [
            "oca", "OCA", "abc", "sid:v1:", "000", "absent", "éab", "a\0b",
        ] {
            for limit in [0, 1, 2, 3, 5, 20] {
                let actual = w2_hits(&db, "pin:v1:test:1", query, limit).unwrap();
                let literal =
                    with_literal_scan(|| w2_hits(&db, "pin:v1:test:1", query, limit)).unwrap();
                assert_eq!(actual, literal, "W2 {query:?} {limit}");
                if let Some(expression) = match_expression(query) {
                    let values: Vec<&dyn rusqlite::ToSql> =
                        vec![&"pin:v1:test:1", &query, &limit, &expression];
                    let ordered = w2_search()
                        .ordered_hits(&db, query, limit, can_match_an_id(query), &values)
                        .unwrap();
                    assert_eq!(unranked(ordered), literal, "ordered W2 {query:?} {limit}");
                    let old_names = "(SELECT rowid AS r FROM native_declaration_trigrams
                        WHERE native_declaration_trigrams MATCH ?4 UNION SELECT d.rowid
                        FROM native_version_declarations d JOIN revision_documents m ON m.vid=d.vid
                        WHERE m.revision_id=?1 AND instr('sid:v1:'||lower(hex(d.syntax_id)),lower(?2))>0) c
                        CROSS JOIN native_version_declarations d ON d.rowid=c.r";
                    let previous = ranked(&db, &w2_search().sql(old_names), &values).unwrap();
                    assert_eq!(unranked(previous), literal, "old FTS W2 {query:?} {limit}");
                }
                let actual = graph_hits(&db, query, limit).unwrap();
                let literal = with_literal_scan(|| graph_hits(&db, query, limit)).unwrap();
                assert_eq!(actual, literal, "graph {query:?} {limit}");
            }
        }
        for prefix in ["abc", "éab", "a\u{d7ff}", "a\u{10ffff}", "\u{10ffff}"] {
            if let Some(end) = prefix_end(prefix) {
                assert!(end.as_str() > prefix);
                assert!(format!("{prefix}tail") < end);
            } else {
                assert_eq!(prefix, "\u{10ffff}");
            }
        }
    }

    #[test]
    fn sparse_scattered_matches_keep_the_fts_driven_query() {
        for count in [22, 100, 200] {
            let db = ranking_fixture();
            let query = format!("sparse{count}q");
            for at in 1..=3200 {
                // Spread hits across the entire binary-name range, including
                // retained rows that cannot answer the selected revision.
                let spacing = 3200 / count;
                let hit = at % spacing == 0 && at / spacing <= count;
                let name = format!("{at:04}_{}", if hit { &query } else { "other" });
                let owner = if hit && at / spacing % 3 == 0 { 2 } else { 1 };
                insert_ranked(&db, at, owner, Some(&name));
            }
            for search in [w2_search(), graph_search()] {
                let before = ORDERED_SEARCHES.get();
                let limit = 21;
                let revision = "pin:v1:test:1";
                let mut values: Vec<&dyn rusqlite::ToSql> = vec![&query, &limit];
                if search.query_parameter == 2 {
                    values.insert(0, &revision);
                }
                let hits = search.hits(&db, &query, limit, &values).unwrap();
                let literal =
                    with_literal_scan(|| search.hits(&db, &query, limit, &values)).unwrap();
                assert_eq!(hits, literal, "{} {count}", search.source);
                assert_eq!(
                    ORDERED_SEARCHES.get(),
                    before,
                    "{}: {count} scattered FTS matches must not select an ordered walk",
                    search.source,
                );
            }
        }
    }

    #[test]
    fn ordered_dispatch_requires_more_than_the_global_threshold_and_limit() {
        let db = ranking_fixture();
        for rowid in 1..=4096 {
            let owner = if rowid % 2 == 0 { 2 } else { 1 };
            insert_ranked(&db, rowid, owner, Some("a_thresholdq"));
        }
        for (count, limit, ordered_expected) in
            [(4096, 21, false), (4097, 21, true), (4097, 4097, false)]
        {
            if count == 4097 && limit == 21 {
                insert_ranked(&db, 4097, 1, Some("a_thresholdq"));
            }
            for search in [w2_search(), graph_search()] {
                let before = ORDERED_SEARCHES.get();
                let query = "thresholdq";
                let revision = "pin:v1:test:1";
                let mut values: Vec<&dyn rusqlite::ToSql> = vec![&query, &limit];
                if search.query_parameter == 2 {
                    values.insert(0, &revision);
                }
                let hits = search.hits(&db, query, limit, &values).unwrap();
                let literal =
                    with_literal_scan(|| search.hits(&db, query, limit, &values)).unwrap();
                assert_eq!(hits, literal);
                assert_eq!(
                    ORDERED_SEARCHES.get() - before,
                    usize::from(ordered_expected),
                    "{} count={count} limit={limit}",
                    search.source
                );
            }
        }
    }

    #[test]
    fn prefix_tier_uses_a_folded_range_and_sorts_binary_names() {
        use rusqlite::StatementStatus;
        let db = ranking_fixture();
        // Folded order is ocaa, ocaz, but binary order is OCAz, ocaa.
        for (at, name) in ["ocaa", "OCAz", "oca", "a_oca", "other"].iter().enumerate() {
            insert_ranked(&db, at as i64 + 1, 1, Some(name));
        }
        for search in [w2_search(), graph_search()] {
            let query = "oca";
            let expression = match_expression(query).unwrap();
            let upper = prefix_end(query).unwrap();
            let limit = 21;
            let revision = "pin:v1:test:1";
            let mut values: Vec<&dyn rusqlite::ToSql> = vec![&query, &limit, &expression, &upper];
            if search.query_parameter == 2 {
                values.insert(0, &revision);
            }
            let sql = search.ordered_sql(1, true, false);
            let plan: Vec<String> = db
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(values.as_slice(), |r| r.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter().any(|p| p.contains("USING COVERING INDEX")
                    && p.contains("_search_folded")
                    && p.contains("<expr>>?")
                    && p.contains("<expr><?")),
                "{plan:?}"
            );
            assert!(
                plan.iter().any(|p| p.contains("TEMP B-TREE FOR ORDER BY")),
                "{plan:?}"
            );
            let mut statement = db.prepare(&sql).unwrap();
            let ids: Vec<Vec<u8>> = statement
                .query_map(values.as_slice(), |r| r.get(1))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(
                ids,
                [2, 1].map(
                    |id| super::super::id_codec::encode_sid(&format!("sid:v1:{id:032x}")).unwrap()
                )
            );
            assert_eq!(statement.get_status(StatementStatus::Sort), 1);
        }
    }

    #[test]
    fn seeded_differential_search_matches_literal_for_native_and_graph() {
        use rand::{RngExt, SeedableRng};
        const SEED: u64 = 0x0038_2384_0013;
        let mut rng = rand::rngs::StdRng::seed_from_u64(SEED);
        let db = ranking_fixture();
        let fragments = [
            "Needle",
            "nEEdLe",
            "abc",
            "aBc",
            "Éab",
            "éab",
            "東京名",
            "😀ab",
            "\"hi\"",
            "a\0b",
            "short",
            "aßZ",
            "\u{d7ff}ab",
            "\u{10ffff}ab",
        ];
        let mut queries: Vec<String> = [
            "needle",
            "NEEDLE",
            "abc",
            "AbC",
            "Éab",
            "éab",
            "東京名",
            "😀ab",
            "\"hi\"",
            "a\0b",
            "a",
            "ab",
            "sid:v1:",
            "SID",
            "v1:",
            "absent",
            "\u{d7ff}ab",
            "\u{10ffff}ab",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for rowid in 1..=768 {
            let first = fragments[rng.random_range(0..fragments.len())];
            let second = fragments[rng.random_range(0..fragments.len())];
            // Duplicates, exact names, prefixes and interior hits; rowids do
            // not correlate with binary-name order or revision membership.
            let name = match rng.random_range(0..4) {
                0 => first.to_owned(),
                1 => format!("{first}_{second}"),
                _ => format!("{:03}_{first}_{second}", rng.random_range(0..128)),
            };
            let owner = if rng.random_ratio(1, 3) { 2 } else { 1 };
            let named = !rng.random_ratio(1, 16);
            insert_ranked(&db, rowid, owner, named.then_some(name.as_str()));
            let id = format!("sid:v1:{:032x}", rng.random::<u128>());
            let encoded = super::super::id_codec::encode_sid(&id).unwrap();
            db.execute(
                "UPDATE native_version_declarations SET syntax_id=?1 WHERE rowid=?2",
                rusqlite::params![encoded, rowid],
            )
            .unwrap();
            db.execute(
                "UPDATE graph_nodes SET id=?1 WHERE rowid=?2",
                rusqlite::params![encoded, rowid],
            )
            .unwrap();
            if rowid % 64 == 0 {
                queries.push(id[28..].to_ascii_uppercase());
                let start = rng.random_range(0..name.chars().count() - 2);
                queries.push(name.chars().skip(start).take(3).collect());
            }
        }
        for query in &queries {
            for limit in [0, 1, 7, 21, 1000] {
                for search in [w2_search(), graph_search()] {
                    let revision = "pin:v1:test:1";
                    let mut values: Vec<&dyn rusqlite::ToSql> = vec![query, &limit];
                    if search.query_parameter == 2 {
                        values.insert(0, &revision);
                    }
                    let literal =
                        with_literal_scan(|| search.hits(&db, query, limit, &values)).unwrap();
                    let actual = search.hits(&db, query, limit, &values).unwrap();
                    assert_eq!(
                        actual, literal,
                        "seed={SEED:x} {} {query:?} limit={limit}",
                        search.source
                    );
                    if let Some(expression) = match_expression(query) {
                        values.push(&expression);
                        // Exercise all ordered tiers directly at normal size,
                        // independent of the production density heuristic.
                        let ordered = search
                            .ordered_hits(&db, query, limit, can_match_an_id(query), &values)
                            .unwrap();
                        assert_eq!(
                            unranked(ordered),
                            literal,
                            "ordered seed={SEED:x} {} {query:?} limit={limit}",
                            search.source
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn common_interior_rank_two_walk_stops_before_the_candidate_suffix() {
        use rusqlite::StatementStatus;
        let db = ranking_fixture();
        for at in 1..=5 {
            insert_ranked(&db, at, 2, Some(&format!("0_oca_{at}")));
        }
        // Raw-name winners deliberately have reverse rowids, not FTS order.
        for at in 0..32 {
            insert_ranked(&db, 100 - at, 1, Some(&format!("a_oca_{at:03}")));
        }
        let measure = |search: Search<'_>| {
            let sql = search.ordered_sql(2, false, false);
            let expression = match_expression("oca").unwrap();
            let limit = 5_i64;
            let revision = "pin:v1:test:1";
            let mut values: Vec<&dyn rusqlite::ToSql> = vec![&"oca", &limit, &expression];
            if search.query_parameter == 2 {
                values.insert(0, &revision);
            }
            let plan: Vec<String> = db
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(values.as_slice(), |r| r.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter()
                    .any(|p| p.contains("USING COVERING INDEX") && p.contains("_search_name")),
                "{plan:?}"
            );
            // SQLite can flatten correlated EXISTS into a bounded FTS semi-join.
            assert!(
                plan.iter().any(|p| p.contains("CORRELATED SCALAR SUBQUERY")
                    || p.contains("EXISTS VIRTUAL TABLE INDEX")),
                "{plan:?}"
            );
            assert!(
                plan.iter().any(|p| p.starts_with("SEARCH m USING ")
                    && p.contains("INDEX revision_documents_")
                    && p.contains("revision_id=?")),
                "{plan:?}"
            );
            assert!(
                plan.iter().any(|p| p.contains("VIRTUAL TABLE INDEX")
                    && p.contains('=')
                    && p.contains('M')),
                "{plan:?}"
            );
            assert!(
                !plan.iter().any(|p| p.contains("TEMP B-TREE")
                    || p.contains("MATERIALIZE")
                    || p.contains("LIST SUBQUERY")),
                "{plan:?}"
            );
            let mut statement = db.prepare(&sql).unwrap();
            let rows: Vec<(String, Vec<u8>, i64)> = statement
                .query_map(values.as_slice(), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(rows.len(), 5);
            let ids: Vec<_> = rows
                .iter()
                .map(|(_, id, _)| super::super::id_codec::decode_sid(id).unwrap())
                .collect();
            assert_eq!(
                ids,
                (96..=100)
                    .rev()
                    .map(|id| format!("sid:v1:{id:032x}"))
                    .collect::<Vec<_>>()
            );
            assert!(rows.iter().all(|(_, _, rank)| *rank == 2));
            assert_eq!(statement.get_status(StatementStatus::Sort), 0);
            let scans = statement.get_status(StatementStatus::FullscanStep);
            assert!(scans < 20, "visited {scans} source rows: {plan:?}");
            eprintln!("rank2 plan {plan:?}; source fullscan steps={scans}");
            (rows, scans)
        };
        let native = measure(w2_search());
        let graph = measure(graph_search());
        for at in 1000..1500 {
            insert_ranked(&db, at, 1, Some(&format!("z_oca_{at}")));
        }
        assert_eq!(measure(w2_search()), native);
        assert_eq!(measure(graph_search()), graph);
    }

    #[test]
    fn short_and_nul_queries_take_the_literal_scan() {
        for query in ["a", "ab", "éa", "é", "日本", "a\0b", "\0\0\0", "abc\0"] {
            assert_eq!(match_expression(query), None, "{query:?}");
        }
        assert_eq!(match_expression("abc").as_deref(), Some("\"abc\""));
        // Three characters in four bytes still form one trigram.
        assert_eq!(match_expression("éab").as_deref(), Some("\"éab\""));
        assert_eq!(with_literal_scan(|| match_expression("abc")), None);
        assert_eq!(match_expression("abc").as_deref(), Some("\"abc\""));
    }

    #[test]
    fn id_shaped_queries_are_exactly_the_substrings_an_id_can_contain() {
        let id = format!("sid:v1:{}", "0123456789abcdef".repeat(2));
        // Every substring of a canonical ID, in any ASCII case, is ID-shaped.
        for start in 0..id.len() {
            for end in start + 1..=id.len() {
                let part = &id[start..end];
                assert!(can_match_an_id(part), "{part:?}");
                assert!(can_match_an_id(&part.to_ascii_uppercase()), "{part:?}");
            }
        }
        for query in [
            "f0001", "add", "DEAD", "sid", "v1:", ":v1:ab", "d:v1:0", "1:a", "sid:v1:",
        ] {
            assert!(can_match_an_id(query), "{query:?}");
        }
        let too_long = format!("sid:v1:{}", "a".repeat(33));
        let many_hex = "a".repeat(33);
        for query in [
            "parseConfig",
            "peerValue",
            "zzzznotfound",
            "f0001x",
            "g00",
            "sid:v2:",
            "sid:v1:ag",
            "ab:",
            "1:g",
            "sid:v1:sid",
            "éab",
            "a b",
            too_long.as_str(),
            many_hex.as_str(),
        ] {
            assert!(!can_match_an_id(query), "{query:?}");
        }
    }

    #[test]
    fn the_query_is_ascii_lowered_and_quoted_as_one_literal() {
        assert_eq!(match_expression("FooÉ").as_deref(), Some("\"fooÉ\""));
        assert_eq!(
            match_expression("a\"b OR c").as_deref(),
            Some("\"a\"\"b or c\"")
        );
    }

    /// Contention on either search, through the prefilter or the literal
    /// scan, is the typed `storage_busy` that MCP and CLI reads retry within
    /// their deadline, never a failed or emptied answer.
    #[test]
    fn contention_on_either_path_is_retryable_storage_busy() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.js"), "function Alpha() {}\n").unwrap();
        let store = super::super::Store::open_for_tests(state.path(), workspace.path()).unwrap();
        crate::index_coordinator::reconcile_workspace(
            &store,
            &crate::indexer::IndexOptions::new(workspace.path().to_owned()),
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let reader = store.cache_write().unwrap();
        reader.busy_timeout(std::time::Duration::ZERO).unwrap();
        let holder = store.cache_write().unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
        for query in ["alpha", "al"] {
            for error in [
                w2_hits(&reader, "pin:v1:any", query, 21).unwrap_err(),
                graph_hits(&reader, query, 50).unwrap_err(),
            ] {
                assert!(
                    super::super::cli_read_retryable_contention(&error),
                    "{query:?}: {error:#}"
                );
            }
        }
        holder.execute_batch("ROLLBACK").unwrap();
        assert_eq!(w2_hits(&reader, "pin:v1:any", "alpha", 21).unwrap(), []);
        assert_eq!(graph_hits(&reader, "alpha", 50).unwrap().len(), 1);
    }

    #[test]
    fn corrupt_graph_node_id_fails_search_and_marks_rebuild() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.js"), "function Alpha() {}\n").unwrap();
        let store = super::super::Store::open_for_tests(state.path(), workspace.path()).unwrap();
        crate::index_coordinator::reconcile_workspace(
            &store,
            &crate::indexer::IndexOptions::new(workspace.path().to_owned()),
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert_eq!(store.symbols("Alpha", 20).unwrap().len(), 1);
        assert_eq!(
            store.disposition(),
            super::super::RecoveryDisposition::Ready
        );

        // The graph's labels reference this key. Simulate an on-disk corrupt
        // BLOB using Store's connection rather than opening a second FD.
        {
            let db = store.cache_write().unwrap();
            db.pragma_update(None, "foreign_keys", "OFF").unwrap();
            let changed = db
                .execute("UPDATE graph_nodes SET id=x'00' WHERE name='Alpha'", [])
                .unwrap();
            assert_eq!(changed, 1);
        }
        let error = store.symbols("Alpha", 20).unwrap_err();
        assert_eq!(
            super::super::recovery_class(&error),
            super::super::RecoveryClass::Rebuild
        );
        assert_eq!(
            store.disposition(),
            super::super::RecoveryDisposition::Rebuild
        );
    }

    /// The triggers keep each table equal to its source rows through inserts,
    /// deletes (as retention and rebaseline delete), updates and reused rowids:
    /// for every query the table's candidates are exactly the literal name
    /// matches.
    #[test]
    fn triggers_keep_the_tables_exact_through_deletes_updates_and_reused_rowids() {
        let db = Connection::open_in_memory().unwrap();
        // Lone rows without their parents; this test is about the triggers.
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        db.execute_batch(super::super::CACHE_SCHEMA_V26).unwrap();
        let insert = |n: i64, name: Option<&str>| {
            let id = format!("sid:v1:{n:032x}");
            let end = name.map(|_| 1);
            db.execute(
                "INSERT INTO native_version_declarations(rowid,vid,syntax_id,kind,role,name,
                    lookup_key,key_signature_present,key_ordinal,start_byte,end_byte,name_start,name_end)
                    VALUES(?1,1,?2,'function','production',?3,?3,0,0,0,1,?4,?5)",
                rusqlite::params![n, super::super::id_codec::encode_sid(&id).unwrap(), name, end.map(|_| 0), end],
            )
            .unwrap();
            db.execute(
                "INSERT INTO graph_nodes(rowid,gid,id,name,kind) VALUES(?1,1,?2,?3,'module')",
                rusqlite::params![
                    n,
                    super::super::id_codec::encode_sid(&id).unwrap(),
                    name.unwrap_or("a.js")
                ],
            )
            .unwrap();
        };
        let names = ["AlphaBeta", "alphaGamma", "ÜNÏcode", "beta_alpha", "Gamma"];
        for (n, name) in names.iter().enumerate() {
            insert(n as i64 + 1, Some(name));
        }
        insert(6, None);
        // Retire the highest rowids so new rows reuse them, rename one row,
        // then delete everything the way a rebaseline does for one version.
        db.execute_batch(
            "DELETE FROM native_version_declarations WHERE rowid>=5;
            DELETE FROM graph_nodes WHERE rowid>=5;
            UPDATE native_version_declarations SET name='Renamed',lookup_key='Renamed' WHERE rowid=2;
            UPDATE graph_nodes SET name='Renamed' WHERE rowid=2;",
        )
        .unwrap();
        insert(5, Some("deltaALPHA"));
        insert(6, Some("Omega"));
        for query in [
            "alp", "ALPHA", "gam", "Gamma", "ren", "ünï", "ÜNÏ", "a.js", "sid:v1:", "0005", "meg",
        ] {
            let expression = match_expression(query).unwrap();
            for (table, source) in [
                ("native_declaration_trigrams", "native_version_declarations"),
                ("graph_node_trigrams", "graph_nodes"),
            ] {
                let ids = |sql: &str, values: &[&dyn rusqlite::ToSql]| -> Vec<i64> {
                    db.prepare(sql)
                        .unwrap()
                        .query_map(values, |r| r.get(0))
                        .unwrap()
                        .collect::<rusqlite::Result<_>>()
                        .unwrap()
                };
                let candidates = ids(
                    &format!("SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY rowid"),
                    &[&expression],
                );
                let literal = ids(
                    &format!(
                        "SELECT rowid FROM {source} WHERE instr(lower(name),lower(?1))>0 ORDER BY rowid"
                    ),
                    &[&query],
                );
                assert_eq!(candidates, literal, "{table} {query:?}");
            }
        }
        db.execute_batch("DELETE FROM native_version_declarations; DELETE FROM graph_nodes;")
            .unwrap();
        for table in ["native_declaration_trigrams", "graph_node_trigrams"] {
            let left: i64 = db
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE {table} MATCH '\"sid\"'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(left, 0, "{table}");
        }
    }

    /// Against a real trigram table, operator-shaped queries match literally
    /// and never fail to parse.
    #[test]
    fn match_expressions_parse_as_literals_on_a_trigram_table() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(name,id,content='',contentless_delete=1,
                tokenize='trigram case_sensitive 1');
            INSERT INTO t(rowid,name,id) VALUES(1,lower('Say \"hi\" AND (NEAR) -x*'),'sid:v1:ab'),
                (2,lower('plain'),'sid:v1:cd');",
        )
        .unwrap();
        let rows = |query: &str| -> Vec<i64> {
            db.prepare("SELECT rowid FROM t WHERE t MATCH ?1 ORDER BY rowid")
                .unwrap()
                .query_map([match_expression(query).unwrap()], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        for query in ["\"hi\"", "hi\" ", "AND", "NEAR", "(NEAR)", "-x*", "SAY"] {
            assert_eq!(rows(query), [1], "{query:?}");
        }
        assert_eq!(rows("sid:v1:"), [1, 2]);
        assert_eq!(rows("absent"), Vec::<i64>::new());
    }
}
