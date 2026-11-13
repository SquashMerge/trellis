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
    /// A TypeScript function declaration or signature: several of one name overload.
    overload: bool,
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
    /// Workspace packages (#773).
    workspaces: &'a js_workspace::Workspaces,
    /// The root config and the source set it belongs to; `Err` is its typed reason.
    tsconfig: Option<(
        String,
        std::result::Result<tsconfig::RootTsconfig, &'static str>,
    )>,
}
impl Catalog {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        docs: &[&DocumentInput],
        trees: &[Tree],
        documents: &BTreeMap<DocumentKey, usize>,
        selected: &[usize],
        packages: &external::JavascriptPackages,
        workspaces: &js_workspace::Workspaces,
        root_tsconfig: Option<&tsconfig::RootTsconfigInput>,
        work: &mut Work,
        request: &parallel_check::RequestCheck<'_>,
        workers: usize,
    ) -> Result<Self> {
        let check = &mut request.worker();
        let tsconfig = match root_tsconfig {
            Some(input) => {
                ensure!(
                    work.charge(input.bytes.len()),
                    "bounded_work: reference tsconfig parsing"
                );
                check()?;
                let parsed = tsconfig::parse(&input.bytes);
                check()?;
                Some((input.source_set_id.clone(), parsed))
            }
            None => None,
        };
        let mut resolver = Resolver {
            docs,
            trees,
            documents,
            units: BTreeMap::new(),
            hazards: BTreeMap::new(),
            memo: BTreeMap::new(),
            named_memo: BTreeMap::new(),
            workspaces,
            tsconfig,
        };
        for (document, doc) in docs.iter().enumerate() {
            if !script_family(&doc.key.language) {
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
        // Selected hazards are independent per document: compute them in batches
        // of `workers`, each against the allowance remaining before its batch, and
        // admit their counts in document order.
        let pending: Vec<usize> = selected
            .iter()
            .copied()
            .filter(|document| resolver.units.contains_key(document))
            .collect();
        for batch in pending.chunks(workers.max(1)) {
            let remaining = work.remaining();
            let computed = map_all(batch, workers, request, |&document| {
                let mut local = Work::new(remaining);
                let hazards = body_hazards(
                    docs[document],
                    trees[document].root_node(),
                    &mut local,
                    &mut request.worker(),
                )?;
                Ok((hazards, local.used))
            })?;
            for (&document, (hazards, used)) in batch.iter().zip(computed) {
                merge_work(work, used, check)?;
                owner_step(work, check)?;
                resolver.hazards.insert(document, hazards);
            }
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
                // A package is the fallback only when no local module matched. An
                // ambiguous local mapping stays ambiguous.
                if source.supported
                    && let Some(specifier) = source.specifier.as_deref()
                    && !relative(specifier)
                    && resolver
                        .target(document, Some(specifier), work, check)?
                        .is_err_and(package_fallback)
                {
                    let key = &docs[document].key;
                    charge(
                        specifier.len()
                            + packages.resolution_work(
                                &key.source_set_id,
                                specifier,
                                &key.language,
                            ),
                        work,
                        check,
                    )?;
                    if let Some(package) =
                        packages.resolve(&key.source_set_id, specifier, &key.language)
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
        work: &mut Work,
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
    fn config(
        &self,
        document: usize,
    ) -> Option<&std::result::Result<tsconfig::RootTsconfig, &'static str>> {
        self.tsconfig
            .as_ref()
            .filter(|(source_set_id, _)| *source_set_id == self.docs[document].key.source_set_id)
            .map(|(_, config)| config)
    }
    fn target(
        &self,
        document: usize,
        specifier: Option<&str>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<std::result::Result<usize, &'static str>> {
        owner_step(work, check)?;
        let Some(specifier) = specifier else {
            return Ok(Err("import_escape_unsupported"));
        };
        if !relative(specifier) {
            return self.bare_target(document, specifier, work, check);
        }
        charge(
            self.docs[document].key.path.len() + specifier.len(),
            work,
            check,
        )?;
        let Some(path) = relative_path(&self.docs[document].key.path, specifier) else {
            return Ok(Err("relative_import_outside_source"));
        };
        let directory = specifier.ends_with('/');
        let mut found = self.lookup(document, &path, directory, work, check)?;
        if found.is_empty()
            && let Some(Ok(config)) = self.config(document)
        {
            found = self.root_dirs(document, config, &path, directory, work, check)?;
        }
        Ok(outcome(&found))
    }
    /// A non-relative specifier: `paths`, then `baseUrl`, of a readable root config, then a
    /// workspace package (#773). Packages and the standard library are the caller's, after
    /// this falls through.
    fn bare_target(
        &self,
        document: usize,
        specifier: &str,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<std::result::Result<usize, &'static str>> {
        if specifier.starts_with('/') {
            return Ok(Err(BARE_UNRESOLVED));
        }
        let configured = self.config_target(document, specifier, work, check)?;
        if !configured.is_err_and(|reason| reason != "module_path_ambiguous") {
            return Ok(configured);
        }
        Ok(
            match self.workspaces.lookup(
                &self.docs[document].key.source_set_id,
                specifier,
                work,
                check,
            )? {
                None => configured,
                Some(js_workspace::Lookup::Unknown(reason)) => Err(reason),
                Some(js_workspace::Lookup::Path(path, directory)) => {
                    let found = self.lookup(document, &path, directory, work, check)?;
                    match outcome(&found) {
                        Err("module_path_not_indexed") => Err(js_workspace::NOT_INDEXED),
                        other => other,
                    }
                }
            },
        )
    }
    /// `paths`, then `baseUrl`, of a readable root config.
    fn config_target(
        &self,
        document: usize,
        specifier: &str,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<std::result::Result<usize, &'static str>> {
        const UNRESOLVED: &str = BARE_UNRESOLVED;
        let config = match self.config(document) {
            None => return Ok(Err(UNRESOLVED)),
            Some(Err(reason)) => return Ok(Err(reason)),
            Some(Ok(config)) => config,
        };
        let base = match config.base_url.as_deref() {
            Some(base_url) => tsconfig::normalize(base_url),
            None => Some(String::new()),
        };
        let Some(base) = base else {
            return Ok(Err(UNRESOLVED));
        };
        charge(config.paths.len() + specifier.len() + 1, work, check)?;
        let mut chosen = config
            .paths
            .get_key_value(specifier)
            .map(|(_, targets)| (targets, None));
        if chosen.is_none() {
            let mut best: Option<(usize, &Vec<String>, &str)> = None;
            for (key, targets) in &config.paths {
                let Some((prefix, suffix)) = key.split_once('*') else {
                    continue;
                };
                if !suffix.contains('*')
                    && specifier.len() >= prefix.len() + suffix.len()
                    && specifier.starts_with(prefix)
                    && specifier.ends_with(suffix)
                    && best.is_none_or(|(longest, _, _)| prefix.len() > longest)
                {
                    best = Some((
                        prefix.len(),
                        targets,
                        &specifier[prefix.len()..specifier.len() - suffix.len()],
                    ));
                }
            }
            chosen = best.map(|(_, targets, star)| (targets, Some(star)));
        }
        if let Some((targets, star)) = chosen {
            for target in targets {
                owner_step(work, check)?;
                let target = match star {
                    Some(star) => target.replacen('*', star, 1),
                    None => target.clone(),
                };
                charge(base.len() + target.len() + 1, work, check)?;
                let Some(path) = tsconfig::normalize(&join(&base, &target)) else {
                    continue;
                };
                let found = self.lookup(document, &path, target.ends_with('/'), work, check)?;
                if !found.is_empty() {
                    return Ok(outcome(&found));
                }
            }
        }
        if config.base_url.is_some() {
            charge(base.len() + specifier.len() + 1, work, check)?;
            if let Some(path) = tsconfig::normalize(&join(&base, specifier)) {
                let found = self.lookup(document, &path, specifier.ends_with('/'), work, check)?;
                if !found.is_empty() {
                    return Ok(outcome(&found));
                }
            }
        }
        Ok(Err(UNRESOLVED))
    }
    /// The same relative path under the other `rootDirs`, in config order.
    fn root_dirs(
        &self,
        document: usize,
        config: &tsconfig::RootTsconfig,
        path: &str,
        directory: bool,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Vec<usize>> {
        let importer = &self.docs[document].key.path;
        let importer = importer.rsplit_once('/').map_or("", |(dir, _)| dir);
        charge(config.root_dirs.len() + path.len() + 1, work, check)?;
        let dirs: Vec<Option<String>> = config
            .root_dirs
            .iter()
            .map(|dir| tsconfig::normalize(dir))
            .collect();
        let contains = |dir: &str, path: &str| {
            dir.is_empty()
                || path == dir
                || path
                    .strip_prefix(dir)
                    .is_some_and(|rest| rest.starts_with('/'))
        };
        let Some((own, root)) = dirs
            .iter()
            .enumerate()
            .filter_map(|(index, dir)| Some((index, dir.as_deref()?)))
            .filter(|(_, dir)| contains(dir, importer) && contains(dir, path))
            .max_by_key(|(_, dir)| dir.len())
        else {
            return Ok(Vec::new());
        };
        let rest = path
            .strip_prefix(root)
            .unwrap_or(path)
            .trim_start_matches('/');
        for (index, other) in dirs.iter().enumerate() {
            let Some(other) = other else {
                continue;
            };
            if index == own {
                continue;
            }
            charge(other.len() + rest.len() + 1, work, check)?;
            let joined = join(other, rest);
            let found = self.lookup(document, &joined, directory, work, check)?;
            if !found.is_empty() {
                return Ok(found);
            }
        }
        Ok(Vec::new())
    }
    fn lookup(
        &self,
        document: usize,
        path: &str,
        directory: bool,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Vec<usize>> {
        let candidates = candidates(path, directory);
        charge(
            path.len() * candidates.len() + candidates.len(),
            work,
            check,
        )?;
        Ok(find_script(
            self.docs[document],
            &candidates,
            self.documents,
        ))
    }
    fn source_exports(
        &mut self,
        document: usize,
        source: &Source,
        depth: usize,
        route: &Route,
        work: &mut Work,
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
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Arc<Names>> {
        charge(route.len() + 1, work, check)?;
        if depth > FOLLOW
            || route.contains(&(document, "*".into()))
            || (depth > 0
                && (self.units[&document].test_only || commonjs(&self.docs[document].key.path)))
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
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Arc<Bucket>> {
        charge(route.len() + name.len() + 1, work, check)?;
        if depth > FOLLOW
            || route.contains(&(document, name.to_owned()))
            || (depth > 0
                && (self.units[&document].test_only || commonjs(&self.docs[document].key.path)))
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
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Bucket> {
        owner_step(work, check)?;
        let unit = Arc::clone(&self.units[&document]);
        let Some(exports) = unit.explicit.get(name) else {
            return Ok(unknown_bucket());
        };
        // TypeScript overloads export one local name once per signature.
        let overloads = self.docs[document].key.language == "typescript"
            && matches!(exports.as_slice(), [Export::Local(first), rest @ ..]
                if rest.iter().all(|export| matches!(export, Export::Local(local) if local == first)));
        if exports.len() != 1 && !overloads {
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
        work: &mut Work,
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
            if bindings.len() > 1
                && bindings
                    .iter()
                    .all(|binding| binding.overload && binding.start < before)
            {
                // The overloads of one name stand for one declaration: the first.
                let mut first: Option<&Binding> = None;
                for binding in bindings {
                    if !matches!(binding.kind, BindingKind::Direct(_)) {
                        return Ok(unknown_bucket());
                    }
                    if first.is_none_or(|first| binding.start < first.start) {
                        first = Some(binding);
                    }
                }
                let Some(Binding {
                    kind: BindingKind::Direct(id),
                    ..
                }) = first
                else {
                    return Ok(unknown_bucket());
                };
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
        work: &mut Work,
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

fn charge(count: usize, work: &mut Work, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
    for _ in 0..count {
        owner_step(work, check)?;
    }
    Ok(())
}
fn charge_package(
    package: &external::ExternalPackage,
    work: &mut Work,
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
    work: &mut Work,
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
    work: &mut Work,
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
    work: &mut Work,
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
    work: &mut Work,
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
    work: &mut Work,
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
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for candidate in bucket.candidates.values() {
        charge(candidate.proof.len() + 1, work, check)?;
        result.targets.insert(Target::Native(candidate.id.document));
        result.proof.extend(candidate.proof.iter().copied());
    }
    Ok(())
}
const BARE_UNRESOLVED: &str = "absolute_or_bare_import";
/// Whether a bare specifier's failure lets an npm or Node label stand in: not when a local
/// mapping was ambiguous, nor when a workspace member claims the name (#773).
fn package_fallback(reason: &str) -> bool {
    reason != "module_path_ambiguous"
        && (!reason.starts_with("javascript_workspace_") || reason == js_workspace::UNREADABLE)
}
fn relative(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../")
}
fn join(dir: &str, path: &str) -> String {
    if dir.is_empty() {
        path.to_owned()
    } else if path.is_empty() {
        dir.to_owned()
    } else {
        format!("{dir}/{path}")
    }
}
/// `.cjs` and `.cts` are CommonJS: export following ends there.
fn commonjs(path: &str) -> bool {
    path.ends_with(".cjs") || path.ends_with(".cts")
}
fn outcome(found: &[usize]) -> std::result::Result<usize, &'static str> {
    match found {
        [target] => Ok(*target),
        [] => Err("module_path_not_indexed"),
        _ => Err("module_path_ambiguous"),
    }
}
/// The paths an import specifier may name. `path` is the root-relative specifier without
/// its trailing slash; `directory` is whether the specifier had one.
fn candidates(path: &str, directory: bool) -> Vec<String> {
    let index = |dir: &str| {
        let stem = if dir.is_empty() {
            "index".to_owned()
        } else {
            format!("{dir}/index")
        };
        ["ts", "tsx", "d.ts", "js"].map(|extension| format!("{stem}.{extension}"))
    };
    if directory {
        return index(path).into();
    }
    let with = |stem: &str, extensions: &[&str]| -> Vec<String> {
        extensions
            .iter()
            .map(|extension| format!("{stem}.{extension}"))
            .collect()
    };
    if [".ts", ".tsx", ".mts", ".cts"]
        .iter()
        .any(|extension| path.ends_with(extension))
    {
        return vec![path.to_owned()];
    }
    for (suffix, extensions) in [
        (".js", &["js", "ts", "tsx", "d.ts"][..]),
        (".mjs", &["mjs", "mts", "d.mts"]),
        (".cjs", &["cjs", "cts", "d.cts"]),
        (".jsx", &["tsx"]),
    ] {
        if let Some(stem) = path.strip_suffix(suffix) {
            return with(stem, extensions);
        }
    }
    let mut result = with(path, &["ts", "tsx", "d.ts", "js", "mjs", "cjs"]);
    result.extend(index(path));
    result
}
/// The documents the candidates name, from the first non-empty precedence class:
/// implementations in the importer's language, implementations in the other script
/// language, then declaration-only documents.
fn find_script(
    doc: &DocumentInput,
    candidates: &[String],
    documents: &BTreeMap<DocumentKey, usize>,
) -> Vec<usize> {
    let mut classes: [Vec<usize>; 3] = Default::default();
    for path in candidates {
        let Some(language) = crate::capture::language_of_path(std::path::Path::new(path))
            .filter(|language| script_family(language))
        else {
            continue;
        };
        let key = DocumentKey {
            source_set_id: doc.key.source_set_id.clone(),
            language: language.to_owned(),
            path: path.clone(),
        };
        let Some(&index) = documents.get(&key) else {
            continue;
        };
        let class = if crate::indexer_ts::declaration_only(path) {
            2
        } else if language == doc.key.language {
            0
        } else {
            1
        };
        classes[class].push(index);
    }
    classes
        .into_iter()
        .find(|class| !class.is_empty())
        .unwrap_or_default()
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
        "identifier" | "type_identifier" | "shorthand_property_identifier_pattern" => {
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
/// The module specifier of an import or export, including `import x = require("m")`.
fn source_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("source").or_else(|| {
        child(node, "import_require_clause").and_then(|clause| clause.child_by_field_name("source"))
    })
}
fn source(doc: &DocumentInput, node: Node<'_>) -> Option<Source> {
    let source = source_node(node)?;
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
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    charge(name.len() + 1, work, check)?;
    unit.bindings.entry(name).or_default().push(Binding {
        start,
        kind,
        overload: false,
    });
    Ok(())
}
fn insert_export(
    unit: &mut Unit,
    name: String,
    export: Export,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    charge(name.len() + 1, work, check)?;
    unit.explicit.entry(name).or_default().push(export);
    Ok(())
}
fn imported_name(doc: &DocumentInput, node: Node<'_>) -> Option<(String, Option<String>)> {
    match node.kind() {
        "identifier" if node.parent().is_some_and(|p| p.kind() == "import_clause") => {
            imported_name_in_clause(doc, node)
        }
        "import_specifier" => node.child_by_field_name("name").and_then(|original| {
            name(doc, node.child_by_field_name("alias").unwrap_or(original))
                .map(|local| (local, name(doc, original)))
        }),
        "import_require_clause" => child(node, "identifier")
            .and_then(|n| name(doc, n))
            .map(|local| (local, None)),
        "namespace_import" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find_map(|n| name(doc, n))
                .map(|local| (local, None))
        }
        _ => None,
    }
}
/// `imported_name` for a node already known to be a direct child of an import clause.
fn imported_name_in_clause(
    doc: &DocumentInput,
    node: Node<'_>,
) -> Option<(String, Option<String>)> {
    match node.kind() {
        "identifier" => name(doc, node).map(|local| (local, Some("default".into()))),
        _ => None,
    }
}
fn inventory(
    doc: &DocumentInput,
    root: Node<'_>,
    document: usize,
    native: &BTreeMap<(usize, usize, &str), Vec<&Declaration>>,
    work: &mut Work,
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
            if let Some(clause) = child(statement, "import_clause")
                .or_else(|| child(statement, "import_require_clause"))
            {
                let specifier = source_node(statement).and_then(|n| literal(doc, n));
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
        let assignment = {
            let mut cursor = statement.walk();
            statement.children(&mut cursor).any(|n| n.kind() == "=")
        };
        if assignment {
            // `export = x` replaces the module's whole export surface.
            unit.unknown_set = true;
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
    work: &mut Work,
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
    } else if declaration.kind() == "ambient_declaration" {
        let mut cursor = declaration.walk();
        for inner in declaration.named_children(&mut cursor) {
            declarations(
                doc, inner, document, native, unit, exported, default, work, check,
            )?;
        }
    } else if matches!(declaration.kind(), "internal_module" | "module") {
        match declaration.child_by_field_name("name") {
            Some(name) if name.kind() == "identifier" => record_declaration(
                doc,
                declaration,
                document,
                native,
                unit,
                exported,
                default,
                work,
                check,
            )?,
            // A dotted or string-named namespace exports a name this inventory does not read.
            _ => unit.unknown_set |= exported,
        }
    } else if matches!(
        declaration.kind(),
        "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "function_expression"
            | "generator_function"
            | "class"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "type_alias_declaration"
            | "function_signature"
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
    work: &mut Work,
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
        "class_declaration"
        | "class"
        | "abstract_class_declaration"
        | "interface_declaration"
        | "enum_declaration" => "type",
        "type_alias_declaration" => "alias",
        "internal_module" | "module" => "namespace",
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
    if doc.key.language == "typescript"
        && matches!(node.kind(), "function_declaration" | "function_signature")
        && let Some(last) = unit
            .bindings
            .get_mut(&local)
            .and_then(|bindings| bindings.last_mut())
    {
        last.overload = true;
    }
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
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut pending = vec![(pattern, 0)];
    let mut visits = 0;
    while let Some((node, depth)) = pending.pop() {
        check()?;
        visits += 1;
        if visits > VISITS || depth > DEPTH {
            continue;
        }
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
        } else if matches!(node.kind(), "required_parameter" | "optional_parameter") {
            // A TypeScript parameter binds only its pattern: its type, default value
            // and decorators bind nothing.
            if let Some(pattern) = node.child_by_field_name("pattern") {
                pending.push((pattern, depth + 1));
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
    work: &mut Work,
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
    work: &mut Work,
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
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Hazards> {
    let mut scopes = BTreeMap::<usize, BTreeSet<String>>::new();
    let mut hazards = Hazards::default();
    // Collect whole lexical scopes first: a later inner let/parameter shadows an outer write too.
    walk(root, check, &mut |node, check| {
        // A default import is an identifier child of its import clause. Record it
        // from the clause: `Node::parent` on every identifier is a walk from the root.
        if node.kind() == "import_clause" {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if let Some((local, _)) = imported_name_in_clause(doc, child) {
                    charge(local.len() + 1, work, check)?;
                    scopes.entry(root.id()).or_default().insert(local);
                }
            }
            return Ok(false);
        }
        if node.kind() != "identifier"
            && let Some((local, _)) = imported_name(doc, node)
        {
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
                && node.parent().and_then(|p| p.parent()).is_some_and(|p| {
                    !matches!(
                        p.kind(),
                        "program" | "export_statement" | "ambient_declaration"
                    )
                });
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
    // A hazard past the walk bounds went unseen, so the file proves nothing about its exports.
    hazards.dynamic |= crate::syntax_bounds::scan(root, check)?.is_some();
    Ok(hazards)
}
pub(super) fn extract(
    doc: &DocumentInput,
    tree: &Tree,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<RawSite>> {
    let mut result = Vec::new();
    walk(tree.root_node(), check, &mut |node, check| {
        if matches!(node.kind(), "import_statement" | "export_statement")
            && let Some(source) = source_node(node)
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
#[path = "javascript_tests.rs"]
mod tests;
