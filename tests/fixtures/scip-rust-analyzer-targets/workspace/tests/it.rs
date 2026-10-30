mod common;

#[test]
fn library_runs() {
    let value = fixture::run();
    assert_eq!(value, common::shared() + 1);
}
