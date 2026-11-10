//! A bounded static reader of Python source for interface extraction (#856): the bindings of
//! each name, statically readable strings (literals, f-strings, `%` and `.format` formatting,
//! `+` and implicit concatenation, visible constants), call arguments, and module resolution.
//! Nothing is inferred beyond what is written; anything else is unreadable.
use super::rust_calls::UNKNOWN;
use super::{charge, comment, parse, test_file};
use crate::references::{DocumentInput, Snapshot, work::Work};
use crate::syntax_bounds::{DEPTH, VISITS};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tree_sitter::{Node, Tree};

/// How deep one string is followed through constants and nested expressions.
const EVAL_DEPTH: usize = 32;
/// How deep a binding is followed through imports and module members.
const RESOLVE_DEPTH: usize = 16;

/// Words that make a document a candidate for server routes. A route decorator line is too.
const SERVER_WORDS: [&str; 13] = [
    "flask",
    "Flask",
    "Blueprint",
    "fastapi",
    "FastAPI",
    "APIRouter",
    "django",
    "urlpatterns",
    "ROOT_URLCONF",
    "register_blueprint",
    "include_router",
    ".route(",
    ".api_route(",
];
/// Method decorators such as `@router.get(`.
const DECORATED: [&str; 7] = [
    ".get(",
    ".post(",
    ".put(",
    ".delete(",
    ".patch(",
    ".head(",
    ".options(",
];

/// The named children of a node, comments aside.
pub(super) fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !comment(*child))
        .collect()
}

/// A pre-order walk over named syntax that checks and charges each node. `visit` says
/// whether to descend. Returns `false` when the walk stopped at a bound or skipped a parse
/// error, so that what it saw may be incomplete.
pub(super) fn visit<'t>(
    root: Node<'t>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
    visit: &mut impl FnMut(Node<'t>) -> bool,
) -> Result<bool> {
    let mut complete = true;
    let mut stack = vec![(root, 0usize)];
    let mut visits = 0usize;
    while let Some((node, depth)) = stack.pop() {
        check()?;
        charge(work, 1)?;
        visits += 1;
        if visits > VISITS {
            return Ok(false);
        }
        if depth > DEPTH || node.is_error() || node.is_missing() {
            complete = false;
            continue;
        }
        if visit(node) {
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push((child, depth + 1));
            }
        }
    }
    Ok(complete)
}

/// An import binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Import {
    /// `import a` binds `a`; `import a.b as m` binds `m` to `a.b`.
    Module { level: usize, dotted: String },
    /// `from .a import n` binds `n`; `from a import n as m` binds `m`.
    Member {
        level: usize,
        dotted: String,
        member: String,
    },
}

/// What one binding of a name binds it to.
#[derive(Clone, Debug)]
pub(super) enum Bound<'t> {
    /// `name = value` or `name: T = value`, with one target. `top` when it is a statement of
    /// the module itself.
    Assign {
        value: Node<'t>,
        top: bool,
    },
    /// `name += value`.
    Extend {
        value: Node<'t>,
        top: bool,
    },
    /// `with value as name`.
    With(Node<'t>),
    /// `def name`.
    Def(Node<'t>),
    Import(Import),
    /// Any other binding: a parameter, a `for`, comprehension, `except`, walrus or pattern
    /// target, `global`, a class, a tuple target, and so on.
    Other,
}

/// Every binding of every name in one document. A name bound more than once anywhere in the
/// document (#856 review lessons: any import, parameter, assignment, `for`, comprehension,
/// `with`, `except`, walrus or pattern target) is never read.
pub(super) struct Names<'t> {
    /// Each binding's start, whether it binds in the module's own scope (outside every
    /// function, class and lambda), and what it binds.
    bindings: HashMap<&'t str, Vec<(usize, bool, Bound<'t>)>>,
    /// The starts of the module-scope `from m import *`: each may rebind any name bound
    /// before it.
    stars: Vec<usize>,
    /// Nothing is readable: a star import inside a function or class, a parse error, or a walk
    /// that stopped at a bound.
    hidden: bool,
}

impl<'t> Names<'t> {
    pub(super) fn new(
        root: Node<'t>,
        source: &'t str,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let text = |node: Node<'_>| source.get(node.byte_range()).unwrap_or("");
        let mut found: Vec<(&'t str, usize, Bound<'t>)> = Vec::new();
        let mut stars = Vec::new();
        let mut hidden = false;
        let complete = visit(root, work, check, &mut |node| {
            match node.kind() {
                "assignment" | "augmented_assignment" => {
                    if let Some(left) = node.child_by_field_name("left") {
                        let bound = assigned(node, left, source);
                        targets(left, source, bound, &mut found);
                    }
                }
                "for_statement" | "for_in_clause" => {
                    if let Some(left) = node.child_by_field_name("left") {
                        targets(left, source, Bound::Other, &mut found);
                    }
                }
                "as_pattern" => {
                    if let Some(alias) = node.child_by_field_name("alias") {
                        let target = match named(alias)[..] {
                            [only] => only,
                            _ => alias,
                        };
                        let with = node.parent().is_some_and(|p| p.kind() == "with_item")
                            && target.kind() == "identifier";
                        let bound = match named(node).first() {
                            Some(value) if with => Bound::With(*value),
                            _ => Bound::Other,
                        };
                        targets(target, source, bound, &mut found);
                    }
                }
                "except_clause" => {
                    if let Some(alias) = node.child_by_field_name("alias") {
                        targets(alias, source, Bound::Other, &mut found);
                    }
                }
                "named_expression" | "class_definition" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        targets(name, source, Bound::Other, &mut found);
                    }
                }
                "function_definition" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        targets(name, source, Bound::Def(node), &mut found);
                    }
                }
                "parameters" | "lambda_parameters" => {
                    for parameter in named(node) {
                        let target = match parameter.kind() {
                            "default_parameter" | "typed_default_parameter" => {
                                parameter.child_by_field_name("name")
                            }
                            "typed_parameter" => named(parameter).first().copied(),
                            "identifier"
                            | "list_splat_pattern"
                            | "dictionary_splat_pattern"
                            | "tuple_pattern" => Some(parameter),
                            _ => None,
                        };
                        if let Some(target) = target {
                            targets(target, source, Bound::Other, &mut found);
                        }
                    }
                }
                "import_statement" => {
                    let mut cursor = node.walk();
                    for name in node.children_by_field_name("name", &mut cursor) {
                        match name.kind() {
                            "dotted_name" => {
                                if let Some(first) = named(name).first() {
                                    let import = Import::Module {
                                        level: 0,
                                        dotted: text(*first).to_owned(),
                                    };
                                    targets(*first, source, Bound::Import(import), &mut found);
                                }
                            }
                            "aliased_import" => {
                                let (Some(dotted), Some(alias)) = (
                                    name.child_by_field_name("name"),
                                    name.child_by_field_name("alias"),
                                ) else {
                                    continue;
                                };
                                let import = Import::Module {
                                    level: 0,
                                    dotted: dotted_text(text(dotted)),
                                };
                                targets(alias, source, Bound::Import(import), &mut found);
                            }
                            _ => {}
                        }
                    }
                }
                "import_from_statement" => {
                    let (level, dotted) = match node.child_by_field_name("module_name") {
                        Some(module) if module.kind() == "relative_import" => {
                            let mut level = 0;
                            let mut dotted = String::new();
                            for part in named(module) {
                                match part.kind() {
                                    "import_prefix" => {
                                        level = text(part).matches('.').count();
                                    }
                                    "dotted_name" => dotted = dotted_text(text(part)),
                                    _ => {}
                                }
                            }
                            (level, dotted)
                        }
                        Some(module) => (0, dotted_text(text(module))),
                        None => (0, String::new()),
                    };
                    let mut cursor = node.walk();
                    for name in node.children_by_field_name("name", &mut cursor) {
                        let (member, alias) = match name.kind() {
                            "dotted_name" => (name, name),
                            "aliased_import" => match (
                                name.child_by_field_name("name"),
                                name.child_by_field_name("alias"),
                            ) {
                                (Some(member), Some(alias)) => (member, alias),
                                _ => continue,
                            },
                            _ => continue,
                        };
                        let import = Import::Member {
                            level,
                            dotted: dotted.clone(),
                            member: dotted_text(text(member)),
                        };
                        let target = if alias.kind() == "dotted_name" {
                            named(alias).first().copied().unwrap_or(alias)
                        } else {
                            alias
                        };
                        targets(target, source, Bound::Import(import), &mut found);
                    }
                    if named(node).iter().any(|n| n.kind() == "wildcard_import") {
                        if in_function_or_class(node, usize::MAX) {
                            hidden = true;
                        } else {
                            stars.push(node.start_byte());
                        }
                    }
                }
                "global_statement" | "nonlocal_statement" | "delete_statement" => {
                    for target in named(node) {
                        targets(target, source, Bound::Other, &mut found);
                    }
                }
                "type_alias_statement" => {
                    if let Some(left) = node.child_by_field_name("left") {
                        targets(left, source, Bound::Other, &mut found);
                    }
                }
                "case_pattern" | "type_parameter" => {
                    targets(node, source, Bound::Other, &mut found);
                    return false;
                }
                _ => {}
            }
            true
        })?;
        let mut bindings: HashMap<&'t str, Vec<(usize, bool, Bound<'t>)>> = HashMap::new();
        for (name, at, bound) in found {
            // The binding's own node: a function's name binds in the scope around it.
            let scope = root
                .descendant_for_byte_range(at, at + 1)
                .is_some_and(|node| !in_function_or_class(node, at));
            bindings.entry(name).or_default().push((at, scope, bound));
        }
        Ok(Self {
            bindings,
            stars,
            hidden: hidden || !complete || root.has_error(),
        })
    }

    /// Whether anything may bind the name in this document.
    pub(super) fn bound(&self, name: &str) -> bool {
        self.hidden || !self.stars.is_empty() || self.bindings.contains_key(name)
    }

    /// The only binding of the name, when nothing else in the document can rebind it.
    pub(super) fn single(&self, name: &str) -> Option<&Bound<'t>> {
        self.only(name).map(|(_, bound)| bound)
    }

    /// The only binding of the name, and whether it is in the module's own scope.
    fn only(&self, name: &str) -> Option<(bool, &Bound<'t>)> {
        if self.hidden {
            return None;
        }
        let [(at, scope, bound)] = self.bindings.get(name)?.as_slice() else {
            return None;
        };
        (!self.stars.iter().any(|star| star > at)).then_some((*scope, bound))
    }

    /// The only binding of the name when it binds a module attribute, which another module
    /// can import.
    pub(super) fn exported(&self, name: &str) -> Option<&Bound<'t>> {
        self.only(name)
            .filter(|(scope, _)| *scope)
            .map(|(_, bound)| bound)
    }

    /// The value of a module-level constant: a name bound once in the document, by a
    /// statement of the module itself.
    pub(super) fn constant(&self, name: &str) -> Option<Node<'t>> {
        match self.single(name)? {
            Bound::Assign { value, top: true } => Some(*value),
            _ => None,
        }
    }

    /// The lists a module-level list is built from: one module-level assignment of a list
    /// or tuple, and any module-level `+=` of a list or tuple. Any other binding of the name
    /// makes it unreadable; a `+=` of anything else only adds entries nothing reads.
    pub(super) fn extended(&self, name: &str) -> Option<Vec<Node<'t>>> {
        if self.hidden {
            return None;
        }
        let bindings = self.bindings.get(name)?;
        let mut assigned = None;
        let mut lists = Vec::new();
        for (at, _, bound) in bindings {
            match bound {
                Bound::Assign { value, top: true } if assigned.is_none() => {
                    assigned = Some(*at);
                    lists.insert(0, *value);
                }
                Bound::Extend { value, top: true } => lists.push(*value),
                _ => return None,
            }
        }
        let assigned = assigned?;
        if self.stars.iter().any(|star| *star > assigned) {
            return None;
        }
        Some(
            lists
                .into_iter()
                .filter(|list| matches!(list.kind(), "list" | "tuple"))
                .collect(),
        )
    }

    /// Every name bound exactly once, with its binding.
    pub(super) fn singles(&self) -> Vec<(&'t str, &Bound<'t>)> {
        let mut singles: Vec<_> = self
            .bindings
            .keys()
            .filter_map(|name| Some((*name, self.single(name)?)))
            .collect();
        singles.sort_by_key(|(name, _)| *name);
        singles
    }
}

fn dotted_text(text: &str) -> String {
    text.split_whitespace().collect()
}

/// Whether the node is inside a function, class or lambda body. With `name_at`, the name of
/// a definition starting there binds in the scope around that definition.
fn in_function_or_class(node: Node<'_>, name_at: usize) -> bool {
    let mut current = Some(node);
    while let Some(node) = current {
        if matches!(
            node.kind(),
            "function_definition" | "class_definition" | "lambda"
        ) && node
            .child_by_field_name("name")
            .is_none_or(|name| name.start_byte() != name_at)
        {
            return true;
        }
        current = node.parent();
    }
    false
}

/// What an assignment binds its single identifier target to.
fn assigned<'t>(node: Node<'t>, left: Node<'t>, source: &str) -> Bound<'t> {
    let top = node.parent().is_some_and(|statement| {
        statement.kind() == "expression_statement"
            && statement.parent().is_some_and(|m| m.kind() == "module")
    });
    let value = node.child_by_field_name("right");
    let operator = node
        .child_by_field_name("operator")
        .and_then(|operator| source.get(operator.byte_range()));
    match (node.kind(), left.kind(), value) {
        ("assignment", "identifier", Some(value))
            if !matches!(
                value.kind(),
                "assignment" | "augmented_assignment" | "yield" | "pattern_list"
            ) =>
        {
            Bound::Assign { value, top }
        }
        ("augmented_assignment", "identifier", Some(value)) if operator == Some("+=") => {
            Bound::Extend { value, top }
        }
        _ => Bound::Other,
    }
}

/// Records every identifier a target binds. A lone identifier takes `bound`; the names in a
/// tuple, list, splat or pattern take [`Bound::Other`]. An attribute or subscript target
/// binds no name.
fn targets<'t>(
    node: Node<'t>,
    source: &'t str,
    bound: Bound<'t>,
    found: &mut Vec<(&'t str, usize, Bound<'t>)>,
) {
    let text = |node: Node<'_>| source.get(node.byte_range()).unwrap_or("");
    if node.kind() == "identifier" {
        found.push((text(node), node.start_byte(), bound));
        return;
    }
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "identifier" => found.push((text(node), node.start_byte(), Bound::Other)),
            "attribute" | "subscript" => {}
            _ => stack.extend(named(node)),
        }
    }
}

/// A call argument as read: given, absent, or possibly supplied by a `*` or `**` splat.
#[derive(Clone, Copy)]
pub(super) enum Arg<'t> {
    Given(Node<'t>),
    Absent,
    Unknown,
}

/// The arguments of a call. Positional arguments are read only up to the first `*args`; a
/// `**kwargs` may supply any keyword not written.
pub(super) struct Args<'t> {
    positional: Vec<Node<'t>>,
    open_positional: bool,
    keywords: Vec<(&'t str, Node<'t>)>,
    open_keywords: bool,
}

impl<'t> Args<'t> {
    pub(super) fn of(call: Node<'t>, source: &'t str) -> Option<Self> {
        let list = call
            .child_by_field_name("arguments")
            .filter(|list| list.kind() == "argument_list")?;
        let mut args = Self {
            positional: Vec::new(),
            open_positional: false,
            keywords: Vec::new(),
            open_keywords: false,
        };
        for argument in named(list) {
            match argument.kind() {
                "list_splat" => args.open_positional = true,
                "dictionary_splat" => args.open_keywords = true,
                "keyword_argument" => {
                    if let (Some(name), Some(value)) = (
                        argument.child_by_field_name("name"),
                        argument.child_by_field_name("value"),
                    ) {
                        args.keywords
                            .push((source.get(name.byte_range()).unwrap_or(""), value));
                    }
                }
                _ if !args.open_positional => args.positional.push(argument),
                _ => {}
            }
        }
        Some(args)
    }

    pub(super) fn positional(&self, index: usize) -> Arg<'t> {
        match self.positional.get(index) {
            Some(node) => Arg::Given(*node),
            None if self.open_positional => Arg::Unknown,
            None => Arg::Absent,
        }
    }

    pub(super) fn keyword(&self, name: &str) -> Arg<'t> {
        match self.keywords.iter().find(|(key, _)| *key == name) {
            Some((_, node)) => Arg::Given(*node),
            None if self.open_keywords => Arg::Unknown,
            None => Arg::Absent,
        }
    }

    /// A parameter that may be passed at `index` or as `keyword`.
    pub(super) fn get(&self, keyword: &str, index: Option<usize>) -> Arg<'t> {
        if let Some(index) = index
            && let Some(node) = self.positional.get(index)
        {
            return Arg::Given(*node);
        }
        match self.keyword(keyword) {
            Arg::Absent if index.is_some() && self.open_positional => Arg::Unknown,
            arg => arg,
        }
    }
}

/// Reads strings, constants and imported names in one document.
#[derive(Clone, Copy)]
pub(super) struct Reader<'n, 't> {
    pub(super) source: &'t str,
    pub(super) names: &'n Names<'t>,
}

impl<'n, 't> Reader<'n, 't> {
    pub(super) fn text(&self, node: Node<'_>) -> &'t str {
        self.source.get(node.byte_range()).unwrap_or("")
    }

    /// The external module and member a name or `module.member` reads as: `Flask` after
    /// `from flask import Flask`, or `flask.Flask` after `import flask`.
    pub(super) fn external(&self, node: Node<'_>) -> Option<(&'n str, &'n str)> {
        match node.kind() {
            "identifier" => match self.names.single(self.text(node))? {
                Bound::Import(Import::Member {
                    level: 0,
                    dotted,
                    member,
                }) => Some((dotted.as_str(), member.as_str())),
                _ => None,
            },
            "attribute" => {
                let object = node.child_by_field_name("object")?;
                let attribute = node.child_by_field_name("attribute")?;
                Some((self.module(object)?, self.text(attribute)))
            }
            _ => None,
        }
    }

    /// The external module an identifier is bound to by `import m` or `import a.b as m`.
    pub(super) fn module(&self, node: Node<'_>) -> Option<&'n str> {
        if node.kind() != "identifier" {
            return None;
        }
        match self.names.single(self.text(node))? {
            Bound::Import(Import::Module { level: 0, dotted }) => Some(dotted.as_str()),
            _ => None,
        }
    }

    /// The string an expression reads as, with [`UNKNOWN`] for each substitution that cannot
    /// be read. `None` when it is not a readable string.
    pub(super) fn string(
        &self,
        node: Node<'t>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        self.eval(node, 0, work, check)
    }

    /// A string with no unread substitution.
    pub(super) fn exact(
        &self,
        node: Node<'t>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        Ok(self
            .string(node, work, check)?
            .filter(|text| !text.contains(UNKNOWN)))
    }

    fn eval(
        &self,
        node: Node<'t>,
        depth: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        check()?;
        if depth > EVAL_DEPTH {
            return Ok(None);
        }
        let depth = depth + 1;
        match node.kind() {
            "string" => self.literal(node, depth, work, check),
            "concatenated_string" => {
                let mut out = String::new();
                for part in named(node) {
                    let Some(text) = self.eval(part, depth, work, check)? else {
                        return Ok(None);
                    };
                    charge(work, text.len())?;
                    out.push_str(&text);
                }
                Ok(Some(out))
            }
            "parenthesized_expression" => match named(node)[..] {
                [inner] => self.eval(inner, depth, work, check),
                _ => Ok(None),
            },
            "identifier" => match self.names.constant(self.text(node)) {
                Some(value) => self.eval(value, depth, work, check),
                None => Ok(None),
            },
            "binary_operator" => {
                let (Some(left), Some(right), Some(operator)) = (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                    node.child_by_field_name("operator"),
                ) else {
                    return Ok(None);
                };
                match operator.kind() {
                    "+" => {
                        let left = self.eval(left, depth, work, check)?;
                        let right = self.eval(right, depth, work, check)?;
                        if left.is_none() && right.is_none() {
                            return Ok(None);
                        }
                        let unknown = || UNKNOWN.to_string();
                        let left = left.unwrap_or_else(unknown);
                        let right = right.unwrap_or_else(unknown);
                        charge(work, left.len().saturating_add(right.len()))?;
                        Ok(Some(left + &right))
                    }
                    "%" => self.percent(left, right, depth, work, check),
                    _ => Ok(None),
                }
            }
            "call" => self.format_call(node, depth, work, check),
            _ => Ok(None),
        }
    }

    /// A string literal. An f-string's substitution is read where it has no conversion,
    /// format specification or `=`, and is [`UNKNOWN`] otherwise. A bytes literal, or an
    /// escape sequence outside a raw string, is unreadable.
    fn literal(
        &self,
        node: Node<'t>,
        depth: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        charge(work, node.byte_range().len())?;
        let children = named(node);
        let Some(start) = children.first().filter(|c| c.kind() == "string_start") else {
            return Ok(None);
        };
        let prefix = self
            .text(*start)
            .trim_end_matches(['"', '\''])
            .to_ascii_lowercase();
        if !prefix.chars().all(|c| matches!(c, 'r' | 'u' | 'f')) {
            return Ok(None);
        }
        let raw = prefix.contains('r');
        let formatted = prefix.contains('f');
        let mut out = String::new();
        for child in &children[1..] {
            match child.kind() {
                "string_end" => {}
                "string_content" => {
                    let mut at = child.start_byte();
                    let mut text = String::new();
                    for part in named(*child) {
                        match part.kind() {
                            "escape_sequence" if !raw => return Ok(None),
                            "escape_interpolation" if formatted => {
                                text.push_str(self.source.get(at..part.start_byte()).unwrap_or(""));
                                text.push_str(self.text(part).get(..1).unwrap_or(""));
                                at = part.end_byte();
                            }
                            _ => {}
                        }
                    }
                    text.push_str(self.source.get(at..child.end_byte()).unwrap_or(""));
                    if text.contains(UNKNOWN) {
                        return Ok(None);
                    }
                    out.push_str(&text);
                }
                "interpolation" if formatted => {
                    check()?;
                    // `{`, the expression and `}`: anything more is a conversion, a format
                    // specification or `=`, which change the text.
                    let value = match child.child_by_field_name("expression") {
                        Some(expression) if child.child_count() == 3 => {
                            self.eval(expression, depth, work, check)?
                        }
                        _ => None,
                    };
                    match value {
                        Some(text) => {
                            charge(work, text.len())?;
                            out.push_str(&text);
                        }
                        None => out.push(UNKNOWN),
                    }
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(out))
    }

    /// `template % values`. A `%s` without flags, width or precision takes its readable
    /// string value; any other conversion is [`UNKNOWN`].
    fn percent(
        &self,
        left: Node<'t>,
        right: Node<'t>,
        depth: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        enum Values<'t> {
            Tuple(Vec<Node<'t>>),
            Mapping(Vec<(String, Node<'t>)>),
            Single(Node<'t>),
        }
        let Some(template) = self.eval(left, depth, work, check)? else {
            return Ok(None);
        };
        let mut right = right;
        while right.kind() == "parenthesized_expression"
            && let [inner] = named(right)[..]
        {
            right = inner;
        }
        let values = match right.kind() {
            "tuple" => Values::Tuple(
                named(right)
                    .into_iter()
                    .take_while(|item| item.kind() != "list_splat")
                    .collect(),
            ),
            "dictionary" => {
                // A splat or an unreadable key may supply any key written before it.
                let mut map: Vec<(String, Node<'t>)> = Vec::new();
                for entry in named(right) {
                    let pair = (entry.kind() == "pair")
                        .then(|| {
                            Some((
                                entry.child_by_field_name("key")?,
                                entry.child_by_field_name("value")?,
                            ))
                        })
                        .flatten();
                    let key = match pair {
                        Some((key, _)) => self.exact(key, work, check)?,
                        None => None,
                    };
                    match (key, pair) {
                        (Some(key), Some((_, value))) => {
                            map.retain(|(written, _)| *written != key);
                            map.push((key, value));
                        }
                        _ => map.clear(),
                    }
                }
                Values::Mapping(map)
            }
            _ => Values::Single(right),
        };
        let mut out = String::new();
        let mut next = 0usize;
        let mut single_read = false;
        let mut conversions = 0usize;
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            if chars.next_if_eq(&'%').is_some() {
                out.push('%');
                continue;
            }
            check()?;
            let key = if chars.next_if_eq(&'(').is_some() {
                let mut key = String::new();
                loop {
                    match chars.next() {
                        None => return Ok(None),
                        Some(')') => break,
                        Some(c) => key.push(c),
                    }
                }
                Some(key)
            } else {
                None
            };
            let mut spec = String::new();
            while let Some(c) = chars.next_if(|c| c.is_ascii_digit() || "#0- +.*hlL".contains(*c)) {
                spec.push(c);
            }
            let Some(conversion) = chars.next() else {
                return Ok(None);
            };
            if !"diouxXeEfFgGcrsa".contains(conversion) {
                return Ok(None);
            }
            conversions += 1;
            let stars = spec.matches('*').count();
            let value = match (&key, &values) {
                (Some(key), Values::Mapping(map)) => map
                    .iter()
                    .find(|(written, _)| written == key)
                    .map(|(_, value)| *value),
                (None, Values::Tuple(items)) => {
                    let at = next + stars;
                    next = at + 1;
                    items.get(at).copied()
                }
                (None, Values::Single(value)) => {
                    next += 1 + stars;
                    (next == 1).then_some(*value)
                }
                _ => None,
            };
            let text = match value {
                Some(value) if conversion == 's' && spec.is_empty() => {
                    self.eval(value, depth, work, check)?
                }
                _ => None,
            };
            match text {
                Some(text) => {
                    single_read |= matches!(values, Values::Single(_));
                    charge(work, text.len())?;
                    out.push_str(&text);
                }
                None => out.push(UNKNOWN),
            }
        }
        // One string formats exactly one conversion; any other count raises.
        if single_read && conversions != 1 {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// `template.format(..)`. A replacement field with a conversion or a format
    /// specification, an attribute or index, or an argument a splat may supply is
    /// [`UNKNOWN`].
    fn format_call(
        &self,
        call: Node<'t>,
        depth: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        let Some(function) = call
            .child_by_field_name("function")
            .filter(|function| function.kind() == "attribute")
        else {
            return Ok(None);
        };
        let (Some(object), Some(attribute)) = (
            function.child_by_field_name("object"),
            function.child_by_field_name("attribute"),
        ) else {
            return Ok(None);
        };
        if self.text(attribute) != "format" {
            return Ok(None);
        }
        let Some(args) = Args::of(call, self.source) else {
            return Ok(None);
        };
        let Some(template) = self.eval(object, depth, work, check)? else {
            return Ok(None);
        };
        let mut out = String::new();
        let mut auto = 0usize;
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.next_if_eq(&'{').is_some() => out.push('{'),
                '}' if chars.next_if_eq(&'}').is_some() => out.push('}'),
                '}' => return Ok(None),
                '{' => {
                    check()?;
                    let mut field = String::new();
                    let mut nesting = 0usize;
                    loop {
                        match chars.next() {
                            None => return Ok(None),
                            Some('}') if nesting == 0 => break,
                            Some('}') => {
                                nesting -= 1;
                                field.push('}');
                            }
                            Some('{') => {
                                nesting += 1;
                                field.push('{');
                            }
                            Some(c) => field.push(c),
                        }
                    }
                    let end = field.find(['!', ':']).unwrap_or(field.len());
                    let (name, rest) = field.split_at(end);
                    let argument = if name.is_empty() {
                        auto += 1;
                        args.positional(auto - 1)
                    } else if let Ok(index) = name.parse::<usize>() {
                        args.positional(index)
                    } else if name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        args.keyword(name)
                    } else {
                        Arg::Unknown
                    };
                    let text = match argument {
                        Arg::Given(value) if rest.is_empty() => {
                            self.eval(value, depth, work, check)?
                        }
                        _ => None,
                    };
                    match text {
                        Some(text) => {
                            charge(work, text.len())?;
                            out.push_str(&text);
                        }
                        None => out.push(UNKNOWN),
                    }
                }
                c => out.push(c),
            }
        }
        Ok(Some(out))
    }
}

/// One parsed production Python document.
pub(super) struct Doc<'t> {
    pub(super) input: &'t DocumentInput,
    pub(super) root: Node<'t>,
    pub(super) names: Names<'t>,
    /// May register server routes.
    pub(super) server: bool,
    /// May make `requests` or `httpx` calls.
    pub(super) client: bool,
}

impl<'t> Doc<'t> {
    pub(super) fn reader(&self) -> Reader<'_, 't> {
        Reader {
            source: &self.input.file.text,
            names: &self.names,
        }
    }
}

/// The parse trees of the candidate Python documents of a snapshot.
pub(super) struct Sources {
    trees: Vec<(usize, Tree, bool, bool)>,
}

/// Whether the document is production Python with evidence.
fn production(snapshot: &Snapshot, doc: &DocumentInput) -> bool {
    doc.key.language == "python"
        && !snapshot.evidence_gaps.contains_key(&doc.key)
        && !test_file(doc)
}

impl Sources {
    /// Each production Python document is scanned for candidate words (charged), and parsed
    /// (charged again) when it may register routes, make client calls, or re-export names as
    /// a package `__init__.py`.
    pub(super) fn new(
        snapshot: &Snapshot,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut trees = Vec::new();
        for (at, doc) in snapshot.documents.iter().enumerate() {
            check()?;
            if !production(snapshot, doc) {
                continue;
            }
            let text = &doc.file.text;
            charge(work, text.len())?;
            let server = SERVER_WORDS.iter().any(|word| text.contains(word))
                || text.lines().any(|line| {
                    line.trim_start().starts_with('@')
                        && DECORATED.iter().any(|verb| line.contains(verb))
                });
            let client = text.contains("requests") || text.contains("httpx");
            let package = doc.key.path == "__init__.py" || doc.key.path.ends_with("/__init__.py");
            if !(server || client || package) {
                continue;
            }
            charge(work, text.len())?;
            let tree = parse(doc, &tree_sitter_python::LANGUAGE.into(), check)?;
            trees.push((at, tree, server, client));
        }
        Ok(Self { trees })
    }
}

/// The production Python documents of a snapshot, the parsed ones with their bindings.
pub(super) struct Python<'t> {
    pub(super) docs: Vec<Doc<'t>>,
    /// Every production Python document path.
    paths: BTreeSet<&'t str>,
    /// The parsed ones, as indexes into `docs`.
    parsed: BTreeMap<&'t str, usize>,
}

/// A name resolved through the bindings of the documents.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Target {
    /// A module, by its path without extension, such as `pkg/views`.
    Module(String),
    /// A name bound once in a parsed document.
    Binding(usize, String),
}

impl<'t> Python<'t> {
    pub(super) fn new(
        snapshot: &'t Snapshot,
        sources: &'t Sources,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let paths = snapshot
            .documents
            .iter()
            .filter(|doc| production(snapshot, doc))
            .map(|doc| doc.key.path.as_str())
            .collect();
        let mut docs = Vec::new();
        let mut parsed = BTreeMap::new();
        for (at, tree, server, client) in &sources.trees {
            let input = &snapshot.documents[*at];
            let root = tree.root_node();
            let names = Names::new(root, &input.file.text, work, check)?;
            parsed.insert(input.key.path.as_str(), docs.len());
            docs.push(Doc {
                input,
                root,
                names,
                server: *server,
                client: *client,
            });
        }
        Ok(Self {
            docs,
            paths,
            parsed,
        })
    }

    /// The document path of a module: `m.py` or `m/__init__.py`, when exactly one exists.
    fn module_path(&self, module: &str) -> Option<&'t str> {
        let file = format!("{module}.py");
        let package = if module.is_empty() {
            "__init__.py".to_owned()
        } else {
            format!("{module}/__init__.py")
        };
        match (
            self.paths.get(file.as_str()),
            self.paths.get(package.as_str()),
        ) {
            (Some(path), None) | (None, Some(path)) => Some(path),
            _ => None,
        }
    }

    /// The parsed document of a module.
    pub(super) fn module_doc(&self, module: &str) -> Option<usize> {
        self.parsed.get(self.module_path(module)?).copied()
    }

    /// Whether a module or a package directory with Python documents exists.
    fn module_exists(&self, module: &str) -> bool {
        if self.module_path(module).is_some() {
            return true;
        }
        let prefix = format!("{module}/");
        self.paths
            .range(prefix.as_str()..)
            .next()
            .is_some_and(|path| path.starts_with(&prefix))
    }

    /// The module an import in the document at `from` names. A relative import is resolved
    /// from the document's package. An absolute one is resolved from each directory above
    /// the document, and only when exactly one of them holds the module.
    pub(super) fn resolve(&self, from: &str, level: usize, dotted: &str) -> Option<String> {
        let join = |base: &str| {
            let mut path = base.to_owned();
            for part in dotted.split('.').filter(|part| !part.is_empty()) {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(part);
            }
            path
        };
        let parent = |path: &str| -> Option<String> {
            if path.is_empty() {
                return None;
            }
            Some(path.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned())
        };
        let directory = parent(from)?;
        if level > 0 {
            let mut base = directory;
            for _ in 1..level {
                base = parent(&base)?;
            }
            return Some(join(&base));
        }
        if dotted.is_empty() {
            return None;
        }
        let mut found = None;
        let mut base = Some(directory);
        while let Some(directory) = base {
            let candidate = join(&directory);
            if self.module_exists(&candidate) {
                if found.is_some() {
                    return None;
                }
                found = Some(candidate);
            }
            base = parent(&directory);
        }
        found
    }

    /// What a name in a parsed document resolves to: its own single binding, or what the
    /// import that binds it names.
    /// `exported` reads the name as a module attribute, which only a module-scope binding
    /// is.
    fn resolve_name(
        &self,
        doc: usize,
        name: &str,
        exported: bool,
        depth: usize,
        work: &mut Work,
    ) -> Result<Option<Target>> {
        charge(work, 1)?;
        if depth > RESOLVE_DEPTH {
            return Ok(None);
        }
        let from = self.docs[doc].input.key.path.as_str();
        let names = &self.docs[doc].names;
        let bound = if exported {
            names.exported(name)
        } else {
            names.single(name)
        };
        Ok(match bound {
            Some(Bound::Import(Import::Module { level, dotted })) => {
                self.resolve(from, *level, dotted).map(Target::Module)
            }
            Some(Bound::Import(Import::Member {
                level,
                dotted,
                member,
            })) => match self.resolve(from, *level, dotted) {
                Some(module) => self.member(&module, member, depth + 1, work)?,
                None => None,
            },
            Some(Bound::Assign { .. } | Bound::Def(_) | Bound::With(_)) => {
                Some(Target::Binding(doc, name.to_owned()))
            }
            _ => None,
        })
    }

    /// A module's member: a name the module binds, else a submodule.
    pub(super) fn member(
        &self,
        module: &str,
        name: &str,
        depth: usize,
        work: &mut Work,
    ) -> Result<Option<Target>> {
        if let Some(path) = self.module_path(module)
            && let Some(doc) = self.parsed.get(path)
            && self.docs[*doc].names.bound(name)
        {
            return self.resolve_name(*doc, name, true, depth, work);
        }
        if self
            .module_path(module)
            .is_some_and(|path| !self.parsed.contains_key(path))
        {
            // A module that was not read may bind the name itself.
            return Ok(None);
        }
        let submodule = if module.is_empty() {
            name.to_owned()
        } else {
            format!("{module}/{name}")
        };
        Ok(self
            .module_exists(&submodule)
            .then_some(Target::Module(submodule)))
    }

    /// What a name or `module.member` expression in a parsed document resolves to.
    pub(super) fn resolve_expression(
        &self,
        doc: usize,
        node: Node<'_>,
        work: &mut Work,
    ) -> Result<Option<Target>> {
        let source = &self.docs[doc].input.file.text;
        let text = |node: Node<'_>| source.get(node.byte_range()).unwrap_or("");
        match node.kind() {
            "identifier" => self.resolve_name(doc, text(node), false, 0, work),
            "attribute" => {
                let (Some(object), Some(attribute)) = (
                    node.child_by_field_name("object"),
                    node.child_by_field_name("attribute"),
                ) else {
                    return Ok(None);
                };
                match self.resolve_expression(doc, object, work)? {
                    Some(Target::Module(module)) => self.member(&module, text(attribute), 1, work),
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }
}
