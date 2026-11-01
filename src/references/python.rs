//! Bounded captured Python module/name proofs, not interpreter or call bindings.
use super::*;

type ModuleKey = (String, Vec<String>);
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
    declaration: DeclarationId,
    proof: BTreeSet<usize>,
}
#[derive(Clone, Default)]
struct Bucket {
    candidates: BTreeMap<DeclarationId, Candidate>,
    unknown: bool,
}
#[derive(Clone, Default)]
struct Names {
    names: BTreeMap<String, Bucket>,
    unknown_set: bool,
}
#[derive(Clone)]
struct Layout {
    document: usize,
    package: bool,
    proof: BTreeSet<usize>,
}
#[derive(Clone)]
struct Import {
    start: usize,
    end: usize,
    path: Vec<String>,
    dots: usize,
    name: Option<String>,
    alias: Option<String>,
    from: bool,
    star: bool,
    module_scope: bool,
    uncertain: bool,
}
#[derive(Clone)]
enum BindingKind {
    Direct(DeclarationId),
    Imported(usize),
    Alias(String),
    Unknown,
}
#[derive(Clone)]
struct Binding {
    start: usize,
    kind: BindingKind,
}
#[derive(Default)]
enum All {
    #[default]
    Absent,
    Literal(BTreeSet<String>),
    Unknown,
}
#[derive(Default)]
struct Unit {
    imports: Vec<Import>,
    bindings: BTreeMap<String, Vec<Binding>>,
    all: All,
    dynamic: bool,
}
pub(super) struct Catalog {
    units: BTreeMap<usize, Unit>,
    source_sets: BTreeMap<usize, String>,
    modules: BTreeMap<ModuleKey, Vec<(usize, bool)>>,
    membership: BTreeMap<usize, Vec<String>>,
    module_proofs: BTreeMap<usize, BTreeSet<usize>>,
    child_names: BTreeMap<usize, BTreeSet<String>>,
    roots: BTreeSet<(String, String)>,
    file_declarations: BTreeMap<usize, DeclarationId>,
    test_modules: BTreeSet<usize>,
    results: BTreeMap<(usize, usize, usize), SiteResolution>,
    export_memo: std::cell::RefCell<BTreeMap<(usize, usize, Route), std::sync::Arc<Names>>>,
}

impl Catalog {
    pub(super) fn new(
        docs: &[&DocumentInput],
        trees: &[Tree],
        _documents: &BTreeMap<DocumentKey, usize>,
        selected: &[usize],
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut result = Self {
            units: BTreeMap::new(),
            source_sets: BTreeMap::new(),
            modules: BTreeMap::new(),
            membership: BTreeMap::new(),
            module_proofs: BTreeMap::new(),
            child_names: BTreeMap::new(),
            roots: BTreeSet::new(),
            file_declarations: BTreeMap::new(),
            test_modules: BTreeSet::new(),
            results: BTreeMap::new(),
            export_memo: std::cell::RefCell::new(BTreeMap::new()),
        };
        for (document, doc) in docs.iter().enumerate() {
            if doc.key.language != "python" {
                continue;
            }
            charge(doc.key.path.len(), work, check)?;
            result
                .source_sets
                .insert(document, doc.key.source_set_id.clone());
            let mut native = BTreeMap::<_, Vec<&Declaration>>::new();
            for declaration in &doc.declarations {
                owner_step(work, check)?;
                if declaration.kind == "module" && declaration.ancestors.is_empty() {
                    result
                        .file_declarations
                        .insert(document, declaration_id(document, declaration));
                    if declaration.role == DeclarationRole::Test {
                        result.test_modules.insert(document);
                    }
                } else {
                    native
                        .entry((declaration.range.start, declaration.range.end))
                        .or_default()
                        .push(declaration);
                }
            }
            if let Some((path, package)) = module_path(&doc.key.path) {
                if let Some(first) = path.first() {
                    owner_step(work, check)?;
                    result
                        .roots
                        .insert((doc.key.source_set_id.clone(), first.clone()));
                }
                charge(path.len(), work, check)?;
                result
                    .modules
                    .entry((doc.key.source_set_id.clone(), path))
                    .or_default()
                    .push((document, package));
            }
            let mut unit = Unit::default();
            scan(
                doc,
                trees[document].root_node(),
                document,
                &native,
                &mut unit,
                selected.binary_search(&document).is_ok(),
                work,
                check,
            )?;
            result.units.insert(document, unit);
        }
        for (&document, doc) in &result.file_declarations {
            owner_step(work, check)?;
            let input = docs[doc.document];
            if let Some((mut path, package)) = module_path(&input.key.path)
                && let Ok(layout) = result.layout(&input.key.source_set_id, &path, work, check)?
            {
                result.module_proofs.insert(document, layout.proof);
                if !package {
                    path.pop();
                }
                result.membership.insert(document, path);
            }
        }
        for (source_set, path) in result.modules.keys() {
            owner_step(work, check)?;
            if path.len() > 1
                && let Ok(parent) =
                    result.layout(source_set, &path[..path.len() - 1], work, check)?
                && parent.package
            {
                owner_step(work, check)?;
                result
                    .child_names
                    .entry(parent.document)
                    .or_default()
                    .insert(path.last().expect("child module name").clone());
            }
        }
        // Catalog work happens before request-source filters. Each unit merges its stars once;
        // resolving an observed site below is an indexed lookup, never a catalog scan.
        result.check_followed_bodies(docs, trees, selected, work, check)?;
        for (&document, unit) in &result.units {
            if !selected.binary_search(&document).is_ok() {
                continue;
            }
            owner_step(work, check)?;
            charge(unit.imports.len(), work, check)?;
            let mut contributors = Vec::new();
            for import in &unit.imports {
                owner_step(work, check)?;
                if import.star && import.module_scope && !import.uncertain {
                    contributors.push(result.import_exports(
                        document,
                        import,
                        1,
                        &mut Route::new(),
                        work,
                        check,
                    )?);
                } else if import.star {
                    contributors.push(std::sync::Arc::new(Names {
                        unknown_set: true,
                        ..Names::default()
                    }));
                }
            }
            let mut merged = Names::default();
            for (import, names) in unit.imports.iter().filter(|i| i.star).zip(&contributors) {
                if import.module_scope {
                    merge_names(&mut merged, names, work, check)?;
                }
            }
            let mut star_index = 0;
            for import in &unit.imports {
                owner_step(work, check)?;
                let mut resolution = if import.star {
                    let own = &contributors[star_index];
                    star_index += 1;
                    let mut resolution = SiteResolution::default();
                    if own.unknown_set {
                        resolution.residual = Some("python_export_name_set_unknown");
                    }
                    let mut shadowed = false;
                    for name in own.names.keys() {
                        owner_step(work, check)?;
                        if unit.bindings.contains_key(name) {
                            shadowed = true;
                            let binding = result.binding(
                                document,
                                name,
                                1,
                                &mut Route::new(),
                                work,
                                check,
                            )?;
                            if binding.unknown || binding.candidates.len() != 1 {
                                resolution.residual = Some("python_explicit_name_unproved");
                            }
                            continue;
                        }
                        let bucket = &merged.names[name];
                        if unit.dynamic
                            || merged.unknown_set
                            || bucket.unknown
                            || bucket.candidates.len() != 1
                        {
                            resolution.residual = Some("python_wildcard_name_ambiguous");
                            continue;
                        }
                        add_bucket(&mut resolution, bucket, work, check)?;
                    }
                    if resolution.targets.is_empty() && resolution.residual.is_none() {
                        resolution.residual = Some(if shadowed {
                            "python_wildcard_fully_shadowed"
                        } else {
                            "python_wildcard_empty"
                        });
                    }
                    resolution
                } else {
                    result.explicit(document, import, work, check)?
                };
                if import.star
                    && let Some(package) = result.external(document, import, work, check)?
                {
                    owner_step(work, check)?;
                    resolution
                        .targets
                        .insert(Target::External(package.identity));
                    resolution.residual = Some("python_external_members_unknown");
                }
                owner_step(work, check)?;
                result
                    .results
                    .insert((document, import.start, import.end), resolution);
            }
        }
        Ok(result)
    }

    fn check_followed_bodies(
        &mut self,
        docs: &[&DocumentInput],
        trees: &[Tree],
        selected: &[usize],
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        charge(selected.len(), work, check)?;
        let mut pending: Vec<_> = selected
            .iter()
            .filter(|d| self.units.contains_key(d))
            .map(|d| (*d, 0usize))
            .collect();
        let mut seen = BTreeMap::<usize, usize>::new();
        while let Some((document, depth)) = pending.pop() {
            owner_step(work, check)?;
            if seen.get(&document).is_some_and(|old| *old <= depth) {
                continue;
            }
            seen.insert(document, depth);
            if depth < FOLLOW {
                for import in &self.units[&document].imports {
                    if !import.module_scope {
                        continue;
                    }
                    if let Some(path) = self.path(document, import, work, check)?
                        && let Ok(layout) =
                            self.layout(&self.source_sets[&document], &path, work, check)?
                    {
                        owner_step(work, check)?;
                        pending.push((layout.document, depth + 1));
                    }
                }
            }
        }
        for document in seen.into_keys() {
            let unit = self.units.get_mut(&document).expect("followed Python unit");
            // A hazard past the walk bounds goes unseen, so the file proves nothing.
            unit.dynamic |=
                crate::syntax_bounds::scan(trees[document].root_node(), check)?.is_some();
            // Only followed/selected units need body hazards. Bounded syntax walking is
            // cancellation-aware, but irrelevant AST nodes are not charged as owner work.
            walk(trees[document].root_node(), check, &mut |node, check| {
                if node.kind() == "global_statement" {
                    block_pattern(docs[document], node, unit, node.start_byte(), work, check)?;
                }
                if node.kind() == "call"
                    && let Some(function) = node.child_by_field_name("function")
                {
                    if function.kind() == "identifier"
                        && matches!(text(docs[document], function), "exec" | "eval")
                    {
                        unit.dynamic = true;
                    }
                    if function.kind() == "attribute"
                        && let Some(object) = function.child_by_field_name("object")
                    {
                        if object.kind() == "identifier"
                            && text(docs[document], object) == "__all__"
                        {
                            unit.all = All::Unknown;
                        }
                        if object.kind() == "call"
                            && object.child_by_field_name("function").is_some_and(|f| {
                                matches!(text(docs[document], f), "globals" | "locals")
                            })
                        {
                            unit.dynamic = true;
                        }
                    }
                }
                if matches!(
                    node.kind(),
                    "assignment" | "augmented_assignment" | "delete_statement"
                ) {
                    let target = node.child_by_field_name("left").unwrap_or(node);
                    // A nested write/escape of the captured export list is not proof
                    // that the module's single literal assignment remains complete.
                    walk(target, check, &mut |part, _check| {
                        if part.kind() == "identifier"
                            && text(docs[document], part) == "__all__"
                            && (node.kind() != "assignment"
                                || part.start_byte() != target.start_byte()
                                || part.end_byte() != target.end_byte())
                        {
                            unit.all = All::Unknown;
                        }
                        Ok(matches!(
                            part.kind(),
                            "comment" | "string" | "concatenated_string"
                        ))
                    })?;
                    if node.child_by_field_name("right").is_some_and(|right| {
                        right.kind() == "identifier" && text(docs[document], right) == "__all__"
                    }) {
                        unit.all = All::Unknown;
                    }
                }
                if node.kind() == "subscript"
                    && let Some(value) = node.child_by_field_name("value")
                    && value.kind() == "call"
                    && value
                        .child_by_field_name("function")
                        .is_some_and(|f| matches!(text(docs[document], f), "globals" | "locals"))
                {
                    unit.dynamic = true;
                }
                Ok(matches!(
                    node.kind(),
                    "comment" | "string" | "concatenated_string"
                ))
            })?;
        }
        Ok(())
    }
    fn layout(
        &self,
        source_set: &str,
        path: &[String],
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<std::result::Result<Layout, &'static str>> {
        let mut proof = BTreeSet::new();
        let mut found = None;
        for end in 1..=path.len() {
            charge(end, work, check)?;
            let Some(rows) = self
                .modules
                .get(&(source_set.to_owned(), path[..end].to_vec()))
            else {
                return Ok(Err("module_path_not_indexed"));
            };
            if rows.len() != 1 {
                return Ok(Err("module_path_ambiguous"));
            }
            let (document, package) = rows[0];
            if end < path.len() && !package {
                return Ok(Err("python_package_prefix_unproved"));
            }
            owner_step(work, check)?;
            proof.insert(document);
            found = Some((document, package));
        }
        Ok(match found {
            Some((document, package)) => Ok(Layout {
                document,
                package,
                proof,
            }),
            None => Err("python_package_membership_unproved"),
        })
    }
    fn path(
        &self,
        document: usize,
        import: &Import,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Vec<String>>> {
        charge(import.path.len(), work, check)?;
        if import.dots == 0 {
            return Ok(Some(import.path.clone()));
        }
        let Some(package) = self.membership.get(&document) else {
            return Ok(None);
        };
        if import.dots > package.len() {
            return Ok(None);
        }
        charge(package.len(), work, check)?;
        let mut path = package[..package.len() + 1 - import.dots].to_vec();
        path.extend(import.path.iter().cloned());
        Ok(Some(path))
    }
    fn external(
        &self,
        document: usize,
        import: &Import,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<external::ExternalPackage>> {
        owner_step(work, check)?;
        if import.dots != 0 || import.uncertain {
            return Ok(None);
        }
        let Some(root) = import.path.first() else {
            return Ok(None);
        };
        let source_set = &self.source_sets[&document];
        if self.roots.contains(&(source_set.clone(), root.clone())) {
            return Ok(None);
        }
        charge(root.len() + source_set.len() + 1, work, check)?;
        Ok(external::python_standard_library(source_set, root))
    }
    fn module_candidate(
        &self,
        layout: Layout,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Bucket> {
        owner_step(work, check)?;
        if layout.proof.iter().any(|d| self.test_modules.contains(d)) {
            return Ok(unknown_bucket());
        }
        let Some(declaration) = self.file_declarations.get(&layout.document) else {
            return Ok(unknown_bucket());
        };
        charge(layout.proof.len(), work, check)?;
        Ok(Bucket {
            candidates: BTreeMap::from([(
                declaration.clone(),
                Candidate {
                    declaration: declaration.clone(),
                    proof: layout.proof,
                },
            )]),
            unknown: false,
        })
    }
    fn binding(
        &self,
        document: usize,
        name: &str,
        depth: usize,
        route: &mut Route,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Bucket> {
        owner_step(work, check)?;
        if depth > FOLLOW {
            return Ok(unknown_bucket());
        }
        // Same-module aliases do not spend module-follow edges. Walk their chain
        // iteratively so a long captured chain cannot become Rust stack depth.
        let mut current = name.to_owned();
        let mut entered = Vec::new();
        let resolved = (|| {
            loop {
                owner_step(work, check)?;
                let key = (document, current.clone());
                if !route.insert(key.clone()) {
                    return Ok(unknown_bucket());
                }
                entered.push(key);
                let unit = &self.units[&document];
                if let Some(rows) = unit.bindings.get(&current).filter(|rows| rows.len() == 1)
                    && self.later_star_may_bind(
                        document,
                        &current,
                        rows[0].start,
                        depth,
                        route,
                        work,
                        check,
                    )?
                {
                    return Ok(unknown_bucket());
                }
                let alias = unit
                    .bindings
                    .get(&current)
                    .filter(|rows| rows.len() == 1)
                    .and_then(|rows| {
                        if let BindingKind::Alias(original) = &rows[0].kind {
                            Some((rows[0].start, original))
                        } else {
                            None
                        }
                    });
                if let Some((start, original)) = alias {
                    let prior = unit
                        .bindings
                        .get(original)
                        .is_some_and(|rows| rows.len() == 1 && rows[0].start < start);
                    if !prior {
                        return Ok(unknown_bucket());
                    }
                    charge(original.len(), work, check)?;
                    current = original.clone();
                    continue;
                }
                return self.compute_binding(document, &current, depth, route, work, check);
            }
        })();
        for key in entered {
            route.remove(&key);
        }
        resolved
    }
    #[allow(clippy::too_many_arguments)] // One ordered name check keeps route and shared budget explicit.
    fn later_star_may_bind(
        &self,
        document: usize,
        name: &str,
        start: usize,
        depth: usize,
        route: &mut Route,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        for import in &self.units[&document].imports {
            owner_step(work, check)?;
            if !import.star || !import.module_scope || import.start <= start {
                continue;
            }
            let bucket =
                self.star_binding(document, import, name, depth + 1, route, work, check)?;
            if bucket.unknown || !bucket.candidates.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn export_policy_uncertain(
        &self,
        document: usize,
        depth: usize,
        route: &mut Route,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        let unit = &self.units[&document];
        if unit.dynamic || matches!(unit.all, All::Unknown) {
            return Ok(true);
        }
        let key = (document, "__all__ policy".to_owned());
        owner_step(work, check)?;
        if !route.insert(key.clone()) {
            return Ok(true);
        }
        let start = unit
            .bindings
            .get("__all__")
            .and_then(|rows| rows.first())
            .map_or(0, |row| row.start);
        let uncertain =
            self.later_star_may_bind(document, "__all__", start, depth, route, work, check);
        route.remove(&key);
        uncertain
    }
    fn compute_binding(
        &self,
        document: usize,
        name: &str,
        depth: usize,
        route: &mut Route,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Bucket> {
        let unit = &self.units[&document];
        if unit.dynamic {
            return Ok(unknown_bucket());
        }
        if let Some(bindings) = unit.bindings.get(name) {
            owner_step(work, check)?;
            if bindings.len() != 1 {
                return Ok(unknown_bucket());
            }
            let binding = &bindings[0];
            return match &binding.kind {
                BindingKind::Unknown => Ok(unknown_bucket()),
                BindingKind::Direct(declaration) => {
                    owner_step(work, check)?;
                    Ok(Bucket {
                        candidates: BTreeMap::from([(
                            declaration.clone(),
                            Candidate {
                                declaration: declaration.clone(),
                                proof: BTreeSet::from([document]),
                            },
                        )]),
                        unknown: false,
                    })
                }
                BindingKind::Alias(_) => unreachable!("aliases are followed iteratively"),
                BindingKind::Imported(index) => self.import_binding(
                    document,
                    &unit.imports[*index],
                    depth + 1,
                    route,
                    work,
                    check,
                ),
            };
        }
        let mut result = Bucket::default();
        for import in &unit.imports {
            owner_step(work, check)?;
            if !import.star || !import.module_scope {
                continue;
            }
            let bucket =
                self.star_binding(document, import, name, depth + 1, route, work, check)?;
            merge_bucket(&mut result, &bucket, work, check)?;
        }
        Ok(result)
    }
    #[allow(clippy::too_many_arguments)] // Named export proof keeps route and shared work explicit.
    fn star_binding(
        &self,
        document: usize,
        import: &Import,
        name: &str,
        depth: usize,
        route: &mut Route,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Bucket> {
        owner_step(work, check)?;
        if import.uncertain || depth > FOLLOW {
            return Ok(unknown_bucket());
        }
        let Some(path) = self.path(document, import, work, check)? else {
            return Ok(unknown_bucket());
        };
        let Ok(layout) = self.layout(&self.source_sets[&document], &path, work, check)? else {
            return Ok(unknown_bucket());
        };
        let unit = &self.units[&layout.document];
        if self.export_policy_uncertain(layout.document, depth, route, work, check)?
            || layout.proof.iter().any(|d| self.test_modules.contains(d))
        {
            return Ok(unknown_bucket());
        }
        let selected = match &unit.all {
            All::Literal(names) => names.contains(name),
            All::Absent => !name.starts_with('_'),
            All::Unknown => false,
        };
        if !selected {
            return Ok(Bucket::default());
        }
        if matches!(unit.all, All::Absent)
            && self
                .child_names
                .get(&layout.document)
                .is_some_and(|names| names.contains(name))
        {
            return Ok(unknown_bucket());
        }
        let mut bucket = self.binding(layout.document, name, depth, route, work, check)?;
        if matches!(unit.all, All::Literal(_)) && bucket.candidates.is_empty() {
            bucket.unknown = true;
        }
        extend_proof(&mut bucket, &layout.proof, work, check)?;
        Ok(bucket)
    }
    fn import_binding(
        &self,
        document: usize,
        import: &Import,
        depth: usize,
        route: &mut Route,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Bucket> {
        owner_step(work, check)?;
        if import.uncertain || depth > FOLLOW {
            return Ok(unknown_bucket());
        }
        let Some(path) = self.path(document, import, work, check)? else {
            return Ok(unknown_bucket());
        };
        let source_set = &self.source_sets[&document];
        let Ok(layout) = self.layout(source_set, &path, work, check)? else {
            return Ok(unknown_bucket());
        };
        if !import.from {
            if import.alias.is_some() {
                return self.module_candidate(layout, work, check);
            }
            let Ok(root) = self.layout(source_set, &path[..1], work, check)? else {
                return Ok(unknown_bucket());
            };
            let mut bucket = self.module_candidate(root, work, check)?;
            extend_proof(&mut bucket, &layout.proof, work, check)?;
            return Ok(bucket);
        }
        let Some(spelling) = &import.name else {
            return Ok(unknown_bucket());
        };
        let name_key = crate::native_evidence::lookup("python", spelling)?;
        let name = &name_key;
        let target_unit = &self.units[&layout.document];
        let present = target_unit.bindings.contains_key(name);
        let mut bucket = if present || !target_unit.imports.is_empty() || target_unit.dynamic {
            self.binding(layout.document, name, depth, route, work, check)?
        } else {
            Bucket::default()
        };
        if layout.package {
            charge(path.len() + 1, work, check)?;
            let mut child_path = path.clone();
            child_path.push(name.clone());
            if let Ok(child) = self.layout(source_set, &child_path, work, check)? {
                if !present
                    && !target_unit.dynamic
                    && bucket.candidates.is_empty()
                    && target_unit.imports.iter().all(|i| !i.star)
                {
                    bucket = self.module_candidate(child, work, check)?;
                } else if !bucket.unknown {
                    merge_bucket(
                        &mut bucket,
                        &self.module_candidate(child, work, check)?,
                        work,
                        check,
                    )?;
                }
            }
        }
        if bucket.candidates.is_empty() {
            bucket.unknown = true;
        }
        extend_proof(&mut bucket, &layout.proof, work, check)?;
        Ok(bucket)
    }
    fn import_exports(
        &self,
        document: usize,
        import: &Import,
        depth: usize,
        route: &mut Route,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<std::sync::Arc<Names>> {
        owner_step(work, check)?;
        if import.uncertain || !import.module_scope || depth > FOLLOW {
            return Ok(std::sync::Arc::new(Names {
                unknown_set: true,
                ..Names::default()
            }));
        }
        let Some(path) = self.path(document, import, work, check)? else {
            return Ok(std::sync::Arc::new(Names {
                unknown_set: true,
                ..Names::default()
            }));
        };
        let Ok(layout) = self.layout(&self.source_sets[&document], &path, work, check)? else {
            return Ok(std::sync::Arc::new(Names {
                unknown_set: true,
                ..Names::default()
            }));
        };
        if layout.proof.iter().any(|d| self.test_modules.contains(d)) {
            return Ok(std::sync::Arc::new(Names {
                unknown_set: true,
                ..Names::default()
            }));
        }
        self.exports(layout.document, depth, route, work, check)
    }
    fn exports(
        &self,
        document: usize,
        depth: usize,
        route: &mut Route,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<std::sync::Arc<Names>> {
        owner_step(work, check)?;
        let key = (document, "*".to_owned());
        if depth > FOLLOW || route.contains(&key) {
            return Ok(std::sync::Arc::new(Names {
                unknown_set: true,
                ..Names::default()
            }));
        }
        // The active route is part of this small request-local memo key. A cyclic
        // caller's unknown result must never poison an independent export route.
        charge(route.len() + 1, work, check)?;
        let memo_key = (document, depth, route.clone());
        if let Some(names) = self.export_memo.borrow().get(&memo_key).cloned() {
            return Ok(names);
        }
        route.insert(key.clone());
        let resolved = self.compute_exports(document, depth, route, work, check);
        route.remove(&key);
        let mut names = resolved?;
        if let Some(proof) = self.module_proofs.get(&document) {
            for bucket in names.names.values_mut() {
                extend_proof(bucket, proof, work, check)?;
            }
        }
        charge_names(&names, work, check)?;
        owner_step(work, check)?;
        let names = std::sync::Arc::new(names);
        self.export_memo
            .borrow_mut()
            .insert(memo_key, names.clone());
        Ok(names)
    }
    fn compute_exports(
        &self,
        document: usize,
        depth: usize,
        route: &mut Route,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Names> {
        let unit = &self.units[&document];
        if self.export_policy_uncertain(document, depth, route, work, check)? {
            return Ok(Names {
                unknown_set: true,
                ..Names::default()
            });
        }
        let mut spellings = BTreeSet::new();
        let mut result = Names::default();
        match &unit.all {
            All::Literal(names) => {
                charge(names.len(), work, check)?;
                spellings.extend(names.iter().cloned());
            }
            All::Absent => {
                if let Some(names) = self.child_names.get(&document) {
                    for name in names {
                        owner_step(work, check)?;
                        if !name.starts_with('_') {
                            spellings.insert(name.clone());
                        }
                    }
                }
                for name in unit.bindings.keys() {
                    owner_step(work, check)?;
                    if !name.starts_with('_') {
                        spellings.insert(name.clone());
                    }
                }
                for import in &unit.imports {
                    owner_step(work, check)?;
                    if !import.star || !import.module_scope {
                        continue;
                    }
                    let names =
                        self.import_exports(document, import, depth + 1, route, work, check)?;
                    result.unknown_set |= names.unknown_set;
                    for name in names.names.keys() {
                        owner_step(work, check)?;
                        if !name.starts_with('_') {
                            spellings.insert(name.clone());
                        }
                    }
                }
            }
            All::Unknown => unreachable!(),
        }
        for name in spellings {
            owner_step(work, check)?;
            let mut bucket = if matches!(unit.all, All::Absent)
                && self
                    .child_names
                    .get(&document)
                    .is_some_and(|names| names.contains(&name))
            {
                unknown_bucket()
            } else {
                self.binding(document, &name, depth, route, work, check)?
            };
            if bucket.candidates.is_empty() {
                bucket.unknown = true;
            }
            result.names.insert(name, bucket);
        }
        Ok(result)
    }
    fn explicit(
        &self,
        document: usize,
        import: &Import,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SiteResolution> {
        if import.uncertain {
            return Ok(SiteResolution::unknown("python_conditional_import"));
        }
        let Some(path) = self.path(document, import, work, check)? else {
            return Ok(SiteResolution::unknown(
                "python_package_membership_unproved",
            ));
        };
        match self.layout(&self.source_sets[&document], &path, work, check)? {
            Ok(layout) => {
                owner_step(work, check)?;
                Ok(SiteResolution {
                    targets: BTreeSet::from([Target::Native(layout.document)]),
                    proof: layout.proof,
                    residual: None,
                })
            }
            Err(reason) => {
                if let Some(package) = self.external(document, import, work, check)? {
                    owner_step(work, check)?;
                    Ok(SiteResolution {
                        targets: BTreeSet::from([Target::External(package.identity)]),
                        ..SiteResolution::default()
                    })
                } else {
                    Ok(SiteResolution::unknown(reason))
                }
            }
        }
    }
    pub(super) fn resolve(
        &self,
        document: usize,
        site: &RawSite,
        packages: &mut BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SiteResolution> {
        owner_step(work, check)?;
        let Some(result) = self.results.get(&(document, site.start, site.end)) else {
            return Ok(SiteResolution::unknown("python_import_unproved"));
        };
        let mut resolution = SiteResolution {
            residual: result.residual,
            ..SiteResolution::default()
        };
        for target in &result.targets {
            owner_step(work, check)?;
            if let Target::External(identity) = target {
                owner_step(work, check)?;
                charge(
                    identity.source_set_id.len() + identity.package.len() + 1,
                    work,
                    check,
                )?;
                if let Some(package) =
                    external::python_standard_library(&identity.source_set_id, &identity.package)
                {
                    packages.entry(identity.clone()).or_insert(package);
                }
            }
            resolution.targets.insert(target.clone());
        }
        charge(result.proof.len(), work, check)?;
        resolution.proof.extend(result.proof.iter().copied());
        Ok(resolution)
    }
}

fn declaration_id(document: usize, row: &Declaration) -> DeclarationId {
    DeclarationId {
        document,
        start: row.range.start,
        end: row.range.end,
        sid: row.syntax_id.clone(),
    }
}
fn unknown_bucket() -> Bucket {
    Bucket {
        unknown: true,
        ..Bucket::default()
    }
}
fn charge(count: usize, work: &mut Work, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
    for _ in 0..count {
        owner_step(work, check)?;
    }
    Ok(())
}
fn charge_names(
    names: &Names,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for (name, bucket) in &names.names {
        charge(name.len() + 1, work, check)?;
        for candidate in bucket.candidates.values() {
            charge(
                candidate.proof.len() + candidate.declaration.sid.len() + 1,
                work,
                check,
            )?;
        }
    }
    Ok(())
}
fn merge_bucket(
    into: &mut Bucket,
    other: &Bucket,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    into.unknown |= other.unknown;
    for (id, candidate) in &other.candidates {
        charge(candidate.proof.len() + 1, work, check)?;
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
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    into.unknown_set |= other.unknown_set;
    for (name, bucket) in &other.names {
        owner_step(work, check)?;
        merge_bucket(
            into.names.entry(name.clone()).or_default(),
            bucket,
            work,
            check,
        )?;
    }
    Ok(())
}
fn extend_proof(
    bucket: &mut Bucket,
    proof: &BTreeSet<usize>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for candidate in bucket.candidates.values_mut() {
        charge(proof.len(), work, check)?;
        candidate.proof.extend(proof.iter().copied());
    }
    Ok(())
}
fn add_bucket(
    result: &mut SiteResolution,
    bucket: &Bucket,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for candidate in bucket.candidates.values() {
        charge(candidate.proof.len() + 1, work, check)?;
        result
            .targets
            .insert(Target::Native(candidate.declaration.document));
        result.proof.extend(candidate.proof.iter().copied());
    }
    Ok(())
}
fn module_path(path: &str) -> Option<(Vec<String>, bool)> {
    let package = path.ends_with("/__init__.py");
    let stem = if package {
        path.strip_suffix("/__init__.py")?
    } else {
        path.strip_suffix(".py")?
    };
    if stem == "__init__" {
        return None;
    }
    let parts: Vec<_> = stem.split('/').map(str::to_owned).collect();
    parts
        .iter()
        .all(|p| identifier(p))
        .then_some((parts, package))
}
fn identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
}

fn import_nodes(
    doc: &DocumentInput,
    node: Node<'_>,
    module_scope: bool,
    uncertain: bool,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Import>> {
    let from = node.kind() == "import_from_statement";
    let mut result = Vec::new();
    if from {
        let Some(module) = node.child_by_field_name("module_name") else {
            return Ok(result);
        };
        charge(module.end_byte() - module.start_byte(), work, check)?;
        let spelling = text(doc, module);
        let dots = spelling.bytes().take_while(|b| *b == b'.').count();
        let path: Vec<_> = spelling[dots..]
            .split('.')
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        let mut cursor = node.walk();
        let star = node
            .named_children(&mut cursor)
            .any(|child| child.kind() == "wildcard_import");
        if star {
            owner_step(work, check)?;
            result.push(Import {
                start: module.start_byte(),
                end: module.end_byte(),
                path,
                dots,
                name: None,
                alias: None,
                from,
                star: true,
                module_scope,
                uncertain,
            });
        } else {
            let mut cursor = node.walk();
            for item in node.children_by_field_name("name", &mut cursor) {
                owner_step(work, check)?;
                let (name, alias) = import_name(doc, item);
                charge(
                    name.len() + alias.as_ref().map_or(0, String::len) + path.len(),
                    work,
                    check,
                )?;
                result.push(Import {
                    start: module.start_byte(),
                    end: module.end_byte(),
                    path: path.clone(),
                    dots,
                    name: Some(name),
                    alias,
                    from,
                    star: false,
                    module_scope,
                    uncertain,
                });
            }
        }
    } else {
        let mut cursor = node.walk();
        for item in node.children_by_field_name("name", &mut cursor) {
            owner_step(work, check)?;
            let (name, alias) = import_name(doc, item);
            let source = item.child_by_field_name("name").unwrap_or(item);
            charge(
                name.len() + alias.as_ref().map_or(0, String::len),
                work,
                check,
            )?;
            result.push(Import {
                start: source.start_byte(),
                end: source.end_byte(),
                path: name.split('.').map(str::to_owned).collect(),
                dots: 0,
                name: None,
                alias,
                from: false,
                star: false,
                module_scope,
                uncertain,
            });
        }
    }
    Ok(result)
}
fn import_name(doc: &DocumentInput, node: Node<'_>) -> (String, Option<String>) {
    if node.kind() == "aliased_import" {
        (
            node.child_by_field_name("name")
                .map(|n| text(doc, n))
                .unwrap_or("")
                .to_owned(),
            node.child_by_field_name("alias")
                .map(|n| text(doc, n).to_owned()),
        )
    } else {
        (text(doc, node).to_owned(), None)
    }
}
fn record(
    unit: &mut Unit,
    name: String,
    start: usize,
    kind: BindingKind,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    charge(name.len() + 1, work, check)?;
    let name = crate::native_evidence::lookup("python", &name)?;
    unit.bindings
        .entry(name)
        .or_default()
        .push(Binding { start, kind });
    Ok(())
}
fn native_binding(
    document: usize,
    node: Node<'_>,
    native: &BTreeMap<(usize, usize), Vec<&Declaration>>,
) -> BindingKind {
    let Some(rows) = native
        .get(&(node.start_byte(), node.end_byte()))
        .filter(|rows| rows.len() == 1)
    else {
        return BindingKind::Unknown;
    };
    let expected = match node.kind() {
        "function_definition" => "function",
        "class_definition" => "type",
        "assignment" => "variable",
        _ => return BindingKind::Unknown,
    };
    if rows[0].role == DeclarationRole::Test || rows[0].kind != expected {
        return BindingKind::Unknown;
    }
    BindingKind::Direct(declaration_id(document, rows[0]))
}
#[allow(clippy::too_many_arguments)] // Captured unit inventory shares the caller's work and cancellation.
fn scan(
    doc: &DocumentInput,
    root: Node<'_>,
    document: usize,
    native: &BTreeMap<(usize, usize), Vec<&Declaration>>,
    unit: &mut Unit,
    selected: bool,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut pending = vec![(root, true, false, false, 0usize)];
    let mut visits = 0usize;
    while let Some((node, module_scope, conditional, decorated, depth)) = pending.pop() {
        check()?;
        visits += 1;
        if visits > VISITS || depth > DEPTH {
            continue;
        }
        let kind = node.kind();
        if matches!(kind, "comment" | "string" | "concatenated_string") {
            continue;
        }
        if matches!(kind, "import_statement" | "import_from_statement") {
            let imports = import_nodes(doc, node, module_scope, conditional, work, check)?;
            for import in imports {
                owner_step(work, check)?;
                if module_scope && !import.star {
                    let name = import.alias.clone().or_else(|| {
                        if import.from {
                            import.name.clone()
                        } else {
                            import.path.first().cloned()
                        }
                    });
                    if let Some(name) = name {
                        let index = unit.imports.len();
                        record(
                            unit,
                            name,
                            node.start_byte(),
                            if conditional {
                                BindingKind::Unknown
                            } else {
                                BindingKind::Imported(index)
                            },
                            work,
                            check,
                        )?;
                    }
                }
                unit.imports.push(import);
            }
            continue;
        }
        if kind == "future_import_statement" {
            continue;
        }
        if kind == "type_alias_statement"
            && module_scope
            && let Some(mut left) = node.child_by_field_name("left")
        {
            while left.kind() != "identifier" && left.named_child_count() > 0 {
                check()?;
                left = left.named_child(0).expect("type alias name child");
            }
            if left.kind() == "identifier" {
                record(
                    unit,
                    text(doc, left).to_owned(),
                    node.start_byte(),
                    BindingKind::Unknown,
                    work,
                    check,
                )?;
            }
            continue;
        }
        if matches!(kind, "function_definition" | "class_definition")
            && module_scope
            && let Some(name) = node.child_by_field_name("name")
        {
            let name = text(doc, name).to_owned();
            if name == "__getattr__" {
                unit.dynamic = true;
            }
            record(
                unit,
                name,
                node.start_byte(),
                if conditional || decorated {
                    BindingKind::Unknown
                } else {
                    native_binding(document, node, native)
                },
                work,
                check,
            )?;
        }
        if kind == "assignment"
            && module_scope
            && let Some(left) = node.child_by_field_name("left")
        {
            if left.kind() == "identifier" {
                let name = text(doc, left).to_owned();
                let right = node.child_by_field_name("right");
                if name == "__all__" {
                    let selected = if !conditional && matches!(unit.all, All::Absent) {
                        if let Some(right) = right {
                            literal_all(doc, right, work, check)?
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    unit.all = selected.map(All::Literal).unwrap_or(All::Unknown);
                }
                let binding = match right {
                    Some(right) if !conditional && right.kind() == "identifier" => {
                        BindingKind::Alias(crate::native_evidence::lookup(
                            "python",
                            text(doc, right),
                        )?)
                    }
                    Some(right)
                        if !conditional
                            && !matches!(right.kind(), "assignment" | "augmented_assignment") =>
                    {
                        native_binding(document, node, native)
                    }
                    _ => BindingKind::Unknown,
                };
                record(unit, name, node.start_byte(), binding, work, check)?;
                if right.is_some_and(|right| {
                    right.kind() == "identifier" && text(doc, right) == "__all__"
                }) {
                    unit.all = All::Unknown;
                }
            } else {
                block_pattern(doc, left, unit, node.start_byte(), work, check)?;
            }
        }
        if module_scope
            && matches!(kind, "augmented_assignment" | "named_expression")
            && let Some(left) = node.child_by_field_name(if kind == "named_expression" {
                "name"
            } else {
                "left"
            })
        {
            block_pattern(doc, left, unit, node.start_byte(), work, check)?;
        }
        if module_scope
            && matches!(
                kind,
                "for_statement"
                    | "for_in_clause"
                    | "with_item"
                    | "except_clause"
                    | "as_pattern"
                    | "delete_statement"
                    | "case_pattern"
            )
        {
            if let Some(left) = node
                .child_by_field_name("left")
                .or_else(|| node.child_by_field_name("alias"))
            {
                block_pattern(doc, left, unit, node.start_byte(), work, check)?;
            } else if matches!(kind, "delete_statement" | "as_pattern" | "case_pattern") {
                block_pattern(doc, node, unit, node.start_byte(), work, check)?;
            }
        }
        if module_scope
            && kind == "call"
            && let Some(function) = node.child_by_field_name("function")
        {
            if function.kind() == "identifier" && matches!(text(doc, function), "exec" | "eval") {
                unit.dynamic = true;
            }
            if function.kind() == "attribute"
                && let Some(object) = function.child_by_field_name("object")
            {
                if object.kind() == "identifier" && text(doc, object) == "__all__" {
                    unit.all = All::Unknown;
                }
                if object.kind() == "call"
                    && object
                        .child_by_field_name("function")
                        .is_some_and(|f| matches!(text(doc, f), "globals" | "locals"))
                {
                    unit.dynamic = true;
                }
            }
        }
        let nested = matches!(kind, "function_definition" | "class_definition" | "lambda");
        let controlled = matches!(
            kind,
            "if_statement"
                | "elif_clause"
                | "else_clause"
                | "try_statement"
                | "except_clause"
                | "finally_clause"
                | "for_statement"
                | "while_statement"
                | "with_statement"
                | "match_statement"
                | "case_clause"
        );
        if nested && !selected {
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            pending.push((
                child,
                module_scope && !nested,
                conditional || controlled || kind == "assignment",
                decorated || kind == "decorated_definition",
                depth + 1,
            ));
        }
    }
    Ok(())
}
fn block_pattern(
    doc: &DocumentInput,
    root: Node<'_>,
    unit: &mut Unit,
    start: usize,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        owner_step(work, check)?;
        if node.kind() == "identifier" {
            let name = text(doc, node).to_owned();
            if name == "__all__" {
                unit.all = All::Unknown;
            }
            record(unit, name, start, BindingKind::Unknown, work, check)?;
        } else if node.kind() == "subscript" {
            if let Some(value) = node.child_by_field_name("value") {
                if value.kind() == "call"
                    && value
                        .child_by_field_name("function")
                        .is_some_and(|f| matches!(text(doc, f), "globals" | "locals"))
                {
                    unit.dynamic = true;
                }
                pending.push(value);
            }
        } else if node.kind() == "attribute" {
            if let Some(object) = node.child_by_field_name("object") {
                pending.push(object);
            }
        } else {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                owner_step(work, check)?;
                pending.push(child);
            }
        }
    }
    Ok(())
}
fn literal_all(
    doc: &DocumentInput,
    node: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<BTreeSet<String>>> {
    if node.kind() != "list" {
        return Ok(None);
    }
    let mut names = BTreeSet::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        owner_step(work, check)?;
        if child.kind() == "comment" {
            continue;
        }
        if child.kind() != "string" {
            return Ok(None);
        }
        let Some(value) = literal_string(text(doc, child), work, check)? else {
            return Ok(None);
        };
        owner_step(work, check)?;
        names.insert(value);
    }
    Ok(Some(names))
}
// Named Unicode escapes, bytes/f strings and implicit concatenation remain unknown.
fn literal_string(
    value: &str,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<String>> {
    charge(value.len(), work, check)?;
    let Some(quote) = value.find(['\'', '"']) else {
        return Ok(None);
    };
    let prefix = &value[..quote];
    if !prefix.is_empty() && !prefix.eq_ignore_ascii_case("r") && !prefix.eq_ignore_ascii_case("u")
    {
        return Ok(None);
    }
    let raw = prefix.eq_ignore_ascii_case("r");
    let tail = &value[quote..];
    let delimiter = if tail.starts_with("\"\"\"") || tail.starts_with("'''") {
        3
    } else {
        1
    };
    if tail.len() < delimiter * 2 || tail[..delimiter] != tail[tail.len() - delimiter..] {
        return Ok(None);
    }
    let content = &tail[delimiter..tail.len() - delimiter];
    if raw {
        return Ok(Some(content.to_owned()));
    }
    let mut decoded = String::new();
    let mut chars = content.chars();
    while let Some(ch) = chars.next() {
        owner_step(work, check)?;
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }
        let Some(escape) = chars.next() else {
            return Ok(None);
        };
        let ch = match escape {
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            'a' => '\x07',
            'b' => '\x08',
            'f' => '\x0c',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\x0b',
            '\n' => continue,
            '\r' => {
                if chars.next() != Some('\n') {
                    return Ok(None);
                }
                continue;
            }
            'x' | 'u' | 'U' => {
                let count = match escape {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let mut code = 0u32;
                for _ in 0..count {
                    owner_step(work, check)?;
                    let Some(digit) = chars.next().and_then(|c| c.to_digit(16)) else {
                        return Ok(None);
                    };
                    let Some(next) = code
                        .checked_mul(16)
                        .and_then(|code| code.checked_add(digit))
                    else {
                        return Ok(None);
                    };
                    code = next;
                }
                let Some(ch) = char::from_u32(code) else {
                    return Ok(None);
                };
                ch
            }
            _ => return Ok(None),
        };
        decoded.push(ch);
    }
    Ok(Some(decoded))
}

pub(super) fn extract(
    doc: &DocumentInput,
    tree: &Tree,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<RawSite>> {
    let mut result = Vec::new();
    walk(tree.root_node(), check, &mut |node, check| {
        if matches!(node.kind(), "import_statement" | "import_from_statement") {
            let imports = import_nodes(doc, node, true, false, work, check)?;
            for import in imports {
                owner_step(work, check)?;
                ensure!(result.len() < SITES, "bounded_work: reference site count");
                result.push(RawSite {
                    start: import.start,
                    end: import.end,
                    kind: "import",
                    spelling: doc.file.text[import.start..import.end].to_owned(),
                    destination: Ok(import.path),
                    namespace: None,
                });
            }
            return Ok(true);
        }
        Ok(matches!(
            node.kind(),
            "comment" | "string" | "concatenated_string"
        ))
    })?;
    result.sort_by_key(|site| (site.start, site.end));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};

    fn documents() -> Vec<DocumentInput> {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in [
            ("use.py", "from provider import *\n"),
            ("provider.py", "def public_name():\n    pass\n"),
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
    fn expansion(test: impl FnOnce(&Catalog, usize, &Import, &mut Work)) {
        let documents = documents();
        let docs: Vec<_> = documents.iter().collect();
        let trees: Vec<_> = docs
            .iter()
            .map(|doc| parse(doc, &mut || Ok(())).unwrap())
            .collect();
        let source = docs
            .iter()
            .position(|doc| doc.key.path == "use.py")
            .unwrap();
        let mut work = Work::new(work::reference_limit(docs.len(), 0));
        let catalog = Catalog::new(
            &docs,
            &trees,
            &BTreeMap::new(),
            &[source],
            &mut work,
            &mut || Ok(()),
        )
        .unwrap();
        catalog.export_memo.borrow_mut().clear();
        test(
            &catalog,
            source,
            &catalog.units[&source].imports[0],
            &mut work,
        );
    }
    #[test]
    fn export_memo_hit_shares_the_immutable_result() {
        expansion(|catalog, source, import, work| {
            let path = catalog
                .path(source, import, work, &mut || Ok(()))
                .unwrap()
                .unwrap();
            let layout = catalog
                .layout(&catalog.source_sets[&source], &path, work, &mut || Ok(()))
                .unwrap()
                .unwrap();
            let first = catalog
                .exports(layout.document, 1, &mut Route::new(), work, &mut || Ok(()))
                .unwrap();
            let before = work.used;
            let hit = catalog
                .exports(layout.document, 1, &mut Route::new(), work, &mut || Ok(()))
                .unwrap();
            assert!(std::sync::Arc::ptr_eq(&first, &hit));
            assert_eq!(work.used - before, 2);
        });
    }
    #[test]
    fn wildcard_expansion_uses_accumulated_work_limit() {
        expansion(|catalog, source, import, work| {
            assert!(work.used > 0);
            work.limit = work.used + 2;
            let result =
                catalog.import_exports(source, import, 1, &mut Route::new(), work, &mut || Ok(()));
            assert!(
                result
                    .err()
                    .expect("shared budget must be exhausted")
                    .to_string()
                    .starts_with("bounded_work:")
            );
            assert!(work.used > work.limit);
        });
    }
    #[test]
    fn wildcard_expansion_cancels_before_publication() {
        expansion(|catalog, source, import, work| {
            let mut checks = 0;
            let result =
                catalog.import_exports(source, import, 1, &mut Route::new(), work, &mut || {
                    checks += 1;
                    ensure!(checks < 3, "cancelled");
                    Ok(())
                });
            assert_eq!(
                result.err().expect("expansion must cancel").to_string(),
                "cancelled"
            );
            assert_eq!(checks, 3);
        });
    }
}
