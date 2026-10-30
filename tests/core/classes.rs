use std::{
    cell::Cell,
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use trellis::{
    classes::Catalog,
    indexer::{IndexOptions, index_workspace},
    model::{CancelFlag, Graph},
};
thread_local! { static GOLDEN_ORDINAL: Cell<usize> = const { Cell::new(0) }; }
fn golden(catalog: &Catalog) {
    // libtest names the thread `classes::<test>` in the grouped binary; the
    // goldens are named by the bare test name.
    let name = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .rsplit("::")
        .next()
        .unwrap()
        .to_owned();
    let ordinal = GOLDEN_ORDINAL.with(|counter| {
        let n = counter.get();
        counter.set(n + 1);
        n
    });
    let filename = format!("{name}-{ordinal}.json");
    let path = std::path::Path::new("tests/fixtures/classes/below-cap").join(filename);
    let bytes = serde_json::to_vec(catalog).unwrap();
    assert_eq!(
        fs::read(path).unwrap(),
        bytes,
        "frozen pre-edit class JSON differs"
    );
}
fn fixture(files: &[(&str, &str)]) -> (Graph, Catalog) {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let git = temp.path().join(".git");
    let private = git.join("trellis");
    fs::create_dir(&git).unwrap();
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let marker = private.join("workspace-id");
    // Fixed canonical v4 UUID: temp directory names must not alter frozen symbol IDs.
    fs::write(&marker, b"123e4567-e89b-42d3-a456-426614174000").unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    for (path, text) in files {
        let path = temp.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let graph = index_workspace(&IndexOptions::new(temp.path().into()), &cancel, |_| {}).unwrap();
    let catalog = Catalog::build(&graph.files, &graph.nodes, &cancel).unwrap();
    golden(&catalog);
    (graph, catalog)
}
fn class<'a>(c: &'a Catalog, name: &str) -> &'a trellis::model::ClassDefinition {
    c.classes
        .iter()
        .find(|c| c.qualified_name == name)
        .unwrap_or_else(|| panic!("Missing {name}: {c:#?}"))
}
fn linked(c: &Catalog, owner: &str, name: &str, kind: &str, target: &str) {
    let owner = &class(c, owner).symbol.id;
    let _target_is_a_declared_class = &class(c, target).symbol.id;
    assert!(
        c.relations.iter().any(|r| &r.owner == owner
            && r.type_name == name
            && r.kind == kind
            && r.target.is_none()
            && r.candidate_ids.is_empty()
            && r.match_kind == "unmatched"),
        "missing {name} {kind} -> {target}: {c:#?}"
    );
}
#[test]
fn java_members_inheritance_generics_and_nested_classes_are_source_bound() {
    let (g, c) = fixture(&[(
        "p/Model.java",
        r#"package p;
interface Api {} class Base {} class Item {} class Box<T> {}
class Model<T> extends Base implements Api {
    Box<Item> values; T generic; int count;
    Model(Item value) {} Item find(Item value) { class Local {} return value; }
    class Nested { Item value; }
}"#,
    )]);
    assert!(!c.truncated, "{c:#?}");
    linked(&c, "p.Model", "Base", "extends", "p.Base");
    linked(&c, "p.Model", "Api", "implements", "p.Api");
    linked(&c, "p.Model", "Box", "field", "p.Box");
    linked(&c, "p.Model", "Item", "field", "p.Item");
    linked(&c, "p.Model", "Item", "parameter", "p.Item");
    linked(&c, "p.Model", "Item", "returns", "p.Item");
    linked(&c, "p.Model.Nested", "Item", "field", "p.Item");
    assert!(!c.classes.iter().any(|c| c.symbol.name == "Local"));
    let model = class(&c, "p.Model");
    assert_eq!(model.fields.len(), 3);
    assert_eq!(model.methods.len(), 2);
    assert!(model.methods.iter().all(|m| m.symbol_id.is_some()));
    for def in &c.classes {
        assert!(g.nodes.contains(&def.symbol));
    }
    for r in &c.relations {
        let f = g.files.iter().find(|f| f.path == r.path).unwrap();
        assert_eq!(&f.text[r.range.start_byte..r.range.end_byte], r.type_name);
    }
    let again = Catalog::build(&g.files, &g.nodes, &Arc::new(AtomicBool::new(false))).unwrap();
    assert_eq!(c, again);
    let json = serde_json::to_value(&c).unwrap();
    assert!(json["classes"][0].get("qualifiedName").is_some());
}
#[test]
fn java_scopes_never_guess_global_names_or_generic_parameters() {
    let (_, c) = fixture(&[
        (
            "p/Owner.java",
            r#"package p; import other.Imported;
class Owner<Shadow> { Imported imported; Missing missing; Shadow shadow;
 <Imported> Imported generic(Imported value) { return value; }
 class Inner {} Inner nested;
}
class Shadow {}"#,
        ),
        (
            "other/Types.java",
            "package other; class Imported {} class Missing {}",
        ),
    ]);
    linked(&c, "p.Owner", "Imported", "field", "other.Imported");
    linked(&c, "p.Owner", "Inner", "field", "p.Owner.Inner");
    for r in c.relations.iter().filter(|r| {
        r.type_name == "Shadow"
            || r.type_name == "Missing"
            || r.kind == "returns"
            || r.kind == "parameter"
    }) {
        assert!(r.target.is_none(), "{r:?}");
    }
}
#[test]
fn duplicate_qualified_classes_stay_ambiguous() {
    let (_, c) = fixture(&[
        ("a/A.java", "package p; class Same {}"),
        ("b/B.java", "package p; class Same {}"),
        ("c/C.java", "package p; class Owner { Same field; }"),
    ]);
    let r = c.relations.iter().find(|r| r.type_name == "Same").unwrap();
    assert_eq!(r.match_kind, "unmatched");
    assert!(r.candidate_ids.is_empty());
    assert!(r.target.is_none());
}
#[test]
fn python_imports_annotations_members_and_nested_classes() {
    let (_, c) = fixture(&[
        ("pkg/models.py", "class Item: pass\nclass Base: pass\n"),
        (
            "pkg/owner.py",
            r#"from .models import Item as Renamed, Base
import pkg.models as models
class Owner(Base):
    item: Renamed
    values: list[models.Item]
    quoted: 'Renamed'
    def find(self, value: Renamed) -> models.Item:
        class Local: pass
        self.hidden: models.Item = value
        return value
    class Nested:
        value: models.Item
"#,
        ),
    ]);
    assert!(!c.truncated, "{c:#?}");
    linked(&c, "pkg.owner.Owner", "Base", "extends", "pkg.models.Base");
    linked(&c, "pkg.owner.Owner", "Renamed", "field", "pkg.models.Item");
    linked(
        &c,
        "pkg.owner.Owner",
        "models.Item",
        "field",
        "pkg.models.Item",
    );
    linked(
        &c,
        "pkg.owner.Owner",
        "Renamed",
        "parameter",
        "pkg.models.Item",
    );
    linked(
        &c,
        "pkg.owner.Owner",
        "models.Item",
        "returns",
        "pkg.models.Item",
    );
    linked(
        &c,
        "pkg.owner.Owner.Nested",
        "models.Item",
        "field",
        "pkg.models.Item",
    );
    assert_eq!(class(&c, "pkg.owner.Owner").fields.len(), 3);
    assert_eq!(class(&c, "pkg.owner.Owner").methods.len(), 1);
    assert!(!c.classes.iter().any(|c| c.symbol.name == "Local"));
    assert!(!c.relations.iter().any(|r| r.range.start_line == 10));
}
#[test]
fn python_shadowing_and_type_parameters_do_not_claim_workspace_targets() {
    let (_, c) = fixture(&[
        ("types.py", "class Item: pass\nclass T: pass\n"),
        (
            "use.py",
            r#"from types import Item, T
Item = factory()
class Owner[T]:
    value: Item
    other: T
    def generic[Item](self, value: Item) -> Item: pass
"#,
        ),
        ("elsewhere.py", "class Unimported: pass\n"),
        ("plain.py", "class Owner:\n    value: Unimported\n"),
    ]);
    assert!(!c.truncated, "{c:#?}");
    assert!(c.relations.iter().all(|r| r.target.is_none()), "{c:#?}");
}
#[test]
fn python_literal_and_annotated_metadata_are_not_type_links() {
    let (_, c) = fixture(&[
        ("types.py", "class Item: pass\nclass Metadata: pass\n"),
        (
            "use.py",
            r#"from typing import Literal, Annotated
from types import Item, Metadata
class Owner:
    literal: Literal['Item']
    annotated: Annotated[Item, Metadata]
"#,
        ),
    ]);
    linked(&c, "use.Owner", "Item", "field", "types.Item");
    assert!(!c.relations.iter().any(|r| r.type_name == "Metadata"));
    assert_eq!(
        c.relations.iter().filter(|r| r.type_name == "Item").count(),
        1
    );
}
#[test]
fn cancellation_and_old_or_unsupported_snapshots_are_safe() {
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    assert_eq!(
        Catalog::build(&[], &[], &cancel).unwrap(),
        Catalog::default()
    );
    cancel.store(true, Ordering::Relaxed);
    assert!(
        Catalog::build(&[], &[], &cancel)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    let (_, c) = fixture(&[("a.js", "class Foo {}"), ("a.rs", "struct Foo {}")]);
    assert!(c.classes.is_empty());
    assert!(c.relations.is_empty());
}
#[test]
fn missing_measured_symbols_and_member_limits_are_explicit() {
    let (g, _) = fixture(&[("a.py", "class A: pass\n")]);
    let c = Catalog::build(&g.files, &[], &Arc::new(AtomicBool::new(false))).unwrap();
    assert!(c.classes.is_empty());
    assert!(c.truncated);
    assert!(!c.warnings.is_empty());
    let source = format!(
        "class A:\n{}",
        (0..300)
            .map(|i| format!("    field{i}: A\n"))
            .collect::<String>()
    );
    let (_, c) = fixture(&[("a.py", &source)]);
    assert!(c.truncated);
    assert!(class(&c, "a.A").truncated);
    assert_eq!(class(&c, "a.A").fields.len(), 256);
    assert!(
        c.relations
            .iter()
            .all(|r| r.target.is_none() && r.candidate_ids.is_empty())
    );
    assert!(!c.warnings.iter().any(|w| w.contains("linking is disabled")));
}
#[test]
fn java_records_interfaces_enums_and_annotations_have_explicit_members_only() {
    let (_, c) = fixture(&[(
        "p/Types.java",
        r#"package p;
class Item {} interface Parent {} interface Child extends Parent { Item get(); }
record Data(Item value) implements Parent { Item get() {return value;} }
enum Choice { YES, NO; Item value; }
@interface Mark { Item value(); }
"#,
    )]);
    assert!(!c.truncated, "{c:#?}");
    linked(&c, "p.Child", "Parent", "extends", "p.Parent");
    linked(&c, "p.Data", "Parent", "implements", "p.Parent");
    assert_eq!(class(&c, "p.Data").fields[0].name, "value");
    assert_eq!(class(&c, "p.Data").methods.len(), 1);
    assert_eq!(class(&c, "p.Choice").fields.len(), 3);
    assert_eq!(class(&c, "p.Mark").methods.len(), 1);
}

#[test]
fn python_nested_class_bodies_skip_enclosing_class_namespace_but_bases_do_not() {
    let (_, c) = fixture(&[(
        "nested.py",
        r#"class ModuleType: pass
class Outer:
    class T: pass
    class ModuleType: pass
    class Inner(T):
        unknown: T
        known: ModuleType
"#,
    )]);
    linked(&c, "nested.Outer.Inner", "T", "extends", "nested.Outer.T");
    linked(
        &c,
        "nested.Outer.Inner",
        "ModuleType",
        "field",
        "nested.ModuleType",
    );
    assert!(
        c.relations
            .iter()
            .find(|r| r.type_name == "T" && r.kind == "field")
            .unwrap()
            .target
            .is_none()
    );
}
#[test]
fn java_static_imports_block_wrong_same_package_candidates() {
    let (_, c) = fixture(&[
        (
            "p/C.java",
            "package p; import static q.Outer.T; class C { T value; } class T {}",
        ),
        (
            "q/Outer.java",
            "package q; class Outer { static class T {} }",
        ),
    ]);
    assert!(
        c.relations
            .iter()
            .find(|r| r.type_name == "T")
            .unwrap()
            .target
            .is_none()
    );
}
#[test]
fn python_relative_imports_cannot_escape_package_root() {
    let (_, c) = fixture(&[
        ("a.py", "class T: pass\n"),
        (
            "pkg/mod.py",
            "from ..a import T\nclass Owner:\n    value: T\n",
        ),
    ]);
    assert!(c.relations.iter().all(|r| r.target.is_none()));
}

#[test]
fn direct_python_methods_keep_measured_ids_including_legacy_function_kind() {
    let (mut g, c) = fixture(&[(
        "a.py",
        "class Owner:\n    def method(self):\n        def nested(): pass\n        return nested()\n",
    )]);
    let method = &class(&c, "a.Owner").methods[0];
    assert!(method.symbol_id.is_some());
    assert_eq!(class(&c, "a.Owner").methods.len(), 1);
    let id = method.symbol_id.clone().unwrap();
    g.nodes.iter_mut().find(|s| s.id == id).unwrap().kind = trellis::model::SymbolKind::Function;
    let c = Catalog::build(&g.files, &g.nodes, &Arc::new(AtomicBool::new(false))).unwrap();
    assert_eq!(
        class(&c, "a.Owner").methods[0].symbol_id.as_ref(),
        Some(&id)
    );
}

#[test]
fn python_typing_aliases_do_not_turn_literal_values_or_metadata_into_types() {
    let (_, c) = fixture(&[(
        "a.py",
        r#"from typing import Literal as L, Annotated as A
class Item: pass
class Metadata: pass
class Owner:
    literal: L["Item"]
    annotated: A[Item, Metadata]
"#,
    )]);
    linked(&c, "a.Owner", "Item", "field", "a.Item");
    assert_eq!(
        c.relations.iter().filter(|r| r.type_name == "Item").count(),
        1
    );
    assert!(!c.relations.iter().any(|r| r.type_name == "Metadata"));
}
#[test]
fn generic_base_arguments_are_not_additional_inheritance_edges() {
    let (_, c) = fixture(&[
        (
            "a.py",
            "class Item: pass\nclass Base: pass\nclass Owner(Base[Item]): pass\n",
        ),
        (
            "A.java",
            "class Item {} class Base<T> {} class Owner extends Base<Item> {}",
        ),
    ]);
    assert_eq!(
        c.relations.iter().filter(|r| r.kind == "extends").count(),
        2
    );
    assert!(
        c.relations
            .iter()
            .filter(|r| r.kind == "extends")
            .all(|r| r.type_name == "Base")
    );
}

#[test]
fn recovered_syntax_and_oversized_cached_sources_report_incompleteness() {
    let (_, c) = fixture(&[("a.java", "class A { A value; void broken( }")]);
    assert!(c.truncated);
    assert!(c.warnings.iter().any(|w| w.contains("recovered syntax")));
    assert!(c.relations.iter().all(|r| r.target.is_none()));
    let file = trellis::model::SourceFile {
        path: "huge.py".into(),
        hash: "cached".into(),
        language: "python".into(),
        text: " ".repeat(2 * 1024 * 1024 + 1),
    };
    let c = Catalog::build(&[file], &[], &Arc::new(AtomicBool::new(false))).unwrap();
    assert!(c.truncated);
    assert!(c.classes.is_empty());
    assert!(c.warnings.iter().any(|w| w.contains("2 MiB")));
}
#[test]
fn generic_and_namespace_shadowing_also_blocks_dotted_names() {
    let (_, c) = fixture(&[
        (
            "p/A.java",
            "package p; class Container { class Item {} } class Owner<Container> { Container.Item value; }",
        ),
        ("py/a.py", "class Item: pass\n"),
        (
            "py/owner.py",
            "import py.a as alias\nalias = factory()\nclass Owner:\n    value: alias.Item\n",
        ),
    ]);
    assert!(c.relations.iter().all(|r| r.target.is_none()));
}
#[test]
fn java_fully_qualified_and_python_init_relative_imports_are_scoped_candidates() {
    let (_, c) = fixture(&[
        ("q/A.java", "package q; class A {}"),
        ("p/B.java", "package p; class B { q.A value; }"),
        (
            "pkg/__init__.py",
            "from .model import A\nclass B:\n    value: A\n",
        ),
        ("pkg/model.py", "class A: pass\n"),
    ]);
    linked(&c, "p.B", "q.A", "field", "q.A");
    linked(&c, "pkg.B", "A", "field", "pkg.model.A");
}

#[test]
fn python_nested_classes_preserve_outer_generic_parameter_shadowing() {
    let (_, c) = fixture(&[(
        "a.py",
        "class T: pass\nclass Outer[T]:\n    class Inner:\n        value: T\n",
    )]);
    assert!(!c.truncated);
    assert!(c.relations.iter().all(|r| r.target.is_none()));
}
#[test]
fn candidate_limit_never_turns_large_duplicate_sets_into_unique_matches() {
    let mut files = vec![];
    for i in 0..34 {
        files.push((
            format!("duplicate{i}/A.java"),
            "package p; class A {}".to_owned(),
        ));
    }
    files.push((
        "Owner.java".into(),
        "package p; class Owner { A first; A second; Owner self; }".into(),
    ));
    let refs = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect::<Vec<_>>();
    let (_, c) = fixture(&refs);
    assert!(
        c.relations
            .iter()
            .filter(|r| r.type_name == "A")
            .all(|r| r.target.is_none()
                && r.candidate_ids.is_empty()
                && r.match_kind == "unmatched")
    );
    linked(&c, "p.Owner", "Owner", "field", "p.Owner");
}

#[test]
fn member_clipping_preserves_later_nested_and_duplicate_declaration_registry() {
    let early = format!(
        "package p; class Early {{ {} class Nested {{}} }} class Target {{}}",
        (0..300)
            .map(|i| format!("Target value{i};"))
            .collect::<String>()
    );
    let (_, c) = fixture(&[
        ("A.java", &early),
        (
            "Z.java",
            "package p; class Target {} class Owner { Target value; Early.Nested nested; }",
        ),
    ]);
    assert!(c.truncated);
    assert!(class(&c, "p.Early").truncated);
    linked(&c, "p.Owner", "Early.Nested", "field", "p.Early.Nested");
    assert!(
        c.relations
            .iter()
            .filter(|r| r.type_name == "Target")
            .all(|r| r.target.is_none()
                && r.match_kind == "unmatched"
                && r.candidate_ids.is_empty())
    );
    assert!(!c.warnings.iter().any(|w| w.contains("linking is disabled")));
}

#[test]
fn method_generic_detail_limit_does_not_stop_declarations_or_leak_partial_blockers() {
    let parameters = (0..257)
        .map(|i| format!("T{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let source = format!(
        "package p; class T256 {{}} class Owner {{ <{parameters}> T256 method(T256 value) {{ return value; }} T256 field; class Nested {{}} }} class Later {{ Owner.Nested value; }}"
    );
    let (_, c) = fixture(&[("A.java", &source)]);
    assert!(c.truncated);
    assert!(!c.warnings.iter().any(|w| w.contains("linking is disabled")));
    assert!(
        !c.relations
            .iter()
            .any(|r| r.kind == "returns" || r.kind == "parameter")
    );
    linked(&c, "p.Owner", "T256", "field", "p.T256");
    linked(&c, "p.Later", "Owner.Nested", "field", "p.Owner.Nested");
}

#[test]
fn cohort72_small_java_python_generated_below_cap() {
    let (_, catalog) = fixture(&[
        (
            "small/java/Csmall0000.java",
            include_str!("../fixtures/classes/below-cap/source/small/java/Csmall0000.java"),
        ),
        (
            "small/python/Csmall0000.py",
            include_str!("../fixtures/classes/below-cap/source/small/python/Csmall0000.py"),
        ),
    ]);
    assert!(!catalog.truncated, "{catalog:#?}");
    assert!(!catalog.classes.is_empty());
}

// The checked-in cap expectations are hand-authored from D0004 rules, not emitted
// by the composer. Tiny limits make equality and one-unit overflow observable.
fn cap_file(
    path: &str,
    source_bytes: usize,
    registry_bytes: usize,
    class: bool,
) -> trellis::model::FileExtraction {
    let mut definition: trellis::model::ClassDefinition = serde_json::from_value(
        serde_json::from_slice::<serde_json::Value>(include_bytes!("../fixtures/classes/below-cap/java_members_inheritance_generics_and_nested_classes_are_source_bound-0.json")).unwrap()["classes"][0].clone()
    ).unwrap();
    definition.symbol.path = path.into();
    definition.symbol.id = format!("class-{path}");
    definition.symbol.name = path.trim_end_matches(".java").into();
    definition.qualified_name = definition.symbol.name.clone();
    trellis::model::FileExtraction {
        path: path.into(),
        source_bytes,
        registry_bytes,
        warnings: vec![],
        truncated: false,
        registry_incomplete: false,
        classes: if class { vec![definition] } else { vec![] },
        items: if class { vec![cap_ref(path)] } else { vec![] },
    }
}
fn cap_ref(path: &str) -> trellis::model::DetailItem {
    use trellis::model::{ClassRelation, DetailItem, DetailValue};
    DetailItem {
        class_index: 0,
        records: 0,
        text: 0,
        value: DetailValue::Relation(ClassRelation {
            id: format!("ref-{path}"),
            owner: format!("class-{path}"),
            target: None,
            type_name: "Target".into(),
            kind: "field".into(),
            path: path.into(),
            range: Default::default(),
            candidate_ids: vec![],
            match_kind: "unmatched".into(),
        }),
    }
}
fn cap_expected(
    name: &str,
    files: &[trellis::model::FileExtraction],
    count: usize,
    symbols: usize,
    limits: trellis::classes::Limits,
) -> Catalog {
    let actual = Catalog::compose(files, count, symbols, limits).unwrap();
    let expected = fs::read(format!("tests/fixtures/classes/caps/{name}.json")).unwrap();
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        expected,
        "independent byte-exact normative fixture {name}"
    );
    actual
}
#[test]
fn hand_authored_workspace_cap_boundaries_and_moving_cut() {
    use trellis::{
        classes::Limits,
        model::{DetailItem, DetailValue},
    };
    let ordinary = Limits::default();
    let a = cap_file("A.java", 1, 1, true);
    let b = cap_file("B.java", 1, 1, true);
    let empty = cap_file("0.java", 1, 1, false);
    let warned = trellis::model::FileExtraction {
        warnings: vec!["excluded warning".into()],
        truncated: true,
        ..b.clone()
    };
    let mut limits = ordinary;
    limits.total_bytes = 1;
    cap_expected("source-equal", std::slice::from_ref(&a), 1, 1, limits);
    let result = cap_expected(
        "source-over",
        &[empty.clone(), warned.clone()],
        2,
        2,
        limits,
    );
    assert!(!result.warnings.iter().any(|w| w == "excluded warning"));
    limits = ordinary;
    limits.registry_text = 1;
    cap_expected("registry-equal", std::slice::from_ref(&a), 1, 1, limits);
    cap_expected(
        "registry-over",
        &[empty.clone(), warned.clone()],
        2,
        2,
        limits,
    );
    limits = ordinary;
    limits.classes = 1;
    cap_expected("classes-equal", std::slice::from_ref(&a), 1, 1, limits);
    cap_expected("classes-over", &[a.clone(), b.clone()], 2, 2, limits);
    let member: trellis::model::ClassMember=serde_json::from_value(
        serde_json::from_slice::<serde_json::Value>(include_bytes!("../fixtures/classes/below-cap/java_members_inheritance_generics_and_nested_classes_are_source_bound-0.json")).unwrap()["classes"][4]["fields"][0].clone()
    ).unwrap();
    let mut first = member.clone();
    first.name = "first".into();
    first.path = "A.java".into();
    let mut second = member;
    second.name = "second".into();
    second.path = "B.java".into();
    let mut detailed_a = a.clone();
    detailed_a.items = vec![
        cap_ref("A.java"),
        DetailItem {
            class_index: 0,
            records: 1,
            text: 1,
            value: DetailValue::Field(first),
        },
    ];
    let mut detailed_b = b.clone();
    detailed_b.items = vec![
        cap_ref("B.java"),
        DetailItem {
            class_index: 0,
            records: 1,
            text: 1,
            value: DetailValue::Field(second),
        },
    ];
    limits = ordinary;
    limits.records = 2;
    cap_expected(
        "records-equal",
        &[detailed_a.clone(), detailed_b.clone()],
        2,
        2,
        limits,
    );
    limits.records = 1;
    cap_expected(
        "records-over",
        &[detailed_a.clone(), detailed_b.clone()],
        2,
        2,
        limits,
    );
    limits = ordinary;
    limits.output_text = 2;
    cap_expected(
        "output-equal",
        &[detailed_a.clone(), detailed_b.clone()],
        2,
        2,
        limits,
    );
    limits.output_text = 1;
    cap_expected("output-over", &[detailed_a, detailed_b], 2, 2, limits);
    let mut inside = cap_file("A.java", 1, 1, true);
    let original: trellis::model::ClassMember = serde_json::from_value(
        serde_json::from_slice::<serde_json::Value>(include_bytes!("../fixtures/classes/below-cap/java_members_inheritance_generics_and_nested_classes_are_source_bound-0.json")).unwrap()["classes"][4]["fields"][0].clone()
    ).unwrap();
    inside.items = std::iter::once(cap_ref("A.java"))
        .chain(["first", "second"].into_iter().map(|name| {
            let mut m = original.clone();
            m.name = name.into();
            m.path = "A.java".into();
            DetailItem {
                class_index: 0,
                records: 1,
                text: 1,
                value: DetailValue::Field(m),
            }
        }))
        .collect();
    limits = ordinary;
    limits.records = 1;
    cap_expected("records-inside", &[inside.clone(), b.clone()], 2, 2, limits);
    limits = ordinary;
    limits.output_text = 1;
    cap_expected("output-inside", &[inside, b.clone()], 2, 2, limits);
    limits = ordinary;
    limits.files = 1;
    limits.symbols = 1;
    cap_expected("input-equal", std::slice::from_ref(&a), 1, 1, limits);
    cap_expected("input-over", std::slice::from_ref(&a), 2, 1, limits);
    limits = ordinary;
    limits.total_bytes = 2;
    let before = cap_expected("moving-before", &[a.clone(), b.clone()], 2, 2, limits);
    let mut grown = a;
    grown.source_bytes = 2;
    let before_f_b = serde_json::to_vec(&b).unwrap();
    // Produce B's per-file extraction independently of the earlier A growth;
    // comparing B to its own clone would prove nothing about the cut.
    let after_f_b = cap_file("B.java", 1, 1, true);
    let after_f_b_bytes = serde_json::to_vec(&after_f_b).unwrap();
    let after = cap_expected("moving-after", &[grown, after_f_b], 2, 2, limits);
    assert_eq!(
        before_f_b, after_f_b_bytes,
        "the later file's independent F must not change with earlier growth"
    );
    #[derive(serde::Serialize)]
    struct LaterProjection<'a> {
        classes: Vec<&'a trellis::model::ClassDefinition>,
        relations: Vec<&'a trellis::model::ClassRelation>,
    }
    let later_projection = |catalog: &Catalog| {
        serde_json::to_vec(&LaterProjection {
            classes: catalog
                .classes
                .iter()
                .filter(|c| c.symbol.path == "B.java")
                .collect(),
            relations: catalog
                .relations
                .iter()
                .filter(|r| r.path == "B.java")
                .collect(),
        })
        .unwrap()
    };
    let before_b = later_projection(&before);
    let after_b = later_projection(&after);
    assert_eq!(
        before_b,
        include_bytes!("../fixtures/classes/caps/moving-later-before.json")
    );
    assert_eq!(
        after_b,
        include_bytes!("../fixtures/classes/caps/moving-later-after.json")
    );
    assert_ne!(
        before_b, after_b,
        "moving cut must change the later file projection"
    );
}
#[test]
fn warnings_dedupe_at_100_then_append_ordered_closers() {
    let mut f = cap_file("A.java", 1, 1, true);
    f.warnings = (0..105).map(|n| format!("warning-{n}")).collect();
    f.warnings.push("warning-3".into());
    f.registry_incomplete = true;
    let c = Catalog::compose(&[f], 1, 1, trellis::classes::Limits::default()).unwrap();
    assert_eq!(c.warnings.len(), 102);
    assert_eq!(c.warnings[0], "warning-0");
    assert_eq!(c.warnings[99], "warning-99");
    assert!(c.warnings[100].starts_with("Declared types are terminal"));
    assert_eq!(
        c.warnings[101],
        "Incomplete class declaration registry: some declarations may be absent."
    );
}

#[test]
fn class_registry_import_charge_tiny_native_pilot() {
    use sha2::{Digest, Sha256};
    use trellis::{
        indexer::{IndexOptions, index_workspace_bundle},
        store::topology::WorkspaceIdentity,
    };
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path();
    eprintln!(
        "{}",
        serde_json::json!({"pilot":"class-registry-tiny","stage":"allocated",
        "workspace":workspace.display().to_string()})
    );
    std::io::Write::flush(&mut std::io::stderr()).unwrap();
    let long = format!("import {}.{};\n", "p".repeat(1023), "c".repeat(1024));
    let short = |prefix| format!("import {}.{};\n", "p".repeat(prefix), "c".repeat(60));
    let selected = "class Selected {}\n";
    assert_eq!(
        (
            long.len(),
            short(100).len(),
            short(101).len(),
            selected.len()
        ),
        (2057, 170, 171, 18)
    );
    fs::write(workspace.join("a00.java"), &long).unwrap();
    fs::write(workspace.join("z.java"), selected).unwrap();
    let identity = WorkspaceIdentity::discover(Some(workspace), workspace).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let mut selected_id = None;
    for prefix in [100, 101] {
        let short = short(prefix);
        fs::write(workspace.join("a01.java"), &short).unwrap();
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(workspace.to_owned()),
            &identity.record_id,
            &cancel,
            |_| {},
        )
        .unwrap();
        assert_eq!(graph.files.len(), 3);
        assert_eq!(native.revision.documents.len(), 3);
        assert_eq!(native.coverage.len(), 3);
        assert!(
            native.coverage.iter().all(|c| c.state == "complete"),
            "{:#?}",
            native.coverage
        );
        assert!(native.declarations.len() < 100 && graph.nodes.len() < 100);
        let symbol: Vec<_> = graph
            .nodes
            .iter()
            .filter(|s| {
                s.path == "z.java"
                    && s.name == "Selected"
                    && s.kind == trellis::model::SymbolKind::Class
            })
            .collect();
        assert_eq!(symbol.len(), 1, "measured class must be unique");
        let symbol = symbol[0];
        if let Some(id) = &selected_id {
            assert_eq!(id, &symbol.id, "unmodified class ID changed");
        }
        selected_id = Some(symbol.id.clone());
        let qualified = &symbol.name;
        let class_charge = symbol.id.len()
            + symbol.name.len()
            + symbol.path.len()
            + qualified.len()
            + symbol.name.len()
            + qualified.len();
        let expected = [
            ("a00.java", long.len(), 1024 + (1023 + 1 + 1024)),
            ("a01.java", short.len(), 60 + (prefix + 1 + 60)),
            ("z.java", selected.len(), class_charge),
        ];
        let mut files = Vec::new();
        for (path, size, charge) in expected {
            let file = graph.files.iter().find(|file| file.path == path).unwrap();
            let captured = capture.files.iter().find(|file| file.path == path).unwrap();
            let disk = fs::read(workspace.join(path)).unwrap();
            let sha = hex::encode(Sha256::digest(&disk));
            assert_eq!(disk.len(), size);
            assert_eq!(file.hash, sha);
            assert_eq!(captured.hash, sha);
            assert_eq!(captured.text.as_bytes(), disk);
            let document = native
                .revision
                .documents
                .iter()
                .find(|d| d.key.path == path)
                .unwrap();
            assert_eq!(document.content_hash, sha);
            assert_eq!(document.byte_length, size);
            let f = trellis::model::FileExtraction::extract_file(
                file,
                &graph.nodes,
                &cancel,
                trellis::classes::Limits::default(),
            )
            .unwrap();
            assert_eq!(f.source_bytes, size, "{path}");
            assert_eq!(f.registry_bytes, charge, "{path}");
            assert!(
                !f.truncated && !f.registry_incomplete && f.warnings.is_empty(),
                "{path}: {f:#?}"
            );
            files.push(serde_json::json!({"path":path,"sourceBytes":size,"sha256":sha,"registryBytes":charge}));
        }
        println!(
            "{}",
            serde_json::json!({"pilot":"class-registry-tiny","prefixBytes":prefix,
            "workspace":workspace.display().to_string(),"files":files,
            "nativeDocuments":native.revision.documents.len(),"nativeDeclarations":native.declarations.len(),
            "graphNodes":graph.nodes.len(),"selectedSymbolId":symbol.id,"selectedIdBytes":symbol.id.len()})
        );
    }
}

#[test]
fn production_class_limits_are_fixed_at_decision_0004_values() {
    let limits = trellis::classes::Limits::default();
    assert_eq!(limits.file_bytes, 2 * 1024 * 1024);
    assert_eq!(limits.total_bytes, 256 * 1024 * 1024);
    assert_eq!(limits.visits, 100_000);
    assert_eq!(limits.depth, 64);
    assert_eq!(limits.file_classes, 1_000);
    assert_eq!(limits.classes, 20_000);
    assert_eq!(limits.members, 256);
    assert_eq!(limits.file_refs, 8_192);
    assert_eq!(limits.records, 250_000);
    assert_eq!(limits.text, 2_048);
    assert_eq!(limits.output_text, 64 * 1024 * 1024);
    assert_eq!(limits.registry_text, 32 * 1024 * 1024);
    assert_eq!(limits.files, 100_000);
    assert_eq!(limits.symbols, 2_000_000);
}

#[test]
fn tiny_limits_control_measured_names_import_bindings_and_type_parameter_details() {
    use trellis::{
        classes::Limits,
        model::{DetailValue, FileExtraction},
    };
    let extract = |path: &str, source: &str, limits: Limits| {
        let work = tempfile::tempdir().unwrap();
        fs::write(work.path().join(path), source).unwrap();
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let graph =
            index_workspace(&IndexOptions::new(work.path().to_owned()), &cancel, |_| {}).unwrap();
        FileExtraction::extract_file(&graph.files[0], &graph.nodes, &cancel, limits).unwrap()
    };
    let default = Limits::default();
    let long_name = "class LongerName {}";
    assert_eq!(extract("A.java", long_name, default).classes.len(), 1);
    let short_text = Limits { text: 2, ..default };
    let clipped = extract("A.java", long_name, short_text);
    assert!(clipped.registry_incomplete);
    assert!(clipped.classes.is_empty());
    assert_eq!(clipped.warnings, ["Cl", "A."]);

    let import = "import verylongpackage.B; class A { B field; }";
    assert!(!extract("A.java", import, default).registry_incomplete);
    let clipped = extract("A.java", import, Limits { text: 5, ..default });
    assert!(clipped.registry_incomplete);
    assert_eq!(clipped.classes.len(), 1);
    assert_eq!(clipped.classes[0].symbol.name, "A");
    assert_eq!(clipped.warnings, ["Class"]);

    let class_parameters = "class A<T,U> {}";
    assert_eq!(
        extract("A.java", class_parameters, default).classes.len(),
        1
    );
    let clipped = extract(
        "A.java",
        class_parameters,
        Limits {
            members: 1,
            ..default
        },
    );
    assert!(clipped.registry_incomplete);
    assert!(clipped.classes.is_empty());
    assert!(
        clipped
            .warnings
            .iter()
            .any(|w| w.contains("Class type parameter limit reached (1/declaration)"))
    );

    let inherited = "class A[T]:\n    class B[U]: pass\n";
    assert_eq!(extract("a.py", inherited, default).classes.len(), 2);
    let clipped = extract(
        "a.py",
        inherited,
        Limits {
            members: 1,
            ..default
        },
    );
    assert!(clipped.registry_incomplete);
    assert_eq!(clipped.classes.len(), 1);
    assert!(
        clipped
            .warnings
            .iter()
            .any(|w| w.contains("Class inherited type parameter limit reached (1/scope)"))
    );

    let method = "class A { <T,U> A method(A value) { return value; } A field; class Nested {} }";
    let ordinary = extract("A.java", method, default);
    assert!(ordinary.items.iter().any(|item| matches!(&item.value,
        DetailValue::Relation(relation) if relation.kind == "returns")));
    let clipped = extract(
        "A.java",
        method,
        Limits {
            members: 1,
            ..default
        },
    );
    assert!(
        clipped
            .classes
            .iter()
            .any(|c| c.qualified_name == "A.Nested")
    );
    assert!(
        clipped
            .warnings
            .iter()
            .any(|w| w.contains("Method type parameter detail limit reached (1)"))
    );
    assert!(!clipped.items.iter().any(|item| matches!(&item.value,
        DetailValue::Relation(relation) if relation.kind == "returns" || relation.kind == "parameter")));
    assert!(clipped.items.iter().any(|item| matches!(&item.value,
        DetailValue::Relation(relation) if relation.kind == "field")));
}

#[test]
fn decision_0004_per_file_f_is_independent_of_other_workspace_documents() {
    use trellis::{classes::Limits, model::FileExtraction};
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("A.java");
    let b = temp.path().join("B.java");
    let stable = "class A { int f() { return 1; } }\n";
    fs::write(&a, stable).unwrap();
    fs::write(&b, "class B { int x() { return 1; } }\n").unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(temp.path().into());
    let selected_f = |graph: &Graph| {
        let file = graph
            .files
            .iter()
            .find(|file| file.path == "A.java")
            .unwrap();
        let symbols: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| node.path == "A.java")
            .cloned()
            .collect();
        FileExtraction::extract_file(file, &symbols, &cancel, Limits::default()).unwrap()
    };
    let before = index_workspace(&options, &cancel, |_| {}).unwrap();
    let f_before = selected_f(&before);
    fs::write(&b, "class B { int totallyDifferent() { return 78901; } }\n").unwrap();
    let after_unrelated_edit = index_workspace(&options, &cancel, |_| {}).unwrap();
    assert_eq!(
        selected_f(&after_unrelated_edit),
        f_before,
        "Decision 0004 F must not depend on unrelated workspace bytes or classes"
    );
    fs::write(&a, "class A { int f() { return 12345; } }\n").unwrap();
    let after_own_edit = index_workspace(&options, &cancel, |_| {}).unwrap();
    assert_ne!(
        selected_f(&after_own_edit),
        f_before,
        "F still authenticates its own source and class/method ranges"
    );
}
