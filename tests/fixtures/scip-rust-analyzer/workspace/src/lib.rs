pub mod util;

/// A running total.
pub struct Counter {
    total: u64,
}

impl Counter {
    pub fn new() -> Self {
        Counter { total: 0 }
    }

    pub fn add(&mut self, amount: u64) -> u64 {
        self.total = util::saturating(self.total, amount);
        self.total
    }
}

pub fn run(values: &[u64]) -> u64 {
    let mut counter = Counter::new();
    for value in values {
        counter.add(*value);
    }
    util::double(counter.total)
}
