#[test]
fn test_calc() {
    let callback = |x: i32| if x > 0 { 1 } else { 0 };
    assert_eq!(callback(1), 1);
}
fn calc(n: i32) -> i32 {
    if n > 0 { calc(n - 1) } else { 0 }
}
