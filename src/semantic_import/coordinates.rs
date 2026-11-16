//! Occurrence endpoints, coordinate maps, the inventory index and document coordinates.
#[cfg(test)]
use super::work::complexity_work;
use crate::{
    native_evidence as native,
    scip_raw::{CapturedClaim, CapturedScipInventory},
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) type Position = (i32, i32);
pub(crate) type Endpoints = (Position, Position);
pub(super) type CoordinateMap = BTreeMap<Position, usize>;

pub(super) fn occurrence_endpoints(
    claim: &CapturedClaim,
    admitted_encoding: u8,
) -> Result<(u8, Endpoints)> {
    ensure!(
        claim.envelope.record_kind == "occurrence",
        "not an occurrence"
    );
    range_endpoints(
        &claim.envelope.raw["range"],
        claim.envelope.position_encoding,
        admitted_encoding,
    )
}

pub(super) fn range_endpoints(
    raw: &serde_json::Value,
    position_encoding: u8,
    admitted_encoding: u8,
) -> Result<(u8, Endpoints)> {
    let mut values = [0u64; 4];
    let length = match raw["encoding"]
        .as_str()
        .context("missing raw range encoding")?
    {
        "legacy" => {
            let source = raw["values"].as_array().context("missing legacy values")?;
            ensure!(matches!(source.len(), 3 | 4), "invalid raw range length");
            for (target, value) in values.iter_mut().zip(source) {
                *target = value.as_u64().context("invalid legacy value")?;
            }
            source.len()
        }
        "singleLine" => {
            values[..3].copy_from_slice(&[
                raw["line"].as_u64().context("invalid line")?,
                raw["startCharacter"].as_u64().context("invalid start")?,
                raw["endCharacter"].as_u64().context("invalid end")?,
            ]);
            3
        }
        "multiLine" => {
            values = [
                raw["startLine"].as_u64().context("invalid start line")?,
                raw["startCharacter"].as_u64().context("invalid start")?,
                raw["endLine"].as_u64().context("invalid end line")?,
                raw["endCharacter"].as_u64().context("invalid end")?,
            ];
            4
        }
        _ => anyhow::bail!("invalid raw range shape"),
    };
    let encoding = if position_encoding == 0 {
        admitted_encoding
    } else {
        position_encoding
    };
    ensure!(
        matches!(encoding, 1..=3) && encoding == admitted_encoding,
        "SCIP encoding differs from admission"
    );
    let endpoints = if length == 3 {
        (
            (i32::try_from(values[0])?, i32::try_from(values[1])?),
            (i32::try_from(values[0])?, i32::try_from(values[2])?),
        )
    } else {
        (
            (i32::try_from(values[0])?, i32::try_from(values[1])?),
            (i32::try_from(values[2])?, i32::try_from(values[3])?),
        )
    };
    Ok((encoding, endpoints))
}

/// Recognize the pinned producers' raw file/module markers, not a measured name anchor.
/// Admission decoded this claim and rejected any empty marker that does not name its document
/// under the authenticated package or project root (owner ruling 2026-10-29, #443), so the
/// marker shape suffices here.
fn module_definition(claim: &CapturedClaim) -> bool {
    claim.envelope.record_kind == "occurrence"
        && claim.envelope.raw["symbolRoles"]
            .as_u64()
            .is_some_and(|roles| roles & 1 != 0)
        && claim.envelope.raw["symbol"].as_str().is_some_and(|symbol| {
            crate::scip_raw::module_marker_shape(
                &claim.language,
                &claim.envelope.relative_path,
                symbol,
            )
        })
}

pub(super) fn mapped_range(
    claim: &CapturedClaim,
    endpoints: Endpoints,
    offsets: &CoordinateMap,
) -> Result<native::Range> {
    let start = *offsets
        .get(&endpoints.0)
        .context("missing SCIP start position")?;
    let end = *offsets
        .get(&endpoints.1)
        .context("missing SCIP end position")?;
    if start > end || (start == end && !module_definition(claim)) {
        return Err(crate::scip_raw::ScipCaptureError::InvalidArtifact(
            "empty or reversed SCIP range".into(),
        )
        .into());
    }
    Ok(native::Range { start, end })
}

/// Convert one standalone range. Import loops use CoordinateMaps instead.
pub fn occurrence_range(
    claim: &CapturedClaim,
    source: &[u8],
    admitted_encoding: u8,
) -> Result<native::Range> {
    let (encoding, endpoints) = occurrence_endpoints(claim, admitted_encoding)?;
    let text = std::str::from_utf8(source).context("non-UTF8 authenticated source")?;
    let offsets = crate::scip_raw::source_byte_offsets(
        text,
        encoding,
        &mut vec![endpoints.0, endpoints.1],
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )?;
    mapped_range(claim, endpoints, &offsets)
}

pub(super) struct CoordinateMaps {
    offsets: BTreeMap<(String, String, u8), CoordinateMap>,
}

impl CoordinateMaps {
    pub(super) fn new(
        inventory: &CapturedScipInventory,
        native: &native::Artifact,
        sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
        admitted_encoding: u8,
    ) -> Result<Self> {
        let mut positions: BTreeMap<(String, String, u8), Vec<Position>> = BTreeMap::new();
        for claim in &inventory.claims {
            if claim.envelope.record_kind != "occurrence" {
                continue;
            }
            for enclosing in [false, true] {
                if enclosing && claim.envelope.raw["enclosingRange"].is_null() {
                    continue;
                }
                let mut envelope_claim;
                let range_claim = if enclosing {
                    envelope_claim = claim.clone();
                    envelope_claim.envelope.raw["range"] =
                        claim.envelope.raw["enclosingRange"].clone();
                    &envelope_claim
                } else {
                    claim
                };
                let (encoding, endpoints) = occurrence_endpoints(range_claim, admitted_encoding)?;
                positions
                    .entry((
                        claim.language.clone(),
                        claim.envelope.relative_path.clone(),
                        encoding,
                    ))
                    .or_default()
                    .extend([endpoints.0, endpoints.1]);
            }
        }
        let mut offsets = BTreeMap::new();
        for ((language, path, encoding), mut positions) in positions {
            let document = native::DocumentKey {
                source_set_id: native.revision.source_set_id.clone(),
                language: language.clone(),
                path: path.clone(),
            };
            let bytes = sources
                .get(&document)
                .context("missing admitted claim document")?;
            let source = std::str::from_utf8(bytes).context("non-UTF8 authenticated source")?;
            #[cfg(test)]
            complexity_work(|work| work.coordinate_batches += 1);
            let map = crate::scip_raw::source_byte_offsets(
                source,
                encoding,
                &mut positions,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )?;
            offsets.insert((language, path, encoding), map);
        }
        Ok(Self { offsets })
    }

    pub(super) fn range(
        &self,
        claim: &CapturedClaim,
        admitted_encoding: u8,
    ) -> Result<native::Range> {
        let (encoding, endpoints) = occurrence_endpoints(claim, admitted_encoding)?;
        let offsets = self
            .offsets
            .get(&(
                claim.language.clone(),
                claim.envelope.relative_path.clone(),
                encoding,
            ))
            .context("missing SCIP coordinate map")?;
        mapped_range(claim, endpoints, offsets)
    }
}

pub(super) struct InventoryIndex<'a> {
    pub(super) claims: &'a [CapturedClaim],
    proofs: BTreeMap<(String, String, String), &'a CapturedClaim>,
    pub(super) by_proof: BTreeMap<String, &'a CapturedClaim>,
    pub(super) symbol_info: BTreeMap<(String, String, String), &'a CapturedClaim>,
    declared_symbols: BTreeSet<String>,
    information_symbols: BTreeSet<String>,
    /// Languages with a document admitted with no claims under
    /// `scip_raw::DOCUMENT_CAP_EXCEEDED` (#634).
    pub(super) excluded_languages: BTreeSet<String>,
}

impl<'a> InventoryIndex<'a> {
    pub(super) fn new(inventory: &'a CapturedScipInventory) -> Result<Self> {
        let mut index = Self {
            claims: &inventory.claims,
            proofs: BTreeMap::new(),
            by_proof: BTreeMap::new(),
            symbol_info: BTreeMap::new(),
            declared_symbols: BTreeSet::new(),
            information_symbols: BTreeSet::new(),
            excluded_languages: inventory
                .capped_documents
                .iter()
                .filter(|(_, cap)| cap.excludes())
                .map(|((language, _), _)| language.clone())
                .collect(),
        };
        for claim in &inventory.claims {
            #[cfg(test)]
            complexity_work(|work| work.raw_claims += 1);
            let key = (
                claim.captured_proof_id.clone(),
                claim.envelope.record_kind.clone(),
                claim.fact_digest.clone(),
            );
            if index.proofs.insert(key, claim).is_some() {
                return Err(crate::scip_raw::ScipCaptureError::NonUniqueProof.into());
            }
            index
                .by_proof
                .entry(claim.captured_proof_id.clone())
                .or_insert(claim);
            if let Some(symbol) = claim.envelope.raw["symbol"].as_str() {
                if claim.envelope.record_kind == "symbolInformation" {
                    index.information_symbols.insert(symbol.to_owned());
                    index
                        .symbol_info
                        .entry((
                            claim.language.clone(),
                            claim.envelope.relative_path.clone(),
                            symbol.to_owned(),
                        ))
                        .or_insert(claim);
                } else if claim.envelope.record_kind == "occurrence"
                    && claim.envelope.raw["symbolRoles"]
                        .as_u64()
                        .is_some_and(|roles| roles & 1 != 0)
                {
                    index.declared_symbols.insert(symbol.to_owned());
                }
            }
        }
        Ok(index)
    }

    pub(super) fn lookup(
        &self,
        proof: &str,
        kind: &str,
        digest: &str,
    ) -> Result<&'a CapturedClaim> {
        #[cfg(test)]
        complexity_work(|work| work.raw_lookups += 1);
        self.resolve(proof, kind, digest)
    }

    pub(super) fn resolve(
        &self,
        proof: &str,
        kind: &str,
        digest: &str,
    ) -> Result<&'a CapturedClaim> {
        self.proofs
            .get(&(proof.to_owned(), kind.to_owned(), digest.to_owned()))
            .copied()
            .ok_or_else(|| crate::scip_raw::ScipCaptureError::MissingProof.into())
    }
}

/// The per-encoding coordinate maps of one document's occurrence claims.
pub(super) fn document_coordinates<'c>(
    claims: impl IntoIterator<Item = &'c CapturedClaim>,
    source: &str,
    admitted_encoding: u8,
) -> Result<BTreeMap<u8, CoordinateMap>> {
    let mut positions: BTreeMap<u8, Vec<Position>> = BTreeMap::new();
    for claim in claims {
        if claim.envelope.record_kind != "occurrence" {
            continue;
        }
        for enclosing in [false, true] {
            if enclosing && claim.envelope.raw["enclosingRange"].is_null() {
                continue;
            }
            let raw = &claim.envelope.raw[if enclosing { "enclosingRange" } else { "range" }];
            let (encoding, endpoints) =
                range_endpoints(raw, claim.envelope.position_encoding, admitted_encoding)?;
            positions
                .entry(encoding)
                .or_default()
                .extend([endpoints.0, endpoints.1]);
        }
    }
    let mut maps = BTreeMap::new();
    for (encoding, mut positions) in positions {
        maps.insert(
            encoding,
            crate::scip_raw::source_byte_offsets(
                source,
                encoding,
                &mut positions,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )?,
        );
    }
    Ok(maps)
}
