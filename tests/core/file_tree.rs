use trellis::file_tree::{SCAN_LIMIT, SourceDir};
#[test]
fn shallow_metadata_sorted_pages_and_validation() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("z-dir")).unwrap();
    std::fs::write(temp.path().join("a.rs"), "private content").unwrap();
    std::fs::write(temp.path().join(".hidden"), "secret").unwrap();
    let dir = SourceDir::open(temp.path()).unwrap();
    let (items, next, truncated) = dir.list("", 0, 1).unwrap();
    assert_eq!(items[0].name, "z-dir");
    assert_eq!(next, Some(1));
    assert!(!truncated);
    let (items, next, _) = dir.list("", 1, 200).unwrap();
    assert_eq!(
        items.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        [".hidden", "a.rs"]
    );
    assert!(items.iter().all(|e| e.indexed_path.is_none()));
    assert_eq!(next, None);
    for path in [
        "/tmp", "..", "z-dir/..", "./z-dir", "z-dir/", "z-dir//x", "a\\b", "a\0b", "C:foo",
    ] {
        assert!(dir.list(path, 0, 200).is_err(), "{path:?}");
    }
    assert!(dir.list("a.rs", 0, 200).is_err());
    assert!(dir.list("missing", 0, 200).is_err());
    assert!(dir.list("", 0, 201).is_err());
    assert!(dir.list("", SCAN_LIMIT + 1, 200).is_err());
}
#[cfg(unix)]
#[test]
fn symlinks_are_metadata_never_traversed_even_after_rename() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(outside.path().join("sub")).unwrap();
    symlink(outside.path(), temp.path().join("alias")).unwrap();
    symlink("missing", temp.path().join("broken")).unwrap();
    std::fs::create_dir(temp.path().join("real")).unwrap();
    let dir = SourceDir::open(temp.path()).unwrap();
    let (items, _, _) = dir.list("", 0, 200).unwrap();
    assert_eq!(
        items.iter().find(|e| e.name == "alias").unwrap().kind,
        "symlink"
    );
    assert_eq!(
        items.iter().find(|e| e.name == "broken").unwrap().kind,
        "symlink"
    );
    for path in ["alias", "alias/sub", "broken"] {
        assert!(dir.list(path, 0, 200).is_err());
    }
    std::fs::remove_dir(temp.path().join("real")).unwrap();
    symlink(outside.path(), temp.path().join("real")).unwrap();
    assert!(dir.list("real/sub", 0, 200).is_err());
}
#[test]
fn scan_is_bounded_and_explicit() {
    let temp = tempfile::tempdir().unwrap();
    for n in 0..SCAN_LIMIT + 1 {
        std::fs::write(temp.path().join(format!("{n:05}")), "").unwrap();
    }
    let dir = SourceDir::open(temp.path()).unwrap();
    let (items, next, truncated) = dir.list("", 0, 200).unwrap();
    assert_eq!(items.len(), 200);
    assert_eq!(next, Some(200));
    assert!(truncated);
    assert!(items.windows(2).all(|w| w[0].name < w[1].name));
}
