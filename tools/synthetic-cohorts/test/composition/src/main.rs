//! Independent source-AST checks; never reads generator counters or manifest composition labels.
use std::{
    collections::{HashMap, HashSet},
    env, fs, panic,
    path::{Path, PathBuf},
    thread,
};
use tree_sitter::{Node, Parser};

const MIN_BODIES: usize = 64;
const MAX_BODIES: usize = 80;
fn ceil_percent(n: usize, percent: usize) -> usize {
    (n * percent).div_ceil(100)
}
fn text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[node.byte_range()]).expect("source is UTF-8")
}
fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
fn descendants<'a>(node: Node<'a>, f: &mut impl FnMut(Node<'a>)) {
    f(node);
    for child in named(node) {
        descendants(child, f);
    }
}
fn field<'a>(node: Node<'a>, name: &str) -> Option<Node<'a>> {
    node.child_by_field_name(name)
}
fn is_callable(kind: &str) -> bool {
    matches!(
        kind,
        "method_declaration"
            | "constructor_declaration"
            | "function_item"
            | "function_definition"
            | "function_declaration"
            | "method_definition"
            | "generator_function_declaration"
            | "arrow_function"
    )
}
fn is_call(kind: &str) -> bool {
    matches!(
        kind,
        "method_invocation" | "call_expression" | "call" | "object_creation_expression"
    )
}
fn owner_calls<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>) {
    if is_callable(node.kind()) {
        return;
    }
    if is_call(node.kind()) {
        out.push(node);
    }
    for child in named(node) {
        owner_calls(child, out);
    }
}
fn shape(node: Node<'_>, bytes: &[u8], out: &mut String) {
    if node.kind() == "comment" {
        return;
    }
    out.push('(');
    out.push_str(node.kind());
    // Literal values may have named fragment children; normalize the whole literal subtree.
    if matches!(
        node.kind(),
        "string"
            | "string_literal"
            | "interpreted_string_literal"
            | "raw_string_literal"
            | "character_literal"
            | "char_literal"
            | "integer_literal"
            | "decimal_integer_literal"
            | "float_literal"
            | "number"
    ) {
        out.push(')');
        return;
    }
    if node.child_count() == 0 {
        let kind = node.kind();
        if node.is_named()
            && !matches!(
                kind,
                "identifier"
                    | "type_identifier"
                    | "property_identifier"
                    | "integer_literal"
                    | "decimal_integer_literal"
                    | "float_literal"
                    | "number"
                    | "string"
                    | "string_literal"
                    | "true"
                    | "false"
            )
        {
            out.push_str(text(node, bytes));
        } else if !node.is_named() {
            out.push_str(text(node, bytes));
        }
    }
    for i in 0..node.child_count() {
        shape(node.child(i).unwrap(), bytes, out);
    }
    out.push(')');
}
#[derive(Clone)]
struct Import {
    path: String,
    alias: String,
    target: String,
}
struct Fact {
    path: String,
    language: String,
    size: String,
    imports: Vec<Import>,
    exports: HashSet<String>,
    typed: bool,
    relationship: bool,
    root: bool,
    comment_bytes: usize,
    bytes: usize,
}
fn syntax(language: &str) -> tree_sitter::Language {
    match language {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        _ => panic!("unknown language {language}"),
    }
}
fn import_for(
    node: Node<'_>,
    bytes: &[u8],
    language: &str,
    size: &str,
    dir: &str,
) -> Option<Import> {
    let s = text(node, bytes).trim();
    let (module, alias, target) = match language {
        "java" if node.kind() == "import_declaration" => {
            let class = s
                .strip_prefix(&format!("import {size}.java."))?
                .strip_suffix(';')?;
            (class.to_string(), class.to_string(), "f0".to_string())
        }
        "rust" if node.kind() == "use_declaration" => {
            let statement = s.strip_prefix("use crate::")?.strip_suffix(';')?;
            let (module, alias) = statement.rsplit_once("::f0 as ")?;
            (
                module.rsplit("::").next()?.to_string(),
                alias.to_string(),
                "f0".to_string(),
            )
        }
        "python" if node.kind() == "import_statement" => {
            let (module, alias) = s.strip_prefix("import ")?.split_once(" as ")?;
            (module.to_string(), alias.to_string(), "f0".to_string())
        }
        "javascript" if node.kind() == "import_statement" => {
            let path_node = field(node, "source")?;
            let spec = text(path_node, bytes).trim_matches(['\'', '"']);
            let module = spec.strip_prefix("./")?.strip_suffix(".js")?;
            let alias = s.split("f0 as ").nth(1)?.split('}').next()?.trim();
            (module.to_string(), alias.to_string(), "f0".to_string())
        }
        _ => return None,
    };
    assert!(
        module.starts_with(&format!("C{size}"))
            && module
                .chars()
                .skip(size.len() + 1)
                .all(|c| c.is_ascii_digit()),
        "invalid peer module in {s}"
    );
    let extension = match language {
        "java" => "java",
        "rust" => "rs",
        "python" => "py",
        _ => "js",
    };
    let path = format!("{dir}/{module}.{extension}");
    if language == "rust" {
        let qualified = s
            .strip_prefix("use crate::")
            .unwrap()
            .split("::f0 as ")
            .next()
            .unwrap();
        let expected = if dir.ends_with("/rust") {
            module.clone()
        } else {
            format!("{}::{module}", dir.rsplit('/').next().unwrap())
        };
        assert_eq!(
            qualified, expected,
            "Rust use must name the declared local module"
        );
    }
    Some(Import {
        path,
        alias,
        target,
    })
}
fn callee(node: Node<'_>, bytes: &[u8]) -> String {
    let name = field(node, "function").or_else(|| field(node, "name"));
    if let Some(n) = name {
        if node.kind() == "method_invocation" {
            let method = text(n, bytes);
            return field(node, "object").map_or_else(
                || method.to_string(),
                |o| format!("{}.{}", text(o, bytes), method),
            );
        }
        text(n, bytes).to_string()
    } else {
        String::new()
    }
}
fn relationship(node: Node<'_>, bytes: &[u8], lang: &str, declared: &HashSet<String>) -> bool {
    match (lang, node.kind()) {
        ("java", "class_declaration") => ["interfaces", "superclass"]
            .iter()
            .filter_map(|name| field(node, name))
            .any(|n| declared.iter().any(|t| text(n, bytes).contains(t))),
        ("python", "class_definition") => field(node, "superclasses")
            .is_some_and(|n| declared.iter().any(|t| text(n, bytes).contains(t))),
        ("javascript", "class_declaration") => named(node).iter().any(|n| {
            n.kind() == "class_heritage" && declared.iter().any(|t| text(*n, bytes).contains(t))
        }),
        ("rust", "impl_item") => {
            field(node, "trait").is_some_and(|t| declared.contains(text(t, bytes)))
                && field(node, "type").is_some_and(|t| declared.contains(text(t, bytes)))
        }
        _ => false,
    }
}
fn inspect(root: &Path, rel: &str) -> Fact {
    let parts: Vec<_> = rel.split('/').collect();
    let size = parts[0].to_string();
    let language = parts[1].to_string();
    let dir = rel.rsplit_once('/').unwrap().0;
    let root_file = language == "rust" && (rel.ends_with("/lib.rs") || rel.ends_with("/mod.rs"));
    let bytes = fs::read(root.join(rel)).unwrap();
    let mut parser = Parser::new();
    parser.set_language(&syntax(&language)).unwrap();
    let tree = parser.parse(&bytes, None).unwrap();
    let top = tree.root_node();
    assert!(!top.has_error(), "ERROR in {rel}: {}", top.to_sexp());
    descendants(top, &mut |n| {
        assert!(
            !n.is_missing() && n.kind() != "ERROR",
            "MISSING/ERROR {rel} {}",
            n.kind()
        )
    });
    let mut imports = vec![];
    let mut bodies = vec![];
    let mut methods = 0;
    let mut declared = HashSet::new();
    let mut local_targets = HashSet::new();
    let mut exports = HashSet::new();
    let mut comment_bytes = 0;
    descendants(top, &mut |n| {
        if n.kind() == "comment" {
            comment_bytes += n.end_byte() - n.start_byte();
        }
        if n.kind() == "method_declaration" && language == "java" {
            methods += 1;
        }
        if matches!(
            n.kind(),
            "class_declaration"
                | "class_definition"
                | "interface_declaration"
                | "struct_item"
                | "trait_item"
        ) {
            if let Some(name) = field(n, "name") {
                declared.insert(text(name, &bytes).to_string());
            }
        }
        if is_callable(n.kind()) {
            if let Some(body) = field(n, "body") {
                bodies.push(body);
                if let Some(name) = field(n, "name") {
                    let name = text(name, &bytes);
                    local_targets.insert(name.to_string());
                    if name == "f0" {
                        // Public/exported leaf is a source declaration, not a resolved binding.
                        let declaration = text(n, &bytes);
                        let public = match language.as_str() {
                            "java" => declaration.contains("public "),
                            "rust" => declaration.starts_with("pub "),
                            "javascript" => {
                                n.parent().is_some_and(|p| p.kind() == "export_statement")
                            }
                            _ => true,
                        };
                        if public {
                            exports.insert(name.to_string());
                        }
                    }
                }
            }
        }
        let is_import = match language.as_str() {
            "java" => n.kind() == "import_declaration",
            "rust" => n.kind() == "use_declaration",
            "python" => matches!(n.kind(), "import_statement" | "import_from_statement"),
            "javascript" => n.kind() == "import_statement",
            _ => false,
        };
        if is_import {
            imports.push(
                import_for(n, &bytes, &language, &size, dir).unwrap_or_else(|| {
                    panic!(
                        "{rel}: unrecognized or invalid peer import {}",
                        text(n, &bytes)
                    )
                }),
            );
        }
    });
    if root_file {
        assert!(
            bodies.is_empty() && imports.is_empty(),
            "module roots must contain declarations only: {rel}"
        );
        let modules: Vec<_> = named(top)
            .into_iter()
            .filter(|n| n.kind() == "mod_item")
            .collect();
        assert!(
            !modules.is_empty() && modules.len() <= 64,
            "Rust root must declare 1..64 modules: {rel}"
        );
        for module in modules {
            let name = text(field(module, "name").unwrap(), &bytes);
            let path = if rel.ends_with("lib.rs") && name.starts_with('g') {
                root.join(dir).join(name).join("mod.rs")
            } else {
                root.join(dir).join(format!("{name}.rs"))
            };
            assert!(path.is_file(), "Rust module is missing: {}", path.display());
        }
        return Fact {
            path: rel.into(),
            language,
            size,
            imports,
            exports,
            typed: false,
            relationship: false,
            root: true,
            comment_bytes,
            bytes: bytes.len(),
        };
    }
    let (minimum_bodies, maximum_bodies, maximum_java_methods) = if size == "large" {
        (38, 40, 40)
    } else {
        (MIN_BODIES, MAX_BODIES, 72)
    };
    assert!(
        (minimum_bodies..=maximum_bodies).contains(&bodies.len()),
        "{rel}: callable bodies {}",
        bodies.len()
    );
    assert!(
        methods <= maximum_java_methods,
        "{rel}: Java method signatures {methods}"
    );
    assert_eq!(imports.len(), 2, "{rel}: exact two AST peer imports");
    assert_ne!(imports[0].path, imports[1].path, "{rel}: distinct peers");
    assert!(
        imports
            .iter()
            .all(|p| p.path != rel && p.path.starts_with(&format!("{size}/{language}/"))),
        "{rel}: same-language peers only"
    );
    let mut nonleaf = 0;
    let mut imported_callers = 0;
    let mut statement_bytes = 0;
    let mut callfree_bytes = 0;
    let mut shapes = HashMap::<String, usize>::new();
    for body in bodies.iter().copied() {
        let mut calls = vec![];
        owner_calls(body, &mut calls);
        let mut assignments = HashSet::<String>::new();
        for statement in named(body) {
            if statement.kind() == "comment" {
                continue;
            }
            let mut statement_calls = vec![];
            owner_calls(statement, &mut statement_calls);
            let len = statement.end_byte() - statement.start_byte();
            statement_bytes += len;
            if statement_calls.is_empty() {
                callfree_bytes += len;
                if matches!(
                    statement.kind(),
                    "assignment"
                        | "augmented_assignment"
                        | "assignment_expression"
                        | "expression_statement"
                        | "let_declaration"
                        | "local_variable_declaration"
                        | "lexical_declaration"
                ) {
                    let mut normalized = String::new();
                    shape(statement, &bytes, &mut normalized);
                    // Only arithmetic assignments need this no-repeat rule; other call-free statements are bounded by bytes.
                    if text(statement, &bytes).contains(['+', '-', '*', '/', '^', '%']) {
                        assert!(
                            assignments.insert(normalized),
                            "{rel}: repeated call-free arithmetic assignment"
                        );
                    }
                }
            }
        }
        if calls.is_empty() {
            continue;
        }
        nonleaf += 1;
        let callees: Vec<_> = calls.into_iter().map(|c| callee(c, &bytes)).collect();
        assert!(
            callees.len() >= 2,
            "{rel}: non-leaf has fewer than 2 owned AST calls"
        );
        assert!(
            callees
                .iter()
                .any(|name| local_targets.contains(name.as_str())
                    || ["self.", "this.", "super.", "super()."]
                        .iter()
                        .any(|prefix| name
                            .strip_prefix(prefix)
                            .is_some_and(|target| local_targets.contains(target)))),
            "{rel}: non-leaf lacks intra-file f0 call: {callees:?}"
        );
        if callees.iter().any(|name| {
            imports
                .iter()
                .any(|p| name == &p.alias || name == &format!("{}.{}", p.alias, p.target))
        }) {
            imported_callers += 1;
        }
        let mut normalized = String::new();
        shape(body, &bytes, &mut normalized);
        *shapes.entry(normalized).or_default() += 1;
    }
    assert!(
        nonleaf >= ceil_percent(bodies.len(), 90),
        "{rel}: non-leaf floor {nonleaf}/{}",
        bodies.len()
    );
    assert!(
        imported_callers >= 8 && imported_callers >= ceil_percent(nonleaf, 40),
        "{rel}: distinct cross-file calling owners {imported_callers}/{nonleaf}"
    );
    assert!(
        *shapes.values().max().unwrap_or(&0) <= nonleaf * 80 / 100,
        "{rel}: repeated non-leaf AST shape"
    );
    assert!(
        callfree_bytes <= statement_bytes * 15 / 100,
        "{rel}: call-free statement bytes {callfree_bytes}/{statement_bytes}"
    );
    assert!(
        comment_bytes <= bytes.len() * 10 / 100,
        "{rel}: comment padding {comment_bytes}/{}",
        bytes.len()
    );
    assert!(
        !bytes.windows(2).any(|w| w == b"\n\n") && !bytes.windows(2).any(|w| w == b" \n"),
        "{rel}: repeated blank lines or trailing spaces"
    );
    let method_bodies = bodies
        .iter()
        .filter(|b| {
            let parent = b.parent().unwrap();
            matches!(parent.kind(), "method_declaration" | "method_definition")
                || (language == "rust"
                    && parent.kind() == "function_item"
                    && parent.parent().is_some_and(|p| {
                        matches!(p.kind(), "declaration_list")
                            && p.parent().is_some_and(|g| g.kind() == "impl_item")
                    }))
                || (language == "python"
                    && parent.kind() == "function_definition"
                    && parent.parent().is_some_and(|p| {
                        p.parent().is_some_and(|g| g.kind() == "class_definition")
                    }))
        })
        .count();
    let typed = !declared.is_empty() && method_bodies >= 2;
    let mut has_relationship = false;
    descendants(top, &mut |n| {
        if relationship(n, &bytes, &language, &declared) {
            has_relationship = true;
        }
    });
    Fact {
        path: rel.into(),
        language,
        size,
        imports,
        exports,
        typed,
        relationship: typed && has_relationship,
        root: false,
        comment_bytes,
        bytes: bytes.len(),
    }
}
fn paths(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("java" | "rs" | "py" | "js")
            ) {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut result = vec![];
    walk(root, root, &mut result);
    result.sort();
    result
}
fn check_peer_groups(facts: &[Fact], selection: Option<&str>) {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for fact in facts {
        if fact.root {
            continue;
        }
        for peer in &fact.imports {
            adjacency.entry(&fact.path).or_default().push(&peer.path);
            adjacency.entry(&peer.path).or_default().push(&fact.path);
        }
    }
    let mut visited = HashSet::new();
    for fact in facts {
        if fact.root
            || selection.is_some_and(|path| path != fact.path)
            || visited.contains(fact.path.as_str())
        {
            continue;
        }
        let mut pending = vec![fact.path.as_str()];
        let mut component = HashSet::new();
        while let Some(member) = pending.pop() {
            if !component.insert(member) {
                continue;
            }
            for peer in adjacency.get(member).into_iter().flatten() {
                pending.push(peer);
            }
        }
        assert!(
            (3..=5).contains(&component.len()),
            "{}: connected peer group must have 3..5 ordinary files, has {}",
            fact.path,
            component.len()
        );
        visited.extend(component);
    }
}
fn check(root: &Path, selection: Option<&str>) {
    let files = paths(root);
    let selected: Vec<_> = files
        .iter()
        .filter(|p| selection.is_none_or(|wanted| wanted == p.as_str()))
        .collect();
    assert!(!selected.is_empty(), "no generated files selected");
    // Each file parses independently; fan out in order-preserving chunks.
    let workers = thread::available_parallelism().map_or(1, usize::from);
    let facts: Vec<_> = thread::scope(|scope| {
        let parts: Vec<_> = selected
            .chunks(selected.len().div_ceil(workers))
            .map(|part| scope.spawn(|| part.iter().map(|p| inspect(root, p)).collect::<Vec<_>>()))
            .collect();
        parts
            .into_iter()
            .flat_map(|part| {
                part.join()
                    .unwrap_or_else(|panic| panic::resume_unwind(panic))
            })
            .collect()
    });
    let existing: HashSet<_> = files.iter().map(String::as_str).collect();
    let by_path: HashMap<_, _> = facts.iter().map(|f| (f.path.as_str(), f)).collect();
    for fact in &facts {
        if fact.root {
            continue;
        }
        for peer in &fact.imports {
            assert!(
                existing.contains(peer.path.as_str()),
                "{}: imported peer file is missing {}",
                fact.path,
                peer.path
            );
            assert!(
                peer.path.rsplit_once('/').unwrap().0 == fact.path.rsplit_once('/').unwrap().0,
                "{}: peer escapes local directory {}",
                fact.path,
                peer.path
            );
            let selected_peer;
            let peer_fact = if let Some(found) = by_path.get(peer.path.as_str()) {
                *found
            } else {
                selected_peer = inspect(root, &peer.path);
                &selected_peer
            };
            assert!(
                peer_fact.exports.contains(&peer.target),
                "{}: peer {} lacks exported leaf {}",
                fact.path,
                peer.path,
                peer.target
            );
        }
    }
    if let Some(wanted) = selection {
        let selected = &facts[0];
        if !selected.root {
            let dir = wanted.rsplit_once('/').unwrap().0;
            let neighbors: Vec<_> = files
                .iter()
                .filter(|path| path.starts_with(&format!("{dir}/")) && path != &wanted)
                .map(|path| inspect(root, path))
                .collect();
            let mut group_facts = neighbors;
            group_facts.push(inspect(root, wanted));
            check_peer_groups(&group_facts, Some(wanted));
        }
        return;
    }
    check_peer_groups(&facts, None);
    for size in ["small", "medium", "large"] {
        let cell: Vec<_> = facts.iter().filter(|f| f.size == size).collect();
        let expected = match size {
            "small" => 100,
            "medium" => 1000,
            _ => 10000,
        };
        assert_eq!(cell.len(), expected, "{size}: exact file inventory");
        let comment: usize = cell.iter().map(|f| f.comment_bytes).sum();
        let bytes: usize = cell.iter().map(|f| f.bytes).sum();
        assert!(
            comment <= bytes / 10,
            "{size}: cohort-wide comments {comment}/{bytes}"
        );
        let roots = cell.iter().filter(|f| f.root).count();
        assert!(
            roots
                <= match size {
                    "small" => 1,
                    "medium" => 5,
                    _ => 40,
                },
            "{size}: Rust root limit"
        );
        for language in ["java", "rust", "python", "javascript"] {
            let language_files: Vec<_> = cell
                .iter()
                .filter(|f| f.language == language && !f.root)
                .collect();
            let typed = language_files.iter().filter(|f| f.typed).count();
            let relations = language_files.iter().filter(|f| f.relationship).count();
            assert!(
                typed >= ceil_percent(language_files.len(), 10),
                "{size}/{language}: typed-file floor {typed}/{}",
                language_files.len()
            );
            assert!(
                relations >= 1.max(ceil_percent(typed, 10)),
                "{size}/{language}: relationship typed-file floor {relations}/{typed}"
            );
        }
    }
}
fn main() {
    let args: Vec<_> = env::args().collect();
    assert!(
        (2..=3).contains(&args.len()),
        "usage: composition-check <corpus-root> [relative-source-file]"
    );
    check(&PathBuf::from(&args[1]), args.get(2).map(String::as_str));
}
