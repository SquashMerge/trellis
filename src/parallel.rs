//! Bounded, order-preserving parallel map for pure per-item CPU work.
//!
//! Results land in the slot of their input index, never in completion order,
//! so callers consume them in input order and the first failure in that
//! order is the one they report. No SQLite handle or other shared mutable
//! state is touched by a worker.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

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
    let fault = crate::native_records::extraction_auth_fault_snapshot();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                #[cfg(test)]
                let _fault = crate::native_records::enter_extraction_auth_fault(&fault);
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

/// `map_ordered` without a barrier between chunks. Workers take items in
/// input order and run at most `window` items ahead of the consumer; the
/// calling thread hands each result to `consume` strictly in input order as
/// soon as it and every earlier one are ready. So at most `window` results
/// are held, and the first `Err` from `consume` is the earliest in input order.
///
/// Returns `Ok(true)` when every item was consumed, `Ok(false)` when `cancel`
/// was set first, and the first `Err` from `consume` otherwise. After either
/// of the last two no item starts; every started item runs to completion
/// before this returns. A panic in `f` or `consume` stops the other workers
/// and is propagated to the caller once they have finished, as `map_ordered`
/// propagates a worker panic.
pub(crate) fn map_ordered_streaming<T, R, E, F, C>(
    items: &[T],
    workers: usize,
    window: usize,
    cancel: &AtomicBool,
    f: F,
    mut consume: C,
) -> Result<bool, E>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> R + Sync,
    C: FnMut(usize, R) -> Result<(), E>,
{
    let window = window.max(1);
    let workers = workers.clamp(1, items.len().max(1));
    if workers == 1 {
        for (index, item) in items.iter().enumerate() {
            if cancel.load(Ordering::Acquire) {
                return Ok(false);
            }
            consume(index, f(index, item))?;
        }
        return Ok(true);
    }
    let shared = Stream {
        state: Mutex::new(StreamState {
            slots: VecDeque::new(),
            base: 0,
            next: 0,
            live: workers,
            stop: false,
            panicked: false,
        }),
        ready: Condvar::new(),
        room: Condvar::new(),
    };
    // Unit-test fault injection is per thread; workers see the caller's.
    #[cfg(test)]
    let fault = crate::native_records::extraction_auth_fault_snapshot();
    std::thread::scope(|scope| {
        // Stops the workers on every exit, including a panic in `consume` or
        // in spawning, so the scope's join never waits on a worker blocked
        // for room.
        let _stop = ConsumerExit(&shared);
        for _ in 0..workers {
            scope.spawn(|| {
                #[cfg(test)]
                let _fault = crate::native_records::enter_extraction_auth_fault(&fault);
                // Wakes the consumer on every exit, including a panic in `f`.
                let _exit = WorkerExit(&shared);
                loop {
                    let index = {
                        let mut state = shared.lock();
                        loop {
                            if state.stop || state.panicked || cancel.load(Ordering::Acquire) {
                                return;
                            }
                            if state.next >= items.len() {
                                return;
                            }
                            if state.next < state.base + window {
                                state.next += 1;
                                break state.next - 1;
                            }
                            state = shared.room.wait(state).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let result = f(index, &items[index]);
                    let mut state = shared.lock();
                    let offset = index - state.base;
                    if state.slots.len() <= offset {
                        state.slots.resize_with(offset + 1, || None);
                    }
                    state.slots[offset] = Some(result);
                    drop(state);
                    shared.ready.notify_all();
                }
            });
        }
        for index in 0..items.len() {
            if cancel.load(Ordering::Acquire) {
                return Ok(false);
            }
            let result = {
                let mut state = shared.lock();
                loop {
                    if state.panicked {
                        // The scope re-raises the worker's panic after joining.
                        return Ok(false);
                    }
                    if let Some(result) = state.slots.front_mut().and_then(Option::take) {
                        state.slots.pop_front();
                        state.base += 1;
                        break result;
                    }
                    if state.live == 0 {
                        // No worker is left to produce this item: they all saw
                        // `cancel` before taking it.
                        return Ok(false);
                    }
                    state = shared.ready.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            };
            shared.room.notify_all();
            consume(index, result)?;
        }
        Ok(true)
    })
}

struct Stream<R> {
    state: Mutex<StreamState<R>>,
    /// A slot was filled or a worker exited; the consumer waits on it.
    ready: Condvar,
    /// The window moved or the stream stopped; idle workers wait on it.
    room: Condvar,
}

struct StreamState<R> {
    /// Results for items `base..`, in input order; `None` is still running.
    slots: VecDeque<Option<R>>,
    /// Items before `base` have been handed to the consumer.
    base: usize,
    /// The next item a worker takes.
    next: usize,
    /// Workers that have not exited.
    live: usize,
    /// The consumer has returned or unwound; no new item starts.
    stop: bool,
    /// A worker panicked; no new item starts and the consumer returns.
    panicked: bool,
}

impl<R> Stream<R> {
    fn lock(&self) -> std::sync::MutexGuard<'_, StreamState<R>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct WorkerExit<'a, R>(&'a Stream<R>);
impl<R> Drop for WorkerExit<'_, R> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.live -= 1;
        if std::thread::panicking() {
            state.panicked = true;
        }
        drop(state);
        self.0.ready.notify_all();
        self.0.room.notify_all();
    }
}

struct ConsumerExit<'a, R>(&'a Stream<R>);
impl<R> Drop for ConsumerExit<'_, R> {
    fn drop(&mut self) {
        self.0.lock().stop = true;
        self.0.room.notify_all();
    }
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

    /// Runs `f` on its own thread; the timeout is a liveness backstop only.
    fn bounded<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(60))
            .expect("map_ordered_streaming hung")
    }

    /// Item 0 is held until item 2 starts, so later items finish first.
    /// The consumer still sees input order, across several windows.
    #[test]
    fn streaming_consumes_in_input_order_not_completion_order() {
        let cancel = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let items: Vec<usize> = (0..9).collect();
        let mut seen = Vec::new();
        let done = map_ordered_streaming(
            &items,
            2,
            3,
            &cancel,
            |index, item| {
                match index {
                    0 => rx.lock().unwrap().recv().unwrap(),
                    2 => tx.send(()).unwrap(),
                    _ => {}
                }
                item * 10
            },
            |index, value| -> Result<(), ()> {
                seen.push((index, value));
                Ok(())
            },
        );
        assert_eq!(done, Ok(true));
        assert_eq!(seen, (0..9).map(|i| (i, i * 10)).collect::<Vec<_>>());
    }

    /// No item starts more than `window` items ahead of the consumer.
    #[test]
    fn streaming_runs_at_most_one_window_ahead() {
        let cancel = AtomicBool::new(false);
        let consumed = AtomicUsize::new(0);
        let ahead = AtomicUsize::new(0);
        let items: Vec<usize> = (0..40).collect();
        let done = map_ordered_streaming(
            &items,
            4,
            3,
            &cancel,
            |index, _| {
                // Only the item being consumed is taken but not yet counted.
                let lead = index - consumed.load(Ordering::SeqCst);
                ahead.fetch_max(lead, Ordering::SeqCst);
            },
            |_, ()| -> Result<(), ()> {
                consumed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );
        assert_eq!(done, Ok(true));
        assert!(ahead.load(Ordering::SeqCst) <= 3, "ran ahead of the window");
    }

    /// Items 1 and 3 fail. Item 1 is held until item 4 starts, so with two
    /// workers item 3 has already failed and been stored. Item 1's failure
    /// is reported, nothing after it is consumed, and the sequential path
    /// agrees.
    #[test]
    fn streaming_reports_the_earliest_failure_in_input_order() {
        let cancel = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let items: Vec<usize> = (0..6).collect();
        let fail = |index: usize| {
            if matches!(index, 1 | 3) {
                Err(index)
            } else {
                Ok(index)
            }
        };
        let mut seen = Vec::new();
        let error = map_ordered_streaming(
            &items,
            2,
            6,
            &cancel,
            |index, _| {
                match index {
                    1 => rx.lock().unwrap().recv().unwrap(),
                    4 => tx.send(()).unwrap(),
                    _ => {}
                }
                fail(index)
            },
            |index, result| {
                seen.push(index);
                result.map(drop)
            },
        );
        assert_eq!(error, Err(1));
        assert_eq!(seen, vec![0, 1]);
        let sequential = map_ordered_streaming(
            &items,
            1,
            6,
            &cancel,
            |index, _| fail(index),
            |_, r| r.map(drop),
        );
        assert_eq!(sequential, Err(1));
    }

    /// The item the consumer waits for panics while the other worker waits
    /// for room. The panic reaches the caller instead of hanging it.
    #[test]
    fn a_worker_panic_propagates_instead_of_hanging() {
        for workers in [1, 2, 4] {
            let outcome = bounded(move || {
                let cancel = AtomicBool::new(false);
                let items: Vec<usize> = (0..8).collect();
                let mut consumed = 0;
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    map_ordered_streaming(
                        &items,
                        workers,
                        2,
                        &cancel,
                        |index, _| {
                            if index == 0 {
                                panic!("injected worker panic");
                            }
                        },
                        |_, ()| -> Result<(), ()> {
                            consumed += 1;
                            Ok(())
                        },
                    )
                }))
                .map(|_| consumed)
            });
            assert!(outcome.is_err(), "{workers} workers: the panic was lost");
        }
    }

    /// A panic in the consumer stops workers waiting for room, then reaches
    /// the caller with its own payload.
    #[test]
    fn a_consumer_panic_propagates_instead_of_hanging() {
        let outcome = bounded(|| {
            let cancel = AtomicBool::new(false);
            let items: Vec<usize> = (0..8).collect();
            std::panic::catch_unwind(|| {
                map_ordered_streaming(
                    &items,
                    3,
                    1,
                    &cancel,
                    |_, item| *item,
                    |_, _| -> Result<(), ()> { panic!("injected consumer panic") },
                )
            })
        });
        let payload = outcome.expect_err("the panic was lost");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"injected consumer panic")
        );
    }

    /// Cancelling during consumption stops it with `Ok(false)`: no later
    /// item is consumed and none starts beyond the window.
    #[test]
    fn streaming_cancellation_stops_consumption() {
        for workers in [1, 2] {
            let cancel = AtomicBool::new(false);
            let started = AtomicUsize::new(0);
            let items: Vec<usize> = (0..20).collect();
            let mut seen = Vec::new();
            let done = map_ordered_streaming(
                &items,
                workers,
                2,
                &cancel,
                |_, _| {
                    started.fetch_add(1, Ordering::SeqCst);
                },
                |index, ()| -> Result<(), ()> {
                    seen.push(index);
                    if index == 1 {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    Ok(())
                },
            );
            assert_eq!(done, Ok(false));
            assert_eq!(seen, vec![0, 1]);
            assert!(
                started.load(Ordering::SeqCst) <= 4,
                "started beyond the window"
            );
        }
    }

    #[test]
    fn streaming_with_a_set_cancel_flag_starts_no_item() {
        let cancel = AtomicBool::new(true);
        for workers in [1, 2] {
            let started = AtomicUsize::new(0);
            let done = map_ordered_streaming(
                &[0usize, 1, 2],
                workers,
                2,
                &cancel,
                |_, _| {
                    started.fetch_add(1, Ordering::SeqCst);
                },
                |_, ()| -> Result<(), ()> { panic!("nothing is consumed") },
            );
            assert_eq!(done, Ok(false));
            assert_eq!(started.load(Ordering::SeqCst), 0);
        }
    }
}
