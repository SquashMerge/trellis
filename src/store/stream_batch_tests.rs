//! #1313: a prepared publication batch is bounded by its raw spool bytes as well as by its
//! document count.
use super::next_stream_batch;

/// Every batch of `sizes` (raw bytes per document, in stream order), as the documents'
/// indexes into `sizes`, with `order` reversed so indexes and positions differ.
fn batches(sizes: &[usize], max_documents: usize, max_bytes: usize) -> Vec<Vec<usize>> {
    let order: Vec<usize> = (0..sizes.len()).rev().collect();
    let mut cursor = 0;
    let mut out = Vec::new();
    while cursor < order.len() {
        let before = cursor;
        let batch = next_stream_batch(&order, &mut cursor, max_documents, max_bytes, |index| {
            (index, sizes[index])
        });
        assert_eq!(cursor - before, batch.len());
        assert!(batch.iter().all(|(index, item)| index == item));
        out.push(batch.into_iter().map(|(index, _)| index).collect());
    }
    out
}

#[test]
fn a_batch_stops_once_its_raw_bytes_reach_the_cap() {
    // Documents of 3 MiB against an 8 MiB cap: a batch takes three (9 MiB, the third one
    // crossing the cap) and the next batch starts after them, well under the count cap.
    let mib = 1 << 20;
    let sizes = vec![3 * mib; 7];
    let got = batches(&sizes, 100, 8 * mib);
    assert_eq!(got, [vec![6, 5, 4], vec![3, 2, 1], vec![0]]);
    // Each batch holds less than the cap plus one document.
    for batch in &got {
        let bytes: usize = batch.iter().map(|&index| sizes[index]).sum();
        assert!(bytes < 8 * mib + 3 * mib, "{bytes}");
    }
}

#[test]
fn a_batch_keeps_its_document_count_cap_and_takes_an_oversized_document_alone() {
    let mib = 1 << 20;
    // Small documents: the count cap ends each batch.
    assert_eq!(
        batches(&[1; 5], 2, 8 * mib),
        [vec![4, 3], vec![2, 1], vec![0]]
    );
    // A document larger than the cap ends the batch it joins, and on its own it still forms
    // a batch, so a stream always advances.
    assert_eq!(
        batches(&[mib, 20 * mib, mib, mib], 100, 8 * mib),
        [vec![3, 2, 1], vec![0]]
    );
    assert_eq!(
        batches(&[20 * mib, 20 * mib], 100, 8 * mib),
        [vec![1], vec![0]]
    );
    // Nothing to read: no batch.
    let mut cursor = 0;
    assert!(next_stream_batch(&[], &mut cursor, 4, 8 * mib, |index| (index, 0)).is_empty());
    assert_eq!(cursor, 0);
}
