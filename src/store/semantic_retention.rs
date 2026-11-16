//! Revision-owned recovery status. Readers only consult their admitted SQLite snapshot.
use super::*;
use crate::producer::{retention, state::StateDir};

// Only an exact retained selection on the same native executable belongs to ordinary S work.
const ORDINARY_RETAINED_PENDING: &str = "WITH ordinary_retained_pending AS (
    SELECT p.producer_id
    FROM semantic_pending_target t
    JOIN native_revisions n
      ON n.id=t.revision_id AND n.native_revision_id=t.native_revision_id
    JOIN semantic_pending_selections p ON p.revision_id=t.revision_id
    JOIN semantic_revision_raw_selections r
      ON r.revision_id=p.revision_id AND r.producer_id=p.producer_id
     AND r.batch_id=p.batch_id AND r.captured_pin=p.captured_pin
    JOIN semantic_projection_origins o
      ON o.revision_id=p.revision_id AND o.producer_id=p.producer_id
     AND o.basis_pin=p.basis_pin
    JOIN semantic_raw_batches b
      ON b.batch_id=r.batch_id AND b.producer_id=r.producer_id
    JOIN retained.retained_runs held
      ON held.producer=r.producer_id
     AND held.descriptor_sha256=b.descriptor_sha256
    JOIN revision_producer_bindings old_binding ON old_binding.revision_id=?1
    JOIN revision_producer_bindings new_binding ON new_binding.revision_id=?2
    WHERE t.revision_id=?2
      AND old_binding.producer_sha=new_binding.producer_sha
) ";

pub(super) fn semantic_reimport_at(
    db: &Connection,
    key: &str,
) -> Result<BTreeMap<String, SemanticReimport>> {
    let mut statement = db.prepare("SELECT language,producer,reason FROM semantic_reimport WHERE revision_id=?1 ORDER BY language,producer")?;
    let rows = statement
        .query_map([key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut reasons: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (language, reason) in rows {
        reasons.entry(language).or_default().push(reason);
    }
    Ok(reasons
        .into_iter()
        .map(|(language, reasons)| {
            (
                language,
                SemanticReimport::NeedsReimport {
                    reason: reasons.join("; "),
                },
            )
        })
        .collect())
}

pub(super) fn copy(db: &Connection, old: &str, new: &str, clear: Option<&str>) -> Result<()> {
    db.execute("INSERT INTO semantic_reimport SELECT ?1,producer,language,reason FROM semantic_reimport WHERE revision_id=?2 AND (?3 IS NULL OR producer<>?3)",params![new,old,clear])?;
    Ok(())
}

/// Record evidence discarded by a new native identity, not ordinary unavailable languages.
pub(super) fn discarded(
    db: &Connection,
    old: &str,
    new: &str,
    retention_attached: bool,
) -> Result<()> {
    let sql = "INSERT OR IGNORE INTO semantic_reimport
         SELECT DISTINCT ?2,prior.producer_id,prior.language,
         'native revision identity changed; prior semantic evidence cannot be carried over; import a current artifact'
         FROM semantic_coverage prior
         JOIN native_revisions previous ON previous.id=?1
         JOIN native_revisions current ON current.id=?2
         WHERE prior.revision_id=?1 AND prior.requested=1 AND prior.selected=1
         AND previous.native_revision_id<>current.native_revision_id
         AND NOT EXISTS(SELECT 1 FROM semantic_coverage selected
             WHERE selected.revision_id=?2 AND selected.producer_id=prior.producer_id
             AND selected.language=prior.language AND selected.selected=1)
         -- A retained raw selection the native-first S will re-admit is catching up (#620).
         AND NOT EXISTS(SELECT 1 FROM semantic_pending_selections pending
             WHERE pending.revision_id=?2 AND pending.producer_id=prior.producer_id)";
    let sql = if retention_attached {
        format!(
            "{ORDINARY_RETAINED_PENDING}{sql} AND NOT EXISTS(
                SELECT 1 FROM ordinary_retained_pending owned
                WHERE owned.producer_id=prior.producer_id)"
        )
    } else {
        sql.to_owned()
    };
    db.execute(&sql, params![old, new])?;
    Ok(())
}

/// The reboot-stable root identity a retention catalog is bound to.
pub(crate) fn retention_root_key(identity: &super::topology::WorkspaceIdentity) -> String {
    format!("{}:{}", identity.record_id, identity.inode)
}

/// The head's identity and which producers an automatic re-run is due for (#660 SD11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoRunHead {
    pub pin: IndexPin,
    pub native_revision_id: String,
    /// Producer ID to whether a re-run is due.
    pub due: BTreeMap<String, bool>,
}

// A re-import row of this producer, or a producer-less legacy row (#632), for one language.
// Any producer's import of the language clears a legacy row, so it is due for each of them.
const REIMPORT_FOR: &str = "SELECT EXISTS(SELECT 1 FROM semantic_reimport
    WHERE revision_id=?1 AND producer IN (?2,'') AND language=?3)";

const SELECTED_CAPTURE: &str = "SELECT b.captured_revision,s.captured_pin
    FROM semantic_revision_raw_selections s
    JOIN semantic_raw_batches b ON b.batch_id=s.batch_id AND b.producer_id=s.producer_id
    WHERE s.revision_id=?1 AND s.producer_id=?2";

// Added, changed or removed documents of one language: the symmetric difference over (path, version).
const DOCUMENTS_DIFFER: &str = "SELECT EXISTS(
        SELECT 1 FROM revision_documents h WHERE h.revision_id=?1 AND h.language=?3
        AND NOT EXISTS(SELECT 1 FROM revision_documents c
            WHERE c.revision_id=?2 AND c.language=?3 AND c.path=h.path AND c.vid=h.vid)
    ) OR EXISTS(
        SELECT 1 FROM revision_documents c WHERE c.revision_id=?2 AND c.language=?3
        AND NOT EXISTS(SELECT 1 FROM revision_documents h
            WHERE h.revision_id=?1 AND h.language=?3 AND h.path=c.path AND h.vid=c.vid)
    )";

impl Store {
    /// Which of `producers` (ID and languages) are due an automatic re-run at the head, read
    /// in one snapshot. Everything is keyed by the producer asked about (#660 AC5): it is due
    /// for a re-import row of its own, or a producer-less legacy row, on one of its languages,
    /// or when its own selected batch was captured at another native revision and one of its
    /// languages has different documents. Another producer's row or batch never makes it due.
    /// No selection and no row is not due; manifest-only changes never are.
    pub fn auto_run_due(&self, producers: &[(&str, &[&str])]) -> Result<AutoRunHead> {
        self.with_evidence(|db| {
            let pin = self.read_revision(db, None)?.pin;
            let key = semantic_pin_key(pin);
            let native_revision_id: String = db.query_row(
                "SELECT native_revision_id FROM native_revisions WHERE id=?1",
                [&key],
                |row| row.get(0),
            )?;
            let mut due = BTreeMap::new();
            for (producer, languages) in producers {
                let mut is_due = false;
                for language in languages.iter() {
                    is_due = db
                        .prepare_cached(REIMPORT_FOR)?
                        .query_row(params![key, producer, language], |row| row.get(0))?;
                    if is_due {
                        break;
                    }
                }
                let captured: Option<(String, String)> = if is_due {
                    None
                } else {
                    db.prepare_cached(SELECTED_CAPTURE)?
                        .query_row(params![key, producer], |row| Ok((row.get(0)?, row.get(1)?)))
                        .optional()?
                };
                if let Some((captured_revision, captured_pin)) = captured
                    && captured_revision != native_revision_id
                {
                    for language in languages.iter() {
                        is_due = db
                            .prepare_cached(DOCUMENTS_DIFFER)?
                            .query_row(params![key, captured_pin, language], |row| row.get(0))?;
                        if is_due {
                            break;
                        }
                    }
                }
                due.insert((*producer).to_string(), is_due);
            }
            Ok(AutoRunHead {
                pin,
                native_revision_id,
                due,
            })
        })
    }

    pub(crate) fn attach_retention(&self, db: &Connection) -> Result<StateDir> {
        let state = StateDir::open(&self.roots, &self.identity)?;
        // Never `st_dev`: APFS renumbers it across reboots (#811).
        retention::attach(db, &state, &retention_root_key(&self.identity))?;
        Ok(state)
    }

    pub(super) fn materialize_reimport(
        &self,
        db: &Connection,
        key: &str,
        state: Option<&StateDir>,
        recovery: bool,
        previous: Option<&str>,
    ) -> Result<()> {
        if let Some(state) = state {
            let slots = retention::slots(db)?;
            let published_or_pending_sql = format!(
                "{ORDINARY_RETAINED_PENDING}SELECT EXISTS(
                    SELECT 1 FROM semantic_coverage
                    WHERE revision_id=?2 AND producer_id=?3 AND selected=1
                ) OR EXISTS(
                    SELECT 1 FROM ordinary_retained_pending WHERE producer_id=?3
                )"
            );
            let configured = retention::configured_languages(state, &slots);
            for retention::RetainedRun {
                producer,
                languages,
                options,
                ..
            } in slots
            {
                let published_or_pending: bool = db.query_row(
                    &published_or_pending_sql,
                    params![previous, key, producer],
                    |row| row.get(0),
                )?;
                if published_or_pending {
                    continue;
                }
                // A failed attempt belongs to its native capture identity. Semantic-only
                // publications and unchanged worker cycles must not re-admit it again.
                if let Some(previous) = previous {
                    db.execute(
                        "UPDATE retained.retained_runs SET attempted_generation=NULL
                         WHERE producer=?1 AND EXISTS(
                             SELECT 1 FROM native_revisions old JOIN native_revisions current
                             WHERE old.id=?2 AND current.id=?3
                             AND old.native_revision_id<>current.native_revision_id)",
                        params![producer, previous, key],
                    )?;
                }
                let reason = retention::recovery_reason(db, key, &options)?;
                for language in languages {
                    db.execute(
                        "INSERT OR REPLACE INTO semantic_reimport VALUES(?1,?2,?3,?4)",
                        params![key, producer, language, reason],
                    )?;
                }
            }
            if recovery {
                let mut statement = db.prepare("SELECT DISTINCT language FROM revision_documents WHERE revision_id=?1 AND language NOT IN (SELECT language FROM semantic_reimport WHERE revision_id=?1)")?;
                let languages = statement
                    .query_map([key], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let covered = |language: &String| {
                    configured
                        .as_ref()
                        .is_none_or(|configured| configured.contains(language))
                };
                for language in languages.into_iter().filter(covered) {
                    db.execute(
                        "INSERT OR IGNORE INTO retained.legacy_languages VALUES(?1)",
                        [language],
                    )?;
                }
            }
            // A language no producer ever covered had no evidence to lose (#664): it is not
            // configured, so neither an older legacy row nor a carried-over one applies. Only
            // a complete inspection shows that: an incomplete one keeps every recorded row.
            if let Some(configured) = &configured {
                let mut statement = db.prepare(
                    "SELECT language FROM retained.legacy_languages
                     UNION SELECT language FROM semantic_reimport WHERE revision_id=?1 AND producer=''",
                )?;
                let legacy = statement
                    .query_map([key], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for language in legacy.iter().filter(|l| !configured.contains(*l)) {
                    db.execute(
                        "DELETE FROM retained.legacy_languages WHERE language=?1",
                        [language],
                    )?;
                    db.execute(
                        "DELETE FROM semantic_reimport WHERE revision_id=?1 AND producer='' AND language=?2",
                        params![key, language],
                    )?;
                }
            }
            db.execute("INSERT OR IGNORE INTO semantic_reimport SELECT ?1,'',language,'original run inputs were not retained before this upgrade; import a current artifact' FROM retained.legacy_languages", [key])?;
        } else if recovery {
            db.execute("INSERT OR IGNORE INTO semantic_reimport SELECT ?1,'',language,'retained run state is unavailable; import a current artifact' FROM revision_documents WHERE revision_id=?1 GROUP BY language", [key])?;
        }
        Ok(())
    }

    /// Only an already published, still-held original run can use retained recovery admission.
    /// A fresh producer run has no matching slot until its successful publication.
    pub(crate) fn is_retained_reimport(
        &self,
        options: &crate::store::requests::GeneratedImportOptions,
        producer: &str,
        destination: IndexPin,
    ) -> Result<bool> {
        if options.origin != crate::semantic_batch::origin::GenerationOrigin::TrellisRun {
            return Ok(false);
        }
        let db = self.cache_write()?;
        match self.attach_retention(&db) {
            Ok(_) => {}
            Err(error) if retention::unavailable_catalog(&error) => return Ok(false),
            Err(error) => return Err(error),
        }
        let needed: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM semantic_reimport WHERE revision_id=?1 AND producer=?2)",
            params![semantic_pin_key(destination), producer],
            |row| row.get(0),
        )?;
        Ok(needed
            && retention::slots(&db)?
                .iter()
                .any(|slot| slot.producer == producer && slot.options == *options))
    }

    /// Called by the single checkout worker after native reconciliation, before the FIFO.
    /// This is admission into the existing queue, not a second retry owner.
    pub(crate) fn enqueue_retained_runs(&self) -> Result<()> {
        let db = self.cache_write()?;
        match self.attach_retention(&db) {
            Ok(_) => {}
            Err(error) if nonterminal_storage_busy(&error) => return Err(error),
            Err(error) if retention::unavailable_catalog(&error) => return Ok(()),
            Err(error) => return Err(typed_storage_full(error)),
        }
        let pin = self.index_baseline()?;
        let key = semantic_pin_key(pin);
        let generation = pin.index_generation.to_string();
        let pending = self.pending_generation_ids()?;
        for retention::RetainedRun {
            producer,
            options,
            attempted_generation,
            ..
        } in retention::slots(&db)?
        {
            let needed: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_reimport WHERE revision_id=?1 AND producer=?2)",params![key,producer], |row| row.get(0))?;
            if !needed || attempted_generation.as_deref() == Some(&generation) {
                continue;
            }
            let present = retention::inputs_present(&options)?;
            if present
                && !retention::descriptor_admissible(&options, &self.batch_publication_limits())?
            {
                // An inadmissible retained descriptor is a missing input: its needs-reimport
                // row stays published, and the slot goes so it is never queued again (#811).
                // Releasing the hold lets cleanup strip the run's inputs.
                db.execute(
                    "DELETE FROM retained.retained_runs WHERE producer=?1",
                    [&producer],
                )?;
                continue;
            }
            if present
                && !pending
                    .iter()
                    .any(|(origin, id)| *origin == options.origin && *id == options.generation_id)
            {
                match self
                    .enqueue_generated_semantic_import(Path::new(self.workspace_root()), &options)
                {
                    Ok(_) => {}
                    Err(error) if nonterminal_storage_busy(&error) => return Err(error),
                    // Missing input is already published as needs-reimport. Do not retry it
                    // forever, and never rewrite the original captured identity.
                    Err(error)
                        if error
                            .to_string()
                            .starts_with("invalid_request: generation directory") => {}
                    Err(error) => return Err(error),
                }
            }
            db.execute(
                "UPDATE retained.retained_runs SET attempted_generation=?1 WHERE producer=?2",
                params![generation, producer],
            )?;
        }
        Ok(())
    }
}
