#[test]
fn both_libraries() {
    let first = alpha::run();
    let second = beta::run();
    assert_eq!(first + second, 3);
}
