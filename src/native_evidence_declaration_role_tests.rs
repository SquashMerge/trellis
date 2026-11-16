use super::test_path;

#[test]
fn typescript_test_path_is_convention_only() {
    assert!(test_path("src/button.integration.test.ts"));
    assert!(test_path("src/button.spec.ts"));
    assert!(!test_path("src/button.ts"));
    assert!(test_path("src/button.spec.tsx"));
    assert!(test_path("tests/button.mts"));
    assert!(test_path("tests/types.d.ts"));
    assert!(test_path("src/button.test.d.ts"));
    assert!(!test_path("src/types.d.ts"));
    assert!(!test_path("src/button.d.mts"));
    assert!(test_path("tests/case.rs"));
    assert!(test_path("src/conftest.py"));
    assert!(!test_path("src/testing.rs"));
}
