// Closures, calls and items inside allowlisted macros, which a plain parse sees only as
// tokens (#551).
use anyhow::{Result, ensure};

struct Meter {
    reading: u32,
}

fn positive(value: u32) -> bool {
    value > 0
}

pub fn checked(values: &[u32]) -> Result<String> {
    ensure!(values.iter().any(|v| *v != 0), "all {} are zero", values.len());
    assert!(values.iter().all(|v| {
        if positive(*v) { true } else { *v == 0 }
    }));
    let label = format!("{}", values.iter().map(|v| v + 1).sum::<u32>());
    let rows = vec![values.iter().filter(|v| **v > 1).count(), values.len()];
    assert_eq!(rows.iter().map(|r| format!("{}", (|x: &usize| x + 1)(r))).count(), 2);
    Ok(label)
}

pub fn local_items() {
    assert!({
        fn countdown(n: u32) -> u32 {
            if n == 0 { 0 } else { countdown(n - 1) }
        }
        impl Meter {
            fn reading(&self) -> u32 {
                self.reading
            }
        }
        countdown(1) + Meter { reading: 1 }.reading() == 1
    });
}

pub fn outside(values: &[u32]) -> u32 {
    values.iter().map(|v| v * 2).sum()
}
