fn calc(x: i32, y: bool) -> i32 {
    if y && x > 0 { return x; }
    match x {
        1 => 1,
        _ => 0,
    }
}
