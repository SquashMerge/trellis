/// Adds without overflowing.
pub fn saturating(left: u64, right: u64) -> u64 {
    left.saturating_add(right)
}

pub fn double(value: u64) -> u64 {
    saturating(value, value)
}
