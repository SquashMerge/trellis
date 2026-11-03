//! Bounded original-admission authority for LOCAL source-association reuse.
//! Every call uses the owner's fresh exact-target snapshot; no SQLite handle survives it.
use super::*;
use crate::native_evidence::{DocumentKey, Range};
use crate::scip_raw::RawProofIdentity;
use crate::semantic_batch::{
    BatchLimits,
    descriptor::{Descriptor, SourceManifestHasher},
};
use crate::semantic_evidence as sem;

pub(crate) type Reserve<'a> = dyn FnMut(u64) -> Result<()> + 'a;

pub(crate) struct SourceAuthority {
    pub batch_id: i64,
    pub capture_id: i64,
    pub artifact_hash: String,
    pub captured_pin: IndexPin,
    pub original_pin: IndexPin,
    pub producer: sem::Producer,
    pub descriptor_sha256: String,
    pub decoder_version: String,
    pub source_set_id: String,
    pub captured_revision_id: String,
    pub source_manifest_hash: String,
    pub toolchain_hash: String,
    pub config_hash: String,
    pub dependency_hash: String,
    pub descriptor: Descriptor,
    target: PendingSemanticTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SelectedVersion {
    pub vid: i64,
    pub version_id: String,
    pub content_hash: String,
    pub byte_length: u64,
    pub extraction_context: String,
    pub producer_id: String,
    pub producer_version: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalSourceWitness {
    pub tuple_id: i64,
    pub document: DocumentKey,
    pub captured_version: SelectedVersion,
    pub current_version: SelectedVersion,
    pub source_hash: String,
    pub wire_digest: Option<String>,
    pub capped: Option<i64>,
    pub requested: bool,
    pub selected: bool,
    pub state: sem::CoverageState,
    pub diagnostic: Option<String>,
    pub reasons: Option<sem::CoverageReasons>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CoverageInput {
    pub document: DocumentKey,
    pub current: SelectedVersion,
    pub requested: bool,
    pub selected: bool,
    pub exclusion: Option<String>,
}

pub(crate) enum WitnessEligibility {
    Witness(OriginalSourceWitness),
    ChangedBytes(CoverageInput),
    NewDocument(CoverageInput),
    Full(&'static str),
}

pub(crate) enum AuthorityStep {
    Progress,
    Eligible(Box<SourceAuthority>),
    Full(&'static str),
}

#[derive(Default)]
pub(crate) struct OriginalAdmissionCursor {
    authority: Option<SourceAuthority>,
    after_selection: i64,
    original: Option<(u64, IndexPin)>,
    inventory: (usize, String),
    manifest: Option<SourceManifestHasher>,
    source_count: u64,
    phase: u8,
}

#[derive(Default)]
pub(crate) struct OriginalAssociationCursor {
    phase: u8,
    after_id: i64,
    after_text: String,
    after_blob: Vec<u8>,
    parent: i64,
    parent_blob: Vec<u8>,
    child_phase: u8,
    child_ordinal: i64,
}

#[derive(Default)]
pub(crate) struct InternalSymbolCursor {
    tuple: i64,
    binding_ordinal: i64,
    binding: Option<i64>,
    symbol_ordinal: i64,
    started: bool,
    pub done: bool,
}

pub(crate) struct OriginalPage {
    pub rows: Vec<OriginalRow>,
    pub done: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalBasis {
    pub basis_id: i64,
    pub producer_id: String,
    pub producer_version: String,
    pub producer_hash: String,
    pub artifact_hash: String,
    pub language: sem::Language,
    pub source_set_id: String,
    pub revision_id: String,
    pub source_manifest_hash: String,
    pub toolchain_hash: String,
    pub config_hash: String,
    pub dependency_hash: String,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalProvenance {
    pub id: String,
    pub content_hash: String,
    pub evidence_kind: sem::EvidenceKind,
    pub freshness: sem::Freshness,
    pub basis_id: i64,
    pub capture_id: i64,
    pub derived_from: sem::DerivedFrom,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalSymbol {
    pub row_id: i64,
    pub ordinal: i64,
    pub key: sem::SymbolKey,
    pub display_name: Option<String>,
    pub provenance_id: String,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalJoin {
    pub document: DocumentKey,
    pub revision_id: String,
    pub content_hash: String,
    pub range: Range,
    pub kind: sem::JoinKind,
    pub status: sem::JoinStatus,
    pub diagnostic: Option<String>,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalDeclaration {
    pub row_id: i64,
    pub ordinal: i64,
    pub syntax_id: Option<String>,
    pub provenance_id: String,
    pub join: OriginalJoin,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalReference {
    pub id: String,
    pub ordinal: i64,
    pub owner_syntax_id: String,
    pub range: Range,
    pub spelling: String,
    pub lookup_key: String,
    pub site: sem::ReferenceSite,
    pub resolution: sem::Resolution,
    pub declared_target: Option<sem::Target>,
    pub provenance_id: String,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalCall {
    pub row_id: i64,
    pub ordinal: i64,
    pub call_id: Option<String>,
    pub join: OriginalJoin,
    pub resolution: sem::Resolution,
    pub declared_target: Option<sem::Target>,
    pub dispatch: sem::Dispatch,
    pub possible_dispatch_complete: bool,
    pub stale_target: Option<bool>,
    pub provenance_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum OriginalRow {
    Coverage(sem::Coverage),
    CoverageRole {
        observed: bool,
        ordinal: i64,
        role: sem::Role,
    },
    Basis(OriginalBasis),
    BasisLookupDependency {
        basis_id: i64,
        ordinal: i64,
        key: String,
    },
    Provenance(OriginalProvenance),
    Symbol(OriginalSymbol),
    SymbolTarget {
        row_id: i64,
        ordinal: i64,
        target: sem::Target,
    },
    DeclarationBinding(OriginalDeclaration),
    DeclarationSymbol {
        row_id: i64,
        ordinal: i64,
        key: sem::SymbolKey,
    },
    DeclarationCandidate {
        row_id: i64,
        ordinal: i64,
        id: String,
    },
    Reference(OriginalReference),
    ReferenceRole {
        reference_id: String,
        ordinal: i64,
        role: sem::Role,
    },
    ReferenceCandidate {
        reference_id: String,
        ordinal: i64,
        target: sem::Target,
    },
    CallBinding(OriginalCall),
    CallJoinCandidate {
        row_id: i64,
        ordinal: i64,
        id: String,
    },
    CallCandidate {
        row_id: i64,
        ordinal: i64,
        target: sem::Target,
    },
    CallDispatch {
        row_id: i64,
        ordinal: i64,
        target: sem::Target,
    },
    TypeRelationship {
        ordinal: i64,
        row: sem::TypeRelationship,
    },
}

#[derive(Debug)]
pub(crate) struct CurrentDeclaration {
    pub version: SelectedVersion,
    pub syntax_id: String,
    pub kind: String,
    pub name: Option<String>,
    pub name_range: Option<Range>,
    /// Coarse declaration family only; not the semantic dispatch/callability policy.
    pub callable: bool,
}

fn read_join(row: &rusqlite::Row<'_>, offset: usize, source_set: &str) -> Result<OriginalJoin> {
    let start: i64 = row.get(offset + 4)?;
    let end: i64 = row.get(offset + 5)?;
    selected_ensure!(start >= 0 && end >= start, "invalid original join span");
    Ok(OriginalJoin {
        document: DocumentKey {
            source_set_id: source_set.to_owned(),
            language: row.get(offset)?,
            path: row.get(offset + 1)?,
        },
        revision_id: row.get(offset + 2)?,
        content_hash: row.get(offset + 3)?,
        range: Range {
            start: start as usize,
            end: end as usize,
        },
        kind: semantic_enum(row.get(offset + 6)?)?,
        status: semantic_enum(row.get(offset + 7)?)?,
        diagnostic: row.get(offset + 8)?,
    })
}
fn decoded_id(row: &rusqlite::Row<'_>, column: usize, call: bool) -> Result<String> {
    let bytes = row.get_ref(column)?.as_blob()?;
    if call {
        id_codec::decode_occ(bytes)
    } else {
        id_codec::decode_sid(bytes)
    }
    .map_err(|_| corrupt("invalid original semantic ID"))
}
fn optional_id(row: &rusqlite::Row<'_>, column: usize, call: bool) -> Result<Option<String>> {
    if matches!(row.get_ref(column)?, rusqlite::types::ValueRef::Null) {
        Ok(None)
    } else {
        decoded_id(row, column, call).map(Some)
    }
}
fn next_parent(cursor: &mut OriginalAssociationCursor, phase: u8) {
    cursor.phase = phase;
    cursor.after_id = -1;
    cursor.after_text.clear();
    cursor.after_blob.clear();
    cursor.parent = 0;
    cursor.child_phase = 0;
    cursor.child_ordinal = -1;
}

pub(crate) fn original_document_page(
    db: &Connection,
    authority: &SourceAuthority,
    witness: &OriginalSourceWitness,
    cursor: &mut OriginalAssociationCursor,
    limit: usize,
    reserve: &mut Reserve<'_>,
) -> Result<OriginalPage> {
    original_guard(db, authority)?;
    ensure!(limit > 0, "witness page limit must be positive");
    let mut result = Vec::new();
    // Transitions are units too: a document with empty families cannot hide an unbounded scan.
    for _ in 0..limit {
        if cursor.phase >= 10 {
            break;
        }
        if let Some(row) = association_unit(db, authority, witness, cursor, reserve)? {
            result.push(row);
        }
    }
    Ok(OriginalPage {
        rows: result,
        done: cursor.phase >= 10,
    })
}

fn association_unit(
    db: &Connection,
    a: &SourceAuthority,
    w: &OriginalSourceWitness,
    c: &mut OriginalAssociationCursor,
    reserve: &mut Reserve<'_>,
) -> Result<Option<OriginalRow>> {
    let source_set = &a.source_set_id;
    reserve(
        (source_set.len() as u64)
            .checked_mul(2)
            .ok_or_else(|| corrupt("witness source-set bytes"))?,
    )?;
    if c.phase == 0 {
        reserve(
            std::mem::size_of::<OriginalRow>() as u64
                + w.document.path.len() as u64
                + w.source_hash.len() as u64
                + w.diagnostic.as_ref().map_or(0, |s| s.len() as u64)
                + a.producer.id.len() as u64
                + a.captured_revision_id.len() as u64
                + source_set.len() as u64,
        )?;
        next_parent(c, 1);
        return Ok(Some(OriginalRow::Coverage(sem::Coverage {
            producer_id: a.producer.id.clone(),
            language: semantic_enum(w.document.language.clone())?,
            source_set_id: source_set.clone(),
            document_path: w.document.path.clone(),
            revision_id: a.captured_revision_id.clone(),
            requested: w.requested,
            selected: w.selected,
            state: w.state,
            supported_roles: Vec::new(),
            observed_roles: Vec::new(),
            diagnostic: w.diagnostic.clone(),
            reasons: w.reasons.clone(),
        })));
    }
    if c.phase == 1 {
        let observed = c.child_phase == 1;
        let mut stmt=db.prepare("SELECT ordinal,role FROM semantic_coverage_roles WHERE tuple_id=?1 AND kind=?2 AND ordinal>?3 ORDER BY ordinal LIMIT 1")?;
        let mut rows = stmt.query(params![
            w.tuple_id,
            if observed { "observed" } else { "supported" },
            c.child_ordinal
        ])?;
        if let Some(row) = rows.next()? {
            row_reserve(row, reserve)?;
            let ordinal = row.get(0)?;
            selected_ensure!(
                ordinal == c.child_ordinal + 1,
                "original coverage role ordinal gap"
            );
            c.child_ordinal = ordinal;
            return Ok(Some(OriginalRow::CoverageRole {
                observed,
                ordinal,
                role: semantic_enum(row.get(1)?)?,
            }));
        }
        if observed {
            next_parent(c, 2);
        } else {
            c.child_phase = 1;
            c.child_ordinal = -1;
        }
        return Ok(None);
    }
    if c.phase == 2 {
        if c.parent != 0 {
            let mut stmt=db.prepare("SELECT ordinal,lookup_key FROM semantic_basis_lookup_dependencies WHERE basis_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
            let mut rows = stmt.query(params![c.parent, c.child_ordinal])?;
            if let Some(row) = rows.next()? {
                row_reserve(row, reserve)?;
                let ordinal = row.get(0)?;
                selected_ensure!(
                    ordinal == c.child_ordinal + 1,
                    "original basis dependency ordinal gap"
                );
                c.child_ordinal = ordinal;
                return Ok(Some(OriginalRow::BasisLookupDependency {
                    basis_id: c.parent,
                    ordinal,
                    key: row.get(1)?,
                }));
            }
            c.parent = 0;
            c.child_ordinal = -1;
            return Ok(None);
        }
        let mut stmt=db.prepare("SELECT basis_id,producer_id,producer_version,producer_hash,artifact_hash,language,source_set_id,captured_native_revision_id,source_manifest_hash,toolchain_hash,config_hash,dependency_hash FROM semantic_basis WHERE tuple_id=?1 AND basis_id>?2 ORDER BY basis_id LIMIT 1")?;
        let mut rows = stmt.query(params![w.tuple_id, c.after_id])?;
        let Some(row) = rows.next()? else {
            next_parent(c, 3);
            return Ok(None);
        };
        row_reserve(row, reserve)?;
        let b = OriginalBasis {
            basis_id: row.get(0)?,
            producer_id: row.get(1)?,
            producer_version: row.get(2)?,
            producer_hash: row.get(3)?,
            artifact_hash: row.get(4)?,
            language: semantic_enum(row.get(5)?)?,
            source_set_id: row.get(6)?,
            revision_id: row.get(7)?,
            source_manifest_hash: row.get(8)?,
            toolchain_hash: row.get(9)?,
            config_hash: row.get(10)?,
            dependency_hash: row.get(11)?,
        };
        selected_ensure!(
            b.producer_id == a.producer.id
                && b.producer_version == a.producer.version
                && b.producer_hash == a.producer.executable_hash
                && b.artifact_hash == a.artifact_hash
                && b.source_set_id == a.source_set_id
                && b.revision_id == a.captured_revision_id
                && b.source_manifest_hash == a.source_manifest_hash
                && b.toolchain_hash == a.toolchain_hash
                && b.config_hash == a.config_hash
                && b.dependency_hash == a.dependency_hash
                && semantic_variant(&b.language)? == w.document.language,
            "original basis mismatch"
        );
        c.parent = b.basis_id;
        c.after_id = b.basis_id;
        return Ok(Some(OriginalRow::Basis(b)));
    }
    if c.phase == 3 {
        let mut stmt=db.prepare("SELECT provenance_id,content_hash,evidence_kind,freshness,basis_id,capture_id,derived_from_provenance_id,derived_from_record_kind,derived_from_fact_digest FROM semantic_provenance WHERE tuple_id=?1 AND provenance_id>?2 ORDER BY provenance_id LIMIT 1")?;
        let mut rows = stmt.query(params![w.tuple_id, c.after_text])?;
        let Some(row) = rows.next()? else {
            next_parent(c, 4);
            return Ok(None);
        };
        row_reserve(row, reserve)?;
        let capture: Option<i64> = row.get(5)?;
        let raw: Option<String> = row.get(6)?;
        let kind: Option<String> = row.get(7)?;
        let digest: Option<String> = row.get(8)?;
        let (Some(capture_id), Some(raw), Some(kind), Some(digest)) = (capture, raw, kind, digest)
        else {
            return Err(corrupt("original proof missing derivation"));
        };
        selected_ensure!(
            capture_id == a.capture_id && row.get_ref(1)?.as_str()? == w.source_hash,
            "original provenance source mismatch"
        );
        let membership:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id=?2 AND record_kind=?3 AND fact_digest=?4 AND language=?5 AND path=?6)",params![capture_id,raw,kind,digest,w.document.language,w.document.path],|r|r.get(0))?;
        selected_ensure!(membership, "original derivation identity mismatch");
        let proof = OriginalProvenance {
            id: row.get(0)?,
            content_hash: row.get(1)?,
            evidence_kind: semantic_enum(row.get(2)?)?,
            freshness: semantic_enum(row.get(3)?)?,
            basis_id: row.get(4)?,
            capture_id,
            derived_from: sem::DerivedFrom {
                provenance_id: raw,
                record_kind: semantic_enum(kind)?,
                fact_digest: digest,
            },
        };
        c.after_text = proof.id.clone();
        return Ok(Some(OriginalRow::Provenance(proof)));
    }
    if c.phase == 4 {
        if c.parent != 0 {
            let mut stmt=db.prepare("SELECT * FROM semantic_symbol_targets WHERE symbol_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
            let mut rows = stmt.query(params![c.parent, c.child_ordinal])?;
            if let Some(row) = rows.next()? {
                row_reserve(row, reserve)?;
                let ordinal: i64 = row.get(1)?;
                selected_ensure!(
                    ordinal == c.child_ordinal + 1,
                    "original symbol target ordinal gap"
                );
                c.child_ordinal = ordinal;
                let target = semantic_target_read(row, 2, source_set)?
                    .ok_or_else(|| corrupt("missing symbol target"))?;
                return Ok(Some(OriginalRow::SymbolTarget {
                    row_id: c.parent,
                    ordinal,
                    target,
                }));
            }
            c.parent = 0;
            c.child_ordinal = -1;
            return Ok(None);
        }
        let mut stmt=db.prepare("SELECT symbol_id,ordinal,key_scheme,key_symbol,key_scope,key_document_language,key_document_path,display_name,provenance_id FROM semantic_symbols WHERE tuple_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
        let mut rows = stmt.query(params![w.tuple_id, c.after_id])?;
        let Some(row) = rows.next()? else {
            next_parent(c, 5);
            return Ok(None);
        };
        row_reserve(row, reserve)?;
        let ordinal: i64 = row.get(1)?;
        selected_ensure!(ordinal == c.after_id + 1, "original symbol ordinal gap");
        let symbol = OriginalSymbol {
            row_id: row.get(0)?,
            ordinal,
            key: semantic_symbol_key_read(
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                source_set,
            )?,
            display_name: row.get(7)?,
            provenance_id: row.get(8)?,
        };
        c.parent = symbol.row_id;
        c.after_id = ordinal;
        return Ok(Some(OriginalRow::Symbol(symbol)));
    }
    if c.phase == 5 {
        if c.parent != 0 {
            return declaration_child(db, source_set, c, reserve);
        }
        let mut stmt=db.prepare("SELECT binding_id,ordinal,syntax_id,provenance_id,join_document_language,join_document_path,join_native_revision_id,join_content_hash,join_start,join_end,join_kind,join_status,join_diagnostic FROM semantic_declaration_bindings WHERE tuple_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
        let mut rows = stmt.query(params![w.tuple_id, c.after_id])?;
        let Some(row) = rows.next()? else {
            next_parent(c, 6);
            return Ok(None);
        };
        row_reserve(row, reserve)?;
        let ordinal: i64 = row.get(1)?;
        selected_ensure!(
            ordinal == c.after_id + 1,
            "original declaration ordinal gap"
        );
        let binding = OriginalDeclaration {
            row_id: row.get(0)?,
            ordinal,
            syntax_id: optional_id(row, 2, false)?,
            provenance_id: row.get(3)?,
            join: read_join(row, 4, source_set)?,
        };
        c.parent = binding.row_id;
        c.after_id = ordinal;
        return Ok(Some(OriginalRow::DeclarationBinding(binding)));
    }
    if c.phase == 6 {
        if !c.parent_blob.is_empty() {
            return reference_child(db, w.tuple_id, source_set, c, reserve);
        }
        let mut stmt=db.prepare("SELECT * FROM semantic_references WHERE tuple_id=?1 AND reference_id>?2 ORDER BY reference_id LIMIT 1")?;
        let mut rows = stmt.query(params![w.tuple_id, c.after_blob])?;
        let Some(row) = rows.next()? else {
            next_parent(c, 7);
            return Ok(None);
        };
        row_reserve(row, reserve)?;
        let start: i64 = row.get(4)?;
        let end: i64 = row.get(5)?;
        selected_ensure!(
            start >= 0 && end >= start,
            "invalid original reference range"
        );
        let reference = OriginalReference {
            id: decoded_id(row, 1, true)?,
            ordinal: row.get(2)?,
            owner_syntax_id: decoded_id(row, 3, false)?,
            range: Range {
                start: start as usize,
                end: end as usize,
            },
            spelling: row.get(6)?,
            lookup_key: row.get(7)?,
            site: semantic_enum(row.get(8)?)?,
            resolution: semantic_enum(row.get(9)?)?,
            declared_target: semantic_target_read(row, 10, source_set)?,
            provenance_id: row.get(20)?,
        };
        c.parent_blob = row.get(1)?;
        c.after_blob = c.parent_blob.clone();
        return Ok(Some(OriginalRow::Reference(reference)));
    }
    if c.phase == 7 {
        if c.parent != 0 {
            return call_child(db, source_set, c, reserve);
        }
        let mut stmt=db.prepare("SELECT * FROM semantic_call_bindings WHERE tuple_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
        let mut rows = stmt.query(params![w.tuple_id, c.after_id])?;
        let Some(row) = rows.next()? else {
            next_parent(c, 8);
            return Ok(None);
        };
        row_reserve(row, reserve)?;
        let ordinal: i64 = row.get(2)?;
        selected_ensure!(ordinal == c.after_id + 1, "original call ordinal gap");
        let binding = OriginalCall {
            row_id: row.get(0)?,
            ordinal,
            call_id: optional_id(row, 3, true)?,
            join: read_join(row, 4, source_set)?,
            resolution: semantic_enum(row.get(13)?)?,
            declared_target: semantic_target_read(row, 14, source_set)?,
            dispatch: semantic_enum(row.get(24)?)?,
            possible_dispatch_complete: row.get(25)?,
            stale_target: row.get(26)?,
            provenance_id: row.get(27)?,
        };
        c.parent = binding.row_id;
        c.after_id = ordinal;
        return Ok(Some(OriginalRow::CallBinding(binding)));
    }
    if c.phase == 8 {
        let mut stmt=db.prepare("SELECT * FROM semantic_type_relationships WHERE tuple_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
        let mut rows = stmt.query(params![w.tuple_id, c.after_id])?;
        let Some(row) = rows.next()? else {
            next_parent(c, 10);
            return Ok(None);
        };
        row_reserve(row, reserve)?;
        let ordinal: i64 = row.get(2)?;
        selected_ensure!(
            ordinal == c.after_id + 1,
            "original relationship ordinal gap"
        );
        c.after_id = ordinal;
        return Ok(Some(OriginalRow::TypeRelationship {
            ordinal,
            row: sem::TypeRelationship {
                kind: semantic_enum(row.get(3)?)?,
                source: semantic_target_read(row, 4, source_set)?
                    .ok_or_else(|| corrupt("missing relationship source"))?,
                target: semantic_target_read(row, 14, source_set)?
                    .ok_or_else(|| corrupt("missing relationship target"))?,
                provenance_id: row.get(24)?,
            },
        }));
    }
    Err(corrupt("invalid original association cursor"))
}

fn declaration_child(
    db: &Connection,
    source_set: &str,
    c: &mut OriginalAssociationCursor,
    reserve: &mut Reserve<'_>,
) -> Result<Option<OriginalRow>> {
    if c.child_phase == 0 {
        let mut stmt=db.prepare("SELECT ordinal,scheme,symbol,scope,document_language,document_path FROM semantic_declaration_binding_symbols WHERE binding_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
        let mut rows = stmt.query(params![c.parent, c.child_ordinal])?;
        if let Some(row) = rows.next()? {
            row_reserve(row, reserve)?;
            let ordinal: i64 = row.get(0)?;
            selected_ensure!(
                ordinal == c.child_ordinal + 1,
                "original declaration symbol ordinal gap"
            );
            c.child_ordinal = ordinal;
            return Ok(Some(OriginalRow::DeclarationSymbol {
                row_id: c.parent,
                ordinal,
                key: semantic_symbol_key_read(
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    source_set,
                )?,
            }));
        }
        c.child_phase = 1;
        c.child_ordinal = -1;
        return Ok(None);
    }
    let mut stmt=db.prepare("SELECT ordinal,candidate_id FROM semantic_declaration_join_candidates WHERE binding_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
    let mut rows = stmt.query(params![c.parent, c.child_ordinal])?;
    if let Some(row) = rows.next()? {
        row_reserve(row, reserve)?;
        let ordinal: i64 = row.get(0)?;
        selected_ensure!(
            ordinal == c.child_ordinal + 1,
            "original declaration candidate ordinal gap"
        );
        c.child_ordinal = ordinal;
        return Ok(Some(OriginalRow::DeclarationCandidate {
            row_id: c.parent,
            ordinal,
            id: decoded_id(row, 1, false)?,
        }));
    }
    c.parent = 0;
    c.child_phase = 0;
    c.child_ordinal = -1;
    Ok(None)
}
fn reference_child(
    db: &Connection,
    tuple: i64,
    source_set: &str,
    c: &mut OriginalAssociationCursor,
    reserve: &mut Reserve<'_>,
) -> Result<Option<OriginalRow>> {
    if c.child_phase == 0 {
        let mut stmt=db.prepare("SELECT ordinal,role FROM semantic_reference_roles WHERE tuple_id=?1 AND reference_id=?2 AND ordinal>?3 ORDER BY ordinal LIMIT 1")?;
        let mut rows = stmt.query(params![tuple, c.parent_blob, c.child_ordinal])?;
        if let Some(row) = rows.next()? {
            row_reserve(row, reserve)?;
            let ordinal: i64 = row.get(0)?;
            selected_ensure!(
                ordinal == c.child_ordinal + 1,
                "original reference role ordinal gap"
            );
            c.child_ordinal = ordinal;
            return Ok(Some(OriginalRow::ReferenceRole {
                reference_id: id_codec::decode_occ(&c.parent_blob)
                    .map_err(|_| corrupt("invalid reference child ID"))?,
                ordinal,
                role: semantic_enum(row.get(1)?)?,
            }));
        }
        c.child_phase = 1;
        c.child_ordinal = -1;
        return Ok(None);
    }
    let mut stmt=db.prepare("SELECT * FROM semantic_reference_candidates WHERE tuple_id=?1 AND reference_id=?2 AND ordinal>?3 ORDER BY ordinal LIMIT 1")?;
    let mut rows = stmt.query(params![tuple, c.parent_blob, c.child_ordinal])?;
    if let Some(row) = rows.next()? {
        row_reserve(row, reserve)?;
        let ordinal: i64 = row.get(2)?;
        selected_ensure!(
            ordinal == c.child_ordinal + 1,
            "original reference candidate ordinal gap"
        );
        c.child_ordinal = ordinal;
        return Ok(Some(OriginalRow::ReferenceCandidate {
            reference_id: id_codec::decode_occ(&c.parent_blob)
                .map_err(|_| corrupt("invalid reference child ID"))?,
            ordinal,
            target: semantic_target_read(row, 3, source_set)?
                .ok_or_else(|| corrupt("missing reference candidate"))?,
        }));
    }
    c.parent_blob.clear();
    c.child_phase = 0;
    c.child_ordinal = -1;
    Ok(None)
}
fn call_child(
    db: &Connection,
    source_set: &str,
    c: &mut OriginalAssociationCursor,
    reserve: &mut Reserve<'_>,
) -> Result<Option<OriginalRow>> {
    if c.child_phase == 0 {
        let mut stmt=db.prepare("SELECT ordinal,candidate_id FROM semantic_call_join_candidates WHERE binding_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
        let mut rows = stmt.query(params![c.parent, c.child_ordinal])?;
        if let Some(row) = rows.next()? {
            row_reserve(row, reserve)?;
            let ordinal: i64 = row.get(0)?;
            selected_ensure!(
                ordinal == c.child_ordinal + 1,
                "original call join candidate ordinal gap"
            );
            c.child_ordinal = ordinal;
            return Ok(Some(OriginalRow::CallJoinCandidate {
                row_id: c.parent,
                ordinal,
                id: decoded_id(row, 1, true)?,
            }));
        }
        c.child_phase = 1;
        c.child_ordinal = -1;
        return Ok(None);
    }
    // Only these two closed table names are selected by this cursor.
    let sql = if c.child_phase == 1 {
        "SELECT * FROM semantic_call_candidates WHERE binding_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1"
    } else {
        "SELECT * FROM semantic_call_possible_dispatch WHERE binding_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1"
    };
    let mut stmt = db.prepare(sql)?;
    let mut rows = stmt.query(params![c.parent, c.child_ordinal])?;
    if let Some(row) = rows.next()? {
        row_reserve(row, reserve)?;
        let ordinal: i64 = row.get(1)?;
        selected_ensure!(
            ordinal == c.child_ordinal + 1,
            "original call target ordinal gap"
        );
        c.child_ordinal = ordinal;
        let target = semantic_target_read(row, 2, source_set)?
            .ok_or_else(|| corrupt("missing call target"))?;
        return Ok(Some(if c.child_phase == 1 {
            OriginalRow::CallCandidate {
                row_id: c.parent,
                ordinal,
                target,
            }
        } else {
            OriginalRow::CallDispatch {
                row_id: c.parent,
                ordinal,
                target,
            }
        }));
    }
    if c.child_phase == 1 {
        c.child_phase = 2;
        c.child_ordinal = -1;
    } else {
        c.parent = 0;
        c.child_phase = 0;
        c.child_ordinal = -1;
    }
    Ok(None)
}

fn corrupt(message: &str) -> anyhow::Error {
    SelectedIntegrity(format!("incompatible_index: {message}")).into()
}
fn row_reserve(row: &rusqlite::Row<'_>, reserve: &mut Reserve<'_>) -> Result<()> {
    let mut bytes = (2 * std::mem::size_of::<OriginalRow>()) as u64;
    for index in 0..row.as_ref().column_count() {
        let length = match row.get_ref(index)? {
            rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
                value.len() as u64
            }
            _ => 0,
        };
        bytes = bytes
            .checked_add(
                length
                    .checked_mul(2)
                    .ok_or_else(|| corrupt("witness row size"))?,
            )
            .ok_or_else(|| corrupt("witness row size"))?;
    }
    reserve(bytes)
}
fn guard(db: &Connection, authority: &SourceAuthority) -> Result<()> {
    selected_ensure!(
        pending_target(db)?.as_ref() == Some(&authority.target),
        "witness target changed"
    );
    let selected: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_revision_raw_selections WHERE revision_id=?1 AND producer_id=?2 AND batch_id=?3 AND captured_pin=?4)",
        params![semantic_pin_key(authority.target.pin), authority.producer.id, authority.batch_id, semantic_pin_key(authority.captured_pin)], |r|r.get(0))?;
    selected_ensure!(selected, "witness raw selection changed");
    Ok(())
}

fn original_guard(db: &Connection, authority: &SourceAuthority) -> Result<()> {
    guard(db, authority)?;
    let available:bool=db.query_row("SELECT NOT EXISTS(SELECT 1 FROM native_revision_supersessions WHERE revision_id=?1 AND state<>'retained') AND EXISTS(SELECT 1 FROM semantic_revision_raw_selections WHERE revision_id=?1 AND producer_id=?2 AND batch_id=?3 AND captured_pin=?4)",params![semantic_pin_key(authority.original_pin),authority.producer.id,authority.batch_id,semantic_pin_key(authority.captured_pin)],|r|r.get(0))?;
    selected_ensure!(available, "original witness authority unavailable");
    Ok(())
}

pub(crate) fn selected_version(
    db: &Connection,
    pin: IndexPin,
    key: &DocumentKey,
) -> Result<Option<SelectedVersion>> {
    db.query_row(
        "SELECT m.vid,v.id,v.content_hash,v.byte_length,v.extraction_context,v.producer_id,v.producer_version
         FROM revision_documents m JOIN document_versions v ON v.vid=m.vid AND v.language=m.language AND v.path=m.path
         JOIN native_revisions r ON r.id=m.revision_id
         WHERE m.revision_id=?1 AND m.language=?2 AND m.path=?3 AND r.source_set_id=?4",
        params![semantic_pin_key(pin),key.language,key.path,key.source_set_id], |r| {
            Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?))
        }).optional()?.map(|(vid,version_id,content_hash,length,extraction_context,producer_id,producer_version)| {
            selected_ensure!(vid>0 && crate::scip_admission::hexhash(&content_hash) && crate::scip_admission::hexhash(&extraction_context), "invalid witness version metadata");
            Ok(SelectedVersion {vid,version_id,content_hash,byte_length:length.try_into().map_err(|_|corrupt("negative document length"))?,extraction_context,producer_id,producer_version})
        }).transpose()
}

pub(crate) fn source_authority(
    db: &Connection,
    target: &PendingSemanticTarget,
    selection: &PendingSemanticSelection,
    cursor: &mut OriginalAdmissionCursor,
    limits: &BatchLimits,
    cancel: &CancelFlag,
) -> Result<AuthorityStep> {
    check_cancel(cancel)?;
    selected_ensure!(
        pending_target(db)?.as_ref() == Some(target),
        "witness target changed"
    );
    if cursor.phase == 0 {
        let length: i64 = db.query_row(
            "SELECT length(descriptor_bytes) FROM semantic_raw_batches WHERE batch_id=?1",
            [selection.batch_id],
            |r| r.get(0),
        )?;
        if length < 0 || length as u64 > limits.descriptor_bytes {
            return Ok(AuthorityStep::Full("descriptor_bound"));
        }
        let (bytes, sha): (Vec<u8>, String) = db.query_row(
            "SELECT descriptor_bytes,descriptor_sha256 FROM semantic_raw_batches WHERE batch_id=?1",
            [selection.batch_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let descriptor = Descriptor::parse_admitted(&bytes, &sha, limits)?;
        let pinned = descriptor.check_producer()?;
        if descriptor.trust_origin != crate::scip_admission::REFERENCE_TRUST_ORIGIN
            || descriptor.artifacts.len() != 1
        {
            return Ok(AuthorityStep::Full("unsupported_witness_profile"));
        }
        descriptor.check_pages(limits)?;
        descriptor.check_artifacts(limits)?;
        let captured = Store::read_native_header(
            db,
            &ReadRevision::new(
                selection.captured_pin,
                semantic_pin_key(selection.captured_pin),
            ),
        )?;
        let current = Store::read_native_header(
            db,
            &ReadRevision::new(target.pin, semantic_pin_key(target.pin)),
        )?;
        // Project-root admission already ran; bind all immutable source/header components again.
        descriptor.bind_header(
            &[descriptor.metadata.project_root.clone()],
            &captured.source_set,
            &captured.revision,
        )?;
        if captured.producer != current.producer
            || captured.source_set != current.source_set
            || captured.revision.toolchain_hash != current.revision.toolchain_hash
            || captured.revision.config_hash != current.revision.config_hash
            || captured.revision.dependency_hash != current.revision.dependency_hash
        {
            return Ok(AuthorityStep::Full("witness_context_changed"));
        }
        let producer = pinned.producer();
        let producer_ok:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_revision_producers s JOIN semantic_producers p ON p.id=s.id AND p.version=s.version WHERE s.revision_id=?1 AND s.id=?2 AND p.version=?3 AND p.executable_hash=?4 AND p.kind='semantic' AND p.position_encoding=?5)",params![semantic_pin_key(target.pin),selection.producer_id,producer.version,producer.executable_hash,semantic_variant(&producer.position_encoding)?],|r|r.get(0))?;
        selected_ensure!(
            producer_ok && producer.id == selection.producer_id,
            "witness producer mismatch"
        );
        let metadata:bool=db.query_row("SELECT producer_id=?2 AND producer_version=?3 AND profile_id=?4 AND source_set_id=?5 AND captured_revision=?6 AND source_manifest_hash=?7 AND decoder_version=?8 FROM semantic_raw_batches WHERE batch_id=?1",
            params![selection.batch_id,producer.id,producer.version,descriptor.profile_id,descriptor.source_set.id,descriptor.source_revision,descriptor.source_manifest_hash,descriptor.decoder_version],|r|r.get(0))?;
        selected_ensure!(metadata, "witness batch metadata mismatch");
        let entry = &descriptor.artifacts[0];
        let (capture_id,valid):(i64,bool)=db.query_row("SELECT capture_id,ordinal=?2 AND language=?3 AND artifact_hash=?4 AND producer_id=?5 AND source_set_id=?6 AND captured_revision=?7 AND decoder_version=?8 AND length(binary_bytes)=?9 FROM semantic_raw_captures WHERE batch_id=?1 AND ordinal=?2",
            params![selection.batch_id,entry.ordinal,entry.language,entry.sha256,producer.id,descriptor.source_set.id,descriptor.source_revision,descriptor.decoder_version,i64::try_from(entry.byte_length)?],|r|Ok((r.get(0)?,r.get(1)?)))?;
        selected_ensure!(valid, "witness capture mismatch");
        let mut captures =
            db.prepare("SELECT capture_id FROM semantic_raw_captures WHERE batch_id=?1 LIMIT 2")?;
        let mut captures = captures.query([selection.batch_id])?;
        selected_ensure!(
            captures.next()?.is_some() && captures.next()?.is_none(),
            "witness artifact count mismatch"
        );
        cursor.manifest = Some(SourceManifestHasher::new(&descriptor.source_set.id));
        cursor.authority = Some(SourceAuthority {
            batch_id: selection.batch_id,
            capture_id,
            artifact_hash: entry.sha256.clone(),
            captured_pin: selection.captured_pin,
            original_pin: selection.captured_pin,
            producer,
            descriptor_sha256: sha,
            decoder_version: descriptor.decoder_version.clone(),
            source_set_id: descriptor.source_set.id.clone(),
            captured_revision_id: descriptor.source_revision.clone(),
            source_manifest_hash: descriptor.source_manifest_hash.clone(),
            toolchain_hash: captured.revision.toolchain_hash,
            config_hash: captured.revision.config_hash,
            dependency_hash: captured.revision.dependency_hash,
            descriptor,
            target: target.clone(),
        });
        cursor.phase = 1;
        return Ok(AuthorityStep::Progress);
    }
    let authority = cursor
        .authority
        .as_ref()
        .ok_or_else(|| corrupt("completed witness cursor"))?;
    guard(db, authority)?;
    if cursor.phase == 1 {
        let row:Option<(i64,String,i64,String,String)>=db.query_row("SELECT s.rowid,s.revision_id,r.published_index_revision,r.native_revision_id,r.source_set_id FROM semantic_revision_raw_selections s JOIN native_revisions r ON r.id=s.revision_id WHERE s.batch_id=?1 AND s.rowid>?2 AND s.producer_id=?3 AND s.captured_pin=?4 ORDER BY s.rowid LIMIT 1",
            params![authority.batch_id,cursor.after_selection,authority.producer.id,semantic_pin_key(authority.captured_pin)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        if let Some((rowid, key, revision, native, sources)) = row {
            cursor.after_selection = rowid;
            if native == authority.captured_revision_id && sources == authority.source_set_id {
                let pin = parse_pin(&key)?;
                let revision: u64 = revision
                    .try_into()
                    .map_err(|_| corrupt("negative original revision"))?;
                if cursor
                    .original
                    .as_ref()
                    .is_none_or(|(old, _)| revision < *old)
                {
                    cursor.original = Some((revision, pin));
                }
            }
            return Ok(AuthorityStep::Progress);
        }
        let Some((_, pin)) = cursor.original else {
            return Ok(AuthorityStep::Full("original_import_missing"));
        };
        let releasing:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM native_revision_supersessions WHERE revision_id=?1 AND state<>'retained')",[semantic_pin_key(pin)],|r|r.get(0))?;
        if releasing {
            return Ok(AuthorityStep::Full("original_import_releasing"));
        }
        // Controlled profiles need the owner's profile-input comparison, not just equal VID.
        let full:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_revision_producers WHERE revision_id=?1 AND id=?2 AND version=?3 AND context_mode='fullCaptured')",params![semantic_pin_key(pin),authority.producer.id,authority.producer.version],|r|r.get(0))?;
        if !full {
            return Ok(AuthorityStep::Full("controlled_profile_requires_full"));
        }
        cursor.authority.as_mut().unwrap().original_pin = pin;
        cursor.phase = 2;
        return Ok(AuthorityStep::Progress);
    }
    let authority = cursor.authority.as_ref().unwrap();
    if let Some(language) = authority
        .descriptor
        .source_set
        .languages
        .get(cursor.inventory.0)
    {
        let row:Option<(String,String)>=db.query_row("SELECT m.path,v.content_hash FROM revision_documents m JOIN document_versions v ON v.vid=m.vid WHERE m.revision_id=?1 AND m.language=?2 AND m.path>?3 ORDER BY m.path LIMIT 1",
            params![semantic_pin_key(authority.captured_pin),language,cursor.inventory.1],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((path, hash)) = row {
            cursor
                .manifest
                .as_mut()
                .unwrap()
                .push(language, &path, &hash);
            cursor.inventory.1 = path;
            cursor.source_count += 1;
            selected_ensure!(
                cursor.source_count <= authority.descriptor.totals.source_documents
                    && cursor.source_count <= limits.source_documents,
                "witness source inventory exceeds descriptor"
            );
        } else {
            cursor.inventory.0 += 1;
            cursor.inventory.1.clear();
        }
        return Ok(AuthorityStep::Progress);
    }
    selected_ensure!(
        cursor.source_count == authority.descriptor.totals.source_documents
            && cursor.manifest.take().unwrap().finish() == authority.source_manifest_hash,
        "witness source manifest mismatch"
    );
    cursor.phase = 3;
    Ok(AuthorityStep::Eligible(Box::new(
        cursor.authority.take().unwrap(),
    )))
}

fn has_nonderived_original_provenance(db: &Connection, tuple: i64) -> Result<bool> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_provenance WHERE tuple_id=?1 AND capture_id IS NULL AND derived_from_provenance_id IS NULL AND derived_from_record_kind IS NULL AND derived_from_fact_digest IS NULL)",[tuple],|row|row.get(0)).map_err(Into::into)
}

pub(crate) fn original_document_begin(
    db: &Connection,
    authority: &SourceAuthority,
    target: &PendingSemanticTarget,
    key: &DocumentKey,
) -> Result<WitnessEligibility> {
    original_guard(db, authority)?;
    selected_ensure!(
        target == &authority.target && key.source_set_id == authority.source_set_id,
        "witness source identity changed"
    );
    let language: sem::Language = semantic_enum(key.language.clone())?;
    if !authority.producer.languages.contains(&language) {
        return Ok(WitnessEligibility::Full("source_language_not_covered"));
    }
    let Some(current) = selected_version(db, target.pin, key)? else {
        return Ok(WitnessEligibility::Full("destination_source_missing"));
    };
    let captured = selected_version(db, authority.captured_pin, key)?;
    let raw:Option<(String,String,i64)>=db.query_row("SELECT source_hash,wire_digest,capped FROM semantic_raw_documents WHERE capture_id=?1 AND language=?2 AND path=?3",params![authority.capture_id,key.language,key.path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let exclusion:Option<String>=db.query_row("SELECT reason FROM semantic_raw_exclusions WHERE batch_id=?1 AND language=?2 AND path=?3",params![authority.batch_id,key.language,key.path],|r|r.get(0)).optional()?;
    let claims:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_raw_proofs WHERE capture_id=?1 AND language=?2 AND path=?3)",params![authority.capture_id,key.language,key.path],|r|r.get(0))?;
    let Some(captured) = captured else {
        selected_ensure!(
            raw.is_none() && exclusion.is_none() && !claims,
            "raw association outside captured inventory"
        );
        return Ok(WitnessEligibility::NewDocument(CoverageInput {
            document: key.clone(),
            current,
            requested: false,
            selected: false,
            exclusion: None,
        }));
    };
    if raw.is_none() && exclusion.is_none() {
        return Ok(WitnessEligibility::Full("raw_document_missing"));
    }
    selected_ensure!(
        raw.is_none() || exclusion.is_none(),
        "raw document both admitted and excluded"
    );
    let source_hash = raw
        .as_ref()
        .map_or_else(|| captured.content_hash.clone(), |row| row.0.clone());
    selected_ensure!(
        source_hash == captured.content_hash,
        "raw document source mismatch"
    );
    if let Some((_, digest, capped)) = &raw {
        selected_ensure!(
            crate::scip_admission::hexhash(digest) && (0..=3).contains(capped),
            "invalid raw document metadata"
        );
    }
    let requested = exclusion.as_deref() != Some("notRequested");
    if source_hash != current.content_hash {
        return Ok(WitnessEligibility::ChangedBytes(CoverageInput {
            document: key.clone(),
            current,
            requested,
            selected: requested,
            exclusion,
        }));
    }
    if captured != current {
        return Ok(WitnessEligibility::Full("selected_version_changed"));
    }
    let tuple:Option<(i64,bool,bool,String,Option<String>)>=db.query_row("SELECT tuple_id,requested,selected,state,diagnostic FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND producer_version=?3 AND language=?4 AND path=?5",params![semantic_pin_key(authority.original_pin),authority.producer.id,authority.producer.version,key.language,key.path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let Some((tuple_id, requested, selected, state, diagnostic)) = tuple else {
        return Ok(WitnessEligibility::Full("original_tuple_missing"));
    };
    context::validate_tuple(
        db,
        &semantic_pin_key(authority.original_pin),
        &authority.producer.id,
        tuple_id,
    )?;
    let state: sem::CoverageState = semantic_enum(state)?;
    let reasons = read_coverage_reasons(db, tuple_id)?;
    if has_nonderived_original_provenance(db, tuple_id)? {
        return Ok(WitnessEligibility::Full("original_provenance_not_derived"));
    }
    if reasons.as_ref().is_some_and(|r| r.omitted) {
        return Ok(WitnessEligibility::Full("original_associations_omitted"));
    }
    let bearing = matches!(
        state,
        sem::CoverageState::Complete | sem::CoverageState::Partial
    );
    if raw.is_some() && (!requested || !selected || !bearing) {
        return Ok(WitnessEligibility::Full("original_coverage_differs"));
    }
    if exclusion.is_some() {
        let authored: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM semantic_provenance WHERE tuple_id=?1)",
            [tuple_id],
            |r| r.get(0),
        )?;
        if bearing || claims || authored {
            return Ok(WitnessEligibility::Full("excluded_original_has_facts"));
        }
    }
    Ok(WitnessEligibility::Witness(OriginalSourceWitness {
        tuple_id,
        document: key.clone(),
        captured_version: captured,
        current_version: current,
        source_hash: source_hash.clone(),
        wire_digest: raw.as_ref().map(|r| r.1.clone()),
        capped: raw.map(|r| r.2),
        requested,
        selected,
        state,
        diagnostic,
        reasons,
    }))
}

pub(crate) fn raw_document_proof_identity_page(
    db: &Connection,
    authority: &SourceAuthority,
    key: &DocumentKey,
    after: (&str, &str),
    limit: usize,
    reserve: &mut Reserve<'_>,
) -> Result<Vec<RawProofIdentity>> {
    original_guard(db, authority)?;
    ensure!(limit > 0, "witness page limit must be positive");
    selected_ensure!(
        key.source_set_id == authority.source_set_id,
        "raw proof source set mismatch"
    );
    let mut stmt=db.prepare("SELECT p.proof_id,p.record_kind,p.fact_digest,p.language,p.path,c.artifact_hash,c.producer_id,c.source_set_id,c.captured_revision,c.decoder_version FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE p.capture_id=?1 AND p.language=?2 AND p.path=?3 AND (p.record_kind,p.fact_digest)>(?4,?5) ORDER BY p.record_kind,p.fact_digest LIMIT ?6")?;
    let mut rows = stmt.query(params![
        authority.capture_id,
        key.language,
        key.path,
        after.0,
        after.1,
        i64::try_from(limit)?
    ])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        row_reserve(row, reserve)?;
        selected_ensure!(
            row.get_ref(5)?.as_str()? == authority.artifact_hash
                && row.get_ref(6)?.as_str()? == authority.producer.id
                && row.get_ref(7)?.as_str()? == authority.source_set_id
                && row.get_ref(8)?.as_str()? == authority.captured_revision_id
                && row.get_ref(9)?.as_str()? == authority.decoder_version,
            "raw proof capture mismatch"
        );
        selected_ensure!(
            crate::scip_admission::hexhash(row.get_ref(0)?.as_str()?)
                && crate::scip_admission::hexhash(row.get_ref(2)?.as_str()?)
                && matches!(
                    row.get_ref(1)?.as_str()?,
                    "occurrence" | "symbolInformation" | "relationship"
                ),
            "invalid raw proof metadata"
        );
        result.push(RawProofIdentity {
            proof_id: row.get(0)?,
            record_kind: row.get(1)?,
            fact_digest: row.get(2)?,
            language: row.get(3)?,
            path: row.get(4)?,
            artifact_hash: row.get(5)?,
        });
    }
    Ok(result)
}

pub(crate) fn current_declaration(
    db: &Connection,
    pin: IndexPin,
    key: &DocumentKey,
    syntax_id: &str,
    reserve: &mut Reserve<'_>,
) -> Result<Option<CurrentDeclaration>> {
    let Some(version) = selected_version(db, pin, key)? else {
        return Ok(None);
    };
    let encoded =
        id_codec::encode_sid(syntax_id).ok_or_else(|| corrupt("invalid targeted syntax ID"))?;
    let mut stmt=db.prepare("SELECT kind,name,name_start,name_end,start_byte,end_byte FROM native_version_declarations WHERE vid=?1 AND syntax_id=?2 LIMIT 2")?;
    let mut rows = stmt.query(params![version.vid, encoded])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    row_reserve(row, reserve)?;
    let (kind, name, start, end, outer_start, outer_end): (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
    ) = (
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    );
    selected_ensure!(rows.next()?.is_none(), "duplicate targeted declaration");
    match (name, start, end) {
        (None, None, None) => Ok(Some(CurrentDeclaration {
            version,
            syntax_id: syntax_id.to_owned(),
            kind,
            name: None,
            name_range: None,
            callable: false,
        })),
        (Some(name), Some(start), Some(end)) => {
            selected_ensure!(
                outer_start >= 0
                    && start >= outer_start
                    && end > start
                    && outer_end >= end
                    && outer_end as u64 <= version.byte_length,
                "invalid targeted declaration span"
            );
            let callable = matches!(kind.as_str(), "function" | "method" | "constructor");
            Ok(Some(CurrentDeclaration {
                version,
                syntax_id: syntax_id.to_owned(),
                kind,
                name: Some(name),
                name_range: Some(Range {
                    start: start as usize,
                    end: end as usize,
                }),
                callable,
            }))
        }
        _ => Err(corrupt("malformed targeted declaration")),
    }
}

pub(crate) fn internal_symbols_page(
    db: &Connection,
    authority: &SourceAuthority,
    cursor: &mut InternalSymbolCursor,
    limit: usize,
    reserve: &mut Reserve<'_>,
) -> Result<Vec<sem::SymbolKey>> {
    original_guard(db, authority)?;
    ensure!(limit > 0, "witness page limit must be positive");
    let mut result = Vec::new();
    if cursor.done {
        return Ok(result);
    }
    if !cursor.started {
        cursor.binding_ordinal = -1;
        cursor.symbol_ordinal = -1;
        cursor.started = true;
    }
    // Each call advances at most `limit` rows/transitions, including empty parents.
    for _ in 0..limit {
        if let Some(binding) = cursor.binding {
            let mut stmt=db.prepare("SELECT ordinal,scheme,symbol,scope,document_language,document_path FROM semantic_declaration_binding_symbols WHERE binding_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
            let mut rows = stmt.query(params![binding, cursor.symbol_ordinal])?;
            if let Some(row) = rows.next()? {
                row_reserve(row, reserve)?;
                let ordinal: i64 = row.get(0)?;
                selected_ensure!(
                    ordinal == cursor.symbol_ordinal + 1,
                    "original scope symbol ordinal gap"
                );
                cursor.symbol_ordinal = ordinal;
                reserve(authority.source_set_id.len() as u64)?;
                result.push(semantic_symbol_key_read(
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    &authority.source_set_id,
                )?);
            } else {
                cursor.binding = None;
                cursor.symbol_ordinal = -1;
            }
        } else {
            let row:Option<(i64,i64)>=db.query_row("SELECT binding_id,ordinal FROM semantic_declaration_bindings WHERE tuple_id=?1 AND ordinal>?2 AND syntax_id IS NOT NULL AND join_status='exact' ORDER BY ordinal LIMIT 1",params![cursor.tuple,cursor.binding_ordinal],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((id, ordinal)) = row {
                cursor.binding = Some(id);
                cursor.binding_ordinal = ordinal;
            } else {
                let next:Option<i64>=db.query_row("SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND tuple_id>?3 ORDER BY tuple_id LIMIT 1",params![semantic_pin_key(authority.original_pin),authority.producer.id,cursor.tuple],|r|r.get(0)).optional()?;
                if let Some(tuple) = next {
                    cursor.tuple = tuple;
                    cursor.binding_ordinal = -1;
                } else {
                    cursor.done = true;
                    break;
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version_fixture() -> (Connection, IndexPin, DocumentKey) {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE native_revisions(id TEXT PRIMARY KEY,source_set_id TEXT);
            CREATE TABLE revision_documents(revision_id TEXT,language TEXT,path TEXT,vid INTEGER,PRIMARY KEY(revision_id,language,path));
            CREATE TABLE document_versions(vid INTEGER PRIMARY KEY,id TEXT,language TEXT,path TEXT,content_hash TEXT,byte_length INTEGER,extraction_context TEXT,producer_id TEXT,producer_version TEXT);
            CREATE TABLE native_version_declarations(vid INTEGER,syntax_id BLOB,kind TEXT,name TEXT,name_start INTEGER,name_end INTEGER,start_byte INTEGER,end_byte INTEGER,PRIMARY KEY(vid,syntax_id));").unwrap();
        let pin = IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 1,
        };
        let key = DocumentKey {
            source_set_id: "sources".into(),
            language: "javascript".into(),
            path: "a.js".into(),
        };
        db.execute(
            "INSERT INTO native_revisions VALUES(?1,'sources')",
            [semantic_pin_key(pin)],
        )
        .unwrap();
        db.execute("INSERT INTO document_versions VALUES(7,'version','javascript','a.js',?1,40,?2,'native','1')",params!["a".repeat(64),"b".repeat(64)]).unwrap();
        db.execute(
            "INSERT INTO revision_documents VALUES(?1,'javascript','a.js',7)",
            [semantic_pin_key(pin)],
        )
        .unwrap();
        (db, pin, key)
    }

    #[test]
    fn nonderived_original_provenance_requires_full_without_hiding_mixed_links() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE semantic_provenance(tuple_id INTEGER,capture_id INTEGER,derived_from_provenance_id TEXT,derived_from_record_kind TEXT,derived_from_fact_digest TEXT);
            INSERT INTO semantic_provenance VALUES(1,NULL,NULL,NULL,NULL),(2,9,'proof','occurrence','digest'),(3,9,NULL,NULL,NULL),(4,NULL,'proof',NULL,NULL);").unwrap();
        assert!(has_nonderived_original_provenance(&db, 1).unwrap());
        for tuple in [2, 3, 4, 5] {
            assert!(!has_nonderived_original_provenance(&db, tuple).unwrap());
        }
    }

    #[test]
    fn exact_version_link_and_target_absence_are_metadata_only() {
        let (db, pin, key) = version_fixture();
        let selected = selected_version(&db, pin, &key).unwrap().unwrap();
        assert_eq!(selected.vid, 7);
        assert_eq!(selected.extraction_context, "b".repeat(64));
        let sid = "sid:v1:11111111111111111111111111111111";
        let encoded = id_codec::encode_sid(sid).unwrap();
        assert!(
            current_declaration(&db, pin, &key, sid, &mut |_| Ok(()))
                .unwrap()
                .is_none()
        );
        db.execute(
            "INSERT INTO native_version_declarations VALUES(7,?1,'function',NULL,NULL,NULL,0,30)",
            [&encoded],
        )
        .unwrap();
        let mut refused =
            |_: u64| -> Result<()> { anyhow::bail!("target row reservation refused") };
        assert!(current_declaration(&db, pin, &key, sid, &mut refused).is_err());
        let unnamed = current_declaration(&db, pin, &key, sid, &mut |_| Ok(()))
            .unwrap()
            .unwrap();
        assert!(unnamed.name.is_none());
        db.execute("UPDATE native_version_declarations SET name='entry',name_start=9,name_end=14 WHERE vid=7",[]).unwrap();
        let named = current_declaration(&db, pin, &key, sid, &mut |_| Ok(()))
            .unwrap()
            .unwrap();
        assert_eq!(named.name.as_deref(), Some("entry"));
        assert_eq!(named.name_range, Some(Range { start: 9, end: 14 }));
        db.execute("UPDATE native_version_declarations SET name_end=41", [])
            .unwrap();
        assert!(
            current_declaration(&db, pin, &key, sid, &mut |_| Ok(()))
                .unwrap_err()
                .is::<SelectedIntegrity>()
        );
        let mut other = key;
        other.source_set_id = "other".into();
        assert!(selected_version(&db, pin, &other).unwrap().is_none());
    }

    #[test]
    fn declaration_children_page_independently_and_reserve_before_ownership() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE semantic_declaration_binding_symbols(binding_id INTEGER,ordinal INTEGER,scheme TEXT,symbol TEXT,scope TEXT,document_language TEXT,document_path TEXT,PRIMARY KEY(binding_id,ordinal));
            CREATE TABLE semantic_declaration_join_candidates(binding_id INTEGER,ordinal INTEGER,candidate_id BLOB,PRIMARY KEY(binding_id,ordinal));
            INSERT INTO semantic_declaration_binding_symbols VALUES(1,0,'scip','first','global',NULL,NULL),(1,1,'scip','second','document','javascript','a.js');").unwrap();
        let mut cursor = OriginalAssociationCursor {
            phase: 5,
            parent: 1,
            child_ordinal: -1,
            ..Default::default()
        };
        let mut refused = |_: u64| -> Result<()> { anyhow::bail!("reservation refused") };
        assert!(declaration_child(&db, "sources", &mut cursor, &mut refused).is_err());
        assert_eq!(cursor.child_ordinal, -1);
        let mut owned = 0u64;
        let mut reserve = |bytes: u64| -> Result<()> {
            owned += bytes;
            Ok(())
        };
        let first = declaration_child(&db, "sources", &mut cursor, &mut reserve)
            .unwrap()
            .unwrap();
        assert!(matches!(
            first,
            OriginalRow::DeclarationSymbol { ordinal: 0, .. }
        ));
        let second = declaration_child(&db, "sources", &mut cursor, &mut reserve)
            .unwrap()
            .unwrap();
        match second {
            OriginalRow::DeclarationSymbol {
                ordinal: 1, key, ..
            } => assert_eq!(key.document.unwrap().path, "a.js"),
            _ => panic!("second child not emitted"),
        }
        assert!(
            declaration_child(&db, "sources", &mut cursor, &mut reserve)
                .unwrap()
                .is_none()
        );
        assert_eq!(cursor.child_phase, 1);
        assert!(
            declaration_child(&db, "sources", &mut cursor, &mut reserve)
                .unwrap()
                .is_none()
        );
        assert_eq!(cursor.parent, 0);
        assert!(owned > 0);
    }
}
