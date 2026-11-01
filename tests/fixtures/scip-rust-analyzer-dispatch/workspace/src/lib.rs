pub mod shapes;

use shapes::{Shape, Square};

/// A trait object: rust-analyzer can only name the trait's declaration.
pub fn total(shape: &dyn Shape) -> u64 {
    shape.area()
}

/// A type parameter: rust-analyzer names the trait's provided method.
pub fn generic<T: Shape>(shape: &T) -> u64 {
    shape.twice()
}

pub fn concrete(side: u64) -> u64 {
    let square = Square::new(side);
    assert!(square.navigation_at(1) > 0);
    square.area() + total(&square) + generic(&square) + square.navigation_at(2)
}
