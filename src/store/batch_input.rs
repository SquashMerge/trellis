//! The publication input of a semantic batch: the admitted batch, which owns the stage, and
//! the joined batch that publication takes. Storage defines them; admission
//! (`crate::semantic_batch`) and the staged join (`crate::semantic_import`) build them.
use crate::{
    batch_descriptor::Descriptor, model::IndexPin, producer_pin::PinnedProducer,
    semantic_batch::admission_proof::AdmissionProof, store::staging::Stage,
};
use anyhow::{Context, Result};
use rusqlite::Connection;

/// A batch whose staged join emitted only validated rows. Only the staged join builds one, and
/// publication takes it by reference, so nothing unvalidated can reach the index.
#[derive(Debug)]
pub struct JoinedBatch {
    captured: IndexPin,
    destination: IndexPin,
    destination_revision_id: String,
    descriptor_sha256: String,
    attempt: i64,
    report: serde_json::Value,
}

impl JoinedBatch {
    pub(crate) fn seal(
        captured: IndexPin,
        destination: IndexPin,
        destination_revision_id: String,
        descriptor_sha256: String,
        attempt: i64,
        report: serde_json::Value,
    ) -> Self {
        Self {
            captured,
            destination,
            destination_revision_id,
            descriptor_sha256,
            attempt,
            report,
        }
    }

    pub fn captured_pin(&self) -> IndexPin {
        self.captured
    }

    pub fn destination_pin(&self) -> IndexPin {
        self.destination
    }

    pub fn destination_revision_id(&self) -> &str {
        &self.destination_revision_id
    }

    pub fn descriptor_sha256(&self) -> &str {
        &self.descriptor_sha256
    }

    /// The stage join attempt this batch certified. Publication refuses it once a later
    /// attempt has replaced the stage's output.
    pub fn attempt(&self) -> i64 {
        self.attempt
    }

    pub fn report(&self) -> &serde_json::Value {
        &self.report
    }
}

/// A batch whose descriptor, pages and artifacts passed admission and whose claims are staged.
/// It owns the stage; dropping it removes the stage directory.
///
/// Invariant: an `AdmittedBatch` was built by batch admission, so nothing that skipped
/// admission can be staged for the index under this type. `new` is the only constructor: the
/// fields are private to this module, and nothing else here builds one. `new` consumes an
/// `AdmissionProof`, which only the `crate::semantic_batch` module tree can create, so the
/// compiler rejects a construction anywhere else in the crate, storage included. Admission
/// creates the proof at two sites, each inside its call of `new`: the end of `stage_batch`
/// (`semantic_batch/admit.rs`) and the `Done` phase of `RetainedStageContinuation`
/// (`semantic_batch/retained_continuation.rs`).
///
/// The invariant is about construction. The stage stays writable through `stage_mut` and
/// `join_parts`: witness preparation and the join write their own stage tables after admission.
pub struct AdmittedBatch {
    stage: Stage,
    descriptor: Descriptor,
    descriptor_sha256: String,
    producer: PinnedProducer,
    captured: IndexPin,
    reimport_destination: Option<String>,
}

impl AdmittedBatch {
    pub(crate) fn new(
        _proof: AdmissionProof,
        stage: Stage,
        descriptor: Descriptor,
        descriptor_sha256: String,
        producer: PinnedProducer,
        captured: IndexPin,
    ) -> Self {
        Self {
            stage,
            descriptor,
            descriptor_sha256,
            producer,
            captured,
            reimport_destination: None,
        }
    }

    pub fn stage(&self) -> &Stage {
        &self.stage
    }

    pub fn stage_mut(&mut self) -> &mut Stage {
        &mut self.stage
    }

    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    pub fn descriptor_sha256(&self) -> &str {
        &self.descriptor_sha256
    }

    pub fn producer(&self) -> &PinnedProducer {
        &self.producer
    }

    pub fn captured_pin(&self) -> IndexPin {
        self.captured
    }

    /// Compare the fully admitted original source pages with a retained-recovery destination.
    /// The descriptor, captured pin and captured native context remain unchanged.
    pub(crate) fn validate_reimport_destination(
        &mut self,
        store: &crate::store::Store,
        destination: IndexPin,
        cancel: &crate::model::CancelFlag,
    ) -> Result<String> {
        self.reimport_destination = None;
        let (captured_header, (captured_toolchain, captured_config, captured_dependencies)) = store
            .with_native_reader(self.captured, |reader| {
                Ok((reader.header()?, reader.retained_components()?))
            })?;
        store.with_native_reader(destination, |reader| {
            let header = reader.header()?;
            let mismatch = || anyhow::anyhow!("stale_basis: retained source pages no longer match");
            if header.source_set.id != self.descriptor.source_set.id
                || header.source_set.root_id != self.descriptor.source_set.root_id
                || header.source_set.languages != self.descriptor.source_set.languages
                || header.source_set.dependencies != self.descriptor.source_set.dependencies
            {
                return Err(mismatch());
            }
            let mut inventory = reader.inventory()?;
            let mut statement = self.stage.conn()?.prepare(
                "SELECT language,path,content_hash,byte_length FROM sources ORDER BY ord",
            )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    anyhow::bail!("index publication cancelled");
                }
                let document = inventory.next_document()?.ok_or_else(mismatch)?;
                if document.key.language != row.get::<_, String>(0)?
                    || document.key.path != row.get::<_, String>(1)?
                    || document.content_hash != row.get::<_, String>(2)?
                    || document.byte_length as i64 != row.get::<_, i64>(3)?
                {
                    return Err(mismatch());
                }
            }
            if inventory.next_document()?.is_some() {
                return Err(mismatch());
            }
            let (toolchain, config, dependencies) = reader.retained_components()?;
            let toolchain_matches = reimport_toolchain_matches(
                captured_toolchain,
                &captured_header.revision.toolchain_hash,
                toolchain,
                &header.revision.toolchain_hash,
            )?;
            let captured_producer = &captured_header.producer;
            let producer = &header.producer;
            if toolchain_matches
                && captured_config == config
                && captured_dependencies == dependencies
                && captured_producer.id == producer.id
                && captured_producer.version == producer.version
                && captured_producer.kind == producer.kind
                && captured_producer.position_encoding == producer.position_encoding
                && captured_producer.languages == producer.languages
            {
                self.reimport_destination = Some(header.revision.id.clone());
            }
            Ok(header.revision.id)
        })
    }

    /// The stage connection together with the admitted batch's read-only parts.
    pub(crate) fn join_parts(&mut self) -> Result<JoinParts<'_>> {
        let conn = self.stage.conn()?;
        Ok(JoinParts {
            conn,
            descriptor: &self.descriptor,
            descriptor_sha256: &self.descriptor_sha256,
            producer: &self.producer,
            reimport_destination: self.reimport_destination.as_deref(),
        })
    }
}

pub(crate) struct JoinParts<'a> {
    pub conn: &'a Connection,
    pub descriptor: &'a Descriptor,
    pub descriptor_sha256: &'a str,
    pub producer: &'a PinnedProducer,
    pub reimport_destination: Option<&'a str>,
}

/// Only Trellis's own executable identity is exempt during validated retained admission.
fn reimport_toolchain_matches(
    mut captured: serde_json::Value,
    captured_hash: &str,
    mut destination: serde_json::Value,
    destination_hash: &str,
) -> Result<bool> {
    for (component, expected) in [(&captured, captured_hash), (&destination, destination_hash)] {
        anyhow::ensure!(
            crate::native_ids::digest(b"", &crate::native_ids::canonical(component)) == expected,
            "incompatible_index: retained toolchain hash mismatch"
        );
    }
    captured
        .as_object_mut()
        .context("invalid captured toolchain")?
        .remove("nativeExecutable");
    destination
        .as_object_mut()
        .context("invalid destination toolchain")?
        .remove("nativeExecutable");
    Ok(captured == destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_toolchain_exempts_only_native_executable_after_hash_validation() {
        let captured = serde_json::json!({"nativeExecutable":"binary-a", "rootRustToolchainToml":null, "futureContext":"same"});
        let hash = |value: &serde_json::Value| {
            crate::native_ids::digest(b"", &crate::native_ids::canonical(value))
        };
        let mut destination = captured.clone();
        destination["nativeExecutable"] = serde_json::json!("binary-b");
        let matches = |destination: &serde_json::Value| {
            reimport_toolchain_matches(
                captured.clone(),
                &hash(&captured),
                destination.clone(),
                &hash(destination),
            )
            .unwrap()
        };
        assert!(matches(&destination));
        destination["rootRustToolchainToml"] = serde_json::json!({"hash":"changed-selector"});
        assert!(!matches(&destination));
        destination = captured.clone();
        destination["futureContext"] = serde_json::json!("changed");
        assert!(!matches(&destination));
        assert!(
            reimport_toolchain_matches(
                captured.clone(),
                &"0".repeat(64),
                destination.clone(),
                &hash(&destination)
            )
            .is_err()
        );
    }
}
