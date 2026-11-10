//! actix-web server routes (#858): route attribute macros, `App`/`Scope` `.route(path, r)`,
//! `web::resource(path).route(r)` and the `web::scope(prefix)` prefixes around them.
use super::{Document, Found, path_verb, router_verb};
use crate::interfaces::{HandlerKind, Route, charge, segments};
use crate::references::work::Work;
use crate::syntax_bounds::{DEPTH, VISITS};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tree_sitter::Node;

/// Whether a Rust document can register actix-web routes.
pub(super) fn candidate(text: &str) -> bool {
    text.contains("actix_web") || text.contains("web::")
}

/// A function carrying a route attribute. Its path is complete only once the scopes that
/// register it with `.service(f)` are known, which may be in another document.
struct Attributed {
    name: String,
    method: &'static str,
    path: String,
    found: Found,
}

/// What the actix-web documents of a snapshot register.
#[derive(Default)]
pub(super) struct Collected {
    attributed: Vec<Attributed>,
    /// The prefixes each `.service(name)` registers `name` under. `None` is a scope whose
    /// prefix cannot be read.
    services: BTreeMap<String, Vec<Option<String>>>,
}

impl Collected {
    /// Each attributed function as a route under every prefix it is registered with, or as
    /// written when no `.service(name)` registers it.
    pub(super) fn finish(
        self,
        found: &mut Vec<Found>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        // The expansion is every attributed route under every readable prefix that registers
        // its name. Its size and bytes are charged before any of it is built (#867 review).
        let unregistered = vec![Some(String::new())];
        let prefixes = |name: &str| self.services.get(name).unwrap_or(&unregistered);
        let mut sizes: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for (name, registered) in &self.services {
            check()?;
            charge(work, registered.len())?;
            let readable = registered.iter().flatten();
            sizes.insert(
                name,
                (
                    readable.clone().count(),
                    readable.map(String::len).sum::<usize>(),
                ),
            );
        }
        let mut cost = self.attributed.len();
        for attributed in &self.attributed {
            let (count, bytes) = sizes
                .get(attributed.name.as_str())
                .copied()
                .unwrap_or((1, 0));
            cost = cost
                .saturating_add(count.saturating_mul(attributed.path.len().saturating_add(2)))
                .saturating_add(bytes);
        }
        charge(work, cost)?;
        for attributed in &self.attributed {
            let mut seen = BTreeSet::new();
            for prefix in prefixes(&attributed.name).iter().flatten() {
                check()?;
                let Some(template) = normalize(&join(prefix, &attributed.path)) else {
                    continue;
                };
                if !seen.insert(template.clone()) {
                    continue;
                }
                let mut route = attributed.found.route.clone();
                route.method = attributed.method.into();
                route.segments = segments(&template);
                route.path_template = template;
                found.push(Found {
                    path: attributed.found.path.clone(),
                    start: attributed.found.start,
                    route,
                });
            }
        }
        Ok(())
    }
}

/// The root of a builder chain: `web::scope(p)`, `web::resource(p)`, `App::new()`, or
/// anything else, whose mount point is unknown.
#[derive(Clone)]
enum Root {
    Scope(Option<String>),
    Resource(Option<String>),
    App,
    Unknown,
}

struct Walker<'d, 'a> {
    document: &'d Document<'a>,
    roots: HashMap<usize, Root>,
}

/// Collects one document's actix-web registrations into `collected` and its builder routes
/// into `found`.
pub(super) fn collect(
    document: &Document<'_>,
    collected: &mut Collected,
    found: &mut Vec<Found>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut walker = Walker {
        document,
        roots: HashMap::new(),
    };
    walker.walk(collected, found, check)
}

impl<'a> Walker<'_, 'a> {
    fn text(&self, node: Node<'_>) -> &'a str {
        self.document.text(node)
    }

    /// A pre-order walk that carries the scope prefix of each node: arguments of a call on a
    /// `web::scope(p)` chain are under `p`. `None` is under a scope that cannot be read.
    fn walk(
        &mut self,
        collected: &mut Collected,
        found: &mut Vec<Found>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut prefixes: Vec<Option<String>> = vec![Some(String::new())];
        let mut stack = vec![(self.document.root, 0usize, 0usize)];
        let mut visits = 0;
        while let Some((node, depth, prefix)) = stack.pop() {
            check()?;
            visits += 1;
            if visits > VISITS {
                break;
            }
            if depth > DEPTH || node.is_error() {
                continue;
            }
            let mut inner = prefix;
            match node.kind() {
                "function_item" => self.attributes(node, collected, check)?,
                "call_expression" => {
                    if let Some(scoped) =
                        self.call(node, &prefixes[prefix], collected, found, check)?
                    {
                        prefixes.push(scoped);
                        inner = prefixes.len() - 1;
                    }
                }
                _ => {}
            }
            let arguments = (node.kind() == "call_expression")
                .then(|| node.child_by_field_name("arguments"))
                .flatten();
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                let under = if Some(child) == arguments {
                    inner
                } else {
                    prefix
                };
                stack.push((child, depth + 1, under));
            }
        }
        Ok(())
    }

    /// Reads one call. Returns the prefix its arguments are under when its receiver chain
    /// starts at a scope.
    fn call(
        &mut self,
        call: Node<'a>,
        prefix: &Option<String>,
        collected: &mut Collected,
        found: &mut Vec<Found>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Option<String>>> {
        let Some(function) = call
            .child_by_field_name("function")
            .filter(|function| function.kind() == "field_expression")
        else {
            return Ok(None);
        };
        let (Some(receiver), Some(field)) = (
            function.child_by_field_name("value"),
            function.child_by_field_name("field"),
        ) else {
            return Ok(None);
        };
        let root = self.root(receiver);
        // An `App` is always the root. A receiver that is not a proven chain, such as a
        // variable bound to a scope or the `cfg` of a `configure` callback, is mounted at an
        // unknown prefix, so nothing registers under it (#867 review).
        let scoped = match (&root, prefix) {
            (Root::App, _) => Some(String::new()),
            (Root::Scope(Some(scope)), Some(prefix)) => Some(join(prefix, scope)),
            (Root::Resource(_), Some(prefix)) => Some(prefix.clone()),
            _ => None,
        };
        let arguments = Document::arguments(call);
        match (self.text(field), &arguments[..], &root) {
            ("route", [path_node, route], Root::Scope(_) | Root::App) => {
                if let (Some(scoped), Some(path)) = (&scoped, self.document.literal(*path_node)) {
                    let at = path_node.start_byte();
                    self.builder(call, at, &join(scoped, &path), *route, found, check)?;
                }
            }
            ("route", [route], Root::Resource(path)) => {
                if let (Some(scoped), Some(path)) = (&scoped, path) {
                    let at = route.start_byte();
                    self.builder(call, at, &join(scoped, path), *route, found, check)?;
                }
            }
            ("service", [service], _)
                if matches!(service.kind(), "identifier" | "scoped_identifier")
                    && !self.document.excluded(call) =>
            {
                let name = self.text(*service).rsplit("::").next().unwrap_or("").trim();
                collected
                    .services
                    .entry(name.to_owned())
                    .or_default()
                    .push(scoped.clone());
            }
            _ => {}
        }
        // `.configure(..)` is not followed: its callback is mounted at an unknown prefix.
        Ok(match root {
            _ if self.text(field) == "configure" => Some(None),
            Root::Resource(_) => None,
            _ => Some(scoped),
        })
    }

    /// The root of the chain `node` heads, remembered per node so that a chain is read once.
    fn root(&mut self, node: Node<'a>) -> Root {
        let mut chain = Vec::new();
        let mut current = node;
        let root = loop {
            if let Some(root) = self.roots.get(&current.id()) {
                break root.clone();
            }
            chain.push(current.id());
            let Some(function) = (current.kind() == "call_expression")
                .then(|| current.child_by_field_name("function"))
                .flatten()
            else {
                break Root::Unknown;
            };
            if function.kind() == "field_expression" {
                match function.child_by_field_name("value") {
                    Some(value) => current = value,
                    None => break Root::Unknown,
                }
                continue;
            }
            let path: String = self.text(function).split_whitespace().collect();
            if matches!(path.as_str(), "App::new" | "actix_web::App::new")
                && Document::arguments(current).is_empty()
            {
                break Root::App;
            }
            break match (
                self.web_function(function),
                &Document::arguments(current)[..],
            ) {
                (Some("scope"), [path]) => Root::Scope(self.document.literal(*path)),
                (Some("resource"), [path]) => Root::Resource(self.document.literal(*path)),
                _ => Root::Unknown,
            };
        };
        for id in chain {
            self.roots.insert(id, root.clone());
        }
        root
    }

    /// The name of `web::f`, `actix_web::web::f` or a bare `f`.
    fn web_function(&self, function: Node<'a>) -> Option<&'a str> {
        match function.kind() {
            "identifier" => Some(self.text(function)),
            "scoped_identifier" => {
                let path = self.text(function.child_by_field_name("path")?);
                let path: String = path.split_whitespace().collect();
                matches!(path.as_str(), "web" | "actix_web::web")
                    .then(|| function.child_by_field_name("name"))
                    .flatten()
                    .map(|name| self.text(name))
            }
            _ => None,
        }
    }

    /// The method and handler of `web::get().to(h)`, `web::method(Method::M).to(h)` or
    /// `web::route().method(Method::M).to(h)`, with any other link such as `.guard(..)`
    /// between. `None` when the method cannot be read.
    fn route_expression(&self, route: Node<'a>) -> Option<(&'static str, Node<'a>)> {
        let to = route.child_by_field_name("function")?;
        if to.kind() != "field_expression" || self.text(to.child_by_field_name("field")?) != "to" {
            return None;
        }
        let [handler] = Document::arguments(route)[..] else {
            return None;
        };
        let mut method = None;
        let mut node = to.child_by_field_name("value")?;
        loop {
            if node.kind() != "call_expression" {
                return None;
            }
            let function = node.child_by_field_name("function")?;
            let arguments = Document::arguments(node);
            let verb = |argument: &Node<'a>| {
                path_verb(&self.text(*argument).split_whitespace().collect::<String>())
            };
            if function.kind() == "field_expression" {
                if self.text(function.child_by_field_name("field")?) == "method"
                    && let [argument] = &arguments[..]
                {
                    method = method.or(verb(argument));
                }
                node = function.child_by_field_name("value")?;
                continue;
            }
            let name = self.web_function(function)?;
            let base = match (name, &arguments[..]) {
                ("route", []) => None,
                ("method", [argument]) => verb(argument),
                (name, []) => Some(router_verb(name)?),
                _ => return None,
            };
            return Some((base.or(method)?, handler));
        }
    }

    /// One `.route(..)` builder registration at `path`.
    fn builder(
        &self,
        call: Node<'a>,
        start: usize,
        path: &str,
        route: Node<'a>,
        found: &mut Vec<Found>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let Some((method, handler)) = self.route_expression(route) else {
            return Ok(());
        };
        let Some(template) = normalize(path) else {
            return Ok(());
        };
        if self.document.excluded(call) || self.document.serves_asset(handler, check)? {
            return Ok(());
        }
        let handler = match self.document.function(handler) {
            Some(function) => self.document.declaration_handler(function, None),
            None => {
                self.document
                    .span_handler(HandlerKind::Closure, Document::range(handler), None)
            }
        };
        found.push(Found {
            path: self.document.doc.key.path.clone(),
            start,
            route: Route {
                method: method.into(),
                segments: segments(&template),
                path_template: template,
                from_table: false,
                handler,
            },
        });
        Ok(())
    }

    /// The route attributes on a function: `#[get("/p")]`, `#[actix_web::post("/p")]` and
    /// `#[route("/p", method = "GET", method = "POST")]`.
    fn attributes(
        &self,
        function: Node<'a>,
        collected: &mut Collected,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut routes = Vec::new();
        let mut previous = function.prev_named_sibling();
        while let Some(sibling) = previous {
            if sibling.kind() == "attribute_item" {
                if let Some(read) = self.attribute(sibling) {
                    routes.push((sibling.start_byte(), read));
                }
            } else if !crate::interfaces::comment(sibling) {
                break;
            }
            previous = sibling.prev_named_sibling();
        }
        if routes.is_empty() || self.document.excluded(function) {
            return Ok(());
        }
        let Some(name) = function
            .child_by_field_name("name")
            .map(|name| self.text(name))
        else {
            return Ok(());
        };
        if self.document.embeds(function, check)? {
            return Ok(());
        }
        let span = Document::range(function);
        let declaration = self.document.doc.declarations.iter().find(|d| {
            d.kind == "function"
                && d.name.as_deref() == Some(name)
                && span.start <= d.range.start
                && d.range.end <= span.end
        });
        let handler = match declaration {
            Some(function) => self.document.declaration_handler(function, None),
            None => self.document.span_handler(HandlerKind::Closure, span, None),
        };
        routes.reverse();
        for (start, (methods, path)) in routes {
            for method in methods {
                collected.attributed.push(Attributed {
                    name: name.to_owned(),
                    method,
                    path: path.clone(),
                    found: Found {
                        path: self.document.doc.key.path.clone(),
                        start,
                        route: Route {
                            method: method.into(),
                            path_template: String::new(),
                            segments: Vec::new(),
                            from_table: false,
                            handler: handler.clone(),
                        },
                    },
                });
            }
        }
        Ok(())
    }

    /// The methods and path of one route attribute.
    fn attribute(&self, item: Node<'a>) -> Option<(Vec<&'static str>, String)> {
        let attribute = Document::named_children(item).into_iter().next()?;
        let mut children = Document::named_children(attribute).into_iter();
        let path = children.next()?;
        let name = match path.kind() {
            "identifier" => self.text(path),
            "scoped_identifier" => {
                let qualifier: String = self
                    .text(path.child_by_field_name("path")?)
                    .split_whitespace()
                    .collect();
                if qualifier != "actix_web" {
                    return None;
                }
                self.text(path.child_by_field_name("name")?)
            }
            _ => return None,
        };
        let arguments = children.find(|child| child.kind() == "token_tree")?;
        let tokens = Document::named_children(arguments);
        let literal = self.document.literal(*tokens.first()?)?;
        if name == "route" {
            let mut methods = Vec::new();
            let mut cursor = arguments.walk();
            let all: Vec<_> = arguments.children(&mut cursor).collect();
            for window in all.windows(3) {
                if window[0].kind() == "identifier"
                    && self.text(window[0]) == "method"
                    && self.text(window[1]) == "="
                {
                    let method = self.document.literal(window[2])?;
                    methods.push(router_verb(&method.to_ascii_lowercase())?);
                }
            }
            return (!methods.is_empty()).then_some((methods, literal));
        }
        Some((vec![router_verb(name)?], literal))
    }
}

/// `prefix` then `path`, with one `/` between them and a leading `/`.
pub(in crate::interfaces) fn join(prefix: &str, path: &str) -> String {
    let path = path.strip_prefix('/').unwrap_or(path);
    let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
    match (prefix.is_empty(), path.is_empty()) {
        (true, true) => "/".into(),
        (_, true) => format!("{}{prefix}", if prefix.starts_with('/') { "" } else { "/" }),
        _ => {
            let joined = format!("{prefix}/{path}");
            if joined.starts_with('/') {
                joined
            } else {
                format!("/{joined}")
            }
        }
    }
}

/// An actix-web pattern in the #788 template form: `{name:regex}` is `{name}`, and a last
/// segment `{name:.*}`, `{name:.+}` or `{name}*` is `{*name}`. An unbalanced brace is
/// unreadable.
pub(super) fn normalize(pattern: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = pattern.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '{' => {
                let mut depth = 1;
                let mut end = None;
                for (inner, c) in chars.by_ref() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(inner);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let body = &pattern[at + 1..end?];
                let (name, regex) = body.split_once(':').unwrap_or((body, ""));
                let name = name.trim();
                if name.is_empty() {
                    return None;
                }
                let star = chars.next_if(|(_, c)| *c == '*').is_some();
                let rest = chars.peek().is_none() && (star || matches!(regex.trim(), ".*" | ".+"));
                if star && !rest {
                    return None;
                }
                if rest {
                    out.push_str(&format!("{{*{name}}}"));
                } else {
                    out.push_str(&format!("{{{name}}}"));
                }
            }
            '}' => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_normalise_to_the_template_form() {
        for (pattern, template) in [
            ("/users/{id}", Some("/users/{id}")),
            ("/users/{id:\\d+}/posts", Some("/users/{id}/posts")),
            ("/n/{id:\\d{3}}", Some("/n/{id}")),
            ("/files/{tail:.*}", Some("/files/{*tail}")),
            ("/files/{tail:.+}", Some("/files/{*tail}")),
            ("/files/{tail}*", Some("/files/{*tail}")),
            ("/a/{x:.*}/b", Some("/a/{x}/b")),
            ("/bad/{id", None),
            ("/bad/}", None),
            ("/bad/{}", None),
            ("/bad/{x}*/y", None),
        ] {
            assert_eq!(normalize(pattern).as_deref(), template, "{pattern}");
        }
    }

    #[test]
    fn prefixes_join_with_one_slash() {
        assert_eq!(join("", ""), "/");
        assert_eq!(join("", "/x"), "/x");
        assert_eq!(join("/api", ""), "/api");
        assert_eq!(join("/api/", "/x"), "/api/x");
        assert_eq!(join("/api", "x"), "/api/x");
        assert_eq!(join("api", "x"), "/api/x");
    }
}
