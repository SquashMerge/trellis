//! JavaScript and TypeScript server routes (#855): express, fastify and Koa registrations
//! reached from an app object through its mounts, and Next.js file routes.
use super::js::{
    Binding, BindingId, Budget, Factory, Value, World, arguments, children, function_like,
    readable_path, spread, verb,
};
use super::{
    ANY, Handler, HandlerKind, LineIndex, Route, named_callable, segments, test_file, test_span,
};
use crate::native_evidence::{Declaration, Range};
use crate::references::walk;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

/// How many mounts deep a router is followed.
const MOUNT_DEPTH: usize = 16;
/// The packages whose app objects start route discovery.
const FRAMEWORKS: [&str; 4] = ["express", "fastify", "koa", "@koa/router"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Framework {
    Express,
    Fastify,
    KoaApp,
    KoaRouter,
}

/// An app or router object, the binding that holds it, and the path prefix it serves under.
#[derive(Clone)]
struct Receiver<'w> {
    framework: Framework,
    binding: Binding<'w>,
    /// The Koa router's own `prefix` option; `None` when unreadable.
    own_prefix: Option<String>,
    prefix: String,
}

pub(super) struct Found {
    pub(super) path: String,
    pub(super) start: usize,
    pub(super) route: Route,
}

/// The routes of every production JavaScript and TypeScript document.
pub(super) fn extract(
    world: &World<'_>,
    next_roots: &[String],
    budget: &mut Budget<'_>,
) -> Result<Vec<Found>> {
    let mut extractor = Extractor {
        world,
        found: Vec::new(),
        visited: BTreeSet::new(),
        lines: BTreeMap::new(),
        keys: BTreeMap::new(),
    };
    for at in 0..world.snapshot.documents.len() {
        budget.step(1)?;
        let doc = world.doc(at);
        if !world.readable(at) || test_file(doc) {
            continue;
        }
        let text = &doc.file.text;
        if FRAMEWORKS.iter().any(|name| {
            text.contains(&format!("\"{name}\"")) || text.contains(&format!("'{name}'"))
        }) {
            extractor.roots(at, budget)?;
        }
        if let Some(path) = next_route_path(&doc.key.path, next_roots) {
            extractor.next(at, path, budget)?;
        }
    }
    Ok(extractor.found)
}

struct Extractor<'w, 's> {
    world: &'w World<'s>,
    found: Vec<Found>,
    visited: BTreeSet<(BindingId, String)>,
    lines: BTreeMap<usize, LineIndex>,
    keys: BTreeMap<usize, BTreeMap<String, String>>,
}

impl<'w> Extractor<'w, '_> {
    /// The app objects a document creates: `express()`, `fastify()` and `new Koa()`.
    fn roots(&mut self, doc: usize, budget: &mut Budget<'_>) -> Result<()> {
        let Some(tree) = self.world.tree(doc, budget)? else {
            return Ok(());
        };
        let mut declarators = Vec::new();
        walk(tree.root_node(), &mut budget.check, &mut |node, _| {
            if node.kind() == "variable_declarator" {
                declarators.push(node);
            }
            Ok(false)
        })?;
        for declarator in declarators {
            let Some(name) = declarator
                .child_by_field_name("name")
                .filter(|name| name.kind() == "identifier")
            else {
                continue;
            };
            // An app object is made by a call or `new`.
            if !declarator
                .child_by_field_name("value")
                .is_some_and(|value| {
                    matches!(
                        super::js::unwrap(value).kind(),
                        "call_expression" | "new_expression"
                    )
                })
            {
                continue;
            }
            let framework = match self.world.eval(doc, name, budget)? {
                Value::Made {
                    factory: Factory::ExpressApp,
                    ..
                } => Framework::Express,
                Value::Made {
                    factory: Factory::FastifyApp,
                    ..
                } => Framework::Fastify,
                Value::Made {
                    factory: Factory::KoaApp,
                    ..
                } => Framework::KoaApp,
                _ => continue,
            };
            if let Some(binding) = self.world.binding(doc, name, budget)? {
                self.process(
                    Receiver {
                        framework,
                        binding,
                        own_prefix: Some(String::new()),
                        prefix: String::new(),
                    },
                    0,
                    budget,
                )?;
            }
        }
        Ok(())
    }

    /// Every use of a receiver in its scope: registrations, mounts and hand-offs.
    fn process(
        &mut self,
        receiver: Receiver<'w>,
        depth: usize,
        budget: &mut Budget<'_>,
    ) -> Result<()> {
        let doc = receiver.binding.id.doc;
        if depth > MOUNT_DEPTH || test_file(self.world.doc(doc)) {
            return Ok(());
        }
        let Some(own) = receiver.own_prefix.clone() else {
            return Ok(());
        };
        let mut prefix = join(&receiver.prefix, &own);
        if !self.visited.insert((receiver.binding.id, prefix.clone())) {
            return Ok(());
        }
        let scope = receiver.binding.scope;
        budget.step(scope.end_byte() - scope.start_byte())?;
        let mut calls = Vec::new();
        walk(scope, &mut budget.check, &mut |node, _| {
            if node.kind() == "call_expression" {
                calls.push(node);
            }
            Ok(false)
        })?;
        let name = self.world.text(doc, receiver.binding.name);
        // A Koa router's `prefix(path)` replaces its prefix option.
        if receiver.framework == Framework::KoaRouter {
            for call in &calls {
                if let Some(("prefix", arguments)) =
                    self.method_use(&receiver, name, *call, budget)?
                {
                    match arguments.first() {
                        Some(path) => match self.world.eval(doc, *path, budget)? {
                            Value::Str(path) if readable_path(&path) => {
                                prefix = join(&receiver.prefix, &path);
                            }
                            _ => return Ok(()),
                        },
                        None => return Ok(()),
                    }
                }
            }
        }
        for call in calls {
            if let Some((method, arguments)) = self.method_use(&receiver, name, call, budget)? {
                self.method(&receiver, &prefix, call, method, &arguments, depth, budget)?;
                continue;
            }
            self.hand_off(&receiver, &prefix, name, call, depth, budget)?;
        }
        Ok(())
    }

    /// `receiver.method(arguments)`, when the call's object is the receiver's binding.
    fn method_use(
        &self,
        receiver: &Receiver<'w>,
        name: &str,
        call: Node<'w>,
        budget: &mut Budget<'_>,
    ) -> Result<Option<(&'w str, Vec<Node<'w>>)>> {
        let doc = receiver.binding.id.doc;
        let Some(function) = call
            .child_by_field_name("function")
            .filter(|f| f.kind() == "member_expression")
        else {
            return Ok(None);
        };
        let (Some(object), Some(property)) = (
            function.child_by_field_name("object"),
            function.child_by_field_name("property"),
        ) else {
            return Ok(None);
        };
        if object.kind() != "identifier" || self.world.text(doc, object) != name {
            return Ok(None);
        }
        if self.world.binding(doc, object, budget)?.map(|b| b.id) != Some(receiver.binding.id) {
            return Ok(None);
        }
        Ok(Some((self.world.text(doc, property), arguments(call))))
    }

    /// The receiver passed to a function: that function's parameter is the same receiver.
    fn hand_off(
        &mut self,
        receiver: &Receiver<'w>,
        prefix: &str,
        name: &str,
        call: Node<'w>,
        depth: usize,
        budget: &mut Budget<'_>,
    ) -> Result<()> {
        let doc = receiver.binding.id.doc;
        for (index, argument) in arguments(call).into_iter().enumerate() {
            if argument.kind() != "identifier" || self.world.text(doc, argument) != name {
                continue;
            }
            if self.world.binding(doc, argument, budget)?.map(|b| b.id) != Some(receiver.binding.id)
            {
                continue;
            }
            let Some(function) = call.child_by_field_name("function") else {
                continue;
            };
            if let Value::Function {
                doc: target, node, ..
            } = self.world.eval(doc, function, budget)?
                && let Some(binding) = self.world.parameter(target, node, index)
            {
                self.process(
                    Receiver {
                        framework: receiver.framework,
                        binding,
                        own_prefix: Some(String::new()),
                        prefix: prefix.to_owned(),
                    },
                    depth + 1,
                    budget,
                )?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn method(
        &mut self,
        receiver: &Receiver<'w>,
        prefix: &str,
        call: Node<'w>,
        method: &str,
        arguments: &[Node<'w>],
        depth: usize,
        budget: &mut Budget<'_>,
    ) -> Result<()> {
        let doc = receiver.binding.id.doc;
        let registered = match (receiver.framework, method) {
            (Framework::KoaApp, _) => None,
            (_, "all") => Some(ANY),
            (Framework::KoaRouter, "del") => Some("DELETE"),
            (_, method) if method.chars().all(|c| c.is_ascii_lowercase()) => verb(method),
            _ => None,
        };
        if let Some(registered) = registered {
            let mut arguments = arguments;
            // A Koa named route: `router.get('name', '/path', handler)`.
            if receiver.framework == Framework::KoaRouter
                && arguments.len() >= 3
                && let Value::Str(name) = self.world.eval(doc, arguments[0], budget)?
                && !name.starts_with('/')
            {
                arguments = &arguments[1..];
            }
            return self.register(doc, prefix, call, &[registered], arguments, budget);
        }
        match (receiver.framework, method) {
            (Framework::Express, "route") => self.route_chain(doc, prefix, call, budget),
            (Framework::Fastify, "route") => {
                self.fastify_route(doc, prefix, call, arguments, budget)
            }
            (Framework::Express | Framework::KoaApp | Framework::KoaRouter, "use") => {
                self.mount(receiver, prefix, arguments, depth, budget)
            }
            (Framework::Fastify, "register") => {
                self.register_plugin(receiver, prefix, arguments, depth, budget)
            }
            _ => Ok(()),
        }
    }

    /// `use([path,] ...routers)`: each router argument serves under the path.
    fn mount(
        &mut self,
        receiver: &Receiver<'w>,
        prefix: &str,
        arguments: &[Node<'w>],
        depth: usize,
        budget: &mut Budget<'_>,
    ) -> Result<()> {
        let doc = receiver.binding.id.doc;
        let Some(first) = arguments.first() else {
            return Ok(());
        };
        let (path, rest) = match self.world.eval(doc, *first, budget)? {
            Value::Str(path) if readable_path(&path) => (path, &arguments[1..]),
            Value::Str(_) => return Ok(()),
            // A name or member with no readable value may be a path: nothing after it is
            // mounted anywhere this extraction can say.
            Value::Unknown
                if matches!(
                    super::js::unwrap(*first).kind(),
                    "identifier" | "member_expression" | "subscript_expression"
                ) =>
            {
                return Ok(());
            }
            _ => (String::new(), arguments),
        };
        let prefix = join(prefix, &path);
        for argument in rest {
            let (framework, value) =
                match (receiver.framework, self.world.eval(doc, *argument, budget)?) {
                    (Framework::Express, value) => (Framework::Express, value),
                    (_, Value::Routes(inner)) => (Framework::KoaRouter, *inner),
                    _ => continue,
                };
            let (binding, own_prefix) = match value {
                Value::Made {
                    factory: Factory::ExpressRouter,
                    binding: Some(binding),
                } if framework == Framework::Express => (binding, Some(String::new())),
                Value::Made {
                    factory: Factory::KoaRouter(own),
                    binding: Some(binding),
                } if framework == Framework::KoaRouter => (binding, own),
                Value::Param(binding) => (binding, Some(String::new())),
                _ => continue,
            };
            self.process(
                Receiver {
                    framework,
                    binding,
                    own_prefix,
                    prefix: prefix.clone(),
                },
                depth + 1,
                budget,
            )?;
        }
        Ok(())
    }

    /// `fastify.register(plugin, {prefix})`: the plugin's first parameter serves under it.
    fn register_plugin(
        &mut self,
        receiver: &Receiver<'w>,
        prefix: &str,
        arguments: &[Node<'w>],
        depth: usize,
        budget: &mut Budget<'_>,
    ) -> Result<()> {
        let doc = receiver.binding.id.doc;
        let Some(plugin) = arguments.first() else {
            return Ok(());
        };
        let path = match arguments.get(1) {
            None => String::new(),
            Some(options) => match self.world.eval(doc, *options, budget)? {
                Value::Object(at, object) => match self.world.property(at, object, "prefix") {
                    None if spread(object) => return Ok(()),
                    None => String::new(),
                    Some(path) => match self.world.eval(at, path, budget)? {
                        Value::Str(path) if readable_path(&path) => path,
                        _ => return Ok(()),
                    },
                },
                _ => return Ok(()),
            },
        };
        if let Value::Function {
            doc: target, node, ..
        } = self.world.eval(doc, *plugin, budget)?
            && let Some(binding) = self.world.parameter(target, node, 0)
        {
            self.process(
                Receiver {
                    framework: Framework::Fastify,
                    binding,
                    own_prefix: Some(String::new()),
                    prefix: join(prefix, &path),
                },
                depth + 1,
                budget,
            )?;
        }
        Ok(())
    }

    /// `app.route('/x').get(h).post(h)`.
    fn route_chain(
        &mut self,
        doc: usize,
        prefix: &str,
        call: Node<'w>,
        budget: &mut Budget<'_>,
    ) -> Result<()> {
        let route_arguments = arguments(call);
        let Some(path) = route_arguments.first() else {
            return Ok(());
        };
        let mut current = call;
        while let Some(member) = current.parent().filter(|p| p.kind() == "member_expression")
            && let Some(next) = member.parent().filter(|p| p.kind() == "call_expression")
            && let Some(property) = member.child_by_field_name("property")
        {
            let method = self.world.text(doc, property);
            let registered = match method {
                "all" => Some(ANY),
                method if method.chars().all(|c| c.is_ascii_lowercase()) => verb(method),
                _ => None,
            };
            let Some(registered) = registered else { break };
            let handlers = arguments(next);
            let mut with_path = vec![*path];
            with_path.extend(handlers);
            if with_path.len() >= 2 {
                self.register(doc, prefix, next, &[registered], &with_path, budget)?;
            }
            current = next;
        }
        Ok(())
    }

    /// `fastify.route({method, url, handler})`.
    fn fastify_route(
        &mut self,
        doc: usize,
        prefix: &str,
        call: Node<'w>,
        arguments: &[Node<'w>],
        budget: &mut Budget<'_>,
    ) -> Result<()> {
        let Some(Value::Object(at, object)) = arguments
            .first()
            .map(|options| self.world.eval(doc, *options, budget))
            .transpose()?
        else {
            return Ok(());
        };
        let (Some(method), Some(url), Some(handler)) = (
            self.world.property(at, object, "method"),
            self.world
                .property(at, object, "url")
                .or_else(|| self.world.property(at, object, "path")),
            self.world.property(at, object, "handler"),
        ) else {
            return Ok(());
        };
        let methods: Vec<Node<'w>> = match method.kind() {
            "array" => children(method),
            _ => vec![method],
        };
        let mut verbs = Vec::new();
        for method in methods {
            match self.world.eval(at, method, budget)? {
                Value::Str(text) if verb(&text).is_some() => verbs.extend(verb(&text)),
                _ => return Ok(()),
            }
        }
        self.register(at, prefix, call, &verbs, &[url, handler], budget)
    }

    /// One route per method: the path at argument 0 and the handler last.
    fn register(
        &mut self,
        doc: usize,
        prefix: &str,
        call: Node<'w>,
        methods: &[&str],
        arguments: &[Node<'w>],
        budget: &mut Budget<'_>,
    ) -> Result<()> {
        let [path, .., handler] = arguments else {
            return Ok(());
        };
        let source = self.world.doc(doc);
        if test_span(source, &range(call)) {
            return Ok(());
        }
        let path = match self.world.eval(doc, *path, budget)? {
            Value::Str(path) if path == "*" => "/*".to_owned(),
            Value::Str(path) if path.starts_with('/') && readable_path(&path) => path,
            _ => return Ok(()),
        };
        // A fastify handler can be the `handler` of an options object.
        let mut handler = *handler;
        if handler.kind() == "object"
            && let Some(inner) = self.world.property(doc, handler, "handler")
        {
            handler = inner;
        }
        let handler = self.handler(doc, handler, budget)?;
        let template = normalize(&join(prefix, &path));
        for method in methods {
            self.found.push(Found {
                path: source.key.path.clone(),
                start: call.start_byte(),
                route: Route {
                    method: (*method).to_owned(),
                    segments: segments(&template),
                    path_template: template.clone(),
                    from_table: false,
                    handler: handler.clone(),
                },
            });
        }
        Ok(())
    }

    fn lines(&mut self, doc: usize, range: &Range) -> super::Lines {
        let text = &self.world.doc(doc).file.text;
        self.lines
            .entry(doc)
            .or_insert_with(|| LineIndex::new(text))
            .lines(range)
    }

    fn key(&mut self, doc: usize, syntax_id: &str) -> Option<String> {
        let source = self.world.doc(doc);
        self.keys
            .entry(doc)
            .or_insert_with(|| {
                crate::symbol_lookup::keys_from_source(
                    &source.key.path,
                    &source.key.language,
                    &source.file.text,
                    &source.declarations,
                )
                .unwrap_or_default()
            })
            .get(syntax_id)
            .cloned()
    }

    /// The handler a value names: the declaration of a named function, else the span of
    /// the handler expression inside the named callable around it.
    fn handler(&mut self, doc: usize, node: Node<'w>, budget: &mut Budget<'_>) -> Result<Handler> {
        if let Value::Function {
            doc: at,
            node: function,
            declaration,
        } = self.world.eval(doc, node, budget)?
        {
            if let Some(declared) = declaration.and_then(|d| self.declaration(at, d)) {
                return Ok(self.declaration_handler(at, declared));
            }
            return Ok(self.span_handler(at, range(function)));
        }
        Ok(self.span_handler(doc, range(node)))
    }

    /// The measured declaration of a function declaration or a variable declarator.
    fn declaration(&self, doc: usize, node: Node<'_>) -> Option<&'w Declaration> {
        let kinds: &[&str] = match node.kind() {
            "variable_declarator" => &["variable"],
            _ => &["function"],
        };
        self.world.doc(doc).declarations.iter().find(|d| {
            d.range.start == node.start_byte()
                && d.range.end == node.end_byte()
                && kinds.contains(&d.kind.as_str())
                && d.name.is_some()
        })
    }

    fn declaration_handler(&mut self, doc: usize, declaration: &Declaration) -> Handler {
        Handler {
            kind: HandlerKind::Declaration,
            syntax_id: Some(declaration.syntax_id.clone()),
            stable_lookup_key: self.key(doc, &declaration.syntax_id),
            document: self.world.doc(doc).key.clone(),
            lines: self.lines(doc, &declaration.range),
            range: declaration.range.clone(),
            variant: None,
        }
    }

    fn span_handler(&mut self, doc: usize, span: Range) -> Handler {
        let callable = named_callable(self.world.doc(doc), &span);
        let syntax_id = callable.map(|d| d.syntax_id.clone());
        Handler {
            kind: HandlerKind::Closure,
            stable_lookup_key: syntax_id.as_deref().and_then(|id| self.key(doc, id)),
            syntax_id,
            document: self.world.doc(doc).key.clone(),
            lines: self.lines(doc, &span),
            range: span,
            variant: None,
        }
    }

    // Next.js file routes.

    fn next(&mut self, doc: usize, path: NextPath, budget: &mut Budget<'_>) -> Result<()> {
        let Some(tree) = self.world.tree(doc, budget)? else {
            return Ok(());
        };
        let root = tree.root_node();
        let statements = children(root);
        budget.step(statements.len())?;
        let mut exported: Vec<(&'static str, Node<'w>)> = Vec::new();
        for statement in statements {
            if statement.kind() != "export_statement" {
                continue;
            }
            let default = (0..statement.child_count())
                .filter_map(|at| statement.child(at))
                .any(|child| child.kind() == "default");
            match (path.kind, default) {
                (NextKind::Pages, true) => {
                    let handler = statement
                        .child_by_field_name("declaration")
                        .or_else(|| statement.child_by_field_name("value"));
                    if let Some(handler) = handler {
                        exported.push((ANY, handler));
                    }
                }
                (NextKind::App, false) => self.app_exports(doc, statement, &mut exported),
                _ => {}
            }
        }
        for (method, node) in exported {
            if test_span(self.world.doc(doc), &range(node)) {
                continue;
            }
            let handler = if function_like(node) || node.kind() == "variable_declarator" {
                match self.declaration(doc, node) {
                    Some(declared) => self.declaration_handler(doc, declared),
                    None => self.span_handler(doc, range(node)),
                }
            } else {
                self.handler(doc, node, budget)?
            };
            for template in &path.templates {
                self.found.push(Found {
                    path: self.world.doc(doc).key.path.clone(),
                    start: node.start_byte(),
                    route: Route {
                        method: method.to_owned(),
                        segments: segments(template),
                        path_template: template.clone(),
                        from_table: false,
                        handler: handler.clone(),
                    },
                });
            }
        }
        Ok(())
    }

    /// The HTTP method exports of an app-router `route` file.
    fn app_exports(
        &self,
        doc: usize,
        statement: Node<'w>,
        exported: &mut Vec<(&'static str, Node<'w>)>,
    ) {
        let named = |name: &str| super::js::VERBS.iter().copied().find(|v| *v == name);
        if let Some(declaration) = statement.child_by_field_name("declaration") {
            match declaration.kind() {
                "function_declaration" | "generator_function_declaration" => {
                    if let Some(method) = declaration
                        .child_by_field_name("name")
                        .and_then(|name| named(self.world.text(doc, name)))
                    {
                        exported.push((method, declaration));
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    for declarator in children(declaration) {
                        if let Some(method) = declarator
                            .child_by_field_name("name")
                            .and_then(|name| named(self.world.text(doc, name)))
                        {
                            exported.push((method, declarator));
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        if statement.child_by_field_name("source").is_some() {
            return;
        }
        for clause in children(statement) {
            if clause.kind() != "export_clause" {
                continue;
            }
            for specifier in children(clause) {
                let Some(local) = specifier.child_by_field_name("name") else {
                    continue;
                };
                let exported_name = specifier.child_by_field_name("alias").unwrap_or(local);
                if let Some(method) = named(self.world.text(doc, exported_name)) {
                    exported.push((method, local));
                }
            }
        }
    }
}

fn range(node: Node<'_>) -> Range {
    Range {
        start: node.start_byte(),
        end: node.end_byte(),
    }
}

/// A mount prefix and a path, with one `/` between them and no trailing `/`.
fn join(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    match (prefix.is_empty(), path.is_empty()) {
        (true, true) => String::new(),
        (_, true) => prefix.to_owned(),
        _ => format!("{prefix}/{path}"),
    }
}

/// `:name` as `{name}`, `*name` as `{*name}`, and the root as `/`.
fn normalize(path: &str) -> String {
    if path.is_empty() {
        return "/".to_owned();
    }
    path.split('/')
        .map(
            |part| match (part.strip_prefix(':'), part.strip_prefix('*')) {
                (Some(name), _) => format!("{{{name}}}"),
                (_, Some(name)) => format!("{{*{name}}}"),
                _ => part.to_owned(),
            },
        )
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NextKind {
    /// `pages/api/**`: the default export answers every method.
    Pages,
    /// `app/**/route.(js|ts)`: each exported method function answers its method.
    App,
}

struct NextPath {
    kind: NextKind,
    /// One template, or two for an optional catch-all.
    templates: Vec<String>,
}

const NEXT_EXTENSIONS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];

/// The route template a file under a Next.js app root has, if it is a route file.
fn next_route_path(path: &str, roots: &[String]) -> Option<NextPath> {
    let relative = roots.iter().find_map(|root| match root.as_str() {
        "" => Some(path),
        root => path.strip_prefix(root)?.strip_prefix('/'),
    })?;
    let relative = relative.strip_prefix("src/").unwrap_or(relative);
    let (stem, extension) = relative.rsplit_once('.')?;
    if !NEXT_EXTENSIONS.contains(&extension) || stem.ends_with(".d") {
        return None;
    }
    let (kind, parts): (NextKind, Vec<&str>) = if let Some(rest) = stem.strip_prefix("pages/api/") {
        let mut parts: Vec<&str> = std::iter::once("api").chain(rest.split('/')).collect();
        if parts.last().is_some_and(|last| last.starts_with('_')) {
            return None;
        }
        if parts.last() == Some(&"index") {
            parts.pop();
        }
        (NextKind::Pages, parts)
    } else if let Some(rest) = stem.strip_prefix("app/") {
        let mut parts: Vec<&str> = rest.split('/').collect();
        if parts.pop() != Some("route") {
            return None;
        }
        (NextKind::App, parts)
    } else if stem == "app/route" {
        (NextKind::App, Vec::new())
    } else {
        return None;
    };
    let mut written = Vec::new();
    let mut optional = None;
    for part in parts {
        if part.starts_with('_') || part.starts_with("(.") {
            return None;
        }
        if (part.starts_with('(') && part.ends_with(')')) || part.starts_with('@') {
            continue;
        }
        if let Some(name) = part
            .strip_prefix("[[...")
            .and_then(|name| name.strip_suffix("]]"))
        {
            optional = Some(written.len());
            written.push(format!("{{*{name}}}"));
        } else if let Some(name) = part.strip_prefix("[...").and_then(|n| n.strip_suffix(']')) {
            written.push(format!("{{*{name}}}"));
        } else if let Some(name) = part.strip_prefix('[').and_then(|n| n.strip_suffix(']')) {
            written.push(format!("{{{name}}}"));
        } else if part.contains(['[', ']', '{', '}']) || part.is_empty() {
            return None;
        } else {
            written.push(part.to_owned());
        }
    }
    let template = |parts: &[String]| format!("/{}", parts.join("/"));
    let mut templates = vec![template(&written)];
    if let Some(at) = optional {
        templates.insert(0, template(&written[..at]));
    }
    Some(NextPath { kind, templates })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_files_have_normalised_templates() {
        let roots = ["web".to_owned()];
        let template = |path: &str| {
            next_route_path(path, &roots).map(|p| (p.kind == NextKind::App, p.templates))
        };
        assert_eq!(
            template("web/pages/api/users/[id].ts"),
            Some((false, vec!["/api/users/{id}".into()]))
        );
        assert_eq!(
            template("web/src/pages/api/index.js"),
            Some((false, vec!["/api".into()]))
        );
        assert_eq!(
            template("web/app/(shop)/items/[...slug]/route.ts"),
            Some((true, vec!["/items/{*slug}".into()]))
        );
        assert_eq!(
            template("web/app/docs/[[...page]]/route.js"),
            Some((true, vec!["/docs".into(), "/docs/{*page}".into()]))
        );
        assert_eq!(template("web/app/route.ts"), Some((true, vec!["/".into()])));
        assert_eq!(template("web/app/items/page.tsx"), None);
        assert_eq!(template("web/app/_lib/x/route.ts"), None);
        assert_eq!(template("web/pages/users.tsx"), None);
        assert_eq!(template("web/pages/api/_middleware.ts"), None);
        assert_eq!(template("other/pages/api/x.ts"), None);
    }

    #[test]
    fn mount_paths_join_and_normalise() {
        assert_eq!(normalize(&join("/api/", "/users/:id")), "/api/users/{id}");
        assert_eq!(normalize(&join("/api", "/")), "/api");
        assert_eq!(normalize(&join("", "/")), "/");
        assert_eq!(normalize(&join("/files", "/*rest")), "/files/{*rest}");
    }
}
