use trellis::dependency_rust_symbols::extract;

fn declarations(text: &str) -> trellis::dependency_rust_symbols::Declarations {
    extract("pkg:one@1", "demo", "src/lib.rs", text, "source:one").unwrap()
}

#[test]
fn extracts_lexical_declarations_and_keeps_impl_distinct_from_types() {
    let source = r#"
mod nested {
    pub struct Thing<T>(T);
    pub enum Choice { A, B { value: u32 } }
    pub union Bits { a: u32, b: f32 }
    pub trait Work: Sized {
        type Output;
        fn required(&self) -> Self::Output;
        fn defaulted(&self) { hidden_call(); }
    }
    impl<T> Work for Thing<T> {
        type Output = T;
        fn required(&self) -> T { panic!("opaque") }
    }
    impl<T> Thing<T> { pub fn new(value: T) -> Self { Self(value) } }
    pub type Alias = Thing<u32>;
    pub fn free() { nested_call(); }
}
"#;
    let output = declarations(source);
    assert_eq!(output.symbols.len(), 15);
    let symbols = &output.symbols;
    let find = |name: &str, kind: &str| {
        symbols
            .iter()
            .find(|s| s.name == name && s.kind == kind)
            .unwrap()
    };
    let module = find("nested", "module");
    let ty = find("Thing", "struct");
    assert_eq!(ty.qualified_name, "demo::nested::Thing");
    assert_eq!(ty.parent.as_deref(), Some(module.id.as_str()));
    let implementation = find("impl Work for Thing<T>", "impl");
    assert_ne!(implementation.id, ty.id);
    assert_ne!(implementation.qualified_name, ty.qualified_name);
    assert_eq!(implementation.owner_expression.as_deref(), Some("Thing<T>"));
    let method = symbols
        .iter()
        .find(|s| s.name == "required" && s.parent.as_deref() == Some(implementation.id.as_str()))
        .unwrap();
    assert_eq!(method.kind, "method");
    assert_eq!(method.owner_expression.as_deref(), Some("Thing<T>"));
    assert_eq!(find("free", "function").signature, "pub fn free()");
    assert_eq!(
        find("Alias", "type").signature,
        "pub type Alias = Thing<u32>;"
    );
    assert!(
        symbols
            .iter()
            .all(|s| !s.signature.contains("hidden_call") && !s.signature.contains("panic!"))
    );
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("not validated exports"))
    );
}

#[test]
fn skips_nested_declarations_in_all_behavior_and_macro_bodies() {
    let output = declarations(
        r#"
fn top() {
    fn hidden() {}
    struct Local;
    let closure = || { fn closure_hidden() {} };
    if condition() { loop { branch_call(); } }
}
const C: () = { fn const_hidden() {} };
static S: () = { struct StaticHidden; };
macro_rules! definitions { () => { struct MacroHidden; } }
definitions! { fn InvocationHidden() {} }
struct WithConst<const N: usize = { fn generic_hidden() {} 1 }>;
enum Discriminant { One = { fn enum_hidden() {} 1 } }
type WithArray = [u8; { fn array_hidden() {} 1 }];
"#,
    );
    assert_eq!(
        output
            .symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["top", "WithConst", "Discriminant", "WithArray"]
    );
    assert!(
        output
            .symbols
            .iter()
            .all(|s| !s.signature.contains("hidden"))
    );
    let implementation = include_str!("../../src/dependency_rust_symbols.rs");
    for forbidden in [
        "CallSite",
        "ControlRegion",
        "Graph",
        "indexer_rust",
        "Command::",
        "behavior::",
    ] {
        assert!(
            !implementation.contains(forbidden),
            "forbidden behavior dependency: {forbidden}"
        );
    }
}

#[test]
fn large_function_body_does_not_consume_declaration_walk_budget() {
    let source = format!("fn body() {{ {} }}\nstruct After;", "f();".repeat(60_000));
    let output = declarations(&source);
    assert_eq!(output.symbols.len(), 2);
    assert!(!output.warnings.iter().any(|w| w.contains("limit")));
}

#[test]
fn package_source_path_and_span_all_contribute_to_ids() {
    let source = "struct Same; struct Same;";
    let a = extract("registry:one@1", "same", "src/lib.rs", source, "source:a").unwrap();
    let same = extract("registry:one@1", "same", "src/lib.rs", source, "source:a").unwrap();
    let b = extract("registry:one@2", "same", "src/lib.rs", source, "source:a").unwrap();
    let c = extract("registry:one@1", "same", "src/lib.rs", source, "source:b").unwrap();
    let d = extract("registry:one@1", "same", "src/other.rs", source, "source:a").unwrap();
    assert_eq!(a.symbols[0].id, same.symbols[0].id);
    for other in [&a.symbols[1], &b.symbols[0], &c.symbols[0], &d.symbols[0]] {
        assert_ne!(a.symbols[0].id, other.id);
    }
    assert_eq!(a.symbols[0].package_id, "registry:one@1");
    assert_eq!(a.symbols[0].source_ref, "source:a");
    assert_eq!(a.symbols[0].path, "src/lib.rs");
}

#[test]
fn source_layout_and_inline_module_paths_are_only_syntax_candidates() {
    for (path, expected) in [
        ("src/lib.rs", "my_crate::inner::Item"),
        ("src/net.rs", "my_crate::net::inner::Item"),
        ("src/net/mod.rs", "my_crate::net::inner::Item"),
        ("src/net/client.rs", "my_crate::net::client::inner::Item"),
        (
            "registry/src/cache/pkg-1/src/net.rs",
            "my_crate::net::inner::Item",
        ),
        ("std/src/fs.rs", "my_crate::fs::inner::Item"),
    ] {
        let output = extract("pkg", "my-crate", path, "mod inner { struct Item; }", "ref").unwrap();
        assert_eq!(output.symbols[1].qualified_name, expected);
    }
}

#[test]
fn warns_on_cfg_paths_and_reexports_without_following_or_evaluating_them() {
    let output = declarations(
        r#"
#[cfg(any())]
pub struct Inactive;
#[cfg_attr(feature = "x", path = "outside.rs")]
mod alternate;
#[path = "../../secret.rs"]
mod outside;
pub use other::Type as Alias;
"#,
    );
    assert_eq!(output.symbols.len(), 3);
    assert!(output.warnings.iter().any(|w| w.contains("cfg")));
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("path attributes"))
    );
    assert!(output.warnings.iter().any(|w| w.contains("reexports")));
    assert!(output.symbols.iter().all(|s| s.name != "Alias"));
}

#[test]
fn ranges_preserve_utf8_byte_offsets_and_line_positions() {
    let source = "// 🌍\nmod café {\n  pub struct Élément;\n}\n";
    let output = declarations(source);
    let item = output.symbols.iter().find(|s| s.name == "Élément").unwrap();
    assert_eq!(
        &source[item.range.start_byte..item.range.end_byte],
        "pub struct Élément;"
    );
    assert_eq!(item.range.start_line, 3);
    assert_eq!(item.range.start_column, 3);
    assert_eq!(item.qualified_name, "demo::café::Élément");
}

#[test]
fn bounds_declaration_count_and_retains_explicit_partial_warning() {
    let source: String = (0..2100).map(|i| format!("struct Type{i};\n")).collect();
    let output = declarations(&source);
    assert_eq!(output.symbols.len(), 2000);
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("2000") && w.contains("partial"))
    );
}

#[test]
fn bounds_depth_and_ast_visits_without_visiting_expression_trees() {
    let nested = format!(
        "{}struct Deep;{}\nstruct After;",
        "mod m {".repeat(140),
        "}".repeat(140)
    );
    let output = declarations(&nested);
    assert!(!output.symbols.iter().any(|s| s.name == "Deep"));
    assert!(output.symbols.iter().any(|s| s.name == "After"));
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("depth limit") && w.contains("partial"))
    );
    let sparse = format!("{}struct After;", "// comment\n".repeat(50_001));
    let output = declarations(&sparse);
    assert!(output.symbols.is_empty());
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("visit limit") && w.contains("partial"))
    );
}

#[test]
fn bounds_signature_bytes_on_utf8_boundaries_and_source_size() {
    let source = format!("fn long(argument: {}) {{ secret(); }}", "É".repeat(1800));
    let output = declarations(&source);
    assert_eq!(output.symbols.len(), 1);
    assert!(output.symbols[0].signature.len() <= 2048);
    assert!(!output.symbols[0].signature.contains("secret"));
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("signatures truncated"))
    );
    assert!(
        extract(
            "pkg",
            "demo",
            "src/lib.rs",
            &" ".repeat(2 * 1024 * 1024 + 1),
            "ref"
        )
        .is_err()
    );
}

#[test]
fn handles_trait_signatures_associated_types_and_foreign_declarations() {
    let output = declarations(
        r#"
trait Service { type Result; fn call(&self); }
unsafe extern "C" { fn foreign(); type Opaque; }
"#,
    );
    let call = output.symbols.iter().find(|s| s.name == "call").unwrap();
    assert_eq!(call.kind, "method");
    assert_eq!(call.owner_expression.as_deref(), Some("Service"));
    assert_eq!(
        output
            .symbols
            .iter()
            .find(|s| s.name == "foreign")
            .unwrap()
            .kind,
        "function"
    );
    assert!(
        output
            .symbols
            .iter()
            .any(|s| s.name == "Result" && s.kind == "type")
    );
}

#[test]
fn malformed_rust_marks_the_catalog_partial() {
    let output = declarations("fn valid() {}\n struct Broken { ?? }");
    assert!(output.symbols.iter().any(|s| s.name == "valid"));
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("parse errors") && w.contains("partial"))
    );
}

#[test]
fn huge_module_name_does_not_multiply_source_text_across_children() {
    let children: String = (0..2000).map(|i| format!("struct S{i};")).collect();
    let source = format!(
        "mod {} {{ {children} }} struct After;",
        "x".repeat(1024 * 1024)
    );
    let output = declarations(&source);
    assert_eq!(output.symbols.len(), 1);
    assert_eq!(output.symbols[0].name, "After");
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("name exceeds 512") && w.contains("partial"))
    );
}

#[test]
fn overflowing_lexical_scope_skips_subtree_without_fabricating_truncated_paths() {
    let source = format!(
        "{}struct Hidden;{} struct After;",
        format!("mod {} {{", "m".repeat(512)).repeat(10),
        "}".repeat(10)
    );
    let output = declarations(&source);
    assert!(!output.symbols.iter().any(|s| s.name == "Hidden"));
    assert!(output.symbols.iter().any(|s| s.name == "After"));
    assert!(
        output
            .symbols
            .iter()
            .all(|s| s.name.len() <= 512 && s.qualified_name.len() <= 4096)
    );
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("scope exceeds 4096") && w.contains("partial"))
    );
}

#[test]
fn overflowing_owner_and_impl_names_skip_methods_and_preserve_following_items() {
    let source = format!(
        "impl {} {{ fn hidden() {{}} }} struct After;",
        "T".repeat(2049)
    );
    let output = declarations(&source);
    assert_eq!(output.symbols.len(), 1);
    assert_eq!(output.symbols[0].name, "After");
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("owner expression exceeds 2048") && w.contains("partial"))
    );
    let output = declarations(&format!(
        "impl {} for T {{ fn hidden() {{}} }} struct After;",
        "Trait".repeat(110)
    ));
    assert_eq!(output.symbols.len(), 1);
    assert_eq!(output.symbols[0].name, "After");
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("name exceeds 512") && w.contains("partial"))
    );
}

#[test]
fn bounds_external_crate_names_and_source_metadata_before_per_symbol_allocation() {
    let output = extract("pkg", &"c".repeat(129), "src/lib.rs", "struct A;", "ref").unwrap();
    assert!(output.symbols.is_empty());
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("crate name exceeds 128") && w.contains("partial"))
    );
    for (package, path, source_ref) in [
        ("p".repeat(4097), "src/lib.rs".into(), "ref".into()),
        ("pkg".into(), "p".repeat(4097), "ref".into()),
        ("pkg".into(), "src/lib.rs".into(), "r".repeat(4097)),
    ] {
        let output = extract(&package, "demo", &path, "struct A;", &source_ref).unwrap();
        assert!(output.symbols.is_empty());
        assert!(
            output
                .warnings
                .iter()
                .any(|w| w.contains("identity/path exceeds 4096") && w.contains("partial"))
        );
    }
    let path = format!("src/{}.rs", "p".repeat(4000));
    let output = extract("pkg", &"c".repeat(128), &path, "struct A;", "ref").unwrap();
    assert!(output.symbols.is_empty());
    assert!(
        output
            .warnings
            .iter()
            .any(|w| w.contains("scope exceeds 4096") && w.contains("partial"))
    );
}
