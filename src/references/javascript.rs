//! Captured ESM export names. No runtime, package member or call binding.
use super::*;
use std::sync::Arc;

type Route = BTreeSet<(usize, String)>;
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DeclarationId {
    document: usize,
    start: usize,
    end: usize,
    sid: String,
}
#[derive(Clone)]
struct Candidate {
    id: DeclarationId,
    proof: BTreeSet<usize>,
}
#[derive(Clone, Default)]
struct Bucket {
    candidates: BTreeMap<DeclarationId, Candidate>,
    unknown: bool,
}
#[derive(Default)]
struct Names {
    names: BTreeMap<String, Bucket>,
    unknown_set: bool,
}
#[derive(Clone)]
enum BindingKind {
    Direct(DeclarationId),
    Alias {
        original: String,
        declaration: DeclarationId,
    },
    Imported {
        source: Option<String>,
        name: Option<String>,
    },
    Unknown,
}
struct Binding {
    start: usize,
    kind: BindingKind,
}
enum Export {
    Local(String),
    Forward(Option<String>, Option<String>),
    Unknown,
}
struct Source {
    start: usize,
    end: usize,
    specifier: Option<String>,
    star: bool,
    supported: bool,
}
#[derive(Default)]
struct Unit {
    test_only: bool,
    bindings: BTreeMap<String, Vec<Binding>>,
    explicit: BTreeMap<String, Vec<Export>>,
    sources: Vec<Source>,
    unknown_set: bool,
}
#[derive(Default)]
struct Hazards {
    writes: BTreeSet<String>,
    dynamic: bool,
}
pub(super) struct Catalog {
    results: BTreeMap<(usize, usize, usize), SiteResolution>,
    packages: BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
}
struct Resolver<'a> {
    docs: &'a [&'a DocumentInput],
    trees: &'a [Tree],
    documents: &'a BTreeMap<DocumentKey, usize>,
    units: BTreeMap<usize, Arc<Unit>>,
    hazards: BTreeMap<usize, Hazards>,
    memo: BTreeMap<(usize, usize, Route), Arc<Names>>,
    named_memo: BTreeMap<(usize, String, usize, Route), Arc<Bucket>>,
}
impl Catalog {
    pub(super) fn new(
        docs: &[&DocumentInput],
        trees: &[Tree],
        documents: &BTreeMap<DocumentKey, usize>,
        selected: &[usize],
        packages: &external::JavascriptPackages,
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut resolver = Resolver {
            docs,
            trees,
            documents,
            units: BTreeMap::new(),
            hazards: BTreeMap::new(),
            memo: BTreeMap::new(),
            named_memo: BTreeMap::new(),
        };
        for (document, doc) in docs.iter().enumerate() {
            if doc.key.language != "javascript" {
                continue;
            }
            let mut native = BTreeMap::<_, Vec<&Declaration>>::new();
            let mut test_only = false;
            for row in &doc.declarations {
                owner_step(work, check)?;
                if row.kind == "module"
                    && row.ancestors.is_empty()
                    && row.role == DeclarationRole::Test
                {
                    test_only = true;
                }
                native
                    .entry((row.range.start, row.range.end, row.kind.as_str()))
                    .or_default()
                    .push(row);
            }
            let mut unit = inventory(
                doc,
                trees[document].root_node(),
                document,
                &native,
                work,
                check,
            )?;
            unit.test_only = test_only;
            owner_step(work, check)?;
            resolver.units.insert(document, Arc::new(unit));
        }
        let mut catalog = Self {
            results: BTreeMap::new(),
            packages: BTreeMap::new(),
        };
        for &document in selected {
            let Some(unit) = resolver.units.get(&document).cloned() else {
                continue;
            };
            resolver.hazards(document, work, check)?;
            let mut contributors = Vec::new();
            for source in unit.sources.iter().filter(|s| s.star) {
                let names =
                    resolver.source_exports(document, source, 1, &Route::new(), work, check)?;
                owner_step(work, check)?;
                contributors.push(names);
            }
            let merged = if let [only] = contributors.as_slice() {
                Arc::clone(only)
            } else {
                let mut merged = Names::default();
                for names in &contributors {
                    merge_names(&mut merged, names, work, check)?;
                }
                Arc::new(merged)
            };
            let mut stars = contributors.into_iter();
            for source in &unit.sources {
                owner_step(work, check)?;
                let mut resolution = SiteResolution::default();
                if source.star {
                    let own = stars.next().expect("star contribution");
                    if own.unknown_set || unit.unknown_set || resolver.hazards[&document].dynamic {
                        resolution.residual = Some("javascript_export_name_set_unknown");
                    }
                    let mut shadowed = false;
                    for name in own.names.keys() {
                        owner_step(work, check)?;
                        if unit.explicit.contains_key(name) {
                            shadowed = true;
                            let bucket =
                                resolver.named(document, name, 0, &Route::new(), work, check)?;
                            if bucket.unknown || bucket.candidates.len() != 1 {
                                resolution.residual = Some("javascript_explicit_name_unproved");
                            }
                            continue;
                        }
                        let bucket = &merged.names[name];
                        if unit.unknown_set
                            || resolver.hazards[&document].dynamic
                            || bucket.unknown
                            || bucket.candidates.len() != 1
                        {
                            resolution.residual = Some("javascript_wildcard_name_ambiguous");
                        } else {
                            add_bucket(&mut resolution, bucket, work, check)?;
                        }
                    }
                    if resolution.targets.is_empty() && resolution.residual.is_none() {
                        resolution.residual = Some(if shadowed {
                            "javascript_wildcard_fully_shadowed"
                        } else {
                            "javascript_wildcard_empty"
                        });
                    }
                } else {
                    match resolver.target(document, source.specifier.as_deref(), work, check)? {
                        Ok(target) => {
                            owner_step(work, check)?;
                            resolution.targets.insert(Target::Native(target));
                        }
                        Err(reason) => resolution.residual = Some(reason),
                    }
                }
                if source.supported
                    && let Some(specifier) = source.specifier.as_deref()
                    && !relative(specifier)
                {
                    charge(
                        specifier.len()
                            + packages
                                .resolution_work(&docs[document].key.source_set_id, specifier),
                        work,
                        check,
                    )?;
                    if let Some(package) =
                        packages.resolve(&docs[document].key.source_set_id, specifier)
                    {
                        charge_package(&package, work, check)?;
                        resolution
                            .targets
                            .insert(Target::External(package.identity.clone()));
                        catalog
                            .packages
                            .entry(package.identity.clone())
                            .or_insert(package);
                        resolution.residual =
                            source.star.then_some("javascript_external_members_unknown");
                    }
                }
                owner_step(work, check)?;
                catalog
                    .results
                    .insert((document, source.start, source.end), resolution);
            }
        }
        Ok(catalog)
    }
    pub(super) fn resolve(
        &self,
        document: usize,
        site: &RawSite,
        packages: &mut BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SiteResolution> {
        owner_step(work, check)?;
        let Some(result) = self.results.get(&(document, site.start, site.end)) else {
            return Ok(SiteResolution::unknown("dynamic_import_unsupported"));
        };
        let mut resolution = SiteResolution {
            residual: result.residual,
            ..SiteResolution::default()
        };
        for target in &result.targets {
            owner_step(work, check)?;
            if let Target::External(identity) = target
                && !packages.contains_key(identity)
            {
                let package = &self.packages[identity];
                charge_package(package, work, check)?;
                packages.insert(identity.clone(), package.clone());
            }
            resolution.targets.insert(target.clone());
        }
        charge(result.proof.len(), work, check)?;
        resolution.proof.extend(result.proof.iter().copied());
        Ok(resolution)
    }
}
impl Resolver<'_> {
    fn target(
        &self,
        document: usize,
        specifier: Option<&str>,
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<std::result::Result<usize, &'static str>> {
        owner_step(work, check)?;
        let Some(specifier) = specifier else {
            return Ok(Err("import_escape_unsupported"));
        };
        if !relative(specifier) {
            return Ok(Err("absolute_or_bare_import"));
        }
        charge(
            self.docs[document].key.path.len() + specifier.len(),
            work,
            check,
        )?;
        let Some(path) = relative_path(&self.docs[document].key.path, specifier) else {
            return Ok(Err("relative_import_outside_source"));
        };
        let candidates = if specifier.ends_with('/') {
            charge(path.len() + 1, work, check)?;
            vec![if path.is_empty() {
                "index.js".to_owned()
            } else {
                format!("{path}/index.js")
            }]
        } else if path.rsplit('/').next().is_some_and(|p| p.contains('.')) {
            vec![path]
        } else {
            charge(path.len() * 4 + 4, work, check)?;
            vec![
                format!("{path}.js"),
                format!("{path}.mjs"),
                format!("{path}.cjs"),
                format!("{path}/index.js"),
            ]
        };
        charge(candidates.len(), work, check)?;
        let found = find(self.docs[document], &candidates, self.documents);
        Ok(match found.as_slice() {
            [target] => Ok(*target),
            [] => Err("module_path_not_indexed"),
            _ => Err("module_path_ambiguous"),
        })
    }
    fn source_exports(
        &mut self,
        document: usize,
        source: &Source,
        depth: usize,
        route: &Route,
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Arc<Names>> {
        if !source.supported || depth > FOLLOW {
            return Ok(Arc::new(unknown_names()));
        }
        match self.target(document, source.specifier.as_deref(), work, check)? {
            Ok(target) => self.exports(target, depth, route, work, check),
            Err(_) => Ok(Arc::new(unknown_names())),
        }
    }
    fn exports(
        &mut self,
        document: usize,
        depth: usize,
        route: &Route,
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Arc<Names>> {
        charge(route.len() + 1, work, check)?;
        if depth > FOLLOW
            || route.contains(&(document, "*".into()))
            || (depth > 0
                && (self.units[&document].test_only
                    || self.docs[document].key.path.ends_with(".cjs")))
        {
            return Ok(Arc::new(unknown_names()));
        }
        let key = (document, depth, route.clone());
        if let Some(names) = self.memo.get(&key) {
            return Ok(Arc::clone(names));
        }
        self.hazards(document, work, check)?;
        let unit = Arc::clone(&self.units[&document]);
        let mut active = route.clone();
        active.insert((document, "*".into()));
        let mut names = Names {
            unknown_set: unit.unknown_set || self.hazards[&document].dynamic,
            ..Names::default()
        };
        for source in unit.sources.iter().filter(|s| s.star) {
            let other = self.source_exports(document, source, depth + 1, &active, work, check)?;
            merge_names(&mut names, &other, work, check)?;
        }
        // ESM explicit exported names, not private local bindings, shadow all stars.
        for name in unit
            .explicit
            .keys()
            .filter(|name| name.as_str() != "default")
        {
            let bucket = self.explicit(document, name, depth, &active, work, check)?;
            charge_bucket(&bucket, work, check)?;
            names.names.insert(name.clone(), bucket);
        }
        for (name, bucket) in &mut names.names {
            owner_step(work, check)?;
            if self.hazards[&document].dynamic
                || (!unit.explicit.contains_key(name) && unit.unknown_set)
            {
                bucket.unknown = true;
            }
            extend_proof(bucket, document, work, check)?;
        }
        owner_step(work, check)?;
        let names = Arc::new(names);
        self.memo.insert(key, Arc::clone(&names));
        Ok(names)
    }
    fn named(
        &mut self,
        document: usize,
        name: &str,
        depth: usize,
        route: &Route,
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Arc<Bucket>> {
        charge(route.len() + name.len() + 1, work, check)?;
        if depth > FOLLOW
            || route.contains(&(document, name.to_owned()))
            || (depth > 0
                && (self.units[&document].test_only
                    || self.docs[document].key.path.ends_with(".cjs")))
        {
            return Ok(Arc::new(unknown_bucket()));
        }
        let key = (document, name.to_owned(), depth, route.clone());
        if let Some(bucket) = self.named_memo.get(&key) {
            return Ok(Arc::clone(bucket));
        }
        self.hazards(document, work, check)?;
        let unit = Arc::clone(&self.units[&document]);
        let mut active = route.clone();
        active.insert((document, name.to_owned()));
        let mut bucket = if unit.explicit.contains_key(name) {
            self.explicit(document, name, depth, &active, work, check)?
        } else if name == "default" {
            unknown_bucket()
        } else {
            let mut bucket = Bucket {
                unknown: unit.unknown_set || self.hazards[&document].dynamic,
                ..Bucket::default()
            };
            for source in unit.sources.iter().filter(|s| s.star) {
                let other = if source.supported && depth < FOLLOW {
                    match self.target(document, source.specifier.as_deref(), work, check)? {
                        Ok(target) => self.named(target, name, depth + 1, &active, work, check)?,
                        Err(_) => Arc::new(unknown_bucket()),
                    }
                } else {
                    Arc::new(unknown_bucket())
                };
                merge_bucket(&mut bucket, &other, work, check)?;
            }
            bucket
        };
        extend_proof(&mut bucket, document, work, check)?;
        owner_step(work, check)?;
        let bucket = Arc::new(bucket);
        self.named_memo.insert(key, Arc::clone(&bucket));
        Ok(bucket)
    }
    fn explicit(
        &mut self,
        document: usize,
        name: &str,
        depth: usize,
        route: &Route,
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Bucket> {
        owner_step(work, check)?;
        let unit = Arc::clone(&self.units[&document]);
        let Some(exports) = unit.explicit.get(name) else {
            return Ok(unknown_bucket());
        };
        if exports.len() != 1 {
            return Ok(unknown_bucket());
        }
        let mut bucket = match &exports[0] {
            Export::Local(local) => self.binding(document, local, depth, route, work, check)?,
            Export::Forward(Some(source), Some(original)) if depth < FOLLOW => {
                match self.target(document, Some(source), work, check)? {
                    Ok(target) => {
                        let bucket = self.named(target, original, depth + 1, route, work, check)?;
                        charge_bucket(&bucket, work, check)?;
                        (*bucket).clone()
                    }
                    Err(_) => unknown_bucket(),
                }
            }
            _ => unknown_bucket(),
        };
        if self.hazards[&document].dynamic {
            bucket.unknown = true;
        }
        extend_proof(&mut bucket, document, work, check)?;
        Ok(bucket)
    }
    fn binding(
        &mut self,
        document: usize,
        name: &str,
        depth: usize,
        route: &Route,
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Bucket> {
        let unit = Arc::clone(&self.units[&document]);
        let mut name = name.to_owned();
        let mut seen = BTreeSet::new();
        let mut before = usize::MAX;
        let mut alias = None;
        loop {
            charge(name.len() + 1, work, check)?;
            if !seen.insert(name.clone()) || self.hazards[&document].writes.contains(&name) {
                return Ok(unknown_bucket());
            }
            let Some(bindings) = unit.bindings.get(&name) else {
                return Ok(unknown_bucket());
            };
            if bindings.len() != 1 || bindings[0].start >= before {
                return Ok(unknown_bucket());
            }
            let binding = &bindings[0];
            match &binding.kind {
                BindingKind::Direct(id) => {
                    charge(id.sid.len() + 2, work, check)?;
                    let bucket = Bucket {
                        candidates: BTreeMap::from([(
                            id.clone(),
                            Candidate {
                                id: id.clone(),
                                proof: BTreeSet::from([document]),
                            },
                        )]),
                        unknown: false,
                    };
                    return local_alias(bucket, alias, work, check);
                }
                BindingKind::Alias {
                    original,
                    declaration,
                } => {
                    if alias.is_none() {
                        charge(declaration.sid.len() + 1, work, check)?;
                        alias = Some(declaration.clone());
                    }
                    before = binding.start;
                    name = original.clone();
                }
                BindingKind::Imported {
                    source: Some(source),
                    name: Some(original),
                } if depth < FOLLOW => {
                    return match self.target(document, Some(source), work, check)? {
                        Ok(target) => {
                            let bucket =
                                self.named(target, original, depth + 1, route, work, check)?;
                            charge_bucket(&bucket, work, check)?;
                            local_alias((*bucket).clone(), alias, work, check)
                        }
                        Err(_) => Ok(unknown_bucket()),
                    };
                }
                _ => return Ok(unknown_bucket()),
            }
        }
    }
    fn hazards(
        &mut self,
        document: usize,
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        if self.hazards.contains_key(&document) {
            check()?;
            return Ok(());
        }
        let hazards = body_hazards(
            self.docs[document],
            self.trees[document].root_node(),
            work,
            check,
        )?;
        owner_step(work, check)?;
        self.hazards.insert(document, hazards);
        Ok(())
    }
}

fn charge(count: usize, work: &mut usize, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
    for _ in 0..count {
        owner_step(work, check)?;
    }
    Ok(())
}
fn charge_package(
    package: &external::ExternalPackage,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    charge(
        package.identity.source_set_id.len() + package.identity.package.len() + 1,
        work,
        check,
    )?;
    for evidence in &package.evidence {
        let size = match evidence {
            external::ExternalEvidence::Manifest {
                source_set_id,
                native_revision_id,
                dependency_hash,
                path,
                content_hash,
                import_root,
                section,
                key,
                version_constraint,
            } => {
                source_set_id.len()
                    + native_revision_id.len()
                    + dependency_hash.len()
                    + path.len()
                    + content_hash.len()
                    + import_root.len()
                    + section.len()
                    + key.len()
                    + version_constraint.as_ref().map_or(0, String::len)
            }
            external::ExternalEvidence::StandardLibraryCatalog { catalog, entry } => {
                catalog.len() + entry.len()
            }
        };
        charge(size + 1, work, check)?;
    }
    Ok(())
}
fn local_alias(
    mut bucket: Bucket,
    alias: Option<DeclarationId>,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Bucket> {
    let Some(id) = alias else {
        return Ok(bucket);
    };
    if bucket.unknown || bucket.candidates.len() != 1 {
        return Ok(unknown_bucket());
    }
    // A local variable has its own binding. The RHS proof admits the initializer,
    // but only an import binding re-export preserves the imported identity.
    charge(id.sid.len() + 2, work, check)?;
    let mut candidate = bucket.candidates.pop_first().expect("unique alias RHS").1;
    candidate.proof.insert(id.document);
    candidate.id = id.clone();
    Ok(Bucket {
        candidates: BTreeMap::from([(id, candidate)]),
        unknown: false,
    })
}
fn unknown_bucket() -> Bucket {
    Bucket {
        unknown: true,
        ..Bucket::default()
    }
}
fn unknown_names() -> Names {
    Names {
        unknown_set: true,
        ..Names::default()
    }
}
fn charge_bucket(
    bucket: &Bucket,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    owner_step(work, check)?;
    for candidate in bucket.candidates.values() {
        charge(
            candidate.id.sid.len() + candidate.proof.len() + 1,
            work,
            check,
        )?;
    }
    Ok(())
}
fn merge_bucket(
    into: &mut Bucket,
    other: &Bucket,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    into.unknown |= other.unknown;
    for (id, candidate) in &other.candidates {
        charge(candidate.proof.len() + id.sid.len() + 1, work, check)?;
        let merged = into
            .candidates
            .entry(id.clone())
            .or_insert_with(|| candidate.clone());
        merged.proof.extend(candidate.proof.iter().copied());
    }
    Ok(())
}
fn merge_names(
    into: &mut Names,
    other: &Names,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    // Unknown sets compete only for spellings they do not themselves settle.
    // An explicit export may settle one spelling while its other star names remain unknown.
    if other.unknown_set {
        for (name, bucket) in &mut into.names {
            owner_step(work, check)?;
            if !other.names.contains_key(name) {
                bucket.unknown = true;
            }
        }
    }
    for (name, bucket) in &other.names {
        charge(name.len() + 1, work, check)?;
        let unknown = into.unknown_set && !into.names.contains_key(name);
        let merged = into.names.entry(name.clone()).or_default();
        merged.unknown |= unknown;
        merge_bucket(merged, bucket, work, check)?;
    }
    into.unknown_set |= other.unknown_set;
    Ok(())
}
fn extend_proof(
    bucket: &mut Bucket,
    document: usize,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for candidate in bucket.candidates.values_mut() {
        owner_step(work, check)?;
        candidate.proof.insert(document);
    }
    Ok(())
}
fn add_bucket(
    result: &mut SiteResolution,
    bucket: &Bucket,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for candidate in bucket.candidates.values() {
        charge(candidate.proof.len() + 1, work, check)?;
        result.targets.insert(Target::Native(candidate.id.document));
        result.proof.extend(candidate.proof.iter().copied());
    }
    Ok(())
}
fn relative(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../")
}
fn literal(doc: &DocumentInput, node: Node<'_>) -> Option<String> {
    let value = text(doc, node);
    if node.kind() != "string" || value.contains('\\') {
        return None;
    }
    value.get(1..value.len().checked_sub(1)?).map(str::to_owned)
}
fn name(doc: &DocumentInput, node: Node<'_>) -> Option<String> {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            crate::native_evidence::lookup("javascript", text(doc, node)).ok()
        }
        "default" => Some("default".into()),
        "string" => literal(doc, node),
        _ => None,
    }
}
fn child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|n| n.kind() == kind)
}
fn star(node: Node<'_>) -> bool {
    node.kind() == "export_statement"
        && node.child_by_field_name("source").is_some()
        && child(node, "export_clause").is_none()
        && child(node, "namespace_export").is_none()
}
fn source(doc: &DocumentInput, node: Node<'_>) -> Option<Source> {
    let source = node.child_by_field_name("source")?;
    let star = star(node);
    let site = if star { node } else { source };
    Some(Source {
        start: site.start_byte(),
        end: site.end_byte(),
        specifier: literal(doc, source),
        star,
        supported: child(node, "import_attribute").is_none(),
    })
}
fn insert_binding(
    unit: &mut Unit,
    name: String,
    start: usize,
    kind: BindingKind,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    charge(name.len() + 1, work, check)?;
    unit.bindings
        .entry(name)
        .or_default()
        .push(Binding { start, kind });
    Ok(())
}
fn insert_export(
    unit: &mut Unit,
    name: String,
    export: Export,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    charge(name.len() + 1, work, check)?;
    unit.explicit.entry(name).or_default().push(export);
    Ok(())
}
fn imported_name(doc: &DocumentInput, node: Node<'_>) -> Option<(String, Option<String>)> {
    match node.kind() {
        "identifier" if node.parent().is_some_and(|p| p.kind() == "import_clause") => {
            name(doc, node).map(|local| (local, Some("default".into())))
        }
        "import_specifier" => node.child_by_field_name("name").and_then(|original| {
            name(doc, node.child_by_field_name("alias").unwrap_or(original))
                .map(|local| (local, name(doc, original)))
        }),
        "namespace_import" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find_map(|n| name(doc, n))
                .map(|local| (local, None))
        }
        _ => None,
    }
}
fn inventory(
    doc: &DocumentInput,
    root: Node<'_>,
    document: usize,
    native: &BTreeMap<(usize, usize, &str), Vec<&Declaration>>,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Unit> {
    let mut unit = Unit::default();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        check()?;
        if let Some(source) = source(doc, statement) {
            charge(
                source.specifier.as_ref().map_or(0, String::len) + 1,
                work,
                check,
            )?;
            unit.sources.push(source);
        }
        if statement.kind() == "import_statement" {
            if let Some(clause) = child(statement, "import_clause") {
                let specifier = statement
                    .child_by_field_name("source")
                    .and_then(|n| literal(doc, n));
                let supported = child(statement, "import_attribute").is_none();
                walk(clause, check, &mut |node, check| {
                    let pair = imported_name(doc, node);
                    if let Some((local, original)) = pair {
                        charge(specifier.as_ref().map_or(0, String::len), work, check)?;
                        insert_binding(
                            &mut unit,
                            local,
                            statement.start_byte(),
                            if supported {
                                BindingKind::Imported {
                                    source: specifier.clone(),
                                    name: original,
                                }
                            } else {
                                BindingKind::Unknown
                            },
                            work,
                            check,
                        )?;
                        return Ok(true);
                    }
                    Ok(false)
                })?;
            }
            continue;
        }
        let exported = statement.kind() == "export_statement";
        let default = exported && {
            let mut cursor = statement.walk();
            statement
                .children(&mut cursor)
                .any(|n| n.kind() == "default")
        };
        let declaration = if exported {
            statement.child_by_field_name("declaration")
        } else {
            Some(statement)
        };
        if let Some(declaration) = declaration {
            declarations(
                doc,
                declaration,
                document,
                native,
                &mut unit,
                exported,
                default,
                work,
                check,
            )?;
        }
        if !exported {
            continue;
        }
        if default && declaration.is_none() {
            // Named default expressions can be native function/class nodes too.
            if let Some(value) = statement.child_by_field_name("value")
                && matches!(
                    value.kind(),
                    "function_expression" | "generator_function" | "class"
                )
            {
                declarations(
                    doc, value, document, native, &mut unit, true, true, work, check,
                )?;
            } else {
                insert_export(&mut unit, "default".into(), Export::Unknown, work, check)?;
            }
        }
        if let Some(clause) = child(statement, "export_clause") {
            let mut cursor = clause.walk();
            for specifier in clause.named_children(&mut cursor) {
                if specifier.kind() != "export_specifier" {
                    continue;
                }
                let original = specifier.child_by_field_name("name");
                let alias = specifier
                    .child_by_field_name("alias")
                    .or(original)
                    .and_then(|n| name(doc, n));
                let Some(alias) = alias else {
                    unit.unknown_set = true;
                    continue;
                };
                let original = original.and_then(|n| name(doc, n));
                let export = if let Some(source_node) = statement.child_by_field_name("source") {
                    if child(statement, "import_attribute").is_some() {
                        Export::Unknown
                    } else {
                        Export::Forward(literal(doc, source_node), original)
                    }
                } else {
                    original.map(Export::Local).unwrap_or(Export::Unknown)
                };
                insert_export(&mut unit, alias, export, work, check)?;
            }
        }
        if let Some(namespace) = child(statement, "namespace_export") {
            let mut cursor = namespace.walk();
            if let Some(name) = namespace
                .named_children(&mut cursor)
                .next()
                .and_then(|n| name(doc, n))
            {
                insert_export(&mut unit, name, Export::Unknown, work, check)?;
            } else {
                unit.unknown_set = true;
            }
        }
    }
    Ok(unit)
}
#[allow(clippy::too_many_arguments)]
fn declarations(
    doc: &DocumentInput,
    declaration: Node<'_>,
    document: usize,
    native: &BTreeMap<(usize, usize, &str), Vec<&Declaration>>,
    unit: &mut Unit,
    exported: bool,
    default: bool,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    if matches!(
        declaration.kind(),
        "lexical_declaration" | "variable_declaration"
    ) {
        let mut cursor = declaration.walk();
        for node in declaration
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "variable_declarator")
        {
            record_declaration(
                doc, node, document, native, unit, exported, false, work, check,
            )?;
        }
    } else if matches!(
        declaration.kind(),
        "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "function_expression"
            | "generator_function"
            | "class"
    ) {
        record_declaration(
            doc,
            declaration,
            document,
            native,
            unit,
            exported,
            default,
            work,
            check,
        )?;
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn record_declaration(
    doc: &DocumentInput,
    node: Node<'_>,
    document: usize,
    native: &BTreeMap<(usize, usize, &str), Vec<&Declaration>>,
    unit: &mut Unit,
    exported: bool,
    default: bool,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let Some(local) = node.child_by_field_name("name").and_then(|n| name(doc, n)) else {
        if default {
            insert_export(unit, "default".into(), Export::Unknown, work, check)?;
        }
        if let Some(pattern) = node.child_by_field_name("name") {
            for local in pattern_names(doc, pattern, work, check)? {
                insert_binding(
                    unit,
                    local.clone(),
                    node.start_byte(),
                    BindingKind::Unknown,
                    work,
                    check,
                )?;
                if exported {
                    insert_export(unit, local, Export::Unknown, work, check)?;
                }
            }
        }
        return Ok(());
    };
    let kind = match node.kind() {
        "variable_declarator" => "variable",
        "class_declaration" | "class" => "type",
        _ => "function",
    };
    owner_step(work, check)?;
    let rows = native.get(&(node.start_byte(), node.end_byte(), kind));
    let mut binding = if let Some(rows) = rows
        && rows.len() == 1
        && rows[0].role == DeclarationRole::Production
    {
        charge(rows[0].syntax_id.len() + 1, work, check)?;
        BindingKind::Direct(DeclarationId {
            document,
            start: node.start_byte(),
            end: node.end_byte(),
            sid: rows[0].syntax_id.clone(),
        })
    } else {
        BindingKind::Unknown
    };
    if node.kind() == "variable_declarator"
        && let Some(value) = node.child_by_field_name("value")
    {
        if value.kind() == "identifier" {
            binding = match (name(doc, value), binding) {
                (Some(original), BindingKind::Direct(declaration)) => BindingKind::Alias {
                    original,
                    declaration,
                },
                _ => BindingKind::Unknown,
            };
        } else if !matches!(
            value.kind(),
            "number"
                | "string"
                | "true"
                | "false"
                | "null"
                | "undefined"
                | "array"
                | "object"
                | "function_expression"
                | "generator_function"
                | "arrow_function"
                | "class"
                | "regex"
        ) {
            binding = BindingKind::Unknown;
        }
    }
    if child(node, "decorator").is_some() {
        binding = BindingKind::Unknown;
    }
    insert_binding(unit, local.clone(), node.start_byte(), binding, work, check)?;
    if exported {
        insert_export(
            unit,
            if default {
                "default".into()
            } else {
                local.clone()
            },
            Export::Local(local),
            work,
            check,
        )?;
    }
    Ok(())
}
fn pattern_names(
    doc: &DocumentInput,
    pattern: Node<'_>,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut pending = vec![(pattern, 0)];
    let mut visits = 0;
    while let Some((node, depth)) = pending.pop() {
        check()?;
        visits += 1;
        ensure!(
            visits <= VISITS && depth <= DEPTH,
            "bounded_work: reference AST"
        );
        if matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) {
            if let Some(name) = name(doc, node) {
                charge(name.len() + 1, work, check)?;
                names.push(name);
            }
        } else if matches!(
            node.kind(),
            "pair_pattern" | "assignment_pattern" | "object_assignment_pattern"
        ) {
            if let Some(value) = node
                .child_by_field_name("value")
                .or_else(|| node.child_by_field_name("left"))
            {
                pending.push((value, depth + 1));
            }
        } else if !matches!(
            node.kind(),
            "string"
                | "number"
                | "computed_property_name"
                | "member_expression"
                | "subscript_expression"
        ) {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                pending.push((child, depth + 1));
            }
        }
    }
    Ok(names)
}

fn is_function(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
    )
}
fn scope(node: Node<'_>) -> bool {
    is_function(node)
        || matches!(
            node.kind(),
            "program"
                | "statement_block"
                | "catch_clause"
                | "for_statement"
                | "for_in_statement"
                | "class_body"
                | "switch_body"
        )
}
fn enclosing_scope(mut node: Node<'_>, function_only: bool) -> Node<'_> {
    while let Some(parent) = node.parent() {
        node = parent;
        if (function_only && (is_function(node) || node.kind() == "program"))
            || (!function_only && scope(node))
        {
            break;
        }
    }
    node
}
fn shadowed(
    doc: &DocumentInput,
    node: Node<'_>,
    spelling: &str,
    scopes: &BTreeMap<usize, BTreeSet<String>>,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<bool> {
    let mut parent = node.parent();
    while let Some(node) = parent {
        if node.kind() == "program" {
            break;
        }
        if scope(node) {
            charge(spelling.len() + 1, work, check)?;
            if scopes
                .get(&node.id())
                .is_some_and(|names| names.contains(spelling))
            {
                return Ok(true);
            }
        }
        parent = node.parent();
    }
    let _ = doc;
    Ok(false)
}
fn loop_binding_kind<'tree>(node: Node<'tree>) -> Option<&'tree str> {
    let mut cursor = node.walk();
    node.children_by_field_name("kind", &mut cursor)
        .map(|kind| kind.kind())
        .find(|kind| matches!(*kind, "let" | "const" | "var" | "using"))
}
fn record_write(
    doc: &DocumentInput,
    target: Node<'_>,
    scopes: &BTreeMap<usize, BTreeSet<String>>,
    hazards: &mut Hazards,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for name in pattern_names(doc, target, work, check)? {
        if !shadowed(doc, target, &name, scopes, work, check)? {
            charge(name.len() + 1, work, check)?;
            hazards.writes.insert(name);
        }
    }
    Ok(())
}
fn body_hazards(
    doc: &DocumentInput,
    root: Node<'_>,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Hazards> {
    let mut scopes = BTreeMap::<usize, BTreeSet<String>>::new();
    let mut hazards = Hazards::default();
    // Collect whole lexical scopes first: a later inner let/parameter shadows an outer write too.
    walk(root, check, &mut |node, check| {
        if let Some((local, _)) = imported_name(doc, node) {
            charge(local.len() + 1, work, check)?;
            scopes.entry(root.id()).or_default().insert(local);
            return Ok(true);
        }
        let declaration = matches!(
            node.kind(),
            "variable_declarator"
                | "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
        );
        if declaration && let Some(pattern) = node.child_by_field_name("name") {
            let var = node.kind() == "variable_declarator"
                && node
                    .parent()
                    .is_some_and(|p| p.kind() == "variable_declaration");
            let owner = enclosing_scope(node, var);
            let names = pattern_names(doc, pattern, work, check)?;
            let conditional = owner.kind() == "program"
                && node.kind() == "variable_declarator"
                && node
                    .parent()
                    .and_then(|p| p.parent())
                    .is_some_and(|p| !matches!(p.kind(), "program" | "export_statement"));
            for name in names {
                charge(name.len() + 1, work, check)?;
                scopes.entry(owner.id()).or_default().insert(name.clone());
                if conditional {
                    owner_step(work, check)?;
                    hazards.writes.insert(name);
                }
            }
        }
        if node.kind() == "for_in_statement"
            && let Some(kind) = loop_binding_kind(node)
            && let Some(pattern) = node.child_by_field_name("left")
        {
            let owner = if kind == "var" {
                enclosing_scope(node, true)
            } else {
                node
            };
            for name in pattern_names(doc, pattern, work, check)? {
                charge(name.len() + 1, work, check)?;
                scopes.entry(owner.id()).or_default().insert(name.clone());
                if kind == "var" && owner.kind() == "program" {
                    owner_step(work, check)?;
                    hazards.writes.insert(name);
                }
            }
        }
        if is_function(node) {
            if let Some(parameters) = node
                .child_by_field_name("parameters")
                .or_else(|| node.child_by_field_name("parameter"))
            {
                for name in pattern_names(doc, parameters, work, check)? {
                    charge(name.len() + 1, work, check)?;
                    scopes.entry(node.id()).or_default().insert(name);
                }
            }
            if matches!(node.kind(), "function_expression" | "generator_function")
                && let Some(name) = node.child_by_field_name("name").and_then(|n| name(doc, n))
            {
                charge(name.len() + 1, work, check)?;
                scopes.entry(node.id()).or_default().insert(name);
            }
        }
        if node.kind() == "catch_clause"
            && let Some(pattern) = node.child_by_field_name("parameter")
        {
            for name in pattern_names(doc, pattern, work, check)? {
                charge(name.len() + 1, work, check)?;
                scopes.entry(node.id()).or_default().insert(name);
            }
        }
        Ok(matches!(
            node.kind(),
            "comment" | "string" | "string_fragment"
        ))
    })?;
    walk(root, check, &mut |node, check| {
        // Import/export names are binding syntax, not CommonJS expression uses.
        if matches!(
            node.kind(),
            "import_statement" | "export_clause" | "namespace_export"
        ) {
            return Ok(true);
        }
        if (matches!(
            node.kind(),
            "assignment_expression" | "augmented_assignment_expression" | "update_expression"
        ) || (node.kind() == "for_in_statement" && loop_binding_kind(node).is_none()))
            && let Some(target) = node
                .child_by_field_name("left")
                .or_else(|| node.child_by_field_name("argument"))
        {
            record_write(doc, target, &scopes, &mut hazards, work, check)?;
        }
        if node.kind() == "identifier"
            && let Some(name) = name(doc, node)
            && matches!(name.as_str(), "exports" | "module")
            && !scopes
                .get(&root.id())
                .is_some_and(|names| names.contains(&name))
            && !shadowed(doc, node, &name, &scopes, work, check)?
        {
            owner_step(work, check)?;
            hazards.dynamic = true;
        }
        if node.kind() == "call_expression"
            && let Some(function) = node.child_by_field_name("function")
            && function.kind() == "identifier"
            && let Some(name) = name(doc, function)
            && matches!(name.as_str(), "require" | "eval")
            && !scopes
                .get(&root.id())
                .is_some_and(|names| names.contains(&name))
            && !shadowed(doc, node, &name, &scopes, work, check)?
        {
            owner_step(work, check)?;
            hazards.dynamic = true;
        }
        Ok(matches!(
            node.kind(),
            "comment" | "string" | "string_fragment"
        ))
    })?;
    Ok(hazards)
}
pub(super) fn extract(
    doc: &DocumentInput,
    tree: &Tree,
    work: &mut usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<RawSite>> {
    let mut result = Vec::new();
    walk(tree.root_node(), check, &mut |node, check| {
        if matches!(node.kind(), "import_statement" | "export_statement")
            && let Some(source) = node.child_by_field_name("source")
        {
            let site = if star(node) { node } else { source };
            charge(site.end_byte() - site.start_byte() + 1, work, check)?;
            ensure!(result.len() < SITES, "bounded_work: reference site count");
            result.push(raw(doc, site, "import", Err("javascript_import_unproved")));
            return Ok(true);
        }
        if node.kind() == "call_expression"
            && node
                .child_by_field_name("function")
                .is_some_and(|n| matches!(text(doc, n), "import" | "require"))
        {
            charge(node.end_byte() - node.start_byte() + 1, work, check)?;
            ensure!(result.len() < SITES, "bounded_work: reference site count");
            result.push(raw(doc, node, "import", Err("dynamic_import_unsupported")));
            return Ok(true);
        }
        Ok(false)
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
    fn expansion(test: impl FnOnce(&mut Resolver<'_>, usize, &mut usize)) {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in [
            ("use.js", "export * from './provider.js';"),
            (
                "provider.js",
                "export function value() {} export function zebra() {}",
            ),
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
        let documents: Vec<_> = graph
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
                    declarations: native
                        .declarations
                        .iter()
                        .filter(|row| row.document == key)
                        .cloned()
                        .collect(),
                    calls: Vec::new(),
                    key,
                    file,
                }
            })
            .collect();
        let docs: Vec<_> = documents.iter().collect();
        let trees: Vec<_> = docs
            .iter()
            .map(|doc| parse(doc, &mut || Ok(())).unwrap())
            .collect();
        let document_map: BTreeMap<_, _> = docs
            .iter()
            .enumerate()
            .map(|(i, doc)| (doc.key.clone(), i))
            .collect();
        let provider = docs
            .iter()
            .position(|doc| doc.key.path == "provider.js")
            .unwrap();
        let mut resolver = Resolver {
            docs: &docs,
            trees: &trees,
            documents: &document_map,
            units: BTreeMap::new(),
            hazards: BTreeMap::new(),
            memo: BTreeMap::new(),
            named_memo: BTreeMap::new(),
        };
        let mut work = 0;
        for (document, doc) in docs.iter().enumerate() {
            let mut native = BTreeMap::<_, Vec<&Declaration>>::new();
            for row in &doc.declarations {
                native
                    .entry((row.range.start, row.range.end, row.kind.as_str()))
                    .or_default()
                    .push(row);
            }
            let unit = inventory(
                doc,
                trees[document].root_node(),
                document,
                &native,
                &mut work,
                &mut || Ok(()),
            )
            .unwrap();
            resolver.units.insert(document, Arc::new(unit));
        }
        test(&mut resolver, provider, &mut work);
    }
    #[test]
    fn immutable_export_memo_hit_and_route_depth_separation() {
        expansion(|resolver, provider, work| {
            let first = resolver
                .exports(provider, 1, &Route::new(), work, &mut || Ok(()))
                .unwrap();
            let hit = resolver
                .exports(provider, 1, &Route::new(), work, &mut || Ok(()))
                .unwrap();
            assert!(Arc::ptr_eq(&first, &hit));
            assert_eq!(first.names["value"].candidates.len(), 1);
            let route = BTreeSet::from([(provider, "*".into())]);
            assert!(
                resolver
                    .exports(provider, 1, &route, work, &mut || Ok(()))
                    .unwrap()
                    .unknown_set
            );
            assert!(
                resolver
                    .exports(provider, FOLLOW + 1, &Route::new(), work, &mut || Ok(()))
                    .unwrap()
                    .unknown_set
            );
        });
    }
    fn first_export_prefix(
        resolver: &mut Resolver<'_>,
        provider: usize,
        work: &mut usize,
    ) -> (usize, usize) {
        resolver.hazards(provider, work, &mut || Ok(())).unwrap();
        // The same helpers measure one successful native candidate before the next
        // name. Hazards are warm, so neither refusal can stop in the body walk.
        let active = BTreeSet::from([(provider, "*".into())]);
        let first_name = resolver.units[&provider]
            .explicit
            .keys()
            .next()
            .unwrap()
            .clone();
        assert_eq!(resolver.units[&provider].explicit.len(), 2);
        let before = *work;
        let mut checks = 0;
        let mut check = || {
            checks += 1;
            Ok(())
        };
        charge(Route::new().len() + 1, work, &mut check).unwrap();
        resolver.hazards(provider, work, &mut check).unwrap();
        let first = resolver
            .explicit(provider, &first_name, 1, &active, work, &mut check)
            .unwrap();
        assert_eq!(first.candidates.len(), 1);
        charge_bucket(&first, work, &mut check).unwrap();
        assert!(resolver.memo.is_empty());
        assert!(resolver.named_memo.is_empty());
        (*work - before, checks)
    }
    #[test]
    fn actual_expansion_uses_accumulated_budget_and_cancellation() {
        expansion(|resolver, provider, work| {
            assert!(*work > 0);
            let (prefix_work, _) = first_export_prefix(resolver, provider, work);
            assert!(prefix_work > 0);
            *work = OWNER_WORK - prefix_work;
            let before = *work;
            let error = resolver
                .exports(provider, 1, &Route::new(), work, &mut || Ok(()))
                .err()
                .unwrap();
            assert!(error.to_string().starts_with("bounded_work:"));
            assert!(*work - before > prefix_work);
            assert!(*work > OWNER_WORK);
            assert!(resolver.memo.is_empty());
            assert!(resolver.named_memo.is_empty());
        });
        expansion(|resolver, provider, work| {
            let (_, prefix_checks) = first_export_prefix(resolver, provider, work);
            let mut checks = 0;
            let before = *work;
            let error = resolver
                .exports(provider, 1, &Route::new(), work, &mut || {
                    checks += 1;
                    ensure!(checks <= prefix_checks, "cancelled");
                    Ok(())
                })
                .err()
                .unwrap();
            assert_eq!(error.to_string(), "cancelled");
            assert_eq!(checks, prefix_checks + 1);
            assert!(*work > before);
            assert!(resolver.memo.is_empty());
            assert!(resolver.named_memo.is_empty());
        });
    }
}
