//! Language-neutral static possible-path sequence contract.
//! Adapters consume cached source and measured calls. This is not a runtime trace.
use crate::model::{CallSite, IndexPin, Resolution, SourceFile, SourceRange, Symbol};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SequenceView {
    pub revision: IndexPin,
    pub seed: Symbol,
    pub participants: Vec<Participant>,
    pub steps: Vec<SequenceStep>,
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub semantic_warnings: Vec<crate::graph_query::Warning>,
    pub hidden_steps: usize,
    pub truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Participant {
    pub id: String,
    pub label: String,
    pub kind: String,
    #[serde(default)]
    pub identification: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration: Option<Symbol>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SequenceStep {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub path: String,
    pub range: SourceRange,
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_evidence: Option<CallEvidence>,
    pub children: Vec<SequenceStep>,
    pub alternate: Vec<SequenceStep>,
    pub hidden: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CallEvidence {
    pub boundary_reason: crate::graph_query::BoundaryReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<crate::semantic_evidence::Resolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<crate::semantic_evidence::Dispatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<crate::semantic_evidence::Freshness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance_id: Option<String>,
    /// The package an `external` binding's SCIP symbol names (#450). A producer claim
    /// for in-context library lookup, never a resolved library declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_package: Option<ExternalPackage>,
}
/// The `<manager> <name> <version>` package of a global SCIP symbol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalPackage {
    pub manager: String,
    pub name: String,
    pub version: Option<String>,
}
impl ExternalPackage {
    /// Reads `<scheme> <manager> <name> <version> <descriptors>`: a doubled space escapes
    /// a space and `.` is an empty field. Local or malformed symbols, and symbols without
    /// a manager or name, carry no package.
    pub fn from_scip(symbol: &str) -> Option<Self> {
        if symbol.starts_with("local ") {
            return None;
        }
        let mut fields: Vec<String> = Vec::with_capacity(4);
        let mut field = String::new();
        let mut chars = symbol.char_indices().peekable();
        while let Some((at, ch)) = chars.next() {
            if ch != ' ' {
                field.push(ch);
            } else if chars.peek().is_some_and(|(_, next)| *next == ' ') {
                chars.next();
                field.push(' ');
            } else {
                fields.push(std::mem::take(&mut field));
                if fields.len() == 4 {
                    // One or more well-formed descriptors must follow the package.
                    if !scip_descriptors(&symbol[at + 1..]) {
                        return None;
                    }
                    let present =
                        |value: &str| (!value.is_empty() && value != ".").then(|| value.to_owned());
                    if fields[0].is_empty() {
                        return None;
                    }
                    return Some(Self {
                        manager: present(&fields[1])?,
                        name: present(&fields[2])?,
                        version: present(&fields[3]),
                    });
                }
            }
        }
        None
    }
}
/// Whether `tail` is one or more SCIP descriptors and nothing else:
/// `name/` `name#` `name.` `name:` `name!` `name(disambiguator?).` `[name]` `(name)`,
/// where a name is `[_+$A-Za-z0-9-]+` or a backtick-quoted identifier with `` `` `` for a
/// backtick. Linear in `tail`; never panics.
fn scip_descriptors(tail: &str) -> bool {
    fn simple(bytes: &[u8], mut at: usize) -> Option<usize> {
        let start = at;
        while at < bytes.len()
            && (bytes[at].is_ascii_alphanumeric() || matches!(bytes[at], b'_' | b'+' | b'-' | b'$'))
        {
            at += 1;
        }
        (at > start).then_some(at)
    }
    fn name(bytes: &[u8], at: usize) -> Option<usize> {
        if bytes.get(at) != Some(&b'`') {
            return simple(bytes, at);
        }
        let mut at = at + 1;
        let mut empty = true;
        loop {
            match bytes.get(at)? {
                b'`' if bytes.get(at + 1) == Some(&b'`') => at += 2,
                b'`' => return (!empty).then_some(at + 1),
                _ => at += 1,
            }
            empty = false;
        }
    }
    let bytes = tail.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        at = match bytes[at] {
            b'[' => match name(bytes, at + 1) {
                Some(end) if bytes.get(end) == Some(&b']') => end + 1,
                _ => return false,
            },
            b'(' => match name(bytes, at + 1) {
                Some(end) if bytes.get(end) == Some(&b')') => end + 1,
                _ => return false,
            },
            _ => {
                let Some(end) = name(bytes, at) else {
                    return false;
                };
                match bytes.get(end) {
                    Some(b'/' | b'#' | b'.' | b':' | b'!') => end + 1,
                    Some(b'(') => {
                        let close = simple(bytes, end + 1).unwrap_or(end + 1);
                        if bytes.get(close..close + 2) != Some(b").") {
                            return false;
                        }
                        close + 2
                    }
                    _ => return false,
                }
            }
        };
    }
    at > 0
}
pub fn build_sequence(
    revision: IndexPin,
    seed: &Symbol,
    file: &SourceFile,
    calls: &[CallSite],
    show_all: bool,
) -> Result<SequenceView> {
    match file.language.as_str() {
        "javascript" => crate::behavior_js::build(revision, seed, file, calls, show_all),
        "rust" => crate::behavior_rust::build(revision, seed, file, calls, show_all),
        "java" => crate::behavior_java::build(revision, seed, file, calls, show_all),
        "python" => crate::behavior_python::build(revision, seed, file, calls, show_all),
        _ => bail!("unsupported sequence language: {}", file.language),
    }
}

#[cfg(test)]
mod external_package_tests {
    use super::ExternalPackage;

    fn package(manager: &str, name: &str, version: Option<&str>) -> Option<ExternalPackage> {
        Some(ExternalPackage {
            manager: manager.into(),
            name: name.into(),
            version: version.map(Into::into),
        })
    }

    #[test]
    fn reads_the_package_field_of_global_scip_symbols() {
        assert_eq!(
            ExternalPackage::from_scip("rust-analyzer cargo serde_json 1.0.140 de/from_str()."),
            package("cargo", "serde_json", Some("1.0.140"))
        );
        assert_eq!(
            ExternalPackage::from_scip("scip-java maven com.google  guava . Lists#"),
            package("maven", "com.google guava", None)
        );
        // Every descriptor kind, escaped names and method disambiguators.
        for tail in [
            "candidate_only().",
            "std/io/Read#read(+1).",
            "Vec#[T]push().(value)",
            "`impl Foo for Bar`#meta:mac!",
            "`a``b`#",
        ] {
            assert_eq!(
                ExternalPackage::from_scip(&format!(
                    "rust-analyzer cargo local-library 0.1.0 {tail}"
                )),
                package("cargo", "local-library", Some("0.1.0")),
                "{tail}"
            );
        }
    }

    #[test]
    fn local_malformed_and_nameless_symbols_carry_no_package() {
        for symbol in [
            "local 7",
            "pkg outside",
            "rust-analyzer cargo serde 1.0",
            "rust-analyzer cargo serde 1.0 ",
            "rust-analyzer . serde 1.0 x().",
            "rust-analyzer cargo . 1.0 x().",
            " cargo serde 1.0 x().",
            // Descriptor tails must be complete SCIP descriptors (#508 review).
            "rust-analyzer cargo local-library 0.1.0 definitely-not-a-descriptor",
            "rust-analyzer cargo local-library 0.1.0 x().@",
            "rust-analyzer cargo local-library 0.1.0 x(",
            "rust-analyzer cargo local-library 0.1.0 [T",
            "rust-analyzer cargo local-library 0.1.0 ``#",
            "rust-analyzer cargo local-library 0.1.0 `open.",
            "rust-analyzer cargo local-library 0.1.0 a/ b#",
            "",
        ] {
            assert_eq!(ExternalPackage::from_scip(symbol), None, "{symbol:?}");
        }
    }
}
