//! Immutable native-N / validated semantic-basis-P selection and atomic catch-up.
use super::*;
pub(super) mod context;
mod continuation;
mod local_targets;
#[cfg(feature = "test-support")]
mod oracle;
#[cfg(test)]
mod tests;
pub(crate) mod witness;
mod witness_preparation;
#[cfg(any(test, feature = "test-support"))]
pub use continuation::set_semantic_step_slice_for_tests;
pub(crate) use continuation::{SemanticContinuation, SemanticUnitOutcome, step_slice};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingSemanticSelection {
    pub producer_id: String,
    pub batch_id: i64,
    pub captured_pin: IndexPin,
    pub basis_pin: IndexPin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingSemanticTarget {
    pub pin: IndexPin,
    pub native_revision_id: String,
    pub root_id: String,
    pub selections: Vec<PendingSemanticSelection>,
}

pub(crate) struct PreparedSemantic {
    pub producer: crate::semantic_evidence::Producer,
    pub output: PreparedOutput,
    /// A below-capture batch whose staged capture witness this S publishes.
    pub capture_witness: Option<i64>,
}

pub(crate) enum PreparedOutput {
    Joined {
        admitted: Box<crate::semantic_batch::AdmittedBatch>,
        joined: crate::semantic_batch::JoinedBatch,
    },
    Unavailable {
        diagnostic: String,
    },
}

fn parse_pin(key: &str) -> Result<IndexPin> {
    let rest = key
        .strip_prefix("pin:v1:")
        .context("invalid semantic projection pin")?;
    let (generation, revision) = rest
        .rsplit_once(':')
        .context("invalid semantic projection pin")?;
    let pin = IndexPin {
        index_generation: generation.parse().context(SelectedIntegrity(
            "incompatible_index: invalid projection generation".into(),
        ))?,
        index_revision: revision.parse().context(SelectedIntegrity(
            "incompatible_index: invalid projection revision".into(),
        ))?,
    };
    selected_ensure!(
        pin.index_revision > 0 && semantic_pin_key(pin) == key,
        "incompatible_index: invalid projection pin"
    );
    Ok(pin)
}

pub(super) fn clear(db: &Connection) -> Result<()> {
    db.execute_batch("DELETE FROM semantic_pending_target; DELETE FROM semantic_pending_selections; DELETE FROM semantic_catchup_commits; DELETE FROM semantic_projection_documents; DELETE FROM semantic_projection_origins; DELETE FROM semantic_projection_dependencies; DELETE FROM semantic_projection_lookup_dependencies; DELETE FROM semantic_projection_contexts;")?;
    Ok(())
}

/// Build reverse associations only from validated, published endpoints. Opaque lookup keys
/// remain historical dependencies; they are not a namespace for inventing N resolutions.
pub(super) fn index_dependencies(db: &Connection, key: &str) -> Result<()> {
    db.execute("INSERT OR IGNORE INTO semantic_projection_contexts SELECT DISTINCT b.tuple_id FROM semantic_basis b JOIN semantic_coverage c ON c.tuple_id=b.tuple_id WHERE c.revision_id=?1",[key])?;
    context::validate(db, key)?;
    let endpoints = [
        "SELECT s.tuple_id,t.internal_language,t.internal_path FROM semantic_symbols s JOIN semantic_symbol_targets t ON t.symbol_id=s.symbol_id WHERE t.kind='internal'",
        "SELECT b.tuple_id,b.join_document_language,b.join_document_path FROM semantic_declaration_bindings b",
        "SELECT b.tuple_id,b.join_document_language,b.join_document_path FROM semantic_declaration_bindings b JOIN semantic_declaration_join_candidates j ON j.binding_id=b.binding_id",
        "SELECT r.tuple_id,r.declared_internal_language,r.declared_internal_path FROM semantic_references r WHERE r.declared_kind='internal'",
        "SELECT r.tuple_id,r.internal_language,r.internal_path FROM semantic_reference_candidates r WHERE r.kind='internal'",
        "SELECT b.tuple_id,b.declared_internal_language,b.declared_internal_path FROM semantic_call_bindings b WHERE b.declared_kind='internal'",
        "SELECT b.tuple_id,t.internal_language,t.internal_path FROM semantic_call_bindings b JOIN semantic_call_candidates t ON t.binding_id=b.binding_id WHERE t.kind='internal'",
        "SELECT b.tuple_id,t.internal_language,t.internal_path FROM semantic_call_bindings b JOIN semantic_call_possible_dispatch t ON t.binding_id=b.binding_id WHERE t.kind='internal'",
        "SELECT b.tuple_id,b.join_document_language,b.join_document_path FROM semantic_call_bindings b",
        "SELECT b.tuple_id,b.join_document_language,b.join_document_path FROM semantic_call_bindings b JOIN semantic_call_join_candidates j ON j.binding_id=b.binding_id",
        "SELECT r.tuple_id,r.source_internal_language,r.source_internal_path FROM semantic_type_relationships r WHERE r.source_kind='internal'",
        "SELECT r.tuple_id,r.target_internal_language,r.target_internal_path FROM semantic_type_relationships r WHERE r.target_kind='internal'",
    ];
    for endpoint in endpoints {
        db.execute(&format!("INSERT OR IGNORE INTO semantic_projection_dependencies SELECT e.* FROM ({endpoint}) e JOIN semantic_coverage c ON c.tuple_id=e.tuple_id WHERE c.revision_id=?1"), [key])?;
    }
    let missing: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_projection_dependencies e JOIN semantic_coverage c ON c.tuple_id=e.tuple_id LEFT JOIN revision_documents d ON d.revision_id=c.revision_id AND d.language=e.language AND d.path=e.path WHERE c.revision_id=?1 AND d.vid IS NULL)", [key], |row|row.get(0))?;
    selected_ensure!(
        !missing,
        "incompatible_index: semantic dependency endpoint missing"
    );
    db.execute("INSERT OR IGNORE INTO semantic_projection_lookup_dependencies SELECT c.tuple_id,d.lookup_key FROM semantic_coverage c JOIN semantic_basis b ON b.tuple_id=c.tuple_id JOIN semantic_basis_lookup_dependencies d ON d.basis_id=b.basis_id WHERE c.revision_id=?1", [key])?;
    Ok(())
}

/// Flat origins always point to a validated publication, not a preceding overlay.
pub(super) fn write_pending(
    db: &Connection,
    prior: IndexPin,
    pin: IndexPin,
    root_id: &str,
    config: &[serde_json::Value],
    dependencies: &[serde_json::Value],
    cancel: &CancelFlag,
) -> Result<()> {
    let old = semantic_pin_key(prior);
    let key = semantic_pin_key(pin);
    let missing_requested: bool = db.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM semantic_requested_producer_languages l
            LEFT JOIN semantic_requested_producers p
              ON p.revision_id=l.revision_id AND p.id=l.id
            WHERE l.revision_id=?1 AND p.id IS NULL
        )",
        [&old],
        |row| row.get(0),
    )?;
    selected_ensure!(
        !missing_requested,
        "incompatible_index: selected requested producer descriptor missing"
    );
    context::validate_profiles(db, &old)?;
    for sql in [
        "INSERT INTO semantic_revision_producers SELECT ?1,id,version,context_mode,context_trust,context_input_hash FROM semantic_revision_producers WHERE revision_id=?2",
        "INSERT INTO semantic_requested_producers SELECT ?1,id,version,executable_hash FROM semantic_requested_producers WHERE revision_id=?2",
        "INSERT INTO semantic_requested_producer_languages SELECT ?1,id,language,ordinal FROM semantic_requested_producer_languages WHERE revision_id=?2",
        "INSERT INTO semantic_revision_raw_selections SELECT ?1,producer_id,batch_id,captured_pin FROM semantic_revision_raw_selections WHERE revision_id=?2",
        "INSERT INTO semantic_import_reports SELECT ?1,producer_id,report_json FROM semantic_import_reports WHERE revision_id=?2",
    ] {
        db.execute(sql, params![key, old])?;
    }
    let same_native: bool = db.query_row("SELECT p.native_revision_id=n.native_revision_id AND p.source_set_id=n.source_set_id FROM native_revisions p CROSS JOIN native_revisions n WHERE p.id=?1 AND n.id=?2",params![old,key],|r|r.get(0))?;
    if same_native {
        context::validate(db, &old)?;
        // Same-native copies retain exact destination authority, including #657.
        copy_semantic_rows(db, &old, &key, true)?;
        copy_projection_except(db, &old, &key, "")?;
        index_dependencies(db, &key)?;
        return Ok(());
    }
    db.execute("INSERT INTO semantic_projection_origins SELECT ?1,p.id,COALESCE(o.basis_pin,?2) FROM semantic_revision_producers p LEFT JOIN semantic_projection_origins o ON o.revision_id=p.revision_id AND o.producer_id=p.id WHERE p.revision_id=?2", params![key, old])?;
    // Missing tuples at a validated basis are corruption, never intentional pending.
    let missing: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_projection_origins o JOIN revision_documents d ON d.revision_id=o.basis_pin JOIN semantic_revision_producers p ON p.revision_id=o.basis_pin AND p.id=o.producer_id JOIN semantic_producer_languages l ON l.id=p.id AND l.version=p.version AND l.language=d.language LEFT JOIN semantic_coverage c ON c.revision_id=o.basis_pin AND c.producer_id=o.producer_id AND c.language=d.language AND c.path=d.path WHERE o.revision_id=?1 AND c.tuple_id IS NULL)", [&key], |row| row.get(0))?;
    selected_ensure!(
        !missing,
        "incompatible_index: projection basis tuple missing"
    );
    db.execute("INSERT INTO semantic_projection_documents
        SELECT ?1,o.producer_id,n.language,n.path,
          CASE WHEN p.vid IS NULL OR p.vid<>n.vid OR b.source_set_id<>h.source_set_id THEN 1 ELSE 0 END
        FROM semantic_projection_origins o
        JOIN semantic_revision_producers sp ON sp.revision_id=o.basis_pin AND sp.id=o.producer_id
        JOIN semantic_producer_languages l ON l.id=sp.id AND l.version=sp.version
        JOIN revision_documents n ON n.revision_id=?1 AND n.language=l.language
        JOIN native_revisions h ON h.id=n.revision_id
        JOIN native_revisions b ON b.id=o.basis_pin
        LEFT JOIN revision_documents p ON p.revision_id=o.basis_pin AND p.language=n.language AND p.path=n.path
        WHERE o.revision_id=?1", [&key])?;
    // Changed documents drive the indexed reverse lookup. A pending target's outgoing
    // evidence does not invalidate callers unless their own recorded endpoints changed.
    db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS native_first_changed(basis_pin TEXT NOT NULL,language TEXT NOT NULL,path TEXT NOT NULL,PRIMARY KEY(basis_pin,language,path)); DELETE FROM native_first_changed;")?;
    db.execute("INSERT OR IGNORE INTO native_first_changed
        SELECT o.basis_pin,p.language,p.path FROM semantic_projection_origins o
        JOIN revision_documents p ON p.revision_id=o.basis_pin
        LEFT JOIN revision_documents n ON n.revision_id=?1 AND n.language=p.language AND n.path=p.path
        WHERE o.revision_id=?1 AND (n.vid IS NULL OR n.vid<>p.vid)", [&key])?;
    db.execute("UPDATE semantic_projection_documents SET pending=1 WHERE revision_id=?1 AND (producer_id,language,path) IN (
        SELECT c.producer_id,c.language,c.path FROM native_first_changed x
        CROSS JOIN semantic_projection_dependencies e INDEXED BY semantic_projection_reverse ON e.language=x.language AND e.path=x.path
        JOIN semantic_coverage c ON c.tuple_id=e.tuple_id AND c.revision_id=x.basis_pin
        JOIN semantic_projection_origins o ON o.revision_id=?1 AND o.producer_id=c.producer_id AND o.basis_pin=x.basis_pin)", [&key])?;
    db.execute_batch("DELETE FROM native_first_changed")?;
    context::mark_pending(db, &key, config, dependencies, cancel)?;
    db.execute("INSERT INTO semantic_pending_selections SELECT s.revision_id,s.producer_id,s.batch_id,s.captured_pin,o.basis_pin FROM semantic_revision_raw_selections s JOIN semantic_projection_origins o ON o.revision_id=s.revision_id AND o.producer_id=s.producer_id WHERE s.revision_id=?1", [&key])?;
    db.execute("DELETE FROM semantic_pending_target", [])?;
    // No selected raw input means there is no derived work to resume.
    db.execute("INSERT INTO semantic_pending_target SELECT 1,r.id,r.native_revision_id,?2 FROM native_revisions r WHERE r.id=?1 AND EXISTS(SELECT 1 FROM semantic_revision_raw_selections WHERE revision_id=r.id)", params![key, root_id])?;
    Ok(())
}

fn pending_target(db: &Connection) -> Result<Option<PendingSemanticTarget>> {
    // Every S step re-reads the marker; its statements stay prepared.
    let marker: Option<(String,String,String)> = db.prepare_cached("SELECT revision_id,native_revision_id,root_id FROM semantic_pending_target WHERE singleton=1")?.query_row([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((key, native_revision_id, root_id)) = marker else {
        let missing: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_pending_selections s JOIN index_metadata m ON m.singleton=1 AND s.revision_id='pin:v1:'||m.index_generation||':'||m.index_revision)",[],|r|r.get(0))?;
        selected_ensure!(
            !missing,
            "incompatible_index: selected semantic target marker missing"
        );
        return Ok(None);
    };
    let pin = parse_pin(&key)?;
    let head: (String,i64,String) = db.prepare_cached("SELECT m.index_generation,m.index_revision,r.native_revision_id FROM index_metadata m JOIN native_revisions r ON r.id=?1 WHERE m.singleton=1")?.query_row([&key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    selected_ensure!(
        head.0 == pin.index_generation.to_string()
            && head.1 == pin.index_revision as i64
            && head.2 == native_revision_id,
        "incompatible_index: pending semantic target differs from head"
    );
    let mut statement = db.prepare_cached("SELECT producer_id,batch_id,captured_pin,basis_pin FROM semantic_pending_selections WHERE revision_id=?1 ORDER BY producer_id")?;
    let rows = statement.query_map([&key], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut selections = Vec::new();
    for row in rows {
        let (producer_id, batch_id, captured, basis) = row?;
        selections.push(PendingSemanticSelection {
            producer_id,
            batch_id,
            captured_pin: parse_pin(&captured)?,
            basis_pin: parse_pin(&basis)?,
        });
    }
    let malformed: bool = db.prepare_cached("SELECT EXISTS(SELECT 1 FROM semantic_pending_selections s LEFT JOIN semantic_revision_raw_selections r ON r.revision_id=s.revision_id AND r.producer_id=s.producer_id LEFT JOIN semantic_projection_origins o ON o.revision_id=s.revision_id AND o.producer_id=s.producer_id WHERE s.revision_id=?1 AND (r.batch_id IS NOT s.batch_id OR r.captured_pin IS NOT s.captured_pin OR o.basis_pin IS NOT s.basis_pin))")?.query_row([&key], |r|r.get(0))?;
    selected_ensure!(
        !malformed,
        "incompatible_index: pending raw selection differs"
    );
    let count: i64 = db
        .prepare_cached(
            "SELECT count(*) FROM semantic_revision_raw_selections r
         JOIN semantic_projection_origins o ON o.revision_id=r.revision_id
            AND o.producer_id=r.producer_id WHERE r.revision_id=?1",
        )?
        .query_row([&key], |r| r.get(0))?;
    selected_ensure!(
        !selections.is_empty() && count == selections.len() as i64,
        "incompatible_index: pending semantic selection missing"
    );
    Ok(Some(PendingSemanticTarget {
        pin,
        native_revision_id,
        root_id,
        selections,
    }))
}

pub(crate) fn semantic_projection_for(
    db: &Connection,
    pin: IndexPin,
    producer: &str,
    language: &str,
    path: &str,
) -> Result<Option<SemanticProjection>> {
    let key = semantic_pin_key(pin);
    let row: Option<(String,String,bool)> = db.query_row("SELECT o.basis_pin,b.native_revision_id,d.pending FROM semantic_projection_origins o JOIN native_revisions b ON b.id=o.basis_pin JOIN semantic_projection_documents d ON d.revision_id=o.revision_id AND d.producer_id=o.producer_id WHERE o.revision_id=?1 AND o.producer_id=?2 AND d.language=?3 AND d.path=?4",params![key,producer,language,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if row.is_none() {
        let required: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_projection_origins o JOIN semantic_revision_producers p ON p.revision_id=o.revision_id AND p.id=o.producer_id JOIN semantic_producer_languages l ON l.id=p.id AND l.version=p.version AND l.language=?3 JOIN revision_documents d ON d.revision_id=o.revision_id AND d.language=?3 AND d.path=?4 WHERE o.revision_id=?1 AND o.producer_id=?2)",params![key,producer,language,path],|r|r.get(0))?;
        selected_ensure!(
            !required,
            "incompatible_index: projection document disposition missing"
        );
    }
    let retained: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_revision_raw_selections WHERE revision_id=?1 AND producer_id=?2)",
        params![key, producer],
        |r| r.get(0),
    )?;
    row.map(|(basis, native, pending)| {
        let tuple: Option<i64> = db.query_row(
            "SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",
            params![basis,producer,language,path], |r|r.get(0)).optional()?;
        if let Some(tuple) = tuple {
            context::validate_tuple(db, &basis, producer, tuple)?;
        }
        Ok(SemanticProjection {
            current_pin: pin,
            semantic_basis_pin: parse_pin(&basis)?,
            basis_native_revision_id: native,
            semantic_status: if pending && retained {
                SemanticStatus::CatchingUp
            } else if pending {
                SemanticStatus::NeedsReimport
            } else {
                SemanticStatus::OlderBasis
            },
        })
    })
    .transpose()
}

pub(crate) fn projection_tuple(
    db: &Connection,
    pin: IndexPin,
    producer: &str,
    language: &str,
    path: &str,
) -> Result<Option<(String, i64, SemanticProjection)>> {
    let Some(projection) = semantic_projection_for(db, pin, producer, language, path)? else {
        return Ok(None);
    };
    if projection.semantic_status.is_pending() {
        return Ok(None);
    }
    let basis = semantic_pin_key(projection.semantic_basis_pin);
    let tuple: Option<i64> = db.query_row("SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",params![basis,producer,language,path],|r|r.get(0)).optional()?;
    let tuple = tuple.context(SelectedIntegrity(
        "incompatible_index: eligible projection tuple missing".into(),
    ))?;
    Ok(Some((basis, tuple, projection)))
}

/// `None` when the endpoint is eligible older-basis evidence; otherwise its boundary.
pub(crate) fn endpoint_boundary(
    db: &Connection,
    pin: IndexPin,
    producer: &str,
    language: &str,
    path: &str,
    basis_pin: IndexPin,
) -> Result<Option<crate::graph_query::BoundaryReason>> {
    use crate::graph_query::BoundaryReason;
    Ok(
        match semantic_projection_for(db, pin, producer, language, path)? {
            Some(p)
                if p.semantic_basis_pin == basis_pin
                    && p.semantic_status == SemanticStatus::OlderBasis =>
            {
                None
            }
            Some(p) if p.semantic_status.is_pending() => {
                Some(BoundaryReason::pending(p.semantic_status))
            }
            _ => Some(BoundaryReason::CatchingUp),
        },
    )
}

impl Store {
    /// Complete an offline fixture's exact target through the ordinary bounded owner units.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn complete_pending_semantic_for_tests(
        &self,
        expected: IndexPin,
        session: &Arc<topology::WriterSession>,
        cancel: &CancelFlag,
    ) -> Result<Option<IndexPin>> {
        self.verify_writer_session(session)?;
        ensure!(
            self.index_baseline()? == expected,
            "semantic fixture head changed"
        );
        let Some(target) = self.pending_semantic_target()? else {
            return Ok(None);
        };
        ensure!(target.pin == expected, "semantic fixture target changed");
        let mut continuation = None;
        // A unit bound detects a broken fixture; only Published proves completion.
        for _ in 0..10_000 {
            ensure!(
                self.pending_semantic_target()?.as_ref() == Some(&target),
                "semantic fixture target superseded"
            );
            match crate::index_coordinator::cooperative_semantic_unit(
                self,
                session,
                &mut continuation,
                cancel,
                || true,
            )? {
                SemanticUnitOutcome::Progress => {}
                SemanticUnitOutcome::Published => {
                    let published = self.index_baseline()?;
                    ensure!(
                        published.index_generation == expected.index_generation
                            && published.index_revision == expected.index_revision + 1,
                        "semantic fixture published a different paired pin"
                    );
                    self.with_evidence(|db| {
                        let committed: bool = db.query_row(
                            "SELECT EXISTS(SELECT 1 FROM semantic_catchup_commits k JOIN native_revisions s ON s.id=k.revision_id WHERE k.revision_id=?1 AND k.target_revision_id=?2 AND s.native_revision_id=?3)",
                            params![semantic_pin_key(published), semantic_pin_key(expected), target.native_revision_id],
                            |row| row.get(0),
                        )?;
                        ensure!(committed && pending_target(db)?.is_none(), "semantic fixture publication did not clear its exact target");
                        Ok(())
                    })?;
                    return Ok(Some(published));
                }
                other => anyhow::bail!("semantic fixture did not progress: {other:?}"),
            }
        }
        anyhow::bail!("semantic fixture exceeded its unit bound")
    }

    /// Offline fixtures that published native N with their own writer and dropped it: take
    /// the writer again and drive any outstanding exact target at head to its S commit.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn catch_up_semantic_for_tests(&self, cancel: &CancelFlag) -> Result<IndexPin> {
        let session = Arc::new(topology::WriterSession::writer(self.writer()?));
        let head = self.index_baseline()?;
        Ok(self
            .complete_pending_semantic_for_tests(head, &session, cancel)?
            .unwrap_or(head))
    }

    /// Whether the published head still has an outstanding semantic catch-up target.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn semantic_catch_up_pending_for_tests(&self) -> Result<bool> {
        Ok(self.pending_semantic_target()?.is_some())
    }

    pub(crate) fn pending_semantic_target(&self) -> Result<Option<PendingSemanticTarget>> {
        self.with_evidence(|db| {
            let target = pending_target(db)?;
            if let Some(target) = &target {
                selected_ensure!(
                    target.root_id == self.identity.root_key,
                    "incompatible_index: pending semantic root differs"
                );
            }
            Ok(target)
        })
    }

    pub fn semantic_projection_at(
        &self,
        pin: IndexPin,
        producer: &str,
        language: &str,
        path: &str,
    ) -> Result<Option<SemanticProjection>> {
        self.native_at(pin, |db, _| {
            semantic_projection_for(db, pin, producer, language, path)
        })
    }

    fn with_pending_semantic_inputs<T>(
        &self,
        target: &PendingSemanticTarget,
        read: impl FnOnce(&Connection, &ReadRevision) -> Result<T>,
    ) -> Result<Option<T>> {
        self.native_at_reconcile(target.pin, |db, selected| {
            // S binds new keys to the same cached statements row after row. Without the
            // query planner stability guarantee SQLite re-prepares a statement whenever a
            // value it planned around (a LIMIT, a small-integer comparison) is rebound.
            storage_result(
                db.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_ENABLE_QPSG, true),
            )?;
            if pending_target(db)?.as_ref() != Some(target) {
                return Ok(None);
            }
            selected_ensure!(
                target.root_id == self.identity.root_key,
                "incompatible_index: pending semantic root differs"
            );
            read(db, selected).map(Some)
        })
    }
}

/// Preserve untouched producer overlays across a semantic-only publication. A successful
/// import replaces only its producer slot, not a competing producer's pending target.
pub(super) fn copy_projection_except(
    db: &Connection,
    old: &str,
    new: &str,
    except: &str,
) -> Result<()> {
    db.execute("INSERT INTO semantic_projection_origins SELECT ?1,producer_id,basis_pin FROM semantic_projection_origins WHERE revision_id=?2 AND producer_id<>?3",params![new,old,except])?;
    db.execute("INSERT INTO semantic_projection_documents SELECT ?1,producer_id,language,path,pending FROM semantic_projection_documents WHERE revision_id=?2 AND producer_id<>?3",params![new,old,except])?;
    db.execute("INSERT INTO semantic_pending_selections SELECT ?1,producer_id,batch_id,captured_pin,basis_pin FROM semantic_pending_selections WHERE revision_id=?2 AND producer_id<>?3",params![new,old,except])?;
    db.execute(
        "UPDATE semantic_pending_target SET revision_id=?1 WHERE revision_id=?2",
        params![new, old],
    )?;
    db.execute("DELETE FROM semantic_pending_target WHERE revision_id=?1 AND NOT EXISTS(SELECT 1 FROM semantic_pending_selections WHERE revision_id=?1)",[new])?;
    Ok(())
}

impl Store {
    /// Publish every selected producer together. Stages contain only validated output;
    /// raw selections and the original import reports keep their identities.
    pub(crate) fn publish_pending_semantic(
        &self,
        target: &PendingSemanticTarget,
        prepared: &mut [PreparedSemantic],
        session: &topology::WriterSession,
        cancel: &CancelFlag,
    ) -> Result<Option<IndexPin>> {
        self.verify_writer_session(session)?;
        let leader = session.writer_guard()?;
        leader.verify()?;
        self.identity.verify()?;
        check_cancel(cancel)?;
        let Some(_permit) = self.maintenance_try_enter() else {
            return Ok(None);
        };
        Self::semantic_phase_diagnostic("commit", "start");
        let selected_ids = target
            .selections
            .iter()
            .map(|s| s.producer_id.as_str())
            .collect::<BTreeSet<_>>();
        let prepared_ids = prepared
            .iter()
            .map(|p| p.producer.id.as_str())
            .collect::<BTreeSet<_>>();
        ensure!(
            selected_ids == prepared_ids && prepared.len() == selected_ids.len(),
            "incompatible_index: semantic catch-up producers differ"
        );
        // Verify each private stage before the main write transaction starts.
        for output in prepared.iter_mut() {
            let PreparedOutput::Joined { admitted, joined } = &mut output.output else {
                continue;
            };
            let selection = target
                .selections
                .iter()
                .find(|s| s.producer_id == output.producer.id)
                .context("missing pending producer")?;
            ensure!(
                joined.destination_pin() == target.pin
                    && joined.destination_revision_id() == target.native_revision_id
                    && joined.captured_pin() == selection.captured_pin
                    && admitted.producer().producer() == output.producer,
                "incompatible_index: semantic stage target differs"
            );
            let parts = admitted.join_parts()?;
            let (attempt, descriptor): (i64, String) = parts.conn.query_row(
                "SELECT s.attempt,h.descriptor_sha256 FROM join_state s CROSS JOIN header h",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            ensure!(
                attempt == joined.attempt() && descriptor == joined.descriptor_sha256(),
                "joined batch superseded by a later join attempt"
            );
        }
        let db = self.cache_write()?;
        storage_result(db.pragma_update(None, "cache_size", -PUBLICATION_CACHE_KIB))?;
        let tx = storage_result(Transaction::new_unchecked(
            &db,
            TransactionBehavior::Immediate,
        ))?;
        let current = self.recovery_baseline_preflight(&tx)?;
        if current.pin != Some(target.pin) || pending_target(&tx)?.as_ref() != Some(target) {
            return Ok(None);
        }
        selected_ensure!(
            target.root_id == self.identity.root_key,
            "incompatible_index: semantic target root differs"
        );
        let next = IndexPin {
            index_generation: target.pin.index_generation,
            index_revision: target
                .pin
                .index_revision
                .checked_add(1)
                .filter(|n| *n <= 9_007_199_254_740_991)
                .context("revision overflow")?,
        };
        let old = semantic_pin_key(target.pin);
        let key = semantic_pin_key(next);
        tx.execute("INSERT INTO native_revisions SELECT ?1,source_set_id,toolchain_hash,config_hash,dependency_hash,native_revision_id,source_inventory,dependency_observations,retained_toolchain,retained_config,retained_dependencies,reconcile_options,class_warnings,class_truncated,graph_stats,graph_diagnostics,?2 FROM native_revisions WHERE id=?3",params![key,next.index_revision as i64,old])?;
        tx.execute("INSERT INTO revision_documents SELECT ?1,language,path,vid,gid,cid,capture_stat,coverage_requested,coverage_selected,coverage_state,coverage_diagnostic,ordinal,class_complete FROM revision_documents WHERE revision_id=?2",params![key,old])?;
        tx.execute("INSERT INTO revision_capture_inputs SELECT ?1,input_key,payload FROM revision_capture_inputs WHERE revision_id=?2",params![key,old])?;
        let (producer_sha, binding_sha) = revision_producer_binding(&tx, &key)?;
        tx.execute(
            "INSERT INTO revision_producer_bindings VALUES(?1,?2,?3)",
            params![key, producer_sha, binding_sha],
        )?;
        for sql in [
            "INSERT INTO semantic_revision_producers SELECT ?1,id,version,context_mode,context_trust,context_input_hash FROM semantic_revision_producers WHERE revision_id=?2",
            "INSERT INTO semantic_requested_producers SELECT ?1,id,version,executable_hash FROM semantic_requested_producers WHERE revision_id=?2",
            "INSERT INTO semantic_requested_producer_languages SELECT ?1,id,language,ordinal FROM semantic_requested_producer_languages WHERE revision_id=?2",
            "INSERT INTO semantic_revision_raw_selections SELECT ?1,producer_id,batch_id,captured_pin FROM semantic_revision_raw_selections WHERE revision_id=?2",
            "INSERT INTO semantic_import_reports SELECT ?1,producer_id,report_json FROM semantic_import_reports WHERE revision_id=?2",
        ] {
            tx.execute(sql, params![key, old])?;
        }
        copy_semantic_rows(&tx, &old, &key, true)?;
        let mut blocked = ReadersBlockedWatch::new(&self.publication_gate, 0);
        #[cfg(feature = "test-support")]
        let mut observer = self.semantic_publication_observer.lock().unwrap().take();
        #[cfg(feature = "test-support")]
        if let Some(observer) = observer.as_mut() {
            observer(crate::store::SemanticPublicationStage::BeforeSemantic, &tx);
        }
        for output in prepared.iter_mut() {
            check_cancel(cancel)?;
            match &mut output.output {
                PreparedOutput::Joined { admitted, .. } => {
                    let parts = admitted.join_parts()?;
                    if let Some(batch) = output.capture_witness {
                        super::captured_witness::publish_staged(&tx, parts.conn, batch)?;
                    }
                    crate::semantic_import::read_staged_documents(
                        parts.conn,
                        "",
                        &output.producer,
                        &mut |document| {
                            check_cancel(cancel)?;
                            write_import_coverage(&tx, &key, &document)?;
                            blocked.check(&tx);
                            #[cfg(feature = "test-support")]
                            if let Some(observer) = observer.as_mut() {
                                observer(
                                    crate::store::SemanticPublicationStage::AfterDocument,
                                    &tx,
                                );
                            }
                            Ok(())
                        },
                    )?;
                    context::publish(
                        self,
                        &tx,
                        &key,
                        &output.producer.id,
                        parts.descriptor,
                        cancel,
                    )?;
                }
                PreparedOutput::Unavailable { diagnostic } => {
                    tx.execute("INSERT INTO semantic_coverage(revision_id,producer_id,producer_version,language,path,requested,selected,state,diagnostic)
                        SELECT ?1,p.id,p.version,d.language,d.path,COALESCE(c.requested,0),COALESCE(c.requested,0),CASE WHEN c.requested=1 THEN 'failed' ELSE 'notRequested' END,CASE WHEN c.requested=1 THEN ?4 ELSE NULL END
                        FROM revision_documents d JOIN semantic_revision_producers p ON p.revision_id=?1 AND p.id=?3 JOIN semantic_producer_languages l ON l.id=p.id AND l.version=p.version AND l.language=d.language
                        LEFT JOIN semantic_projection_origins o ON o.revision_id=?2 AND o.producer_id=p.id
                        LEFT JOIN semantic_coverage c ON c.revision_id=o.basis_pin AND c.producer_id=p.id AND c.language=d.language AND c.path=d.path WHERE d.revision_id=?1",params![key,old,output.producer.id,diagnostic.as_str()])?;
                }
            }
        }
        // Producers without retained raw input keep their explicit P view, not missing tuples.
        tx.execute("INSERT INTO semantic_projection_origins SELECT ?1,o.producer_id,o.basis_pin FROM semantic_projection_origins o WHERE o.revision_id=?2 AND NOT EXISTS(SELECT 1 FROM semantic_pending_selections s WHERE s.revision_id=?2 AND s.producer_id=o.producer_id)",params![key,old])?;
        tx.execute("INSERT INTO semantic_projection_documents SELECT ?1,d.producer_id,d.language,d.path,d.pending FROM semantic_projection_documents d JOIN semantic_projection_origins o ON o.revision_id=?1 AND o.producer_id=d.producer_id WHERE d.revision_id=?2",params![key,old])?;
        index_dependencies(&tx, &key)?;
        semantic_retention::copy(&tx, &old, &key, None)?;
        // A producer S re-admitted no longer needs a re-import.
        for output in prepared.iter() {
            if matches!(output.output, PreparedOutput::Joined { .. }) {
                tx.execute(
                    "DELETE FROM semantic_reimport WHERE revision_id=?1 AND producer=?2",
                    params![key, output.producer.id],
                )?;
            }
        }
        search::publish_head(&tx, &key)?;
        let published_at = self.retention_time()?;
        stamp_predecessor(&tx, &old, published_at.as_secs() as i64)?;
        tx.execute(
            "INSERT INTO semantic_catchup_commits VALUES(?1,?2)",
            params![key, old],
        )?;
        let updated = tx.execute("UPDATE index_metadata SET index_revision=?1,indexed_at=?2 WHERE singleton=1 AND index_generation=?3 AND index_revision=?4",params![next.index_revision as i64,published_at.as_millis().to_string(),target.pin.index_generation.to_string(),target.pin.index_revision as i64])?;
        ensure!(updated == 1, "revision conflict: semantic target changed");
        let cleared = tx.execute("DELETE FROM semantic_pending_target WHERE revision_id=?1 AND native_revision_id=?2 AND root_id=?3",params![old,target.native_revision_id,target.root_id])?;
        ensure!(cleared == 1, "revision conflict: semantic target changed");
        leader.verify()?;
        self.identity.verify()?;
        check_cancel(cancel)?;
        let committed = storage_result(tx.commit());
        Self::semantic_phase_diagnostic("commit", "end");
        match committed {
            Ok(()) => Ok(Some(next)),
            Err(error) => {
                drop(db);
                // The transaction may have committed despite an I/O reporting failure.
                let durable = self.cache()?;
                let recorded: bool = durable.query_row("SELECT EXISTS(SELECT 1 FROM semantic_catchup_commits WHERE revision_id=?1 AND target_revision_id=?2)",params![key,old],|r|r.get(0))?;
                if recorded { Ok(Some(next)) } else { Err(error) }
            }
        }
    }
}

/// A bounded one-document overlay read. The tuple and every target retain P authority.
pub(crate) fn projected_semantic_document(
    db: &Connection,
    pin: IndexPin,
    producer: &str,
    language: &str,
    path: &str,
) -> Result<
    Option<(
        SemanticProjection,
        Option<crate::semantic_evidence::SemanticSnapshot>,
    )>,
> {
    let Some(projection) = semantic_projection_for(db, pin, producer, language, path)? else {
        return Ok(None);
    };
    if projection.semantic_status.is_pending() {
        return Ok(Some((projection, None)));
    }
    let (_, tuple, _) = projection_tuple(db, pin, producer, language, path)?
        .context("missing eligible projection")?;
    let rows = read_semantic_tuple(
        db,
        &semantic_pin_key(projection.semantic_basis_pin),
        Some(tuple),
        None,
        true,
    )?;
    Ok(Some((projection, Some(rows))))
}

impl Store {
    /// Check selected semantic integrity without materializing an unlabelled mixed-basis tree.
    pub(crate) fn validate_semantic_read_at(&self, pin: IndexPin) -> Result<()> {
        self.native_at(pin, |db, selected| {
            context::validate(db, &selected.key)?;
            if let Some(target) = pending_target(db)? {
                selected_ensure!(
                    target.root_id == self.identity.root_key,
                    "incompatible_index: pending semantic root differs"
                );
            }
            let mut statement = db.prepare(
                "SELECT p.id,d.language,d.path FROM semantic_revision_producers p
                 JOIN semantic_producer_languages l ON l.id=p.id AND l.version=p.version
                 JOIN revision_documents d ON d.revision_id=p.revision_id AND d.language=l.language
                 WHERE p.revision_id=?1 ORDER BY p.id,d.language,d.path",
            )?;
            let mut rows = statement.query([&selected.key])?;
            while let Some(row) = rows.next()? {
                let producer: String = row.get(0)?;
                let language: String = row.get(1)?;
                let path: String = row.get(2)?;
                if projected_semantic_document(db, pin, &producer, &language, &path)?.is_some() {
                    continue;
                }
                let tuple: Option<i64> = db
                    .query_row(
                        "SELECT tuple_id FROM semantic_coverage
                     WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",
                        params![selected.key, producer, language, path],
                        |row| row.get(0),
                    )
                    .optional()?;
                let tuple = tuple.context(SelectedIntegrity(
                    "incompatible_index: selected semantic tuple missing".into(),
                ))?;
                read_semantic_tuple(db, &selected.key, Some(tuple), None, true)?;
            }
            Ok(())
        })
    }

    pub fn projected_semantic_document_at(
        &self,
        pin: IndexPin,
        producer: &str,
        document: &crate::native_evidence::DocumentKey,
    ) -> Result<
        Option<(
            SemanticProjection,
            Option<crate::semantic_evidence::SemanticSnapshot>,
        )>,
    > {
        self.native_at(pin, |db, _| {
            projected_semantic_document(db, pin, producer, &document.language, &document.path)
        })
    }
}
