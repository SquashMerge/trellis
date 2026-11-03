//! Native-readable publication and immutable origin-qualified semantic selections.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingSemanticRejoin {
    pub(crate) target_pin: IndexPin,
    native_revision_id: String,
    selection_identity: String,
}

pub(super) struct DocumentSemanticOrigin {
    pub origin_key: String,
    pub origin_pin: IndexPin,
    pub native_revision_id: String,
    pub source_eligible: bool,
    pub catching_up: bool,
    pub tuple_id: Option<i64>,
}

fn pin_for_key(db: &Connection, selected: &ReadRevision, key: &str) -> Result<IndexPin> {
    let revision: i64 = db.query_row(
        "SELECT published_index_revision FROM native_revisions WHERE id=?1",
        [key],
        |r| r.get(0),
    )?;
    Ok(IndexPin {
        index_generation: selected.pin.index_generation,
        index_revision: u64::try_from(revision)?,
    })
}

pub(super) fn semantic_document_origin(
    db: &Connection,
    selected: &ReadRevision,
    producer: &str,
    language: &str,
    path: &str,
) -> Result<DocumentSemanticOrigin> {
    let inherited: Option<(String, Option<i64>, bool, bool)> = db.query_row(
        "SELECT o.origin_revision_id,d.origin_tuple_id,d.source_eligible,d.targets_complete FROM semantic_document_origins d JOIN semantic_native_origins o ON o.revision_id=d.revision_id AND o.producer_id=d.producer_id WHERE d.revision_id=?1 AND d.producer_id=?2 AND d.language=?3 AND d.path=?4",
        params![selected.key,producer,language,path], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let (origin_key, tuple_id, source_eligible, _targets_complete, catching_up) = if let Some((
        key,
        tuple,
        source,
        targets,
    )) = inherited
    {
        let pending:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_revision_raw_selections WHERE revision_id=?1 AND producer_id=?2)",params![selected.key,producer],|r|r.get(0))?;
        (
            key,
            tuple,
            source,
            targets,
            pending && (!source || !targets),
        )
    } else {
        let tuple: Option<(i64,bool)> = db.query_row("SELECT tuple_id,selected=1 AND state IN ('complete','partial') FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",params![selected.key,producer,language,path],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        (
            selected.key.clone(),
            tuple.map(|t| t.0),
            tuple.is_some_and(|t| t.1),
            true,
            false,
        )
    };
    let native_revision_id = db.query_row(
        "SELECT native_revision_id FROM native_revisions WHERE id=?1",
        [&origin_key],
        |r| r.get(0),
    )?;
    Ok(DocumentSemanticOrigin {
        origin_pin: pin_for_key(db, selected, &origin_key)?,
        origin_key,
        native_revision_id,
        source_eligible,
        catching_up,
        tuple_id,
    })
}

pub(super) fn semantic_ineligible_provenance(
    db: &Connection,
    selected: &ReadRevision,
    tuple: i64,
) -> Result<BTreeSet<String>> {
    Ok(db.prepare("SELECT provenance_id FROM semantic_ineligible_proofs WHERE revision_id=?1 AND origin_tuple_id=?2")?.query_map(params![selected.key,tuple],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// Retained evidence keeps its original identity; a different native revision
/// can only weaken its comparison freshness, never promote the captured proof.
pub(super) fn semantic_comparison_freshness(
    stored: crate::semantic_evidence::Freshness,
    proof_revision: &str,
    selected_native_revision: &str,
) -> crate::semantic_evidence::Freshness {
    use crate::semantic_evidence::Freshness;
    if stored == Freshness::Fresh && proof_revision != selected_native_revision {
        Freshness::PossiblyStale
    } else {
        stored
    }
}

pub(super) fn semantic_comparison_revision(
    db: &Connection,
    selected: &ReadRevision,
) -> Result<String> {
    Ok(db.query_row(
        "SELECT native_revision_id FROM native_revisions WHERE id=?1",
        [&selected.key],
        |r| r.get(0),
    )?)
}

pub(super) fn semantic_document_status(
    db: &Connection,
    selected: &ReadRevision,
    producer: &str,
    language: &str,
    path: &str,
) -> Result<DocumentSemanticStatus> {
    let origin = semantic_document_origin(db, selected, producer, language, path)?;
    Ok(DocumentSemanticStatus {
        state: if origin.catching_up {
            SemanticAvailability::CatchingUp
        } else if origin.source_eligible && origin.tuple_id.is_some() {
            SemanticAvailability::Fresh
        } else {
            SemanticAvailability::Unavailable
        },
        producer_id: Some(producer.to_owned()),
        semantic_origin: origin.tuple_id.map(|_| SemanticOrigin {
            pin: origin.origin_pin,
            revision_id: origin.native_revision_id,
        }),
        reason: origin.catching_up.then(|| {
            if origin.source_eligible {
                "semantic targets catching up"
            } else {
                "semantic source catching up"
            }
            .to_owned()
        }),
    })
}

pub(super) fn document_status_for_path(
    db: &Connection,
    selected: &ReadRevision,
    path: &str,
) -> Result<DocumentSemanticStatus> {
    let language: Option<String> = db
        .query_row(
            "SELECT language FROM revision_documents WHERE revision_id=?1 AND path=?2",
            params![selected.key, path],
            |r| r.get(0),
        )
        .optional()?;
    let language = language.context("not_found: selected document missing")?;
    let defaults = semantic_display::default_producers(db, &selected.key)?;
    match defaults.producer(language.as_str()) {
        Some(producer) => semantic_document_status(db, selected, producer, &language, path),
        None => Ok(DocumentSemanticStatus::default()),
    }
}

fn selection_identity(db: &Connection, key: &str) -> Result<String> {
    let rows=db.prepare("SELECT producer_id,batch_id,captured_pin FROM semantic_revision_raw_selections WHERE revision_id=?1 ORDER BY producer_id")?.query_map([key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let requests=db.prepare("SELECT p.id,p.version,p.executable_hash,l.language,l.ordinal FROM semantic_requested_producers p LEFT JOIN semantic_requested_producer_languages l ON l.revision_id=p.revision_id AND l.id=p.id WHERE p.revision_id=?1 ORDER BY p.id,l.ordinal")?.query_map([key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<i64>>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let origins=db.prepare("SELECT producer_id,origin_revision_id FROM semantic_native_origins WHERE revision_id=?1 ORDER BY producer_id")?.query_map([key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(crate::native_ids::digest(
        b"trellis.pending-semantic.v1\0",
        &serde_json::to_vec(&(rows, requests, origins))?,
    ))
}

pub(super) fn write_native_first_selection(
    db: &Connection,
    prior: Option<IndexPin>,
    next: IndexPin,
    changed: Option<&BTreeSet<String>>,
    cancel: &CancelFlag,
) -> Result<()> {
    let Some(prior) = prior else { return Ok(()) };
    let previous = semantic_pin_key(prior);
    let key = semantic_pin_key(next);
    let same_set:bool=db.query_row("SELECT old.source_set_id=new.source_set_id FROM native_revisions old,native_revisions new WHERE old.id=?1 AND new.id=?2",params![previous,key],|r|r.get(0))?;
    if !same_set {
        return Ok(());
    }
    let orphaned_requested_language:bool=db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_requested_producer_languages l LEFT JOIN semantic_requested_producers p ON p.revision_id=l.revision_id AND p.id=l.id WHERE l.revision_id=?1 AND p.id IS NULL)",
        [&previous],|row|row.get(0),
    )?;
    selected_ensure!(
        !orphaned_requested_language,
        "incompatible_index: selected requested producer missing"
    );
    let unrequested_raw_selection:bool=db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s LEFT JOIN semantic_requested_producers p ON p.revision_id=s.revision_id AND p.id=s.producer_id WHERE s.revision_id=?1 AND p.id IS NULL)",
        [&previous],|row|row.get(0),
    )?;
    selected_ensure!(
        !unrequested_raw_selection,
        "incompatible_index: selected requested producer missing"
    );
    for (table, columns) in [
        ("semantic_revision_producers", "id,version"),
        ("semantic_requested_producers", "id,version,executable_hash"),
        (
            "semantic_requested_producer_languages",
            "id,language,ordinal",
        ),
        (
            "semantic_revision_raw_selections",
            "producer_id,batch_id,captured_pin",
        ),
        ("semantic_import_reports", "producer_id,report_json"),
    ] {
        check_cancel(cancel)?;
        db.execute(
            &format!("INSERT INTO {table} SELECT ?1,{columns} FROM {table} WHERE revision_id=?2"),
            params![key, previous],
        )?;
    }
    db.execute("INSERT INTO semantic_native_origins SELECT ?1,p.id,COALESCE(o.origin_revision_id,?2) FROM semantic_revision_producers p LEFT JOIN semantic_native_origins o ON o.revision_id=p.revision_id AND o.producer_id=p.id WHERE p.revision_id=?2",params![key,previous])?;
    // Same selected vid authenticates bytes, native producer and extraction context.
    // A document already pending never becomes eligible just because a restore matches base.
    db.execute("INSERT INTO semantic_document_origins SELECT ?1,o.producer_id,d.language,d.path,c.tuple_id,COALESCE(c.selected=1 AND c.state IN ('complete','partial') AND base.vid=d.vid AND old.vid=d.vid AND COALESCE(inherited.source_eligible,1) AND EXISTS(SELECT 1 FROM native_revisions captured JOIN native_revisions current ON current.id=?1 WHERE captured.id=o.origin_revision_id AND captured.source_set_id=current.source_set_id AND captured.toolchain_hash=current.toolchain_hash AND captured.config_hash=current.config_hash AND captured.dependency_hash=current.dependency_hash),0),1 FROM semantic_native_origins o JOIN semantic_revision_producers producer ON producer.revision_id=o.revision_id AND producer.id=o.producer_id JOIN semantic_producer_languages language ON language.id=producer.id AND language.version=producer.version JOIN revision_documents d ON d.revision_id=o.revision_id AND d.language=language.language LEFT JOIN semantic_coverage c ON c.revision_id=o.origin_revision_id AND c.producer_id=o.producer_id AND c.language=d.language AND c.path=d.path LEFT JOIN revision_documents base ON base.revision_id=o.origin_revision_id AND base.language=d.language AND base.path=d.path LEFT JOIN revision_documents old ON old.revision_id=?2 AND old.language=d.language AND old.path=d.path LEFT JOIN semantic_document_origins inherited ON inherited.revision_id=?2 AND inherited.producer_id=o.producer_id AND inherited.language=d.language AND inherited.path=d.path WHERE o.revision_id=?1",params![key,previous])?;
    if let Some(changed) = changed {
        for path in changed {
            check_cancel(cancel)?;
            db.execute("UPDATE semantic_document_origins SET source_eligible=0 WHERE revision_id=?1 AND path=?2",params![key,path])?;
        }
    }
    write_target_eligibility(db, &key, cancel)?;
    let has_raw: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_revision_raw_selections WHERE revision_id=?1)",
        [&key],
        |r| r.get(0),
    )?;
    if has_raw {
        let native: String = db.query_row(
            "SELECT native_revision_id FROM native_revisions WHERE id=?1",
            [&key],
            |r| r.get(0),
        )?;
        db.execute(
            "INSERT INTO semantic_pending_rejoins VALUES(?1,?2,?3)",
            params![key, native, selection_identity(db, &key)?],
        )?;
    }
    Ok(())
}

fn write_target_eligibility(db: &Connection, key: &str, cancel: &CancelFlag) -> Result<()> {
    // Retained rows keep origin target revision IDs. A cross-version target is withheld,
    // not rewritten to a matching spelling or assumed from a shared stable ID.
    for (table, join, kind, syntax, language, path, tuple, proof) in [
        (
            "semantic_symbol_targets",
            "JOIN semantic_symbols s ON s.symbol_id=t.symbol_id",
            "t.kind",
            "t.internal_syntax_id",
            "t.internal_language",
            "t.internal_path",
            "s.tuple_id",
            "s.provenance_id",
        ),
        (
            "semantic_references",
            "",
            "t.declared_kind",
            "t.declared_internal_syntax_id",
            "t.declared_internal_language",
            "t.declared_internal_path",
            "t.tuple_id",
            "t.provenance_id",
        ),
        (
            "semantic_call_bindings",
            "",
            "t.declared_kind",
            "t.declared_internal_syntax_id",
            "t.declared_internal_language",
            "t.declared_internal_path",
            "t.tuple_id",
            "t.provenance_id",
        ),
        (
            "semantic_type_relationships",
            "",
            "t.source_kind",
            "t.source_internal_syntax_id",
            "t.source_internal_language",
            "t.source_internal_path",
            "t.tuple_id",
            "t.provenance_id",
        ),
        (
            "semantic_type_relationships",
            "",
            "t.target_kind",
            "t.target_internal_syntax_id",
            "t.target_internal_language",
            "t.target_internal_path",
            "t.tuple_id",
            "t.provenance_id",
        ),
        (
            "semantic_reference_candidates",
            "JOIN semantic_references s ON s.tuple_id=t.tuple_id AND s.reference_id=t.reference_id",
            "t.kind",
            "t.internal_syntax_id",
            "t.internal_language",
            "t.internal_path",
            "s.tuple_id",
            "s.provenance_id",
        ),
        (
            "semantic_call_candidates",
            "JOIN semantic_call_bindings s ON s.binding_id=t.binding_id",
            "t.kind",
            "t.internal_syntax_id",
            "t.internal_language",
            "t.internal_path",
            "s.tuple_id",
            "s.provenance_id",
        ),
        (
            "semantic_call_possible_dispatch",
            "JOIN semantic_call_bindings s ON s.binding_id=t.binding_id",
            "t.kind",
            "t.internal_syntax_id",
            "t.internal_language",
            "t.internal_path",
            "s.tuple_id",
            "s.provenance_id",
        ),
    ] {
        check_cancel(cancel)?;
        db.execute(&format!("INSERT OR IGNORE INTO semantic_ineligible_proofs SELECT ?1,{tuple},{proof} FROM {table} t {join} JOIN semantic_document_origins o ON o.revision_id=?1 AND o.origin_tuple_id={tuple} AND o.source_eligible=1 JOIN semantic_coverage c ON c.tuple_id=o.origin_tuple_id WHERE {kind}='internal' AND NOT EXISTS(SELECT 1 FROM revision_documents old JOIN revision_documents current ON current.revision_id=?1 AND current.language=old.language AND current.path=old.path AND current.vid=old.vid JOIN native_version_declarations decl ON decl.vid=current.vid AND decl.syntax_id={syntax} WHERE old.revision_id=c.revision_id AND old.language={language} AND old.path={path})"),[key])?;
    }
    db.execute("UPDATE semantic_document_origins SET targets_complete=0 WHERE revision_id=?1 AND EXISTS(SELECT 1 FROM semantic_ineligible_proofs p WHERE p.revision_id=?1 AND p.origin_tuple_id=semantic_document_origins.origin_tuple_id)",[key])?;
    Ok(())
}

impl Store {
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_semantic_rejoin_active_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        self.publication_before_semantic_hook.set(hook);
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn complete_pending_semantic_for_tests(
        &self,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<Option<IndexPin>> {
        match self.pending_semantic_rejoin_at_head()? {
            Some(pending) => self.continue_pending_semantic(&pending, leader, cancel, || Ok(())),
            None => Ok(None),
        }
    }

    pub(crate) fn pending_semantic_rejoin_at_head(&self) -> Result<Option<PendingSemanticRejoin>> {
        self.with_evidence(|db| {
            let selected = ReadRevision::current(db)?;
            pending_at(db, selected.pin)
        })
    }
    pub(crate) fn continue_pending_semantic<G>(
        &self,
        pending: &PendingSemanticRejoin,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
        final_cutoff: impl FnOnce() -> Result<G>,
    ) -> Result<Option<IndexPin>> {
        let result = (|| {
            check_cancel(cancel)?;
            leader.belongs_to(&self.identity)?;
            if self.pending_semantic_rejoin_at_head()?.as_ref() != Some(pending) {
                return Ok(None);
            }
            let (selected, native, sources) = {
                let mut db = self.cache()?;
                let read_tx = storage_result(db.transaction())?;
                begin_read_snapshot(
                    &read_tx,
                    &self.roots.index_db(&self.identity),
                    Duration::from_secs(5),
                )?;
                let read = self.read_revision(&read_tx, Some(pending.target_pin))?;
                let context = SemanticProgress(cancel);
                let _progress = install_semantic_progress(&read_tx, &context);
                let selected = self.selected_native_inputs_from_db_cancellable(
                    &read_tx,
                    &read,
                    pending.target_pin,
                    None,
                    Some(cancel),
                )?;
                let (native, sources) = self.retained_native_snapshot_from_db_cancellable(
                    &read_tx,
                    &read,
                    None,
                    Some(cancel),
                )?;
                (selected, native, sources)
            };
            check_cancel(cancel)?;
            let mut db = self.cache_write()?;
            let context = SemanticProgress(cancel);
            let mut readers_blocked = ReadersBlockedWatch::new_semantic(
                &self.publication_gate,
                native.revision.documents.len(),
                &db,
                serde_json::json!({
                    "targetPin": pending.target_pin,
                    "nativeRevisionId": pending.native_revision_id,
                    "plannedIndexRevision": pending.target_pin.index_revision + 1,
                }),
            );
            let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
            let _progress = install_semantic_progress(&tx, &context);
            if ReadRevision::current(&tx)?.pin != pending.target_pin
                || pending_at(&tx, pending.target_pin)?.as_ref() != Some(pending)
                || selection_identity(&tx, &semantic_pin_key(pending.target_pin))?
                    != pending.selection_identity
            {
                return Ok(None);
            }
            let current_native: String = tx.query_row(
                "SELECT native_revision_id FROM native_revisions WHERE id=?1",
                [semantic_pin_key(pending.target_pin)],
                |r| r.get(0),
            )?;
            ensure!(
                current_native == pending.native_revision_id,
                "revision conflict: semantic destination changed"
            );
            let next = IndexPin {
                index_generation: pending.target_pin.index_generation,
                index_revision: pending
                    .target_pin
                    .index_revision
                    .checked_add(1)
                    .filter(|n| *n <= 9_007_199_254_740_991)
                    .context("revision overflow")?,
            };
            let old = semantic_pin_key(pending.target_pin);
            let key = semantic_pin_key(next);
            clone_native_revision(&tx, &old, &key, next)?;
            #[cfg(feature = "test-support")]
            let mut observer = self.semantic_publication_observer.lock().unwrap().take();
            #[cfg(feature = "test-support")]
            if let Some(observer) = observer.as_mut() {
                observer(SemanticPublicationStage::BeforeSemantic, &tx);
            }
            write_semantic(
                &tx,
                &native,
                Some(pending.target_pin),
                next,
                None,
                &selected,
            )?;
            readers_blocked.check(&tx);
            #[cfg(feature = "test-support")]
            self.publication_before_semantic_hook.run();
            check_cancel(cancel)?;
            selected_native_rejoin(
                self,
                &tx,
                &selected,
                &native,
                &sources,
                next,
                cancel,
                None,
                Some(pending.target_pin),
                &mut |db| {
                    readers_blocked.check(db);
                    #[cfg(feature = "test-support")]
                    if let Some(observer) = observer.as_mut() {
                        observer(SemanticPublicationStage::AfterDocument, db);
                    }
                    Ok(())
                },
            )?;
            fill_semantic_coverage(&tx, &old, &key)?;
            carry_unrejoined_origins(&tx, &old, &key)?;
            tx.execute(
                "INSERT INTO semantic_import_reports(revision_id,producer_id,report_json)
                 SELECT ?1,r.producer_id,r.report_json FROM semantic_import_reports r
                 JOIN semantic_revision_raw_selections previous ON previous.revision_id=r.revision_id AND previous.producer_id=r.producer_id
                 JOIN semantic_revision_raw_selections next ON next.revision_id=?1 AND next.producer_id=previous.producer_id AND next.batch_id=previous.batch_id AND next.captured_pin=previous.captured_pin
                 WHERE r.revision_id=?2",
                params![key,old],
            )?;
            tx.execute(
                "INSERT INTO semantic_rejoin_completions VALUES(?1,?2,?3)",
                params![key, pending.native_revision_id, pending.selection_identity],
            )?;
            search::publish_head(&tx, &key)?;
            let published = self.retention_time()?;
            stamp_predecessor(&tx, &old, published.as_secs() as i64)?;
            tx.execute("UPDATE index_metadata SET index_revision=?1,indexed_at=?2 WHERE singleton=1 AND index_generation=?3 AND index_revision=?4",params![next.index_revision as i64,published.as_millis().to_string(),next.index_generation.to_string(),pending.target_pin.index_revision as i64])?;
            check_cancel(cancel)?;
            leader.verify()?;
            self.identity.verify()?;
            let _cutoff = final_cutoff()?;
            check_cancel(cancel)?;
            readers_blocked.check(&tx);
            storage_result(tx.commit())?;
            drop(readers_blocked);
            self.published_admission.invalidate();
            drop(_progress);
            drop(_cutoff);
            if let Err(error) = self.poc_checkpoint_after_semantic(leader, &db) {
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr(),
                    "trellis_poc_checkpoint_error committedPin={next:?} error={error:#}"
                );
            }
            drop(db);
            if std::env::var_os("TRELLIS_POC_INDEX_JOURNAL_MODE").is_some() {
                let path = self.roots.index_db(&self.identity);
                let files: BTreeMap<_, _> = ["", "-wal", "-shm", "-journal"]
                    .into_iter()
                    .map(|suffix| {
                        let name = format!("index.db{suffix}");
                        let bytes = std::fs::metadata(path.with_file_name(&name))
                            .ok()
                            .map(|m| m.len());
                        (name, bytes)
                    })
                    .collect();
                poc_journal_diagnostic(
                    &serde_json::json!({"event":"poc_semantic_writer_connection_closed","committedPin":next,"files":files,"lastConnectionCloseNotGuaranteed":true}),
                );
            }
            #[cfg(test)]
            if self
                .test_publish_post_commit_busy_once
                .swap(false, Ordering::AcqRel)
            {
                cancel.store(true, Ordering::Release);
                anyhow::bail!("storage_busy: SQLite lock contention");
            }
            Ok(Some(next))
        })();
        if result.is_err() {
            // Resolve a possibly committed transaction before interpreting cancellation.
            // The completion marker binds this publication to the exact frozen input set.
            let committed=self.with_evidence(|db| {
                let revision=pending.target_pin.index_revision.checked_add(1).context("revision overflow")?;
                let key=semantic_pin_key(IndexPin {index_generation:pending.target_pin.index_generation,index_revision:revision});
                let complete:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_rejoin_completions c JOIN native_revisions n ON n.id=c.revision_id WHERE c.revision_id=?1 AND c.selection_identity=?2 AND c.native_revision_id=?3 AND n.native_revision_id=?3)",params![key,pending.selection_identity,pending.native_revision_id],|r|r.get(0))?;
                Ok(complete.then_some(IndexPin {index_generation:pending.target_pin.index_generation,index_revision:revision}))
            })?;
            if committed.is_some() {
                return Ok(committed);
            }
            if cancel.load(Ordering::Acquire) {
                // All operation resources, progress handler and transaction have dropped.
                return Ok(None);
            }
        }
        result.map_err(typed_storage_full)
    }
}

fn pending_at(db: &Connection, pin: IndexPin) -> Result<Option<PendingSemanticRejoin>> {
    Ok(db.query_row("SELECT native_revision_id,selection_identity FROM semantic_pending_rejoins WHERE revision_id=?1",[semantic_pin_key(pin)],|r|Ok(PendingSemanticRejoin {target_pin:pin,native_revision_id:r.get(0)?,selection_identity:r.get(1)?})).optional()?)
}

fn carry_unrejoined_origins(db: &Connection, old: &str, key: &str) -> Result<()> {
    db.execute("INSERT INTO semantic_native_origins SELECT ?1,o.producer_id,o.origin_revision_id FROM semantic_native_origins o WHERE o.revision_id=?2 AND NOT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s WHERE s.revision_id=?2 AND s.producer_id=o.producer_id)",params![key,old])?;
    db.execute("INSERT INTO semantic_document_origins SELECT ?1,d.producer_id,d.language,d.path,d.origin_tuple_id,d.source_eligible,d.targets_complete FROM semantic_document_origins d JOIN semantic_native_origins o ON o.revision_id=?1 AND o.producer_id=d.producer_id WHERE d.revision_id=?2",params![key,old])?;
    db.execute("INSERT INTO semantic_ineligible_proofs SELECT ?1,p.origin_tuple_id,p.provenance_id FROM semantic_ineligible_proofs p JOIN semantic_document_origins d ON d.revision_id=?1 AND d.origin_tuple_id=p.origin_tuple_id WHERE p.revision_id=?2",params![key,old])?;
    Ok(())
}

fn clone_native_revision(db: &Connection, old: &str, key: &str, next: IndexPin) -> Result<()> {
    db.execute("INSERT INTO native_revisions SELECT ?1,source_set_id,toolchain_hash,config_hash,dependency_hash,native_revision_id,source_inventory,dependency_observations,retained_toolchain,retained_config,retained_dependencies,reconcile_options,class_warnings,class_truncated,graph_stats,graph_diagnostics,?2 FROM native_revisions WHERE id=?3",params![key,next.index_revision as i64,old])?;
    db.execute("INSERT INTO revision_documents SELECT ?1,language,path,vid,gid,cid,capture_stat,coverage_requested,coverage_selected,coverage_state,coverage_diagnostic,ordinal,class_complete FROM revision_documents WHERE revision_id=?2",params![key,old])?;
    db.execute("INSERT INTO revision_capture_inputs SELECT ?1,input_key,payload FROM revision_capture_inputs WHERE revision_id=?2",params![key,old])?;
    let (producer, binding) = revision_producer_binding(db, key)?;
    db.execute(
        "INSERT INTO revision_producer_bindings VALUES(?1,?2,?3)",
        params![key, producer, binding],
    )?;
    Ok(())
}

struct SemanticProgress<'a>(&'a CancelFlag);
unsafe extern "C" fn semantic_progress_callback(arg: *mut std::ffi::c_void) -> libc::c_int {
    // SAFETY: the scoped guard removes this callback before its context drops.
    let context = unsafe { &*(arg as *const SemanticProgress<'_>) };
    i32::from(context.0.load(Ordering::Acquire))
}
fn install_semantic_progress(
    db: &Connection,
    context: &SemanticProgress<'_>,
) -> MaintenanceProgressGuard {
    // SAFETY: callback and context live on this connection's thread until guard drop.
    let handle = unsafe { db.handle() };
    unsafe {
        rusqlite::ffi::sqlite3_progress_handler(
            handle,
            256,
            Some(semantic_progress_callback),
            (context as *const SemanticProgress<'_>).cast_mut().cast(),
        );
    }
    MaintenanceProgressGuard(handle)
}

pub(super) fn read_origin_semantic(
    db: &Connection,
    key: &str,
) -> Result<crate::semantic_evidence::SemanticSnapshot> {
    let inherited: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_document_origins WHERE revision_id=?1)",
        [key],
        |r| r.get(0),
    )?;
    if !inherited {
        return read_semantic_tuple(db, key, None, None, false);
    }
    let pin_revision: i64 = db.query_row(
        "SELECT published_index_revision FROM native_revisions WHERE id=?1",
        [key],
        |r| r.get(0),
    )?;
    let generation: String = db.query_row(
        "SELECT index_generation FROM index_metadata WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let selected = ReadRevision::new(
        IndexPin {
            index_generation: uuid::Uuid::parse_str(&generation)?,
            index_revision: u64::try_from(pin_revision)?,
        },
        key.to_owned(),
    );
    let selected_native_revision = semantic_comparison_revision(db, &selected)?;
    let mut result = read_semantic_tuple(db, key, None, None, false)?;
    let rows=db.prepare("SELECT producer_id,language,path FROM semantic_document_origins WHERE revision_id=?1 ORDER BY producer_id,language,path")?.query_map([key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (producer, language, path) in rows {
        let origin = semantic_document_origin(db, &selected, &producer, &language, &path)?;
        if !origin.source_eligible {
            continue;
        }
        let Some(tuple) = origin.tuple_id else {
            continue;
        };
        let mut inherited = read_semantic_tuple(db, &origin.origin_key, Some(tuple), None, false)?;
        let ineligible = semantic_ineligible_provenance(db, &selected, tuple)?;
        let eligible: BTreeSet<_> = inherited
            .provenance
            .iter()
            .filter(|p| !ineligible.contains(&p.id))
            .map(|p| p.id.clone())
            .collect();
        inherited.provenance.retain(|p| eligible.contains(&p.id));
        inherited
            .symbols
            .retain(|r| eligible.contains(&r.provenance_id));
        inherited
            .declaration_bindings
            .retain(|r| eligible.contains(&r.provenance_id));
        inherited
            .references
            .retain(|r| eligible.contains(&r.provenance_id));
        inherited
            .call_bindings
            .retain(|r| eligible.contains(&r.provenance_id));
        inherited
            .type_relationships
            .retain(|r| eligible.contains(&r.provenance_id));
        // Keep basis/proof IDs and target revision IDs authentic. The selected view
        // reports origin separately and comparison freshness remains conservative.
        for proof in &mut inherited.provenance {
            proof.freshness = semantic_comparison_freshness(
                proof.freshness,
                &proof.revision_id,
                &selected_native_revision,
            );
        }
        result.coverage.retain(|c| {
            !(c.producer_id == producer
                && c.language.as_str() == language
                && c.document_path == path)
        });
        result.coverage.extend(inherited.coverage);
        result.provenance.extend(inherited.provenance);
        result.symbols.extend(inherited.symbols);
        result
            .declaration_bindings
            .extend(inherited.declaration_bindings);
        result.references.extend(inherited.references);
        result.call_bindings.extend(inherited.call_bindings);
        result
            .type_relationships
            .extend(inherited.type_relationships);
    }
    Ok(result)
}

pub(super) fn carry_import_origins(
    db: &Connection,
    old: &str,
    key: &str,
    replaced: &str,
) -> Result<()> {
    for (table, columns) in [
        ("semantic_native_origins", "producer_id,origin_revision_id"),
        (
            "semantic_document_origins",
            "producer_id,language,path,origin_tuple_id,source_eligible,targets_complete",
        ),
    ] {
        db.execute(&format!("INSERT INTO {table} SELECT ?1,{columns} FROM {table} WHERE revision_id=?2 AND producer_id<>?3"),params![key,old,replaced])?;
    }
    db.execute("INSERT INTO semantic_ineligible_proofs SELECT ?1,p.origin_tuple_id,p.provenance_id FROM semantic_ineligible_proofs p JOIN semantic_coverage c ON c.tuple_id=p.origin_tuple_id WHERE p.revision_id=?2 AND c.producer_id<>?3",params![key,old,replaced])?;
    let remains: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_native_origins WHERE revision_id=?1)",
        [key],
        |r| r.get(0),
    )?;
    if remains {
        let native: String = db.query_row(
            "SELECT native_revision_id FROM native_revisions WHERE id=?1",
            [key],
            |r| r.get(0),
        )?;
        db.execute(
            "INSERT INTO semantic_pending_rejoins VALUES(?1,?2,?3)",
            params![key, native, selection_identity(db, key)?],
        )?;
    }
    Ok(())
}

#[cfg(all(test, feature = "test-support"))]
pub(crate) fn fixture_for_tests() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    crate::indexer::IndexOptions,
    IndexPin,
    IndexPin,
) {
    let (state, root, store) = super::selected_read_once_tests::published_store(2);
    let options = crate::indexer::IndexOptions::new(root.path().to_owned());
    let base = import_empty_batch_for_tests(&store, &options, "trellis-synthetic-scip");
    std::fs::write(root.path().join("f0.js"), "function f0(x) { return 42; }\n").unwrap();
    let pending = publish_native_for_tests(&store, &options);
    (state, root, store, options, base, pending)
}

#[cfg(all(test, feature = "test-support"))]
fn publish_native_for_tests(store: &Store, options: &crate::indexer::IndexOptions) -> IndexPin {
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) =
        crate::indexer::index_workspace_bundle(options, store.root_id(), &cancel, |_| {}).unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &store.writer().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap()
}

#[cfg(all(test, feature = "test-support"))]
fn import_empty_batch_for_tests(
    store: &Store,
    options: &crate::indexer::IndexOptions,
    producer: &str,
) -> IndexPin {
    use protobuf::{EnumOrUnknown, Message};
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = crate::capture::Capture::admit(options, &cancel, &|_| {}).unwrap();
    let pinned = crate::scip_admission::pinned_producer_by_id(producer).unwrap();
    let mut artifact = scip::types::Index::new();
    let mut metadata = scip::types::Metadata::new();
    metadata.project_root = crate::scip_admission::file_uri(capture.physical_root()).unwrap();
    metadata.text_document_encoding = EnumOrUnknown::from_i32(1);
    artifact.metadata = Some(metadata).into();
    for source in &capture.files {
        let mut document = scip::types::Document::new();
        document.relative_path = source.path.clone();
        document.language = source.language.clone();
        document.position_encoding = EnumOrUnknown::from_i32(i32::from(pinned.position_encoding));
        artifact.documents.push(document);
    }
    let bytes = artifact.write_to_bytes().unwrap();
    let (descriptor, pages) = crate::semantic_batch::testing::single_artifact_batch(
        &capture,
        producer,
        "javascript",
        &bytes,
    )
    .unwrap();
    let batch_relative = ".trellis-semantic-batch-fixture";
    let batch = capture.physical_root().join(batch_relative);
    let mut descriptor: serde_json::Value = serde_json::from_slice(&descriptor).unwrap();
    for family in ["pages", "artifacts"] {
        for entry in descriptor[family].as_array_mut().unwrap() {
            let name = entry["relativePath"].as_str().unwrap();
            entry["relativePath"] = format!("{batch_relative}/{name}").into();
        }
    }
    let descriptor = serde_json::to_vec(&descriptor).unwrap();
    std::fs::create_dir(&batch).unwrap();
    std::fs::write(batch.join("batch.json"), &descriptor).unwrap();
    for (name, page) in ["source-00000.json", "coverage-00000.json"]
        .into_iter()
        .zip(pages)
    {
        std::fs::write(batch.join(name), page).unwrap();
    }
    std::fs::write(batch.join("artifact.scip"), bytes).unwrap();
    let (mut admitted, joined, _) = crate::semantic_batch::testing::stage_and_join_admitted(
        store,
        capture.physical_root(),
        &batch.join("batch.json"),
        &crate::scip_raw::sha(&descriptor),
        &crate::semantic_batch::BatchLimits::default(),
        crate::semantic_import::DestinationMutations::default(),
        &cancel,
    )
    .unwrap();
    let pin = store
        .publish_semantic_batch(
            &joined,
            admitted.stage_mut(),
            store.index_baseline().unwrap(),
            &store.writer().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(admitted);
    std::fs::remove_dir_all(batch).unwrap();
    pin
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    fn selected_reports(
        store: &Store,
        pin: IndexPin,
    ) -> BTreeMap<String, (String, serde_json::Value)> {
        let db = store.cache().unwrap();
        let descriptors:Vec<(String,String)>=db.prepare("SELECT s.producer_id,b.descriptor_sha256 FROM semantic_revision_raw_selections s JOIN semantic_raw_batches b ON b.batch_id=s.batch_id WHERE s.revision_id=?1 ORDER BY s.producer_id").unwrap().query_map([semantic_pin_key(pin)],|row|Ok((row.get(0)?,row.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        drop(db);
        descriptors
            .into_iter()
            .map(|(producer, digest)| {
                let report = store
                    .semantic_batch_report_at(pin, &digest)
                    .unwrap()
                    .expect("selected batch retains its authentic import report");
                (producer, (digest, report))
            })
            .collect()
    }

    #[test]
    fn native_first_is_readable_and_second_pin_never_mutates_first() {
        let (_state, _root, store, _options, base, native) = fixture_for_tests();
        let pending = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
        let imported_reports = selected_reports(&store, base);
        let native_reports = selected_reports(&store, native);
        assert_eq!(native_reports, imported_reports);
        assert_eq!(pending.target_pin, native);
        let status = |pin, path| {
            store
                .evidence_response()
                .unwrap()
                .document_semantic_status_at(path, Some(pin))
                .unwrap()
        };
        assert_eq!(
            status(native, "f0.js").state,
            SemanticAvailability::CatchingUp
        );
        assert_eq!(status(native, "f1.js").state, SemanticAvailability::Fresh);
        assert_eq!(status(native, "f1.js").semantic_origin.unwrap().pin, base);
        let cancel = Arc::new(AtomicBool::new(false));
        let complete = store
            .continue_pending_semantic(&pending, &store.writer().unwrap(), &cancel, || Ok(()))
            .unwrap()
            .unwrap();
        assert!(complete.index_revision > native.index_revision);
        assert_eq!(selected_reports(&store, complete), imported_reports);
        assert_eq!(selected_reports(&store, native), native_reports);
        assert_eq!(
            status(native, "f0.js").state,
            SemanticAvailability::CatchingUp
        );
        assert!(store.pending_semantic_rejoin_at_head().unwrap().is_none());
        let native_id = |pin| {
            store
                .retained_native_snapshot_at(pin)
                .unwrap()
                .0
                .revision
                .id
        };
        assert_eq!(native_id(native), native_id(complete));
    }
    #[test]
    fn cancelled_full_transaction_rolls_back_and_keeps_pending_pin() {
        let (_state, _root, store, _options, _base, native) = fixture_for_tests();
        let pending = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        store.set_semantic_rejoin_active_hook_for_tests(move || {
            signal.store(true, Ordering::Release)
        });
        assert_eq!(
            store
                .continue_pending_semantic(&pending, &store.writer().unwrap(), &cancel, || Ok(()))
                .unwrap(),
            None
        );
        assert_eq!(store.index_baseline().unwrap(), native);
        assert_eq!(
            store.pending_semantic_rejoin_at_head().unwrap(),
            Some(pending)
        );
        // The old attempt has released its writer. A new edit publishes native
        // before semantic completion and only the latest target can finish.
        std::fs::write(_root.path().join("f1.js"), "function f1(){return 7;}\n").unwrap();
        let latest_cancel = Arc::new(AtomicBool::new(false));
        let (graph, evidence, capture) = crate::indexer::index_workspace_bundle(
            &_options,
            store.root_id(),
            &latest_cancel,
            |_| {},
        )
        .unwrap();
        let leader = store.writer().unwrap();
        let newer = store
            .publish_native(&graph, &capture, &evidence, &leader, native, &latest_cancel)
            .unwrap();
        let latest = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
        assert_eq!(latest.target_pin, newer);
        let completed = store
            .continue_pending_semantic(&latest, &leader, &latest_cancel, || Ok(()))
            .unwrap()
            .unwrap();
        assert!(completed.index_revision > newer.index_revision);
        assert!(store.pending_semantic_rejoin_at_head().unwrap().is_none());
        assert_eq!(
            store
                .evidence_response()
                .unwrap()
                .document_semantic_status_at("f0.js", Some(native))
                .unwrap()
                .state,
            SemanticAvailability::CatchingUp
        );
    }
    #[test]
    fn superseded_attempt_cannot_publish_or_clear_new_target() {
        let (_state, root, store, options, _base, native) = fixture_for_tests();
        let old = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
        std::fs::write(root.path().join("f1.js"), "function f1(){return 9;}\n").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, evidence, capture) =
            crate::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let leader = store.writer().unwrap();
        let newer = store
            .publish_native(&graph, &capture, &evidence, &leader, native, &cancel)
            .unwrap();
        assert_eq!(
            store
                .continue_pending_semantic(&old, &leader, &cancel, || Ok(()))
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .pending_semantic_rejoin_at_head()
                .unwrap()
                .unwrap()
                .target_pin,
            newer
        );
    }
    #[test]
    fn origin_links_are_flat_and_held_by_retained_pending_views() {
        let (_state, root, store, options, base, native) = fixture_for_tests();
        let cancel = Arc::new(AtomicBool::new(false));
        std::fs::write(root.path().join("f1.js"), "function f1(){return 99;}\n").unwrap();
        let (graph, evidence, capture) =
            crate::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let leader = store.writer().unwrap();
        let newer = store
            .publish_native(&graph, &capture, &evidence, &leader, native, &cancel)
            .unwrap();
        let pending = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
        store
            .continue_pending_semantic(&pending, &leader, &cancel, || Ok(()))
            .unwrap()
            .unwrap();
        let db = store.cache().unwrap();
        for pin in [native, newer] {
            let origins: Vec<String> = db
                .prepare(
                    "SELECT origin_revision_id FROM semantic_native_origins WHERE revision_id=?1",
                )
                .unwrap()
                .query_map([semantic_pin_key(pin)], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(origins, vec![semantic_pin_key(base)]);
        }
        // Head no longer selects the origin, but both immutable native-first
        // pins still do. Their grace must not lose its semantic/raw inputs.
        let held:bool=db.query_row(concat!("SELECT NOT EXISTS(SELECT 1 FROM native_revision_supersessions WHERE revision_id=?1 ",revision_release_holds!(),")"),[semantic_pin_key(base)],|r|r.get(0)).unwrap();
        assert!(held);
    }

    #[test]
    fn added_documents_catch_up_and_deleted_documents_have_no_origin_row() {
        let (_state, root, store, options, _base, native) = fixture_for_tests();
        std::fs::write(root.path().join("new.js"), "function added(){return 5;}\n").unwrap();
        std::fs::remove_file(root.path().join("f1.js")).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, evidence, capture) =
            crate::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        let next = store
            .publish_native(
                &graph,
                &capture,
                &evidence,
                &store.writer().unwrap(),
                native,
                &cancel,
            )
            .unwrap();
        let response = store.evidence_response().unwrap();
        let status = response
            .document_semantic_status_at("new.js", Some(next))
            .unwrap();
        assert_eq!(status.state, SemanticAvailability::CatchingUp);
        store.native_at(next,|db,selected| {
            let new_row:(Option<i64>,bool)=db.query_row("SELECT origin_tuple_id,source_eligible FROM semantic_document_origins WHERE revision_id=?1 AND path='new.js'",[&selected.key],|r|Ok((r.get(0)?,r.get(1)?)))?;
            assert_eq!(new_row,(None,false));
            let deleted:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_document_origins WHERE revision_id=?1 AND path='f1.js')",[&selected.key],|r|r.get(0))?;
            assert!(!deleted);
            Ok(())
        }).unwrap();
        assert!(store.methods_at("new.js", Some(next)).unwrap().is_some());
    }

    #[test]
    fn retained_native_pin_holds_origin_until_actual_release_and_collection() {
        let _state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        for ordinal in 0..2 {
            std::fs::write(
                root.path().join(format!("f{ordinal}.js")),
                format!("function f{ordinal}(){{return {ordinal};}}\n"),
            )
            .unwrap();
        }
        let store = Store::open_for_tests(_state.path(), root.path()).unwrap();
        let options = crate::indexer::IndexOptions::new(root.path().to_owned());
        store.set_retention_clock_for_tests(1000, 0);
        publish_native_for_tests(&store, &options);
        let origin = import_empty_batch_for_tests(&store, &options, "trellis-synthetic-scip");
        let origin_key = semantic_pin_key(origin);
        let old_batch: i64 = store
            .cache()
            .unwrap()
            .query_row(
                "SELECT batch_id FROM semantic_revision_raw_selections WHERE revision_id=?1",
                [&origin_key],
                |r| r.get(0),
            )
            .unwrap();
        store.set_retention_clock_for_tests(1901, 901);
        std::fs::write(root.path().join("f0.js"), "function f0(){return 9;}\n").unwrap();
        let native = publish_native_for_tests(&store, &options);
        store.set_retention_clock_for_tests(1902, 902);
        let cancel = Arc::new(AtomicBool::new(false));
        let pending = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
        store
            .continue_pending_semantic(&pending, &store.writer().unwrap(), &cancel, || Ok(()))
            .unwrap()
            .unwrap();
        // Two replacement imports move both the head selection and its captured
        // revision past the old raw batch; only retained native views hold origin.
        store.set_retention_clock_for_tests(1903, 903);
        import_empty_batch_for_tests(&store, &options, "trellis-synthetic-scip");
        store.set_retention_clock_for_tests(1904, 904);
        let head = import_empty_batch_for_tests(&store, &options, "trellis-synthetic-scip");
        let maintain = || {
            let leader = store.writer().unwrap();
            store.maintain_revisions(&leader).unwrap();
            store.collect_unreferenced(&leader).unwrap();
        };
        store.set_retention_clock_for_tests(1905, 905);
        maintain();
        assert_eq!(
            store
                .evidence_response()
                .unwrap()
                .document_semantic_status_at("f1.js", Some(native))
                .unwrap()
                .semantic_origin
                .unwrap()
                .pin,
            origin
        );
        assert!(
            store
                .cache()
                .unwrap()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM semantic_coverage WHERE revision_id=?1)",
                    [&origin_key],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
        store.set_retention_clock_for_tests(2805, 1805);
        maintain();
        assert!(store.retained_native_snapshot_at(native).is_err());
        let db = store.cache().unwrap();
        assert!(
            !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM semantic_coverage WHERE revision_id=?1)",
                [&origin_key],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        assert!(
            !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM semantic_raw_batches WHERE batch_id=?1)",
                [old_batch],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        Store::check_retained_foreign_keys(&db).unwrap();
        assert_eq!(store.index_baseline().unwrap(), head);
        assert!(
            store
                .selected_raw_inputs_at(head)
                .unwrap()
                .iter()
                .all(|batch| batch.batch_id != old_batch)
        );
    }

    #[test]
    fn explicit_import_replaces_only_its_pending_producer_slot() {
        let (_state, root, store) = super::super::selected_read_once_tests::published_store(2);
        let options = crate::indexer::IndexOptions::new(root.path().to_owned());
        import_empty_batch_for_tests(&store, &options, "trellis-synthetic-scip");
        let q_origin = import_empty_batch_for_tests(&store, &options, "scip-typescript");
        std::fs::write(root.path().join("f0.js"), "function f0(){return 17;}\n").unwrap();
        let native = publish_native_for_tests(&store, &options);
        let old_reports = selected_reports(&store, native);
        let old = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
        let imported = import_empty_batch_for_tests(&store, &options, "trellis-synthetic-scip");
        let plan = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
        assert_eq!(plan.target_pin, imported);
        let imported_reports = selected_reports(&store, imported);
        let producer = "trellis-synthetic-scip";
        assert_ne!(imported_reports[producer].0, old_reports[producer].0);
        assert!(
            store
                .semantic_batch_report_at(imported, &old_reports[producer].0)
                .unwrap()
                .is_none()
        );
        store
            .native_at(imported, |db, selected| {
                let p = semantic_document_status(
                    db,
                    selected,
                    "trellis-synthetic-scip",
                    "javascript",
                    "f0.js",
                )?;
                let q = semantic_document_status(
                    db,
                    selected,
                    "scip-typescript",
                    "javascript",
                    "f0.js",
                )?;
                let unchanged = semantic_document_status(
                    db,
                    selected,
                    "scip-typescript",
                    "javascript",
                    "f1.js",
                )?;
                assert_eq!(p.state, SemanticAvailability::Fresh);
                assert_eq!(q.state, SemanticAvailability::CatchingUp);
                assert_eq!(unchanged.semantic_origin.unwrap().pin, q_origin);
                Ok(())
            })
            .unwrap();
        let batches: Vec<_> = store
            .selected_raw_inputs_at(imported)
            .unwrap()
            .into_iter()
            .map(|b| (b.producer_id, b.batch_id))
            .collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let leader = store.writer().unwrap();
        assert_eq!(
            store
                .continue_pending_semantic(&old, &leader, &cancel, || Ok(()))
                .unwrap(),
            None
        );
        assert_eq!(store.index_baseline().unwrap(), imported);
        let completed = store
            .continue_pending_semantic(&plan, &leader, &cancel, || Ok(()))
            .unwrap()
            .unwrap();
        assert_eq!(selected_reports(&store, completed), imported_reports);
        assert!(
            store
                .semantic_batch_report_at(completed, &old_reports[producer].0)
                .unwrap()
                .is_none()
        );
        assert_eq!(selected_reports(&store, native), old_reports);
        assert_eq!(
            store
                .selected_raw_inputs_at(completed)
                .unwrap()
                .into_iter()
                .map(|b| (b.producer_id, b.batch_id))
                .collect::<Vec<_>>(),
            batches
        );
        assert!(store.pending_semantic_rejoin_at_head().unwrap().is_none());
        assert_eq!(
            store
                .retained_native_snapshot_at(native)
                .unwrap()
                .0
                .revision
                .id,
            store
                .retained_native_snapshot_at(completed)
                .unwrap()
                .0
                .revision
                .id
        );
        store
            .native_at(imported, |db, selected| {
                assert_eq!(
                    semantic_document_status(
                        db,
                        selected,
                        "scip-typescript",
                        "javascript",
                        "f0.js"
                    )?
                    .state,
                    SemanticAvailability::CatchingUp
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn pending_reopens_and_lost_commit_resolves_before_late_cancellation() {
        let (state, root, store, _options, _base, native) = fixture_for_tests();
        let pending = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
        drop(store);
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert_eq!(
            store.pending_semantic_rejoin_at_head().unwrap(),
            Some(pending.clone())
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        store.set_semantic_rejoin_active_hook_for_tests(move || {
            signal.store(true, Ordering::Release)
        });
        assert_eq!(
            store
                .continue_pending_semantic(&pending, &store.writer().unwrap(), &cancel, || Ok(()))
                .unwrap(),
            None
        );
        drop(store);
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert_eq!(
            store.pending_semantic_rejoin_at_head().unwrap(),
            Some(pending.clone())
        );
        store.fail_next_live_publish_post_commit_busy();
        let cancel = Arc::new(AtomicBool::new(false));
        let completed = store
            .continue_pending_semantic(&pending, &store.writer().unwrap(), &cancel, || Ok(()))
            .unwrap()
            .unwrap();
        assert!(cancel.load(Ordering::Acquire));
        assert!(completed.index_revision > native.index_revision);
        drop(store);
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert_eq!(store.index_baseline().unwrap(), completed);
        assert!(store.pending_semantic_rejoin_at_head().unwrap().is_none());
        assert_eq!(
            store
                .continue_pending_semantic(
                    &pending,
                    &store.writer().unwrap(),
                    &Arc::new(AtomicBool::new(false)),
                    || Ok(())
                )
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .evidence_response()
                .unwrap()
                .document_semantic_status_at("f0.js", Some(native))
                .unwrap()
                .state,
            SemanticAvailability::CatchingUp
        );
        let db = store.cache().unwrap();
        assert_eq!(
            db.query_row(
                "SELECT selection_identity FROM semantic_rejoin_completions WHERE revision_id=?1",
                [semantic_pin_key(completed)],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            pending.selection_identity
        );
    }

    #[test]
    fn sqlite_progress_cancellation_rolls_back_without_publishing() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE progress_rows(n INTEGER);")
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        let (active_tx, active_rx) = std::sync::mpsc::channel();
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            active_rx.recv().unwrap();
            signal.store(true, Ordering::Release);
            cancelled_tx.send(()).unwrap();
        });
        struct Context {
            active: std::sync::mpsc::Sender<()>,
            cancelled: std::sync::mpsc::Receiver<()>,
            cancel: CancelFlag,
        }
        unsafe extern "C" fn active_statement(arg: *mut std::ffi::c_void) -> libc::c_int {
            // SAFETY: this test owns the connection; context outlives handler.
            let context = unsafe { &*(arg as *const Context) };
            let _ = context.active.send(());
            let _ = context.cancelled.recv();
            i32::from(context.cancel.load(Ordering::Acquire))
        }
        let tx = db.unchecked_transaction().unwrap();
        let context = Context {
            active: active_tx,
            cancelled: cancelled_rx,
            cancel,
        };
        let handle = unsafe { tx.handle() };
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                handle,
                256,
                Some(active_statement),
                (&context as *const Context).cast_mut().cast(),
            );
        }
        let progress = MaintenanceProgressGuard(handle);
        let result=tx.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) INSERT INTO progress_rows SELECT x FROM n",[]);
        assert!(result.is_err());
        drop(progress);
        drop(tx);
        worker.join().unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM progress_rows", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
