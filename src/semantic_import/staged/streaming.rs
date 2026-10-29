//! Complete side discovery followed by document-local Common classification.
use super::super::join_store::{GlobalDiscoverySink, MethodSummary};
use super::super::{
    JoinedFacts, discover_join_claim, emit_join_symbols, emit_type_relationships, join_occurrence,
    method_target_owner_matches, scoped_key, settle_join_references,
};
use super::*;

const DEFINITIONS_SQL: &str = "SELECT owner_language,owner_path,syntax_id FROM g_definitions WHERE side=?1 \
    AND symbol=?2 AND local_language=?3 AND local_path=?4 AND common_compatible=1 \
    ORDER BY owner_language,owner_path,syntax_id";
const TARGET_METHODS_SQL: &str = "SELECT ord,body_bytes,decoded_bytes FROM g_methods WHERE side=?1 AND symbol=?2 AND local_language=?3 AND local_path=?4 ORDER BY ord";
const SOURCE_METHODS_SQL: &str = "SELECT ord,body_bytes,decoded_bytes FROM g_methods WHERE side=?1 AND symbol=?2 AND owner_language=?3 AND owner_path=?4 ORDER BY ord";

#[cfg(feature = "test-support")]
pub(super) fn cursor_sql() -> [&'static str; 3] {
    [DEFINITIONS_SQL, TARGET_METHODS_SQL, SOURCE_METHODS_SQL]
}

struct CompletedGlobal<'a> {
    ctx: &'a JoinCtx<'a>,
    side: u8,
    source_set_id: String,
    _source_reservation: Reservation,
}

impl CompletedGlobal<'_> {
    fn declaration_shape(&self, key: &ScopedKey) -> Result<(usize, u64)> {
        self.ctx.charge(1)?;
        let (language, path) = local_columns(key);
        let mut statement = self.ctx.conn.prepare_cached(DEFINITIONS_SQL)?;
        let mut rows = statement.query(params![self.side, key.0, language, path])?;
        let mut count = 0usize;
        let mut bytes = std::mem::size_of::<Vec<(native::DocumentKey, String)>>() as u64;
        while let Some(row) = rows.next()? {
            self.ctx.charge(1)?;
            count = count
                .checked_add(1)
                .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
            bytes = sizes::sum(&[
                bytes,
                std::mem::size_of::<(native::DocumentKey, String)>() as u64,
                self.source_set_id.len() as u64,
                row.get_ref(0)?.as_str()?.len() as u64,
                row.get_ref(1)?.as_str()?.len() as u64,
                row.get_ref(2)?.as_str()?.len() as u64,
            ])?;
        }
        Ok((count, bytes))
    }
    fn declarations_query(&self, key: &ScopedKey) -> Result<Vec<(native::DocumentKey, String)>> {
        let (count, _) = self.declaration_shape(key)?;
        let mut result = Vec::with_capacity(count);
        self.ctx.charge(1)?;
        let (language, path) = local_columns(key);
        let mut statement = self.ctx.conn.prepare_cached(DEFINITIONS_SQL)?;
        let mut rows = statement.query(params![self.side, key.0, language, path])?;
        while let Some(row) = rows.next()? {
            self.ctx.charge(1)?;
            result.push((
                native::DocumentKey {
                    source_set_id: self.source_set_id.clone(),
                    language: row.get(0)?,
                    path: row.get(1)?,
                },
                row.get(2)?,
            ));
        }
        Ok(result)
    }

    fn presence(&self, symbol: &str) -> Result<(bool, bool)> {
        self.ctx.charge(1)?;
        Ok(self
            .ctx
            .conn
            .prepare_cached(
                "SELECT declared,information FROM g_presence WHERE side=?1 AND symbol=?2",
            )?
            .query_row(params![self.side, symbol], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?
            .unwrap_or_default())
    }

    fn method_shape(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
        target: bool,
    ) -> Result<(usize, u64)> {
        self.ctx.charge(1)?;
        let (language, path) = if target && !symbol.starts_with("local ") {
            ("", "")
        } else {
            (document.language.as_str(), document.path.as_str())
        };
        let sql = if target {
            TARGET_METHODS_SQL
        } else {
            SOURCE_METHODS_SQL
        };
        let mut statement = self.ctx.conn.prepare_cached(sql)?;
        let mut rows = statement.query(params![self.side, symbol, language, path])?;
        let mut count = 0usize;
        let mut bytes = std::mem::size_of::<Vec<MethodSummary>>() as u64;
        while let Some(row) = rows.next()? {
            self.ctx.charge(1)?;
            count = count
                .checked_add(1)
                .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
            bytes = sizes::sum(&[bytes, u64::try_from(row.get::<_, i64>(2)?)?])?;
        }
        Ok((count, bytes))
    }
    fn methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
        target: bool,
    ) -> Result<Vec<MethodSummary>> {
        let (count, _) = self.method_shape(document, symbol, target)?;
        self.ctx.charge(1)?;
        let (language, path) = if target && !symbol.starts_with("local ") {
            ("", "")
        } else {
            (document.language.as_str(), document.path.as_str())
        };
        let sql = if target {
            TARGET_METHODS_SQL
        } else {
            SOURCE_METHODS_SQL
        };
        let mut statement = self.ctx.conn.prepare_cached(sql)?;
        let mut rows = statement.query(params![self.side, symbol, language, path])?;
        let mut result = Vec::with_capacity(count);
        while let Some(row) = rows.next()? {
            self.ctx.charge(1)?;
            let ordinal: i64 = row.get(0)?;
            let body_bytes = u64::try_from(row.get::<_, i64>(1)?)?;

            // Borrowed SQL text avoids a String copy, but serde still owns escape scratch.
            let escape_bytes = if body_bytes == 0 {
                0
            } else {
                body_bytes
                    .checked_mul(2)
                    .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?
                    .max(8)
            };
            let _body_reservation = self
                .ctx
                .live
                .reserve(sizes::sum(&[body_bytes, escape_bytes])?)?;
            self.ctx.charge(1)?;
            let mut body_statement = self
                .ctx
                .conn
                .prepare_cached("SELECT body FROM g_methods WHERE side=?1 AND ord=?2")?;
            let mut body_rows = body_statement.query(params![self.side, ordinal])?;
            let body = body_rows.next()?.context("missing method summary")?;
            self.ctx.charge(1)?;
            result.push(serde_json::from_slice(body.get_ref(0)?.as_bytes()?)?);
        }
        Ok(result)
    }
}

impl GlobalJoinReads for CompletedGlobal<'_> {
    fn declarations_clone_bytes(&self, key: &ScopedKey) -> Result<u64> {
        Ok(self.declaration_shape(key)?.1)
    }
    fn source_methods_clone_bytes(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<u64> {
        Ok(self.method_shape(document, symbol, false)?.1)
    }
    fn target_methods_clone_bytes(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<u64> {
        Ok(self.method_shape(document, symbol, true)?.1)
    }
    fn declarations(&self, key: &ScopedKey) -> Result<Vec<(native::DocumentKey, String)>> {
        self.declarations_query(key)
    }
    fn is_declared_symbol(&self, symbol: &str) -> Result<bool> {
        Ok(self.presence(symbol)?.0)
    }
    fn is_information_symbol(&self, symbol: &str) -> Result<bool> {
        Ok(self.presence(symbol)?.1)
    }
    fn source_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<Vec<MethodSummary>> {
        self.methods(document, symbol, false)
    }
    fn target_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<Vec<MethodSummary>> {
        self.methods(document, symbol, true)
    }
}

struct GlobalWriter<'a> {
    global: CompletedGlobal<'a>,
    methods: i64,
}

impl GlobalDiscoverySink for GlobalWriter<'_> {
    fn symbol_presence(&mut self, symbol: &str, declared: bool, information: bool) -> Result<()> {
        self.global.ctx.row()?;
        self.global.ctx.conn.prepare_cached(
            "INSERT INTO g_presence VALUES(?1,?2,?3,?4) ON CONFLICT(side,symbol) DO UPDATE SET declared=MAX(declared,excluded.declared),information=MAX(information,excluded.information)",
        )?.execute(params![self.global.side,symbol,declared,information])?;
        Ok(())
    }
    fn exact_definition(
        &mut self,
        key: ScopedKey,
        document: native::DocumentKey,
        syntax_id: String,
        common_compatible: bool,
    ) -> Result<()> {
        let (language, path) = local_columns(&key);
        self.global.ctx.row()?;
        self.global.ctx.conn.prepare_cached(
            "INSERT INTO g_definitions VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(side,symbol,local_language,local_path,owner_language,owner_path,syntax_id) DO UPDATE SET common_compatible=MAX(common_compatible,excluded.common_compatible)",
        )?.execute(params![self.global.side,key.0,language,path,document.language,document.path,syntax_id,common_compatible])?;
        Ok(())
    }
}

struct CountJson {
    bytes: u64,
    overflow: bool,
}
impl CountJson {
    fn measure(value: &impl Serialize) -> Result<u64> {
        let mut count = Self {
            bytes: 0,
            overflow: false,
        };
        let result = serde_json::to_writer(&mut count, value);
        if count.overflow {
            return Err(ScipCaptureError::LimitExceeded("unit expansion".into()).into());
        }
        result?;
        Ok(count.bytes)
    }
}
impl std::io::Write for CountJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(total) = self.bytes.checked_add(bytes.len() as u64) else {
            self.overflow = true;
            return Err(std::io::Error::other("unit expansion"));
        };
        self.bytes = total;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl GlobalWriter<'_> {
    fn method(
        &mut self,
        document: &native::DocumentKey,
        symbol: &str,
        summary: MethodSummary,
    ) -> Result<()> {
        let _key_reservation = self.global.ctx.live.reserve(sizes::sum(&[
            std::mem::size_of::<ScopedKey>() as u64,
            symbol.len() as u64,
            if symbol.starts_with("local ") {
                sizes::document(document)?
            } else {
                0
            },
        ])?)?;
        let key = scoped_key(symbol, document);
        let (language, path) = local_columns(&key);
        let decoded_bytes = sizes::decoded_method(&summary)?;
        let body_bytes = CountJson::measure(&summary)?;
        let _body_reservation = self.global.ctx.live.reserve(body_bytes)?;
        let mut body = Vec::with_capacity(usize::try_from(body_bytes)?);
        serde_json::to_writer(&mut body, &summary)?;
        let body = std::str::from_utf8(&body)?;
        self.global.ctx.row()?;
        self.global
            .ctx
            .conn
            .prepare_cached("INSERT INTO g_methods VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)")?
            .execute(params![
                self.global.side,
                self.methods,
                symbol,
                language,
                path,
                document.language,
                document.path,
                body,
                i64::try_from(body_bytes)?,
                i64::try_from(decoded_bytes)?,
            ])?;
        self.methods += 1;
        Ok(())
    }
}

/// Admit a new requested buffer while the old buffer remains live, then move the row.
fn append_reserved<T>(
    rows: &mut Vec<T>,
    row: T,
    payload: u64,
    retained: &mut Reservation,
) -> Result<()> {
    append_reserved_with(rows, payload, retained, || row)
}

fn append_reserved_with<T>(
    rows: &mut Vec<T>,
    payload: u64,
    retained: &mut Reservation,
    make_row: impl FnOnce() -> T,
) -> Result<()> {
    let capacity = rows.capacity();
    if rows.len() == capacity {
        let next = capacity
            .checked_mul(2)
            .map(|n| n.max(4))
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
        let bytes = (next as u64)
            .checked_mul(std::mem::size_of::<T>() as u64)
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
        retained.grow(sizes::sum(&[payload, bytes])?)?;
        rows.reserve_exact(next - rows.len());
        retained.release((capacity * std::mem::size_of::<T>()) as u64);
    } else {
        retained.grow(payload)?;
    }
    rows.push(make_row());
    Ok(())
}

struct DocumentIndex<'g, 'a> {
    global: &'g CompletedGlobal<'a>,
    document: native::DocumentKey,
    retained: Reservation,
    type_uses: BTreeMap<(usize, usize), BTreeSet<String>>,
    occurrences: Vec<OccurrenceRow>,
    relationships: Vec<RelationshipRow>,
    endpoints: BTreeMap<String, BTreeMap<String, Vec<usize>>>,
    reference_claims: BTreeMap<String, Vec<(String, semantic::Target)>>,
    call_references: BTreeMap<String, String>,
}

impl<'g, 'a> DocumentIndex<'g, 'a> {
    fn new(global: &'g CompletedGlobal<'a>, document: &native::DocumentKey) -> Result<Self> {
        let retained = global.ctx.live.reserve(sizes::sum(&[
            std::mem::size_of::<Self>() as u64,
            sizes::document(document)?,
        ])?)?;
        Ok(Self {
            global,
            document: document.clone(),
            retained,
            type_uses: BTreeMap::new(),
            occurrences: Vec::new(),
            relationships: Vec::new(),
            endpoints: BTreeMap::new(),
            reference_claims: BTreeMap::new(),
            call_references: BTreeMap::new(),
        })
    }

    fn check_document(&self, language: &str, path: &str) -> Result<()> {
        anyhow::ensure!(
            self.document.language == language && self.document.path == path,
            "foreign document in local join state"
        );
        Ok(())
    }
}

impl GlobalJoinReads for DocumentIndex<'_, '_> {
    fn declarations_clone_bytes(&self, key: &ScopedKey) -> Result<u64> {
        self.global.declarations_clone_bytes(key)
    }
    fn source_methods_clone_bytes(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<u64> {
        self.global.source_methods_clone_bytes(document, symbol)
    }
    fn target_methods_clone_bytes(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<u64> {
        self.global.target_methods_clone_bytes(document, symbol)
    }
    fn declarations(&self, key: &ScopedKey) -> Result<Vec<(native::DocumentKey, String)>> {
        self.global.declarations(key)
    }
    fn is_declared_symbol(&self, symbol: &str) -> Result<bool> {
        self.global.is_declared_symbol(symbol)
    }
    fn is_information_symbol(&self, symbol: &str) -> Result<bool> {
        self.global.is_information_symbol(symbol)
    }
    fn source_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<Vec<MethodSummary>> {
        self.global.source_methods(document, symbol)
    }
    fn target_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<Vec<MethodSummary>> {
        self.global.target_methods(document, symbol)
    }
}

impl DocumentJoinState for DocumentIndex<'_, '_> {
    type ClaimReservation = Reservation;
    fn put_type_use(
        &mut self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        symbol: &str,
    ) -> Result<()> {
        self.check_document(&document.language, &document.path)?;
        let key = (start, end);
        if self
            .type_uses
            .get(&key)
            .is_some_and(|symbols| symbols.contains(symbol))
        {
            return Ok(());
        }
        let entry = if self.type_uses.contains_key(&key) {
            0
        } else {
            sizes::map_entries::<(usize, usize), BTreeSet<String>>(1)?
        };
        self.retained.grow(sizes::sum(&[
            entry,
            sizes::map_entries::<String, ()>(1)?,
            symbol.len() as u64,
        ])?)?;
        self.type_uses.entry(key).or_default().insert(symbol.into());
        Ok(())
    }
    fn type_uses_clone_bytes(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
    ) -> Result<u64> {
        self.check_document(&document.language, &document.path)?;
        self.type_uses
            .get(&(start, end))
            .into_iter()
            .flatten()
            .try_fold(
                std::mem::size_of::<BTreeSet<String>>() as u64,
                |total, symbol| {
                    sizes::sum(&[
                        total,
                        sizes::map_entries::<String, ()>(1)?,
                        symbol.len() as u64,
                    ])
                },
            )
    }
    fn type_uses(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
    ) -> Result<BTreeSet<String>> {
        self.check_document(&document.language, &document.path)?;
        Ok(self
            .type_uses
            .get(&(start, end))
            .cloned()
            .unwrap_or_default())
    }
    fn put_occurrence(&mut self, row: OccurrenceRow) -> Result<()> {
        self.check_document(&row.document.language, &row.document.path)?;
        let payload = sizes::occurrence(&row)? - std::mem::size_of::<OccurrenceRow>() as u64;
        append_reserved(&mut self.occurrences, row, payload, &mut self.retained)
    }
    fn occurrence_count(&self) -> usize {
        self.occurrences.len()
    }
    fn occurrence_clone_bytes(&self, ordinal: usize) -> Result<u64> {
        sizes::occurrence(
            self.occurrences
                .get(ordinal)
                .context("missing document occurrence")?,
        )
    }
    fn occurrence(&self, ordinal: usize) -> Result<OccurrenceRow> {
        let row = self
            .occurrences
            .get(ordinal)
            .context("missing document occurrence")?;
        Ok(row.clone())
    }
    fn put_relationship(&mut self, language: &str, path: &str, row: RelationshipRow) -> Result<()> {
        self.check_document(language, path)?;
        let payload = sizes::relationship(&row)? - std::mem::size_of::<RelationshipRow>() as u64;
        append_reserved(&mut self.relationships, row, payload, &mut self.retained)
    }
    fn relationships_clone_bytes(&self, language: &str, path: &str) -> Result<u64> {
        self.check_document(language, path)?;
        self.relationships.iter().try_fold(
            std::mem::size_of::<Vec<RelationshipRow>>() as u64,
            |total, row| sizes::sum(&[total, sizes::relationship(row)?]),
        )
    }
    fn relationships(&self, language: &str, path: &str) -> Result<Vec<RelationshipRow>> {
        self.check_document(language, path)?;
        Ok(self.relationships.clone())
    }
    fn put_relationship_endpoint(
        &mut self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
        _row: &RelationshipRow,
    ) -> Result<()> {
        self.check_document(language, path)?;
        let ordinal = self.relationships.len() - 1;
        if let Some(rows) = self
            .endpoints
            .get_mut(source)
            .and_then(|targets| targets.get_mut(target))
        {
            return append_reserved(rows, ordinal, 0, &mut self.retained);
        }
        let outer = if self.endpoints.contains_key(source) {
            0
        } else {
            sizes::sum(&[
                source.len() as u64,
                sizes::map_entries::<String, BTreeMap<String, Vec<usize>>>(1)?,
            ])?
        };
        let entry = sizes::sum(&[
            outer,
            target.len() as u64,
            sizes::map_entries::<String, Vec<usize>>(1)?,
        ])?;
        let mut rows = Vec::new();
        append_reserved(&mut rows, ordinal, entry, &mut self.retained)?;
        if let Some(targets) = self.endpoints.get_mut(source) {
            targets.insert(target.into(), rows);
        } else {
            self.endpoints
                .insert(source.into(), BTreeMap::from([(target.into(), rows)]));
        }
        Ok(())
    }
    fn relationship_endpoints_clone_bytes(
        &self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
    ) -> Result<u64> {
        self.check_document(language, path)?;
        self.endpoints
            .get(source)
            .and_then(|targets| targets.get(target))
            .into_iter()
            .flatten()
            .try_fold(
                std::mem::size_of::<Vec<RelationshipRow>>() as u64,
                |total, ordinal| {
                    sizes::sum(&[total, sizes::relationship(&self.relationships[*ordinal])?])
                },
            )
    }
    fn relationship_endpoints(
        &self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
    ) -> Result<Vec<RelationshipRow>> {
        self.check_document(language, path)?;
        let ordinals = self
            .endpoints
            .get(source)
            .and_then(|targets| targets.get(target));
        let mut result = Vec::with_capacity(ordinals.map_or(0, Vec::len));
        result.extend(
            ordinals
                .into_iter()
                .flatten()
                .map(|ordinal| self.relationships[*ordinal].clone()),
        );
        Ok(result)
    }
    fn put_reference_claim(
        &mut self,
        reference_id: &str,
        proof_id: &str,
        target: semantic::Target,
    ) -> Result<()> {
        let entry = if self.reference_claims.contains_key(reference_id) {
            0
        } else {
            sizes::sum(&[
                reference_id.len() as u64,
                sizes::map_entries::<String, Vec<(String, semantic::Target)>>(1)?,
            ])?
        };
        let payload = sizes::sum(&[sizes::target_payload(&target)?, proof_id.len() as u64])?;
        if let Some(rows) = self.reference_claims.get_mut(reference_id) {
            append_reserved_with(rows, payload, &mut self.retained, || {
                (proof_id.into(), target)
            })?;
        } else {
            let mut rows = Vec::new();
            append_reserved_with(
                &mut rows,
                sizes::sum(&[entry, payload])?,
                &mut self.retained,
                || (proof_id.into(), target),
            )?;
            self.reference_claims.insert(reference_id.into(), rows);
        }
        Ok(())
    }
    fn take_reference_claims(
        &mut self,
        reference_id: &str,
    ) -> Result<(Vec<(String, semantic::Target)>, Reservation)> {
        let mut moved = self.global.ctx.live.reserve(0)?;
        let Some(claims) = self.reference_claims.get(reference_id) else {
            return Ok((vec![], moved));
        };
        let bytes = claims.iter().try_fold(
            sizes::sum(&[
                std::mem::size_of::<Vec<(String, semantic::Target)>>() as u64,
                (claims.capacity() * std::mem::size_of::<(String, semantic::Target)>()) as u64,
            ])?,
            |total, (proof, target)| {
                sizes::sum(&[total, proof.len() as u64, sizes::target_payload(target)?])
            },
        )?;
        // The returned guard follows moved payload until this settlement item ends.
        self.retained.transfer_to(&mut moved, bytes);
        let result = self
            .reference_claims
            .remove(reference_id)
            .expect("entry just found");
        self.retained.release(sizes::sum(&[
            reference_id.len() as u64,
            sizes::map_entries::<String, Vec<(String, semantic::Target)>>(1)?
                - std::mem::size_of::<Vec<(String, semantic::Target)>>() as u64,
        ])?);
        Ok((result, moved))
    }
    fn put_call_reference(&mut self, proof_id: &str, reference_id: &str) -> Result<()> {
        if let Some(value) = self.call_references.get_mut(proof_id) {
            let previous = value.len() as u64;
            self.retained.grow(reference_id.len() as u64)?;
            *value = reference_id.into();
            self.retained.release(previous);
        } else {
            self.retained.grow(sizes::sum(&[
                sizes::map_entries::<String, String>(1)?,
                proof_id.len() as u64,
                reference_id.len() as u64,
            ])?)?;
            self.call_references
                .insert(proof_id.into(), reference_id.into());
        }
        Ok(())
    }
    fn call_reference_clone_bytes(&self, proof_id: &str) -> Result<u64> {
        Ok(self.call_references.get(proof_id).map_or(0, |value| {
            std::mem::size_of::<String>() as u64 + value.len() as u64
        }))
    }
    fn call_reference(&self, proof_id: &str) -> Result<Option<String>> {
        Ok(self.call_references.get(proof_id).cloned())
    }
}

struct DocumentSink {
    facts: JoinedFacts,
    retained: Reservation,
    reference_indices: BTreeMap<String, usize>,
}

impl DocumentSink {
    fn new(global: &CompletedGlobal<'_>) -> Result<Self> {
        let retained = global
            .ctx
            .live
            .reserve(std::mem::size_of::<Self>() as u64)?;
        Ok(Self {
            facts: JoinedFacts::default(),
            retained,
            reference_indices: BTreeMap::new(),
        })
    }

    fn into_facts(self) -> (JoinedFacts, Reservation) {
        (self.facts, self.retained)
    }
}

impl JoinSink for DocumentSink {
    fn symbol(&mut self, row: semantic::Symbol) -> Result<()> {
        let payload = sizes::symbol(&row)? - std::mem::size_of::<semantic::Symbol>() as u64;
        append_reserved(&mut self.facts.symbols, row, payload, &mut self.retained)
    }

    fn declaration_binding(&mut self, row: semantic::DeclarationBinding) -> Result<()> {
        let payload =
            sizes::binding(&row)? - std::mem::size_of::<semantic::DeclarationBinding>() as u64;
        append_reserved(
            &mut self.facts.declaration_bindings,
            row,
            payload,
            &mut self.retained,
        )
    }

    fn join(&mut self, _proof_id: &str, _join: &semantic::Join) -> Result<()> {
        Ok(())
    }

    fn reference_claim(&mut self, proof_id: &str, row: semantic::Reference) -> Result<()> {
        if let Some(previous) = self.facts.reference_claims.get_mut(proof_id) {
            let old = sizes::reference(previous)?;
            self.retained.grow(sizes::reference(&row)?)?;
            *previous = row;
            self.retained.release(old);
        } else {
            self.retained.grow(sizes::sum(&[
                sizes::map_entries::<String, semantic::Reference>(1)?,
                proof_id.len() as u64,
                sizes::reference(&row)? - std::mem::size_of::<semantic::Reference>() as u64,
            ])?)?;
            self.facts.reference_claims.insert(proof_id.into(), row);
        }
        Ok(())
    }
    fn reference_slot(&self, reference_id: &str) -> Result<Option<usize>> {
        Ok(self.reference_indices.get(reference_id).copied())
    }

    fn reference_count(&self) -> usize {
        self.facts.references.len()
    }

    fn reference_clone_bytes(&self, slot: usize) -> Result<u64> {
        sizes::reference(
            self.facts
                .references
                .get(slot)
                .context("missing reference row")?,
        )
    }
    fn reference(&self, slot: usize) -> Result<semantic::Reference> {
        let row = self
            .facts
            .references
            .get(slot)
            .context("missing reference row")?;
        Ok(row.clone())
    }

    fn put_reference(&mut self, slot: Option<usize>, row: semantic::Reference) -> Result<usize> {
        let key = if slot.is_none() {
            sizes::sum(&[row.id.len() as u64, sizes::map_entries::<String, usize>(1)?])?
        } else {
            0
        };
        let payload = sizes::reference(&row)? - std::mem::size_of::<semantic::Reference>() as u64;
        match slot {
            Some(slot) => {
                let destination = self
                    .facts
                    .references
                    .get_mut(slot)
                    .context("missing reference row")?;
                let previous = sizes::reference(destination)?
                    - std::mem::size_of::<semantic::Reference>() as u64;
                self.retained.grow(payload)?;
                *destination = row;
                self.retained.release(previous);
                Ok(slot)
            }
            None => {
                let slot = self.facts.references.len();
                append_reserved(
                    &mut self.facts.references,
                    row,
                    sizes::sum(&[key, payload])?,
                    &mut self.retained,
                )?;
                self.reference_indices
                    .insert(self.facts.references[slot].id.clone(), slot);
                Ok(slot)
            }
        }
    }

    fn call_binding_count(&self) -> usize {
        self.facts.call_bindings.len()
    }

    fn call_binding_clone_bytes(&self, slot: usize) -> Result<u64> {
        sizes::call(
            self.facts
                .call_bindings
                .get(slot)
                .context("missing call binding row")?,
        )
    }
    fn call_binding(&self, slot: usize) -> Result<semantic::CallBinding> {
        let row = self
            .facts
            .call_bindings
            .get(slot)
            .context("missing call binding row")?;
        Ok(row.clone())
    }

    fn put_call_binding(
        &mut self,
        slot: Option<usize>,
        row: semantic::CallBinding,
    ) -> Result<usize> {
        let payload = sizes::call(&row)? - std::mem::size_of::<semantic::CallBinding>() as u64;
        match slot {
            Some(slot) => {
                let destination = self
                    .facts
                    .call_bindings
                    .get_mut(slot)
                    .context("missing call binding row")?;
                let previous =
                    sizes::call(destination)? - std::mem::size_of::<semantic::CallBinding>() as u64;
                self.retained.grow(payload)?;
                *destination = row;
                self.retained.release(previous);
                Ok(slot)
            }
            None => {
                append_reserved(
                    &mut self.facts.call_bindings,
                    row,
                    payload,
                    &mut self.retained,
                )?;
                Ok(self.facts.call_bindings.len() - 1)
            }
        }
    }

    fn type_relationship(&mut self, row: semantic::TypeRelationship) -> Result<()> {
        let payload = sizes::type_relationship(&row)?
            - std::mem::size_of::<semantic::TypeRelationship>() as u64;
        append_reserved(
            &mut self.facts.type_relationships,
            row,
            payload,
            &mut self.retained,
        )
    }

    fn diagnostic(&mut self, document: &native::DocumentKey, code: &str) -> Result<()> {
        if self
            .facts
            .diagnostics
            .get(document)
            .is_some_and(|codes| codes.contains(code))
        {
            return Ok(());
        }
        let entry = if self.facts.diagnostics.contains_key(document) {
            0
        } else {
            sizes::sum(&[
                sizes::document(document)?,
                sizes::map_entries::<native::DocumentKey, BTreeSet<String>>(1)?,
            ])?
        };
        self.retained.grow(sizes::sum(&[
            entry,
            code.len() as u64,
            sizes::map_entries::<String, ()>(1)?,
        ])?)?;
        if let Some(codes) = self.facts.diagnostics.get_mut(document) {
            codes.insert(code.into());
        } else {
            self.facts
                .diagnostics
                .insert(document.clone(), BTreeSet::from([code.into()]));
        }
        Ok(())
    }
}

struct DiscardDiscovery;
impl GlobalDiscoverySink for DiscardDiscovery {
    fn symbol_presence(
        &mut self,
        _symbol: &str,
        _declared: bool,
        _information: bool,
    ) -> Result<()> {
        Ok(())
    }
    fn exact_definition(
        &mut self,
        _key: ScopedKey,
        _document: native::DocumentKey,
        _syntax_id: String,
        _compatible: bool,
    ) -> Result<()> {
        Ok(())
    }
}

pub(super) fn native_document_side<'a>(
    side: &StagedSide<'a>,
    document: native::DocumentKey,
) -> Result<StagedSide<'a>> {
    let group = side.group(&document)?;
    Ok(StagedSide {
        ctx: side.ctx,
        reader: side.reader,
        header: side.header,
        rev: side.rev,
        side: side.side,
        filtered: side.filtered,
        document: Some(document),
        local_claims: None,
        local_group: group,
    })
}

pub(super) fn document_side<'a>(
    side: &StagedSide<'a>,
    document: native::DocumentKey,
) -> Result<StagedSide<'a>> {
    let claims = DocumentClaims::load(side, &document)?;
    let mut local = native_document_side(side, document)?;
    local.local_claims = Some(Rc::new(claims));
    Ok(local)
}

fn discover_side<'a>(side: &StagedSide<'a>, encoding: u8) -> Result<CompletedGlobal<'a>> {
    let global = || -> Result<CompletedGlobal<'a>> {
        Ok(CompletedGlobal {
            ctx: side.ctx,
            side: side.side,
            _source_reservation: side.ctx.live.reserve(side.source_set_id().len() as u64)?,
            source_set_id: side.source_set_id().to_owned(),
        })
    };
    let mut writer = GlobalWriter {
        global: global()?,
        methods: 0,
    };
    for document in side.documents() {
        let document = document?;
        let _view_document_reservation = side.ctx.live.reserve(sizes::sum(&[
            std::mem::size_of::<native::DocumentKey>() as u64,
            sizes::document(&document)?,
        ])?)?;
        let local_store = document_side(side, document.clone())?;
        let reads = global()?;
        let mut local = DocumentIndex::new(&reads, &document)?;
        for claim in local_store.claims() {
            let claim = claim?;
            let mut operation = local_store.operation()?;
            if let Some((range, row)) =
                discover_join_claim(encoding, &local_store, &mut writer, &claim, &mut operation)?
            {
                local.put_type_use(&document, range.start, range.end, &row.symbol)?;
            }
        }
        for declaration in &local_store
            .borrowed_group(&document)?
            .context("missing native document")?
            .native
            .declarations
        {
            if declaration.kind != "method" {
                continue;
            }
            let Some(range) = &declaration.name_range else {
                continue;
            };
            let mut symbols_operation = local_store.operation()?;
            local_store.reserve_operation(
                &mut symbols_operation,
                local.type_uses_clone_bytes(&document, range.start, range.end)?,
            )?;
            for symbol in local.type_uses(&document, range.start, range.end)? {
                let _summary_reservation = side.ctx.live.reserve(sizes::sum(&[
                    sizes::declaration(declaration)?,
                    (std::mem::size_of::<MethodSummary>()
                        - std::mem::size_of::<native::Declaration>()) as u64,
                ])?)?;
                let summary = MethodSummary {
                    target_owner_matches: method_target_owner_matches(
                        &local_store,
                        &local,
                        declaration,
                        &symbol,
                    )?,
                    method: declaration.clone(),
                };
                writer.method(&document, &symbol, summary)?;
            }
        }
    }
    Ok(writer.global)
}

fn join_document(
    store: &StagedSide<'_>,
    global: &CompletedGlobal<'_>,
    encoding: u8,
) -> Result<(JoinedFacts, Reservation)> {
    let mut index = DocumentIndex::new(global, store.document.as_ref().expect("document view"))?;
    for claim in store.claims() {
        let claim = claim?;
        let mut operation = store.operation()?;
        if let Some((range, row)) = discover_join_claim(
            encoding,
            store,
            &mut DiscardDiscovery,
            &claim,
            &mut operation,
        )? {
            index.put_type_use(&row.document, range.start, range.end, &row.symbol)?;
            index.put_occurrence(row)?;
        }
    }
    super::super::discover_relationship_claims(store, &mut index)?;
    let mut sink = DocumentSink::new(global)?;
    emit_join_symbols(store, &index, &mut sink)?;
    let mut role_sites = None;
    for ordinal in 0..index.occurrence_count() {
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, index.occurrence_clone_bytes(ordinal)?)?;
        let row = index.occurrence(ordinal)?;
        join_occurrence(
            encoding,
            store,
            &mut index,
            &mut sink,
            row,
            &mut role_sites,
            &mut operation,
        )?;
    }
    settle_join_references(store, &mut index, &mut sink)?;
    emit_type_relationships(store, &index, &mut sink)?;
    Ok(sink.into_facts())
}

fn flush_document(facts: JoinedFacts, sink: &mut StagedFactSink<'_>) -> Result<()> {
    for row in facts.symbols {
        sink.symbol(row)?;
    }
    for row in facts.declaration_bindings {
        sink.declaration_binding(row)?;
    }
    for (proof, row) in facts.reference_claims {
        sink.reference_claim(&proof, row)?;
    }
    for row in facts.references {
        sink.put_reference(None, row)?;
    }
    for row in facts.call_bindings {
        sink.put_call_binding(None, row)?;
    }
    for row in facts.type_relationships {
        sink.type_relationship(row)?;
    }
    for (document, codes) in facts.diagnostics {
        for code in codes {
            sink.diagnostic(&document, &code)?;
        }
    }
    Ok(())
}

pub(super) fn join_side(
    side: &StagedSide<'_>,
    encoding: u8,
    sink: &mut StagedFactSink<'_>,
) -> Result<()> {
    let global = discover_side(side, encoding)?;
    for document in side.documents() {
        let store = document_side(side, document?)?;
        let (facts, _facts_reservation) = join_document(&store, &global, encoding)?;
        #[cfg(feature = "test-support")]
        crate::semantic_batch::testing::run_document_common_observer(
            side.side,
            store.document.as_ref().expect("document view"),
            &facts,
            side.ctx.conn,
        );
        flush_document(facts, sink)?;
    }
    Ok(())
}

#[cfg(test)]
mod reservation_tests {
    use super::*;
    fn method_fixture() -> MethodSummary {
        MethodSummary {
            target_owner_matches: true,
            method: native::Declaration {
                role: native::DeclarationRole::Production,
                syntax_id: "method-é".into(),
                document: native::DocumentKey {
                    source_set_id: "source".into(),
                    language: "java".into(),
                    path: "é.java".into(),
                },
                revision_id: "revision".into(),
                kind: "method".into(),
                name: Some("f\n".into()),
                lookup_key: Some("f".into()),
                ancestors: vec![],
                key: native::Key {
                    kind: "method".into(),
                    name: Some("f".into()),
                    signature: Some(native::Signature {
                        parameter_types: vec!["Té".into()],
                        type_parameter_count: 0,
                        variadic: false,
                    }),
                    ordinal: 0,
                },
                range: native::Range { start: 0, end: 1 },
                name_range: Some(native::Range { start: 0, end: 1 }),
                header: native::Header {
                    kind: "method".into(),
                    name: Some("f".into()),
                    modifiers: vec!["public".into()],
                    type_parameters: vec![],
                    parameters: vec![native::Parameter {
                        name: Some("p".into()),
                        type_name: Some("Té".into()),
                        variadic: false,
                    }],
                    result_type: None,
                    bases: vec![],
                },
                provenance_id: "proof".into(),
            },
        }
    }

    #[test]
    fn sql_declaration_metadata_uses_utf8_bytes_and_exact_result_capacity() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE g_definitions(side, symbol, local_language, local_path, owner_language, owner_path, syntax_id, common_compatible);").unwrap();
        for (path, id) in [("é.java", "id-é"), ("β.java", "id-β")] {
            conn.execute(
                "INSERT INTO g_definitions VALUES(0,'pkg Té#','','','java',?1,?2,1)",
                params![path, id],
            )
            .unwrap();
        }
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let key = ("pkg Té#".into(), None);
        let expected = std::mem::size_of::<Vec<(native::DocumentKey, String)>>() as u64
            + 2 * std::mem::size_of::<(native::DocumentKey, String)>() as u64
            + [("é.java", "id-é"), ("β.java", "id-β")]
                .iter()
                .map(|(path, id)| ("source".len() + "java".len() + path.len() + id.len()) as u64)
                .sum::<u64>();
        assert_eq!(global.declarations_clone_bytes(&key).unwrap(), expected);
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - expected + 1)
            .unwrap();
        let mut operation = ctx.live.reserve(0).unwrap();
        let error = (|| -> Result<Vec<(native::DocumentKey, String)>> {
            operation.grow(global.declarations_clone_bytes(&key)?)?;
            global.declarations(&key)
        })()
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        drop(blocker);
        operation.grow(expected).unwrap();
        let rows = global.declarations(&key).unwrap();
        assert_eq!(rows.capacity(), 2);
        assert_eq!(
            rows.iter()
                .map(|(doc, id)| (doc.path.as_str(), id.as_str()))
                .collect::<Vec<_>>(),
            [("é.java", "id-é"), ("β.java", "id-β")]
        );
        drop(rows);
        drop(operation);
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    #[test]
    fn sql_methods_exact_capacity_and_scratch_denial_precedes_decode() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE g_methods(side,ord,symbol,local_language,local_path,owner_language,owner_path,body,body_bytes,decoded_bytes);").unwrap();
        let first = method_fixture();
        let mut second = method_fixture();
        second.method.syntax_id = "method-β".into();
        for (ordinal, summary) in [(0, &first), (1, &second)] {
            let body = serde_json::to_string(summary).unwrap();
            conn.execute(
                "INSERT INTO g_methods VALUES(0,?1,'pkg Té#','','','java','é.java',?2,?3,?4)",
                params![
                    ordinal,
                    body,
                    body.len() as i64,
                    sizes::decoded_method(summary).unwrap() as i64
                ],
            )
            .unwrap();
        }
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let document = first.method.document.clone();
        let bytes = global
            .target_methods_clone_bytes(&document, "pkg Té#")
            .unwrap();
        assert_eq!(
            bytes,
            std::mem::size_of::<Vec<MethodSummary>>() as u64
                + sizes::decoded_method(&first).unwrap()
                + sizes::decoded_method(&second).unwrap()
        );
        assert_eq!(
            global
                .source_methods_clone_bytes(&document, "pkg Té#")
                .unwrap(),
            bytes
        );
        let operation = ctx.live.reserve(bytes).unwrap();
        let rows = global.target_methods(&document, "pkg Té#").unwrap();
        assert_eq!(rows.capacity(), 2);
        assert_eq!(rows[0].method, first.method);
        assert_eq!(rows[1].method, second.method);
        assert!(rows.iter().all(|row| row.target_owner_matches));
        let probe = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - bytes)
            .unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(rows);
        drop(operation);
        conn.execute("DELETE FROM g_methods", []).unwrap();
        conn.execute(
            "INSERT INTO g_methods VALUES(0,0,'pkg Té#','','','java','é.java','{',1,?1)",
            [sizes::decoded_method(&first).unwrap() as i64],
        )
        .unwrap();
        let bytes = global
            .target_methods_clone_bytes(&document, "pkg Té#")
            .unwrap();
        let operation = ctx.live.reserve(bytes).unwrap();
        let scratch = 1 + 8;
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - bytes - scratch + 1)
            .unwrap();
        let error = global
            .target_methods(&document, "pkg Té#")
            .err()
            .expect("scratch denial");
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        drop(blocker);
        let error = global
            .target_methods(&document, "pkg Té#")
            .err()
            .expect("corrupt body");
        assert!(error.downcast_ref::<serde_json::Error>().is_some());
        let probe = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - bytes)
            .unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(operation);
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    #[test]
    fn local_getter_owners_release_between_calls_and_overlap_when_live() {
        let conn = Connection::open_in_memory().unwrap();
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let document = native::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let mut index = DocumentIndex::new(&global, &document).unwrap();
        index.put_type_use(&document, 1, 2, "pkg A#").unwrap();
        let occupied = std::mem::size_of_val(&index) as u64
            + sizes::document(&document).unwrap()
            + sizes::map_entries::<(usize, usize), BTreeSet<String>>(1).unwrap()
            + sizes::map_entries::<String, ()>(1).unwrap()
            + "pkg A#".len() as u64;
        let bytes = index.type_uses_clone_bytes(&document, 1, 2).unwrap();
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - occupied - bytes + 1)
            .unwrap();
        let error = ctx
            .live
            .reserve(bytes)
            .err()
            .expect("one-byte-short lookup admission");
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        drop(blocker);
        for _ in 0..2 {
            let operation = ctx.live.reserve(bytes).unwrap();
            let symbols = index.type_uses(&document, 1, 2).unwrap();
            assert_eq!(symbols, BTreeSet::from(["pkg A#".into()]));
            let probe = ctx
                .live
                .reserve(ctx.limits.unit_expansion_bytes - occupied - bytes)
                .unwrap();
            assert!(ctx.live.reserve(1).is_err());
            drop(probe);
            drop(symbols);
            drop(operation);
        }
        let first = ctx.live.reserve(bytes).unwrap();
        let first_rows = index.type_uses(&document, 1, 2).unwrap();
        let second = ctx.live.reserve(bytes).unwrap();
        let second_rows = index.type_uses(&document, 1, 2).unwrap();
        let probe = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - occupied - 2 * bytes)
            .unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(first_rows);
        drop(second_rows);
        drop(first);
        drop(second);
        drop(index);
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    #[test]
    fn denied_reference_append_leaves_no_reference_slot_or_key_charge() {
        let conn = Connection::open_in_memory().unwrap();
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let mut sink = DocumentSink::new(&global).unwrap();
        let row = semantic::Reference {
            id: "ref".into(),
            owner_syntax_id: "owner".into(),
            ordinal: 0,
            document: native::DocumentKey {
                source_set_id: "source".into(),
                language: "java".into(),
                path: "A.java".into(),
            },
            revision_id: "revision".into(),
            range: native::Range { start: 1, end: 2 },
            spelling: "A".into(),
            lookup_key: "A".into(),
            site: semantic::ReferenceSite::Use,
            roles: vec![],
            resolution: semantic::Resolution::Unresolved,
            declared_target: None,
            candidates: vec![],
            provenance_id: "proof".into(),
        };
        let payload =
            sizes::reference(&row).unwrap() - std::mem::size_of::<semantic::Reference>() as u64;
        let key = row.id.len() as u64 + sizes::map_entries::<String, usize>(1).unwrap();
        let room = key + payload + std::mem::size_of::<semantic::Reference>() as u64;
        let base = std::mem::size_of_val(&sink) as u64;
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - base - room)
            .unwrap();
        let error = sink.put_reference(None, row).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        assert!(sink.facts.references.is_empty());
        assert!(sink.reference_indices.is_empty());
        let probe = ctx.live.reserve(room).unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(blocker);
        drop(sink);
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    fn local_global<'a>(ctx: &'a JoinCtx<'a>) -> CompletedGlobal<'a> {
        CompletedGlobal {
            ctx,
            side: 0,
            source_set_id: "source".into(),
            _source_reservation: ctx.live.reserve(0).unwrap(),
        }
    }

    #[test]
    fn denied_local_vector_insertions_leave_maps_and_charges_unchanged() {
        let conn = Connection::open_in_memory().unwrap();
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let document = native::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let mut index = DocumentIndex::new(&global, &document).unwrap();
        let base = std::mem::size_of_val(&index) as u64 + sizes::document(&document).unwrap();
        let target = semantic::Target::External {
            symbol: super::super::super::symbol_key("pkg A#", &document),
        };
        let payload = 1 + sizes::target_payload(&target).unwrap();
        let entry = 1 + sizes::map_entries::<String, Vec<(String, semantic::Target)>>(1).unwrap();
        let room = entry + payload + std::mem::size_of::<(String, semantic::Target)>() as u64;
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - base - room)
            .unwrap();
        let error = index.put_reference_claim("r", "p", target).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        assert!(index.reference_claims.is_empty());
        let probe = ctx.live.reserve(room).unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(blocker);
        let relationship = RelationshipRow {
            proof_id: "p".into(),
            source_symbol: Some("s".into()),
            target_symbol: Some("t".into()),
        };
        let row_payload = sizes::relationship(&relationship).unwrap()
            - std::mem::size_of::<RelationshipRow>() as u64;
        index
            .put_relationship("java", "A.java", relationship.clone())
            .unwrap();
        let occupied = base
            + row_payload
            + (index.relationships.capacity() * std::mem::size_of::<RelationshipRow>()) as u64;
        let entry = sizes::sum(&[
            2,
            sizes::map_entries::<String, BTreeMap<String, Vec<usize>>>(1).unwrap(),
            sizes::map_entries::<String, Vec<usize>>(1).unwrap(),
        ])
        .unwrap();
        let room = entry + std::mem::size_of::<usize>() as u64;
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - occupied - room)
            .unwrap();
        assert!(
            index
                .put_relationship_endpoint("java", "A.java", "s", "t", &relationship)
                .is_err()
        );
        assert!(index.endpoints.is_empty());
        assert_eq!(index.relationships.len(), 1);
        let probe = ctx.live.reserve(room).unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(blocker);
        drop(index);
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    #[test]
    fn moved_claims_keep_spare_buffer_slots_at_exhausted_budget() {
        let conn = Connection::open_in_memory().unwrap();
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let document = native::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let mut index = DocumentIndex::new(&global, &document).unwrap();
        let target = semantic::Target::External {
            symbol: super::super::super::symbol_key("pkg A#", &document),
        };
        let payload = 1 + sizes::target_payload(&target).unwrap();
        index.put_reference_claim("r", "p", target).unwrap();
        let capacity = index.reference_claims["r"].capacity();
        let buffer = (capacity * std::mem::size_of::<(String, semantic::Target)>()) as u64;
        let base = std::mem::size_of_val(&index) as u64 + sizes::document(&document).unwrap();
        let entry = 1 + sizes::map_entries::<String, Vec<(String, semantic::Target)>>(1).unwrap();
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - base - entry - buffer - payload)
            .unwrap();
        assert!(ctx.live.reserve(1).is_err());
        let (claims, moved) = index.take_reference_claims("r").unwrap();
        assert_eq!(claims.capacity(), capacity);
        assert_eq!(claims[0].0, "p");
        assert!(index.reference_claims.is_empty());
        drop(index);
        drop(blocker);
        let held = std::mem::size_of::<Vec<(String, semantic::Target)>>() as u64 + buffer + payload;
        let probe = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - held)
            .unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(claims);
        drop(moved);
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    #[test]
    fn vector_admission_pays_spare_slots_and_old_new_overlap() {
        let slot = std::mem::size_of::<usize>() as u64;
        let budget = LiveBudget::new(slot);
        let mut retained = budget.reserve(0).unwrap();
        let mut rows = Vec::new();
        assert!(append_reserved(&mut rows, 1usize, 0, &mut retained).is_err());
        assert!(rows.is_empty());
        assert_eq!(rows.capacity(), 0);
        drop(retained);
        drop(budget.reserve(slot).unwrap());

        let budget = LiveBudget::new(8 * slot);
        let mut retained = budget.reserve(0).unwrap();
        for value in 0usize..4 {
            append_reserved(&mut rows, value, 0, &mut retained).unwrap();
        }
        assert_eq!(rows.capacity(), 4);
        assert!(append_reserved(&mut rows, 4, 0, &mut retained).is_err());
        assert_eq!(rows, [0, 1, 2, 3]);
        assert_eq!(rows.capacity(), 4);
        drop(rows);
        drop(retained);
        drop(budget.reserve(8 * slot).unwrap());

        let budget = LiveBudget::new(12 * slot);
        let mut retained = budget.reserve(0).unwrap();
        let mut rows = Vec::new();
        for value in 0usize..5 {
            append_reserved(&mut rows, value, 0, &mut retained).unwrap();
        }
        assert_eq!(rows.capacity(), 8);
        let spare = budget.reserve(4 * slot).unwrap();
        assert!(budget.reserve(1).is_err());
        drop(spare);
        drop(rows);
        drop(retained);
        drop(budget.reserve(12 * slot).unwrap());
    }

    fn row_context(conn: &Connection, limits: BatchLimits) -> JoinCtx<'_> {
        JoinCtx {
            conn,
            budget: RefCell::new(LookupBudget::new(&limits)),
            live: LiveBudget::new(limits.unit_expansion_bytes),
            limits,
            counters: RefCell::new(BatchCounters::default()),
            cache: RefCell::new(DocCache::default()),
            rows: Cell::new(0),
            stage_base: stage_bytes(conn).unwrap(),
            mutations: DestinationMutations::default(),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[test]
    fn bulk_row_admission_denies_before_sql_and_counts_duplicate_attempts() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE input(id INTEGER PRIMARY KEY); INSERT INTO input VALUES(1),(2),(3); CREATE TABLE output(id INTEGER PRIMARY KEY);").unwrap();
        let mut limits = BatchLimits::PRODUCTION;
        limits.staged_rows = 2;
        let ctx = row_context(&conn, limits.clone());
        let error = ctx
            .insert_select(
                "SELECT COUNT(*) FROM input",
                "INSERT INTO output SELECT * FROM input",
            )
            .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="staged rows")
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM output", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        limits.staged_rows = 5;
        let ctx = row_context(&conn, limits);
        ctx.insert_select(
            "SELECT COUNT(*) FROM input",
            "INSERT INTO output SELECT * FROM input",
        )
        .unwrap();
        for _ in 0..2 {
            ctx.row().unwrap();
            conn.execute("INSERT OR IGNORE INTO output VALUES(1)", [])
                .unwrap();
        }
        assert_eq!(ctx.rows.get(), 5);
        assert!(ctx.row().is_err());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM output", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            3
        );
        conn.execute("DELETE FROM output WHERE id=1", []).unwrap();
        assert_eq!(
            ctx.rows.get(),
            6,
            "failed and deleted attempts are not refunded"
        );
    }

    #[test]
    fn bulk_crossing_checks_physical_growth_after_sql_and_seed_is_native_only() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE input(body TEXT); INSERT INTO input VALUES(zeroblob(4000)),(zeroblob(4000)),(zeroblob(4000)); CREATE TABLE output(body TEXT); CREATE TABLE nat_docs(id); CREATE TABLE nat_declarations(id); INSERT INTO nat_docs VALUES(1); INSERT INTO nat_declarations VALUES(1),(2); CREATE TABLE claims(id); INSERT INTO claims VALUES(1),(2);").unwrap();
        assert_eq!(retained_rows(&conn).unwrap(), 3);
        let mut limits = BatchLimits::PRODUCTION;
        limits.output_bytes = 0;
        let ctx = row_context(&conn, limits);
        ctx.reserve_rows(OUTPUT_CHECK_ROWS - 1).unwrap();
        let error = ctx
            .insert_select(
                "SELECT COUNT(*) FROM input",
                "INSERT INTO output SELECT * FROM input",
            )
            .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="output bytes")
        );
        assert_eq!(ctx.rows.get(), OUTPUT_CHECK_ROWS + 2);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM output", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            3,
            "growth check runs after bulk materialization"
        );
        let mut limits = BatchLimits::PRODUCTION;
        limits.staged_rows = 2;
        let ctx = row_context(&conn, limits);
        assert!(ctx.reserve_rows(retained_rows(&conn).unwrap()).is_err());
        ctx.rows.set(u64::MAX);
        assert!(ctx.reserve_rows(1).is_err());
        assert_eq!(ctx.rows.get(), u64::MAX);
    }

    #[test]
    fn role_span_admission_precedes_insert_and_duplicate_needs_no_charge() {
        let bytes = sizes::map_entries::<(usize, usize), ()>(1).unwrap();
        let denied = LiveBudget::new(bytes - 1);
        let mut operation = denied.reserve(0).unwrap();
        let mut spans = BTreeSet::new();
        let error = super::super::super::reserve_role_span(&mut spans, (1, 2), &mut |bytes| {
            operation.grow(bytes)
        })
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        assert!(spans.is_empty());
        drop(operation);
        drop(denied.reserve(bytes - 1).unwrap());

        let budget = LiveBudget::new(bytes);
        let mut operation = budget.reserve(0).unwrap();
        super::super::super::reserve_role_span(&mut spans, (1, 2), &mut |bytes| {
            operation.grow(bytes)
        })
        .unwrap();
        super::super::super::reserve_role_span(&mut spans, (1, 2), &mut |_| {
            panic!("duplicate admission")
        })
        .unwrap();
        assert_eq!(spans.len(), 1);
        assert!(budget.reserve(1).is_err());
        drop(spans);
        drop(operation);
        drop(budget.reserve(bytes).unwrap());
    }

    #[test]
    fn role_parser_owners_release_on_success_and_partial_denial() {
        let source = b"import p.T; class A { T x; T y; }";
        let root = std::mem::size_of::<RoleSites>() as u64;
        let entry = sizes::map_entries::<(usize, usize), ()>(1).unwrap();
        let budget = LiveBudget::new(BatchLimits::PRODUCTION.unit_expansion_bytes);
        let mut operation = budget.reserve(0).unwrap();
        let sites = super::super::super::source_role_sites_reserved("java", source, &mut |bytes| {
            operation.grow(bytes)
        })
        .unwrap();
        let expected = super::super::super::source_role_sites("java", source).unwrap();
        assert_eq!(sites.types, expected.types);
        assert_eq!(sites.imports, expected.imports);
        assert!(!sites.types.is_empty());
        assert!(!sites.imports.is_empty());
        let charged = root + (sites.types.len() + sites.imports.len()) as u64 * entry;
        let remaining = budget
            .reserve(BatchLimits::PRODUCTION.unit_expansion_bytes - charged)
            .unwrap();
        assert!(budget.reserve(1).is_err());
        drop(remaining);
        drop(sites);
        drop(operation);
        drop(
            budget
                .reserve(BatchLimits::PRODUCTION.unit_expansion_bytes)
                .unwrap(),
        );

        let limit = root + entry - 1;
        let denied = LiveBudget::new(limit);
        let error = (|| -> Result<RoleSites> {
            let mut operation = denied.reserve(0)?;
            super::super::super::source_role_sites_reserved("java", source, &mut |bytes| {
                operation.grow(bytes)
            })
        })()
        .err()
        .expect("role entry should be denied");
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        drop(denied.reserve(limit).unwrap());
    }

    #[test]
    fn json_counter_reports_typed_overflow_without_wrapping() {
        use std::io::Write;
        let mut count = CountJson {
            bytes: u64::MAX,
            overflow: false,
        };
        assert!(count.write(b"x").is_err());
        assert!(count.overflow);
        assert_eq!(count.bytes, u64::MAX);
        let value = serde_json::json!({"name":"é\n"});
        assert_eq!(
            CountJson::measure(&value).unwrap(),
            serde_json::to_vec(&value).unwrap().len() as u64
        );
    }

    #[test]
    fn repeated_local_updates_only_charge_their_live_owners() {
        let conn = Connection::open_in_memory().unwrap();
        let limits = BatchLimits::PRODUCTION;
        let ctx = JoinCtx {
            conn: &conn,
            limits: limits.clone(),
            budget: RefCell::new(LookupBudget::new(&limits)),
            live: LiveBudget::new(limits.unit_expansion_bytes),
            counters: RefCell::new(BatchCounters::default()),
            cache: RefCell::new(DocCache::default()),
            rows: Cell::new(0),
            stage_base: 0,
            mutations: DestinationMutations::default(),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let global = CompletedGlobal {
            ctx: &ctx,
            side: 0,
            source_set_id: "source".into(),
            _source_reservation: ctx.live.reserve(0).unwrap(),
        };
        let document = native::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let mut index = DocumentIndex::new(&global, &document).unwrap();
        index.put_type_use(&document, 1, 2, "pkg A#").unwrap();
        let type_high = ctx.live.high_water();
        for _ in 0..20 {
            index.put_type_use(&document, 1, 2, "pkg A#").unwrap();
        }
        assert_eq!(ctx.live.high_water(), type_high);
        let mut sink = DocumentSink::new(&global).unwrap();
        sink.diagnostic(&document, "unmeasurable_override_endpoint")
            .unwrap();
        let diagnostic_high = ctx.live.high_water();
        for _ in 0..20 {
            sink.diagnostic(&document, "unmeasurable_override_endpoint")
                .unwrap();
        }
        assert_eq!(ctx.live.high_water(), diagnostic_high);
        index.put_call_reference("proof", "reference").unwrap();
        index.put_call_reference("proof", "reference").unwrap();
        let replacement_high = ctx.live.high_water();
        for _ in 0..20 {
            index.put_call_reference("proof", "reference").unwrap();
        }
        assert_eq!(ctx.live.high_water(), replacement_high);
        for _ in 0..2 {
            let _lookup = ctx
                .live
                .reserve(index.type_uses_clone_bytes(&document, 1, 2).unwrap())
                .unwrap();
            assert_eq!(index.type_uses(&document, 1, 2).unwrap().len(), 1);
        }
        let scratch_before = ctx.live.high_water();
        let target = semantic::Target::External {
            symbol: super::super::super::symbol_key("pkg A#", &document),
        };
        index
            .put_reference_claim("reference", "proof", target)
            .unwrap();
        let after_insert = ctx.live.high_water();
        let (claims, moved) = index.take_reference_claims("reference").unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(ctx.live.high_water(), after_insert);
        drop(claims);
        drop(moved);
        // Only existing type/call indexes and the diagnostic remain; no moved owner.
        drop(index);
        drop(sink);
        drop(ctx.live.reserve(limits.unit_expansion_bytes).unwrap());
        assert!(after_insert >= scratch_before);
    }
}
