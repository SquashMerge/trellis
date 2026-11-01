//! Captured Java package/type proofs, not compiler or member bindings.
use super::*;

type PackageKey = (String, Vec<String>);
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DeclarationId {
    document: usize,
    start: usize,
    end: usize,
    sid: String,
}
#[derive(Default)]
struct Bucket {
    declarations: BTreeSet<DeclarationId>,
    unknown: bool,
}
#[derive(Default)]
struct Package {
    exports: BTreeMap<String, Bucket>,
    declared: BTreeSet<String>,
    /// A file here has a parser error, so any top-level type can be hidden.
    partial: bool,
    declarations: BTreeMap<String, usize>,
}
struct Import {
    path: Vec<String>,
    star: bool,
    static_: bool,
}
#[derive(Default)]
struct Blockers {
    ranges: Vec<(usize, usize)>,
    max_ends: Vec<usize>,
}
impl Blockers {
    fn finish(&mut self, work: &mut Work, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
        self.ranges.sort_unstable();
        charge(self.ranges.len(), work, check)?;
        self.max_ends.resize(self.ranges.len(), 0);
        self.build(0, self.ranges.len(), work, check)?;
        Ok(())
    }
    fn build(
        &mut self,
        left: usize,
        right: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<usize> {
        if left == right {
            return Ok(0);
        }
        owner_step(work, check)?;
        let mid = left + (right - left) / 2;
        let end = self.ranges[mid]
            .1
            .max(self.build(left, mid, work, check)?)
            .max(self.build(mid + 1, right, work, check)?);
        self.max_ends[mid] = end;
        Ok(end)
    }
    fn contains(
        &self,
        start: usize,
        end: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        owner_step(work, check)?;
        let mut pending = vec![(0, self.ranges.len())];
        while let Some((left, right)) = pending.pop() {
            if left == right {
                continue;
            }
            owner_step(work, check)?;
            let mid = left + (right - left) / 2;
            if self.ranges[left].0 > start || self.max_ends[mid] < end {
                continue;
            }
            if self.ranges[mid].0 <= start && self.ranges[mid].1 >= end {
                return Ok(true);
            }
            charge(2, work, check)?;
            pending.push((mid + 1, right));
            pending.push((left, mid));
        }
        Ok(false)
    }
}
#[derive(Default)]
struct Unit {
    package: Vec<String>,
    imports: BTreeMap<(usize, usize), Import>,
    explicit: BTreeMap<String, BTreeSet<Vec<String>>>,
    static_explicit: BTreeSet<String>,
    top_level: BTreeSet<String>,
    blockers: BTreeMap<String, Blockers>,
    on_demand: BTreeMap<String, Bucket>,
    static_unknown: bool,
    /// An import or the package declaration exceeds the path bound, so its binding is unknown.
    cut: bool,
}
pub(super) struct Catalog {
    packages: BTreeMap<PackageKey, Package>,
    units: BTreeMap<usize, Unit>,
    source_sets: BTreeMap<usize, String>,
    /// Source sets with a file whose package is unknown: any of their packages can hold its
    /// types.
    unplaced: BTreeSet<String>,
}
impl Catalog {
    pub(super) fn new(
        docs: &[&DocumentInput],
        trees: &[Tree],
        selected: &[usize],
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut result = Self {
            packages: BTreeMap::new(),
            units: BTreeMap::new(),
            source_sets: BTreeMap::new(),
            unplaced: BTreeSet::new(),
        };
        for (document, doc) in docs.iter().enumerate() {
            if doc.key.language != "java" {
                continue;
            }
            owner_step(work, check)?;
            let mut native = BTreeMap::<_, Vec<_>>::new();
            for declaration in &doc.declarations {
                owner_step(work, check)?;
                if declaration.kind == "type" {
                    native
                        .entry((declaration.range.start, declaration.range.end))
                        .or_default()
                        .push(declaration);
                }
            }
            let root = trees[document].root_node();
            if package_unknown(doc, root, work, check)? {
                result.unplaced.insert(doc.key.source_set_id.clone());
            }
            let package_path = package_path(doc, root, work, check)?;
            owner_step(work, check)?;
            let package = result
                .packages
                .entry((doc.key.source_set_id.clone(), package_path))
                .or_default();
            // A parser error can hide any top-level type of its file.
            package.partial |= root.has_error();
            let mut cursor = root.walk();
            for child in root.named_children(&mut cursor) {
                owner_step(work, check)?;
                if !type_declaration(child) {
                    continue;
                }
                let Some(name) = child.child_by_field_name("name") else {
                    continue;
                };
                owner_step(work, check)?;
                let name = text(doc, name).to_owned();
                owner_step(work, check)?;
                package.declared.insert(name.clone());
                owner_step(work, check)?;
                *package.declarations.entry(name.clone()).or_default() += 1;
                if !public(child, work, check)? {
                    continue;
                }
                owner_step(work, check)?;
                let bucket = package.exports.entry(name).or_default();
                let rows = native.get(&(child.start_byte(), child.end_byte()));
                if child.kind() == "annotation_type_declaration" {
                    bucket.unknown = true;
                } else if let Some(rows) = rows.filter(|rows| rows.len() == 1) {
                    let row = rows[0];
                    if row.role == DeclarationRole::Test {
                        bucket.unknown = true;
                    } else {
                        owner_step(work, check)?;
                        bucket.declarations.insert(DeclarationId {
                            document,
                            start: row.range.start,
                            end: row.range.end,
                            sid: row.syntax_id.clone(),
                        });
                    }
                } else {
                    bucket.unknown = true;
                }
            }
        }
        for package in result.packages.values_mut() {
            owner_step(work, check)?;
            for (name, bucket) in &mut package.exports {
                owner_step(work, check)?;
                bucket.unknown |= package.declarations[name] > 1;
            }
        }
        for &document in selected {
            let doc = docs[document];
            if doc.key.language != "java" {
                continue;
            }
            owner_step(work, check)?;
            let root = trees[document].root_node();
            let mut unit = Unit {
                package: package_path(doc, root, work, check)?,
                ..Unit::default()
            };
            let mut cursor = root.walk();
            for child in root.named_children(&mut cursor) {
                owner_step(work, check)?;
                if child.kind() == "import_declaration" {
                    if let Some(import) = import(doc, child, work, check)? {
                        if !import.star {
                            if let Some(name) = import.path.last() {
                                owner_step(work, check)?;
                                charge(import.path.len(), work, check)?;
                                unit.explicit
                                    .entry(name.clone())
                                    .or_default()
                                    .insert(import.path.clone());
                                if import.static_ {
                                    owner_step(work, check)?;
                                    unit.static_explicit.insert(name.clone());
                                }
                            }
                        } else if import.static_ {
                            unit.static_unknown = true;
                        }
                        owner_step(work, check)?;
                        unit.imports
                            .insert((child.start_byte(), child.end_byte()), import);
                    } else {
                        unit.cut = true;
                    }
                } else if child.kind() == "package_declaration" {
                    unit.cut |= declaration_path(doc, child, work, check)?.is_none();
                } else if type_declaration(child)
                    && let Some(name) = child.child_by_field_name("name")
                {
                    owner_step(work, check)?;
                    unit.top_level.insert(text(doc, name).to_owned());
                }
            }
            scan_blockers(doc, root, &mut unit, work, check)?;
            // Syntax skipped here can hold an import, a shadow or a binding.
            unit.cut |= root.has_error();
            // So no type root is proved, nor one a hidden type of this package could shadow.
            unit.static_unknown |= unit.cut
                || crate::syntax_bounds::scan(root, check)?.is_some()
                || result.unplaced.contains(&doc.key.source_set_id)
                || result
                    .packages
                    .get(&(doc.key.source_set_id.clone(), unit.package.clone()))
                    .is_some_and(|package| package.partial);
            for blockers in unit.blockers.values_mut() {
                blockers.finish(work, check)?;
            }
            // Merge contributors only for units whose sites will be scanned.
            let source_set = &doc.key.source_set_id;
            let mut seen = BTreeSet::new();
            for import in unit.imports.values() {
                owner_step(work, check)?;
                if !import.star || import.static_ {
                    continue;
                }
                charge(import.path.len(), work, check)?;
                if !seen.insert(import.path.clone()) {
                    continue;
                }
                if result.type_prefix(source_set, &import.path, work, check)? {
                    unit.static_unknown = true;
                }
                if let Some(package) = result
                    .packages
                    .get(&(source_set.clone(), import.path.clone()))
                {
                    unit.static_unknown |= package.partial;
                    for (name, bucket) in &package.exports {
                        owner_step(work, check)?;
                        let merged = unit.on_demand.entry(name.clone()).or_default();
                        merged.unknown |= bucket.unknown;
                        for candidate in &bucket.declarations {
                            owner_step(work, check)?;
                            merged.declarations.insert(candidate.clone());
                        }
                    }
                }
            }
            owner_step(work, check)?;
            result.units.insert(document, unit);
            owner_step(work, check)?;
            result.source_sets.insert(document, source_set.clone());
        }
        Ok(result)
    }

    pub(super) fn resolve(
        &self,
        document: usize,
        site: &RawSite,
        external_packages: &mut BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SiteResolution> {
        owner_step(work, check)?;
        let Some(unit) = self.units.get(&document) else {
            return Ok(SiteResolution::unknown("java_package_mapping_unavailable"));
        };
        let source_set = &self.source_sets[&document];
        if let Some(import) = unit.imports.get(&(site.start, site.end)) {
            if import.static_ {
                return Ok(SiteResolution::unknown("java_static_import_unsupported"));
            }
            if import.star {
                return self.wildcard(
                    unit,
                    source_set,
                    &import.path,
                    external_packages,
                    work,
                    check,
                );
            }
            if unit.cut
                || import.path.last().is_some_and(|name| {
                    unit.static_explicit.contains(name)
                        || unit
                            .explicit
                            .get(name)
                            .is_some_and(|bindings| bindings.len() > 1)
                })
            {
                return Ok(SiteResolution::unknown("java_explicit_type_ambiguous"));
            }
            return self.named(source_set, &import.path, external_packages, work, check);
        }
        let Ok(path) = &site.destination else {
            return Ok(SiteResolution::unknown("java_type_path_unsupported"));
        };
        let Some(root) = path.first() else {
            return Ok(SiteResolution::unknown("java_type_path_unsupported"));
        };
        // A type-root interpretation is not proof of an unsupported nested/member target.
        if unit.static_unknown
            || unit.explicit.contains_key(root)
            || unit.top_level.contains(root)
            || unit.on_demand.contains_key(root)
            || self
                .packages
                .get(&(source_set.clone(), unit.package.clone()))
                .is_some_and(|package| package.declared.contains(root))
            || unit
                .blockers
                .get(root)
                .map(|blockers| blockers.contains(site.start, site.end, work, check))
                .transpose()?
                .unwrap_or(false)
        {
            return Ok(SiteResolution::unknown("java_type_root_shadowed"));
        }
        self.named(source_set, path, external_packages, work, check)
    }

    fn named(
        &self,
        source_set: &str,
        path: &[String],
        external_packages: &mut BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SiteResolution> {
        let Some((name, package_path)) =
            path.split_last().filter(|(_, package)| !package.is_empty())
        else {
            return Ok(SiteResolution::unknown("java_type_path_unsupported"));
        };
        charge(path.len(), work, check)?;
        if let Some(package) = self
            .packages
            .get(&(source_set.to_owned(), package_path.to_vec()))
        {
            if package.partial || self.unplaced.contains(source_set) {
                return Ok(SiteResolution::unknown("java_type_identity_ambiguous"));
            }
            if let Some(bucket) = package.exports.get(name) {
                return bucket_resolution(bucket, work, check);
            }
            return Ok(SiteResolution::unknown("java_type_not_proved"));
        }
        if self.type_prefix(source_set, path, work, check)? {
            return Ok(SiteResolution::unknown("java_member_type_unsupported"));
        }
        if let Some(resolution) =
            standard_library(source_set, package_path, external_packages, work, check)?
        {
            return Ok(resolution);
        }
        Ok(SiteResolution::unknown("java_package_mapping_unavailable"))
    }

    fn type_prefix(
        &self,
        source_set: &str,
        path: &[String],
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        // A captured top-level type in a prefix makes this a member path, not a package.
        for split in 0..path.len() {
            charge(split + 1, work, check)?;
            if self
                .packages
                .get(&(source_set.to_owned(), path[..split].to_vec()))
                // Nothing outside the unnamed package can name a type hidden in it.
                .is_some_and(|package| {
                    package.declared.contains(&path[split]) || (split > 0 && package.partial)
                })
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn wildcard(
        &self,
        unit: &Unit,
        source_set: &str,
        path: &[String],
        external_packages: &mut BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SiteResolution> {
        charge(path.len(), work, check)?;
        let Some(package) = self.packages.get(&(source_set.to_owned(), path.to_vec())) else {
            if self.type_prefix(source_set, path, work, check)? {
                return Ok(SiteResolution::unknown("java_member_type_unsupported"));
            }
            if let Some(resolution) =
                standard_library(source_set, path, external_packages, work, check)?
            {
                return Ok(resolution);
            }
            return Ok(SiteResolution::unknown("java_package_mapping_unavailable"));
        };
        if package.partial || self.unplaced.contains(source_set) {
            return Ok(SiteResolution::unknown("java_wildcard_name_ambiguous"));
        }
        let same_package = self
            .packages
            .get(&(source_set.to_owned(), unit.package.clone()));
        let mut result = SiteResolution::default();
        let mut shadowed = false;
        for name in package.exports.keys() {
            owner_step(work, check)?;
            if unit.explicit.contains_key(name)
                || unit.top_level.contains(name)
                || same_package.is_some_and(|package| package.declared.contains(name))
            {
                shadowed = true;
                if unit
                    .explicit
                    .get(name)
                    .is_some_and(|bindings| bindings.len() > 1)
                {
                    result.residual = Some("java_explicit_type_ambiguous");
                }
                continue;
            }
            let bucket = &unit.on_demand[name];
            if unit.static_unknown || bucket.unknown || bucket.declarations.len() != 1 {
                result.residual = Some("java_wildcard_name_ambiguous");
                continue;
            }
            let declaration = bucket.declarations.first().expect("unique declaration");
            owner_step(work, check)?;
            result.targets.insert(Target::Native(declaration.document));
            owner_step(work, check)?;
            result.proof.insert(declaration.document);
        }
        if result.targets.is_empty() && result.residual.is_none() {
            result.residual = Some(if shadowed {
                "java_wildcard_fully_shadowed"
            } else {
                "java_wildcard_empty"
            });
        }
        Ok(result)
    }
}

fn bucket_resolution(
    bucket: &Bucket,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<SiteResolution> {
    owner_step(work, check)?;
    if bucket.unknown || bucket.declarations.len() != 1 {
        return Ok(SiteResolution::unknown("java_type_identity_ambiguous"));
    }
    let declaration = bucket.declarations.first().expect("unique declaration");
    owner_step(work, check)?;
    let targets = BTreeSet::from([Target::Native(declaration.document)]);
    owner_step(work, check)?;
    Ok(SiteResolution {
        targets,
        proof: BTreeSet::from([declaration.document]),
        residual: None,
    })
}
fn standard_library(
    source_set: &str,
    path: &[String],
    packages: &mut BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<SiteResolution>> {
    let Some(root) = path.first() else {
        return Ok(None);
    };
    owner_step(work, check)?;
    let package_path = if root == "javax" {
        charge(path.len(), work, check)?;
        path.join(".")
    } else {
        root.clone()
    };
    let Some(package) = external::java_standard_library(source_set, &package_path) else {
        return Ok(None);
    };
    owner_step(work, check)?;
    let identity = package.identity.clone();
    owner_step(work, check)?;
    packages.entry(identity.clone()).or_insert(package);
    owner_step(work, check)?;
    Ok(Some(SiteResolution {
        targets: BTreeSet::from([Target::External(identity)]),
        ..SiteResolution::default()
    }))
}
fn charge(count: usize, work: &mut Work, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
    for _ in 0..count {
        owner_step(work, check)?;
    }
    Ok(())
}
fn type_declaration(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}
fn public(node: Node<'_>, work: &mut Work, check: &mut impl FnMut() -> Result<()>) -> Result<bool> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        owner_step(work, check)?;
        if child.kind() == "modifiers" {
            let mut cursor = child.walk();
            for modifier in child.children(&mut cursor) {
                owner_step(work, check)?;
                if modifier.kind() == "public" {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
fn package_path(
    doc: &DocumentInput,
    root: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<String>> {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        owner_step(work, check)?;
        if child.kind() == "package_declaration" {
            return Ok(declaration_path(doc, child, work, check)?.unwrap_or_default());
        }
    }
    Ok(Vec::new())
}
/// Whether this file's package declaration was lost to a parser error or the path bound. It
/// comes first, so a file that starts with an import or type with an intact header declares
/// none, whatever else is broken.
fn package_unknown(
    doc: &DocumentInput,
    root: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<bool> {
    if root.is_error() {
        return Ok(true);
    }
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        owner_step(work, check)?;
        if child.kind() == "package_declaration" {
            return Ok(child.has_error() || declaration_path(doc, child, work, check)?.is_none());
        }
        if !matches!(child.kind(), "line_comment" | "block_comment") {
            let body = child.child_by_field_name("body");
            let mut parts = child.walk();
            let header_error = child
                .children(&mut parts)
                .any(|part| part.has_error() && Some(part) != body);
            return Ok(header_error
                || (root.has_error()
                    && child.kind() != "import_declaration"
                    && !type_declaration(child)));
        }
    }
    Ok(false)
}
fn declaration_path(
    doc: &DocumentInput,
    node: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Vec<String>>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        owner_step(work, check)?;
        if matches!(child.kind(), "identifier" | "scoped_identifier") {
            return path(doc, child, work, check);
        }
    }
    Ok(None)
}
fn path(
    doc: &DocumentInput,
    node: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Vec<String>>> {
    let mut parts = Vec::new();
    owner_step(work, check)?;
    let mut pending = vec![(node, 0)];
    while let Some((node, depth)) = pending.pop() {
        owner_step(work, check)?;
        if depth > DEPTH {
            return Ok(None);
        }
        match node.kind() {
            "identifier" | "type_identifier" => {
                owner_step(work, check)?;
                parts.push(text(doc, node).to_owned());
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let mut children = Vec::new();
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    owner_step(work, check)?;
                    if child.kind() == "." {
                        continue;
                    }
                    if !matches!(
                        child.kind(),
                        "identifier"
                            | "type_identifier"
                            | "scoped_identifier"
                            | "scoped_type_identifier"
                    ) {
                        return Ok(None);
                    }
                    children.push((child, depth + 1));
                }
                for child in children.into_iter().rev() {
                    owner_step(work, check)?;
                    pending.push(child);
                }
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(parts))
}
fn import(
    doc: &DocumentInput,
    node: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Import>> {
    let mut static_ = false;
    let mut star = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        owner_step(work, check)?;
        static_ |= child.kind() == "static";
        star |= child.kind() == "asterisk";
    }
    Ok(
        declaration_path(doc, node, work, check)?.map(|path| Import {
            path,
            star,
            static_,
        }),
    )
}
fn add_blocker(
    unit: &mut Unit,
    name: String,
    start: usize,
    end: usize,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    owner_step(work, check)?;
    unit.blockers
        .entry(name)
        .or_default()
        .ranges
        .push((start, end));
    Ok(())
}
fn scan_blockers(
    doc: &DocumentInput,
    root: Node<'_>,
    unit: &mut Unit,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    super::walk(root, check, &mut |node, check| {
        if type_declaration(node)
            && let (Some(parent), Some(name)) = (node.parent(), node.child_by_field_name("name"))
            && parent.kind() != "program"
        {
            let mut scope = parent;
            if scope.kind() == "enum_body_declarations" {
                scope = scope.parent().expect("enum body");
            }
            if scope.kind() == "switch_block_statement_group" {
                scope = scope.parent().expect("switch block");
            }
            let start = if matches!(scope.kind(), "block" | "constructor_body" | "switch_block") {
                node.start_byte()
            } else {
                scope.start_byte()
            };
            owner_step(work, check)?;
            add_blocker(
                unit,
                text(doc, name).to_owned(),
                start,
                scope.end_byte(),
                work,
                check,
            )?;
        }
        if node.kind() == "type_parameter" {
            let Some(owner) = node.parent().and_then(|parameters| parameters.parent()) else {
                return Ok(false);
            };
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                owner_step(work, check)?;
                if child.kind() == "type_identifier" {
                    owner_step(work, check)?;
                    add_blocker(
                        unit,
                        text(doc, child).to_owned(),
                        owner.start_byte(),
                        owner.end_byte(),
                        work,
                        check,
                    )?;
                    break;
                }
            }
        }
        Ok(false)
    })
}
pub(super) fn extract(
    doc: &DocumentInput,
    tree: &Tree,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<RawSite>> {
    let mut result = Vec::new();
    super::walk(tree.root_node(), check, &mut |node, check| {
        let kind = match node.kind() {
            "import_declaration" => "import",
            "scoped_type_identifier" => "reference",
            _ => return Ok(false),
        };
        ensure!(result.len() < SITES, "bounded_work: reference site count");
        let destination = if kind == "import" {
            declaration_path(doc, node, work, check)?
        } else {
            path(doc, node, work, check)?
        }
        .ok_or("java_type_path_unsupported");
        owner_step(work, check)?;
        result.push(raw(doc, node, kind, destination));
        // Keep a maximal scoped type site without duplicate inner-prefix sites.
        Ok(true)
    })?;
    result.sort_by(|a, b| {
        (a.start, a.end, a.kind, &a.spelling).cmp(&(b.start, b.end, b.kind, &b.spelling))
    });
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};

    fn documents() -> Vec<DocumentInput> {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in [
            ("Use.java", "package app; import pkg.*; class Use {}"),
            ("T.java", "package pkg; public class T {}"),
        ] {
            std::fs::write(root.path().join(path), text).unwrap();
        }
        let identity =
            crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap();
        let (graph, native) = crate::indexer::index_workspace_with_native(
            &crate::indexer::IndexOptions::new(root.path().to_owned()),
            &identity.record_id,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        graph
            .files
            .into_iter()
            .map(|file| {
                let key = native
                    .revision
                    .documents
                    .iter()
                    .find(|doc| doc.key.path == file.path)
                    .unwrap()
                    .key
                    .clone();
                DocumentInput {
                    semantic: Default::default(),
                    declarations: native
                        .declarations
                        .iter()
                        .filter(|row| row.document == key)
                        .cloned()
                        .collect(),
                    calls: native
                        .calls
                        .iter()
                        .filter(|row| row.document == key)
                        .cloned()
                        .collect(),
                    key,
                    file,
                }
            })
            .collect()
    }

    fn expansion(test: impl FnOnce(&Catalog, &Unit, &str, &mut Work)) {
        let documents = documents();
        let docs: Vec<_> = documents.iter().collect();
        let trees: Vec<_> = docs
            .iter()
            .map(|doc| parse(doc, &mut || Ok(())).unwrap())
            .collect();
        let mut work = Work::new(work::reference_limit(docs.len(), 0));
        let selected: Vec<_> = (0..docs.len()).collect();
        let catalog = Catalog::new(&docs, &trees, &selected, &mut work, &mut || Ok(())).unwrap();
        let document = docs
            .iter()
            .position(|doc| doc.key.path == "Use.java")
            .unwrap();
        test(
            &catalog,
            &catalog.units[&document],
            &docs[document].key.source_set_id,
            &mut work,
        );
    }

    #[test]
    fn wildcard_expansion_uses_the_accumulated_work_limit() {
        expansion(|catalog, unit, source_set, work| {
            assert!(work.used > 0);
            work.limit = work.used + 2;
            let result = catalog.wildcard(
                unit,
                source_set,
                &["pkg".into()],
                &mut BTreeMap::new(),
                work,
                &mut || Ok(()),
            );
            let error = result
                .err()
                .expect("expansion must exhaust the shared budget");
            assert!(error.to_string().starts_with("bounded_work:"));
            assert!(work.used > work.limit);
        });
    }

    #[test]
    fn wildcard_expansion_cancels_before_target_publication() {
        expansion(|catalog, unit, source_set, work| {
            let mut checks = 0;
            let result = catalog.wildcard(
                unit,
                source_set,
                &["pkg".into()],
                &mut BTreeMap::new(),
                work,
                &mut || {
                    checks += 1;
                    ensure!(checks < 3, "cancelled");
                    Ok(())
                },
            );
            assert_eq!(
                result.err().expect("expansion must cancel").to_string(),
                "cancelled"
            );
            assert_eq!(checks, 3);
        });
    }
}
