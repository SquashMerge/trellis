//! Admission and language policy over the retained runs (#632, #664, #811). Their
//! persistence, the published-run catalog the index writer attaches, is
//! `crate::store::producer_retention`.
use super::state::StateDir;
use crate::store::{producer_retention::RetainedRun, requests::GeneratedImportOptions};
use anyhow::Result;
use std::collections::BTreeSet;

/// Whether a present retained descriptor still passes check 1 (size, digest, closed grammar,
/// format and decoder version). Its bytes never change, so a failure is permanent: a
/// descriptor written before a grammar change (#800 renamed `language`) can never be
/// admitted again and counts as a missing input (#811). I/O errors stay errors.
pub(crate) fn descriptor_admissible(
    options: &GeneratedImportOptions,
    limits: &crate::semantic_batch::BatchLimits,
) -> Result<bool> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(&options.descriptor)?
        .take(limits.descriptor_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(
        crate::semantic_batch::descriptor::Descriptor::parse_admitted(
            &bytes,
            &options.descriptor_sha256,
            limits,
        )
        .is_ok(),
    )
}

/// The languages a producer covered in this checkout's durable producer state (#664): a
/// retained run, a successful `producer run` record or a configured pickup. Only these can
/// have had semantic evidence lost to an upgrade. `None` when any run record or the pickup
/// configuration could not be read: a failed read is not evidence that no producer ran, so
/// the caller keeps the recovery history it has.
pub(crate) fn configured_languages(
    state: &StateDir,
    slots: &[RetainedRun],
) -> Option<BTreeSet<String>> {
    use super::state::RunState;
    let mut languages: BTreeSet<String> = slots
        .iter()
        .flat_map(|slot| slot.languages.iter().cloned())
        .collect();
    let runs = state.complete_runs().ok()??;
    let done = runs
        .iter()
        .filter(|run| run.state == RunState::Done)
        .map(|run| run.producer.clone());
    let pickup = state
        .read_pickup_config()
        .ok()?
        .map(|config| config.entries)
        .unwrap_or_default();
    for producer in done.chain(pickup.into_iter().map(|entry| entry.producer)) {
        if let Some(run_languages) = crate::scip_admission::run_languages(&producer) {
            languages.extend(run_languages.iter().map(|language| (*language).to_owned()));
        }
    }
    Some(languages)
}
