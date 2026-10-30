mod util;

/// Doubles `value` through the helper in the sibling module.
pub fn double_checked(value: u32) -> u32 {
    util::double(value)
}
