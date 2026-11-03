//! Cooperative polling for a request whose callback is Send but need not be Sync.
use anyhow::Result;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct RequestCheck<'a> {
    callback: Mutex<&'a mut (dyn FnMut() -> Result<()> + Send)>,
    error: Mutex<Option<anyhow::Error>>,
    pub(crate) stopped: AtomicBool,
    #[cfg(feature = "test-support")]
    pub(crate) visits: Option<std::sync::Arc<crate::syntax_bounds::visit_test::Context>>,
}

impl<'a> RequestCheck<'a> {
    pub(crate) fn new(callback: &'a mut (dyn FnMut() -> Result<()> + Send)) -> Self {
        Self {
            callback: Mutex::new(callback),
            error: Mutex::new(None),
            stopped: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            visits: None,
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn enter_visits(&self) -> crate::syntax_bounds::visit_test::Guard {
        crate::syntax_bounds::visit_test::enter(self.visits.clone())
    }

    pub(crate) fn check(&self) -> Result<()> {
        let mut callback = self.callback.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::Acquire) {
            anyhow::bail!("reference request stopped");
        }
        let callback_result = {
            // A caller may reenter Trellis here. Its operation is not part of this
            // request and must not inherit the test-only visit override or counters.
            #[cfg(feature = "test-support")]
            let _guard = crate::syntax_bounds::visit_test::enter(None);
            callback()
        };
        if let Err(error) = callback_result {
            *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
            self.stopped.store(true, Ordering::Release);
            anyhow::bail!("reference request stopped");
        }
        Ok(())
    }

    pub(crate) fn worker(&self) -> impl FnMut() -> Result<()> + Send + '_ {
        let mut polls = 0usize;
        move || {
            if self.stopped.load(Ordering::Acquire) {
                anyhow::bail!("reference request stopped");
            }
            if polls == 0 {
                self.check()?;
            }
            polls = (polls + 1) % 256;
            Ok(())
        }
    }

    // Call only after every worker has joined; only the owner consumes the error.
    pub(crate) fn result<T>(&self, result: Result<T>) -> Result<T> {
        match self.error.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(error) => Err(error),
            None => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[cfg(feature = "test-support")]
    #[test]
    fn callback_reentry_during_walk_uses_default_bound_without_parent_observations() {
        use crate::syntax_bounds::{self, visit_test};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        fn run(reenter: bool) -> (Vec<visit_test::Observation>, usize, usize) {
            let source = format!("[{}];", "0,".repeat(900));
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_javascript::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(&source, None).unwrap();
            let context = Arc::new(visit_test::Context {
                limit: 512,
                observations: Mutex::new(Vec::new()),
            });
            let visited = Arc::new(AtomicUsize::new(0));
            let nested_visits = Arc::new(AtomicUsize::new(0));
            let callback_context = Arc::clone(&context);
            let callback_visited = Arc::clone(&visited);
            let callback_nested = Arc::clone(&nested_visits);
            let mut callback = move || {
                if reenter
                    && callback_visited.load(Ordering::SeqCst) >= 256
                    && callback_nested.load(Ordering::SeqCst) == 0
                {
                    // The real reference walk has entered at least 256 nodes.
                    let before = callback_context.observations.lock().unwrap().len();
                    assert_eq!(syntax_bounds::visit_limit(), syntax_bounds::VISITS);
                    let nested_source = format!("[{}];", "0,".repeat(700));
                    let mut nested_parser = tree_sitter::Parser::new();
                    nested_parser
                        .set_language(&tree_sitter_javascript::LANGUAGE.into())
                        .unwrap();
                    let nested_tree = nested_parser.parse(&nested_source, None).unwrap();
                    let mut checks = 0;
                    let gap = syntax_bounds::scan(nested_tree.root_node(), &mut || {
                        checks += 1;
                        Ok(())
                    })?;
                    assert!(gap.is_none(), "nested scan inherited the parent's bound");
                    assert!(checks > 512, "nested scan did not cross the parent bound");
                    callback_nested.store(checks, Ordering::SeqCst);
                    assert_eq!(callback_context.observations.lock().unwrap().len(), before);
                }
                Ok(())
            };
            let mut request = RequestCheck::new(&mut callback);
            request.visits = Some(Arc::clone(&context));
            {
                let _guard = request.enter_visits();
                super::super::walk(tree.root_node(), &mut request.worker(), &mut |_, _| {
                    visited.fetch_add(1, Ordering::SeqCst);
                    Ok(false)
                })
                .unwrap();
            }
            let observations = context.observations.lock().unwrap().clone();
            (
                observations,
                visited.load(Ordering::SeqCst),
                nested_visits.load(Ordering::SeqCst),
            )
        }

        let (baseline, baseline_visits, _) = run(false);
        let (reentered, reentered_visits, nested_visits) = run(true);
        assert_eq!(
            baseline, reentered,
            "nested scan contaminated parent counts"
        );
        assert_eq!(baseline_visits, 512);
        assert_eq!(reentered_visits, baseline_visits);
        assert_eq!(baseline.len(), 1);
        assert_eq!((baseline[0].kind, baseline[0].visits), ("walk", 513));
        assert!(
            nested_visits > 512,
            "callback did not reenter the nested scan"
        );
        eprintln!(
            "parent walk visits=513; nested scan checks={nested_visits}; observations identical"
        );
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn callback_context_is_restored_after_panic() {
        use crate::syntax_bounds::{self, visit_test};
        use std::sync::Arc;
        let context = Arc::new(visit_test::Context {
            limit: 512,
            observations: Mutex::new(Vec::new()),
        });
        let mut callback = || -> anyhow::Result<()> { panic!("test callback panic") };
        let mut request = RequestCheck::new(&mut callback);
        request.visits = Some(context);
        {
            let _guard = request.enter_visits();
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| request.check())).is_err()
            );
            assert_eq!(syntax_bounds::visit_limit(), 512);
        }
        assert_eq!(syntax_bounds::visit_limit(), syntax_bounds::VISITS);
    }

    #[derive(Debug)]
    struct CallerError;
    impl std::fmt::Display for CallerError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("caller-specific failure")
        }
    }
    impl std::error::Error for CallerError {}

    #[test]
    fn arbitrary_callback_error_survives_parallel_cancellation() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let observed = &calls;
        let local = Cell::new(0usize); // Send, deliberately not Sync.
        let mut callback = move || {
            local.set(local.get() + 1);
            observed.fetch_add(1, Ordering::Relaxed);
            Err(CallerError.into())
        };
        let request = RequestCheck::new(&mut callback);
        let results =
            crate::parallel::map_ordered(&[0, 1], 2, &request.stopped, |_, _| request.worker()());
        let error = request.result(Ok(results)).unwrap_err();
        assert!(error.downcast_ref::<CallerError>().is_some());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_late_worker_error_stops_peers_and_keeps_original_error() {
        let armed = AtomicBool::new(false);
        let local = Cell::new(0);
        let armed_ref = &armed;
        let mut callback = move || {
            local.set(local.get() + 1);
            if armed_ref.load(Ordering::Acquire) {
                Err(CallerError.into())
            } else {
                Ok(())
            }
        };
        let request = RequestCheck::new(&mut callback);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let started_rx = Mutex::new(started_rx);
        let failed_rx = Mutex::new(failed_rx);
        let results = crate::parallel::map_ordered(&[0, 1], 2, &request.stopped, |index, _| {
            let mut check = request.worker();
            check()?;
            if index == 0 {
                started_tx.send(()).unwrap();
                failed_rx.lock().unwrap().recv().unwrap();
                check()
            } else {
                started_rx.lock().unwrap().recv().unwrap();
                armed.store(true, Ordering::Release);
                let result = request.check();
                failed_tx.send(()).unwrap();
                result
            }
        });
        assert!(results.iter().all(|slot| slot.as_ref().unwrap().is_err()));
        let error = request.result(Ok(())).unwrap_err();
        assert!(error.downcast_ref::<CallerError>().is_some());
    }

    #[test]
    fn late_worker_cancellation_and_deadline_errors_are_unchanged() {
        for code in ["cancelled", "deadline_exceeded"] {
            let armed = AtomicBool::new(false);
            let mut callback = || {
                anyhow::ensure!(!armed.load(Ordering::Acquire), "{code}");
                Ok(())
            };
            let request = RequestCheck::new(&mut callback);
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (failed_tx, failed_rx) = std::sync::mpsc::channel();
            let started_rx = Mutex::new(started_rx);
            let failed_rx = Mutex::new(failed_rx);
            let results = crate::parallel::map_ordered(&[0, 1], 2, &request.stopped, |index, _| {
                let mut check = request.worker();
                check()?;
                if index == 0 {
                    started_tx.send(()).unwrap();
                    failed_rx.lock().unwrap().recv().unwrap();
                    check()
                } else {
                    started_rx.lock().unwrap().recv().unwrap();
                    armed.store(true, Ordering::Release);
                    let result = request.check();
                    failed_tx.send(()).unwrap();
                    result
                }
            });
            assert!(results.iter().all(|slot| slot.as_ref().unwrap().is_err()));
            assert_eq!(request.result(Ok(())).unwrap_err().to_string(), code);
        }
    }
}
