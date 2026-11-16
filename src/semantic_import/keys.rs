//! Scoped symbol keys and declaration targets.
use super::{
    join_store::{self, GlobalJoinReads, JoinStore},
    measured::ScopedKey,
    staged,
};
use crate::{native_evidence as native, scip_raw::CapturedClaim, semantic_evidence as semantic};
use anyhow::Result;

pub(super) fn scoped_key(
    symbol: &str,
    document: &native::DocumentKey,
) -> (String, Option<native::DocumentKey>) {
    (
        symbol.to_owned(),
        symbol.starts_with("local ").then(|| document.clone()),
    )
}

pub(super) fn symbol_key(symbol: &str, document: &native::DocumentKey) -> semantic::SymbolKey {
    scoped_symbol_key(&scoped_key(symbol, document))
}

/// The contract key of a join identity: document scope for a local symbol, target scope for
/// one scoped to a Cargo target's crate root (owner ruling 2026-10-30, #394), else global.
pub(super) fn scoped_symbol_key(key: &ScopedKey) -> semantic::SymbolKey {
    semantic::SymbolKey {
        scheme: semantic::SymbolScheme::Scip,
        symbol: key.0.clone(),
        scope: match &key.1 {
            None => semantic::SymbolScope::Global,
            Some(_) if key.0.starts_with("local ") => semantic::SymbolScope::Document,
            Some(_) => semantic::SymbolScope::Target,
        },
        document: key.1.clone(),
    }
}

/// The Cargo target of a claim's document, if its target is determined (#394).
pub(super) fn claim_target(
    source_set_id: &str,
    claim: &CapturedClaim,
) -> Option<join_store::ClaimTarget> {
    claim.envelope.cargo_target.as_ref().map(|target| {
        (
            native::DocumentKey {
                source_set_id: source_set_id.to_owned(),
                language: claim.language.clone(),
                path: target.root.clone(),
            },
            target
                .is_lib()
                .then(|| (target.package.clone(), target.version.clone())),
        )
    })
}

/// The identity a definition or symbol information defines: a local symbol in its document,
/// any other in its document's Cargo target, or globally when that target is not determined.
pub(super) fn defining_key(
    symbol: &str,
    document: &native::DocumentKey,
    target: Option<&join_store::ClaimTarget>,
) -> ScopedKey {
    match target {
        Some((root, _)) if !symbol.starts_with("local ") => (symbol.to_owned(), Some(root.clone())),
        _ => scoped_key(symbol, document),
    }
}

/// Whether a SCIP key's package is the Cargo package `name` (and `version`, when the manifest
/// states one). A key that names no package, or a package without a name, passes: it carries
/// nothing to check against.
fn key_names_package(symbol: &str, name: &str, version: Option<&str>) -> bool {
    let Ok(parsed) = scip::symbol::parse_symbol(symbol) else {
        return false;
    };
    match parsed.package.as_ref() {
        Some(package) if !package.name.is_empty() => {
            package.name == name && version.is_none_or(|version| package.version == version)
        }
        _ => true,
    }
}

/// The identity a use names (#394). A symbol present in the use's own Cargo target is that
/// target's. Otherwise one present in exactly one library target is that library's: a library
/// is the only kind of target another can depend on, and rust-analyzer's key names its
/// package. A use in a document without a determined target takes a library's key only when
/// that library is the one target defining it, since the document may belong to a target that
/// defines the same key. Every other use names the global key.
pub(super) fn using_key<G: GlobalJoinReads>(
    index: &G,
    symbol: &str,
    document: &native::DocumentKey,
    target: Option<&join_store::ClaimTarget>,
) -> Result<ScopedKey> {
    if symbol.starts_with("local ") {
        return Ok(scoped_key(symbol, document));
    }
    if let Some((root, _)) = target
        && index.presence(symbol, Some(root))? != (false, false)
    {
        return Ok((symbol.to_owned(), Some(root.clone())));
    }
    let defining = index.defining_targets(symbol)?;
    // The library must also be the package rust-analyzer's key names: its manifest name, and
    // its version when the manifest states one (supervisor ruling on #394 ruling 2).
    let named = |package: &Option<join_store::LibraryPackage>| {
        package
            .as_ref()
            .is_some_and(|(name, version)| key_names_package(symbol, name, version.as_deref()))
    };
    let library = match (target, &defining[..]) {
        (None, [(root, package)]) if named(package) => Some(root),
        (None, _) => None,
        (Some(_), _) => match &defining
            .iter()
            .filter(|(_, package)| named(package))
            .collect::<Vec<_>>()[..]
        {
            [(root, _)] => Some(root),
            _ => None,
        },
    };
    Ok((symbol.to_owned(), library.cloned()))
}

pub(super) fn declaration_targets<S: JoinStore, G: GlobalJoinReads>(
    store: &S,
    index: &G,
    document: &native::DocumentKey,
    symbol: &str,
    operation: &mut S::Operation,
) -> Result<Vec<semantic::Target>> {
    scoped_declaration_targets(store, index, &scoped_key(symbol, document), operation)
}

pub(super) fn scoped_declaration_targets<S: JoinStore, G: GlobalJoinReads>(
    store: &S,
    index: &G,
    key: &ScopedKey,
    operation: &mut S::Operation,
) -> Result<Vec<semantic::Target>> {
    store.reserve_operation(operation, || {
        staged::sizes::sum(&[
            std::mem::size_of::<ScopedKey>() as u64,
            key.0.len() as u64,
            key.1.as_ref().map_or(Ok(0), staged::sizes::document)?,
        ])
    })?;
    let rows = index.declarations(key, &mut |bytes| {
        store.reserve_operation(operation, || Ok(bytes))
    })?;
    store.reserve_operation(operation, || {
        let revisions = (rows.len() as u64)
            .checked_mul(store.revision_id().len() as u64)
            .ok_or_else(|| {
                crate::scip_raw::ScipCaptureError::LimitExceeded("unit expansion".into())
            })?;
        staged::sizes::sum(&[
            std::mem::size_of::<Vec<semantic::Target>>() as u64,
            staged::target_size::target_slots(rows.len())?,
            revisions,
        ])
    })?;
    let mut targets = Vec::with_capacity(rows.len());
    targets.extend(
        rows.into_iter()
            .map(|(document, syntax_id)| semantic::Target::Internal {
                syntax_id,
                document,
                revision_id: store.revision_id().to_owned(),
            }),
    );
    Ok(targets)
}
pub(super) fn declaration_target<S: JoinStore, G: GlobalJoinReads>(
    store: &S,
    index: &G,
    document: &native::DocumentKey,
    symbol: &str,
    operation: &mut S::Operation,
) -> Result<Option<semantic::Target>> {
    let items = declaration_targets(store, index, document, symbol, operation)?;
    Ok((items.len() == 1)
        .then(|| items.into_iter().next())
        .flatten())
}
