//! #1246 PR 5: the streamed publisher prepares batch b+1 while it writes batch b.
//! A prepared batch's failure is still returned only at its own turn, after every
//! earlier batch is written, typed, and with nothing published.
use super::*;
use crate::indexer::IndexOptions;
use std::{fs, sync::atomic::AtomicBool};

/// A fresh store over two full stream batches and one more document, whose
/// captured (ordinal) order is their path order.
fn workspace() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    IndexOptions,
    usize,
) {
    let batch_len = crate::parallel::default_workers() * 16;
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    for i in 0..=2 * batch_len {
        fs::write(
            work.path().join(format!("f{i:04}.js")),
            format!("function f{i}(){{ return {i}; }}\n"),
        )
        .unwrap();
    }
    let options = IndexOptions::new(work.path().to_owned());
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    (state, work, store, options, batch_len)
}

/// Publish the workspace through the streamed publisher, with the graph rows of
/// the documents at `corrupt` ordinals made undecodable in the spool first.
fn publish_streamed(
    store: &Store,
    options: &IndexOptions,
    corrupt: &[usize],
    during_tx: impl FnMut(PublishStage, &Connection) -> Result<()>,
) -> Result<IndexPin> {
    let cancel = Arc::new(AtomicBool::new(false));
    let leader = store.writer()?;
    let capture = crate::capture::Capture::admit(options, &cancel, &|_| {})?;
    let spool = crate::native_evidence::spool_full_capture(
        options,
        &capture,
        &options.workspace_root,
        store.root_id(),
        &cancel,
        &|_| {},
        |_, _| {},
        &store.native_spool_dir()?,
    )?;
    for &ordinal in corrupt {
        let path = &capture.files()[ordinal].path;
        let index = SpoolReader::header(&spool)
            .revision
            .documents
            .iter()
            .position(|document| &document.key.path == path)
            .unwrap();
        SpoolReader::corrupt_graph_json_for_tests(&spool, index);
    }
    store.publish_validated_stream_target(
        &spool,
        &capture,
        &leader,
        store.publication_index_baseline()?,
        PublicationTarget::Live,
        &cancel,
        None,
        during_tx,
    )
}

fn published_revisions(store: &Store) -> i64 {
    store
        .cache_write()
        .unwrap()
        .query_row("SELECT count(*) FROM native_revisions", [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// Batch 1 is decoded while batch 0 is written, but its decode failure is returned
/// only after all of batch 0 is written, before any of batch 1 is; a later batch's
/// failure is never reached. The failure is typed and publishes nothing.
#[test]
fn a_prepared_batch_failure_is_returned_at_its_turn_without_publishing() {
    let (_state, _work, store, options, batch_len) = workspace();
    let baseline = store.index_baseline().unwrap();
    let mut written = 0;
    let error = publish_streamed(
        &store,
        &options,
        &[batch_len + 1, 2 * batch_len],
        |stage, _| {
            if stage == PublishStage::AfterFile {
                written += 1;
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(
        error.downcast_ref::<NativeSpoolReadFailure>().is_some(),
        "{error:#}"
    );
    assert_eq!(written, batch_len, "{error:#}");
    assert_eq!(store.index_baseline().unwrap(), baseline);
    assert_eq!(published_revisions(&store), 0);

    let pin = publish_streamed(&store, &options, &[], |_, _| Ok(())).unwrap();
    assert_eq!(pin.index_revision, 1);
    assert_eq!(
        store.graph_at(Some(pin)).unwrap().files.len(),
        2 * batch_len + 1
    );
}

/// A failure while batch 0 is written wins over batch 1's decode failure, which was
/// already found while batch 0 was being written: the serial publisher's order.
#[test]
fn an_earlier_batch_write_failure_precedes_a_prepared_batch_failure() {
    let (_state, _work, store, options, batch_len) = workspace();
    let baseline = store.index_baseline().unwrap();
    let mut written = 0;
    let error = publish_streamed(&store, &options, &[batch_len], |stage, _| {
        if stage == PublishStage::AfterFile {
            written += 1;
            ensure!(written < batch_len, "injected batch 0 write failure");
        }
        Ok(())
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("injected batch 0 write failure"),
        "{error:#}"
    );
    assert!(error.downcast_ref::<NativeSpoolReadFailure>().is_none());
    assert_eq!(store.index_baseline().unwrap(), baseline);
    assert_eq!(published_revisions(&store), 0);
}
