//! Python server routes (#856): Flask and FastAPI route decorators on app, blueprint and
//! router objects, under the prefixes their registrations mount them at, and Django URL
//! patterns reached from a `ROOT_URLCONF`. Objects, mounts and views are followed across
//! documents only through bindings that resolve; anything else registers nothing.
use super::js::verb;
use super::python::{Arg, Args, Bound, Python, Target, named, visit};
use super::{
    ANY, Handler, HandlerKind, LineIndex, Route, charge, named_callable, segments, test_span,
};
use crate::native_evidence::Range;
use crate::references::work::Work;
use anyhow::Result;
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

/// How many mounts deep a registration chain is followed.
const MOUNT_DEPTH: usize = 16;
/// What building one route costs beyond its template: the handler and route records.
const ROUTE_COST: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Flask,
    Blueprint,
    FastApi,
    Router,
}

/// A prefix as written: none, a readable string, or one that cannot be read.
#[derive(Clone, Debug)]
enum Prefix {
    Absent,
    Text(String),
    Unknown,
}

#[derive(Clone, Debug)]
struct Object {
    kind: Kind,
    /// A blueprint's `url_prefix` or a router's `prefix`.
    prefix: Prefix,
}

/// An object: the parsed document and the name it is bound to there.
type Key = (usize, String);

/// One route decorator on a function.
struct Site {
    methods: Vec<&'static str>,
    path: String,
    handler: Handler,
    doc: usize,
    start: usize,
}

/// `parent.register_blueprint(child, url_prefix=..)` or `parent.include_router(child,
/// prefix=..)`.
struct Edge {
    child: Key,
    object: Object,
    prefix: Prefix,
}

/// The prefix an object's routes are under: a Flask `url_prefix` (or none), or everything a
/// FastAPI path is appended to.
enum State {
    Flask(Option<String>),
    FastApi(String),
}

/// A Django pattern list: a list a document binds, or one written inline in `include([..])`.
enum List<'t> {
    Named(usize, String),
    Inline(usize, Node<'t>),
}

struct Found {
    path: String,
    start: usize,
    route: Route,
}

struct Handlers {
    lines: LineIndex,
    keys: BTreeMap<String, String>,
}

struct Reading<'p, 't> {
    py: &'p Python<'t>,
    handlers: Vec<OnceCell<Handlers>>,
    found: Vec<Found>,
}

fn range(node: Node<'_>) -> Range {
    Range {
        start: node.start_byte(),
        end: node.end_byte(),
    }
}

/// The server routes of the parsed Python documents, one per method, template and
/// registering document.
pub(super) fn extract(
    py: &Python<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Route>> {
    let mut reading = Reading {
        py,
        handlers: (0..py.docs.len()).map(|_| OnceCell::new()).collect(),
        found: Vec::new(),
    };
    let mut sites: BTreeMap<Key, Vec<Site>> = BTreeMap::new();
    let mut edges: BTreeMap<Key, Vec<Edge>> = BTreeMap::new();
    let mut roots: Vec<(Key, Kind)> = Vec::new();
    let mut urlconfs = Vec::new();
    for (at, doc) in py.docs.iter().enumerate() {
        check()?;
        if !doc.server {
            continue;
        }
        for (name, _) in doc.names.singles() {
            if let Some(object) = reading.object(at, name, work, check)?
                && matches!(object.kind, Kind::Flask | Kind::FastApi)
            {
                roots.push(((at, name.to_owned()), object.kind));
            }
        }
        if let Some(value) = doc.names.constant("ROOT_URLCONF")
            && let Some(dotted) = doc.reader().exact(value, work, check)?
            && let Some(module) = py.resolve(&doc.input.key.path, 0, &dotted)
            && let Some(urls) = py.module_doc(&module)
        {
            urlconfs.push(List::Named(urls, "urlpatterns".to_owned()));
        }
        let mut decorated = Vec::new();
        let mut calls = Vec::new();
        visit(doc.root, work, check, &mut |node| {
            match node.kind() {
                "decorated_definition" => decorated.push(node),
                "call" => calls.push(node),
                _ => {}
            }
            true
        })?;
        for node in decorated {
            reading.decorated(at, node, &mut sites, work, check)?;
        }
        for node in calls {
            reading.registration(at, node, &mut edges, work, check)?;
        }
    }
    reading.mount(&roots, &sites, &edges, work, check)?;
    reading.django(urlconfs, work, check)?;
    let mut found = reading.found;
    found.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
    let mut seen = BTreeSet::new();
    Ok(found
        .into_iter()
        .filter(|f| {
            seen.insert((
                f.route.method.clone(),
                f.route.path_template.clone(),
                f.path.clone(),
            ))
        })
        .map(|f| f.route)
        .collect())
}

impl<'t> Reading<'_, 't> {
    fn handlers(&self, doc: usize) -> &Handlers {
        self.handlers[doc].get_or_init(|| {
            let input = self.py.docs[doc].input;
            Handlers {
                lines: LineIndex::new(&input.file.text),
                keys: crate::symbol_lookup::keys_from_source(
                    &input.key.path,
                    &input.key.language,
                    &input.file.text,
                    &input.declarations,
                )
                .unwrap_or_default(),
            }
        })
    }

    /// The handler of a function definition: its declaration, else its span.
    fn declaration(&self, doc: usize, function: Node<'_>, name: &str) -> Handler {
        let input = self.py.docs[doc].input;
        let span = range(function);
        let declaration = input
            .declarations
            .iter()
            .filter(|d| {
                d.kind == "function"
                    && d.name.as_deref() == Some(name)
                    && span.start <= d.range.start
                    && d.range.end <= span.end
            })
            .min_by_key(|d| d.range.start);
        let Some(declaration) = declaration else {
            return self.span(doc, span);
        };
        let handlers = self.handlers(doc);
        Handler {
            kind: HandlerKind::Declaration,
            syntax_id: Some(declaration.syntax_id.clone()),
            stable_lookup_key: handlers.keys.get(&declaration.syntax_id).cloned(),
            document: input.key.clone(),
            lines: handlers.lines.lines(&declaration.range),
            range: declaration.range.clone(),
            variant: None,
        }
    }

    /// A handler that is a span inside the innermost named callable around it.
    fn span(&self, doc: usize, range: Range) -> Handler {
        let input = self.py.docs[doc].input;
        let handlers = self.handlers(doc);
        let callable = named_callable(input, &range);
        Handler {
            kind: HandlerKind::Closure,
            syntax_id: callable.map(|d| d.syntax_id.clone()),
            stable_lookup_key: callable.and_then(|d| handlers.keys.get(&d.syntax_id).cloned()),
            document: input.key.clone(),
            lines: handlers.lines.lines(&range),
            range,
            variant: None,
        }
    }

    /// The framework object a name is bound to: `Flask(..)`, `Blueprint(..)`, `FastAPI(..)`
    /// or `APIRouter(..)`, each read through the import that binds the class.
    fn object(
        &self,
        doc: usize,
        name: &str,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Object>> {
        let d = &self.py.docs[doc];
        let Some(Bound::Assign { value, .. }) = d.names.single(name) else {
            return Ok(None);
        };
        let reader = d.reader();
        let Some(function) = (value.kind() == "call")
            .then(|| value.child_by_field_name("function"))
            .flatten()
        else {
            return Ok(None);
        };
        let kind = match reader.external(function) {
            Some(("flask" | "flask.app", "Flask")) => Kind::Flask,
            Some(("flask" | "flask.blueprints", "Blueprint")) => Kind::Blueprint,
            Some(("fastapi" | "fastapi.applications", "FastAPI")) => Kind::FastApi,
            Some(("fastapi" | "fastapi.routing", "APIRouter")) => Kind::Router,
            _ => return Ok(None),
        };
        let Some(args) = Args::of(*value, reader.source) else {
            return Ok(None);
        };
        let prefix = match kind {
            // `Blueprint(name, import_name, static_folder, static_url_path, template_folder,
            // url_prefix, ..)`; `APIRouter(*, prefix, ..)`.
            Kind::Blueprint => self.prefix(doc, args.get("url_prefix", Some(5)), work, check)?,
            Kind::Router => self.prefix(doc, args.keyword("prefix"), work, check)?,
            Kind::Flask | Kind::FastApi => Prefix::Absent,
        };
        Ok(Some(Object { kind, prefix }))
    }

    fn prefix(
        &self,
        doc: usize,
        arg: Arg<'t>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Prefix> {
        Ok(match arg {
            Arg::Absent => Prefix::Absent,
            Arg::Unknown => Prefix::Unknown,
            Arg::Given(node) if node.kind() == "none" => Prefix::Absent,
            Arg::Given(node) => match self.py.docs[doc].reader().exact(node, work, check)? {
                Some(text) => Prefix::Text(text),
                None => Prefix::Unknown,
            },
        })
    }

    /// A `methods=[..]` argument: absent or `None` is `GET`; anything but a list, tuple or
    /// set of readable method names is unreadable.
    fn methods(
        &self,
        doc: usize,
        arg: Arg<'t>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Vec<&'static str>>> {
        let node = match arg {
            Arg::Absent => return Ok(Some(vec!["GET"])),
            Arg::Unknown => return Ok(None),
            Arg::Given(node) if node.kind() == "none" => return Ok(Some(vec!["GET"])),
            Arg::Given(node) => node,
        };
        if !matches!(node.kind(), "list" | "tuple" | "set") {
            return Ok(None);
        }
        let reader = self.py.docs[doc].reader();
        let mut methods = Vec::new();
        for item in named(node) {
            let Some(method) = reader
                .exact(item, work, check)?
                .and_then(|text| verb(&text))
            else {
                return Ok(None);
            };
            if !methods.contains(&method) {
                methods.push(method);
            }
        }
        Ok((!methods.is_empty()).then_some(methods))
    }

    /// The route decorators on one function: Flask `@o.route(rule, methods=..)` and
    /// `@o.get(rule)`, and FastAPI `@o.get(path)` and `@o.api_route(path, methods=..)`,
    /// where `o` resolves to an object of that framework.
    fn decorated(
        &self,
        doc: usize,
        node: Node<'t>,
        sites: &mut BTreeMap<Key, Vec<Site>>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let d = &self.py.docs[doc];
        let reader = d.reader();
        let Some(definition) = node
            .child_by_field_name("definition")
            .filter(|definition| definition.kind() == "function_definition")
        else {
            return Ok(());
        };
        let Some(name) = definition.child_by_field_name("name") else {
            return Ok(());
        };
        if test_span(d.input, &range(definition)) {
            return Ok(());
        }
        let name = reader.text(name);
        for decorator in named(node) {
            check()?;
            if decorator.kind() != "decorator" || decorator.has_error() {
                continue;
            }
            let Some(call) = named(decorator)
                .first()
                .copied()
                .filter(|call| call.kind() == "call")
            else {
                continue;
            };
            let Some(function) = call
                .child_by_field_name("function")
                .filter(|function| function.kind() == "attribute")
            else {
                continue;
            };
            let (Some(owner), Some(attribute)) = (
                function.child_by_field_name("object"),
                function.child_by_field_name("attribute"),
            ) else {
                continue;
            };
            let Some(Target::Binding(owner_doc, owner_name)) =
                self.py.resolve_expression(doc, owner, work)?
            else {
                continue;
            };
            let Some(object) = self.object(owner_doc, &owner_name, work, check)? else {
                continue;
            };
            let Some(args) = Args::of(call, reader.source) else {
                continue;
            };
            let attribute = reader.text(attribute);
            let (methods, key) = match (object.kind, attribute) {
                (Kind::Flask | Kind::Blueprint, "route") => (
                    self.methods(doc, args.keyword("methods"), work, check)?,
                    "rule",
                ),
                (Kind::Flask | Kind::Blueprint, "get" | "post" | "put" | "delete" | "patch") => {
                    (verb(attribute).map(|method| vec![method]), "rule")
                }
                (
                    Kind::FastApi | Kind::Router,
                    "get" | "post" | "put" | "delete" | "patch" | "head" | "options",
                ) => (verb(attribute).map(|method| vec![method]), "path"),
                (Kind::FastApi | Kind::Router, "api_route") => (
                    self.methods(doc, args.keyword("methods"), work, check)?,
                    "path",
                ),
                _ => continue,
            };
            let Some(methods) = methods else {
                continue;
            };
            let Arg::Given(path) = args.get(key, Some(0)) else {
                continue;
            };
            let Some(path) = reader.exact(path, work, check)? else {
                continue;
            };
            let handler = self.declaration(doc, definition, name);
            sites
                .entry((owner_doc, owner_name))
                .or_default()
                .push(Site {
                    methods,
                    path,
                    handler,
                    doc,
                    start: decorator.start_byte(),
                });
        }
        Ok(())
    }

    /// One `register_blueprint` or `include_router` call between two resolved objects.
    fn registration(
        &self,
        doc: usize,
        call: Node<'t>,
        edges: &mut BTreeMap<Key, Vec<Edge>>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let d = &self.py.docs[doc];
        let reader = d.reader();
        let Some(function) = call
            .child_by_field_name("function")
            .filter(|function| function.kind() == "attribute")
        else {
            return Ok(());
        };
        let (Some(owner), Some(attribute)) = (
            function.child_by_field_name("object"),
            function.child_by_field_name("attribute"),
        ) else {
            return Ok(());
        };
        let attribute = reader.text(attribute);
        if !matches!(attribute, "register_blueprint" | "include_router")
            || call.has_error()
            || test_span(d.input, &range(call))
        {
            return Ok(());
        }
        check()?;
        let Some(Target::Binding(owner_doc, owner_name)) =
            self.py.resolve_expression(doc, owner, work)?
        else {
            return Ok(());
        };
        let Some(parent) = self.object(owner_doc, &owner_name, work, check)? else {
            return Ok(());
        };
        let Some(args) = Args::of(call, reader.source) else {
            return Ok(());
        };
        // `register_blueprint(blueprint, **options)`; `include_router(router, *, prefix, ..)`.
        let (child, prefix, wanted) = match (parent.kind, attribute) {
            (Kind::Flask | Kind::Blueprint, "register_blueprint") => (
                args.get("blueprint", Some(0)),
                args.keyword("url_prefix"),
                Kind::Blueprint,
            ),
            (Kind::FastApi | Kind::Router, "include_router") => (
                args.get("router", Some(0)),
                args.keyword("prefix"),
                Kind::Router,
            ),
            _ => return Ok(()),
        };
        let Arg::Given(child) = child else {
            return Ok(());
        };
        let Some(Target::Binding(child_doc, child_name)) =
            self.py.resolve_expression(doc, child, work)?
        else {
            return Ok(());
        };
        let Some(object) = self
            .object(child_doc, &child_name, work, check)?
            .filter(|object| object.kind == wanted)
        else {
            return Ok(());
        };
        let prefix = self.prefix(doc, prefix, work, check)?;
        edges
            .entry((owner_doc, owner_name))
            .or_default()
            .push(Edge {
                child: (child_doc, child_name),
                object,
                prefix,
            });
        Ok(())
    }

    /// Every route of every Flask and FastAPI app, through the mounts that reach it. Each
    /// object's routes under each prefix are charged before they are built.
    fn mount(
        &mut self,
        roots: &[(Key, Kind)],
        sites: &BTreeMap<Key, Vec<Site>>,
        edges: &BTreeMap<Key, Vec<Edge>>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut stack: Vec<(&Key, State, usize)> = roots
            .iter()
            .rev()
            .map(|(key, kind)| {
                let state = match kind {
                    Kind::Flask => State::Flask(None),
                    _ => State::FastApi(String::new()),
                };
                (key, state, 0)
            })
            .collect();
        while let Some((key, state, depth)) = stack.pop() {
            check()?;
            charge(work, 1)?;
            let held = match &state {
                State::Flask(prefix) => prefix.as_ref().map_or(0, String::len),
                State::FastApi(prefix) => prefix.len(),
            };
            if let Some(list) = sites.get(key) {
                let mut cost = 0usize;
                for site in list {
                    let each = held
                        .saturating_add(site.path.len())
                        .saturating_add(ROUTE_COST);
                    cost = cost.saturating_add(site.methods.len().saturating_mul(each));
                }
                charge(work, cost)?;
                for site in list {
                    check()?;
                    let template = match &state {
                        State::Flask(prefix) => angles(&flask_join(prefix.as_deref(), &site.path)),
                        State::FastApi(prefix) => braces(&format!("{prefix}{}", site.path)),
                    };
                    let Some(template) = template.as_deref().and_then(finish) else {
                        continue;
                    };
                    for method in &site.methods {
                        self.found.push(Found {
                            path: self.py.docs[site.doc].input.key.path.clone(),
                            start: site.start,
                            route: Route {
                                method: (*method).into(),
                                segments: segments(&template),
                                path_template: template.clone(),
                                from_table: false,
                                handler: site.handler.clone(),
                            },
                        });
                    }
                }
            }
            if depth >= MOUNT_DEPTH {
                continue;
            }
            for edge in edges.get(key).into_iter().flatten().rev() {
                // Flask: a registration's `url_prefix` replaces the blueprint's own, and a
                // nested blueprint's prefix is joined to its parent's. FastAPI: the include
                // prefix, then the router's own, are appended. An unreadable prefix mounts
                // nothing.
                let next = match (&state, edge.object.kind) {
                    (State::Flask(parent), Kind::Blueprint) => {
                        let own = match &edge.prefix {
                            Prefix::Absent => &edge.object.prefix,
                            given => given,
                        };
                        match own {
                            Prefix::Unknown => continue,
                            Prefix::Absent => State::Flask(parent.clone()),
                            Prefix::Text(text) => State::Flask(Some(match parent {
                                Some(parent) => slash_join(parent, text),
                                None => text.clone(),
                            })),
                        }
                    }
                    (State::FastApi(outer), Kind::Router) => {
                        let include = match &edge.prefix {
                            Prefix::Absent => "",
                            Prefix::Text(text) => text,
                            Prefix::Unknown => continue,
                        };
                        let own = match &edge.object.prefix {
                            Prefix::Absent => "",
                            Prefix::Text(text) => text,
                            Prefix::Unknown => continue,
                        };
                        State::FastApi(format!("{outer}{include}{own}"))
                    }
                    _ => continue,
                };
                let held = match &next {
                    State::Flask(prefix) => prefix.as_ref().map_or(0, String::len),
                    State::FastApi(prefix) => prefix.len(),
                };
                charge(work, held)?;
                stack.push((&edge.child, next, depth + 1));
            }
        }
        Ok(())
    }

    /// Every Django pattern reached from a `ROOT_URLCONF`, through `include(..)`. Each
    /// list's entries and each built route are charged before they are read or built.
    fn django(
        &mut self,
        roots: Vec<List<'t>>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut stack: Vec<(List<'t>, String, usize)> = roots
            .into_iter()
            .rev()
            .map(|list| (list, String::new(), 0))
            .collect();
        while let Some((list, prefix, depth)) = stack.pop() {
            check()?;
            charge(work, 1)?;
            let (doc, lists) = match &list {
                List::Named(doc, name) => (
                    *doc,
                    self.py.docs[*doc].names.extended(name).unwrap_or_default(),
                ),
                List::Inline(doc, node) => (*doc, vec![*node]),
            };
            let d = &self.py.docs[doc];
            let reader = d.reader();
            let entries: Vec<Node<'t>> = lists.iter().flat_map(|list| named(*list)).collect();
            charge(work, entries.len())?;
            let mut includes = Vec::new();
            for entry in entries {
                check()?;
                if entry.kind() != "call" || entry.has_error() || test_span(d.input, &range(entry))
                {
                    continue;
                }
                let Some(function) = entry.child_by_field_name("function") else {
                    continue;
                };
                let regex = match reader.external(function) {
                    Some(("django.urls" | "django.urls.conf", "path")) => false,
                    Some(("django.urls" | "django.urls.conf", "re_path")) => true,
                    _ => continue,
                };
                let Some(args) = Args::of(entry, reader.source) else {
                    continue;
                };
                // `path(route, view, kwargs=None, name=None)`.
                let (Arg::Given(route), Arg::Given(view)) =
                    (args.get("route", Some(0)), args.get("view", Some(1)))
                else {
                    continue;
                };
                let Some(route) = reader.exact(route, work, check)? else {
                    continue;
                };
                let fragment = if regex {
                    regex_template(&route)
                } else {
                    angles(&route)
                };
                let Some(fragment) = fragment else {
                    continue;
                };
                charge(
                    work,
                    prefix
                        .len()
                        .saturating_add(fragment.len())
                        .saturating_add(ROUTE_COST),
                )?;
                let full = format!("{prefix}{fragment}");
                if let Some(target) = self.include(doc, view, work, check)? {
                    if let Some(target) = target
                        && depth < MOUNT_DEPTH
                    {
                        includes.push((target, full, depth + 1));
                    }
                    continue;
                }
                let Some(template) = finish(&format!("/{full}")) else {
                    continue;
                };
                let handler = self.view(doc, view, work)?;
                self.found.push(Found {
                    path: d.input.key.path.clone(),
                    start: entry.start_byte(),
                    route: Route {
                        method: ANY.into(),
                        segments: segments(&template),
                        path_template: template,
                        from_table: false,
                        handler,
                    },
                });
            }
            stack.extend(includes.into_iter().rev());
        }
        Ok(())
    }

    /// The pattern list an `include(..)` view names. `None` when the view is not an
    /// `include`; `Some(None)` when it is one whose target cannot be resolved.
    fn include(
        &self,
        doc: usize,
        view: Node<'t>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Option<List<'t>>>> {
        let reader = self.py.docs[doc].reader();
        let Some(function) = (view.kind() == "call")
            .then(|| view.child_by_field_name("function"))
            .flatten()
        else {
            return Ok(None);
        };
        if !matches!(
            reader.external(function),
            Some(("django.urls" | "django.urls.conf", "include"))
        ) {
            return Ok(None);
        }
        let Some(Arg::Given(mut target)) =
            Args::of(view, reader.source).map(|args| args.get("arg", Some(0)))
        else {
            return Ok(Some(None));
        };
        // `include((patterns, app_name))`.
        if target.kind() == "tuple" {
            match named(target).first() {
                Some(first) => target = *first,
                None => return Ok(Some(None)),
            }
        }
        if target.kind() == "list" {
            return Ok(Some(Some(List::Inline(doc, target))));
        }
        let urls = |module: &str| self.py.module_doc(module);
        if let Some(dotted) = reader.exact(target, work, check)? {
            let from = &self.py.docs[doc].input.key.path;
            return Ok(Some(
                self.py
                    .resolve(from, 0, &dotted)
                    .and_then(|module| urls(&module))
                    .map(|urls| List::Named(urls, "urlpatterns".to_owned())),
            ));
        }
        Ok(Some(
            match self.py.resolve_expression(doc, target, work)? {
                Some(Target::Module(module)) => {
                    urls(&module).map(|urls| List::Named(urls, "urlpatterns".to_owned()))
                }
                Some(Target::Binding(at, name))
                    if self.py.docs[at].names.extended(&name).is_some() =>
                {
                    Some(List::Named(at, name))
                }
                _ => None,
            },
        ))
    }

    /// A Django view's handler: the function it resolves to, else its span.
    fn view(&self, doc: usize, view: Node<'t>, work: &mut Work) -> Result<Handler> {
        if let Some(Target::Binding(at, name)) = self.py.resolve_expression(doc, view, work)?
            && let Some(Bound::Def(node)) = self.py.docs[at].names.single(&name)
        {
            return Ok(self.declaration(at, *node, &name));
        }
        Ok(self.span(doc, range(view)))
    }
}

/// Flask's join of a `url_prefix` and a rule: one `/` between them, the prefix alone for an
/// empty rule, the rule alone without a prefix.
fn flask_join(prefix: Option<&str>, rule: &str) -> String {
    match prefix {
        None => rule.to_owned(),
        Some(prefix) if rule.is_empty() => prefix.to_owned(),
        Some(prefix) => slash_join(prefix, rule),
    }
}

fn slash_join(prefix: &str, path: &str) -> String {
    format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// A Flask rule or Django `path()` route in the #788 template form: `<name>`,
/// `<converter:name>` and Flask's `<converter(args):name>` are `{name}`, and the `path`
/// converter is `{*name}`. A brace, or an unbalanced or unnamed angle, is unreadable.
fn angles(pattern: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(c) = rest.chars().next() {
        match c {
            '{' | '}' | '>' => return None,
            '<' => {
                let end = rest.find('>')?;
                let body = &rest[1..end];
                let (converter, name) = match body.rsplit_once(':') {
                    Some((converter, name)) => (
                        converter.split('(').next().unwrap_or("").trim(),
                        name.trim(),
                    ),
                    None => ("", body.trim()),
                };
                if !identifier(name) {
                    return None;
                }
                if converter == "path" {
                    out.push_str(&format!("{{*{name}}}"));
                } else {
                    out.push_str(&format!("{{{name}}}"));
                }
                rest = &rest[end + 1..];
            }
            c => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    Some(out)
}

/// A FastAPI path in the #788 template form: `{name}` and `{name:converter}` are `{name}`,
/// and `{name:path}` is `{*name}`. An unbalanced or unnamed brace is unreadable.
fn braces(pattern: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(c) = rest.chars().next() {
        match c {
            '}' => return None,
            '{' => {
                let end = rest.find('}')?;
                let body = &rest[1..end];
                if body.contains('{') {
                    return None;
                }
                let (name, converter) = body.split_once(':').unwrap_or((body, ""));
                let name = name.trim();
                if !identifier(name) {
                    return None;
                }
                if converter.trim() == "path" {
                    out.push_str(&format!("{{*{name}}}"));
                } else {
                    out.push_str(&format!("{{{name}}}"));
                }
                rest = &rest[end + 1..];
            }
            c => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    Some(out)
}

/// A Django `re_path()` regex in the #788 template form, when it is a literal path with named
/// groups: `^` is required and a final `$` dropped, `(?P<name>..)` is `{name}` (`{*name}`
/// for `.*` or `.+`), and an escaped punctuation character is itself. Any other regex
/// syntax, an optional or repeated group, or a group that can span a `/`, is unreadable.
fn regex_template(pattern: &str) -> Option<String> {
    let pattern = pattern.strip_prefix('^')?;
    let pattern = pattern.strip_suffix('$').unwrap_or(pattern);
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut at = 0;
    while at < chars.len() {
        match chars[at] {
            '\\' => {
                let next = *chars.get(at + 1)?;
                if next.is_alphanumeric() {
                    return None;
                }
                out.push(next);
                at += 2;
            }
            '(' => {
                if chars.get(at + 1..at + 4) != Some(&['?', 'P', '<'][..]) {
                    return None;
                }
                let name_start = at + 4;
                let name_end = name_start + chars[name_start..].iter().position(|c| *c == '>')?;
                let name: String = chars[name_start..name_end].iter().collect();
                if !identifier(&name) {
                    return None;
                }
                let body_start = name_end + 1;
                let mut end = body_start;
                let mut nesting = 0usize;
                let mut class = false;
                loop {
                    match *chars.get(end)? {
                        '\\' => end += 1,
                        '[' if !class => class = true,
                        ']' if class => class = false,
                        '(' if !class => nesting += 1,
                        ')' if !class && nesting == 0 => break,
                        ')' if !class => nesting -= 1,
                        _ => {}
                    }
                    end += 1;
                }
                let body: String = chars[body_start..end].iter().collect();
                at = end + 1;
                if matches!(chars.get(at), Some('?' | '*' | '+' | '{')) || body.contains('/') {
                    return None;
                }
                if matches!(body.as_str(), ".*" | ".+") {
                    out.push_str(&format!("{{*{name}}}"));
                } else {
                    out.push_str(&format!("{{{name}}}"));
                }
            }
            '.' | '^' | '$' | '*' | '+' | '?' | '{' | '}' | '[' | ']' | '|' | ')' => return None,
            c => {
                out.push(c);
                at += 1;
            }
        }
    }
    Some(out)
}

/// A complete template: rooted at `/`, and with a rest capture only as its whole last
/// segment; a rest capture anywhere else is a `{name}`.
fn finish(template: &str) -> Option<String> {
    let template = if template.starts_with('/') {
        template.to_owned()
    } else {
        format!("/{template}")
    };
    let parts: Vec<&str> = template[1..].split('/').collect();
    let last = if parts.len() > 1 && parts.last() == Some(&"") {
        parts.len() - 2
    } else {
        parts.len() - 1
    };
    let mut out = String::new();
    for (at, part) in parts.iter().enumerate() {
        out.push('/');
        let whole_rest =
            part.starts_with("{*") && part.ends_with('}') && part.matches('{').count() == 1;
        if part.contains("{*") && !(at == last && whole_rest) {
            out.push_str(&part.replace("{*", "{"));
        } else {
            out.push_str(part);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converters_normalise_to_the_template_form() {
        for (pattern, template) in [
            ("/users/<int:id>", Some("/users/{id}")),
            ("/users/<id>/posts", Some("/users/{id}/posts")),
            ("/c/<string(length=2):code>", Some("/c/{code}")),
            ("/files/<path:rest>", Some("/files/{*rest}")),
            ("/bad/<int:>", None),
            ("/bad/<id", None),
            ("/bad/{id}", None),
            ("/bad/>", None),
        ] {
            assert_eq!(
                angles(pattern).as_deref().and_then(finish).as_deref(),
                template,
                "{pattern}"
            );
        }
        for (pattern, template) in [
            ("/items/{id}", Some("/items/{id}")),
            ("/items/{id:int}", Some("/items/{id}")),
            ("/files/{p:path}", Some("/files/{*p}")),
            ("/a/{p:path}/b", Some("/a/{p}/b")),
            ("/bad/{id", None),
            ("/bad/}", None),
            ("/bad/{}", None),
        ] {
            assert_eq!(
                braces(pattern).as_deref().and_then(finish).as_deref(),
                template,
                "{pattern}"
            );
        }
        for (pattern, template) in [
            (r"^articles/(?P<year>[0-9]{4})/$", Some("/articles/{year}/")),
            (r"^files/(?P<rest>.*)$", Some("/files/{*rest}")),
            (r"^a\.json$", Some("/a.json")),
            (r"^api/", Some("/api/")),
            (r"articles/$", None),
            (r"^a/(\d+)/$", None),
            (r"^a/(?P<x>\d+)?/$", None),
            (r"^a/\d+/$", None),
            (r"^a/(?P<d>\d+/\d+)/$", None),
            (r"^a|b$", None),
        ] {
            assert_eq!(
                regex_template(pattern)
                    .map(|fragment| format!("/{fragment}"))
                    .as_deref()
                    .and_then(finish)
                    .as_deref(),
                template,
                "{pattern}"
            );
        }
    }

    #[test]
    fn a_rest_capture_is_only_a_whole_last_segment() {
        assert_eq!(finish("/a/{*p}/b").as_deref(), Some("/a/{p}/b"));
        assert_eq!(finish("/a/{*p}/").as_deref(), Some("/a/{*p}/"));
        assert_eq!(finish("/a/{*p}.txt").as_deref(), Some("/a/{p}.txt"));
        assert_eq!(finish("x").as_deref(), Some("/x"));
        assert_eq!(finish("/").as_deref(), Some("/"));
    }
}
