mod common;

fn run() -> u32 {
    common::shared()
}

#[test]
fn local_run() {
    let value = run();
    assert_eq!(value, 41);
}
