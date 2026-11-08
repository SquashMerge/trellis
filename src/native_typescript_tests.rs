//! TypeScript native extraction: routing by path, the shared JavaScript constructs and
//! lookup-key equivalence with JavaScript. These tests never name the parser crate.
use crate::{
    indexer::{IndexOptions, index_workspace_with_native},
    native_evidence::{self, Artifact, DeclarationRole},
};
use std::sync::{Arc, atomic::AtomicBool};

fn index(files: &[(&str, &str)]) -> Artifact {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        std::fs::write(root.path().join(path), text).unwrap();
    }
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    index_workspace_with_native(
        &IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap()
    .1
}

/// `(kind, name)` of every declaration and the spelling of every call, in source order.
fn measured(artifact: &Artifact) -> (Vec<(String, String)>, Vec<String>) {
    let declarations = artifact
        .declarations
        .iter()
        .map(|d| (d.kind.clone(), d.name.clone().unwrap_or_default()))
        .collect();
    let calls = artifact
        .calls
        .iter()
        .map(|c| c.spelling.clone().unwrap_or_default())
        .collect();
    (declarations, calls)
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(kind, name)| ((*kind).to_owned(), (*name).to_owned()))
        .collect()
}

#[test]
fn lookup_uses_javascript_rule() {
    for name in [
        "plain",
        "caf\u{e9}",
        "cafe\u{301}",
        "\\u0061bc",
        "\\u{62}cd",
        "\u{10400}label",
        "a\\u0020b",
        "\\u0069f",
        "",
    ] {
        assert_eq!(
            native_evidence::lookup("typescript", name).ok(),
            native_evidence::lookup("javascript", name).ok(),
            "{name:?}"
        );
    }
    assert_eq!(
        native_evidence::lookup("typescript", "\\u0061bc").unwrap(),
        "abc"
    );
    assert_ne!(
        native_evidence::lookup("typescript", "caf\u{e9}").unwrap(),
        native_evidence::lookup("typescript", "cafe\u{301}").unwrap(),
        "exact code points, not normalization"
    );
    assert!(native_evidence::lookup("typescript", "a\\u0020b").is_err());
    assert!(native_evidence::lookup("TypeScript", "abc").is_err());
}

const SHARED: &str = "\
function alpha() { return beta(); }
const gamma = function () { return alpha(); };
const delta = () => new Epsilon();
function* zeta() { yield alpha(); }
const eta = { run() { return obj.go(); } };
";

#[test]
fn shared_javascript_constructs() {
    let javascript = measured(&index(&[("a.js", SHARED)]));
    for path in ["a.ts", "a.tsx"] {
        let artifact = index(&[(path, SHARED)]);
        assert!(
            artifact.coverage.iter().all(|c| c.diagnostic.is_none()),
            "{path} parses without a gap"
        );
        assert!(
            artifact
                .coverage
                .iter()
                .all(|c| c.language == "typescript" && c.document_path == path)
        );
        let (declarations, calls) = measured(&artifact);
        assert_eq!(
            declarations,
            pairs(&[
                ("module", ""),
                ("function", "alpha"),
                ("variable", "gamma"),
                ("variable", "delta"),
                ("anonymousFunction", ""),
                ("function", "zeta"),
                ("variable", "eta"),
                ("method", "run"),
            ]),
            "{path}"
        );
        assert_eq!(calls, ["beta", "alpha", "Epsilon", "alpha", "go"], "{path}");
        assert_eq!(
            (declarations, calls),
            javascript,
            "{path} measures exactly as JavaScript"
        );
    }
}

#[test]
fn the_grammar_follows_the_path() {
    let jsx = "const view = <div onClick={() => run()} />;\n";
    let gap = |path| {
        index(&[(path, jsx)])
            .coverage
            .iter()
            .any(|c| c.diagnostic.is_some())
    };
    assert!(!gap("a.tsx"), "JSX parses under the TSX grammar");
    assert!(
        gap("a.ts"),
        "JSX is a parse error under the TypeScript grammar"
    );
}

#[test]
fn declaration_only_documents() {
    use crate::indexer_ts::declaration_only;
    for path in ["a.d.ts", "dir/a.d.mts", "a.b.d.cts", ".d.ts"] {
        assert!(declaration_only(path), "{path}");
    }
    for path in [
        "a.ts",
        "a.tsx",
        "a.mts",
        "a.cts",
        "d.ts",
        "a.D.ts",
        "a.d.TS",
        "a.d.tsx",
        "a.d.js",
        "a.d.ts.map",
    ] {
        assert!(!declaration_only(path), "{path}");
    }

    let source = "\
declare function f(x: number): void;
declare class K {
  static { init(); }
  m(): void { go(); if (a) { b(); } }
  n = (): number => { if (c) { return d(); } return 1; };
  g = function* () { yield e(); };
  @dec(1) p = 0;
}
export const k = h();
const t = cond ? 1 : 2;
if (flag) { other(); }
";
    for path in ["a.d.ts", "a.d.mts", "a.d.cts"] {
        let artifact = index(&[(path, source)]);
        no_gap(path, &artifact);
        assert!(artifact.calls.is_empty(), "{path}: no calls");
        assert!(
            artifact.control_regions.is_empty(),
            "{path}: no control regions"
        );
        let (declarations, _) = measured(&artifact);
        assert_eq!(
            declarations,
            pairs(&[
                ("module", ""),
                ("function", "f"),
                ("type", "K"),
                ("method", "m"),
                ("field", "n"),
                ("anonymousFunction", ""),
                ("field", "g"),
                ("field", "p"),
                ("variable", "k"),
                ("variable", "t"),
            ]),
            "{path}"
        );
        let spelled = references(&artifact);
        assert!(spelled.contains(&"h") && spelled.contains(&"dec"), "{path}");
        assert!(
            !spelled.contains(&"go") && !spelled.contains(&"b") && !spelled.contains(&"init"),
            "{path}: bodies are skipped"
        );
    }
    let implementation = index(&[("a.ts", source)]);
    assert!(!implementation.calls.is_empty() && !implementation.control_regions.is_empty());
}

/// Both grammars measure `source`; the TSX variant swaps the TypeScript-only `<T>x`
/// assertion for `x as T`, which the TSX grammar reads.
fn both(source: &str) -> [(&'static str, Artifact); 2] {
    let tsx = source.replace("<T>x", "x as T");
    [
        ("a.ts", index(&[("a.ts", source)])),
        ("a.tsx", index(&[("a.tsx", &tsx)])),
    ]
}

fn no_gap(path: &str, artifact: &Artifact) {
    assert!(
        artifact.coverage.iter().all(|c| c.diagnostic.is_none()),
        "{path} parses without a gap"
    );
}

/// Declarations of one kind, as `name` in source order.
fn names(artifact: &Artifact, kind: &str) -> Vec<String> {
    artifact
        .declarations
        .iter()
        .filter(|d| d.kind == kind)
        .map(|d| d.name.clone().unwrap_or_default())
        .collect()
}

fn declaration<'a>(
    artifact: &'a Artifact,
    kind: &str,
    name: &str,
) -> &'a crate::native_evidence::Declaration {
    artifact
        .declarations
        .iter()
        .find(|d| d.kind == kind && d.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no {kind} {name}"))
}

fn references(artifact: &Artifact) -> Vec<&str> {
    artifact
        .references
        .iter()
        .map(|r| r.spelling.as_str())
        .collect()
}

#[test]
fn classes_and_class_expressions() {
    let source = "\
export class Box<T, U extends object> extends Base implements I { }
const Anon = class Named { };
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        assert_eq!(names(&artifact, "type"), ["Box", "Named"], "{path}");
        let class = declaration(&artifact, "type", "Box");
        assert_eq!(
            class.header.type_parameters,
            ["T", "U extends object"],
            "{path}"
        );
        assert!(class.header.bases.is_empty(), "{path}");
        assert_eq!(names(&artifact, "variable"), ["Anon"], "{path}");
    }
}

#[test]
fn abstract_classes_and_members() {
    let source = "\
abstract class Shape<T> {
  abstract area(): number;
  protected abstract name: string;
  describe(): string { return \"\"; }
}
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        let class = declaration(&artifact, "type", "Shape");
        assert!(
            class.header.modifiers.contains(&"abstract".to_owned()),
            "{path}"
        );
        assert_eq!(class.header.type_parameters, ["T"], "{path}");
        let area = declaration(&artifact, "method", "area");
        assert_eq!(area.header.modifiers, ["abstract"], "{path}");
        assert_eq!(area.header.result_type.as_deref(), Some("number"), "{path}");
        assert_eq!(names(&artifact, "method"), ["area", "describe"], "{path}");
        let field = declaration(&artifact, "field", "name");
        assert_eq!(field.header.modifiers, ["protected", "abstract"], "{path}");
        assert_eq!(field.ancestors.len(), 1, "{path}");
    }
}

#[test]
fn interfaces_and_members() {
    let source = "\
interface Repo<T> extends Base {
  id: number;
  find(key: string): T;
  (call: number): void;
  new (made: number): Repo<T>;
  [index: string]: unknown;
}
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        assert_eq!(names(&artifact, "type"), ["Repo"], "{path}");
        assert_eq!(names(&artifact, "field"), ["id"], "{path}");
        assert_eq!(names(&artifact, "method"), ["find"], "{path}");
        assert_eq!(
            artifact.declarations.len(),
            4,
            "module, type, field, method: {path}"
        );
        let find = declaration(&artifact, "method", "find");
        assert_eq!(find.header.parameters.len(), 1, "{path}");
        assert_eq!(
            find.header.parameters[0].name.as_deref(),
            Some("key"),
            "{path}"
        );
        assert_eq!(
            find.header.parameters[0].type_name.as_deref(),
            Some("string"),
            "{path}"
        );
        assert_eq!(find.header.result_type.as_deref(), Some("T"), "{path}");
    }
}

#[test]
fn enums() {
    let source = "\
enum Color { Red, Green = 2 }
const enum Flag { On = 1 }
declare enum Mode { A }
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        assert_eq!(
            names(&artifact, "type"),
            ["Color", "Flag", "Mode"],
            "{path}"
        );
        assert_eq!(
            artifact.declarations.len(),
            4,
            "members are not declarations: {path}"
        );
        let spelled = references(&artifact);
        for member in ["Red", "Green", "On", "A"] {
            assert!(spelled.contains(&member), "{member} is a candidate: {path}");
        }
    }
}

#[test]
fn type_aliases() {
    let source = "type Pair<A, B> = [A, B];\nexport type Id = string;\n";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        assert_eq!(names(&artifact, "alias"), ["Pair", "Id"], "{path}");
        assert_eq!(
            declaration(&artifact, "alias", "Pair")
                .header
                .type_parameters,
            ["A", "B"],
            "{path}"
        );
    }
}

#[test]
fn namespaces_and_modules() {
    let source = "\
namespace A.B.C { export class Inner {} }
module N { }
declare module \"ambient-mod\" { export function f(): void; }
declare module \"\" { }
declare global { interface Window { app: string } }
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        assert_eq!(
            names(&artifact, "namespace"),
            ["C", "N", "ambient-mod"],
            "{path}"
        );
        let inner = declaration(&artifact, "type", "Inner");
        assert_eq!(inner.ancestors.len(), 1, "{path}");
        assert_eq!(inner.ancestors[0].name.as_deref(), Some("C"), "{path}");
        let window = declaration(&artifact, "type", "Window");
        assert!(
            window.ancestors.is_empty(),
            "declare global declares nothing; Window parents to the module: {path}"
        );
        assert_eq!(
            declaration(&artifact, "function", "f").ancestors[0]
                .name
                .as_deref(),
            Some("ambient-mod"),
            "{path}"
        );
    }
}

#[test]
fn generics_headers_and_calls() {
    let source = "\
function id<T, K extends keyof T = never>(x: T): T { return x; }
const a = id<number>(1);
const b = new Map<string, number>();
const c = id<string>;
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        let function = declaration(&artifact, "function", "id");
        assert_eq!(
            function.header.type_parameters,
            ["T", "K extends keyof T = never"],
            "{path}"
        );
        assert!(names(&artifact, "typeParameter").is_empty(), "{path}");
        let (_, calls) = measured(&artifact);
        assert_eq!(
            calls,
            ["id", "Map"],
            "{path}: `id<string>` alone is not a call"
        );
        assert!(
            artifact.calls.iter().all(|c| c.callee_range.is_some()),
            "{path}"
        );
    }
}

#[test]
fn function_overloads() {
    let source = "\
function f(x: string): string;
function f(x: number): number;
function f(x: any) { return x; }
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        let ordinals: Vec<usize> = artifact
            .declarations
            .iter()
            .filter(|d| d.kind == "function")
            .map(|d| d.key.ordinal)
            .collect();
        assert_eq!(names(&artifact, "function"), ["f", "f", "f"], "{path}");
        assert_eq!(ordinals, [0, 1, 2], "{path}");
    }
}

#[test]
fn method_overloads() {
    let source = "\
class C {
  m(x: string): void;
  m(x: number): void;
  m(x: any) {}
}
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        let ordinals: Vec<usize> = artifact
            .declarations
            .iter()
            .filter(|d| d.kind == "method")
            .map(|d| d.key.ordinal)
            .collect();
        assert_eq!(names(&artifact, "method"), ["m", "m", "m"], "{path}");
        assert_eq!(ordinals, [0, 1, 2], "{path}");
    }
}

#[test]
fn decorators() {
    let source = "\
@sealed(1)
class A {
  @log
  run(@inject(2) dep: Dep) { return 1; }
  @field(3) value = 0;
}
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        let class = declaration(&artifact, "type", "A");
        let run = declaration(&artifact, "method", "run");
        let calls: Vec<(&str, &str)> = artifact
            .calls
            .iter()
            .map(|c| (c.spelling.as_deref().unwrap(), c.owner_syntax_id.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("sealed", class.syntax_id.as_str()),
                ("inject", run.syntax_id.as_str()),
                ("field", class.syntax_id.as_str()),
            ],
            "{path}"
        );
        let spelled = references(&artifact);
        assert!(spelled.contains(&"log"), "@log is a reference only: {path}");
    }
}

#[test]
fn parameter_properties() {
    let source = "\
class A {
  constructor(private readonly id: number, public name?: string, plain: number, override x = 1, readonly z: number) {}
  other(private notField: number) {}
}
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        assert_eq!(
            names(&artifact, "field"),
            ["id", "name", "x", "z"],
            "{path}"
        );
        let constructor = declaration(&artifact, "method", "constructor");
        for field in artifact.declarations.iter().filter(|d| d.kind == "field") {
            let parent = field.ancestors.last().unwrap();
            assert_eq!(parent.name.as_deref(), Some("constructor"), "{path}");
            assert_eq!(parent.kind, constructor.key.kind, "{path}");
        }
        assert!(
            artifact.declarations.iter().all(|d| d.kind != "parameter"),
            "{path}"
        );
    }
}

#[test]
fn typed_members_and_headers() {
    let source = "\
class A {
  public static count: number = 0;
  private readonly tag?: string;
  async load(this: A, id: string, ...rest: number[]): Promise<void> {}
  get size(): number { return 1; }
  static check(x: unknown): x is string { return true; }
  assert(x: unknown): asserts x { }
}
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        assert_eq!(
            declaration(&artifact, "field", "count").header.modifiers,
            ["public", "static"],
            "{path}"
        );
        assert_eq!(
            declaration(&artifact, "field", "tag").header.modifiers,
            ["private", "readonly"],
            "{path}"
        );
        let load = declaration(&artifact, "method", "load");
        assert_eq!(load.header.modifiers, ["async"], "{path}");
        assert_eq!(
            load.header.result_type.as_deref(),
            Some("Promise<void>"),
            "{path}"
        );
        let parameters: Vec<(Option<&str>, Option<&str>)> = load
            .header
            .parameters
            .iter()
            .map(|p| (p.name.as_deref(), p.type_name.as_deref()))
            .collect();
        assert_eq!(
            parameters,
            [
                (None, Some("A")),
                (Some("id"), Some("string")),
                (None, Some("number[]")),
            ],
            "{path}"
        );
        assert!(load.header.parameters[2].variadic, "{path}");
        assert_eq!(
            declaration(&artifact, "method", "size").header.modifiers,
            ["get"],
            "{path}"
        );
        assert_eq!(
            declaration(&artifact, "method", "check")
                .header
                .result_type
                .as_deref(),
            Some("x is string"),
            "{path}"
        );
        assert_eq!(
            declaration(&artifact, "method", "assert")
                .header
                .result_type
                .as_deref(),
            Some("asserts x"),
            "{path}"
        );
    }
}

#[test]
fn type_only_imports_and_exports() {
    let source = "\
import type { a as b } from \"m\";
import { type c as d, e } from \"m\";
import type F from \"m\";
import g = require(\"m\");
import H = N.B;
export type { A };
export type { Z } from \"m\";
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        assert_eq!(names(&artifact, "alias"), ["b", "d"], "{path}");
        assert_eq!(
            artifact.declarations.len(),
            3,
            "only the module and two aliases: {path}"
        );
        let spelled = references(&artifact);
        for candidate in ["a", "c", "e", "F", "g", "H", "N", "B", "A", "Z"] {
            assert!(spelled.contains(&candidate), "{candidate}: {path}");
        }
    }
}

#[test]
fn ambient_declarations() {
    let source = "\
declare function f(x: number): void;
declare class K { m(): void }
declare const v: number;
declare module \"m\" { }
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        let (declarations, _) = measured(&artifact);
        assert_eq!(
            declarations,
            pairs(&[
                ("module", ""),
                ("function", "f"),
                ("type", "K"),
                ("method", "m"),
                ("variable", "v"),
                ("namespace", "m"),
            ]),
            "{path}"
        );
    }
}

#[test]
fn ts_expression_wrappers() {
    let source = "\
const a = (x as T).f();
const b = x!.g();
const c = (x satisfies T).h();
const d = k!();
const e = (<T>x).m();
const f = id<number>;
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        let spellings: Vec<&str> = artifact
            .calls
            .iter()
            .map(|c| c.spelling.as_deref().unwrap())
            .collect();
        assert_eq!(spellings, ["f", "g", "h", "k!", "m"], "{path}");
        let bare = artifact
            .calls
            .iter()
            .find(|c| c.spelling.as_deref() == Some("k!"))
            .unwrap();
        assert!(bare.callee_range.is_none(), "{path}");
        assert!(
            artifact
                .calls
                .iter()
                .filter(|c| c.callee_range.is_some())
                .count()
                == 4,
            "{path}"
        );
    }
}

#[test]
fn tsx_calls_inside_jsx() {
    let source = "\
declare function render(m: number): void;
export const view = <Panel onClick={() => render(1)} label={fmt(2)}><Inner /></Panel>;
";
    let artifact = index(&[("a.tsx", source)]);
    no_gap("a.tsx", &artifact);
    let (_, calls) = measured(&artifact);
    assert_eq!(calls, ["render", "fmt"]);
    let spelled = references(&artifact);
    for tag in ["Panel", "Inner"] {
        assert!(spelled.contains(&tag), "{tag} is a candidate");
    }
    assert_eq!(names(&artifact, "variable"), ["view"]);
}

#[test]
fn test_role_by_path() {
    for path in [
        "a.test.ts",
        "dir/a.spec.tsx",
        "__tests__/x.ts",
        "tests/x.mts",
        "tests/types.d.ts",
        "src/x.test.d.ts",
    ] {
        let artifact = index_at(path);
        assert!(
            artifact
                .declarations
                .iter()
                .all(|d| d.role == DeclarationRole::Test),
            "{path}"
        );
    }
    for path in ["src/types.d.ts", "src/x.ts"] {
        let artifact = index_at(path);
        assert!(
            artifact
                .declarations
                .iter()
                .all(|d| d.role == DeclarationRole::Production),
            "{path}"
        );
    }
}

fn index_at(path: &str) -> Artifact {
    let root = tempfile::tempdir().unwrap();
    let full = root.path().join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, "declare function f(): void;\n").unwrap();
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    index_workspace_with_native(
        &IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap()
    .1
}

#[test]
fn reference_candidates() {
    let source = "\
class #P { #p = 1; m(): Foo { return this.#p + this.q + obj.r; } }
const { s, t: u } = { v, w: 1 };
";
    let source = source.replace("class #P", "class P");
    for (path, artifact) in both(&source) {
        no_gap(path, &artifact);
        let spelled = references(&artifact);
        for candidate in ["P", "m", "Foo", "q", "obj", "r", "s", "t", "u", "v", "w"] {
            assert!(spelled.contains(&candidate), "{candidate}: {path}");
        }
        assert!(
            spelled.iter().all(|s| !s.starts_with('#')),
            "no private names: {path}"
        );
        for keyword in ["class", "const", "return", "this"] {
            assert!(!spelled.contains(&keyword), "{keyword}: {path}");
        }
    }
}

#[test]
fn bounded_walk_reports_gap() {
    let depth = 300;
    let source = format!("const x = {}1{};\n", "(".repeat(depth), ")".repeat(depth));
    for (path, artifact) in both(&source) {
        assert!(
            artifact.coverage.iter().any(bound_gap),
            "{path} reports the bound gap"
        );
    }
    let nested = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
    for (path, artifact) in both(&format!("const y = {nested};\n")) {
        assert!(artifact.coverage.iter().any(bound_gap), "{path}");
    }
}

fn bound_gap(coverage: &crate::native_evidence::Coverage) -> bool {
    coverage
        .diagnostic
        .as_deref()
        .is_some_and(|d| d.starts_with("syntax beyond native walk bounds skipped"))
}

#[test]
fn override_modifier_is_measured() {
    let source = "class D extends B { override run(): void {} }\n";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        assert!(
            declaration(&artifact, "method", "run")
                .header
                .modifiers
                .contains(&"override".to_owned()),
            "{path}"
        );
    }
}

#[test]
fn type_only_import_expressions_are_not_calls() {
    let source = "\
export type ApiModule = typeof import(\"./api\");
export type Api = import(\"./api\").Api;
export const lazy = () => import(\"./api\");
const v = x as import(\"./api\").Api;
const w = import(\"./api\") as Promise<unknown>;
";
    for (path, artifact) in both(source) {
        no_gap(path, &artifact);
        let (_, calls) = measured(&artifact);
        assert_eq!(calls, ["import", "import"], "only runtime calls: {path}");
    }
}
