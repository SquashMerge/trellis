pub trait Shape {
    fn area(&self) -> u64;

    fn twice(&self) -> u64 {
        self.area() * 2
    }
}

pub struct Square {
    pub side: u64,
}

impl Square {
    pub fn new(side: u64) -> Self {
        Square { side: scaled(side) }
    }

    pub fn navigation_at(&self, scale: u64) -> u64 {
        self.navigation_in(scale)
    }

    fn navigation_in(&self, scale: u64) -> u64 {
        scaled(self.side) + scale
    }
}

impl Shape for Square {
    fn area(&self) -> u64 {
        scaled(self.side)
    }
}

fn scaled(value: u64) -> u64 {
    value.saturating_mul(2)
}
