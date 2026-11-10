//! Java server routes (#857): Spring MVC mapping annotations on `@RestController` and
//! `@Controller` classes, and JAX-RS resource classes, with their class prefixes and the
//! method mappings a class inherits from resolved supertypes.
use super::java::{Document, Super, Views, World, has_modifier};
use super::routes::join;
use super::{ANY, Route, charge, segments, test_span};
use crate::references::work::Work;
use anyhow::Result;
use std::collections::{BTreeSet, HashMap};
use tree_sitter::Node;

const SPRING: &str = "org.springframework.web.bind.annotation";
const STEREOTYPE: &str = "org.springframework.stereotype";
const JAXRS: [&str; 2] = ["javax.ws.rs", "jakarta.ws.rs"];
/// The Spring mapping annotations and the method each names. `None` reads `method = ..`.
const SPRING_MAPPINGS: [(&str, Option<&str>); 6] = [
    ("RequestMapping", None),
    ("GetMapping", Some("GET")),
    ("PostMapping", Some("POST")),
    ("PutMapping", Some("PUT")),
    ("DeleteMapping", Some("DELETE")),
    ("PatchMapping", Some("PATCH")),
];
const VERBS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
/// Supertype levels an inheritance walk follows before the rest is unresolved.
const LEVELS: usize = 32;

/// What one annotation reads as.
enum Read<T> {
    Absent,
    Known(T),
    /// Present, but it cannot be read: nothing registers under it.
    Unknown,
}

/// One entry of a class's inheritance order: a resolved type, or a supertype that may
/// declare anything.
#[derive(Clone, Copy)]
enum Item {
    Known(usize, usize),
    Unresolved,
}

/// A route before its template is joined: found in `path` at `start`.
struct Found {
    path: String,
    start: usize,
    route: Route,
}

/// One method signature of a class, as its inheritance order declares it.
struct Entry<'t> {
    /// The first declaration with a body in a class, else in an interface.
    handler: Option<(usize, Node<'t>)>,
    default_handler: Option<(usize, Node<'t>)>,
    /// The mapping that decides the method, and where it is declared. `None` until a
    /// declaration decides it.
    mapping: Option<Read<(Vec<&'static str>, Vec<String>)>>,
    declared: Option<(usize, Node<'t>)>,
}

/// The server routes of every production Java document, one per method, template and
/// controller document.
pub(super) fn extract(
    world: &World<'_>,
    views: &Views<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Route>> {
    let application = application_path(world, views, work, check)?;
    let mut found = Vec::new();
    for &index in &world.servers {
        check()?;
        let (Some(unit), Some(view)) = (world.units.get(&index), views.get(&index)) else {
            continue;
        };
        charge(work, view.source.len())?;
        for at in 0..unit.types.len() {
            check()?;
            let Some((view, node)) = world.node(views, index, at) else {
                continue;
            };
            if node.kind() != "class_declaration"
                || has_modifier(node, "abstract")
                || test_span(view.doc, &Document::range(node))
            {
                continue;
            }
            let annotations = Document::annotations(node);
            let controller = annotations.iter().any(|annotation| {
                let name = view.annotation_name(*annotation);
                view.resolves(&name, &[SPRING], "RestController")
                    || view.resolves(&name, &[STEREOTYPE], "Controller")
            });
            if controller {
                spring(world, views, index, at, &mut found, work, check)?;
            }
            if let Read::Known(prefix) = &application {
                jaxrs(world, views, index, at, prefix, &mut found, work, check)?;
            }
        }
    }
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

/// The JAX-RS application path: the value of the snapshot's one `@ApplicationPath`, or none.
/// Two different ones, or one that cannot be read, leave every JAX-RS mount unknown.
fn application_path(
    world: &World<'_>,
    views: &Views<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Read<String>> {
    let mut paths = BTreeSet::new();
    for &index in &world.servers {
        let Some(unit) = world.units.get(&index) else {
            continue;
        };
        if !views
            .get(&index)
            .is_some_and(|view| view.source.contains("ApplicationPath"))
        {
            continue;
        }
        for at in 0..unit.types.len() {
            check()?;
            let Some((view, node)) = world.node(views, index, at) else {
                continue;
            };
            if test_span(view.doc, &Document::range(node)) {
                continue;
            }
            for annotation in Document::annotations(node) {
                if !view.resolves(&view.annotation_name(annotation), &JAXRS, "ApplicationPath") {
                    continue;
                }
                match single_value(view, annotation, work, check)? {
                    Some(path) => {
                        let path = path.strip_suffix("/*").unwrap_or(&path).to_owned();
                        paths.insert(path);
                    }
                    None => return Ok(Read::Unknown),
                }
            }
        }
    }
    let mut paths = paths.into_iter();
    Ok(match (paths.next(), paths.next()) {
        (None, _) => Read::Known(String::new()),
        (Some(only), None) => Read::Known(only),
        _ => Read::Unknown,
    })
}

/// The one readable string an annotation's `value` (written or unnamed) carries.
fn single_value(
    view: &Document<'_>,
    annotation: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<String>> {
    let arguments = Document::annotation_arguments(annotation);
    let mut values = arguments.values;
    values.extend(
        arguments
            .pairs
            .iter()
            .filter(|(key, _)| view.text(*key) == "value")
            .map(|(_, value)| *value),
    );
    match values[..] {
        [value] => view.strict(value, work, check),
        _ => Ok(None),
    }
}

/// The inheritance order Spring searches a type's annotations in: the type, its interfaces
/// (each with its own), then its superclass (with its own), each type once.
fn spring_order(world: &World<'_>, index: usize, at: usize) -> Vec<Item> {
    fn visit(
        world: &World<'_>,
        item: Super,
        level: usize,
        seen: &mut BTreeSet<(usize, usize)>,
        order: &mut Vec<Item>,
    ) {
        let (index, at) = match item {
            Super::Object => return,
            Super::Unresolved => return order.push(Item::Unresolved),
            Super::Resolved(index, at) => (index, at),
        };
        if level > LEVELS {
            return order.push(Item::Unresolved);
        }
        if !seen.insert((index, at)) {
            return;
        }
        order.push(Item::Known(index, at));
        let Some(info) = world.units.get(&index).and_then(|unit| unit.types.get(at)) else {
            return;
        };
        for interface in &info.interfaces {
            visit(world, *interface, level + 1, seen, order);
        }
        if let Some(superclass) = info.superclass {
            visit(world, superclass, level + 1, seen, order);
        }
    }
    let mut order = Vec::new();
    visit(
        world,
        Super::Resolved(index, at),
        0,
        &mut BTreeSet::new(),
        &mut order,
    );
    order
}

/// The JAX-RS inheritance order: the class, its superclasses, then its interfaces, since
/// a superclass's annotations take precedence over an interface's.
fn jaxrs_order(world: &World<'_>, index: usize, at: usize) -> Vec<Item> {
    fn visit(
        world: &World<'_>,
        item: Super,
        level: usize,
        seen: &mut BTreeSet<(usize, usize)>,
        order: &mut Vec<Item>,
    ) {
        let (index, at) = match item {
            Super::Object => return,
            Super::Unresolved => return order.push(Item::Unresolved),
            Super::Resolved(index, at) => (index, at),
        };
        if level > LEVELS {
            return order.push(Item::Unresolved);
        }
        if !seen.insert((index, at)) {
            return;
        }
        order.push(Item::Known(index, at));
        let Some(info) = world.units.get(&index).and_then(|unit| unit.types.get(at)) else {
            return;
        };
        if let Some(superclass) = info.superclass {
            visit(world, superclass, level + 1, seen, order);
        }
        for interface in &info.interfaces {
            visit(world, *interface, level + 1, seen, order);
        }
    }
    let mut order = Vec::new();
    visit(
        world,
        Super::Resolved(index, at),
        0,
        &mut BTreeSet::new(),
        &mut order,
    );
    order
}

/// The instance methods a type's body declares, with their signatures.
fn methods<'t>(view: &Document<'t>, node: Node<'t>) -> Vec<(String, Node<'t>)> {
    let Some(body) = node.child_by_field_name("body") else {
        return Vec::new();
    };
    Document::named_children(body)
        .into_iter()
        .filter(|child| child.kind() == "method_declaration" && !has_modifier(*child, "static"))
        .filter_map(|method| Some((signature(view, method)?, method)))
        .collect()
}

/// `name(T1,T2)`, with each parameter type's simple name and no type arguments.
fn signature(view: &Document<'_>, method: Node<'_>) -> Option<String> {
    let name = view.text(method.child_by_field_name("name")?);
    let mut types = Vec::new();
    if let Some(parameters) = method.child_by_field_name("parameters") {
        for parameter in Document::named_children(parameters) {
            let ty = match parameter.kind() {
                "formal_parameter" => parameter.child_by_field_name("type").map(|t| view.text(t)),
                "spread_parameter" => Document::named_children(parameter)
                    .into_iter()
                    .find(|n| !matches!(n.kind(), "modifiers" | "variable_declarator"))
                    .map(|t| view.text(t)),
                _ => None,
            };
            let mut simple = String::new();
            let mut depth = 0usize;
            for c in ty.unwrap_or("?").chars() {
                match c {
                    '<' => depth += 1,
                    '>' => depth = depth.saturating_sub(1),
                    '.' if depth == 0 => simple.clear(),
                    c if c.is_whitespace() => {}
                    c if depth == 0 => simple.push(c),
                    _ => {}
                }
            }
            if parameter.kind() == "spread_parameter" {
                simple.push_str("[]");
            }
            types.push(simple);
        }
    }
    Some(format!("{name}({})", types.join(",")))
}

/// The method's own Spring mapping: the methods it answers and its paths.
fn spring_mapping(
    view: &Document<'_>,
    declaration: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Read<(Vec<&'static str>, Vec<String>)>> {
    let mut found = Vec::new();
    for annotation in Document::annotations(declaration) {
        let name = view.annotation_name(annotation);
        if let Some((_, verb)) = SPRING_MAPPINGS
            .iter()
            .find(|(simple, _)| view.resolves(&name, &[SPRING], simple))
        {
            found.push((annotation, *verb));
        }
    }
    let [(annotation, verb)] = found[..] else {
        return Ok(if found.is_empty() {
            Read::Absent
        } else {
            Read::Unknown
        });
    };
    let Some(paths) = spring_paths(view, annotation, work, check)? else {
        return Ok(Read::Unknown);
    };
    let methods = match verb {
        Some(verb) => vec![verb],
        None => match request_methods(view, annotation) {
            Some(methods) => methods,
            None => return Ok(Read::Unknown),
        },
    };
    Ok(Read::Known((methods, paths)))
}

/// The paths of a Spring mapping: its unnamed value, `value` or `path`, each element read
/// completely. No path is the empty path. `None` when any cannot be read or both are given.
fn spring_paths(
    view: &Document<'_>,
    annotation: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Vec<String>>> {
    let arguments = Document::annotation_arguments(annotation);
    let mut given = arguments.values.clone();
    given.extend(
        arguments
            .pairs
            .iter()
            .filter(|(key, _)| matches!(view.text(*key), "value" | "path"))
            .map(|(_, value)| *value),
    );
    let value = match given[..] {
        [] => return Ok(Some(vec![String::new()])),
        [value] => value,
        _ => return Ok(None),
    };
    let elements = Document::elements(value);
    if elements.is_empty() {
        return Ok(Some(vec![String::new()]));
    }
    let mut paths = Vec::with_capacity(elements.len());
    for element in elements {
        match view.strict(element, work, check)? {
            Some(path) => paths.push(path),
            None => return Ok(None),
        }
    }
    Ok(Some(paths))
}

/// The `method` of a `@RequestMapping`: every method when absent, each `RequestMethod.M`
/// when given. `None` when one cannot be read.
fn request_methods(view: &Document<'_>, annotation: Node<'_>) -> Option<Vec<&'static str>> {
    let arguments = Document::annotation_arguments(annotation);
    let mut given = arguments
        .pairs
        .iter()
        .filter(|(key, _)| view.text(*key) == "method");
    let value = match (given.next(), given.next()) {
        (None, _) => return Some(vec![ANY]),
        (Some((_, value)), None) => *value,
        _ => return None,
    };
    let mut methods = Vec::new();
    for element in Document::elements(value) {
        if element.kind() != "field_access" {
            return None;
        }
        let object = view.compact(element.child_by_field_name("object")?);
        let field = view.text(element.child_by_field_name("field")?);
        if !view.resolves(&object, &[SPRING], "RequestMethod") {
            return None;
        }
        let verb = VERBS.iter().copied().find(|verb| *verb == field)?;
        if !methods.contains(&verb) {
            methods.push(verb);
        }
    }
    (!methods.is_empty()).then_some(methods)
}

/// A type's own class-level `@RequestMapping` paths. One that names a method cannot be read.
fn spring_class(
    view: &Document<'_>,
    declaration: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Read<Vec<String>>> {
    let mappings: Vec<Node<'_>> = Document::annotations(declaration)
        .into_iter()
        .filter(|annotation| {
            view.resolves(
                &view.annotation_name(*annotation),
                &[SPRING],
                "RequestMapping",
            )
        })
        .collect();
    let [annotation] = mappings[..] else {
        return Ok(if mappings.is_empty() {
            Read::Absent
        } else {
            Read::Unknown
        });
    };
    let arguments = Document::annotation_arguments(annotation);
    if arguments
        .pairs
        .iter()
        .any(|(key, _)| view.text(*key) == "method")
    {
        return Ok(Read::Unknown);
    }
    Ok(match spring_paths(view, annotation, work, check)? {
        Some(paths) => Read::Known(paths),
        None => Read::Unknown,
    })
}

/// The routes of one Spring controller.
#[allow(clippy::too_many_arguments)]
fn spring(
    world: &World<'_>,
    views: &Views<'_>,
    index: usize,
    at: usize,
    found: &mut Vec<Found>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let order = spring_order(world, index, at);
    charge(work, order.len())?;
    // The class prefix is the first class-level mapping in the order. A supertype that
    // cannot be resolved before it may carry one, so the prefix is unknown.
    let mut prefixes = None;
    for item in &order {
        check()?;
        let Item::Known(index, at) = *item else {
            return Ok(());
        };
        let Some((view, node)) = world.node(views, index, at) else {
            return Ok(());
        };
        match spring_class(view, node, work, check)? {
            Read::Absent => {}
            Read::Known(paths) => {
                prefixes = Some(paths);
                break;
            }
            Read::Unknown => return Ok(()),
        }
    }
    let prefixes = prefixes.unwrap_or_else(|| vec![String::new()]);
    let entries = entries(world, views, &order, work, check, Framework::Spring)?;
    emit(
        views, index, at, world, &prefixes, entries, found, work, check,
    )
}

/// The routes of one JAX-RS resource class: a class with its own `@Path`.
#[allow(clippy::too_many_arguments)]
fn jaxrs(
    world: &World<'_>,
    views: &Views<'_>,
    index: usize,
    at: usize,
    application: &str,
    found: &mut Vec<Found>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let Some((view, node)) = world.node(views, index, at) else {
        return Ok(());
    };
    let paths: Vec<Node<'_>> = Document::annotations(node)
        .into_iter()
        .filter(|annotation| view.resolves(&view.annotation_name(*annotation), &JAXRS, "Path"))
        .collect();
    let [annotation] = paths[..] else {
        return Ok(());
    };
    let Some(class_path) = single_value(view, annotation, work, check)? else {
        return Ok(());
    };
    let order = jaxrs_order(world, index, at);
    charge(work, order.len())?;
    let entries = entries(world, views, &order, work, check, Framework::Jaxrs)?;
    let prefix = join(application, &class_path);
    emit(
        views,
        index,
        at,
        world,
        &[prefix],
        entries,
        found,
        work,
        check,
    )
}

/// A JAX-RS method's own resource method: its HTTP method and `@Path`. `Absent` when neither
/// the method nor a parameter carries a JAX-RS annotation, so it inherits; `Unknown` when
/// it carries some but is not one resource method.
fn jaxrs_method(
    view: &Document<'_>,
    declaration: Node<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Read<(Vec<&'static str>, Vec<String>)>> {
    let annotations = Document::annotations(declaration);
    let own = |annotation: &Node<'_>| view.in_package(&view.annotation_name(*annotation), &JAXRS);
    let mut any = annotations.iter().any(own);
    if let Some(parameters) = declaration.child_by_field_name("parameters") {
        for parameter in Document::named_children(parameters) {
            any |= Document::annotations(parameter).iter().any(own);
        }
    }
    if !any {
        return Ok(Read::Absent);
    }
    let verbs: Vec<&'static str> = annotations
        .iter()
        .filter_map(|annotation| {
            let name = view.annotation_name(*annotation);
            VERBS
                .iter()
                .copied()
                .find(|verb| view.resolves(&name, &JAXRS, verb))
        })
        .collect();
    let [verb] = verbs[..] else {
        return Ok(Read::Unknown);
    };
    let paths: Vec<Node<'_>> = annotations
        .into_iter()
        .filter(|annotation| view.resolves(&view.annotation_name(*annotation), &JAXRS, "Path"))
        .collect();
    let path = match paths[..] {
        [] => String::new(),
        [annotation] => match single_value(view, annotation, work, check)? {
            Some(path) => path,
            None => return Ok(Read::Unknown),
        },
        _ => return Ok(Read::Unknown),
    };
    Ok(Read::Known((vec![verb], vec![path])))
}

/// Whose annotations decide a method.
#[derive(Clone, Copy)]
enum Framework {
    Spring,
    Jaxrs,
}

/// Each method signature of a class in its inheritance order, decided by the first
/// declaration whose own annotations say something. A declaration after a supertype that
/// cannot be resolved is not read, since that supertype may decide the method first.
fn entries<'t>(
    world: &World<'_>,
    views: &'t Views<'t>,
    order: &[Item],
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
    framework: Framework,
) -> Result<Vec<Entry<'t>>> {
    let mut entries: Vec<Entry<'t>> = Vec::new();
    let mut by_signature: HashMap<String, usize> = HashMap::new();
    let mut unresolved = false;
    for item in order {
        let (index, at) = match *item {
            Item::Unresolved => {
                unresolved = true;
                continue;
            }
            Item::Known(index, at) => (index, at),
        };
        let Some((view, node)) = world.node(views, index, at) else {
            unresolved = true;
            continue;
        };
        let class = node.kind() != "interface_declaration";
        let declared = methods(view, node);
        charge(work, declared.len())?;
        for (signature, method) in declared {
            check()?;
            let slot = *by_signature.entry(signature).or_insert_with(|| {
                entries.push(Entry {
                    handler: None,
                    default_handler: None,
                    mapping: None,
                    declared: None,
                });
                entries.len() - 1
            });
            let entry = &mut entries[slot];
            if method.child_by_field_name("body").is_some() {
                let handler = if class {
                    &mut entry.handler
                } else {
                    &mut entry.default_handler
                };
                handler.get_or_insert((index, method));
            }
            if entry.mapping.is_some() {
                continue;
            }
            let read = match framework {
                Framework::Spring => spring_mapping(view, method, work, check)?,
                Framework::Jaxrs => jaxrs_method(view, method, work, check)?,
            };
            if matches!(read, Read::Absent) {
                continue;
            }
            entry.declared = Some((index, method));
            entry.mapping = Some(if unresolved { Read::Unknown } else { read });
        }
    }
    Ok(entries)
}

/// The routes of each known entry under each class prefix, charged before they are built.
#[allow(clippy::too_many_arguments)]
fn emit(
    views: &Views<'_>,
    index: usize,
    at: usize,
    world: &World<'_>,
    prefixes: &[String],
    entries: Vec<Entry<'_>>,
    found: &mut Vec<Found>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let Some((view, class)) = world.node(views, index, at) else {
        return Ok(());
    };
    let prefix_bytes: usize = prefixes.iter().map(String::len).sum();
    let mut cost = 0usize;
    for entry in &entries {
        if let Some(Read::Known((methods, paths))) = &entry.mapping {
            let path_bytes: usize = paths.iter().map(String::len).sum();
            let per_method = prefixes
                .len()
                .saturating_mul(paths.len())
                .saturating_mul(2)
                .saturating_add(prefix_bytes.saturating_mul(paths.len()))
                .saturating_add(path_bytes.saturating_mul(prefixes.len()));
            cost = cost.saturating_add(methods.len().saturating_mul(per_method));
        }
    }
    charge(work, cost)?;
    for entry in entries {
        let Some(Read::Known((methods, paths))) = entry.mapping else {
            continue;
        };
        let Some((handler_index, handler)) =
            entry.handler.or(entry.default_handler).or(entry.declared)
        else {
            continue;
        };
        let Some(handler_view) = views.get(&handler_index) else {
            continue;
        };
        if test_span(handler_view.doc, &Document::range(handler)) {
            continue;
        }
        let handler = handler_view.handler(handler);
        let start = match entry.declared {
            Some((declared, method)) if declared == index => method.start_byte(),
            _ => class.start_byte(),
        };
        for method in &methods {
            for prefix in prefixes {
                for path in &paths {
                    check()?;
                    let Some(template) = template(&join(prefix, path)) else {
                        continue;
                    };
                    found.push(Found {
                        path: view.doc.key.path.clone(),
                        start,
                        route: Route {
                            method: (*method).into(),
                            segments: segments(&template),
                            path_template: template,
                            from_table: false,
                            handler: handler.clone(),
                        },
                    });
                }
            }
        }
    }
    Ok(())
}

/// A Spring or JAX-RS path in the #788 template form. `{name}` and `{name:regex}` are
/// `{name}`. A last segment `{*name}` (Spring), or `{name:.*}` or `{name:.+}` (JAX-RS, whose
/// pattern may span segments), is `{*name}`. A Spring `*`, `**` or `?` wildcard, or an
/// unbalanced brace, makes the path unreadable.
pub(super) fn template(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut out = Vec::with_capacity(parts.len());
    for (at, part) in parts.iter().enumerate() {
        let last = at + 1 == parts.len();
        let mut segment = String::new();
        let mut chars = part.char_indices();
        while let Some((start, c)) = chars.next() {
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
                    let end = end?;
                    let body = &part[start + 1..end];
                    let whole = start == 0 && end + 1 == part.len();
                    if let Some(name) = body.strip_prefix('*') {
                        let name = name.trim();
                        if !(last && whole) || name.is_empty() {
                            return None;
                        }
                        segment.push_str(&format!("{{*{name}}}"));
                        continue;
                    }
                    let (name, regex) = body.split_once(':').unwrap_or((body, ""));
                    let name = name.trim();
                    if name.is_empty() {
                        return None;
                    }
                    if last && whole && matches!(regex.trim(), ".*" | ".+") {
                        segment.push_str(&format!("{{*{name}}}"));
                    } else {
                        segment.push_str(&format!("{{{name}}}"));
                    }
                }
                '}' | '*' | '?' => return None,
                c => segment.push(c),
            }
        }
        out.push(segment);
    }
    Some(out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::template;

    #[test]
    fn paths_normalise_to_the_template_form() {
        for (path, read) in [
            ("/users/{id}", Some("/users/{id}")),
            ("/users/{id:\\d+}/posts", Some("/users/{id}/posts")),
            ("/users/{ id : [0-9]+ }", Some("/users/{id}")),
            ("/n/{id:\\d{3}}", Some("/n/{id}")),
            ("/files/{*path}", Some("/files/{*path}")),
            ("/files/{path: .*}", Some("/files/{*path}")),
            ("/files/{path:.+}", Some("/files/{*path}")),
            ("/a/{x:.*}/b", Some("/a/{x}/b")),
            ("/report-{id}.json", Some("/report-{id}.json")),
            ("/files/{*path}/x", None),
            ("/files/**", None),
            ("/files/*.html", None),
            ("/q?", None),
            ("/bad/{id", None),
            ("/bad/}", None),
            ("/bad/{}", None),
        ] {
            assert_eq!(template(path).as_deref(), read, "{path}");
        }
    }
}
