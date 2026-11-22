//! Exact measured joins for authenticated, document-local SCIP claims.
#[cfg(test)]
use crate::scip_raw::{CapturedClaim, CapturedScipInventory};
use crate::semantic_records::rejoin_provenance_id;
use crate::{native_evidence as native, native_ids, semantic_evidence as semantic};
use anyhow::Result;
#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};

mod coordinates;
mod dispatch;
mod join;
mod join_store;
mod keys;
mod measured;
mod memory;
mod rejoin;
mod relationships;
mod report;
mod role_sites;
mod staged;
mod work;
pub use coordinates::occurrence_range;
use coordinates::{
    CoordinateMap, CoordinateMaps, InventoryIndex, document_coordinates, mapped_range,
    occurrence_endpoints, range_endpoints,
};
pub(crate) use coordinates::{Endpoints, Position};
use dispatch::resolves_to;
pub use join::{JoinedFacts, join_inventory};
#[cfg(test)]
use join::{build_join_index, join_into_facts};
use join::{discover_join_claim, emit_join_symbols, join_occurrence, settle_join_references};
use join_store::JoinStore;
#[cfg(test)]
use join_store::{DocumentJoinState, IndexedJoinStore, ProofIdentities};
use keys::scoped_key;
#[cfg(test)]
use keys::symbol_key;
pub use measured::MeasuredFacts;
pub(crate) use measured::compose_join;
use measured::{AnchorKey, ScopedKey};
#[cfg(test)]
use memory::{MemoryJoinSink, MemoryJoinStore};
#[cfg(feature = "test-support")]
pub(crate) use rejoin::empty_publication;
#[cfg(test)]
pub(crate) use rejoin::manifest_hash;
pub use rejoin::{DestinationMutations, RejoinResult, rejoin_inventory};
#[cfg(test)]
use relationships::{discover_join_methods, reserved_key_bytes};
use relationships::{
    discover_relationship_claims, emit_type_relationships, method_target_owner_matches,
};
pub(crate) use report::LanguageReasons;
#[cfg(test)]
use role_sites::{
    DirectBase, collect_direct_bases, direct_java_types, direct_script_types, reserve_role_span,
    source_role_sites_reserved,
};
use role_sites::{RoleSites, source_role_sites, source_role_sites_parsed};
pub(crate) use staged::JoinContinuation;
#[cfg(feature = "test-support")]
pub(crate) use staged::cursor_sql as staged_cursor_sql;
pub use staged::join_staged;
#[cfg(feature = "test-support")]
pub(crate) use staged::target_callable_sql as staged_target_callable_sql;
#[cfg(test)]
use work::{COMPLEXITY_WORK, ComplexityWork};
#[cfg(feature = "test-support")]
pub(crate) use work::{
    PublicationWork, note_copied_document, note_destination_document, note_destination_index_build,
    note_local_target_lookup, note_local_target_work, note_local_witness_outcome,
    note_original_raw_decode, note_original_raw_envelope_parse, note_original_source_hash,
    note_parsed_role_site_document, note_producer_attempt, note_published_copy,
    note_reused_document, note_selected_original_native_row,
    note_selected_original_raw_proof_bytes, note_selected_original_source_bytes,
    note_witness_proof_read, publication_work, publication_work_checkpoint, reset_publication_work,
    restore_publication_work,
};

/// How a call's exact bound target declaration can be called, measured from its native
/// declaration (owner rulings 2026-10-30, #470, and 2026-10-31, #557). Python and a Rust
/// `impl Trait for Type` method are not classified; they, like every other declaration, are
/// `Other`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum TargetCallable {
    #[default]
    Other = 0,
    /// A free function: a JavaScript `function` declaration or expression, a Rust `fn` that
    /// is not an `impl` or trait item, or a Python `def` that is not directly in a class body.
    FreeFunction = 1,
    /// A JavaScript class, or its `constructor` method, or a Python class.
    Constructor = 2,
    /// One fixed body: a Rust method of an inherent `impl`, or a Java `static`, `private` or
    /// `final` method.
    FixedMethod = 3,
    /// A declaration, not the runtime body: a Rust `trait` method, a JavaScript method other
    /// than `constructor`, any other Java method, or a Python `def` directly in a class body.
    DynamicMethod = 4,
}

impl TargetCallable {
    /// `parent` is the immediate parent declaration of a Rust `implementation` method.
    fn of(declaration: &native::Declaration, parent: Option<&native::Declaration>) -> Self {
        match (
            declaration.document.language.as_str(),
            declaration.kind.as_str(),
        ) {
            // A native Python method is a `function` whose immediate parent is a class (#977).
            ("python", "function")
                if declaration
                    .ancestors
                    .last()
                    .is_some_and(|parent| parent.kind == "type") =>
            {
                Self::DynamicMethod
            }
            ("javascript" | "typescript" | "rust" | "python", "function") => Self::FreeFunction,
            ("javascript" | "typescript" | "python", "type") => Self::Constructor,
            ("javascript" | "typescript", "method")
                if declaration.name.as_deref() == Some("constructor") =>
            {
                Self::Constructor
            }
            ("javascript" | "typescript", "method") => Self::DynamicMethod,
            // The only native `type` that holds a method is a trait.
            ("rust", "method")
                if declaration
                    .ancestors
                    .last()
                    .is_some_and(|parent| parent.kind == "type") =>
            {
                Self::DynamicMethod
            }
            // `impl Type` measures no trait base; `impl Trait for Type` stays unclassified.
            ("rust", "method")
                if parent.is_some_and(|parent| {
                    parent.kind == "implementation" && parent.header.bases.is_empty()
                }) =>
            {
                Self::FixedMethod
            }
            ("java", "method")
                if declaration.header.modifiers.iter().any(|modifier| {
                    matches!(modifier.as_str(), "static" | "private" | "final")
                }) =>
            {
                Self::FixedMethod
            }
            ("java", "method") => Self::DynamicMethod,
            _ => Self::Other,
        }
    }

    /// The immediate parent a Rust method's classification reads, which is only its
    /// `implementation`: a method's measured ancestors name the parent's identity input.
    fn parent<'s, S: JoinStore>(
        store: &'s S,
        declaration: &native::Declaration,
    ) -> Result<Option<std::borrow::Cow<'s, native::Declaration>>> {
        let document = &declaration.document;
        match declaration.ancestors.split_last() {
            Some((parent, ancestors))
                if document.language == "rust"
                    && declaration.kind == "method"
                    && parent.kind == "implementation" =>
            {
                let id = native_ids::syntax_id(&serde_json::json!({
                    "sourceSet": store.source_set_id(),
                    "path": document.path,
                    "language": document.language,
                    "ancestors": ancestors,
                    "declaration": parent,
                }))?;
                store.declaration_of(document, &id)
            }
            _ => Ok(None),
        }
    }

    pub(super) fn from_code(code: i64) -> Result<Self> {
        Ok(match code {
            0 => Self::Other,
            1 => Self::FreeFunction,
            2 => Self::Constructor,
            3 => Self::FixedMethod,
            4 => Self::DynamicMethod,
            _ => anyhow::bail!("corrupt staged target callable"),
        })
    }
}

/// A settled call binding's dispatch (#470): its join-time classification holds only while
/// the settled row still resolves, with a verified target, to the same internal declaration it
/// was classified against. Ambiguous, external, unresolved and stale outcomes are `unknown`.
pub(super) fn settled_dispatch(
    joined: semantic::Dispatch,
    joined_target: Option<&semantic::Target>,
    settled: &semantic::CallBinding,
) -> semantic::Dispatch {
    if resolves_to(joined_target, settled) {
        joined
    } else {
        semantic::Dispatch::Unknown
    }
}

/// A rejoined call's dispatch: the classification of whichever join, destination first then
/// captured, was made against the settled target. A rejoin can settle a call on a captured
/// target retargeted to the destination (its syntax ID verified unchanged) that the
/// destination join alone did not resolve; the call's document bytes are unchanged, so the
/// captured shape and target kind still hold. A method's `direct` is the exception: the facts
/// behind it, its `impl`'s bases and its modifiers, are outside its syntax ID, so it does not
/// survive without a destination classification (#557). `target_is_method` reads the
/// destination's declaration of the settled target.
///
/// `kept_for_read` says whether that method's document changed since the capture and the
/// caller writes a payload (#1279, Decision 0021 D1/D4 as amended 2026-11-21). The destination
/// classification is then missing only because of the change: once the document returns to its
/// captured bytes the destination branch gives the captured `direct` again. So the payload keeps
/// the `direct`, its writer marks the call capture-bound, and every read demotes it while the
/// document differs from the capture. The in-memory rejoin, the reference for reader-visible
/// evidence, never keeps it.
pub(super) fn rejoined_dispatch(
    destination: Option<(semantic::Dispatch, Option<&semantic::Target>)>,
    captured: Option<(semantic::Dispatch, Option<&semantic::Target>)>,
    settled: &semantic::CallBinding,
    target_is_method: impl FnOnce(&str, &native::DocumentKey) -> Result<bool>,
    kept_for_read: impl FnOnce(&native::DocumentKey) -> Result<bool>,
) -> Result<semantic::Dispatch> {
    if let Some((dispatch, target)) = destination
        && resolves_to(target, settled)
    {
        return Ok(dispatch);
    }
    let Some((dispatch, target)) = captured.filter(|(_, target)| resolves_to(*target, settled))
    else {
        return Ok(semantic::Dispatch::Unknown);
    };
    if dispatch == semantic::Dispatch::Direct
        && let Some(semantic::Target::Internal {
            syntax_id,
            document,
            ..
        }) = target
        && target_is_method(syntax_id, document)?
        && !kept_for_read(document)?
    {
        return Ok(semantic::Dispatch::Unknown);
    }
    Ok(dispatch)
}

#[cfg(test)]
#[path = "semantic_import_complexity_tests.rs"]
mod complexity_tests;
