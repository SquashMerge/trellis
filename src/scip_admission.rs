//! Producer registry and shared admission helpers for pinned semantic producers.
pub use crate::scip_raw::{
    CapturedClaim, CapturedScipFactV1, CapturedScipInventory, Completion, Exclusion,
    ScipCaptureError,
};
use crate::{
    semantic_batch::descriptor::{DescriptorProducer, IdentityKind},
    semantic_evidence::{self, Language, PositionEncoding, SemanticKind},
};
use serde::Deserialize;
use std::{path::Path, sync::OnceLock};

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
    #[serde(rename = "positionEncoding")]
    position_encoding: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyntheticProfile {
    profile_id: String,
    identity_kind: String,
    generator: SyntheticGenerator,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyntheticGenerator {
    name: String,
    version: String,
    source_sha256: String,
    position_encoding: String,
    languages: Vec<String>,
}

pub const REFERENCE_TRUST_ORIGIN: &str = "referenceProfile";
pub const SYNTHETIC_TRUST_ORIGIN: &str = "syntheticProfile";

/// A producer identity fixed by a checked-in profile, never by the descriptor that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedProducer {
    pub name: String,
    pub version: String,
    pub identity_kind: IdentityKind,
    /// For `generatorSource` identities this is the SHA-256 of the generator source.
    pub executable_hash: String,
    pub position_encoding: u8,
    pub languages: Vec<String>,
    pub trust_origin: &'static str,
    pub profile_id: String,
}

impl PinnedProducer {
    pub fn producer(&self) -> semantic_evidence::Producer {
        semantic_evidence::Producer {
            id: self.name.clone(),
            version: self.version.clone(),
            executable_hash: self.executable_hash.clone(),
            kind: SemanticKind::Semantic,
            languages: self
                .languages
                .iter()
                .filter_map(|language| semantic_language(language))
                .collect(),
            position_encoding: match self.position_encoding {
                1 => PositionEncoding::Utf8,
                2 => PositionEncoding::Utf16,
                _ => PositionEncoding::UnicodeScalar,
            },
        }
    }
}

struct Profiles {
    reference: Vec<PinnedProducer>,
    synthetic: PinnedProducer,
}

fn semantic_language(name: &str) -> Option<Language> {
    match name {
        "java" => Some(Language::Java),
        "rust" => Some(Language::Rust),
        "python" => Some(Language::Python),
        "javascript" => Some(Language::Javascript),
        _ => None,
    }
}

fn encoding_code(name: &str) -> Option<u8> {
    match name {
        "utf8" => Some(1),
        "utf16" => Some(2),
        "unicodeScalar" => Some(3),
        _ => None,
    }
}

/// The language of each real reference producer.
pub fn producer_language(producer_id: &str) -> Option<&'static str> {
    match producer_id {
        "scip-java" => Some("java"),
        "scip-typescript" => Some("javascript"),
        "scip-python" => Some("python"),
        "rust-analyzer" => Some("rust"),
        _ => None,
    }
}

/// The language of each producer the witness fast path accepts. `scip-java` is omitted on
/// purpose: its rejoin has always taken the FULL path, and that behavior is preserved.
pub fn witness_language(producer_id: &str) -> Option<&'static str> {
    match producer_id {
        "scip-java" => None,
        other => producer_language(other),
    }
}

fn profiles() -> Result<&'static Profiles, ScipCaptureError> {
    static PROFILES: OnceLock<Option<Profiles>> = OnceLock::new();
    PROFILES
        .get_or_init(load_profiles)
        .as_ref()
        .ok_or_else(|| admission("checked-in producer profile"))
}

fn load_profiles() -> Option<Profiles> {
    let hardware: Hardware = serde_json::from_str(include_str!(
        "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
    ))
    .ok()?;
    let synthetic: SyntheticProfile = serde_json::from_str(include_str!(
        "../docs/semantic-evidence/synthetic-profiles/trellis-synthetic-scip-v1.json"
    ))
    .ok()?;
    let reference = hardware
        .semantic_producers
        .iter()
        .filter_map(|producer| {
            let language = producer_language(&producer.name)?;
            Some(PinnedProducer {
                name: producer.name.clone(),
                version: producer.version.clone(),
                identity_kind: IdentityKind::Executable,
                executable_hash: producer.executable_hash.clone(),
                position_encoding: encoding_code(producer.position_encoding.as_deref()?)?,
                languages: vec![language.into()],
                trust_origin: REFERENCE_TRUST_ORIGIN,
                profile_id: hardware.profile_id.clone(),
            })
            .filter(|pinned| hexhash(&pinned.executable_hash))
        })
        .collect();
    let generator = synthetic.generator;
    if synthetic.identity_kind != "generatorSource"
        || !hexhash(&generator.source_sha256)
        || generator
            .languages
            .iter()
            .any(|language| semantic_language(language).is_none())
    {
        return None;
    }
    Some(Profiles {
        reference,
        synthetic: PinnedProducer {
            name: generator.name,
            version: generator.version,
            identity_kind: IdentityKind::GeneratorSource,
            executable_hash: generator.source_sha256,
            position_encoding: encoding_code(&generator.position_encoding)?,
            languages: generator.languages,
            trust_origin: SYNTHETIC_TRUST_ORIGIN,
            profile_id: synthetic.profile_id,
        },
    })
}

/// The pin for a producer id, as the store re-derives it from stored rows.
pub fn pinned_producer_by_id(producer_id: &str) -> Option<PinnedProducer> {
    let profiles = profiles().ok()?;
    if profiles.synthetic.name == producer_id {
        return Some(profiles.synthetic.clone());
    }
    profiles
        .reference
        .iter()
        .find(|pinned| pinned.name == producer_id)
        .cloned()
}

/// The checked-in pin that a declared producer must equal in every field.
pub fn pinned_producer(
    trust_origin: &str,
    profile_id: &str,
    declared: &DescriptorProducer,
) -> Result<PinnedProducer, ScipCaptureError> {
    let profiles = profiles()?;
    let pinned = match trust_origin {
        REFERENCE_TRUST_ORIGIN => profiles
            .reference
            .iter()
            .find(|pinned| pinned.name == declared.name)
            .ok_or_else(|| admission("producer not pinned"))?,
        SYNTHETIC_TRUST_ORIGIN => &profiles.synthetic,
        _ => return Err(admission("producer profile pin")),
    };
    if pinned.profile_id != profile_id
        || pinned.name != declared.name
        || pinned.version != declared.version
        || pinned.identity_kind != declared.identity_kind
        || pinned.executable_hash != declared.executable_hash
        || pinned.position_encoding != declared.position_encoding
        || pinned.languages != declared.languages
    {
        return Err(admission("producer profile pin"));
    }
    Ok(pinned.clone())
}

pub(crate) fn admission(s: &str) -> ScipCaptureError {
    ScipCaptureError::InvalidAdmission(s.into())
}
pub(crate) fn hexhash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['\\', '\0'])
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}
pub(crate) fn validate_selected_role(root: &Path, p: &Path) -> Result<String, ScipCaptureError> {
    if !p.is_absolute() {
        return Err(admission("selected role is not absolute"));
    }
    relative(root, p)
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
pub(crate) fn file_uri(root: &Path) -> Result<String, ScipCaptureError> {
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
pub(crate) fn order(language: &str) -> usize {
    match language {
        "java" => 0,
        "rust" => 1,
        "python" => 2,
        "javascript" => 3,
        _ => usize::MAX,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::scip_raw;
    use serde_json::Value;
    fn declared(pinned: &PinnedProducer) -> DescriptorProducer {
        DescriptorProducer {
            name: pinned.name.clone(),
            version: pinned.version.clone(),
            identity_kind: pinned.identity_kind,
            executable_hash: pinned.executable_hash.clone(),
            position_encoding: pinned.position_encoding,
            languages: pinned.languages.clone(),
        }
    }
    #[test]
    fn synthetic_profile_pins_generator_identity() {
        let synthetic = pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let profile: Value = serde_json::from_str(include_str!(
            "../docs/semantic-evidence/synthetic-profiles/trellis-synthetic-scip-v1.json"
        ))
        .unwrap();
        assert_eq!(synthetic.identity_kind, IdentityKind::GeneratorSource);
        assert_eq!(synthetic.trust_origin, "syntheticProfile");
        assert_eq!(synthetic.profile_id, "trellis-synthetic-scip-v1");
        assert_eq!(synthetic.version, "1");
        assert_eq!(synthetic.position_encoding, 1);
        assert_eq!(
            synthetic.languages,
            ["java", "rust", "python", "javascript"]
        );
        assert_eq!(
            synthetic.executable_hash,
            profile["generator"]["sourceSha256"].as_str().unwrap()
        );
        assert_eq!(
            synthetic.executable_hash,
            scip_raw::sha(include_bytes!(
                "../tools/semantic-contract/generate-performance-scip.py"
            ))
        );
        let producer = synthetic.producer();
        assert_eq!(producer.id, "trellis-synthetic-scip");
        assert_eq!(producer.executable_hash, synthetic.executable_hash);
        assert_eq!(producer.languages.len(), 4);
        assert_eq!(producer.position_encoding, PositionEncoding::Utf8);
        assert_eq!(
            pinned_producer(
                "syntheticProfile",
                &synthetic.profile_id,
                &declared(&synthetic)
            )
            .unwrap(),
            synthetic
        );
    }
    #[test]
    fn reference_profile_producers_are_unchanged() {
        let profile: Hardware = serde_json::from_str(include_str!(
            "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
        ))
        .unwrap();
        for (id, language, semantic_language) in [
            ("scip-java", "java", Language::Java),
            ("scip-typescript", "javascript", Language::Javascript),
            ("scip-python", "python", Language::Python),
            ("rust-analyzer", "rust", Language::Rust),
        ] {
            let row = profile
                .semantic_producers
                .iter()
                .find(|p| p.name == id)
                .unwrap();
            let pinned = pinned_producer_by_id(id).unwrap();
            let encoding = match row.position_encoding.as_deref().unwrap() {
                "utf8" => (1, PositionEncoding::Utf8),
                "utf16" => (2, PositionEncoding::Utf16),
                _ => (3, PositionEncoding::UnicodeScalar),
            };
            assert_eq!(producer_language(id), Some(language));
            assert_eq!(pinned.identity_kind, IdentityKind::Executable);
            assert_eq!(pinned.trust_origin, "referenceProfile");
            assert_eq!(pinned.profile_id, profile.profile_id);
            assert_eq!(pinned.position_encoding, encoding.0);
            assert_eq!(pinned.languages, [language]);
            assert_eq!(
                pinned.producer(),
                semantic_evidence::Producer {
                    id: id.into(),
                    version: row.version.clone(),
                    executable_hash: row.executable_hash.clone(),
                    kind: SemanticKind::Semantic,
                    languages: vec![semantic_language],
                    position_encoding: encoding.1,
                }
            );
            assert_eq!(
                pinned_producer("referenceProfile", &profile.profile_id, &declared(&pinned))
                    .unwrap(),
                pinned
            );
        }
        assert_eq!(pinned_producer_by_id("scip-unknown"), None);
        assert_eq!(producer_language("scip-unknown"), None);
    }
    #[test]
    fn synthetic_and_reference_identities_cannot_be_crossed() {
        let synthetic = pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let reference = pinned_producer_by_id("rust-analyzer").unwrap();
        let pin = |origin: &str, profile: &str, producer: &DescriptorProducer| match pinned_producer(
            origin, profile, producer,
        ) {
            Err(ScipCaptureError::InvalidAdmission(detail)) => detail,
            other => panic!("expected an admission error, got {other:?}"),
        };
        assert_eq!(
            pin(
                "referenceProfile",
                &synthetic.profile_id,
                &declared(&synthetic)
            ),
            "producer not pinned"
        );
        assert_eq!(
            pin(
                "syntheticProfile",
                &reference.profile_id,
                &declared(&synthetic)
            ),
            "producer profile pin"
        );
        assert_eq!(
            pin(
                "syntheticProfile",
                &synthetic.profile_id,
                &declared(&reference)
            ),
            "producer profile pin"
        );
        let mut kind = declared(&synthetic);
        kind.identity_kind = IdentityKind::Executable;
        assert_eq!(
            pin("syntheticProfile", &synthetic.profile_id, &kind),
            "producer profile pin"
        );
        let mut kind = declared(&reference);
        kind.identity_kind = IdentityKind::GeneratorSource;
        assert_eq!(
            pin("referenceProfile", &reference.profile_id, &kind),
            "producer profile pin"
        );
        let mut languages = declared(&synthetic);
        languages.languages.reverse();
        assert_eq!(
            pin("syntheticProfile", &synthetic.profile_id, &languages),
            "producer profile pin"
        );
        assert_eq!(
            pin("localProfile", &reference.profile_id, &declared(&reference)),
            "producer profile pin"
        );
    }
    #[test]
    fn witness_language_keeps_the_fast_path_closure() {
        assert_eq!(witness_language("scip-typescript"), Some("javascript"));
        assert_eq!(witness_language("scip-python"), Some("python"));
        assert_eq!(witness_language("rust-analyzer"), Some("rust"));
        assert_eq!(witness_language("scip-java"), None);
        assert_eq!(witness_language("trellis-synthetic-scip"), None);
    }
}
