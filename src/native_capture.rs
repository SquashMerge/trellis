//! Test-support exporter for source-measured semantic-contract NativeArtifact inputs.
//! Only the fixture's source-set/revision manifest and listed snapshot bytes are read.
use crate::{
    model::SourceFile,
    native_evidence::{
        self, Artifact, Declaration, Document, DocumentKey, Range, Revision, SourceSet,
    },
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::{Component, Path},
};

// Same maximum admitted source size as the indexing CLI.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    source_sets: Vec<SourceSet>,
    revisions: Vec<FixtureRevision>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureRevision {
    id: String,
    source_set_id: String,
    documents: Vec<FixtureDocument>,
    toolchain_hash: String,
    config_hash: String,
    dependency_hash: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureDocument {
    key: DocumentKey,
    revision_id: String,
    source_file: String,
}

pub fn describe() -> Value {
    native_evidence::fixture_descriptor()
}

fn relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && !path.contains(['\\', '\0']),
        "invalid fixture path"
    );
    ensure!(
        Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "fixture path must be relative without dot components"
    );
    // Path::components collapses interior '.' segments; the manifest must not.
    ensure!(
        path.split('/')
            .all(|p| !p.is_empty() && p != "." && p != ".."),
        "invalid fixture path component"
    );
    Ok(())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "fixture input is not a regular file"
    );
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "fixture file exceeds 16 MiB: {}",
        path.display()
    );
    Ok(bytes)
}

/// Capture all selected manifest revisions using the production document extractor.
/// Output creation is atomic and refuses to replace any existing path.
pub fn capture(
    fixture: &Path,
    output: &Path,
    source_set_id: Option<&str>,
    revision_id: Option<&str>,
) -> Result<()> {
    let manifest_path = if fixture.is_dir() {
        fixture.join("fixture.json")
    } else {
        fixture.to_owned()
    };
    let root = manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    let manifest: Manifest = serde_json::from_slice(&read_bounded(&manifest_path)?)?;
    let mut declarations = Vec::new();
    let mut calls = Vec::new();
    let mut controls = Vec::new();
    let mut revision_keys = BTreeSet::new();
    let mut selected = 0;
    for revision in manifest.revisions {
        if source_set_id.is_some_and(|id| id != revision.source_set_id)
            || revision_id.is_some_and(|id| id != revision.id)
        {
            continue;
        }
        ensure!(
            revision_keys.insert((revision.source_set_id.clone(), revision.id.clone())),
            "duplicate fixture revision"
        );
        let source_sets: Vec<_> = manifest
            .source_sets
            .iter()
            .filter(|set| set.id == revision.source_set_id)
            .collect();
        ensure!(
            source_sets.len() == 1,
            "fixture revision requires one source set"
        );
        let source_set = source_sets[0].clone();
        let mut files = Vec::new();
        let mut documents = Vec::new();
        let mut keys = BTreeSet::new();
        for document in revision.documents {
            ensure!(
                document.revision_id == revision.id && document.key.source_set_id == source_set.id,
                "fixture document tuple mismatch"
            );
            ensure!(
                source_set.languages.contains(&document.key.language),
                "fixture language absent from source set"
            );
            ensure!(
                keys.insert(document.key.clone()),
                "duplicate fixture document"
            );
            relative(&document.key.path)?;
            relative(&document.source_file)?;
            let source_path = root.join(&document.source_file).canonicalize()?;
            ensure!(
                source_path.starts_with(&root),
                "snapshot outside fixture directory"
            );
            let bytes = read_bounded(&source_path)?;
            let text = String::from_utf8(bytes).context("fixture source is not UTF-8")?;
            let hash = hex::encode(Sha256::digest(text.as_bytes()));
            documents.push(Document {
                key: document.key.clone(),
                revision_id: revision.id.clone(),
                content_hash: hash.clone(),
                byte_length: text.len(),
            });
            files.push(SourceFile {
                path: document.key.path,
                language: document.key.language,
                hash,
                text,
            });
        }
        let revision = Revision {
            id: revision.id,
            source_set_id: revision.source_set_id,
            documents,
            toolchain_hash: revision.toolchain_hash,
            config_hash: revision.config_hash,
            dependency_hash: revision.dependency_hash,
        };
        let artifact = native_evidence::measure_fixture(&files, source_set, revision)?;
        append_rows(
            &artifact,
            &files,
            &mut declarations,
            &mut calls,
            &mut controls,
        )?;
        selected += 1;
    }
    ensure!(selected > 0, "no matching fixture revision");
    let descriptor = describe();
    let inventories = descriptor["extractionInputs"]
        .as_object()
        .context("missing extraction inventory")?;
    ensure!(
        inventories.values().all(|value| value == &json!([])),
        "export requires document-local extraction inventory"
    );
    let capture = json!({"formatVersion":1, "producerId":descriptor["producer"]["id"], "extractionInputs":[], "declarations":declarations, "calls":calls, "controls":controls, "references":[]});
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(temp.as_file_mut(), &capture)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(output)
        .map_err(|e| e.error)
        .context("create fresh native capture output")?;
    Ok(())
}

fn position(range: &Range) -> Value {
    json!({"start":range.start, "end":range.end, "encoding":"utf8"})
}
fn row_ref(revision: &str, native_id: &str) -> String {
    format!("{}:{revision}:{native_id}", revision.len())
}
fn witness(
    field: &str,
    value: &str,
    source: &str,
    within: &Range,
    exact: Option<&Range>,
) -> Result<Value> {
    let range = if let Some(range) = exact {
        range.clone()
    } else {
        let offset = source
            .get(within.start..within.end)
            .context("invalid witness container")?
            .find(value)
            .with_context(|| {
                format!("measured header leaf has no literal source witness: {field}")
            })?;
        Range {
            start: within.start + offset,
            end: within.start + offset + value.len(),
        }
    };
    ensure!(
        range.start >= within.start
            && range.end <= within.end
            && source.get(range.start..range.end) == Some(value),
        "measured witness mismatch: {field}"
    );
    Ok(json!({"field":field, "witness":{"range":position(&range), "text":value}}))
}
fn declaration_witnesses(row: &Declaration, source: &str) -> Result<Vec<Value>> {
    let mut entries = Vec::new();
    let mut add = |field: String, value: Option<&str>, range: Option<&Range>| -> Result<()> {
        if let Some(value) = value {
            entries.push(witness(&field, value, source, &row.range, range)?);
        }
        Ok(())
    };
    add("name".into(), row.name.as_deref(), row.name_range.as_ref())?;
    add(
        "header.name".into(),
        row.header.name.as_deref(),
        row.name_range.as_ref(),
    )?;
    for (i, value) in row.header.modifiers.iter().enumerate() {
        add(format!("header.modifiers[{i}]"), Some(value), None)?;
    }
    for (i, value) in row.header.type_parameters.iter().enumerate() {
        add(format!("header.typeParameters[{i}]"), Some(value), None)?;
    }
    for (i, value) in row.header.parameters.iter().enumerate() {
        add(
            format!("header.parameters[{i}].name"),
            value.name.as_deref(),
            None,
        )?;
        add(
            format!("header.parameters[{i}].type"),
            value.type_name.as_deref(),
            None,
        )?;
    }
    add(
        "header.resultType".into(),
        row.header.result_type.as_deref(),
        None,
    )?;
    for (i, value) in row.header.bases.iter().enumerate() {
        add(format!("header.bases[{i}]"), Some(value), None)?;
    }
    if let Some(signature) = &row.key.signature {
        for (i, value) in signature.parameter_types.iter().enumerate() {
            add(format!("signature.parameterTypes[{i}]"), Some(value), None)?;
        }
    }
    Ok(entries)
}
fn append_rows(
    artifact: &Artifact,
    files: &[SourceFile],
    declarations: &mut Vec<Value>,
    calls: &mut Vec<Value>,
    controls: &mut Vec<Value>,
) -> Result<()> {
    let reference = |id: &str| row_ref(&artifact.revision.id, id);
    for row in &artifact.declarations {
        if let Some(signature) = &row.key.signature {
            ensure!(
                signature.parameter_types.len() == row.header.parameters.len()
                    && signature.type_parameter_count == row.header.type_parameters.len()
                    && signature.variadic == row.header.parameters.iter().any(|p| p.variadic),
                "production declaration signature cannot be represented by toolkit: {}",
                row.syntax_id
            );
        }
        let source = files
            .iter()
            .find(|file| file.path == row.document.path && file.language == row.document.language)
            .context("missing measured source")?;
        let parent = if row.kind == "module" && row.ancestors.is_empty() {
            None
        } else {
            let parents: Vec<_> = artifact
                .declarations
                .iter()
                .filter(|candidate| {
                    candidate.document == row.document
                        && candidate.syntax_id != row.syntax_id
                        && candidate.range.start <= row.range.start
                        && candidate.range.end >= row.range.end
                        && (if row.ancestors.is_empty() {
                            candidate.kind == "module" && candidate.ancestors.is_empty()
                        } else {
                            candidate.key == *row.ancestors.last().unwrap()
                                && candidate.ancestors == row.ancestors[..row.ancestors.len() - 1]
                        })
                })
                .collect();
            ensure!(
                parents.len() == 1,
                "native declaration parent is not unique"
            );
            Some(reference(&parents[0].syntax_id))
        };
        declarations.push(json!({"role":row.role, "ref":reference(&row.syntax_id), "nativeId":row.syntax_id, "document":row.document, "revisionId":row.revision_id,
            "parentRef":parent, "kind":row.kind, "name":row.name, "range":position(&row.range), "nameRange":row.name_range.as_ref().map(position),
            "header":row.header, "signature":row.key.signature, "witnesses":declaration_witnesses(row, &source.text)?}));
    }
    for row in &artifact.calls {
        let source = files
            .iter()
            .find(|file| file.path == row.document.path && file.language == row.document.language)
            .context("missing measured call source")?;
        let witnesses = row
            .spelling
            .as_ref()
            .map(|value| {
                witness(
                    "spelling",
                    value,
                    &source.text,
                    &row.range,
                    row.callee_range.as_ref(),
                )
            })
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        calls.push(json!({"ref":reference(&row.id), "nativeId":row.id, "document":row.document, "revisionId":row.revision_id,
            "ownerRef":reference(&row.owner_syntax_id), "range":position(&row.range), "calleeRange":row.callee_range.as_ref().map(position), "spelling":row.spelling,
            "regionRefs":row.region_ids.iter().map(|id| reference(id)).collect::<Vec<_>>(), "witnesses":witnesses}));
    }
    for row in &artifact.control_regions {
        controls.push(json!({"ref":reference(&row.id), "nativeId":row.id, "document":row.document, "revisionId":row.revision_id,
            "ownerRef":reference(&row.owner_syntax_id), "parentRef":row.parent_id.as_ref().map(|id| reference(id)), "kind":row.kind, "range":position(&row.range), "arm":row.arm, "witnesses":[]}));
    }
    Ok(())
}
