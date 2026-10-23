mod nested {
    pub fn outer(flag: bool) {
        let callback = |n: i32| n + 1;
        if flag {
            for n in 0..2 {
                consume(callback(n));
            }
        }
    }
}
fn consume(_n: i32) {}
