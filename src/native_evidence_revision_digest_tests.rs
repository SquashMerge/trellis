use super::*;
use crate::model::SourceFile;

/// The construction `native_revision_digest` replaces: the whole input
/// as one JSON value, canonicalized, then hashed.
fn materialized(
    source_set_id: &str,
    files: &[&SourceFile],
    toolchain: &Value,
    config: &[Value],
    dependencies: &[Value],
) -> String {
    let docs: Vec<Value> = files.iter().map(|f| json!({"language":f.language,"path":f.path,"contentHash":f.hash,"byteLength":f.text.len()})).collect();
    let revision_input = json!({"sourceSetId":source_set_id,"documents":docs,"toolchain":toolchain,"config":config,"dependencies":dependencies});
    digest(b"trellis.native-revision.v2\0", &canonical(&revision_input))
}

fn file(language: &str, path: &str, text: String) -> SourceFile {
    SourceFile {
        path: path.into(),
        hash: hex::encode(Sha256::digest(text.as_bytes())),
        language: language.into(),
        text,
    }
}

/// The streamed digest is byte-for-byte the materialized one, including
/// escaped paths, empty and multi-chunk bodies, and an empty corpus.
/// Decision 0010: the ID commits to every document through its content
/// hash, path and length; any change to one of them changes the ID.
#[test]
fn the_revision_id_changes_with_any_documents_content_or_path() {
    let toolchain = json!({"nativeExecutable":"abc"});
    let id = |files: &[SourceFile]| {
        let refs: Vec<&SourceFile> = files.iter().collect();
        native_revision_digest("source-set:v1:r", &refs, &toolchain, &[], &[])
    };
    let base = [
        file("rust", "src/a.rs", "fn a() {}\n".into()),
        file("java", "B.java", "class B {}\n".into()),
    ];
    assert_eq!(id(&base), id(&base.clone()));
    let mut content = base.clone();
    content[1] = file("java", "B.java", "class B { }\n".into());
    assert_ne!(id(&base), id(&content));
    let mut path = base.clone();
    path[0] = file("rust", "src/b.rs", "fn a() {}\n".into());
    assert_ne!(id(&base), id(&path));
}

#[test]
fn the_streamed_revision_digest_equals_the_materialized_one() {
    let toolchain = json!({"nativeExecutable":"abc","rootRustToolchain":{"path":"rust-toolchain","sha256":null},"rootRustToolchainToml":null});
    let config = vec![
        json!({"path":".gitignore","sha256":"x"}),
        json!({"nativeAdmission":{"maxFileBytes":2097152}}),
    ];
    let dependencies = vec![
        json!({"path":"Cargo.lock","bytes":null}),
        json!([1, "two", {"z": true, "a": false}]),
    ];
    let files = [
        file("rust", "src/a.rs", "fn a() {}\n".into()),
        file(
            "java",
            "dir with \"quotes\"\\and\\backslash.java",
            String::new(),
        ),
        file("python", "ctl\u{1}\u{1f}é漢字.py", "x = '\u{0}'\n".into()),
        file("javascript", "big.js", "€".repeat(9000)),
    ];
    let refs: Vec<&SourceFile> = files.iter().collect();
    for (set, docs) in [
        ("source-set:v1:r", &refs[..]),
        ("s\"et", &refs[..0]),
        ("source-set:v1:r", &refs[3..]),
    ] {
        assert_eq!(
            native_revision_digest(set, docs, &toolchain, &config, &dependencies),
            materialized(set, docs, &toolchain, &config, &dependencies),
            "source set {set}, {} documents",
            docs.len()
        );
    }
}
