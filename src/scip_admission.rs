//! A trusted selection binds original captured bytes to native source identity and a checked-in producer.
pub use crate::scip_raw::{
    CapturedClaim, CapturedScipFactV1, CapturedScipInventory, Completion, Exclusion,
    ScipCaptureError,
};
use crate::{
    capture::Capture,
    model::CancelFlag,
    native_evidence::captured_revision_header,
    native_ids::canonical,
    scip_raw::{self, AuthenticatedScipDocument, AuthenticatedScipInput},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::atomic::Ordering,
};

#[derive(Debug, Clone)]
pub struct TrustedSelection {
    pub expected_manifest_hash: String,
    pub profile_id: String,
    pub producer_name: String,
}
pub struct ReferenceProfileAdmission<'a> {
    input: AuthenticatedScipInput<'a>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    format_version: u8,
    trust_origin: String,
    completion: Completion,
    decoder_version: String,
    profile_id: String,
    producer: Producer,
    source_set: SourceSet,
    source_revision: String,
    source_manifest_hash: String,
    components: Components,
    metadata: Metadata,
    source_documents: Vec<Doc>,
    documents: Vec<Doc>,
    exclusions: Vec<Exclusion>,
    artifacts: Vec<Artifact>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Producer {
    name: String,
    version: String,
    executable_hash: String,
    position_encoding: u8,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceSet {
    id: String,
    root_id: String,
    languages: Vec<String>,
    dependencies: Vec<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Components {
    toolchain_hash: String,
    config_hash: String,
    dependency_hash: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Metadata {
    protocol_version: i32,
    project_root: String,
    text_document_encoding: i32,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Doc {
    language: String,
    relative_path: String,
    content_hash: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Artifact {
    kind: String,
    path: String,
    hash: String,
    language: String,
    documents: Vec<String>,
}
#[derive(Deserialize)]
struct Hardware {
    #[serde(rename = "profileId")]
    profile_id: String,
    #[serde(rename = "semanticProducers")]
    semantic_producers: Vec<HardwareProducer>,
}
#[derive(Deserialize)]
struct HardwareProducer {
    name: String,
    version: String,
    #[serde(rename = "executableHash")]
    executable_hash: String,
}
fn admission(s: &str) -> ScipCaptureError {
    ScipCaptureError::InvalidAdmission(s.into())
}
fn hexhash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['\\', '\0'])
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}
fn relative(root: &Path, p: &Path) -> Result<String, ScipCaptureError> {
    let r = p
        .strip_prefix(root)
        .map_err(|_| admission("role outside root"))?
        .to_str()
        .ok_or_else(|| admission("non-UTF8 role"))?;
    if !path(r) {
        return Err(admission("invalid role path"));
    }
    let mut current = root.to_owned();
    for component in r.split('/') {
        current.push(component);
        let metadata =
            std::fs::symlink_metadata(&current).map_err(|_| admission("selected role absent"))?;
        if metadata.file_type().is_symlink() {
            return Err(admission("symlinked selected role"));
        }
    }
    Ok(r.into())
}
fn file_uri(root: &Path) -> Result<String, ScipCaptureError> {
    let s = root
        .to_str()
        .filter(|s| s.starts_with('/'))
        .ok_or_else(|| admission("non-UTF8 physical root"))?;
    let mut out = String::from("file://");
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(out)
}
fn order(language: &str) -> usize {
    match language {
        "java" => 0,
        "rust" => 1,
        "python" => 2,
        "javascript" => 3,
        _ => usize::MAX,
    }
}
fn sorted(rows: &[Doc]) -> bool {
    rows.windows(2).all(|w| {
        (order(&w[0].language), w[0].relative_path.as_bytes())
            < (order(&w[1].language), w[1].relative_path.as_bytes())
    })
}
fn checked_docs(rows: &[Doc]) -> Result<(), ScipCaptureError> {
    if !sorted(rows)
        || rows.iter().any(|d| {
            order(&d.language) == usize::MAX || !path(&d.relative_path) || !hexhash(&d.content_hash)
        })
    {
        return Err(admission("invalid document list"));
    }
    Ok(())
}
impl<'a> ReferenceProfileAdmission<'a> {
    pub fn from_trusted_selection(
        capture: &'a Capture,
        selection: &TrustedSelection,
        cancel: &CancelFlag,
    ) -> Result<Self, ScipCaptureError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(ScipCaptureError::Cancelled);
        }
        let root = capture.physical_root();
        let (artifact_path, manifest_path) = capture
            .selected_scip_inputs()
            .ok_or_else(|| admission("SCIP roles"))?;
        let artifact_bytes = capture
            .bytes(artifact_path)
            .ok_or_else(|| admission("artifact role"))?;
        let manifest_bytes = capture
            .bytes(manifest_path)
            .ok_or_else(|| admission("manifest role"))?;
        if manifest_bytes.len() > 1024 * 1024 || artifact_bytes.len() > 64 * 1024 * 1024 {
            return Err(ScipCaptureError::LimitExceeded("admitted bytes".into()));
        }
        if !hexhash(&selection.expected_manifest_hash)
            || scip_raw::sha(manifest_bytes) != selection.expected_manifest_hash
        {
            return Err(admission("expected manifest digest"));
        }
        let manifest: Manifest =
            serde_json::from_slice(manifest_bytes).map_err(|_| admission("closed manifest"))?;
        let artifact_relative = relative(root, artifact_path)?;
        let _ = relative(root, manifest_path)?;
        if manifest.format_version != 1
            || manifest.trust_origin != "referenceProfile"
            || manifest.decoder_version != scip_raw::DECODER_VERSION
            || manifest.profile_id != selection.profile_id
            || manifest.producer.name != selection.producer_name
        {
            return Err(admission("manifest identity"));
        }
        let profile: Hardware = serde_json::from_str(include_str!(
            "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
        ))
        .map_err(|_| admission("checked-in producer profile"))?;
        let (language, encoding) = match selection.producer_name.as_str() {
            "scip-java" => ("java", 2),
            "scip-typescript" => ("javascript", 2),
            "scip-python" => ("python", 2),
            "rust-analyzer" => ("rust", 1),
            _ => return Err(admission("unsupported producer")),
        };
        let pinned = profile
            .semantic_producers
            .iter()
            .find(|p| p.name == selection.producer_name)
            .ok_or_else(|| admission("producer not pinned"))?;
        if profile.profile_id != selection.profile_id
            || manifest.producer.version != pinned.version
            || manifest.producer.executable_hash != pinned.executable_hash
            || !hexhash(&pinned.executable_hash)
            || manifest.producer.position_encoding != encoding
        {
            return Err(admission("producer profile pin"));
        }
        let identity = crate::store::topology::WorkspaceIdentity::discover(Some(root), root)
            .map_err(|_| admission("workspace identity"))?;
        identity
            .verify()
            .map_err(|_| admission("workspace identity changed"))?;
        let (source_set, revision) = captured_revision_header(capture, root, &identity.record_id)
            .map_err(|_| admission("native captured header"))?;
        if source_set.id != revision.source_set_id
            || manifest.source_set.id != source_set.id
            || manifest.source_set.root_id != source_set.root_id
            || manifest.source_set.languages != source_set.languages
            || manifest.source_set.dependencies != source_set.dependencies
            || manifest.source_revision != revision.id
            || manifest.components.toolchain_hash != revision.toolchain_hash
            || manifest.components.config_hash != revision.config_hash
            || manifest.components.dependency_hash != revision.dependency_hash
            || ![
                &manifest.components.toolchain_hash,
                &manifest.components.config_hash,
                &manifest.components.dependency_hash,
            ]
            .iter()
            .all(|h| hexhash(h))
        {
            return Err(admission("native revision components"));
        }
        let uri = file_uri(root)?;
        if manifest.metadata.project_root != uri
            || manifest.metadata.protocol_version != 0
            || manifest.metadata.text_document_encoding != 1
        {
            return Err(admission("metadata binding"));
        }
        checked_docs(&manifest.source_documents)?;
        checked_docs(&manifest.documents)?;
        if manifest.source_documents.len() != capture.files.len()
            || manifest.source_documents.len() != revision.documents.len()
            || manifest.documents.is_empty()
            || manifest.documents.len() > 1000
        {
            return Err(admission("document count"));
        }
        if capture.files.len() > 100000 {
            return Err(ScipCaptureError::LimitExceeded("source files".into()));
        }
        let files: BTreeMap<_, _> = capture
            .files
            .iter()
            .map(|f| ((f.language.as_str(), f.path.as_str()), f))
            .collect();
        let rev: BTreeMap<_, _> = revision
            .documents
            .iter()
            .map(|d| ((d.key.language.as_str(), d.key.path.as_str()), d))
            .collect();
        if files.len() != capture.files.len() || rev.len() != revision.documents.len() {
            return Err(admission("duplicate captured source"));
        }
        let mut source_keys = BTreeSet::new();
        let mut source_paths = BTreeSet::new();
        let mut source_hashes = Vec::new();
        for d in &manifest.source_documents {
            if cancel.load(Ordering::Relaxed) {
                return Err(ScipCaptureError::Cancelled);
            }
            if !source_paths.insert(d.relative_path.as_str()) {
                return Err(admission("duplicate source path"));
            }
            let key = (d.language.as_str(), d.relative_path.as_str());
            let f = files
                .get(&key)
                .ok_or_else(|| admission("missing captured source"))?;
            let r = rev
                .get(&key)
                .ok_or_else(|| admission("missing revision source"))?;
            if f.hash != d.content_hash
                || r.content_hash != d.content_hash
                || r.key.source_set_id != source_set.id
                || r.revision_id != revision.id
                || r.byte_length != f.text.len()
                || scip_raw::sha(f.text.as_bytes()) != d.content_hash
                || !source_keys.insert(key)
            {
                return Err(admission("captured source hash"));
            }
            source_hashes.push(json!({"document":{"sourceSetId":source_set.id,"language":d.language,"path":d.relative_path},"contentHash":d.content_hash}));
        }
        if scip_raw::sha(&canonical(&json!(source_hashes))) != manifest.source_manifest_hash
            || !hexhash(&manifest.source_manifest_hash)
        {
            return Err(admission("source manifest hash"));
        }
        let artifact = manifest
            .artifacts
            .first()
            .filter(|_| manifest.artifacts.len() == 1)
            .ok_or_else(|| admission("artifact count"))?;
        if artifact.kind != "scipBinary"
            || artifact.path != artifact_relative
            || artifact.hash != scip_raw::sha(artifact_bytes)
            || artifact.language != language
        {
            return Err(admission("artifact identity"));
        }
        let mut covered = BTreeSet::new();
        let mut doc_paths = BTreeSet::new();
        let mut docs = Vec::new();
        for d in &manifest.documents {
            let key = (d.language.as_str(), d.relative_path.as_str());
            let f = files
                .get(&key)
                .ok_or_else(|| admission("unlisted covered source"))?;
            if d.language != language
                || d.content_hash != f.hash
                || !doc_paths.insert(d.relative_path.as_str())
                || !source_keys.contains(&key)
            {
                return Err(admission("covered document"));
            }
            covered.insert(key);
            docs.push(AuthenticatedScipDocument {
                language: d.language.clone(),
                relative_path: d.relative_path.clone(),
                source_bytes: f.text.as_bytes(),
                content_hash: d.content_hash.clone(),
            });
        }
        if artifact
            .documents
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            != manifest
                .documents
                .iter()
                .map(|d| d.relative_path.as_str())
                .collect::<Vec<_>>()
        {
            return Err(admission("artifact document list"));
        }
        let mut excluded = BTreeSet::new();
        for e in &manifest.exclusions {
            let key = (e.language.as_str(), e.relative_path.as_str());
            if e.language != language
                || !path(&e.relative_path)
                || !matches!(e.reason.as_str(), "notRequested" | "unsupported" | "failed")
                || !source_keys.contains(&key)
                || covered.contains(&key)
                || !excluded.insert(key)
            {
                return Err(admission("exclusion"));
            }
        }
        if manifest
            .exclusions
            .windows(2)
            .any(|w| w[0].relative_path.as_bytes() >= w[1].relative_path.as_bytes())
            || source_keys
                .iter()
                .filter(|(lang, _)| *lang == language)
                .any(|k| !covered.contains(k) && !excluded.contains(k))
            || (manifest.completion == Completion::Complete) != manifest.exclusions.is_empty()
        {
            return Err(admission("coverage completion"));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(ScipCaptureError::Cancelled);
        }
        Ok(Self {
            input: AuthenticatedScipInput {
                artifact_bytes,
                artifact_hash: artifact.hash.clone(),
                producer_id: pinned.name.clone(),
                profile_id: profile.profile_id,
                source_set_id: source_set.id,
                source_revision: revision.id,
                source_manifest_hash: manifest.source_manifest_hash,
                decoder_version: scip_raw::DECODER_VERSION,
                completion: manifest.completion,
                exclusions: manifest.exclusions,
                project_root_uri: uri,
                producer_version: pinned.version.clone(),
                position_encoding: encoding,
                protocol_version: 0,
                text_document_encoding: 1,
                documents: docs,
            },
        })
    }
}
pub fn decode_reference_profile(
    admission: &ReferenceProfileAdmission<'_>,
    cancel: &CancelFlag,
) -> Result<CapturedScipInventory, ScipCaptureError> {
    scip_raw::decode_authenticated(&admission.input, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::IndexOptions;
    use serde_json::Value;
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    const WIRE: &str = "122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166";
    fn fixture() -> (
        tempfile::TempDir,
        Capture,
        TrustedSelection,
        Value,
        CancelFlag,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let canonical_root = fs::canonicalize(dir.path()).unwrap();
        let root = canonical_root.as_path();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/A.js"), "function f(){g();}\n").unwrap();
        fs::write(root.join("artifact.scip"), hex::decode(WIRE).unwrap()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut options = IndexOptions::new(root.to_owned());
        options.scip_path = Some(root.join("artifact.scip"));
        options.manifest_path = Some(root.join("manifest.json"));
        let initial = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let identity = crate::store::topology::WorkspaceIdentity::discover(
            Some(initial.physical_root()),
            initial.physical_root(),
        )
        .unwrap();
        let (set, rev) =
            captured_revision_header(&initial, initial.physical_root(), &identity.record_id)
                .unwrap();
        let f = &initial.files[0];
        let profile: Hardware = serde_json::from_str(include_str!(
            "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
        ))
        .unwrap();
        let producer = profile
            .semantic_producers
            .iter()
            .find(|p| p.name == "scip-typescript")
            .unwrap();
        let doc = json!({"language":"javascript","relativePath":"src/A.js","contentHash":f.hash});
        let manifest_hash = scip_raw::sha(&canonical(
            &json!([{"document":{"sourceSetId":set.id,"language":"javascript","path":"src/A.js"},"contentHash":f.hash}]),
        ));
        let manifest = json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":"complete","decoderVersion":scip_raw::DECODER_VERSION,"profileId":profile.profile_id,
            "producer":{"name":producer.name,"version":producer.version,"executableHash":producer.executable_hash,"positionEncoding":2},
            "sourceSet":{"id":set.id,"rootId":set.root_id,"languages":set.languages,"dependencies":set.dependencies},
            "sourceRevision":rev.id,"sourceManifestHash":manifest_hash,
            "components":{"toolchainHash":rev.toolchain_hash,"configHash":rev.config_hash,"dependencyHash":rev.dependency_hash},
            "metadata":{"protocolVersion":0,"projectRoot":file_uri(initial.physical_root()).unwrap(),"textDocumentEncoding":1},
            "sourceDocuments":[doc],"documents":[doc],"exclusions":[],
            "artifacts":[{"kind":"scipBinary","path":"artifact.scip","hash":scip_raw::sha(&hex::decode(WIRE).unwrap()),"language":"javascript","documents":["src/A.js"]}]});
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(root.join("manifest.json"), &bytes).unwrap();
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let selection = TrustedSelection {
            expected_manifest_hash: scip_raw::sha(&bytes),
            profile_id: "mac-mini-m5-pro-v1".into(),
            producer_name: "scip-typescript".into(),
        };
        (dir, capture, selection, manifest, cancel)
    }
    #[test]
    fn scip_trusted_capture_and_unattested_changes() {
        let (root, capture, selection, manifest, cancel) = fixture();
        let admitted =
            ReferenceProfileAdmission::from_trusted_selection(&capture, &selection, &cancel)
                .unwrap();
        let inventory = decode_reference_profile(&admitted, &cancel).unwrap();
        assert_eq!(inventory.claims.len(), 2);
        assert_eq!(
            inventory.artifact_hash,
            scip_raw::sha(&hex::decode(WIRE).unwrap())
        );
        let mut wrong = selection.clone();
        wrong.expected_manifest_hash = "0".repeat(64);
        assert!(matches!(
            ReferenceProfileAdmission::from_trusted_selection(&capture, &wrong, &cancel),
            Err(ScipCaptureError::InvalidAdmission(_))
        ));
        wrong = selection.clone();
        wrong.producer_name = "rust-analyzer".into();
        assert!(
            ReferenceProfileAdmission::from_trusted_selection(&capture, &wrong, &cancel).is_err()
        );
        for (pointer, value) in [
            ("/producer/executableHash", json!("0".repeat(64))),
            ("/sourceDocuments/0/contentHash", json!("1".repeat(64))),
            ("/components/configHash", json!("2".repeat(64))),
            ("/components/toolchainHash", json!("2".repeat(64))),
            ("/components/dependencyHash", json!("2".repeat(64))),
            ("/sourceRevision", json!("revision:v2:wrong")),
            ("/sourceManifestHash", json!("4".repeat(64))),
            ("/sourceSet/rootId", json!("other")),
            ("/artifacts/0/hash", json!("3".repeat(64))),
            ("/artifacts/0/path", json!("../escape")),
            ("/metadata/projectRoot", json!("file:///wrong")),
            ("/producer/positionEncoding", json!(3)),
            ("/producer/version", json!("untrusted-version")),
            ("/profileId", json!("other-profile")),
            ("/completion", json!("partial")),
            (
                "/exclusions",
                json!([{"language":"javascript","relativePath":"src/A.js","reason":"failed"}]),
            ),
        ] {
            let mut changed = manifest.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            let bytes = serde_json::to_vec(&changed).unwrap();
            fs::write(root.path().join("manifest.json"), &bytes).unwrap();
            let expected = TrustedSelection {
                expected_manifest_hash: scip_raw::sha(&bytes),
                ..selection.clone()
            };
            let recaptured = Capture::admit(
                &IndexOptions {
                    workspace_root: fs::canonicalize(root.path()).unwrap(),
                    scip_path: Some(fs::canonicalize(root.path()).unwrap().join("artifact.scip")),
                    manifest_path: Some(
                        fs::canonicalize(root.path()).unwrap().join("manifest.json"),
                    ),
                    max_file_bytes: 2 * 1024 * 1024,
                },
                &cancel,
                &|_| {},
            )
            .unwrap();
            assert!(
                ReferenceProfileAdmission::from_trusted_selection(&recaptured, &expected, &cancel)
                    .is_err(),
                "unexpected admitted mutated {pointer}"
            );
            assert!(
                ReferenceProfileAdmission::from_trusted_selection(&recaptured, &selection, &cancel)
                    .is_err(),
                "unattested mutated {pointer}"
            );
        }
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            ReferenceProfileAdmission::from_trusted_selection(&capture, &selection, &cancel),
            Err(ScipCaptureError::Cancelled)
        ));
        assert!(matches!(
            decode_reference_profile(&admitted, &cancel),
            Err(ScipCaptureError::Cancelled)
        ));
    }
    #[test]
    fn scip_path_and_uri_reject_escapes() {
        assert!(relative(Path::new("/safe"), Path::new("/outside/file")).is_err());
        assert!(relative(Path::new("/safe"), Path::new("/safe/../other")).is_err());
        assert!(!path("/absolute"));
        assert!(!path("../escape"));
        assert!(!path("link/../escape"));
        assert_eq!(
            file_uri(Path::new("/tmp/a b")).unwrap(),
            "file:///tmp/a%20b"
        );
    }
    #[test]
    fn scip_manifest_original_bytes_limit_precedes_parse() {
        let (root, _, selection, _, cancel) = fixture();
        fs::write(
            root.path().join("manifest.json"),
            vec![b' '; 1024 * 1024 + 1],
        )
        .unwrap();
        let mut options = IndexOptions::new(root.path().to_owned());
        options.scip_path = Some(root.path().join("artifact.scip"));
        options.manifest_path = Some(root.path().join("manifest.json"));
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        assert!(matches!(
            ReferenceProfileAdmission::from_trusted_selection(&capture, &selection, &cancel),
            Err(ScipCaptureError::LimitExceeded(_))
        ));
    }
    #[test]
    fn scip_admission_rejects_captured_symlinked_ancestor_role() {
        let (root, _, selection, _, cancel) = fixture();
        let outside = tempfile::tempdir().unwrap();
        fs::write(
            outside.path().join("artifact.scip"),
            hex::decode(WIRE).unwrap(),
        )
        .unwrap();
        let physical = fs::canonicalize(root.path()).unwrap();
        std::os::unix::fs::symlink(outside.path(), physical.join("linked")).unwrap();
        let options = IndexOptions {
            workspace_root: physical.clone(),
            scip_path: Some(physical.join("linked/artifact.scip")),
            manifest_path: Some(physical.join("manifest.json")),
            max_file_bytes: 2 * 1024 * 1024,
        };
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        assert!(
            capture.selected_scip_inputs().is_some(),
            "ancestor leaf bytes were captured"
        );
        assert!(matches!(
            ReferenceProfileAdmission::from_trusted_selection(&capture, &selection, &cancel),
            Err(ScipCaptureError::InvalidAdmission(_))
        ));
    }
    #[test]
    fn scip_capture_rejects_symlinked_role_and_missing_pair() {
        let (root, _, _, _, cancel) = fixture();
        let link = root.path().join("symlink.scip");
        std::os::unix::fs::symlink(root.path().join("artifact.scip"), &link).unwrap();
        let mut options = IndexOptions::new(root.path().to_owned());
        options.scip_path = Some(link);
        options.manifest_path = Some(root.path().join("manifest.json"));
        assert!(Capture::admit(&options, &cancel, &|_| {}).is_err());
        options.scip_path = None;
        let without = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        assert!(without.selected_scip_inputs().is_none());
    }
}
