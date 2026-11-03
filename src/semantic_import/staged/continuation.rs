//! Document-sized retained joins. The caller opens and validates a fresh captured/destination
//! snapshot for each advance. Only private stage rows and bounded cursors survive a yield.
use super::*;

const INVENTORY_PAGE_DOCUMENTS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Start,
    Reset,
    ClearDestinationInventory,
    Seed,
    CapturedInventory,
    DestinationInventory,
    CapturedDiscovery,
    ClaimInventory,
    Eligibility,
    DestinationDiscovery,
    DestinationTargets,
    CapturedTargets,
    Settlement,
    Closure,
    OutputClosure,
    OutputRows,
    Report,
    Done,
    Failed,
}

/// A single join attempt over an admitted retained batch. Dropping this cursor does not
/// publish anything or clear the durable pending target. The admitted batch owns its stage.
pub(crate) struct JoinContinuation {
    destination: IndexPin,
    limits: BatchLimits,
    mutations: DestinationMutations,
    phase: Phase,
    last: i64,
    attempt: i64,
    reset_table: usize,
    output_table: usize,
    stage_base: u64,
    rows: u64,
    ledger: BTreeMap<&'static str, u64>,
    emitted: [i64; 7],
    identical_joins: bool,
    tallies: BTreeMap<semantic::Language, LanguageTally>,
    manifest: Option<SourceManifestHasher>,
    manifest_hash: String,
    captured_pin: Option<IndexPin>,
    descriptor_sha256: Option<String>,
}

impl JoinContinuation {
    pub(crate) fn new(
        destination: IndexPin,
        limits: BatchLimits,
        mutations: DestinationMutations,
    ) -> Self {
        Self {
            destination, limits, mutations, phase: Phase::Start, last: -1, attempt: 0, reset_table: 0, output_table: 1,
            stage_base: 0, rows: 0, ledger: BTreeMap::new(), emitted: [0; 7],
            identical_joins: false, tallies: BTreeMap::new(), manifest: None, manifest_hash: String::new(),
            captured_pin: None, descriptor_sha256: None,
        }
    }

    fn transition(&mut self, phase: Phase) {
        self.phase = phase;
        self.last = -1;
    }

    /// Returns `None` after one unit, or a sealed batch after all closure checks. An error
    /// poisons this attempt: discard its private stage instead of resuming partial writes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn advance(
        &mut self,
        admitted: &mut AdmittedBatch,
        captured: &NativeReader<'_>,
        destination_reader: &NativeReader<'_>,
        budget: &mut LookupBudget,
        counters: &mut BatchCounters,
        cancel: &CancelFlag,
    ) -> Result<Option<JoinedBatch>> {
        anyhow::ensure!(self.phase != Phase::Failed && self.phase != Phase::Done,
            "staged continuation is not active");
        let captured_pin = admitted.captured_pin();
        anyhow::ensure!(captured.pin() == captured_pin && destination_reader.pin() == self.destination,
            "staged continuation reader identity changed");
        if let Some(pin) = self.captured_pin {
            anyhow::ensure!(pin == captured_pin
                && self.descriptor_sha256.as_deref() == Some(admitted.descriptor_sha256()),
                "staged continuation admitted identity changed");
        } else {
            self.captured_pin = Some(captured_pin);
            self.descriptor_sha256 = Some(admitted.descriptor_sha256().to_owned());
        }
        let shared = captured_pin == self.destination;
        let stage_budget = admitted.stage().budget(&self.limits);
        let parts = admitted.join_parts()?;
        if self.phase == Phase::Start {
            self.stage_base = stage_bytes(parts.conn)?;
        }
        let ctx = JoinCtx {
            conn: parts.conn, limits: self.limits.clone(), budget: RefCell::new(budget.clone()),
            live: LiveBudget::new(self.limits.unit_expansion_bytes),
            counters: RefCell::new(BatchCounters::default()),
            cache: RefCell::new(DocCache { budget: self.limits.document_cache_bytes,
                ..DocCache::default() }),
            rows: Cell::new(self.rows), ledger: RefCell::new(std::mem::take(&mut self.ledger)),
            passes: RefCell::new(Vec::new()), stage_base: self.stage_base,
            mutations: if shared { DestinationMutations::default() } else { self.mutations },
            cancel: cancel.clone(), unread: RefCell::default(),
        };
        let producer = parts.producer.producer();
        let result = (|| {
            ctx.check_cancel()?;
            let captured_header = captured.header()?;
            let destination_header = destination_reader.header()?;
            let descriptor = parts.descriptor;
            let destination_rev = if shared { 0 } else { 1 };
            let side = |reader, header, rev, side, filtered| StagedSide {
                ctx: &ctx, reader, header, rev, side, filtered,
                document: None, local_claims: None, local_group: None,
            };
            let join = StagedJoin {
                ctx: &ctx, producer: &producer, descriptor,
                admitted_encoding: descriptor.producer.position_encoding,
                captured: side(captured, &captured_header, 0, 0, false),
                destination: side(destination_reader, &destination_header, destination_rev, 1, false),
                filtered: side(destination_reader, &destination_header, destination_rev, 1, true),
                destination_header: &destination_header,
                validator: EmissionValidator::new(EmissionHeader {
                    producer: &producer, captured_revision: captured_header.revision.clone(),
                    destination_revision: destination_header.revision.clone(),
                    source_set_languages: destination_header.source_set.languages.clone(),
                    artifact_hashes: descriptor.artifacts.iter().map(|a| a.sha256.clone()).collect(),
                    source_manifest_hash: descriptor.source_manifest_hash.clone(),
                }),
                basis: StagedBasis { conn: ctx.conn, descriptor, producer: &producer,
                    captured: &captured_header.revision },
                emitted: Cell::new(self.emitted), identical_joins: Cell::new(self.identical_joins),
                identity_kind: descriptor.producer.identity_kind.as_str(),
                descriptor_sha256: parts.descriptor_sha256,
            };
            let result = in_transaction(ctx.conn, || self.unit(&join, shared));
            self.emitted = join.emitted.get();
            ctx.check_output()?;
            stage_budget.check()?;
            result
        })();
        self.rows = ctx.rows.get();
        self.ledger = ctx.ledger.into_inner();
        *budget = ctx.budget.into_inner();
        let mut measured = ctx.counters.into_inner();
        measured.join_reservation_high_water_bytes = ctx.live.high_water();
        measured.staged_row_attempts = self.ledger.iter().map(|(k,v)| ((*k).to_owned(), *v)).collect();
        counters.absorb_join(&measured);
        if result.is_err() { self.phase = Phase::Failed; }
        result
    }
}

impl JoinContinuation {
    fn document(&self, join: &StagedJoin<'_>, rev: u8) -> Result<Option<(i64, native::DocumentKey)>> {
        Ok(join.conn().query_row(
            "SELECT ord,language,path FROM nat_docs WHERE rev=?1 AND ord>?2 ORDER BY ord LIMIT 1",
            params![rev, self.last], |row| Ok((row.get(0)?, native::DocumentKey {
                source_set_id: if rev == 0 { join.captured.source_set_id() }
                    else { join.destination.source_set_id() }.to_owned(),
                language: row.get(1)?, path: row.get(2)?,
            })),
        ).optional()?)
    }

    fn unit(&mut self, join: &StagedJoin<'_>, shared: bool) -> Result<Option<JoinedBatch>> {
        let conn = join.conn();
        let captured_global = || streaming::CompletedGlobal::new(join.ctx, 0, false,
            join.captured.source_set_id());
        match self.phase {
            Phase::Start => {
                // Invalidate any old seal before clearing its output, then delete bounded
                // pages. The stage may still contain the original captured-witness join.
                self.attempt = NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed) as i64;
                conn.execute("INSERT OR REPLACE INTO join_state VALUES(1,?1,?2,?3)",
                    params![self.destination.index_generation.to_string(),
                        self.destination.index_revision as i64,self.attempt])?;
                conn.execute_batch("CREATE TABLE IF NOT EXISTS continuation_symbol_keys(artifact INTEGER NOT NULL,key TEXT NOT NULL,symbol TEXT,scope TEXT NOT NULL,PRIMARY KEY(artifact,key));
                    CREATE INDEX IF NOT EXISTS continuation_symbol_buckets ON continuation_symbol_keys(artifact,symbol,scope);
                    CREATE TABLE IF NOT EXISTS continuation_closure_documents(language TEXT NOT NULL,path TEXT NOT NULL,PRIMARY KEY(language,path));")?;
                self.transition(Phase::Reset);
            }
            Phase::Reset => {
                let tables: Vec<&str> = JOIN_TABLES.iter().copied().chain([
                    "continuation_symbol_keys","continuation_closure_documents"
                ]).collect();
                if let Some(table) = tables.get(self.reset_table) {
                    let key = match *table {
                        "nat_declarations" => "rev,doc_ord,syntax_id,n",
                        "g_definitions" => "side,symbol,local_doc,owner_doc,syntax_id",
                        _ => "rowid",
                    };
                    let changed = conn.execute(&format!(
                        "DELETE FROM {table} WHERE ({key}) IN (SELECT {key} FROM {table} LIMIT 128)"),[])?;
                    if changed == 0 { self.reset_table += 1; }
                } else {
                    // Empty-table index creation does no whole-corpus work.
                    for table in &OUTPUT_TABLES[1..] {
                        conn.execute_batch(&format!("CREATE INDEX IF NOT EXISTS continuation_{table}_doc ON {table}(doc)"))?;
                    }
                    conn.execute_batch("CREATE INDEX IF NOT EXISTS continuation_coverage_key ON out_coverage(language,path)")?;
                    self.transition(Phase::ClearDestinationInventory);
                }
            }
            Phase::ClearDestinationInventory => {
                let deleted = conn.execute("DELETE FROM nat_docs WHERE rev=1 AND ord IN \
                    (SELECT ord FROM nat_docs WHERE rev=1 ORDER BY ord LIMIT 128)",[])?;
                if deleted == 0 {
                    conn.execute("DELETE FROM nat_done WHERE rev=1",[])?;
                    self.transition(Phase::Seed);
                }
            }
            Phase::Seed => {
                let page: Vec<i64> = conn.prepare_cached(
                    "SELECT ord FROM nat_docs WHERE rev=0 AND ord>?1 ORDER BY ord LIMIT 128")?
                    .query_map([self.last],|row|row.get(0))?.collect::<rusqlite::Result<_>>()?;
                join.ctx.reserve_rows(RETAINED_SEED,page.len() as u64)?;
                if let Some(last) = page.last() { self.last = *last; }
                else { self.transition(Phase::CapturedInventory); }
            }
            Phase::CapturedInventory | Phase::DestinationInventory => {
                let rev = u8::from(self.phase == Phase::DestinationInventory);
                let side = if rev == 0 { &join.captured } else { &join.destination };
                if exists(conn, "SELECT 1 FROM nat_done WHERE rev=?1", &[&rev])? {
                    self.transition(if shared || rev == 1 { Phase::CapturedDiscovery }
                        else { Phase::DestinationInventory });
                    if shared { self.manifest = Some(SourceManifestHasher::new(side.source_set_id())); }
                } else {
                    let page = side.reader.inventory_page(self.last, INVENTORY_PAGE_DOCUMENTS)?;
                    for (ord, document) in &page {
                        join.ctx.check_cancel()?;
                        join.ctx.row("nat_docs")?;
                        conn.prepare_cached("INSERT INTO nat_docs VALUES(?1,?2,?3,?4,?5,?6,NULL)")?
                            .execute(params![rev,ord,document.key.language,document.key.path,
                                document.content_hash,document.byte_length as i64])?;
                        self.last = *ord;
                    }
                    if page.is_empty() {
                        conn.execute("INSERT INTO nat_done VALUES(?1)", [rev])?;
                        self.transition(if shared || rev == 1 { Phase::CapturedDiscovery }
                            else { Phase::DestinationInventory });
                        self.manifest = Some(SourceManifestHasher::new(join.destination.source_set_id()));
                    }
                }
            }
            Phase::CapturedDiscovery | Phase::DestinationDiscovery => {
                let captured = self.phase == Phase::CapturedDiscovery;
                let side = if captured { &join.captured } else { &join.filtered };
                if let Some((ord, document)) = self.document(join, side.rev)? {
                    streaming::discover_document(side, join.admitted_encoding,
                        if !captured { streaming::TargetStatus::OwnAndCaptured }
                        else if shared { streaming::TargetStatus::Own }
                        else { streaming::TargetStatus::None }, &document)?;
                    self.last = ord;
                } else if captured {
                    self.identical_joins = shared;
                    self.transition(Phase::ClaimInventory);
                } else { self.transition(Phase::DestinationTargets); }
            }
            Phase::ClaimInventory => {
                let next: Option<(i64,String,String)> = conn.query_row(
                    "SELECT ord,language,path FROM sources WHERE ord>?1 ORDER BY ord LIMIT 1",
                    [self.last], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                ).optional()?;
                if let Some((ord,language,path)) = next {
                    anyhow::ensure!(!exists(conn,"SELECT 1 FROM claims WHERE source_ord=?1 LIMIT 1", &[&ord])?
                        || exists(conn,"SELECT 1 FROM nat_docs WHERE rev=0 AND language=?1 AND path=?2",
                            &[&language,&path])?, "missing admitted claim document");
                    self.last = ord;
                } else { self.transition(Phase::Eligibility); }
            }
            Phase::Eligibility => {
                if let Some((ord, document)) = self.document(join, 0)? {
                    let (flags,joined) = join.eligibility_document(&document, &captured_global()?)?;
                    if joined.is_some() && (!flags.eligible || flags.failed) {
                        self.identical_joins = false;
                    }
                    self.last = ord;
                } else {
                    self.transition(if shared { Phase::Settlement } else { Phase::DestinationDiscovery });
                }
            }
            Phase::DestinationTargets | Phase::CapturedTargets => {
                let next: Option<(i64,String,String)> = conn.query_row(
                    "SELECT ord,language,path FROM sources WHERE ord>?1 ORDER BY ord LIMIT 1",
                    [self.last], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                ).optional()?;
                if let Some((ord,language,path)) = next {
                    let document = native::DocumentKey {
                        source_set_id: join.captured.source_set_id().to_owned(),language,path,
                    };
                    streaming::remaining_document_status(&join.captured, &join.destination, ord,
                        &document, self.phase == Phase::CapturedTargets)?;
                    self.last = ord;
                } else { self.transition(if self.phase == Phase::DestinationTargets {
                    Phase::CapturedTargets
                } else { Phase::Settlement }); }
            }
            Phase::Settlement => {
                // The manifest is a separate bounded inventory pass. Never rescan the whole
                // inventory for every settlement document.
                if self.manifest_hash.is_empty() {
                    let mut statement = conn.prepare_cached(
                        "SELECT ord,language,path,content_hash FROM nat_docs WHERE rev=?1 AND ord>?2 ORDER BY ord LIMIT 128")?;
                    let page = statement.query_map(params![join.destination.rev,self.last], |row| {
                        Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?))
                    })?.collect::<rusqlite::Result<Vec<_>>>()?;
                    let manifest = self.manifest.get_or_insert_with(|| SourceManifestHasher::new(join.destination.source_set_id()));
                    for (ord,language,path,hash) in &page {
                        manifest.push(language,path,hash); self.last = *ord;
                    }
                    if page.is_empty() {
                        self.manifest_hash = self.manifest.take().expect("manifest initialized").finish();
                        self.last = -1;
                    }
                } else if let Some((ord, document)) = self.document(join, join.destination.rev)? {
                    let captured = captured_global()?;
                    let destination = if shared { captured.eligible_view()? } else {
                        streaming::CompletedGlobal::new(join.ctx,1,false,join.destination.source_set_id())?
                    };
                    let (flags,excluded) = join.document_flags(&document)?;
                    let outcome = if flags.eligible && !flags.failed {
                        join.settle_document(&document,&captured,&destination,&self.manifest_hash,None)?
                    } else { settle::DocumentOutcome::default() };
                    join.document_coverage(&document,flags,excluded.as_deref(),&outcome,&mut self.tallies)?;
                    self.last = ord;
                } else {
                    #[cfg(feature = "test-support")]
                    crate::semantic_batch::testing::run_before_closure_hook(conn);
                    self.transition(Phase::Closure);
                }
            }
            Phase::Closure => {
                if let Some((ord,document)) = self.document(join,join.destination.rev)? {
                    self.closure_document(join,&document)?;
                    self.last = ord;
                } else {
                    self.transition(Phase::OutputClosure);
                }
            }
            Phase::OutputClosure => {
                let next: Option<(i64,String,String)> = conn.query_row(
                    "SELECT ord,language,path FROM out_coverage WHERE ord>?1 ORDER BY ord LIMIT 1",
                    [self.last], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                ).optional()?;
                if let Some((ord,language,path)) = next {
                    anyhow::ensure!(exists(conn,
                        "SELECT 1 FROM continuation_closure_documents WHERE language=?1 AND path=?2",
                        &[&language,&path])?, "coverage on an unselected document");
                    anyhow::ensure!(!exists(conn,
                        "SELECT 1 FROM out_coverage WHERE language=?1 AND path=?2 AND ord<>?3 LIMIT 1",
                        &[&language,&path,&ord])?, "duplicate coverage row");
                    self.last = ord;
                } else { self.transition(Phase::OutputRows); }
            }
            Phase::OutputRows => {
                if let Some(table) = OUTPUT_TABLES.get(self.output_table) {
                    let page = conn.prepare_cached(&format!(
                        "SELECT ord,doc FROM {table} WHERE ord>?1 ORDER BY ord LIMIT 128"))?
                        .query_map([self.last], |row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    for (ord,doc) in &page {
                        join.ctx.check_cancel()?;
                        anyhow::ensure!(exists(conn,"SELECT 1 FROM out_coverage WHERE ord=?1", &[doc])?,
                            "semantic row without coverage");
                        self.last = *ord;
                    }
                    if page.is_empty() { self.output_table += 1; self.last = -1; }
                } else { self.transition(Phase::Report); }
            }
            Phase::Report => {
                let report = join.report(std::mem::take(&mut self.tallies))?;
                self.transition(Phase::Done);
                return Ok(Some(JoinedBatch::seal(
                    self.captured_pin.expect("captured identity initialized"),self.destination,
                    join.destination_id().to_owned(),join.descriptor_sha256.to_owned(),self.attempt,report,
                )));
            }
            Phase::Done | Phase::Failed => anyhow::bail!("staged continuation is not active"),
        }
        Ok(None)
    }
}

impl JoinContinuation {
    fn closure_document(&self, join: &StagedJoin<'_>, document: &native::DocumentKey) -> Result<()> {
        let conn = join.conn();
        let coverage: Option<(i64,String,Option<String>)> = conn.query_row(
            "SELECT ord,body,provenance FROM out_coverage WHERE language=?1 AND path=?2",
            params![document.language,document.path], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional()?;
        let requested = join.producer.languages.contains(&language_of(&document.language)?);
        anyhow::ensure!(coverage.is_some() == requested,
            "coverage count differs from the requested-language source count");
        let Some((doc,body,provenance)) = coverage else { return Ok(()); };
        conn.execute("INSERT INTO continuation_closure_documents VALUES(?1,?2)",
            params![document.language,document.path])?;
        let state: semantic::Coverage = serde_json::from_str(&body)?;
        if let Some(provenance) = provenance.as_deref() {
            let provenance: serde_json::Value = serde_json::from_str(provenance)?;
            anyhow::ensure!(exists(conn,
                "SELECT 1 FROM nat_docs WHERE rev=0 AND language=?1 AND path=?2 AND content_hash=?3",
                &[&document.language,&document.path,&provenance["contentHash"].as_str()])?
                && provenance["document"]["language"].as_str() == Some(document.language.as_str())
                && provenance["document"]["path"].as_str() == Some(document.path.as_str()),
                "captured proof document hash mismatch");
        }
        for table in &OUTPUT_TABLES[2..] {
            anyhow::ensure!(!exists(conn,&format!(
                "SELECT 1 FROM {table} o WHERE o.doc=?1 AND NOT EXISTS \
                 (SELECT 1 FROM out_provenance p WHERE p.id=o.provenance_id AND p.doc=o.doc) LIMIT 1"),
                &[&doc])?, "semantic row without provenance");
        }
        anyhow::ensure!(!exists(conn,
            "SELECT 1 FROM out_provenance o LEFT JOIN claims c ON c.claim_id=o.claim_id \
             LEFT JOIN sources s ON s.ord=c.source_ord WHERE o.doc=?1 AND \
             (s.ord IS NULL OR s.language<>?2 OR s.path<>?3 OR o.kind NOT IN (0,1,2,3)) LIMIT 1",
            &[&doc,&document.language,&document.path])?, "derived provenance without staged claim");
        if provenance.is_none() || !matches!(state.state,
            semantic::CoverageState::Complete | semantic::CoverageState::Partial) {
            anyhow::ensure!(!exists(conn,"SELECT 1 FROM out_provenance WHERE doc=?1 LIMIT 1", &[&doc])?,
                "proof on non-fact-bearing coverage");
        }
        for table in ["out_declaration_bindings","out_call_bindings"] {
            anyhow::ensure!(!exists(conn,&format!(
                "SELECT 1 FROM {table} WHERE doc=?1 GROUP BY provenance_id, \
                 json_extract(body,'$.join.anchor') HAVING COUNT(*)>1 LIMIT 1"), &[&doc])?,
                "duplicate binding fact");
        }
        anyhow::ensure!(!exists(conn,
            "SELECT 1 FROM out_type_relationships WHERE doc=?1 GROUP BY provenance_id, \
             json_extract(body,'$.kind'),json_extract(body,'$.source'),json_extract(body,'$.target') \
             HAVING COUNT(*)>1 LIMIT 1", &[&doc])?, "duplicate relationship fact");
        let mut symbols = conn.prepare_cached(
            "SELECT c.artifact,json_extract(s.body,'$.key'),json_extract(s.body,'$.key.symbol'), \
             json_extract(s.body,'$.key.scope') FROM out_symbols s JOIN out_provenance p \
             ON p.id=s.provenance_id JOIN claims c ON c.claim_id=p.claim_id WHERE s.doc=?1 ORDER BY s.ord")?;
        let mut rows = symbols.query([doc])?;
        while let Some(row) = rows.next()? {
            join.ctx.check_cancel()?;
            let artifact: i64 = row.get(0)?;
            let key: String = row.get(1)?;
            let symbol: Option<String> = row.get(2)?;
            let scope: String = row.get(3)?;
            let conflict = exists(conn,
                "SELECT 1 FROM continuation_symbol_keys WHERE artifact=?1 AND key=?2", &[&artifact,&key])?
                || matches!(scope.as_str(),"global"|"target") && exists(conn,
                    "SELECT 1 FROM continuation_symbol_keys WHERE artifact=?1 AND symbol=?2 \
                     AND scope IN ('global','target') AND (scope='global' OR ?3='global') LIMIT 1",
                    &[&artifact,&symbol,&scope])?;
            if conflict { return Err(ScipCaptureError::InvalidArtifact(DUPLICATE_SYMBOL_KEY.into()).into()); }
            join.ctx.row("continuation_symbol_keys")?;
            conn.prepare_cached("INSERT INTO continuation_symbol_keys VALUES(?1,?2,?3,?4)")?
                .execute(params![artifact,key,symbol,scope])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn output_reset_keeps_the_next_page_on_disk() {
        let parent = tempfile::tempdir().unwrap();
        let mut stage = crate::semantic_batch::staging::Stage::create(parent.path(),"cursor","reset").unwrap();
        {
            let conn = stage.conn().unwrap();
            for ord in 0..129 {
                conn.execute("INSERT INTO out_coverage VALUES(?1,'java',?2,'{}',NULL)",
                    params![ord,format!("{ord}.java")]).unwrap();
            }
            let deleted = conn.execute("DELETE FROM out_coverage WHERE rowid IN \
                (SELECT rowid FROM out_coverage LIMIT 128)",[]).unwrap();
            assert_eq!(deleted,128);
        }
        stage.release_connection();
        let left: i64 = stage.conn().unwrap().query_row("SELECT ord FROM out_coverage",[],|row|row.get(0)).unwrap();
        assert_eq!(left,128);
    }

    #[test]
    fn phase_transition_resets_only_document_cursor() {
        let mut cursor = JoinContinuation::new(IndexPin {
            index_generation: uuid::Uuid::nil(), index_revision: 1,
        }, BatchLimits::PRODUCTION, DestinationMutations::default());
        cursor.last = 7;
        cursor.rows = 11;
        cursor.emitted = [3; 7];
        cursor.transition(Phase::Eligibility);
        assert_eq!(cursor.phase,Phase::Eligibility);
        assert_eq!(cursor.last,-1);
        assert_eq!(cursor.rows,11);
        assert_eq!(cursor.emitted,[3; 7]);
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn yielded_join_matches_full_and_reopens_every_snapshot() {
        use crate::{
            indexer::{IndexOptions,index_workspace_bundle},
            semantic_batch::{StageInput,StageOwner,stage_batch,testing},
        };
        use protobuf::Message;
        use std::sync::{Arc,atomic::AtomicBool};
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        for (name,text) in [("A.java","class A { void f() {} }"),("B.java","class B { void g() {} }")] {
            std::fs::write(root.join(name),text).unwrap();
        }
        let store = Store::open_for_tests(state.path(),&root).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph,native,capture) = index_workspace_bundle(
            &IndexOptions::new(root.clone()),store.root_id(),&cancel,|_| {},
        ).unwrap();
        let leader = store.publication_for_tests().unwrap();
        let pin = store.publish_native(&graph,&capture,&native,&leader,
            store.index_baseline().unwrap(),&cancel).unwrap();
        drop(leader);
        let mut index = scip::types::Index::new();
        for path in ["A.java","B.java"] {
            let mut document = scip::types::Document::new();
            document.relative_path = path.into();
            document.language = "java".into();
            let symbol = format!("pkg {path}#method().");
            let mut occurrence = scip::types::Occurrence::new();
            occurrence.range = vec![0,15,16];
            occurrence.symbol = symbol.clone();
            occurrence.symbol_roles = 1;
            document.occurrences.push(occurrence);
            let mut info = scip::types::SymbolInformation::new();
            info.symbol = symbol;
            info.kind = protobuf::EnumOrUnknown::from_i32(26);
            document.symbols.push(info);
            index.documents.push(document);
        }
        let artifact = index.write_to_bytes().unwrap();
        let (descriptor,pages) = testing::single_artifact_batch(&capture,"scip-java","java",&artifact).unwrap();
        let batch_dir = root.join(".trellis-semantic-batch-test");
        std::fs::create_dir(&batch_dir).unwrap();
        std::fs::write(batch_dir.join("source-00000.json"),&pages[0]).unwrap();
        std::fs::write(batch_dir.join("coverage-00000.json"),&pages[1]).unwrap();
        std::fs::write(batch_dir.join("artifact.scip"),artifact).unwrap();
        let descriptor_path = batch_dir.join("descriptor.json");
        std::fs::write(&descriptor_path,&descriptor).unwrap();
        let limits = BatchLimits::PRODUCTION;
        let mut counters = BatchCounters::default();
        let mut input = StageInput::external(&root).unwrap();
        input.input_root = batch_dir;
        let mut admitted = stage_batch(&store,&input,
            StageOwner {request_id:"continuation-test",incarnation:"claim-1"},
            &descriptor_path,&crate::scip_raw::sha(&descriptor),&limits,&mut counters,&cancel).unwrap();
        let mut full_budget = LookupBudget::new(&limits);
        let full = join_staged(&mut admitted,&store,pin,&limits,&mut full_budget,&mut counters,
            DestinationMutations::default(),&cancel).unwrap();
        let expected = testing::staged_publication(&mut admitted).unwrap();
        let mut cursor = JoinContinuation::new(pin,limits.clone(),DestinationMutations::default());
        let mut budget = LookupBudget::new(&limits);
        let mut completed = None;
        let mut phases = BTreeSet::new();
        for _ in 0..256 {
            phases.insert(format!("{:?}",cursor.phase));
            let next = store.with_native_reader(pin,|reader| {
                cursor.advance(&mut admitted,reader,reader,&mut budget,&mut counters,&cancel)
            }).unwrap();
            admitted.stage_mut().release_connection();
            assert!(!admitted.stage().has_connection());
            if let Some(joined) = next { completed = Some(joined); break; }
        }
        let completed = completed.expect("normal two-document join completes in bounded units");
        assert!(phases.contains("CapturedDiscovery") && phases.contains("Eligibility")
            && phases.contains("Settlement") && phases.contains("Closure"));
        assert_eq!(completed.report(),full.report());
        assert_eq!(serde_json::to_value(testing::staged_publication(&mut admitted).unwrap()).unwrap(),
            serde_json::to_value(expected).unwrap());
        assert!(completed.attempt() > full.attempt());

        // A later native revision rejoins the same admitted claims in separate units.
        std::fs::write(root.join("B.java"),"class B { void renamed() {} }").unwrap();
        let (graph,native,capture) = index_workspace_bundle(
            &IndexOptions::new(root.clone()),store.root_id(),&cancel,|_| {},
        ).unwrap();
        let leader = store.publication_for_tests().unwrap();
        let destination = store.publish_native(&graph,&capture,&native,&leader,pin,&cancel).unwrap();
        drop(leader);
        let mut budget = LookupBudget::new(&limits);
        let full = join_staged(&mut admitted,&store,destination,&limits,&mut budget,&mut counters,
            DestinationMutations::default(),&cancel).unwrap();
        let expected = testing::staged_publication(&mut admitted).unwrap();
        let mut cursor = JoinContinuation::new(destination,limits.clone(),DestinationMutations::default());
        let mut budget = LookupBudget::new(&limits);
        let mut result = None;
        for _ in 0..256 {
            let next = store.with_native_reader(pin,|captured| {
                store.with_native_reader(destination,|destination| {
                    cursor.advance(&mut admitted,captured,destination,&mut budget,&mut counters,&cancel)
                })
            }).unwrap();
            admitted.stage_mut().release_connection();
            if next.is_some() { result = next; break; }
        }
        assert_eq!(result.expect("changed destination completes").report(),full.report());
        assert_eq!(serde_json::to_value(testing::staged_publication(&mut admitted).unwrap()).unwrap(),
            serde_json::to_value(expected).unwrap());
        let stage_path = admitted.stage().dir().to_owned();
        let mut cursor = JoinContinuation::new(destination,limits,DestinationMutations::default());
        cancel.store(true,Ordering::Relaxed);
        let error = store.with_native_reader(pin,|captured| {
            store.with_native_reader(destination,|destination| {
                cursor.advance(&mut admitted,captured,destination,&mut budget,&mut counters,&cancel)
            })
        }).unwrap_err();
        assert!(matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::Cancelled)));
        assert_eq!(cursor.phase,Phase::Failed);
        drop(admitted);
        assert!(!stage_path.exists());
    }
}
