//! Bounded, read-only architecture rules over one admitted source snapshot.
use crate::native_evidence::{Declaration, Range};
#[cfg(test)]
use crate::references::Snapshot;
#[cfg(test)]
use crate::references::work::{self, Work};
use anyhow::{Result, ensure};
use globset::GlobSet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use toml::Spanned;

mod apps;
mod declarations;
mod evaluate;
mod glob_language;
mod parse;

pub(crate) use apps::{
    AppAssignment, AppScope, PATH_APPS, UNKNOWN_APP, app_candidates, file_apps, hidden_layer,
    hidden_layer_paths, path_apps, prompt_apps, unruled_apps,
};
#[cfg(test)]
use apps::{LayerMatcher, effective_apps, validate_apps};
#[cfg(test)]
use declarations::evaluate_document;
#[cfg(feature = "test-support")]
pub use evaluate::evaluate_with_workers;
pub use evaluate::{evaluate, explain};
pub(crate) use evaluate::{evaluate_prepared, evaluate_with_layers, prepare};
#[cfg(test)]
use evaluate::{portable_source_set, semantic_evidence};
pub(crate) use parse::load_test_features;
pub use parse::{descriptor, load, parse};

pub const RULES_PATH: &str = ".trellis/rules.toml";
pub(crate) const CONFIG_BYTES: usize = 2 * 1024 * 1024;
const COLLECTION_LIMIT: usize = 1024;
const PATTERN_LIMIT: usize = 8192;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleError {
    pub code: String,
    pub kind: String,
    pub path: String,
    pub range: Range,
    pub line: usize,
    pub column: usize,
    pub attribution: String,
    pub message: String,
}
impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}:{}:{}: {}",
            self.code, self.path, self.line, self.column, self.message
        )
    }
}
impl std::error::Error for RuleError {}
pub fn diagnostic(error: &anyhow::Error) -> Option<Value> {
    error.downcast_ref::<RuleError>().map(|e| json!(e))
}
fn error(
    text: &str,
    span: std::ops::Range<usize>,
    kind: &str,
    attribution: &str,
    message: impl Into<String>,
) -> anyhow::Error {
    let mut start = span.start.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = span.end.max(start).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = &text[..start];
    RuleError {
        code: "invalid_rules".into(),
        kind: kind.into(),
        path: RULES_PATH.into(),
        range: Range { start, end },
        line: prefix.bytes().filter(|b| *b == b'\n').count() + 1,
        column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
        attribution: attribution.into(),
        message: message.into(),
    }
    .into()
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Raw {
    version: Option<Spanned<i64>>,
    #[serde(default)]
    layer_policy: Policy,
    #[serde(default)]
    external_policy: ExternalPolicy,
    #[serde(default)]
    layers: BTreeMap<String, Spanned<RawLayer>>,
    #[serde(default)]
    roles: BTreeMap<String, Spanned<RawRole>>,
    #[serde(default)]
    rules: Vec<Spanned<RawRule>>,
    #[serde(default)]
    exceptions: Vec<Spanned<RawException>>,
    /// Authored apps (#794).
    #[serde(default)]
    apps: BTreeMap<String, Spanned<RawApp>>,
    /// HTTP interface settings (#855).
    #[serde(default)]
    interfaces: RawInterfaces,
    /// Cargo features whose gates mark test code for reference sites (#988).
    test_features: Option<Spanned<Vec<Spanned<String>>>>,
}
/// The `[interfaces]` table (#855). `wrappers` is its only key.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawInterfaces {
    #[serde(default)]
    wrappers: Vec<Spanned<RawWrapper>>,
}
/// One `[[interfaces.wrappers]]` entry (#855).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWrapper {
    callee: Option<Spanned<String>>,
    method: Option<Spanned<String>>,
    at: Option<Spanned<i64>>,
    prefix: Option<Spanned<String>>,
    root: Option<Spanned<String>>,
    app: Option<Spanned<String>>,
}
/// An authored `[apps.<name>]` table (#794). `roots` is its only key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApp {
    roots: Option<Spanned<Vec<Spanned<String>>>>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Policy {
    peers: Option<Spanned<String>>,
    shared_outbound: Option<Spanned<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayer {
    order: Option<i64>,
    shared: Option<bool>,
    /// Browser presentation hint only (#391); never affects evaluation.
    display: Option<Spanned<String>>,
    /// The app an ordered layer belongs to (#784).
    app: Option<Spanned<String>>,
    /// The apps a shared layer serves (#784).
    apps: Option<Spanned<Vec<Spanned<String>>>>,
    #[serde(default)]
    paths: Vec<Spanned<String>>,
    #[serde(default)]
    external: Vec<Spanned<ExternalSelector>>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ExternalPolicy {
    #[serde(default)]
    deny: Vec<Spanned<ExternalSelector>>,
}
#[derive(Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct ExternalSelector {
    language: String,
    package: String,
    stdlib: bool,
}
impl ExternalSelector {
    fn from_identity(identity: &Value) -> Option<Self> {
        (identity["kind"] == "externalPackage").then_some(())?;
        Some(Self {
            language: identity["language"].as_str()?.into(),
            package: identity["package"].as_str()?.into(),
            stdlib: identity["standardLibrary"].as_bool()?,
        })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRole {
    select: Spanned<RawSelector>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawSelector {
    paths: Option<Vec<Spanned<String>>>,
    name: Option<Spanned<String>>,
    language: Option<Vec<Spanned<String>>>,
    extends: Option<Spanned<String>>,
    implements: Option<Spanned<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: Spanned<String>,
    for_each: Spanned<String>,
    #[serde(default)]
    require: Vec<Spanned<BTreeMap<String, String>>>,
    #[serde(default)]
    forbid: Vec<Spanned<BTreeMap<String, String>>>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct ConfigTarget {
    source_set: String,
    language: String,
    path: String,
    sid: String,
}
impl ConfigTarget {
    fn wire(&self) -> Value {
        json!({"sourceSetId":self.source_set,"language":self.language,"path":self.path,"sid":self.sid})
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct ModulePathTarget {
    language: String,
    path: String,
}
impl ModulePathTarget {
    fn valid(&self) -> bool {
        languages(&self.language)
            && !self.path.is_empty()
            && crate::file_tree::valid_path(&self.path)
            && !self.path.contains(['*', '?'])
    }
    fn matches(&self, identity: &Value, source_set: &str) -> bool {
        identity["sourceSetId"] == source_set
            && identity["language"] == self.language
            && identity["path"] == self.path
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(untagged)]
enum ExceptionTarget {
    Native(ConfigTarget),
    ModulePath(ModulePathTarget),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawException {
    rule: Spanned<String>,
    target: Spanned<ExceptionTarget>,
    destination: Option<Spanned<ExceptionTarget>>,
    reason: Spanned<String>,
}
struct LayerPattern {
    text: String,
    matcher: GlobSet,
}
struct Layer {
    order: Option<i64>,
    display: Option<String>,
    app: Option<(String, std::ops::Range<usize>)>,
    apps: Option<Vec<(String, std::ops::Range<usize>)>>,
    patterns: Vec<LayerPattern>,
    external: BTreeSet<ExternalSelector>,
    span: std::ops::Range<usize>,
}
struct Selector {
    paths: Vec<GlobSet>,
    name: Option<GlobSet>,
    language: Vec<String>,
    extends: Option<String>,
    implements: Option<String>,
}
struct Constraint {
    mode: &'static str,
    kind: String,
    role: String,
}
struct Rule {
    id: String,
    role: String,
    constraints: Vec<Constraint>,
}
enum ExceptionKind {
    Declaration(Value),
    ModulePair {
        source: ModulePathTarget,
        destination: ModulePathTarget,
        span: std::ops::Range<usize>,
    },
}
struct Exception {
    rule: String,
    kind: ExceptionKind,
    reason: String,
}
/// An authored app (#794), located for errors.
struct AuthoredApp {
    name: String,
    span: std::ops::Range<usize>,
    roots: Vec<(String, std::ops::Range<usize>)>,
}
pub struct Config {
    pub text: String,
    pub content_hash: String,
    layers: BTreeMap<String, Layer>,
    roles: BTreeMap<String, Selector>,
    rules: Vec<Rule>,
    exceptions: Vec<Exception>,
    peers: Option<String>,
    shared_outbound: Option<String>,
    external_deny: BTreeSet<ExternalSelector>,
    /// Authored `[apps.<name>]` tables in name order (#794).
    apps: Vec<AuthoredApp>,
    /// Authored `[[interfaces.wrappers]]` entries in written order (#855).
    wrappers: Vec<InterfaceWrapper>,
    /// The authored `test_features`, sorted (#988).
    test_features: Vec<String>,
}

/// Where a configured request wrapper reads its method (#855).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WrapperMethod {
    /// `method` in the options object at this argument.
    Options(usize),
    /// A string at this argument.
    Argument(usize),
}

/// An authored `[[interfaces.wrappers]]` entry (#855): a JavaScript or TypeScript callee whose
/// argument 0 is a request path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InterfaceWrapper {
    /// An identifier, or `object.property`.
    pub(crate) callee: String,
    pub(crate) method: WrapperMethod,
    /// A rooted literal the wrapper adds to every path.
    pub(crate) prefix: Option<String>,
    /// The checkout-relative directory whose files the wrapper applies to; `.` is every file.
    /// It is `.` when `app` is set.
    pub(crate) root: String,
    /// The app whose files the wrapper applies to, and where it is written. It names an app of
    /// the effective inventory, which rule preparation checks.
    pub(crate) app: Option<(String, std::ops::Range<usize>)>,
}

impl Config {
    /// Authored `[[exceptions]]` entries, as the rules inventory lists them.
    pub fn exception_count(&self) -> usize {
        self.exceptions.len()
    }

    /// The authored request wrappers, in written order (#855).
    pub(crate) fn interface_wrappers(&self) -> &[InterfaceWrapper] {
        &self.wrappers
    }

    /// The Cargo features whose `cfg` gates mark a Rust reference site as test (#988).
    pub(crate) fn test_features(&self) -> &[String] {
        &self.test_features
    }

    /// How many `[apps.<name>]` tables the file authors (#794).
    pub(crate) fn authored_app_count(&self) -> usize {
        self.apps.len()
    }

    /// The authored apps in name order, as inventory entries.
    fn authored_apps(&self) -> Vec<crate::apps::App> {
        self.apps
            .iter()
            .map(|app| {
                crate::apps::App::authored(
                    app.name.clone(),
                    app.roots.iter().map(|(root, _)| root.clone()).collect(),
                )
            })
            .collect()
    }
}

pub(crate) fn languages(s: &str) -> bool {
    matches!(s, "java" | "rust" | "python" | "javascript" | "typescript")
}

fn identity(d: &Declaration) -> Value {
    json!({"sourceSetId":d.document.source_set_id,"language":d.document.language,"path":d.document.path,"sid":d.syntax_id})
}
fn native_evidence(d: &Declaration) -> Value {
    json!({"document":d.document,"revisionId":d.revision_id,"range":d.range,"kind":"declaration","syntaxId":d.syntax_id,"callId":null,"provenanceId":d.provenance_id})
}
fn glob_matches(patterns: &[GlobSet], path: &str) -> bool {
    patterns.is_empty() || patterns.iter().any(|p| p.is_match(path))
}
/// Browser-only authored inventory; does not change the closed rules payload.
pub fn view_layers(config: &Config) -> Vec<Value> {
    config.layers.iter().map(|(name, layer)| {
        json!({"name": name, "order": layer.order, "shared": layer.order.is_none(), "display": layer.display,
            "app": layer.app.as_ref().map(|(app, _)| app),
            "apps": layer.apps.as_ref().map(|apps| apps.iter().map(|(app, _)| app).collect::<Vec<_>>())})
    }).collect()
}

#[derive(Default)]
struct Bases {
    extends: Option<Vec<String>>,
    implements: Option<Vec<String>>,
}
impl Bases {
    /// The known-empty bases of a declaration that is no type or impl.
    fn none() -> Self {
        Self {
            extends: Some(vec![]),
            implements: Some(vec![]),
        }
    }
}
fn children(node: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    let mut c = node.walk();
    node.named_children(&mut c).collect()
}
fn base_texts(node: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    children(node)
        .into_iter()
        .flat_map(|n| {
            if matches!(n.kind(), "type_list" | "trait_bounds") {
                children(n)
            } else {
                vec![n]
            }
        })
        .filter_map(|n| source.get(n.byte_range()).map(str::to_owned))
        .collect()
}
/// Each `extends` value from its start to the end of the `type_arguments` that follows it.
fn ts_extends_texts(clause: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let nodes = children(clause);
    let mut texts = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if clause.field_name_for_named_child(index as u32) != Some("value") {
            continue;
        }
        let end = nodes
            .get(index + 1)
            .filter(|next| next.kind() == "type_arguments")
            .map_or(node.end_byte(), |next| next.end_byte());
        if let Some(text) = source.get(node.start_byte()..end) {
            texts.push(text.to_owned());
        }
    }
    texts
}
fn has_bases(node: tree_sitter::Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration"
            | "abstract_class_declaration"
            | "class_definition"
            | "interface_declaration"
            | "type_alias_declaration"
            | "impl_item"
            | "trait_item"
            | "enum_declaration"
            | "record_declaration"
    )
}
fn parse_bases(
    doc: &crate::references::DocumentInput,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<BTreeMap<String, Bases>> {
    check()?;
    ensure!(
        doc.file.text.len() <= CONFIG_BYTES,
        "bounded_work: selector source exceeds file bound"
    );
    let language = match doc.key.language.as_str() {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => crate::indexer_ts::grammar(&doc.key.path),
        _ => anyhow::bail!("store_unavailable: unknown selector language"),
    };
    let source = &doc.file.text;
    // Only a type or an impl reads its syntax. A document without one, such as one published
    // with only its file module (#1300), is not parsed: every row's bases are empty.
    let typed = |d: &crate::native_evidence::Declaration| {
        matches!(d.kind.as_str(), "type" | "implementation")
    };
    if !doc.declarations.iter().any(typed) {
        return Ok(doc
            .declarations
            .iter()
            .map(|d| (d.syntax_id.clone(), Bases::none()))
            .collect());
    }
    // A Rust source past measurement's parse-work budget reads as one parser error (#1300).
    let tree = crate::syntax_bounds::parse_stored(
        &doc.key.language,
        &language,
        source,
        "bounded_work: selector parser stopped",
        check,
    )?
    .unwrap_or_else(|| crate::syntax_bounds::unread_tree(source));
    // A base clause inside a parser error goes unseen, so its file's bases stay unknown.
    let broken = tree.root_node().has_error();
    let mut types = BTreeMap::new();
    // A type past the walk's bounds is unknown here, and references report its file.
    crate::references::walk(tree.root_node(), check, &mut |node, _check| {
        if has_bases(node) {
            types.insert((node.start_byte(), node.end_byte()), node);
        }
        Ok(false)
    })?;
    let mut expansions = crate::native_rust_macros::Expansions::new(source);
    let mut result = BTreeMap::new();
    for d in &doc.declarations {
        check()?;
        if !typed(d) {
            result.insert(d.syntax_id.clone(), Bases::none());
            continue;
        }
        if broken {
            result.insert(d.syntax_id.clone(), Bases::default());
            continue;
        }
        let mut node = types.get(&(d.range.start, d.range.end)).copied();
        if node.is_none() && doc.key.language == "rust" {
            // An `impl` measured inside a macro has syntax only in that macro's expansion
            // (#551). Only measured macros are reached, so none of it can exceed a bound here.
            node = expansions
                .root_for(tree.root_node(), d.range.start, d.range.end)
                .descendant_for_byte_range(d.range.start, d.range.end)
                .filter(|n| {
                    has_bases(*n) && n.start_byte() == d.range.start && n.end_byte() == d.range.end
                });
        }
        let mut bases = Bases::default();
        if let Some(n) = node {
            match doc.key.language.as_str() {
                "java" => {
                    let mut extends = Vec::new();
                    let mut implements = Vec::new();
                    if let Some(b) = n.child_by_field_name("superclass") {
                        extends.extend(base_texts(b, source));
                    }
                    if let Some(b) = n.child_by_field_name("interfaces") {
                        implements.extend(base_texts(b, source));
                    }
                    for b in children(n)
                        .into_iter()
                        .filter(|n| n.kind() == "extends_interfaces")
                    {
                        extends.extend(base_texts(b, source));
                    }
                    bases = Bases {
                        extends: Some(extends),
                        implements: Some(implements),
                    };
                }
                "python" => {
                    bases.extends = Some(
                        n.child_by_field_name("superclasses")
                            .map(|b| base_texts(b, source))
                            .unwrap_or_default(),
                    );
                }
                "javascript" => {
                    bases.extends = Some(
                        children(n)
                            .into_iter()
                            .find(|b| b.kind() == "class_heritage")
                            .map(|b| base_texts(b, source))
                            .unwrap_or_default(),
                    );
                }
                "typescript" => {
                    let mut extends = Vec::new();
                    let mut implements = Vec::new();
                    for heritage in children(n)
                        .into_iter()
                        .filter(|c| c.kind() == "class_heritage")
                    {
                        for clause in children(heritage) {
                            match clause.kind() {
                                "extends_clause" => {
                                    extends.extend(ts_extends_texts(clause, source))
                                }
                                "implements_clause" => {
                                    implements.extend(base_texts(clause, source))
                                }
                                _ => {}
                            }
                        }
                    }
                    for clause in children(n)
                        .into_iter()
                        .filter(|c| c.kind() == "extends_type_clause")
                    {
                        extends.extend(base_texts(clause, source));
                    }
                    bases = Bases {
                        extends: Some(extends),
                        implements: Some(implements),
                    };
                }
                "rust" if n.kind() == "impl_item" => {
                    bases.implements = Some(
                        n.child_by_field_name("trait")
                            .and_then(|b| source.get(b.byte_range()).map(str::to_owned))
                            .into_iter()
                            .collect(),
                    );
                }
                _ => {}
            }
        }
        result.insert(d.syntax_id.clone(), bases);
    }
    Ok(result)
}
/// Every declaration of the snapshot by `(document index, syntax ID)`: semantic binding
/// targets are selected by role against these (#523).
type Targets<'a> = BTreeMap<(usize, &'a str), &'a Declaration>;

struct DocumentEvaluation<'a> {
    config: &'a Config,
    targets: &'a Targets<'a>,
    inheritance: bool,
    source_set: Option<&'a str>,
    #[cfg(test)]
    before_document: Option<&'a (dyn Fn(&crate::references::DocumentInput) + Sync)>,
}

/// What rule evaluation needs from a snapshot: the budget it sets and each document's
/// declaration rows, so that later evaluations need no source (#556).
pub(crate) struct RulesBase {
    limit: usize,
    source_set: Option<String>,
    semantic: bool,
    declarations: Vec<Value>,
    test_declarations: Vec<Value>,
    work_used: usize,
    apps: Vec<crate::apps::App>,
    unread_manifests: usize,
}

impl RulesBase {
    /// The effective apps for this snapshot (#794): authored apps by name, then the detected
    /// apps they do not shadow, in manifest-path order.
    pub(crate) fn apps(&self) -> &[crate::apps::App] {
        &self.apps
    }
}

#[cfg(test)]
mod specificity_tests;

#[cfg(test)]
mod portable_exception_tests;

#[cfg(test)]
mod document_parallel_tests;

#[cfg(test)]
mod display_tests;

#[cfg(test)]
mod app_scope_tests;
