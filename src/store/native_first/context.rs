//! Pending eligibility belongs to a validated destination producer row, not raw capture C.
use super::*;
use crate::producer::{TrustProfile, profiles, worktree};

fn profile(
    producer: &str,
    mode: &str,
    trust: Option<&str>,
    hash: Option<&str>,
) -> Result<Option<profiles::Row>> {
    if mode == "fullCaptured" && trust.is_none() && hash.is_none() {
        return Ok(None);
    }
    selected_ensure!(
        mode == "controlledProfile" && hash.is_some_and(crate::scip_admission::hexhash),
        "incompatible_index: invalid semantic context scope"
    );
    let trust = match trust {
        Some("restricted") => TrustProfile::Restricted,
        Some("trusted") => TrustProfile::Trusted,
        _ => {
            return Err(SelectedIntegrity(
                "incompatible_index: invalid semantic context trust".into(),
            )
            .into());
        }
    };
    let target = profiles::Target::parse(producer, false).context(SelectedIntegrity(
        "incompatible_index: invalid semantic context producer".into(),
    ))?;
    profiles::row(target, trust).map(Some).map_err(|_| {
        SelectedIntegrity("incompatible_index: unsupported semantic context profile".into()).into()
    })
}

pub(super) fn validate_profiles(db: &Connection, key: &str) -> Result<()> {
    let mut statement = db.prepare("SELECT id,context_mode,context_trust,context_input_hash FROM semantic_revision_producers WHERE revision_id=?1")?;
    let mut rows = statement.query([key])?;
    while let Some(row) = rows.next()? {
        profile(
            &row.get::<_, String>(0)?,
            &row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?.as_deref(),
            row.get::<_, Option<String>>(3)?.as_deref(),
        )?;
    }
    Ok(())
}

pub(in crate::store) fn validate(db: &Connection, key: &str) -> Result<()> {
    validate_profiles(db, key)?;
    let missing: bool = db.query_row("SELECT EXISTS(
        SELECT 1 FROM semantic_basis b JOIN semantic_coverage c ON c.tuple_id=b.tuple_id
        LEFT JOIN semantic_projection_contexts x ON x.tuple_id=c.tuple_id
        LEFT JOIN semantic_revision_producers p ON p.revision_id=c.revision_id AND p.id=c.producer_id
        WHERE c.revision_id=?1 AND (x.tuple_id IS NULL OR p.id IS NULL))", [key], |r|r.get(0))?;
    selected_ensure!(
        !missing,
        "incompatible_index: semantic context association missing"
    );
    Ok(())
}

pub(in crate::store) fn validate_tuple(
    db: &Connection,
    key: &str,
    producer: &str,
    tuple: i64,
) -> Result<()> {
    let row: Option<(String, Option<String>, Option<String>)> = db.query_row(
        "SELECT context_mode,context_trust,context_input_hash FROM semantic_revision_producers WHERE revision_id=?1 AND id=?2",
        params![key,producer], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let (mode, trust, hash) = row.context(SelectedIntegrity(
        "incompatible_index: semantic context producer missing".into(),
    ))?;
    profile(producer, &mode, trust.as_deref(), hash.as_deref())?;
    let missing: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_basis WHERE tuple_id=?1)
        AND NOT EXISTS(SELECT 1 FROM semantic_projection_contexts WHERE tuple_id=?1)",
        [tuple],
        |r| r.get(0),
    )?;
    selected_ensure!(
        !missing,
        "incompatible_index: semantic context association missing"
    );
    Ok(())
}

fn input_hash(
    db: &Connection,
    key: &str,
    row: &profiles::Row,
    config: &[serde_json::Value],
    dependencies: &[serde_json::Value],
    cancel: &CancelFlag,
) -> Result<String> {
    worktree::context_hash(
        &profiles::rules(row),
        config,
        dependencies,
        &|path| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?1 AND path=?2)",
                params![key, path],
                |r| r.get(0),
            )?)
        },
        cancel,
    )
}

pub(in crate::store) fn publish(
    store: &Store,
    db: &Connection,
    key: &str,
    producer: &str,
    descriptor: &crate::semantic_batch::descriptor::Descriptor,
    cancel: &CancelFlag,
) -> Result<()> {
    let Some(row) = descriptor.context_profile()? else {
        let updated = db.execute("UPDATE semantic_revision_producers SET context_mode='fullCaptured',context_trust=NULL,context_input_hash=NULL WHERE revision_id=?1 AND id=?2",params![key,producer])?;
        selected_ensure!(
            updated == 1,
            "incompatible_index: semantic context producer missing"
        );
        return Ok(());
    };
    let pin = parse_pin(key)?;
    let (_, config, dependencies) =
        store.retained_native_components_from_db(db, &ReadRevision::new(pin, key.to_owned()))?;
    let hash = input_hash(db, key, &row, &config, &dependencies, cancel)?;
    let trust = match row.trust {
        TrustProfile::Restricted => "restricted",
        TrustProfile::Trusted => "trusted",
    };
    let updated = db.execute("UPDATE semantic_revision_producers SET context_mode='controlledProfile',context_trust=?3,context_input_hash=?4 WHERE revision_id=?1 AND id=?2",params![key,producer,trust,hash])?;
    selected_ensure!(
        updated == 1,
        "incompatible_index: semantic context producer missing"
    );
    Ok(())
}

pub(in crate::store) fn mark_pending(
    db: &Connection,
    key: &str,
    config: &[serde_json::Value],
    dependencies: &[serde_json::Value],
    cancel: &CancelFlag,
) -> Result<()> {
    let missing: bool = db.query_row(
        "SELECT EXISTS(
        SELECT 1 FROM semantic_projection_origins o
        JOIN semantic_coverage c ON c.revision_id=o.basis_pin AND c.producer_id=o.producer_id
        JOIN semantic_basis b ON b.tuple_id=c.tuple_id
        LEFT JOIN semantic_projection_contexts x ON x.tuple_id=c.tuple_id
        WHERE o.revision_id=?1 AND x.tuple_id IS NULL)",
        [key],
        |r| r.get(0),
    )?;
    selected_ensure!(
        !missing,
        "incompatible_index: semantic context association missing"
    );
    let mut statement = db.prepare("SELECT o.basis_pin,o.producer_id,p.context_mode,p.context_trust,p.context_input_hash FROM semantic_projection_origins o LEFT JOIN semantic_revision_producers p ON p.revision_id=o.basis_pin AND p.id=o.producer_id WHERE o.revision_id=?1 AND EXISTS(SELECT 1 FROM semantic_coverage c JOIN semantic_projection_contexts x ON x.tuple_id=c.tuple_id WHERE c.revision_id=o.basis_pin AND c.producer_id=o.producer_id) ORDER BY o.producer_id")?;
    let scopes = statement
        .query_map([key], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (basis, producer, mode, trust, recorded) in scopes {
        check_cancel(cancel)?;
        let mode = mode.context(SelectedIntegrity(
            "incompatible_index: semantic context producer missing".into(),
        ))?;
        let controlled = profile(&producer, &mode, trust.as_deref(), recorded.as_deref())?;
        let input_changed = if let Some(row) = controlled {
            let hash = input_hash(db, key, &row, config, dependencies, cancel)?;
            recorded.as_deref() != Some(hash.as_str())
        } else {
            db.query_row("SELECT p.config_hash<>n.config_hash OR p.dependency_hash<>n.dependency_hash FROM native_revisions p JOIN native_revisions n WHERE p.id=?1 AND n.id=?2",params![basis,key],|r|r.get(0))?
        };
        db.execute("UPDATE semantic_projection_documents AS q SET pending=1
            WHERE q.revision_id=?1 AND q.producer_id=?2 AND q.pending=0 AND EXISTS(
                SELECT 1 FROM semantic_coverage c JOIN semantic_projection_contexts x ON x.tuple_id=c.tuple_id
                JOIN native_revisions p ON p.id=c.revision_id JOIN native_revisions n ON n.id=?1
                WHERE c.revision_id=?3 AND c.producer_id=q.producer_id AND c.language=q.language AND c.path=q.path
                  AND (?4 OR p.toolchain_hash<>n.toolchain_hash))",params![key,producer,basis,input_changed])?;
    }
    Ok(())
}
