fn main() {
    let total = fixture::run();
    report(total);
}

fn report(total: u32) {
    println!("{total}");
}
