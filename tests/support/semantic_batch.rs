//! Shared fixture for batch-import integration tests: workspaces, descriptors, pages and
//! artifacts written the way `tools/semantic-contract/semantic_batch.py` writes them.
#![allow(dead_code)]
use protobuf::{EnumOrUnknown, Message};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use trellis::semantic_batch::descriptor::SourceManifestHasher;

pub const DECODER_VERSION: &str = trellis::scip_raw::DECODER_VERSION;
const LANGUAGES: [&str; 4] = ["java", "rust", "python", "javascript"];

pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn language_order(language: &str) -> usize {
    LANGUAGES.iter().position(|l| *l == language).unwrap()
}

fn extension(language: &str) -> &'static str {
    match language {
        "java" => "java",
        "rust" => "rs",
        "python" => "py",
        _ => "js",
    }
}

/// One occurrence an artifact records in a document.
#[derive(Debug, Clone)]
pub struct OccSpec {
    pub range: Vec<i32>,
    pub enclosing: Vec<i32>,
    pub symbol: String,
    pub roles: i32,
}

/// One document an artifact covers. Without `occurrences` the document gets a single
/// definition occurrence at `[0, 0, 1]`.
#[derive(Debug, Clone)]
pub struct DocSpec {
    pub path: String,
    pub occurrences: Vec<OccSpec>,
    /// `(symbol, SCIP kind, relationship targets)` symbol information rows.
    pub symbols: Vec<(String, i32, Vec<String>)>,
}

impl DocSpec {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.into(),
            occurrences: vec![],
            symbols: vec![],
        }
    }

    pub fn occurrence(
        mut self,
        range: &[i32],
        enclosing: &[i32],
        symbol: &str,
        roles: i32,
    ) -> Self {
        self.occurrences.push(OccSpec {
            range: range.to_vec(),
            enclosing: enclosing.to_vec(),
            symbol: symbol.into(),
            roles,
        });
        self
    }

    pub fn symbol(mut self, symbol: &str, kind: i32, related: &[&str]) -> Self {
        self.symbols.push((
            symbol.into(),
            kind,
            related.iter().map(|s| s.to_string()).collect(),
        ));
        self
    }
}

#[derive(Debug, Clone)]
struct SourceDoc {
    language: String,
    path: String,
    content_hash: String,
    byte_length: u64,
}

#[derive(Debug, Clone)]
struct Producer {
    trust_origin: &'static str,
    profile_id: String,
    name: String,
    version: String,
    identity_kind: &'static str,
    executable_hash: String,
    position_encoding: u8,
    languages: Vec<String>,
}

/// Header of the published native revision plus the batch under construction.
#[derive(Debug, Clone)]
pub struct BatchFixture {
    root: PathBuf,
    source_set: Value,
    source_revision: String,
    components: Value,
    sources: Vec<SourceDoc>,
    producer: Producer,
    artifacts: Vec<(String, Vec<DocSpec>)>,
    byte_docs: BTreeMap<u32, Vec<DocSpec>>,
    raw_bytes: BTreeMap<u32, Vec<u8>>,
    exclusions: BTreeMap<(String, String), &'static str>,
    rows_per_page: usize,
}

fn synthetic_producer() -> Producer {
    let profile: Value = serde_json::from_str(include_str!(
        "../../docs/semantic-evidence/synthetic-profiles/trellis-synthetic-scip-v1.json"
    ))
    .unwrap();
    let generator = &profile["generator"];
    Producer {
        trust_origin: "syntheticProfile",
        profile_id: profile["profileId"].as_str().unwrap().into(),
        name: generator["name"].as_str().unwrap().into(),
        version: generator["version"].as_str().unwrap().into(),
        identity_kind: "generatorSource",
        executable_hash: generator["sourceSha256"].as_str().unwrap().into(),
        position_encoding: 1,
        languages: LANGUAGES.iter().map(|l| l.to_string()).collect(),
    }
}

impl BatchFixture {
    /// Reads the live native revision's header and inventory from a published `index.db`,
    /// as #90's `capture_header` does.
    pub fn from_published(index_db: &Path, root: &Path) -> Self {
        let db = Connection::open_with_flags(index_db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let (revision_key, set_id, native_id, toolchain, config, dependency, inventory): (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        ) = db
            .query_row(
                "SELECT r.id,r.source_set_id,r.native_revision_id,r.toolchain_hash,r.config_hash,r.dependency_hash,r.source_inventory \
                 FROM native_revisions r JOIN index_metadata m ON m.index_revision=r.published_index_revision",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        let (producer_id, producer_version): (String, String) = db
            .query_row(
                "SELECT producer_id,producer_version FROM document_versions d JOIN revision_documents m ON m.vid=d.vid WHERE m.revision_id=?1 LIMIT 1",
                [&revision_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let ordered = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> Vec<String> {
            let mut statement = db.prepare(sql).unwrap();
            statement
                .query_map(params, |row| row.get::<_, String>(1))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let languages = ordered(
            "SELECT ordinal,language FROM native_producer_languages WHERE producer_id=?1 AND producer_version=?2 ORDER BY ordinal",
            &[&producer_id, &producer_version],
        );
        let dependencies = ordered(
            "SELECT ordinal,dependency_id FROM native_source_set_dependencies WHERE source_set_id=?1 ORDER BY ordinal",
            &[&set_id],
        );
        let root_id: String = db
            .query_row(
                "SELECT root_id FROM native_source_sets WHERE id=?1",
                [&set_id],
                |row| row.get(0),
            )
            .unwrap();
        let documents: Vec<Value> = serde_json::from_str(&inventory).unwrap();
        let sources = documents
            .iter()
            .map(|document| SourceDoc {
                language: document["key"]["language"].as_str().unwrap().into(),
                path: document["key"]["path"].as_str().unwrap().into(),
                content_hash: document["contentHash"].as_str().unwrap().into(),
                byte_length: document["byteLength"].as_u64().unwrap(),
            })
            .collect();
        Self {
            root: root.to_owned(),
            source_set: json!({"id":set_id,"rootId":root_id,"languages":languages,"dependencies":dependencies}),
            source_revision: native_id,
            components: json!({"toolchainHash":toolchain,"configHash":config,"dependencyHash":dependency}),
            sources,
            producer: synthetic_producer(),
            artifacts: vec![],
            byte_docs: BTreeMap::new(),
            raw_bytes: BTreeMap::new(),
            exclusions: BTreeMap::new(),
            rows_per_page: 1000,
        }
    }

    pub fn synthetic(mut self) -> Self {
        self.producer = synthetic_producer();
        self
    }

    /// A pinned real producer from the hardware profile; it covers one language.
    pub fn reference(mut self, producer: &str) -> Self {
        let profile: Value = serde_json::from_str(include_str!(
            "../../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
        ))
        .unwrap();
        let entry = profile["semanticProducers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == producer)
            .unwrap();
        let language = match producer {
            "scip-java" => "java",
            "scip-typescript" => "javascript",
            "scip-python" => "python",
            _ => "rust",
        };
        self.producer = Producer {
            trust_origin: "referenceProfile",
            profile_id: profile["profileId"].as_str().unwrap().into(),
            name: producer.into(),
            version: entry["version"].as_str().unwrap().into(),
            identity_kind: "executable",
            executable_hash: entry["executableHash"].as_str().unwrap().into(),
            position_encoding: match entry["positionEncoding"].as_str().unwrap() {
                "utf8" => 1,
                "utf16" => 2,
                _ => 3,
            },
            languages: vec![language.into()],
        };
        self
    }

    pub fn rows_per_page(mut self, rows: usize) -> Self {
        self.rows_per_page = rows;
        self
    }

    /// Every source path of `language`.
    pub fn paths(&self, language: &str) -> Vec<DocSpec> {
        self.sources
            .iter()
            .filter(|source| source.language == language)
            .map(|source| DocSpec::new(&source.path))
            .collect()
    }

    /// Adds an artifact covering `docs`; returns its ordinal.
    pub fn artifact(&mut self, language: &str, docs: Vec<DocSpec>) -> u32 {
        self.artifacts.push((language.into(), docs));
        (self.artifacts.len() - 1) as u32
    }

    /// Makes artifact `ordinal`'s bytes hold `docs` while coverage still assigns the original
    /// documents, to build missing, unlisted and duplicate documents.
    pub fn override_artifact_bytes(&mut self, ordinal: u32, docs: Vec<DocSpec>) {
        self.byte_docs.insert(ordinal, docs);
    }

    /// Makes artifact `ordinal`'s bytes exactly `bytes`, for tests that hand-build the SCIP.
    pub fn override_artifact_raw(&mut self, ordinal: u32, bytes: Vec<u8>) {
        self.raw_bytes.insert(ordinal, bytes);
    }

    pub fn exclude(&mut self, language: &str, path: &str, reason: &'static str) {
        self.exclusions
            .insert((language.into(), path.into()), reason);
    }

    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// Protobuf bytes of one artifact: a definition occurrence in each document.
    pub fn scip_bytes(&self, language: &str, docs: &[DocSpec]) -> Vec<u8> {
        let mut index = scip::types::Index::new();
        let mut metadata = scip::types::Metadata::new();
        metadata.project_root = format!("file://{}", self.root.display());
        metadata.text_document_encoding = EnumOrUnknown::from_i32(1);
        let mut tool = scip::types::ToolInfo::new();
        tool.name = self.producer.name.clone();
        tool.version = self.producer.version.clone();
        metadata.tool_info = Some(tool).into();
        index.metadata = Some(metadata).into();
        for (number, spec) in docs.iter().enumerate() {
            let mut document = scip::types::Document::new();
            document.relative_path = spec.path.clone();
            document.language = language.into();
            if spec.occurrences.is_empty() && spec.symbols.is_empty() {
                let symbol = format!("pkg d{number:05}.");
                let mut occurrence = scip::types::Occurrence::new();
                occurrence.range = vec![0, 0, 1];
                occurrence.symbol = symbol.clone();
                occurrence.symbol_roles = 1;
                occurrence.syntax_kind = EnumOrUnknown::from_i32(0);
                document.occurrences.push(occurrence);
                let mut info = scip::types::SymbolInformation::new();
                info.symbol = symbol;
                info.kind = EnumOrUnknown::from_i32(26);
                info.display_name = format!("d{number:05}");
                document.symbols.push(info);
            }
            for occ in &spec.occurrences {
                let mut occurrence = scip::types::Occurrence::new();
                occurrence.range = occ.range.clone();
                occurrence.enclosing_range = occ.enclosing.clone();
                occurrence.symbol = occ.symbol.clone();
                occurrence.symbol_roles = occ.roles;
                occurrence.syntax_kind = EnumOrUnknown::from_i32(0);
                document.occurrences.push(occurrence);
            }
            for (symbol, kind, related) in &spec.symbols {
                let mut info = scip::types::SymbolInformation::new();
                info.symbol = symbol.clone();
                info.kind = EnumOrUnknown::from_i32(*kind);
                info.display_name = symbol.clone();
                for target in related {
                    let mut relationship = scip::types::Relationship::new();
                    relationship.symbol = target.clone();
                    relationship.is_implementation = true;
                    info.relationships.push(relationship);
                }
                document.symbols.push(info);
            }
            index.documents.push(document);
        }
        index.write_to_bytes().unwrap()
    }

    /// Writes pages, artifacts and the descriptor into the fresh directory `dir`, which must be
    /// under the workspace root. Sources no artifact covers in a requested language are
    /// excluded as `notRequested`. Returns the descriptor path and its SHA-256.
    pub fn write(&self, dir: &Path) -> (PathBuf, String) {
        fs::create_dir_all(dir).unwrap();
        let prefix = dir
            .strip_prefix(&self.root)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let relative = |name: &str| format!("{prefix}/{name}");
        let mut sources = self.sources.clone();
        sources.sort_by(|a, b| {
            (language_order(&a.language), a.path.as_bytes())
                .cmp(&(language_order(&b.language), b.path.as_bytes()))
        });
        let mut assigned: BTreeMap<(String, String), u32> = BTreeMap::new();
        for (ordinal, (language, docs)) in self.artifacts.iter().enumerate() {
            for doc in docs {
                assigned.insert((language.clone(), doc.path.clone()), ordinal as u32);
            }
        }
        let source_rows: Vec<Value> = sources
            .iter()
            .map(|s| json!({"byteLength":s.byte_length,"contentHash":s.content_hash,"language":s.language,"relativePath":s.path}))
            .collect();
        let coverage_rows: Vec<Value> = sources
            .iter()
            .filter(|s| self.producer.languages.contains(&s.language))
            .map(|s| {
                let key = (s.language.clone(), s.path.clone());
                match assigned.get(&key) {
                    Some(artifact) => json!({"artifact":artifact,"exclusion":null,"language":s.language,"relativePath":s.path}),
                    None => json!({"artifact":null,"exclusion":self.exclusions.get(&key).copied().unwrap_or("notRequested"),"language":s.language,"relativePath":s.path}),
                }
            })
            .collect();
        let mut pages = vec![];
        for (kind, rows) in [("source", &source_rows), ("coverage", &coverage_rows)] {
            for (ordinal, chunk) in rows.chunks(self.rows_per_page).enumerate() {
                let bytes = serde_json::to_vec(
                    &json!({"formatVersion":1,"kind":kind,"ordinal":ordinal,"rows":chunk}),
                )
                .unwrap();
                let name = format!("{kind}-{ordinal:05}.json");
                fs::write(dir.join(&name), &bytes).unwrap();
                pages.push(json!({"kind":kind,"ordinal":ordinal,"relativePath":relative(&name),"byteLength":bytes.len(),"sha256":sha256(&bytes),"rowCount":chunk.len()}));
            }
        }
        let mut artifacts = vec![];
        for (ordinal, (language, docs)) in self.artifacts.iter().enumerate() {
            let bytes = match self.raw_bytes.get(&(ordinal as u32)) {
                Some(bytes) => bytes.clone(),
                None => self.scip_bytes(
                    language,
                    self.byte_docs.get(&(ordinal as u32)).unwrap_or(docs),
                ),
            };
            let name = format!("artifact-{ordinal}.scip");
            fs::write(dir.join(&name), &bytes).unwrap();
            artifacts.push(json!({"ordinal":ordinal,"relativePath":relative(&name),"byteLength":bytes.len(),"sha256":sha256(&bytes),"language":language}));
        }
        let set_id = self.source_set["id"].as_str().unwrap();
        let mut hasher = SourceManifestHasher::new(set_id);
        for source in &sources {
            hasher.push(&source.language, &source.path, &source.content_hash);
        }
        let excluded = coverage_rows
            .iter()
            .filter(|r| r["artifact"].is_null())
            .count();
        let page_bytes: u64 = pages
            .iter()
            .map(|p| p["byteLength"].as_u64().unwrap())
            .sum();
        let artifact_bytes: u64 = artifacts
            .iter()
            .map(|a| a["byteLength"].as_u64().unwrap())
            .sum();
        let descriptor = json!({
            "formatVersion":1,
            "trustOrigin":self.producer.trust_origin,
            "profileId":self.producer.profile_id,
            "decoderVersion":DECODER_VERSION,
            "producer":{"name":self.producer.name,"version":self.producer.version,"identityKind":self.producer.identity_kind,
                "executableHash":self.producer.executable_hash,"positionEncoding":self.producer.position_encoding,"languages":self.producer.languages},
            "sourceSet":self.source_set,
            "sourceRevision":self.source_revision,
            "sourceManifestHash":hasher.finish(),
            "components":self.components,
            "metadata":{"protocolVersion":0,"projectRoot":format!("file://{}", self.root.display()),"textDocumentEncoding":1},
            "completion":if excluded == 0 {"complete"} else {"partial"},
            "totals":{"sourceDocuments":source_rows.len(),"coveredDocuments":coverage_rows.len()-excluded,"excludedDocuments":excluded,
                "pageBytes":page_bytes,"artifactBytes":artifact_bytes},
            "pages":pages,
            "artifacts":artifacts,
        });
        write_descriptor(dir, &descriptor)
    }
}

/// Writes `descriptor.json` into `dir`; returns its path and SHA-256.
pub fn write_descriptor(dir: &Path, descriptor: &Value) -> (PathBuf, String) {
    let bytes = serde_json::to_vec(descriptor).unwrap();
    let path = dir.join("descriptor.json");
    fs::write(&path, &bytes).unwrap();
    (path, sha256(&bytes))
}

/// Reads, edits and rewrites a descriptor, returning its new path and digest.
pub fn edit_descriptor(path: &Path, edit: impl FnOnce(&mut Value)) -> (PathBuf, String) {
    let mut descriptor: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    edit(&mut descriptor);
    write_descriptor(path.parent().unwrap(), &descriptor)
}

/// Rewrites one page file after `edit` and updates its descriptor entry (length and digest).
pub fn edit_page(
    descriptor: &Path,
    entry: usize,
    root: &Path,
    edit: impl FnOnce(&mut Value),
) -> (PathBuf, String) {
    let mut parsed: Value = serde_json::from_slice(&fs::read(descriptor).unwrap()).unwrap();
    let page_path = root.join(parsed["pages"][entry]["relativePath"].as_str().unwrap());
    let mut page: Value = serde_json::from_slice(&fs::read(&page_path).unwrap()).unwrap();
    edit(&mut page);
    let bytes = serde_json::to_vec(&page).unwrap();
    fs::write(&page_path, &bytes).unwrap();
    let old = parsed["pages"][entry]["byteLength"].as_u64().unwrap();
    let total = parsed["totals"]["pageBytes"].as_u64().unwrap();
    parsed["totals"]["pageBytes"] = json!(total - old + bytes.len() as u64);
    parsed["pages"][entry]["byteLength"] = json!(bytes.len());
    parsed["pages"][entry]["sha256"] = json!(sha256(&bytes));
    write_descriptor(descriptor.parent().unwrap(), &parsed)
}

fn write_workspace(
    dir: &Path,
    languages: &[&str],
    per_language: usize,
    path: impl Fn(&str, usize) -> String,
    body: impl Fn(&str, usize) -> String,
) {
    for language in languages {
        for number in 0..per_language {
            let file = dir.join(path(language, number));
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, body(language, number)).unwrap();
        }
    }
}

fn body(language: &str, number: usize) -> String {
    match language {
        "java" => format!("class U{number:05} {{ void f() {{ g(); }} }}\n"),
        "rust" => format!("fn u{number:05}() {{ g(); }}\n"),
        "python" => format!("def u{number:05}():\n    g()\n"),
        _ => format!("function u{number:05}() {{ g(); }}\n"),
    }
}

/// Zero-padded names and identically shaped bodies, so every document has equal length.
pub fn uniform_workspace(dir: &Path, languages: &[&str], per_language: usize) {
    write_workspace(
        dir,
        languages,
        per_language,
        |language, number| format!("{language}/u{number:05}.{}", extension(language)),
        body,
    );
}

/// Like `uniform_workspace` but each path carries a directory segment of `segment_len` bytes.
pub fn long_path_workspace(
    dir: &Path,
    languages: &[&str],
    per_language: usize,
    segment_len: usize,
) {
    let segment = "d".repeat(segment_len);
    write_workspace(
        dir,
        languages,
        per_language,
        |language, number| format!("{language}/{segment}/u{number:05}.{}", extension(language)),
        body,
    );
}
