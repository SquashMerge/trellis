fn calc(xs: &[i32], mut n: i32) -> i32 {
    for x in xs {
        if *x > 0 && n > 0 || n < 0 { n += 1; }
    }
    while n > 10 { n -= 1; }
    n
}
