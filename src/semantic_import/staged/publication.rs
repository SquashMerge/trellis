//! Reads the sealed stage's `out_*` rows back as one semantic publication per coverage row.
//! Every family is emitted document-major, so one cursor per family in `ord` order is merged
//! against the coverage rows: memory holds one document, and no per-document index is read.
use super::codec;
use crate::semantic_evidence as semantic;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Rows};

/// One family's rows in `ord` order, handed out a document at a time.
struct Family<'s, T> {
    rows: Rows<'s>,
    pending: Option<(i64, T)>,
    last: i64,
    decode: fn(&rusqlite::Row<'_>) -> Result<(i64, T)>,
}

impl<'s, T> Family<'s, T> {
    fn new(rows: Rows<'s>, decode: fn(&rusqlite::Row<'_>) -> Result<(i64, T)>) -> Self {
        Self {
            rows,
            pending: None,
            last: i64::MIN,
            decode,
        }
    }

    fn fill(&mut self) -> Result<()> {
        if self.pending.is_none()
            && let Some(row) = self.rows.next()?
        {
            let (doc, value) = (self.decode)(row)?;
            ensure!(doc >= self.last, "staged output order");
            self.last = doc;
            self.pending = Some((doc, value));
        }
        Ok(())
    }

    /// The rows of coverage document `doc`. A row of an earlier document has no coverage row.
    fn take(&mut self, doc: i64) -> Result<Vec<T>> {
        let mut rows = vec![];
        loop {
            self.fill()?;
            match &self.pending {
                Some((row_doc, _)) if *row_doc == doc => {
                    rows.push(self.pending.take().expect("pending row").1);
                }
                Some((row_doc, _)) => {
                    ensure!(*row_doc > doc, "staged row without coverage");
                    return Ok(rows);
                }
                None => return Ok(rows),
            }
        }
    }

    fn finish(&mut self) -> Result<()> {
        self.fill()?;
        ensure!(self.pending.is_none(), "staged row without coverage");
        Ok(())
    }
}

type RawProvenance = (rusqlite::types::Value, i64, String, String, String);

/// One provenance row with its raw claim's fields. Every row is kept (the claim is joined
/// outer): a row whose claim is gone is a corrupt stage and fails the publication, never a
/// row silently dropped with the facts it would own.
fn provenance_row(row: &rusqlite::Row<'_>) -> Result<(i64, RawProvenance)> {
    let claim = |index: usize| -> Result<String> {
        row.get::<_, Option<String>>(index)?
            .ok_or_else(|| anyhow::anyhow!("staged provenance without claim"))
    };
    Ok((
        row.get(0)?,
        (row.get(1)?, row.get(2)?, claim(3)?, claim(4)?, claim(5)?),
    ))
}

fn fact<T: codec::ProvenanceOwned>(row: &rusqlite::Row<'_>) -> Result<(i64, T)> {
    Ok((
        row.get(0)?,
        codec::fact_row(row.get_ref(2)?.as_str()?, row.get_ref(1)?).context("staged output row")?,
    ))
}

/// Calls `each` with the publication of every coverage row, in coverage order. `schema` is
/// the stage's schema prefix: `"stage."` when attached, `""` on the stage connection itself.
pub(crate) fn read_documents(
    conn: &Connection,
    schema: &str,
    producer: &semantic::Producer,
    each: &mut dyn FnMut(semantic::SemanticPublication) -> Result<()>,
) -> Result<()> {
    let facts = |table: &str| {
        conn.prepare(&format!(
            "SELECT doc, provenance_id, body FROM {schema}{table} ORDER BY ord"
        ))
    };
    let mut coverage = conn.prepare(&format!(
        "SELECT ord, body, provenance FROM {schema}out_coverage ORDER BY ord"
    ))?;
    let mut provenance = conn.prepare(&format!(
        "SELECT p.doc, p.id, p.kind, c.proof_id, c.record_kind, c.fact_digest \
         FROM {schema}out_provenance p LEFT JOIN {schema}claims c ON c.claim_id=p.claim_id ORDER BY p.ord"
    ))?;
    let mut symbols = facts("out_symbols")?;
    let mut bindings = facts("out_declaration_bindings")?;
    let mut references = facts("out_references")?;
    let mut calls = facts("out_call_bindings")?;
    let mut relationships = facts("out_type_relationships")?;
    let mut provenance = Family::new(provenance.query([])?, provenance_row);
    let mut symbols = Family::new(symbols.query([])?, fact::<semantic::Symbol>);
    let mut bindings = Family::new(bindings.query([])?, fact::<semantic::DeclarationBinding>);
    let mut references = Family::new(references.query([])?, fact::<semantic::Reference>);
    let mut calls = Family::new(calls.query([])?, fact::<semantic::CallBinding>);
    let mut relationships =
        Family::new(relationships.query([])?, fact::<semantic::TypeRelationship>);
    let mut rows = coverage.query([])?;
    while let Some(row) = rows.next()? {
        let doc: i64 = row.get(0)?;
        let coverage: semantic::Coverage =
            serde_json::from_str(row.get_ref(1)?.as_str()?).context("staged output row")?;
        let template = row
            .get_ref(2)?
            .as_str_or_null()?
            .map(serde_json::from_str::<semantic::Provenance>)
            .transpose()
            .context("staged output row")?;
        let raw = provenance.take(doc)?;
        ensure!(
            raw.is_empty() || template.is_some(),
            "staged provenance without template"
        );
        let provenance_rows = raw
            .into_iter()
            .map(|(id, kind, proof, record_kind, digest)| {
                codec::provenance_row(
                    template.as_ref().expect("template checked"),
                    (&id).into(),
                    kind,
                    proof,
                    &record_kind,
                    digest,
                )
                .context("staged output row")
            })
            .collect::<Result<Vec<_>>>()?;
        each(semantic::SemanticPublication {
            producers: vec![producer.clone()],
            provenance: provenance_rows,
            symbols: symbols.take(doc)?,
            declaration_bindings: bindings.take(doc)?,
            references: references.take(doc)?,
            call_bindings: calls.take(doc)?,
            type_relationships: relationships.take(doc)?,
            coverage: vec![coverage],
        })?;
    }
    provenance.finish()?;
    symbols.finish()?;
    bindings.finish()?;
    references.finish()?;
    calls.finish()?;
    relationships.finish()?;
    Ok(())
}

/// The publication statements, for query-plan tests.
#[cfg(feature = "test-support")]
pub(crate) fn statements(schema: &str) -> Vec<String> {
    let mut statements = vec![
        format!("SELECT ord, body, provenance FROM {schema}out_coverage ORDER BY ord"),
        format!(
            "SELECT p.doc, p.id, p.kind, c.proof_id, c.record_kind, c.fact_digest \
             FROM {schema}out_provenance p LEFT JOIN {schema}claims c ON c.claim_id=p.claim_id ORDER BY p.ord"
        ),
    ];
    for table in [
        "out_symbols",
        "out_declaration_bindings",
        "out_references",
        "out_call_bindings",
        "out_type_relationships",
    ] {
        statements.push(format!(
            "SELECT doc, provenance_id, body FROM {schema}{table} ORDER BY ord"
        ));
    }
    statements
}
