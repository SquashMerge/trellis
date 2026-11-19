//! Owner ruling 2026-11-19 (#1234, option 2): native measurement of Rust enum variants as
//! `variant` declarations and of trait associated types as `alias` declarations.
use std::sync::{Arc, atomic::AtomicBool};
use trellis::native_evidence::{Artifact, Declaration};

fn measure(source: &str) -> Artifact {
    use trellis::{capture::Capture, indexer::IndexOptions, native_evidence::from_capture};
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), source).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    // `from_capture` validates its own build, so every row below passed the native validator.
    from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap()
}

fn named<'a>(artifact: &'a Artifact, kind: &str, name: &str) -> &'a Declaration {
    artifact
        .declarations
        .iter()
        .find(|d| d.kind == kind && d.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no {kind} declaration named {name}"))
}

fn text<'a>(source: &'a str, range: &trellis::native_evidence::Range) -> &'a str {
    &source[range.start..range.end]
}

/// The immediate parent declaration: the one whose key is the last ancestor, in the same
/// ancestry.
fn parent<'a>(artifact: &'a Artifact, d: &Declaration) -> &'a Declaration {
    let (key, outer) = d.ancestors.split_last().expect("a parent");
    artifact
        .declarations
        .iter()
        .find(|p| p.document == d.document && p.key == *key && p.ancestors == outer)
        .expect("parent declaration")
}

#[test]
fn rust_enum_variants_are_variant_declarations_under_their_enum() {
    let source = "pub enum Backend { Cpu, Metal(u8), Gpu { n: u8 }, #[doc = \"x\"] Npu = 4 }\n";
    let artifact = measure(source);
    let backend = named(&artifact, "type", "Backend");
    for (name, spelled) in [
        ("Cpu", "Cpu"),
        ("Metal", "Metal(u8)"),
        ("Gpu", "Gpu { n: u8 }"),
        ("Npu", "Npu = 4"),
    ] {
        let variant = named(&artifact, "variant", name);
        assert_eq!(variant.key.kind, "variant");
        assert_eq!(variant.header.kind, "variant");
        assert_eq!(variant.key.ordinal, 0, "{name}");
        assert!(variant.key.signature.is_none(), "{name}");
        assert_eq!(text(source, &variant.range), spelled, "{name}");
        assert_eq!(text(source, variant.name_range.as_ref().unwrap()), name);
        assert!(variant.lookup_key.is_some(), "{name}");
        assert_eq!(
            parent(&artifact, variant).syntax_id,
            backend.syntax_id,
            "{name}"
        );
        // The declaration-name reference candidate belongs to the variant's parent.
        let candidate = artifact
            .references
            .iter()
            .find(|r| Some(&r.range) == variant.name_range.as_ref())
            .unwrap();
        assert_eq!(candidate.owner_syntax_id, backend.syntax_id, "{name}");
    }
    assert_eq!(
        artifact
            .declarations
            .iter()
            .filter(|d| d.kind == "variant")
            .count(),
        4
    );
}

#[test]
fn rust_struct_variant_fields_take_the_variant_as_parent() {
    let source = "enum Shape { Rect { w: u8, h: u8 }, Unit }\nstruct Plain { w: u8 }\n";
    let artifact = measure(source);
    let rect = named(&artifact, "variant", "Rect");
    let fields: Vec<_> = artifact
        .declarations
        .iter()
        .filter(|d| d.kind == "field" && d.ancestors.last() == Some(&rect.key))
        .map(|d| d.name.as_deref().unwrap())
        .collect();
    assert_eq!(fields, ["w", "h"]);
    for field in artifact.declarations.iter().filter(|d| d.kind == "field") {
        let owner = parent(&artifact, field);
        let expected = if field.range.start < source.find("struct").unwrap() {
            rect.syntax_id.as_str()
        } else {
            named(&artifact, "type", "Plain").syntax_id.as_str()
        };
        assert_eq!(owner.syntax_id, expected, "{:?}", field.name);
    }
}

/// A variant is not an executable container: a call in its discriminant stays owned by the
/// enum, as before variants were measured.
#[test]
fn rust_enum_discriminant_call_stays_owned_by_the_enum() {
    let source = "const fn base() -> isize { 1 }\nenum E { A = base(), B }\n";
    let artifact = measure(source);
    let call = artifact
        .calls
        .iter()
        .find(|c| c.spelling.as_deref() == Some("base"))
        .unwrap();
    assert_eq!(
        call.owner_syntax_id,
        named(&artifact, "type", "E").syntax_id
    );
    assert!(
        artifact
            .control_regions
            .iter()
            .all(|r| r.owner_syntax_id != named(&artifact, "variant", "A").syntax_id)
    );
}

#[test]
fn rust_trait_associated_types_are_alias_declarations_like_impl_types() {
    let source = "pub trait Store {\n    type Operation;\n    type Index: Default + Clone;\n    type Page<'a> where Self: 'a;\n    fn f(&self) -> Self::Operation;\n}\nimpl Store for () {\n    type Operation = u8;\n    type Index = ();\n    type Page<'a> = &'a ();\n    fn f(&self) -> u8 { 0 }\n}\n";
    let artifact = measure(source);
    let store = named(&artifact, "type", "Store");
    let implementation = artifact
        .declarations
        .iter()
        .find(|d| d.kind == "implementation")
        .unwrap();
    for (name, in_trait, in_impl) in [
        ("Operation", "type Operation;", "type Operation = u8;"),
        ("Index", "type Index: Default + Clone;", "type Index = ();"),
        (
            "Page",
            "type Page<'a> where Self: 'a;",
            "type Page<'a> = &'a ();",
        ),
    ] {
        let aliases: Vec<_> = artifact
            .declarations
            .iter()
            .filter(|d| d.kind == "alias" && d.name.as_deref() == Some(name))
            .collect();
        assert_eq!(aliases.len(), 2, "{name}");
        for (alias, spelled, owner) in [
            (aliases[0], in_trait, &store.syntax_id),
            (aliases[1], in_impl, &implementation.syntax_id),
        ] {
            assert_eq!(text(source, &alias.range), spelled);
            assert_eq!(text(source, alias.name_range.as_ref().unwrap()), name);
            assert_eq!(&parent(&artifact, alias).syntax_id, owner, "{spelled}");
            assert!(alias.key.signature.is_none());
        }
    }
    assert_eq!(
        named(&artifact, "alias", "Page").header.type_parameters,
        ["'a"],
        "a generic associated type measures its type parameters"
    );
}
