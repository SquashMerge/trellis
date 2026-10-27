//! Cooperative polling for a request whose callback is Send but need not be Sync.
use anyhow::Result;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct RequestCheck<'a> {
    callback: Mutex<&'a mut (dyn FnMut() -> Result<()> + Send)>,
    error: Mutex<Option<anyhow::Error>>,
    pub(crate) stopped: AtomicBool,
}

impl<'a> RequestCheck<'a> {
    pub(crate) fn new(callback: &'a mut (dyn FnMut() -> Result<()> + Send)) -> Self {
        Self {
            callback: Mutex::new(callback),
            error: Mutex::new(None),
            stopped: AtomicBool::new(false),
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        let mut callback = self.callback.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::Acquire) {
            anyhow::bail!("reference request stopped");
        }
        if let Err(error) = callback() {
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
