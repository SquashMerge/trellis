use super::*;
use crate::native_evidence::Range;

#[test]
fn indexed_enclosure_matches_linear_ranges_and_stored_order() {
    let ranges = [
        Range { start: 0, end: 100 },
        Range { start: 20, end: 40 },
        Range { start: 5, end: 30 },
        Range { start: 20, end: 40 },
        Range { start: 40, end: 40 },
        Range { start: 60, end: 80 },
    ];
    let mut work = Work::new(5_000_000);
    let index = RangeIndex::new(ranges.iter(), &mut work, &mut || Ok(())).unwrap();
    for start in 0..=101 {
        for end in start..=101 {
            let expected: Vec<_> = ranges
                .iter()
                .enumerate()
                .filter(|(_, r)| r.start <= start && r.end >= end)
                .map(|(i, _)| i)
                .collect();
            let mut found = index
                .enclosing(start, end, &mut work, &mut || Ok(()))
                .unwrap();
            // Call enclosure chooses the first stored call, not the narrowest.
            assert_eq!(found.iter().min(), expected.first());
            found.sort_unstable();
            assert_eq!(found, expected, "{start}..{end}");
        }
    }
}

#[test]
fn range_index_work_is_bounded_and_cancellable() {
    let ranges = [Range { start: 0, end: 100 }];
    let mut work = Work::new(5_000_000);
    let index = RangeIndex::new(ranges.iter(), &mut work, &mut || Ok(())).unwrap();
    work.used = work.limit;
    let error = index
        .enclosing(5, 10, &mut work, &mut || Ok(()))
        .unwrap_err();
    assert_eq!(error.to_string(), "bounded_work: reference owner lookup");
    let mut checks = 0;
    let error = RangeIndex::new(ranges.iter(), &mut Work::new(5_000_000), &mut || {
        checks += 1;
        ensure!(checks < 3, "cancelled");
        Ok(())
    })
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "cancelled");
    let error = index
        .enclosing(5, 10, &mut Work::new(5_000_000), &mut || {
            anyhow::bail!("cancelled")
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
}
