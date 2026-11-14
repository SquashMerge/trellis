use super::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn snapshot() -> Snapshot {
    snapshot_of(&[
        (
            "src/lib.rs",
            "mod provider; pub use provider::value; pub fn run() { value(); }",
        ),
        ("src/provider.rs", "pub fn value() {}"),
        (
            "Use.java",
            "package app; import app.Provider; public class Use { Provider value; }",
        ),
        ("Provider.java", "package app; public class Provider {}"),
        ("use.py", "from provider import value\nvalue()\n"),
        ("provider.py", "def value():\n    return 1\n"),
        ("use.js", "import { value } from './provider.js'; value();"),
        ("provider.js", "export function value() {}"),
    ])
}

pub(super) fn snapshot_of(files: &[(&str, &str)]) -> Snapshot {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files.iter().copied() {
        let file = root.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let (graph, native) = crate::indexer::index_workspace_with_native(
        &crate::indexer::IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let documents = graph
        .files
        .into_iter()
        .map(|file| {
            let key = native
                .revision
                .documents
                .iter()
                .find(|doc| doc.key.path == file.path)
                .unwrap()
                .key
                .clone();
            let declarations = native
                .declarations
                .iter()
                .filter(|declaration| declaration.document == key)
                .cloned()
                .collect();
            let calls = native
                .calls
                .iter()
                .filter(|call| call.document == key)
                .cloned()
                .collect();
            DocumentInput {
                semantic: Default::default(),
                key,
                file,
                declarations,
                calls,
            }
        })
        .collect();
    Snapshot {
        pin: IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 1,
        },
        documents,
        manifests: Vec::new(),
        root_tsconfig: None,
        evidence_gaps: Default::default(),
    }
}

/// #556: a read selects from one whole-revision base instead of resolving its own paths.
#[test]
fn a_resolved_base_selects_what_a_scoped_projection_returns() {
    let snapshot = snapshot();
    let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
    for paths in [
        vec![],
        vec!["src/**".to_owned()],
        vec!["use.js".to_owned()],
        vec!["*.py".to_owned(), "Use.java".to_owned()],
        vec!["missing/**".to_owned()],
    ] {
        for limit in [1, 5, MAX_SAMPLE_LIMIT] {
            let scoped = project(&snapshot, &paths, limit, &mut || Ok(())).unwrap();
            let selected = base.select(&paths, limit, &mut || Ok(())).unwrap();
            assert_eq!(scoped.data, selected.data, "{paths:?} {limit}");
            assert_eq!(
                format!("{:?}", scoped.coverage),
                format!("{:?}", selected.coverage),
                "{paths:?} {limit}"
            );
        }
    }
}

/// #837: a `prefix` selects the sources under a directory, with or without `paths`, and
/// the scoped projection and the base's selection agree.
#[test]
fn a_prefix_selects_sources_under_a_directory() {
    let snapshot = snapshot();
    let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
    let sources = |data: &Value| -> BTreeSet<String> {
        ["production", "tests", "unresolved"]
            .iter()
            .flat_map(|field| data[*field].as_array().unwrap().clone())
            .map(|row| row["source"]["path"].as_str().unwrap().to_owned())
            .collect()
    };
    for (paths, prefix, scanned) in [
        (vec![], "src", 2),
        (vec!["**/lib.rs".to_owned()], "src", 1),
        (vec!["use.js".to_owned()], "src", 0),
        (vec![], "sr", 0),
        (vec![], "src/lib.rs", 0),
    ] {
        let scoped = project_under(&snapshot, &paths, Some(prefix), 5, &mut || Ok(())).unwrap();
        let selected = base
            .select_under(&paths, Some(prefix), 5, &mut || Ok(()))
            .unwrap();
        assert_eq!(scoped.data, selected.data, "{paths:?} {prefix}");
        assert_eq!(
            scoped.data["counts"]["scannedDocuments"], scanned,
            "{paths:?} {prefix}"
        );
        assert!(
            sources(&scoped.data)
                .iter()
                .all(|path| path.starts_with("src/")),
            "{paths:?} {prefix}"
        );
    }
    assert!(
        !sources(
            &project_under(&snapshot, &[], Some("src"), 5, &mut || Ok(()))
                .unwrap()
                .data
        )
        .is_empty()
    );
    for (prefix, normalized) in [
        ("src", Some("src")),
        ("src/", Some("src")),
        ("a/b c/", Some("a/b c")),
        ("src//", None),
        ("/", None),
        ("", None),
        ("..", None),
        ("a/./b", None),
        ("a\\b", None),
        ("a:b", None),
        ("a\0", None),
    ] {
        assert_eq!(directory_prefix(prefix), normalized, "{prefix:?}");
    }
}

#[test]
fn serial_and_parallel_keep_payload_and_exact_work_counts() {
    let snapshot = snapshot();
    for paths in [vec![], vec!["src/lib.rs".into(), "use.*".into()]] {
        let (serial, used) = project_configured(
            &snapshot,
            Sources {
                paths: &paths,
                prefix: None,
                files: None,
            },
            1,
            1,
            &mut || Ok(()),
        )
        .unwrap();
        assert!(used > 0);
        for workers in [2, 8] {
            let (parallel, parallel_used) = project_configured(
                &snapshot,
                Sources {
                    paths: &paths,
                    prefix: None,
                    files: None,
                },
                1,
                workers,
                &mut || Ok(()),
            )
            .unwrap();
            assert_eq!(parallel_used, used);
            assert_eq!(
                serde_json::to_vec(&parallel.data).unwrap(),
                serde_json::to_vec(&serial.data).unwrap()
            );
            assert_eq!(parallel.modules, serial.modules);
        }
    }
}

#[test]
fn parallel_parse_and_preparation_overlap_and_return_in_source_order() {
    let snapshot = snapshot();
    let docs = &snapshot.documents[..2];
    let (tx, rx) = std::sync::mpsc::channel();
    let rx = Mutex::new(rx);
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    let trees = map_batch(&[0usize, 1], 2, &request, |&index| {
        if index == 0 {
            rx.lock().unwrap().recv().unwrap();
        }
        let tree = parse(&docs[index], &mut request.worker())?;
        if index == 1 {
            tx.send(()).unwrap();
        }
        Ok(tree)
    })
    .unwrap();
    assert_eq!(trees[0].root_node().end_byte(), docs[0].file.text.len());
    assert_eq!(trees[1].root_node().end_byte(), docs[1].file.text.len());
    let prepared = map_batch(&[0usize, 1], 2, &request, |&index| {
        if index == 0 {
            rx.lock().unwrap().recv().unwrap();
        }
        let prepared = prepare_file(
            index,
            &docs[index],
            &trees[index],
            5_000_000,
            &mut request.worker(),
        )?;
        if index == 1 {
            tx.send(()).unwrap();
        }
        Ok(prepared)
    })
    .unwrap();
    assert_eq!(prepared[0].source, 0);
    assert_eq!(prepared[1].source, 1);
    assert!(prepared.iter().all(|file| file.work.used > 0));
}

#[test]
fn independent_catalog_jobs_overlap() {
    let snapshot = snapshot();
    let mut docs: Vec<_> = snapshot.documents.iter().collect();
    docs.sort_by(|a, b| a.key.cmp(&b.key));
    let trees: Vec<_> = docs
        .iter()
        .map(|doc| parse(doc, &mut || Ok(())).unwrap())
        .collect();
    let documents = docs
        .iter()
        .enumerate()
        .map(|(i, doc)| (doc.key.clone(), i))
        .collect();
    let selected: Vec<_> = (0..docs.len()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    let rx = Mutex::new(rx);
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    let catalogs = map_batch(&[0usize, 1], 2, &request, |&language| {
        let mut check = request.worker();
        let mut work = Work::new(5_000_000);
        if language == 0 {
            rx.lock().unwrap().recv().unwrap();
            let packages = external::RustPackages::new(&[], &mut work, &mut check)?;
            let catalog = rust::Catalog::new(
                &docs,
                &trees,
                &documents,
                &cargo_workspace::Workspace::default(),
                work,
                &mut check,
            )?;
            Ok(CatalogJob::Rust(Box::new((catalog, packages))))
        } else {
            let catalog = java::Catalog::new(&docs, &trees, &selected, &mut work, &mut check)?;
            tx.send(()).unwrap();
            Ok(CatalogJob::Java(Box::new(catalog)))
        }
    })
    .unwrap();
    assert!(matches!(&catalogs[0], CatalogJob::Rust(_)));
    assert!(matches!(&catalogs[1], CatalogJob::Java(_)));
}

/// One pass over every item: after a failure no later item starts, and the
/// reported error is the first in input order, not the first in time.
#[test]
fn single_pass_reports_first_failure_in_input_order_and_starts_nothing_later() {
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    let (failed_tx, failed_rx) = std::sync::mpsc::channel();
    let failed_rx = Mutex::new(failed_rx);
    let started = Mutex::new(Vec::new());
    let result = map_all(&[0usize, 1, 2, 3], 2, &request, |&index| -> Result<usize> {
        started.lock().unwrap().push(index);
        match index {
            // Fails only after item 1 has failed, and after every later item
            // had the chance to start on the other worker.
            0 => {
                failed_rx.lock().unwrap().recv().unwrap();
                anyhow::bail!("first source failure")
            }
            1 => {
                failed_tx.send(()).unwrap();
                anyhow::bail!("later source failure")
            }
            _ => Ok(index),
        }
    });
    assert_eq!(result.unwrap_err().to_string(), "first source failure");
    let mut started = started.into_inner().unwrap();
    started.sort();
    assert_eq!(started, vec![0, 1]);
    let all = map_all(&[0usize, 1, 2, 3], 2, &request, |&index| Ok(index * 10));
    assert_eq!(all.unwrap(), vec![0, 10, 20, 30]);
}

/// An earlier index claimed before a later failure, but not yet past the
/// skip check, still runs: the later typed failure is never masked.
#[test]
fn a_claimed_earlier_item_runs_after_a_later_typed_failure() {
    for message in [
        "bounded_work: reference site count",
        "bounded_work: reference owner lookup",
    ] {
        let mut check = || Ok(());
        let request = parallel_check::RequestCheck::new(&mut check);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let failed_rx = Mutex::new(failed_rx);
        let entered = |index: usize| {
            if index == 0 {
                failed_rx.lock().unwrap().recv().unwrap();
            }
        };
        let failed = |index: usize| {
            if index == 1 {
                failed_tx.send(()).unwrap();
            }
        };
        let ran = Mutex::new(Vec::new());
        let result = map_all_hooked(
            &[0usize, 1, 2],
            2,
            &request,
            &entered,
            &failed,
            |&index| -> Result<usize> {
                ran.lock().unwrap().push(index);
                anyhow::ensure!(index != 1, "{message}");
                Ok(index)
            },
        );
        assert_eq!(result.unwrap_err().to_string(), message);
        let mut ran = ran.into_inner().unwrap();
        ran.sort();
        assert_eq!(ran, vec![0, 1]);
        assert!(request.result(Ok(())).is_ok());
    }
}

#[test]
fn ordered_failure_stops_before_next_batch_and_work_is_bounded() {
    let visited = AtomicUsize::new(0);
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    let result: Result<()> = (|| {
        for batch in [0usize, 1, 2].chunks(2) {
            map_batch(batch, 2, &request, |&index| -> Result<()> {
                visited.fetch_add(1, Ordering::Relaxed);
                if index == 0 {
                    anyhow::bail!("first source failure")
                }
                anyhow::bail!("later source failure")
            })?;
        }
        Ok(())
    })();
    assert_eq!(result.unwrap_err().to_string(), "first source failure");
    assert_eq!(visited.load(Ordering::Relaxed), 2);
    let snapshot = snapshot();
    let doc = &snapshot.documents[0];
    let tree = parse(doc, &mut || Ok(())).unwrap();
    let error = prepare_file(0, doc, &tree, 0, &mut || Ok(()))
        .err()
        .unwrap();
    assert!(error.to_string().starts_with("bounded_work:"));
    let mut total = Work::new(3);
    merge_work(&mut total, 2, &mut || Ok(())).unwrap();
    let error = merge_work(&mut total, 2, &mut || Ok(())).unwrap_err();
    assert_eq!(error.to_string(), "bounded_work: reference work total");
    assert_eq!(total.used, 4);
}
