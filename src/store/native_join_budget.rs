//! Pre-load logical expansion model for the staged join's native projection.
//!
//! Counts requested Rust storage, not allocator/RSS or tree-sitter C memory. The model sums
//! fetch, decoder scratch and returned owners conservatively, even when phases do not overlap.
use super::{ReadRevision, SelectedIntegrity, V8NativeScope, v8_native_scope_for};
use crate::{native_evidence as n, scip_raw::ScipCaptureError};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::mem::size_of;

const CARD_ANCESTORS_SQL: &str = "SELECT COALESCE(length(CAST(json_extract(CASE WHEN j.type='object' THEN j.value ELSE '{}' END,'$.kind') AS BLOB)),0),COALESCE(length(CAST(json_extract(CASE WHEN j.type='object' THEN j.value ELSE '{}' END,'$.name') AS BLOB)),0),COALESCE(length(CAST(json_extract(CASE WHEN j.type='object' THEN j.value ELSE '{}' END,'$.self_type') AS BLOB)),0),COALESCE(length(CAST(json_extract(CASE WHEN j.type='object' THEN j.value ELSE '{}' END,'$.trait_name') AS BLOB)),0) FROM native_version_search_cards c,json_each(c.facts_json,'$.ancestors') j WHERE c.vid=?1";

fn limit() -> anyhow::Error {
    ScipCaptureError::LimitExceeded("unit expansion".into()).into()
}
fn add(parts: &[u64]) -> Result<u64> {
    parts
        .iter()
        .try_fold(0u64, |a, b| a.checked_add(*b).ok_or_else(limit))
}
fn times(a: u64, b: u64) -> Result<u64> {
    a.checked_mul(b).ok_or_else(limit)
}
// Current Vec push growth doubles capacity, with at most four initial slots for these types.
fn vector<T>(rows: u64) -> Result<u64> {
    times(
        if rows == 0 { 0 } else { times(rows, 2)?.max(4) },
        size_of::<T>() as u64,
    )
}
fn grouped<T>(rows: u64, groups: u64) -> Result<u64> {
    times(
        add(&[times(rows, 2)?, times(rows.min(groups), 4)?])?,
        size_of::<T>() as u64,
    )
}
// Logical map-entry model: typed key/value plus four pointers for tree links/occupancy.
// This is a declared estimate coefficient, not an allocator-layout or RSS guarantee.
fn map<K, V>(rows: u64) -> Result<u64> {
    times(
        rows,
        (size_of::<K>() + size_of::<V>() + 4 * size_of::<usize>()) as u64,
    )
}

/// Scalar lengths are accumulated without taking ownership of row bodies or a row Vec.
/// Each fixed SQL returns byte lengths/count flags. Invalid/overflowing sizes fail typed.
fn scan<const N: usize>(db: &Connection, sql: &str, vid: i64) -> Result<(u64, [u64; N])> {
    let mut statement = db.prepare_cached(sql)?;
    let mut rows = statement.query([vid])?;
    let mut count = 0u64;
    let mut totals = [0u64; N];
    while let Some(row) = rows.next()? {
        count = count.checked_add(1).ok_or_else(limit)?;
        for (column, total) in totals.iter_mut().enumerate() {
            let value = u64::try_from(row.get::<_, i64>(column)?).map_err(|_| limit())?;
            *total = total.checked_add(value).ok_or_else(limit)?;
        }
    }
    Ok((count, totals))
}

fn signatures(count: u64, strings: u64, bytes: u64) -> Result<u64> {
    add(&[
        times(
            count,
            (4 + "{\"parameterTypes\":,\"typeParameterCount\":,\"variadic\":}".len() + 2 + 20 + 5)
                as u64,
        )?,
        times(strings, 3)?,
        times(bytes, 6)?,
    ])
}
fn keys(count: u64, kind: u64, name: u64, signatures: u64) -> Result<u64> {
    add(&[
        times(
            count,
            ("{\"kind\":,\"name\":,\"signature\":,\"ordinal\":}".len() + 20 + 2 + 4 + 2 + 4) as u64,
        )?,
        times(add(&[kind, name])?, 6)?,
        signatures,
    ])
}

type Own = (String, i64, String);
type AncSig = (String, i64, i64, String);
type AncRow = (
    String,
    i64,
    String,
    Option<String>,
    i64,
    bool,
    Option<i64>,
    Option<bool>,
);
type HeaderRow = (String, String, Option<String>, Option<String>);
type ItemRow = (String, String, i64, String);
type ParamRow = (String, i64, Option<String>, Option<String>, bool);
type DeclRow = (
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    bool,
    Option<i64>,
    Option<bool>,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
);
type AncValue = (
    i64,
    String,
    Option<String>,
    i64,
    bool,
    Option<i64>,
    Option<bool>,
);
type CallRow = (
    String,
    String,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
    String,
    Option<String>,
);
type RefRow = (String, String, i64, i64, i64, String, String);

pub(super) fn document_bytes(
    db: &Connection,
    selected: &ReadRevision,
    document: &n::Document,
) -> Result<u64> {
    let scope = v8_native_scope_for(db, &document.key, selected)?.context(SelectedIntegrity(
        "incompatible_index: retained native document absent".into(),
    ))?;
    let vid = scope.vid;
    let (_, [source, source_language, source_path, source_hash]) = scan(
        db,
        "SELECT length(source_bytes),length(CAST(language AS BLOB)),length(CAST(path AS BLOB)),length(CAST(content_hash AS BLOB)) FROM document_versions WHERE vid=?1",
        vid,
    )?;
    let (d, [owners, dk, dr, dn, dl, ds]) = scan(
        db,
        "SELECT owner_syntax_id IS NOT NULL,length(CAST(kind AS BLOB)),length(CAST(role AS BLOB)),COALESCE(length(CAST(name AS BLOB)),0),COALESCE(length(CAST(lookup_key AS BLOB)),0),key_signature_present<>0 FROM native_version_declarations WHERE vid=?1",
        vid,
    )?;
    let (o, [ot]) = scan(
        db,
        "SELECT length(CAST(type_name AS BLOB)) FROM native_version_own_signature_types WHERE vid=?1",
        vid,
    )?;
    let (a_s, [ast]) = scan(
        db,
        "SELECT length(CAST(type_name AS BLOB)) FROM native_version_ancestor_signature_types WHERE vid=?1",
        vid,
    )?;
    let (a, [ak, an, a_sig]) = scan(
        db,
        "SELECT length(CAST(kind AS BLOB)),COALESCE(length(CAST(name AS BLOB)),0),signature_present<>0 FROM native_version_declaration_ancestors WHERE vid=?1",
        vid,
    )?;
    let (h, [hk, hn, hr]) = scan(
        db,
        "SELECT length(CAST(kind AS BLOB)),COALESCE(length(CAST(name AS BLOB)),0),COALESCE(length(CAST(result_type AS BLOB)),0) FROM native_version_headers WHERE vid=?1",
        vid,
    )?;
    let (hi, [hic, hit]) = scan(
        db,
        "SELECT length(CAST(item_kind AS BLOB)),length(CAST(value AS BLOB)) FROM native_version_header_items WHERE vid=?1",
        vid,
    )?;
    let (p, [pn, pt]) = scan(
        db,
        "SELECT COALESCE(length(CAST(name AS BLOB)),0),COALESCE(length(CAST(type_name AS BLOB)),0) FROM native_version_parameters WHERE vid=?1",
        vid,
    )?;
    let (cr, [cr_parent, cr_kind, cr_arm]) = scan(
        db,
        "SELECT parent_id IS NOT NULL,length(CAST(kind AS BLOB)),COALESCE(length(CAST(arm AS BLOB)),0) FROM native_version_control_regions WHERE vid=?1",
        vid,
    )?;
    let (c, [cs, cl, ck]) = scan(
        db,
        "SELECT COALESCE(length(CAST(spelling AS BLOB)),0),length(CAST(language AS BLOB)),COALESCE(length(CAST(callee_lookup_key AS BLOB)),0) FROM native_version_calls WHERE vid=?1",
        vid,
    )?;
    let (links, _) = scan::<0>(
        db,
        "SELECT 1 FROM native_version_call_regions WHERE vid=?1",
        vid,
    )?;
    let (r, [rs, rk]) = scan(
        db,
        "SELECT length(CAST(spelling AS BLOB)),length(CAST(COALESCE(lookup_key,spelling) AS BLOB)) FROM native_version_references WHERE vid=?1",
        vid,
    )?;
    let (vr, [vrk, vrt]) = scan(
        db,
        "SELECT length(CAST(role_kind AS BLOB)),length(CAST(role AS BLOB)) FROM native_version_coverage_roles WHERE vid=?1",
        vid,
    )?;
    let (f, [fk, fd]) = scan(
        db,
        "SELECT length(CAST(kind AS BLOB)),COALESCE(length(CAST(diagnostic AS BLOB)),0) FROM native_version_capability_families WHERE vid=?1",
        vid,
    )?;
    let (g, [gk, gd]) = scan(
        db,
        "SELECT length(CAST(kind AS BLOB)),length(CAST(diagnostic AS BLOB)) FROM native_version_capability_gaps WHERE vid=?1",
        vid,
    )?;
    let (cards, [card_text, card_hash, card_json, card_doc, card_head]) = scan(
        db,
        "SELECT length(CAST(card_text AS BLOB)),length(CAST(card_hash AS BLOB)),length(CAST(facts_json AS BLOB)),COALESCE(length(CAST(json_extract(facts_json,'$.facts.doc') AS BLOB)),0),COALESCE(length(CAST(json_extract(facts_json,'$.facts.head') AS BLOB)),0) FROM native_version_search_cards WHERE vid=?1",
        vid,
    )?;
    let (card_list, [card_list_text]) = scan(
        db,
        "SELECT COALESCE(length(CAST(j.value AS BLOB)),0) FROM native_version_search_cards c,json_each(c.facts_json,'$.facts.calls') j WHERE c.vid=?1 UNION ALL \
         SELECT COALESCE(length(CAST(j.value AS BLOB)),0) FROM native_version_search_cards c,json_each(c.facts_json,'$.facts.types') j WHERE c.vid=?1 UNION ALL \
         SELECT COALESCE(length(CAST(j.value AS BLOB)),0) FROM native_version_search_cards c,json_each(c.facts_json,'$.facts.strings') j WHERE c.vid=?1 UNION ALL \
         SELECT COALESCE(length(CAST(j.value AS BLOB)),0) FROM native_version_search_cards c,json_each(c.facts_json,'$.facts.error_codes') j WHERE c.vid=?1 UNION ALL \
         SELECT COALESCE(length(CAST(j.value AS BLOB)),0) FROM native_version_search_cards c,json_each(c.facts_json,'$.facts.members') j WHERE c.vid=?1",
        vid,
    )?;
    let (card_ancestors, [card_ak, card_an, card_as, card_at]) = scan(db, CARD_ANCESTORS_SQL, vid)?;
    let (cov_state, cov_diag): (i64, i64) = db.query_row(
        "SELECT length(CAST(coverage_state AS BLOB)),COALESCE(length(CAST(coverage_diagnostic AS BLOB)),0) FROM revision_documents WHERE revision_id=?1 AND language=?2 AND path=?3 AND vid=?4",
        rusqlite::params![scope.revision_key, scope.key.language, scope.key.path, vid],
        |row| Ok((row.get(0)?, row.get(1)?)))?;
    let cov_state = u64::try_from(cov_state).map_err(|_| limit())?;
    let cov_diag = u64::try_from(cov_diag).map_err(|_| limit())?;
    let doc = add(&[
        scope.key.source_set_id.len() as u64,
        scope.key.language.len() as u64,
        scope.key.path.len() as u64,
    ])?;
    let rev = scope.native_revision_id.len() as u64;
    let producer = scope.producer_id.len() as u64;
    let hash = document.content_hash.len() as u64;
    // Decoded identity formats: seven-byte prefix + 32 hex; proof prefix + SHA256 hex.
    let id = "sid:v1:".len() as u64 + 32;
    let proof = "native-proof:v1:".len() as u64 + 64;
    let identity = add(&[doc, rev, proof])?;
    let child_text = add(&[ot, ast, ak, an, hk, hn, hr, hit, pn, pt])?;
    let fetched_declarations = add(&[
        vector::<Own>(o)?,
        vector::<AncSig>(a_s)?,
        vector::<AncRow>(a)?,
        vector::<HeaderRow>(h)?,
        vector::<ItemRow>(hi)?,
        vector::<ParamRow>(p)?,
        vector::<DeclRow>(d)?,
        times(id, add(&[o, a_s, a, h, hi, p, d, owners])?)?,
        child_text,
        hic,
        dk,
        dr,
        dn,
        dl,
    ])?;
    let fetched_calls = add(&[
        vector::<String>(d)?,
        vector::<(String, String)>(cr)?,
        vector::<CallRow>(c)?,
        vector::<(String, String, i64)>(links)?,
        times(
            id,
            add(&[d, times(cr, 2)?, times(c, 2)?, times(links, 2)?])?,
        )?,
        cs,
        cl,
        ck,
    ])?;
    let declarations = add(&[
        vector::<n::Declaration>(d)?,
        grouped::<n::Key>(a, d)?,
        grouped::<String>(o, d)?,
        grouped::<String>(a_s, a)?,
        grouped::<String>(hi, times(h, 3)?)?,
        grouped::<n::Parameter>(p, h)?,
        times(d, add(&[id, identity])?)?,
        times(dk, 2)?,
        times(dn, 2)?,
        dl,
        child_text,
    ])?;
    let calls = add(&[
        vector::<n::Call>(c)?,
        grouped::<String>(links, c)?,
        times(c, add(&[times(id, 2)?, identity])?)?,
        times(links, id)?,
        cs,
    ])?;
    let references = add(&[
        vector::<n::Reference>(r)?,
        times(r, add(&[times(id, 2)?, identity])?)?,
        rs,
        rk,
    ])?;
    let coverage = add(&[
        size_of::<n::Coverage>() as u64,
        producer,
        doc,
        rev,
        cov_state,
        cov_diag,
        grouped::<String>(vr, 2)?,
        vrt,
    ])?;
    let capability = add(&[
        size_of::<n::CapabilityReport>() as u64,
        doc,
        rev,
        vector::<n::MeasurementSupport>(f)?,
        fd,
        grouped::<n::MeasurementGap>(g, f)?,
        gd,
    ])?;
    let provenance = add(&[
        size_of::<n::Provenance>() as u64,
        proof,
        producer,
        doc,
        rev,
        hash,
        "measuredSyntax".len() as u64,
        "fresh".len() as u64,
    ])?;
    let declaration_scratch = add(&[
        map::<String, Vec<String>>(o)?,
        map::<(String, i64), Vec<String>>(a_s)?,
        map::<String, Vec<AncValue>>(a)?,
        map::<String, n::Header>(h)?,
        map::<(String, String), usize>(hi)?,
        map::<String, usize>(p)?,
        map::<String, Option<String>>(d)?,
        map::<(String, String), String>(d)?,
        grouped::<String>(o, o)?,
        grouped::<String>(a_s, a_s)?,
        grouped::<AncValue>(a, a)?,
        child_text,
        hic,
        times(id, add(&[o, a_s, a, h, hi, p, d, owners])?)?,
    ])?;
    let call_scratch = add(&[
        map::<String, ()>(d)?,
        map::<String, String>(cr)?,
        times(map::<String, usize>(c)?, 2)?,
        times(id, add(&[d, times(cr, 2)?, times(c, 2)?])?)?,
    ])?;
    let reference_scratch = add(&[
        map::<String, ()>(d)?,
        times(d, id)?,
        map::<String, usize>(r)?,
        times(r, id)?,
        size_of::<RefRow>() as u64,
        times(id, 2)?,
        rs,
        rk,
    ])?;
    let coverage_scratch = add(&[
        size_of::<(String, String, i64)>() as u64,
        vrk,
        vrt,
        map::<String, usize>(f)?,
        times(fk, 3)?,
        size_of::<(String, i64, Option<String>, i64)>() as u64,
        fd,
        size_of::<serde_json::Value>() as u64,
        size_of::<(String, i64, i64, i64, String)>() as u64,
        gk,
        gd,
    ])?;
    let json_keys = add(&[
        times(d, 2)?,
        keys(d, dk, dn, signatures(ds, o, ot)?)?,
        times(a, 1)?,
        keys(a, ak, an, signatures(a_sig, a_s, ast)?)?,
    ])?;
    // Two stored JSON strings per declaration, plus a whole-sum allowance for parent checks.
    let json = times(add(&[times(json_keys, 2)?, times(d, 256)?])?, 2)?;
    let scope_bytes = add(&[
        size_of::<V8NativeScope>() as u64,
        scope.revision_key.len() as u64,
        scope.version_id.len() as u64,
        doc,
        rev,
        producer,
    ])?;
    let proof_payload = add(&[producer, doc, rev])?;
    let proof_encoded = add(&[
        "{\"producerId\":,\"document\":{\"sourceSetId\":,\"language\":,\"path\":},\"revisionId\":}"
            .len() as u64,
        10,
        times(proof_payload, 6)?,
    ])?;
    let proof_json = add(&[
        times(map::<String, serde_json::Value>(3)?, 2)?,
        "producerIddocumentrevisionIdsourceSetIdlanguagepath".len() as u64,
        proof_payload,
        times(vector::<&String>(3)?, 2)?,
        times(proof_encoded, 2)?.max(8),
        64,
        size_of::<serde_json::Value>() as u64,
    ])?;
    type ControlRow = (
        String,
        String,
        i64,
        String,
        i64,
        i64,
        Option<String>,
        Option<String>,
    );
    let controls = add(&[
        vector::<String>(d)?,
        vector::<ControlRow>(cr)?,
        times(id, add(&[d, times(cr, 2)?, cr_parent])?)?,
        cr_kind,
        cr_arm,
        vector::<n::ControlRegion>(cr)?,
        times(cr, add(&[times(id, 2)?, identity])?)?,
        times(cr_parent, id)?,
        cr_kind,
        cr_arm,
        map::<String, ()>(d)?,
        times(d, id)?,
        times(map::<String, usize>(cr)?, 2)?,
        times(times(cr, 2)?, id)?,
        proof,
    ])?;
    let card_rows = add(&[
        vector::<crate::search_capture::CapturedCard>(cards)?,
        times(cards, add(&[scope.key.path.len() as u64, id])?)?,
        card_text,
        card_hash,
        grouped::<crate::search_card::Ancestor>(card_ancestors, cards)?,
        card_ak,
        card_an,
        card_as,
        card_at,
        grouped::<String>(card_list, times(cards, 5)?)?,
        card_doc,
        card_head,
        card_list_text,
        size_of::<(Vec<u8>, String, String, String)>() as u64,
        card_json,
        16,
        if card_json == 0 {
            0
        } else {
            times(card_json, 2)?.max(8)
        },
    ])?;
    add(&[
        size_of::<super::NativeDocumentRows>() as u64,
        source,
        source_language,
        source_path,
        source_hash,
        doc,
        hash,
        coverage,
        provenance,
        declarations,
        calls,
        references,
        capability,
        fetched_declarations,
        fetched_calls,
        declaration_scratch,
        call_scratch,
        reference_scratch,
        coverage_scratch,
        json,
        times(d, id)?,
        times(scope_bytes, 4)?,
        controls,
        card_rows,
        proof_json,
        times(proof, 3)?,
        16,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_shape_card_ancestor_probe_leaves_rejection_to_the_typed_reader() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE native_version_search_cards(vid INTEGER,facts_json TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO native_version_search_cards VALUES(1,?1)",
            [r#"{"ancestors":["plain",null,42]}"#],
        )
        .unwrap();
        let result = scan::<4>(&db, CARD_ANCESTORS_SQL, 1).unwrap();
        assert_eq!(result, (3, [0; 4]));
    }
}
