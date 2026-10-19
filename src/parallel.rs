//! Bounded, order-preserving parallel map for pure per-item CPU work.
//!
//! Results land in the slot of their input index, never in completion order,
//! so callers consume them in input order and the first failure in that
//! order is the one they report. No SQLite handle or other shared mutable
//! state is touched by a worker.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Upper bound on worker threads for one parallel step.
pub(crate) const MAX_WORKERS: usize = 8;

/// The worker count used by indexing: available parallelism, capped.
pub(crate) fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, MAX_WORKERS)
}

/// Apply `f` to every item on at most `workers` threads. A slot is `None`
/// only when `cancel` was set before its item started; every started item
/// runs to completion. With one worker or one item this runs inline.
pub(crate) fn map_ordered<T, R, F>(
    items: &[T],
    workers: usize,
    cancel: &AtomicBool,
    f: F,
) -> Vec<Option<R>>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> R + Sync,
{
    let workers = workers.clamp(1, items.len().max(1));
    if workers == 1 {
        return items
            .iter()
            .enumerate()
            .map(|(index, item)| (!cancel.load(Ordering::Acquire)).then(|| f(index, item)))
            .collect();
    }
    let slots: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    // Unit-test fault injection is per thread; workers see the caller's.
    #[cfg(test)]
    let fault = crate::native_evidence::extraction_auth_fault_snapshot();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                #[cfg(test)]
                let _fault = crate::native_evidence::enter_extraction_auth_fault(&fault);
                loop {
                    if cancel.load(Ordering::Acquire) {
                        return;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        return;
                    };
                    let result = f(index, item);
                    *slots[index].lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| slot.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With two workers, one is held on item 0 until item 2 starts; the
    /// other completes item 1 first. Results still follow input order.
    #[test]
    fn results_follow_input_order_not_completion_order() {
        let cancel = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let out = map_ordered(&[0usize, 1, 2], 2, &cancel, |index, item| {
            match index {
                0 => rx.lock().unwrap().recv().unwrap(),
                2 => tx.send(()).unwrap(),
                _ => {}
            }
            item * 10
        });
        assert_eq!(out, vec![Some(0), Some(10), Some(20)]);
    }

    #[test]
    fn a_set_cancel_flag_starts_no_item() {
        let cancel = AtomicBool::new(true);
        for workers in [1, 2] {
            let out = map_ordered(&[0usize, 1, 2], workers, &cancel, |_, item| *item);
            assert_eq!(out, vec![None, None, None]);
        }
    }
}
