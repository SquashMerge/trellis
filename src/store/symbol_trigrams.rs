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
    /// The selected `a, b, rank` columns; rank orders exact name (0), name
    /// prefix (1), then any other name or ID substring (2).
    columns: &'a str,
    /// The source table and its alias, as `table alias`.
    source: &'a str,
    /// The FTS5 table over its names.
    names: &'a str,
    /// The covering ID scan of the answering revision, selecting `rowid`.
    ids: &'a str,
    /// Everything after the FROM clause's candidate source: the revision
    /// join, `WHERE {candidates}` with the literal rule, ORDER BY and LIMIT.
    rest: &'a str,
    /// The positional parameter the MATCH expression binds to.
    match_parameter: usize,
}

impl Search<'_> {
    /// The ranked `(a, b)` hits. Literal scan for an unrepresentable query.
    /// Otherwise the name candidates, plus the ID scan's rows when the query
    /// can match an ID, unless the shortcut below proves they can't matter.
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
        let mut values = values.to_vec();
        values.push(&expression);
        let alias = self.source.rsplit(' ').next().unwrap_or_default();
        let table = self.source.split(' ').next().unwrap_or_default();
        let names = format!(
            "(SELECT rowid AS r FROM {names} WHERE {names} MATCH ?{p}) c
                CROSS JOIN {table} {alias} ON {alias}.rowid=c.r",
            names = self.names,
            p = self.match_parameter,
        );
        let hits = ranked(db, &self.sql(&names), &values)?;
        if !can_match_an_id(query) || names_decide(&hits, limit) {
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

    fn sql(&self, from: &str) -> String {
        format!("SELECT {} FROM {from} {}", self.columns, self.rest)
    }
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
    Search {
        columns: "d.version_id,d.syntax_id,CASE WHEN lower(d.name)=lower(?2) THEN 0
            WHEN instr(lower(d.name),lower(?2))=1 THEN 1 ELSE 2 END",
        source: "native_version_declarations d",
        names: "native_declaration_trigrams",
        ids: "SELECT d.rowid FROM revision_documents m
            JOIN native_version_declarations d ON d.version_id=m.document_version_id
            WHERE m.revision_id=?1 AND instr(lower(d.syntax_id),lower(?2))>0",
        rest: "JOIN revision_documents m ON m.document_version_id=d.version_id AND m.revision_id=?1
            WHERE instr(lower(d.name),lower(?2))>0 OR instr(lower(d.syntax_id),lower(?2))>0
            ORDER BY 3, d.name COLLATE BINARY ASC, d.syntax_id COLLATE BINARY ASC LIMIT ?3",
        match_parameter: 4,
    }
    .hits(db, query, lookahead, &[&revision, &query, &lookahead])
}

/// The browser and CLI search's ranked `(path, payload)` graph-node hits in
/// the admitted head, at most `limit` of them, under the same literal rule.
pub(super) fn graph_hits(
    db: &Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<(String, String)>> {
    Search {
        columns: "n.path,n.payload,CASE WHEN lower(n.name)=lower(?1) THEN 0 WHEN instr(lower(n.name),lower(?1))=1 THEN 1 ELSE 2 END",
        source: "graph_nodes n",
        names: "graph_node_trigrams",
        ids: "SELECT n.rowid FROM revision_documents m
            JOIN graph_nodes n ON n.projection_id=m.graph_projection_id
            WHERE m.revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1)
            AND instr(lower(n.id),lower(?1))>0",
        rest: "JOIN revision_documents m ON m.revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) AND m.graph_projection_id=n.projection_id WHERE instr(lower(n.name),lower(?1)) > 0 OR instr(lower(n.id),lower(?1)) > 0 ORDER BY 3,n.name,n.id LIMIT ?2",
        match_parameter: 3,
    }
    .hits(db, query, limit, &[&query, &limit])
}

/// Run one search, selecting `(a, b, rank)`. SQLite contention is the typed,
/// retried `storage_busy`; every other error, from FTS5 or not, fails the
/// read. Nothing here retries the literal scan after FTS5 has run.
fn ranked(
    db: &Connection,
    sql: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<Vec<(String, String, i64)>> {
    storage_result((|| {
        db.prepare(sql)?
            .query_map(values, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    })())
}

#[cfg(test)]
thread_local! {
    static FORCE_LITERAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ID_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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

    /// The triggers keep each table equal to its source rows through inserts,
    /// deletes (as retention and rebaseline delete), updates and reused rowids:
    /// for every query the table's candidates are exactly the literal name
    /// matches.
    #[test]
    fn triggers_keep_the_tables_exact_through_deletes_updates_and_reused_rowids() {
        let db = Connection::open_in_memory().unwrap();
        // Lone rows without their parents; this test is about the triggers.
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        db.execute_batch(super::super::CACHE_SCHEMA_V8).unwrap();
        let insert = |n: i64, name: Option<&str>| {
            let id = format!("sid:v1:{n:032x}");
            let end = name.map(|_| 1);
            db.execute(
                "INSERT INTO native_version_declarations(rowid,version_id,syntax_id,kind,role,name,
                    lookup_key,key_signature_present,key_ordinal,start_byte,end_byte,name_start,name_end)
                    VALUES(?1,'v',?2,'function','production',?3,?3,0,0,0,1,?4,?5)",
                rusqlite::params![n, id, name, end.map(|_| 0), end],
            )
            .unwrap();
            db.execute(
                "INSERT INTO graph_nodes(rowid,projection_id,id,name,path,payload) VALUES(?1,'p',?2,?3,'a.js','{}')",
                rusqlite::params![n, id, name.unwrap_or("a.js")],
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
