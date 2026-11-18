//! The class catalog's composition, apart from its extraction: the persisted `Catalog`
//! record, the limits, the per-file summaries, the ordered bounded composer, the plan of a
//! complete catalog and the fit to the per-document budgets. Pure: model records,
//! `evidence_limits` and serde, with no I/O and no parser. Storage composes and fits a
//! revision's catalog with it at publication. The tree-sitter extraction stays in
//! `classes.rs`, which re-exports the public items at their old paths.
#[cfg(test)]
use crate::model::ClassMember;
use crate::model::{ClassDefinition, ClassRelation, DetailValue, FileExtraction};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const FILE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const TOTAL_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const VISITS: usize = 100_000;
const DEPTH: usize = 64;
const FILE_CLASSES: usize = 1_000;
const CLASSES: usize = 20_000;
const MEMBERS: usize = 256;
const FILE_REFS: usize = 8_192;
pub(crate) const RECORDS: usize = 250_000;
const TEXT: usize = 2_048;
pub(crate) const OUTPUT_TEXT: usize = 64 * 1024 * 1024;
pub(crate) const REGISTRY_TEXT: usize = 32 * 1024 * 1024;
/// Languages whose declarations the class catalog extracts, in API order.
pub const SUPPORTED_LANGUAGES: [&str; 5] = ["java", "python", "javascript", "rust", "typescript"];
/// Whether `language` documents carry a class extraction (`F`) and class rows.
pub fn supports_language(language: &str) -> bool {
    SUPPORTED_LANGUAGES.contains(&language)
}
const CATALOG_NOTICE: &str = "Declared types are terminal source text, not class relationships. Direct class declarations only; no function-body, conditional/anonymous class, generated member, wildcard-import, type-alias or dependency discovery. Rust: inherent impl methods merge by scoped path; derives, macro-generated items and blanket impls are not attributed. TypeScript: classes inside namespaces/modules are not catalogued; enum members are listed as fields.";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub classes: Vec<ClassDefinition>,
    pub relations: Vec<ClassRelation>,
    pub warnings: Vec<String>,
    pub truncated: bool,
    /// Documents whose class rows were cut to fit (#564). They are stored as not class
    /// complete, which keeps this past the bounded warnings: a later publication composes them
    /// again.
    #[serde(skip)]
    pub fitted: BTreeSet<String>,
}
/// One value controls extraction and workspace composition, including test-sized caps.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub file_bytes: usize,
    pub total_bytes: usize,
    pub visits: usize,
    pub depth: usize,
    pub file_classes: usize,
    pub classes: usize,
    pub members: usize,
    pub file_refs: usize,
    pub records: usize,
    pub text: usize,
    pub output_text: usize,
    pub registry_text: usize,
    pub files: usize,
    pub symbols: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            file_bytes: FILE_BYTES,
            total_bytes: TOTAL_BYTES,
            visits: VISITS,
            depth: DEPTH,
            file_classes: FILE_CLASSES,
            classes: CLASSES,
            members: MEMBERS,
            file_refs: FILE_REFS,
            records: RECORDS,
            text: TEXT,
            output_text: OUTPUT_TEXT,
            registry_text: REGISTRY_TEXT,
            files: 100_000,
            symbols: 2_000_000,
        }
    }
}
/// Charges and intrinsic metadata for one extraction, before workspace limits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FileSummary {
    pub source_bytes: usize,
    pub registry_bytes: usize,
    pub declarations: usize,
    pub detail_records: usize,
    pub detail_text: usize,
    pub warnings: Vec<String>,
    pub truncated: bool,
    pub registry_incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogMetadata {
    pub warnings: Vec<String>,
    pub truncated: bool,
}

/// Ordered, bounded class composition. Callers may replay per-file extraction
/// payloads from private storage instead of retaining every payload in memory.
pub(crate) struct CatalogComposer {
    result: Catalog,
    source: usize,
    registry: usize,
    records: usize,
    detail: usize,
    registry_complete: bool,
    detail_open: bool,
    open: bool,
    limits: Limits,
    /// Admitted Rust impl contributions, attached to their unique class at `finish`.
    merged: Vec<(String, DetailValue)>,
}
impl CatalogComposer {
    pub(crate) fn new(file_count: usize, symbol_count: usize, limits: Limits) -> Self {
        let mut result = Catalog::default();
        let open = file_count <= limits.files && symbol_count <= limits.symbols;
        if !open {
            result.truncated = true;
            result.warnings.push(format!(
                "Class catalog input limit exceeded ({} files / {} symbols)",
                limits.files, limits.symbols
            ));
        }
        Self {
            result,
            source: 0,
            registry: 0,
            records: 0,
            detail: 0,
            registry_complete: true,
            detail_open: true,
            open,
            limits,
            merged: Vec::new(),
        }
    }
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }
    /// False means composition reached the exact global stop point. Extraction
    /// of other files must already be complete before callers stop replaying.
    pub(crate) fn push(&mut self, f: &FileExtraction) -> bool {
        if !self.open {
            return false;
        }
        if self.source.saturating_add(f.source_bytes) > self.limits.total_bytes {
            self.result.limit_warning(
                &format!(
                    "Class catalog source limit exceeded ({} MiB)",
                    self.limits.total_bytes / (1024 * 1024)
                ),
                self.limits.text,
            );
            self.registry_complete = false;
            self.open = false;
            return false;
        }
        if self.registry.saturating_add(f.registry_bytes) > self.limits.registry_text {
            self.result.limit_warning(
                &format!(
                    "Class declaration registry text limit reached ({} MiB)",
                    self.limits.registry_text / (1024 * 1024)
                ),
                self.limits.text,
            );
            self.registry_complete = false;
            self.open = false;
            return false;
        }
        self.source += f.source_bytes;
        self.registry += f.registry_bytes;
        for warning in &f.warnings {
            self.result.add_warning(warning, self.limits.text);
        }
        self.result.truncated |= f.truncated;
        self.registry_complete &= !f.registry_incomplete;
        for (index, c) in f.classes.iter().enumerate() {
            if self.result.classes.len() == self.limits.classes {
                self.result.limit_warning(
                    &format!(
                        "Class catalog declaration limit reached ({})",
                        self.limits.classes
                    ),
                    self.limits.text,
                );
                self.registry_complete = false;
                self.open = false;
                return false;
            }
            let mut c = c.clone();
            c.fields.clear();
            c.methods.clear();
            for item in f.items.iter().filter(|i| i.class_index == index) {
                if !self.admit_detail(item.records, item.text) {
                    c.truncated = true;
                    continue;
                }
                match &item.value {
                    DetailValue::Field(value) => c.fields.push(value.clone()),
                    DetailValue::Method(value) => c.methods.push(value.clone()),
                    DetailValue::Relation(value) => self.result.relations.push(value.clone()),
                    DetailValue::ScopeText => (),
                    DetailValue::Truncated => c.truncated = true,
                }
            }
            self.result.classes.push(c);
        }
        for item in &f.impls {
            // Charged in this file's walk position, like any detail item.
            if self.admit_detail(item.records, item.text) {
                self.merged.push((item.target.clone(), item.value.clone()));
            } else {
                // Decision 0004: a class that loses an item is truncated.
                self.merged
                    .push((item.target.clone(), DetailValue::Truncated));
            }
        }
        true
    }
    /// Decision 0004 detail admission: the first failing item closes detail output.
    fn admit_detail(&mut self, records: usize, text: usize) -> bool {
        if !self.detail_open {
            return false;
        }
        if self.records.saturating_add(records) > self.limits.records
            || self.detail.saturating_add(text) > self.limits.output_text
        {
            let message = if self.records.saturating_add(records) > self.limits.records {
                format!(
                    "Class detail limit reached ({} records / {} MiB text / {} visits per file); declaration discovery continues",
                    self.limits.records,
                    self.limits.output_text / (1024 * 1024),
                    self.limits.visits
                )
            } else {
                format!(
                    "Class detail text limit reached ({} MiB); declaration discovery continues",
                    self.limits.output_text / (1024 * 1024)
                )
            };
            self.result.limit_warning(&message, self.limits.text);
            self.detail_open = false;
            return false;
        }
        self.records += records;
        self.detail += text;
        true
    }
    pub(crate) fn finish(mut self) -> Catalog {
        attach_impls(
            &mut self.result,
            std::mem::take(&mut self.merged),
            self.limits.members,
        );
        sort_catalog(&mut self.result);
        if !self.result.classes.is_empty() {
            self.result.warnings.push(CATALOG_NOTICE.into());
        }
        if !self.registry_complete {
            self.result.warnings.push(
                "Incomplete class declaration registry: some declarations may be absent.".into(),
            );
        }
        self.result
    }
}

/// Global step of #452: an admitted Rust impl contribution joins the one Rust
/// class whose qualified name equals its scoped target. No class, or several
/// (for example cfg-duplicated declarations), leave it unattributed.
fn attach_impls(result: &mut Catalog, merged: Vec<(String, DetailValue)>, members: usize) {
    if merged.is_empty() {
        return;
    }
    let mut targets: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, class) in result.classes.iter().enumerate() {
        if class.language == "rust" {
            targets
                .entry(class.qualified_name.as_str())
                .or_default()
                .push(index);
        }
    }
    let targets: BTreeMap<String, usize> = targets
        .into_iter()
        .filter(|(_, found)| found.len() == 1)
        .map(|(key, found)| (key.to_owned(), found[0]))
        .collect();
    for (target, value) in merged {
        let Some(&index) = targets.get(&target) else {
            continue;
        };
        let class = &mut result.classes[index];
        match value {
            DetailValue::Method(method) => {
                if class.fields.len() + class.methods.len() >= members {
                    class.truncated = true;
                } else {
                    class.methods.push(method);
                }
            }
            DetailValue::Relation(mut relation) => {
                relation.owner = class.symbol.id.clone();
                // Byte offsets are unique only within one file, and impl relations
                // from several files share their owner's projection: name the path.
                relation.id = format!(
                    "class-ref:{}:{}:{}:{}:{}:{}",
                    relation.owner,
                    relation.path,
                    relation.range.start_byte,
                    relation.range.end_byte,
                    relation.kind,
                    relation.type_name
                );
                result.relations.push(relation);
            }
            // A contribution refused by a detail cap.
            // Charged detail text without a member (method type parameters).
            DetailValue::ScopeText => {}
            // Omitted by a per-file limit, or refused by a detail cap.
            DetailValue::Truncated => class.truncated = true,
            DetailValue::Field(_) => {}
        }
    }
}
/// Classes in `(path, start_byte, id)` order; relations grouped by their owner's
/// document, then `(path, start_byte, id)`. A Rust `impl Trait for T` relation is
/// stored with T's document even when the impl is in another file.
fn sort_catalog(result: &mut Catalog) {
    result.classes.sort_by(|a, b| {
        (&a.symbol.path, a.symbol.range.start_byte, &a.symbol.id).cmp(&(
            &b.symbol.path,
            b.symbol.range.start_byte,
            &b.symbol.id,
        ))
    });
    let owners: BTreeMap<&str, &str> = result
        .classes
        .iter()
        .map(|c| (c.symbol.id.as_str(), c.symbol.path.as_str()))
        .collect();
    let mut keyed: Vec<(String, ClassRelation)> = std::mem::take(&mut result.relations)
        .into_iter()
        .map(|r| {
            let owner = owners
                .get(r.owner.as_str())
                .map_or_else(|| r.path.clone(), |path| (*path).to_owned());
            (owner, r)
        })
        .collect();
    keyed.sort_by(|(ao, a), (bo, b)| {
        (ao, &a.path, a.range.start_byte, &a.id).cmp(&(bo, &b.path, b.range.start_byte, &b.id))
    });
    result.relations = keyed.into_iter().map(|(_, r)| r).collect();
    result.relations.dedup_by(|a, b| a.id == b.id);
}

/// Plans the no-global-limit-hit case without loading extraction payloads.
/// Summaries must be in the same path order used by `CatalogComposer`.
pub(crate) fn plan_complete_catalog<'a>(
    ordered: impl IntoIterator<Item = &'a FileSummary>,
    file_count: usize,
    symbol_count: usize,
    limits: Limits,
) -> Option<CatalogMetadata> {
    if file_count > limits.files || symbol_count > limits.symbols {
        return None;
    }
    let mut source = 0usize;
    let mut registry = 0usize;
    let mut declarations = 0usize;
    let mut records = 0usize;
    let mut detail = 0usize;
    let mut registry_complete = true;
    let mut result = Catalog::default();
    for summary in ordered {
        source = source.checked_add(summary.source_bytes)?;
        registry = registry.checked_add(summary.registry_bytes)?;
        declarations = declarations.checked_add(summary.declarations)?;
        records = records.checked_add(summary.detail_records)?;
        detail = detail.checked_add(summary.detail_text)?;
        if source > limits.total_bytes
            || registry > limits.registry_text
            || declarations > limits.classes
            || records > limits.records
            || detail > limits.output_text
        {
            return None;
        }
        for warning in &summary.warnings {
            result.add_warning(warning, limits.text);
        }
        result.truncated |= summary.truncated;
        registry_complete &= !summary.registry_incomplete;
    }
    append_complete_catalog_warnings(&mut result, declarations != 0, registry_complete);
    Some(CatalogMetadata {
        warnings: result.warnings,
        truncated: result.truncated,
    })
}

fn append_complete_catalog_warnings(
    result: &mut Catalog,
    has_classes: bool,
    registry_complete: bool,
) {
    if has_classes {
        result.warnings.push(CATALOG_NOTICE.into());
    }
    if !registry_complete {
        result
            .warnings
            .push("Incomplete class declaration registry: some declarations may be absent.".into());
    }
}

/// Reconstructs all admitted per-file rows, without applying workspace limits.
pub(crate) fn complete_file_catalog(extraction: &FileExtraction) -> Catalog {
    let mut result = Catalog {
        truncated: extraction.truncated,
        ..Catalog::default()
    };
    for warning in &extraction.warnings {
        result.add_warning(warning, Limits::default().text);
    }
    for (index, class) in extraction.classes.iter().enumerate() {
        let mut class = class.clone();
        class.fields.clear();
        class.methods.clear();
        for item in extraction.items.iter().filter(|i| i.class_index == index) {
            match &item.value {
                DetailValue::Field(value) => class.fields.push(value.clone()),
                DetailValue::Method(value) => class.methods.push(value.clone()),
                DetailValue::Relation(value) => result.relations.push(value.clone()),
                DetailValue::ScopeText => (),
                DetailValue::Truncated => class.truncated = true,
            }
        }
        result.classes.push(class);
    }
    // Only same-file targets are known here; publication composes a revision
    // in full whenever a Rust document changes.
    attach_impls(
        &mut result,
        extraction
            .impls
            .iter()
            .map(|item| (item.target.clone(), item.value.clone()))
            .collect(),
        Limits::default().members,
    );
    sort_catalog(&mut result);
    let has_classes = !result.classes.is_empty();
    append_complete_catalog_warnings(&mut result, has_classes, !extraction.registry_incomplete);
    result
}

pub(crate) const BUDGET_WARNING: &str = "class rows exceed the per-file evidence budget";

/// What a class row is charged beside its text columns and payload: the projection's two
/// hashes with their keys, and the stored ID.
const CLASS_ROW_BYTES: usize = 84 + 73 + 39;
/// What a relation row is charged beside its ID and payload.
const RELATION_ROW_BYTES: usize = 84 + 2 * 39;

impl Catalog {
    /// Fit each document's class rows to what a selected read admits (#564). A Rust class
    /// takes members from other files, so a small file can own a large row: its last members
    /// go, and it is marked truncated. Past the document's budget, its last rows go too, with a
    /// warning. A class is never published where a read would refuse its document.
    pub(crate) fn fit_stored_budgets(&mut self, source_bytes: impl Fn(&str) -> i64) -> Result<()> {
        use crate::evidence_limits as budget;
        fn encoded<T: serde::Serialize>(value: &T) -> Result<usize> {
            Ok(serde_json::to_vec(value)?.len())
        }
        let mut left = BTreeMap::<String, (i64, i64)>::new();
        let mut fits = |path: &str, row: usize| {
            let bytes = source_bytes(path);
            let (total, rows) = left
                .entry(path.to_owned())
                .or_insert_with(|| (budget::total(bytes), budget::row_limit(bytes)));
            let row = row as i64;
            let fits = row <= budget::max_row(bytes) && row <= *total && *rows > 0;
            if fits {
                *total -= row;
                *rows -= 1;
            }
            fits
        };
        let mut omitted = BTreeSet::new();
        let mut dropped = BTreeSet::new();
        let mut kept = Vec::with_capacity(self.classes.len());
        for mut class in std::mem::take(&mut self.classes) {
            let path = class.symbol.path.clone();
            let limit = budget::max_row(source_bytes(&path)) as usize;
            let fixed =
                CLASS_ROW_BYTES + class.symbol.name.len() + class.qualified_name.len() + path.len();
            let mut row = fixed + encoded(&class)?;
            while row > limit {
                let member = match class.methods.pop().or_else(|| class.fields.pop()) {
                    Some(member) => member,
                    None => break,
                };
                row -= (encoded(&member)? + 1).min(row);
                class.truncated = true;
                omitted.insert(path.clone());
            }
            if fits(&path, row) {
                kept.push(class);
            } else {
                dropped.insert(class.symbol.id);
                omitted.insert(path);
            }
        }
        self.classes = kept;
        let owners: BTreeMap<&str, &str> = self
            .classes
            .iter()
            .map(|class| (class.symbol.id.as_str(), class.symbol.path.as_str()))
            .collect();
        let mut relations = Vec::with_capacity(self.relations.len());
        for relation in std::mem::take(&mut self.relations) {
            let path = owners.get(relation.owner.as_str()).copied();
            let path = path.unwrap_or(relation.path.as_str()).to_owned();
            let row = RELATION_ROW_BYTES + relation.id.len() + encoded(&relation)?;
            if !dropped.contains(&relation.owner) && fits(&path, row) {
                relations.push(relation);
            } else {
                omitted.insert(path);
            }
        }
        self.relations = relations;
        for path in &omitted {
            self.truncated = true;
            self.fitted.insert(path.clone());
            self.add_warning(
                &format!("{path}: {BUDGET_WARNING}; some omitted"),
                Limits::default().text,
            );
        }
        Ok(())
    }

    pub fn compose(
        extracts: &[FileExtraction],
        file_count: usize,
        symbol_count: usize,
        limits: Limits,
    ) -> Result<Self> {
        let mut composer = CatalogComposer::new(file_count, symbol_count, limits);
        let mut ordered: Vec<_> = extracts.iter().collect();
        ordered.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        for extraction in ordered {
            if !composer.push(extraction) {
                break;
            }
        }
        Ok(composer.finish())
    }

    fn add_warning(&mut self, message: &str, text: usize) {
        if self.warnings.len() < 100 && !self.warnings.iter().any(|s| s == message) {
            self.warnings.push(message.chars().take(text).collect());
        }
    }
    fn limit_warning(&mut self, message: &str, text: usize) {
        self.truncated = true;
        self.add_warning(message, text);
    }
}
impl FileExtraction {
    pub(crate) fn summary(&self) -> FileSummary {
        let mut detail_records = 0usize;
        let mut detail_text = 0usize;
        for item in self
            .items
            .iter()
            .filter(|item| item.class_index < self.classes.len())
        {
            detail_records = detail_records.saturating_add(item.records);
            detail_text = detail_text.saturating_add(item.text);
        }
        for item in &self.impls {
            detail_records = detail_records.saturating_add(item.records);
            detail_text = detail_text.saturating_add(item.text);
        }
        FileSummary {
            source_bytes: self.source_bytes,
            registry_bytes: self.registry_bytes,
            declarations: self.classes.len(),
            detail_records,
            detail_text,
            warnings: self.warnings.clone(),
            truncated: self.truncated,
            registry_incomplete: self.registry_incomplete,
        }
    }
}

#[cfg(test)]
mod fit_budget_tests {
    use super::*;
    use crate::model::{Provenance, SourceRange, Symbol, SymbolKind};

    fn range() -> SourceRange {
        SourceRange {
            start_byte: 0,
            end_byte: 1,
            start_line: 1,
            start_column: 0,
            end_line: 1,
            end_column: 1,
        }
    }
    fn member(index: usize) -> ClassMember {
        ClassMember {
            name: format!("member_{index}"),
            type_hint: None,
            symbol_id: None,
            path: "other.rs".into(),
            range: range(),
        }
    }
    fn class(path: &str, name: &str, members: usize) -> ClassDefinition {
        ClassDefinition {
            symbol: Symbol {
                id: format!("sid:{path}:{name}"),
                name: name.into(),
                display_label: None,
                kind: SymbolKind::Class,
                path: path.into(),
                range: range(),
                parent: None,
                accessor: false,
                provenance: Provenance {
                    projection: None,
                    source: "syntax".into(),
                    semantic: crate::model::SemanticState::Unavailable,
                },
            },
            qualified_name: name.into(),
            language: "rust".into(),
            declaration_kind: "struct".into(),
            fields: vec![],
            methods: (0..members).map(member).collect(),
            truncated: false,
        }
    }
    fn relation(owner: &str, index: usize) -> ClassRelation {
        ClassRelation {
            id: format!("rel-{index}"),
            owner: owner.into(),
            target: None,
            type_name: "T".into(),
            kind: "implements".into(),
            path: "other.rs".into(),
            range: range(),
            candidate_ids: vec![],
            match_kind: "unmatched".into(),
        }
    }

    #[test]
    fn rows_within_the_budgets_are_unchanged() {
        let mut catalog = Catalog {
            classes: vec![class("a.rs", "A", 10)],
            relations: vec![relation("sid:a.rs:A", 0)],
            ..Catalog::default()
        };
        let before = catalog.clone();
        catalog.fit_stored_budgets(|_| 100).unwrap();
        assert_eq!(catalog.classes, before.classes);
        assert_eq!(catalog.relations, before.relations);
        assert!(!catalog.truncated && catalog.warnings.is_empty());
    }

    #[test]
    fn a_small_file_keeps_its_class_and_loses_the_last_members_past_its_row_budget() {
        let mut catalog = Catalog {
            classes: vec![class("a.rs", "A", 2_000)],
            ..Catalog::default()
        };
        catalog.fit_stored_budgets(|_| 16).unwrap();
        let class = &catalog.classes[0];
        assert!(class.truncated && catalog.truncated);
        assert!(!class.methods.is_empty() && class.methods.len() < 2_000);
        assert_eq!(class.methods[0].name, "member_0");
        assert!(serde_json::to_vec(class).unwrap().len() <= 32 * 16 + 16 * 1024);
        assert!(catalog.warnings[0].starts_with("a.rs: "));
    }

    #[test]
    fn rows_past_a_files_share_go_with_their_relations() {
        let classes: Vec<_> = (0..2_000)
            .map(|index| class("a.rs", &format!("A{index}"), 0))
            .collect();
        let last = classes.last().unwrap().symbol.id.clone();
        let mut catalog = Catalog {
            classes,
            relations: vec![relation("sid:a.rs:A0", 0), relation(&last, 1)],
            ..Catalog::default()
        };
        catalog.fit_stored_budgets(|_| 16).unwrap();
        assert!(catalog.classes.len() < 2_000 && !catalog.classes.is_empty());
        assert_eq!(catalog.classes[0].symbol.name, "A0");
        assert!(catalog.relations.iter().all(|r| r.owner != last));
        assert!(catalog.truncated);
    }

    #[test]
    fn one_files_rows_are_charged_only_to_that_file() {
        let mut catalog = Catalog {
            classes: vec![class("a.rs", "A", 2_000), class("b.rs", "B", 2_000)],
            ..Catalog::default()
        };
        catalog
            .fit_stored_budgets(|path| if path == "b.rs" { 1024 * 1024 } else { 16 })
            .unwrap();
        assert!(catalog.classes[0].truncated);
        assert!(!catalog.classes[1].truncated);
        assert_eq!(catalog.classes[1].methods.len(), 2_000);
    }
}
