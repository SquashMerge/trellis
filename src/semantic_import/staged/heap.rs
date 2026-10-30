//! Requested heap bytes of values that already exist, read from their real capacities, and a
//! node-level bound for `BTreeMap`/`BTreeSet`. A live owner measures what it keeps with these
//! after its build, so its long-lived charge is its retained storage rather than the build's
//! peak. Counts what Rust requests from the allocator (layout sizes), not allocator slack, RSS
//! or SQLite/tree-sitter C memory.
use std::mem::{align_of, size_of};

/// `std`'s B-tree order: a node holds at most 11 entries and an internal node 12 edges.
const CAPACITY: u64 = 11;
/// A non-root node keeps at least 5 entries.
const MIN_LEN: u64 = 5;

fn round_up(bytes: u64, align: u64) -> u64 {
    bytes.div_ceil(align) * align
}

/// Bytes of one `std` B-tree leaf node of `K`/`V`: the parent link, the parent index and
/// length (two `u16`), then 11 keys and 11 values, padded to the node alignment.
pub(in crate::semantic_import) fn btree_leaf<K, V>() -> u64 {
    let align = align_of::<K>()
        .max(align_of::<V>())
        .max(align_of::<usize>()) as u64;
    round_up(
        size_of::<usize>() as u64 + 4 + CAPACITY * (size_of::<K>() + size_of::<V>()) as u64,
        align,
    )
}

/// Bytes of one internal node: a leaf plus 12 child pointers.
fn btree_internal<K, V>() -> u64 {
    btree_leaf::<K, V>() + (CAPACITY + 1) * size_of::<usize>() as u64
}

/// An upper bound on the node storage of a `std` `BTreeMap<K, V>` (or `BTreeSet<K>`, with
/// `V = ()`) of `len` entries, whatever the insertion order. Exact up to 11 entries (one leaf;
/// none when empty). Above that every non-root leaf holds at least 5 entries and every leaf
/// but the last separates from the next by one entry in an internal node, so there are at most
/// `(len + 1) / 6` leaves; every non-root internal node has at least 6 children. Typical maps
/// fill their nodes further: the bound is about 1.2x an ascending build and 1.4x a random one.
pub(in crate::semantic_import) fn btree<K, V>(len: usize) -> u64 {
    let len = len as u64;
    if len == 0 {
        return 0;
    }
    if len <= CAPACITY {
        return btree_leaf::<K, V>();
    }
    let leaves = (len + 1) / (MIN_LEN + 1);
    // Levels of internal nodes over `leaves` children, each non-root one with >= 6 children.
    let mut internal = 0;
    let mut level = leaves;
    while level > 1 {
        level = level.div_ceil(MIN_LEN + 1).max(1);
        internal += level;
        if level == 1 {
            break;
        }
    }
    leaves * btree_leaf::<K, V>() + internal * btree_internal::<K, V>()
}

/// The heap buffer of a `String`.
pub(in crate::semantic_import) fn string(value: &String) -> u64 {
    value.capacity() as u64
}

pub(in crate::semantic_import) fn optional(value: &Option<String>) -> u64 {
    value.as_ref().map_or(0, string)
}

/// The buffer of a `Vec<T>`, not its elements' own heap storage.
pub(in crate::semantic_import) fn buffer<T>(values: &Vec<T>) -> u64 {
    (values.capacity() * size_of::<T>()) as u64
}

pub(in crate::semantic_import) fn strings(values: &Vec<String>) -> u64 {
    values.iter().map(string).sum::<u64>() + buffer(values)
}

/// The heap storage of a `serde_json::Value` tree. With `arbitrary_precision` a number keeps
/// its decimal text: an integer that fits 64 bits exactly as long as its digits, any other
/// number in the scanner's buffer, which starts at 16 bytes and doubles (counted with room
/// for a longer spelling than the one it prints as).
pub(in crate::semantic_import) fn value(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) => 0,
        serde_json::Value::Number(number) => {
            let digits = number.to_string().len() as u64;
            if number.is_i64() || number.is_u64() {
                digits
            } else {
                (2 * digits).next_power_of_two().max(16)
            }
        }
        serde_json::Value::String(text) => string(text),
        serde_json::Value::Array(values) => {
            buffer(values) + values.iter().map(self::value).sum::<u64>()
        }
        serde_json::Value::Object(entries) => {
            btree::<String, serde_json::Value>(entries.len())
                + entries
                    .iter()
                    .map(|(key, entry)| string(key) + self::value(entry))
                    .sum::<u64>()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_alloc::measure;
    use std::collections::{BTreeMap, BTreeSet};

    fn built<K: Ord, V>(entries: impl Iterator<Item = (K, V)>) -> (u64, BTreeMap<K, V>) {
        let mut map = BTreeMap::new();
        let ((), bytes) = measure(|| {
            for (key, value) in entries {
                map.insert(key, value);
            }
        });
        (bytes, map)
    }

    /// The bound holds for every size and insertion order (and a bulk `collect`), is exact for
    /// one node, and stays within 1.6x of real node storage built by insertion, 2.25x by bulk.
    #[test]
    fn btree_bound_covers_real_nodes_within_stated_ratios() {
        for len in (0..=40).chain([100, 1_000, 10_000, 50_000]) {
            let ascending: Vec<u64> = (0..len).collect();
            let descending: Vec<u64> = (0..len).rev().collect();
            let scattered: Vec<u64> = (0..len)
                .map(|i| i.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x5555)
                .collect();
            let bound = btree::<u64, [u8; 40]>(len as usize);
            for keys in [&ascending, &descending, &scattered] {
                let (real, _map) = built(keys.iter().map(|k| (*k, [0u8; 40])));
                assert!(real <= bound, "len {len}: {real} > {bound}");
                if len <= 11 {
                    assert_eq!(real, bound, "one node is exact");
                } else {
                    assert!(bound * 5 <= real * 8, "len {len}: {bound} vs {real}");
                }
                let (set, _set) = built(keys.iter().map(|k| (*k, ())));
                assert!(set <= btree::<u64, ()>(len as usize));
            }
            let (_map, bulk) = measure(|| {
                scattered
                    .iter()
                    .map(|k| (*k, [0u8; 40]))
                    .collect::<BTreeMap<_, _>>()
            });
            // `collect` sorts into a Vec first; only the map's nodes remain.
            assert!(
                bulk <= bound && bound * 4 <= bulk * 9,
                "len {len}: {bound} vs {bulk}"
            );
        }
        let (_set, one) = measure(|| BTreeSet::from([String::new()]));
        assert_eq!(one, btree::<String, ()>(1));
    }

    #[test]
    fn value_storage_is_what_serde_requests() {
        let text = br#"{"a":[1,22,{"b":"text","c":null}],"d":{"e":true,"f":-325},"g":"\u00e9"}"#;
        let (parsed, bytes) =
            measure(|| serde_json::from_slice::<serde_json::Value>(text).unwrap());
        assert_eq!(value(&parsed), bytes);
        for text in ["-3.25e10", "1.5", "12345678901234567890123", "1e5"] {
            let (parsed, bytes) =
                measure(|| serde_json::from_str::<serde_json::Value>(text).unwrap());
            assert!(value(&parsed) >= bytes, "{text}");
        }
    }

    #[test]
    fn capacities_are_what_was_requested() {
        let (value, bytes) = measure(|| {
            let mut values = Vec::with_capacity(3);
            values.push(String::with_capacity(17));
            values.push("abc".to_owned());
            values
        });
        assert_eq!(strings(&value), bytes);
    }
}
