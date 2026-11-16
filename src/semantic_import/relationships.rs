//! Method and relationship discovery and the type-relationship passes.
#[cfg(test)]
use super::work::complexity_work;
#[cfg(feature = "test-support")]
use super::work::note_parsed_type_document;
use super::{
    join_store::{
        DocumentJoinState, IndexedJoinStore, JoinIndex, JoinSink, JoinStore, MethodSummary,
        RelationshipRow,
    },
    keys::{declaration_target, declaration_targets, symbol_key},
    role_sites::{DirectBase, LazyBytes, direct_java_types, direct_script_types, direct_ts_types},
    staged,
};
use crate::{native_evidence as native, native_ids, semantic_evidence as semantic};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn reserved_key_bytes<S: JoinStore>(
    store: &S,
    key: &native::Key,
    operation: &mut S::Operation,
) -> Result<Vec<u8>> {
    let mut capacity = 0;
    store.reserve_operation(operation, || {
        let (length, scratch) = staged::group_size::key_bytes(key)?;
        capacity = usize::try_from(length)?;
        staged::sizes::sum(&[length, scratch, std::mem::size_of::<Vec<u8>>() as u64])
    })?;
    let value = serde_json::to_value(key)?;
    let mut bytes = Vec::with_capacity(capacity);
    native_ids::canonical_into(&value, &mut |part| bytes.extend_from_slice(part));
    Ok(bytes)
}

pub(super) fn method_target_owner_matches<S: JoinStore, I: DocumentJoinState>(
    store: &S,
    index: &I,
    method: &native::Declaration,
    symbol: &str,
) -> Result<bool> {
    let mut operation = store.operation()?;
    let Some((owner_symbol, _)) = symbol.rsplit_once('#') else {
        return Ok(false);
    };
    let owner_bytes = method
        .ancestors
        .last()
        .map(|key| reserved_key_bytes(store, key, &mut operation))
        .transpose()?;
    let owners = owner_bytes
        .as_ref()
        .map(|key| store.declarations_with_key(&method.document, key, &mut operation))
        .transpose()?
        .into_iter()
        .flatten();
    let mut matching = 0usize;
    for owner in owners {
        let Some(name) = owner.name_range.as_ref() else {
            continue;
        };
        if owner.document == method.document
            && owner.kind == "type"
            && method.ancestors.last() == Some(&owner.key)
            && owner.range.start <= method.range.start
            && method.range.end <= owner.range.end
        {
            let mut keys_operation = store.operation()?;
            store.reserve_operation(&mut keys_operation, || {
                index.type_uses_clone_bytes(&owner.document, name.start, name.end)
            })?;
            let keys = index.type_uses(&owner.document, name.start, name.end)?;
            if keys.len() == 1
                && keys
                    .iter()
                    .next()
                    .is_some_and(|key| key.strip_suffix('#') == Some(owner_symbol))
            {
                matching += 1;
            }
        }
    }
    Ok(matching == 1)
}

pub(super) fn discover_join_methods<S: IndexedJoinStore>(
    store: &S,
    index: &mut S::Index,
) -> Result<()> {
    for doc in store.documents() {
        let doc = doc?;
        let mut operation = store.operation()?;
        for declaration in store.document_declarations(&doc, &mut operation)? {
            #[cfg(test)]
            complexity_work(|work| work.method_index_rows += 1);
            if declaration.kind != "method" {
                continue;
            }
            let Some(range) = &declaration.name_range else {
                continue;
            };
            let mut symbols_operation = store.operation()?;
            store.reserve_operation(&mut symbols_operation, || {
                index.type_uses_clone_bytes(&declaration.document, range.start, range.end)
            })?;
            for symbol in index.type_uses(&declaration.document, range.start, range.end)? {
                let summary = MethodSummary {
                    target_owner_matches: method_target_owner_matches(
                        store,
                        index,
                        &declaration,
                        &symbol,
                    )?,
                    method: declaration.as_ref().clone(),
                };
                index.put_method(&declaration.document, &symbol, &summary)?;
            }
        }
    }
    Ok(())
}

pub(super) fn discover_relationship_claims<S: JoinStore, I: DocumentJoinState>(
    store: &S,
    index: &mut I,
) -> Result<()> {
    for claim in store.claims() {
        let claim = claim?;
        if claim.envelope.record_kind != "relationship" {
            continue;
        }
        let (language, path) = (&claim.language, &claim.envelope.relative_path);
        let source = claim.envelope.raw["sourceSymbol"].as_str();
        let target = claim.envelope.raw["targetSymbol"].as_str();
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || {
            staged::sizes::sum(&[
                std::mem::size_of::<RelationshipRow>() as u64,
                claim.captured_proof_id.len() as u64,
                source.map_or(0, |s| s.len() as u64),
                target.map_or(0, |s| s.len() as u64),
            ])
        })?;
        let row = RelationshipRow {
            proof_id: claim.captured_proof_id.clone(),
            source_symbol: source.map(str::to_owned),
            target_symbol: target.map(str::to_owned),
        };
        store.reserve_operation(&mut operation, || staged::sizes::relationship(&row))?;
        index.put_relationship(language, path, row.clone())?;
        if let (Some(source), Some(target)) = (&row.source_symbol, &row.target_symbol) {
            index.put_relationship_endpoint(language, path, source, target, &row)?;
        }
    }
    Ok(())
}

pub(super) fn join_type_relationships<S: IndexedJoinStore, K: JoinSink>(
    store: &S,
    index: &mut S::Index,
    out: &mut K,
) -> Result<()> {
    discover_join_methods(store, index)?;
    discover_relationship_claims(store, index)?;
    emit_type_relationships(store, index, out)
}

pub(super) fn emit_type_relationships<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    store: &S,
    index: &I,
    out: &mut K,
) -> Result<()> {
    let revision_id = store.revision_id();
    for doc in store.documents() {
        let doc = &doc?;
        let bytes = store
            .bytes_of(doc)?
            .context("missing admitted document source")?;
        let bytes: &[u8] = &bytes;
        #[cfg(feature = "test-support")]
        if matches!(
            doc.language.as_str(),
            "java" | "javascript" | "typescript" | "python"
        ) {
            note_parsed_type_document(doc);
        }
        let mut relationship_operation = store.operation()?;
        let Some(bases) = document_bases(store, doc, bytes, &mut relationship_operation)? else {
            continue;
        };
        let java_bases = java_base_ranges(store, doc, &bases, &mut relationship_operation)?;
        store.reserve_operation(&mut relationship_operation, || {
            index.relationships_clone_bytes(&doc.language, &doc.path)
        })?;
        let document_relationships = index.relationships(&doc.language, &doc.path)?;
        emit_base_relationships(store, index, out, doc, bases)?;
        if doc.language == "java" {
            emit_java_overrides(store, index, out, doc, &document_relationships, &java_bases)?;
            emit_java_enum_extends(
                store,
                index,
                out,
                doc,
                bytes,
                revision_id,
                &document_relationships,
            )?;
        }
    }
    Ok(())
}

/// The direct type bases one document's source declares, or `None` for a language whose
/// bases are not read. Takes the document's parsed tree.
fn document_bases<S: JoinStore>(
    store: &S,
    doc: &native::DocumentKey,
    bytes: &[u8],
    relationship_operation: &mut S::Operation,
) -> Result<Option<Vec<DirectBase>>> {
    let parsed = store.take_parsed_tree(doc);
    let mut reserve_bases =
        |bytes: LazyBytes<'_>| store.reserve_operation(relationship_operation, bytes);
    let bases = match doc.language.as_str() {
        "java" => direct_java_types(bytes, parsed, &mut reserve_bases)?,
        "javascript" => direct_script_types(
            bytes,
            tree_sitter_javascript::LANGUAGE.into(),
            "class_declaration",
            "class_heritage",
            parsed,
            &mut reserve_bases,
        )?,
        "typescript" => direct_ts_types(
            bytes,
            crate::indexer_ts::grammar(&doc.path),
            parsed,
            &mut reserve_bases,
        )?,
        "python" => direct_script_types(
            bytes,
            tree_sitter_python::LANGUAGE.into(),
            "class_definition",
            "argument_list",
            parsed,
            &mut reserve_bases,
        )?,
        _ => return Ok(None),
    };
    Ok(Some(bases))
}

/// A Java document's first direct base per owner, keyed by the owner's name span; empty for
/// any other language.
fn java_base_ranges<S: JoinStore>(
    store: &S,
    doc: &native::DocumentKey,
    bases: &[DirectBase],
    relationship_operation: &mut S::Operation,
) -> Result<BTreeMap<(usize, usize), native::Range>> {
    store.reserve_operation(relationship_operation, || {
        Ok(std::mem::size_of::<BTreeMap<(usize, usize), native::Range>>() as u64)
    })?;
    let mut java_bases = BTreeMap::new();
    if doc.language == "java" {
        for (owner, base, _) in bases {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                java_bases.entry((owner.start, owner.end))
            {
                store.reserve_operation(relationship_operation, || {
                    staged::sizes::map_entries::<(usize, usize), native::Range>(1)
                })?;
                entry.insert(base.clone());
            }
        }
    }
    Ok(java_bases)
}

/// Extends and implements: one relationship per claim between a direct base's two exact
/// endpoints.
fn emit_base_relationships<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    store: &S,
    index: &I,
    out: &mut K,
    doc: &native::DocumentKey,
    bases: Vec<DirectBase>,
) -> Result<()> {
    for (owner_range, base_range, kind) in bases {
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || {
            index.type_uses_clone_bytes(doc, owner_range.start, owner_range.end)
        })?;
        store.reserve_operation(&mut operation, || {
            index.type_uses_clone_bytes(doc, base_range.start, base_range.end)
        })?;
        let owner = index.type_uses(doc, owner_range.start, owner_range.end)?;
        let base = index.type_uses(doc, base_range.start, base_range.end)?;
        if owner.len() != 1 || base.len() != 1 {
            continue;
        }
        let (source_symbol, target_symbol) =
            (owner.iter().next().unwrap(), base.iter().next().unwrap());
        let Some(source) = declaration_target(store, index, doc, source_symbol, &mut operation)?
        else {
            continue;
        };
        let mut target_candidates =
            declaration_targets(store, index, doc, target_symbol, &mut operation)?;
        if target_candidates.is_empty()
            && !target_symbol.starts_with("local ")
            && !index.is_information_symbol(target_symbol)?
            && !store.has_unclaimed_source(&doc.language)?
        {
            store.reserve_operation(&mut operation, || {
                staged::sizes::sum(&[
                    staged::target_size::target_slots(4)?,
                    target_symbol.len() as u64,
                ])
            })?;
            target_candidates.push(semantic::Target::External {
                symbol: symbol_key(target_symbol, doc),
            });
        }
        let Some(target) = (target_candidates.len() == 1)
            .then(|| target_candidates.pop())
            .flatten()
        else {
            if doc.language == "java" {
                out.diagnostic(doc, "unmeasurable_external_base")?;
            }
            continue;
        };
        store.reserve_operation(&mut operation, || {
            index.relationship_endpoints_clone_bytes(
                &doc.language,
                &doc.path,
                source_symbol,
                target_symbol,
            )
        })?;
        for claim in
            index.relationship_endpoints(&doc.language, &doc.path, source_symbol, target_symbol)?
        {
            let mut output_operation = store.operation()?;
            store.reserve_operation(&mut output_operation, || {
                staged::sizes::sum(&[
                    std::mem::size_of::<semantic::TypeRelationship>() as u64,
                    staged::sizes::target_payload(&source)?,
                    staged::sizes::target_payload(&target)?,
                ])
            })?;
            out.type_relationship(semantic::TypeRelationship {
                kind,
                source: source.clone(),
                target: target.clone(),
                provenance_id: claim.proof_id,
            })?;
        }
    }
    Ok(())
}

/// Java overrides: one relationship per claim whose source method's owner directly extends
/// or implements the owner of the target method.
fn emit_java_overrides<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    store: &S,
    index: &I,
    out: &mut K,
    doc: &native::DocumentKey,
    document_relationships: &[RelationshipRow],
    java_bases: &BTreeMap<(usize, usize), native::Range>,
) -> Result<()> {
    for claim in document_relationships {
        let mut operation = store.operation()?;
        let (Some(source_symbol), Some(target_symbol)) = (
            claim.source_symbol.as_deref(),
            claim.target_symbol.as_deref(),
        ) else {
            continue;
        };
        let source_methods = index.source_methods(doc, source_symbol, &mut |bytes| {
            store.reserve_operation(&mut operation, || Ok(bytes))
        })?;
        #[cfg(test)]
        complexity_work(|work| work.method_lookup_rows += source_methods.len());
        let [source_method] = source_methods.as_slice() else {
            if source_methods.is_empty()
                && declaration_target(store, index, doc, source_symbol, &mut operation)?.is_none()
            {
                out.diagnostic(doc, "unmeasurable_override_endpoint")?;
            }
            continue;
        };
        let Some(source_target) =
            declaration_target(store, index, doc, source_symbol, &mut operation)?
        else {
            continue;
        };
        let target_methods = index.target_methods(doc, target_symbol, &mut |bytes| {
            store.reserve_operation(&mut operation, || Ok(bytes))
        })?;
        #[cfg(test)]
        complexity_work(|work| work.method_lookup_rows += target_methods.len());
        if target_methods.len() > 1 {
            out.diagnostic(doc, "unmeasurable_override_endpoint")?;
            continue;
        }
        let target_method = target_methods.first();
        let base_uses =
            direct_base_uses(store, index, doc, source_method, java_bases, &mut operation)?;
        let direct_base = (base_uses.len() == 1)
            .then(|| base_uses.iter().next())
            .flatten();
        let target_owner_key = target_symbol.rsplit_once('#').map(|(owner, _)| owner);
        let direct_owner = direct_base.and_then(|base| base.strip_suffix('#'));
        let method_descriptor_matches =
            target_symbol.rsplit_once('#').is_some_and(|(_, suffix)| {
                source_symbol
                    .rsplit_once('#')
                    .is_some_and(|(_, source_suffix)| {
                        target_method.is_none() || suffix == source_suffix
                    })
            });
        let target_owner_matches = target_method.is_none_or(|method| method.target_owner_matches);
        let owner_matches = direct_owner.is_some()
            && direct_owner == target_owner_key
            && method_descriptor_matches
            && target_owner_matches;
        if !owner_matches {
            if target_method.is_none()
                && (direct_owner.is_none() || direct_owner != target_owner_key)
            {
                out.diagnostic(doc, "unmeasurable_override_endpoint")?;
            }
            continue;
        }
        let destination = override_destination(
            store,
            index,
            doc,
            target_symbol,
            target_method,
            &mut operation,
        )?;
        if let Some(destination) = destination {
            store.reserve_operation(&mut operation, || {
                staged::sizes::sum(&[
                    std::mem::size_of::<semantic::TypeRelationship>() as u64,
                    claim.proof_id.len() as u64,
                ])
            })?;
            out.type_relationship(semantic::TypeRelationship {
                kind: semantic::RelationshipKind::Overrides,
                source: source_target,
                target: destination,
                provenance_id: claim.proof_id.clone(),
            })?;
        }
    }
    Ok(())
}

/// The symbols used at the direct base of the one type that owns `source_method`; empty when
/// that owner or its base is not exact.
fn direct_base_uses<S: JoinStore, I: DocumentJoinState>(
    store: &S,
    index: &I,
    doc: &native::DocumentKey,
    source_method: &MethodSummary,
    java_bases: &BTreeMap<(usize, usize), native::Range>,
    operation: &mut S::Operation,
) -> Result<BTreeSet<String>> {
    let source_owner = source_method.ancestors.last();
    let owner_bytes = source_owner
        .map(|key| reserved_key_bytes(store, key, operation))
        .transpose()?;
    let owner_rows = owner_bytes
        .as_ref()
        .map(|key| store.declarations_with_key(doc, key, operation))
        .transpose()?;
    let mut owner_declarations = owner_rows.unwrap_or_default();
    owner_declarations.retain(|d| {
        d.document == *doc
            && d.kind == "type"
            && source_owner == Some(&d.key)
            && d.range.start <= source_method.range.start
            && source_method.range.end <= d.range.end
    });
    let base_uses = match (owner_declarations.len() == 1)
        .then(|| &owner_declarations[0])
        .and_then(|owner| owner.name_range.as_ref())
        .and_then(|r| java_bases.get(&(r.start, r.end)))
    {
        Some(range) => {
            store.reserve_operation(operation, || {
                index.type_uses_clone_bytes(doc, range.start, range.end)
            })?;
            index.type_uses(doc, range.start, range.end)?
        }
        None => BTreeSet::new(),
    };
    Ok(base_uses)
}

/// The overridden method's target: its one declaration, or an external symbol when no
/// method was measured for it and no unread source could declare it.
fn override_destination<S: JoinStore, I: DocumentJoinState>(
    store: &S,
    index: &I,
    doc: &native::DocumentKey,
    target_symbol: &str,
    target_method: Option<&MethodSummary>,
    operation: &mut S::Operation,
) -> Result<Option<semantic::Target>> {
    let mut destination = declaration_target(store, index, doc, target_symbol, operation)?;
    // A global symbol could be declared in a source that wasn't read (#576) or kept
    // no claims over a per-document cap (#634).
    if destination.is_none()
        && target_method.is_none()
        && (target_symbol.starts_with("local ") || !store.has_unclaimed_source(&doc.language)?)
    {
        store.reserve_operation(operation, || {
            staged::sizes::sum(&[
                std::mem::size_of::<semantic::Target>() as u64,
                target_symbol.len() as u64,
                if target_symbol.starts_with("local ") {
                    staged::sizes::document(doc)?
                } else {
                    0
                },
            ])
        })?;
        destination = Some(semantic::Target::External {
            symbol: symbol_key(target_symbol, doc),
        });
    }
    Ok(destination)
}

/// Java's implicit `Enum` base: an enum declaration extends the external `java/lang/Enum#`
/// its relationship claims name.
fn emit_java_enum_extends<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    store: &S,
    index: &I,
    out: &mut K,
    doc: &native::DocumentKey,
    bytes: &[u8],
    revision_id: &str,
    document_relationships: &[RelationshipRow],
) -> Result<()> {
    let mut operation = store.operation()?;
    let document_types: Vec<_> = store
        .document_declarations(doc, &mut operation)?
        .into_iter()
        .filter(|d| d.kind == "type")
        .collect();
    for declaration in document_types.iter().filter(|d| {
        std::str::from_utf8(bytes).ok().is_some_and(|source| {
            d.name_range
                .as_ref()
                .and_then(|r| source.get(..r.start))
                .and_then(|prefix| prefix.rsplit(['{', '}', ';']).next())
                .is_some_and(|prefix| prefix.split_whitespace().last() == Some("enum"))
        })
    }) {
        let mut enum_operation = store.operation()?;
        let Some(name) = &declaration.name_range else {
            continue;
        };
        store.reserve_operation(&mut enum_operation, || {
            index.type_uses_clone_bytes(doc, name.start, name.end)
        })?;
        let keys = index.type_uses(doc, name.start, name.end)?;
        if keys.len() != 1 {
            continue;
        }
        let source_symbol = keys.iter().next().unwrap();
        if !matches!(declaration_target(store,index,doc,source_symbol,&mut enum_operation)?,
            Some(semantic::Target::Internal {syntax_id,document,revision_id: target_revision})
            if syntax_id==declaration.syntax_id && document==*doc && target_revision==revision_id)
        {
            continue;
        }
        for claim in document_relationships.iter().filter(|c| {
            c.source_symbol.as_deref() == Some(source_symbol.as_str())
                && c.target_symbol
                    .as_deref()
                    .is_some_and(|s| s.ends_with("java/lang/Enum#"))
        }) {
            let key = claim.target_symbol.as_deref().unwrap();
            let mut output_operation = store.operation()?;
            store.reserve_operation(&mut output_operation, || {
                staged::sizes::sum(&[
                    std::mem::size_of::<semantic::TypeRelationship>() as u64,
                    declaration.syntax_id.len() as u64,
                    staged::sizes::document(doc)?,
                    revision_id.len() as u64,
                    key.len() as u64,
                    if key.starts_with("local ") {
                        staged::sizes::document(doc)?
                    } else {
                        0
                    },
                    claim.proof_id.len() as u64,
                ])
            })?;
            out.type_relationship(semantic::TypeRelationship {
                kind: semantic::RelationshipKind::Extends,
                source: semantic::Target::Internal {
                    syntax_id: declaration.syntax_id.clone(),
                    document: doc.clone(),
                    revision_id: revision_id.to_owned(),
                },
                target: semantic::Target::External {
                    symbol: symbol_key(key, doc),
                },
                provenance_id: claim.proof_id.clone(),
            })?;
        }
    }
    Ok(())
}
