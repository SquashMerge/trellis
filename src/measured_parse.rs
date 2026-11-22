//! Measurement's own parse of a Rust source, bounded by deterministic work (#1282).
//!
//! tree-sitter calls the progress callback about once per 100 parse steps, but its error
//! recovery can do work between two steps that grows with the input: `] * ` or
//! `} = ) + ] * ` repeated spends seconds to minutes in recovery over a few hundred ticks, so
//! no tick count bounds it. Almost all of that work creates subtrees and stack nodes through
//! tree-sitter's allocator, as a clean parse's does. So the number of allocator calls a
//! parse makes tracks its CPU work on clean and pathological input alike.
//!
//! Trellis installs allocator functions that count tree-sitter's calls on each thread and
//! leave the memory to libc, as tree-sitter's defaults do. A parse runs on its caller's thread
//! with a fresh parser, so its count is a property of the source, the grammar and the pinned
//! tree-sitter alone: the same on every run and machine. [`parse`] stops a parse whose count
//! passes [`limit`] at the next progress tick, and the source is then beyond measurement's
//! parse-work budget.
use std::cell::Cell;
use std::ffi::c_void;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};

/// Allocator calls a parse of a source of `bytes` may make: [`PER_BYTE`] per byte, plus
/// [`BASE`]. Real Rust parses make at most 2.6 calls per byte (dense numeric tables), and 3.0
/// with an injected syntax error, so the bound is over five times the most measured on real
/// code, and never depends on the source's size alone.
pub(crate) fn limit(bytes: usize) -> u64 {
    PER_BYTE.saturating_mul(bytes as u64).saturating_add(BASE)
}
pub(crate) const PER_BYTE: u64 = 16;
pub(crate) const BASE: u64 = 1_000_000;

/// The coverage diagnostic of a document whose parse passed the parse-work budget. Reads find
/// such a document by it, so a change here is a change of stored meaning.
pub(crate) const DIAGNOSTIC: &str =
    "source parse exceeds the per-file parse-work budget; only the file module was kept";

/// How a bounded parse ended.
pub(crate) enum Parse {
    Tree(tree_sitter::Tree),
    /// Its allocator calls passed [`limit`].
    Beyond,
    /// The cancel flag was set.
    Cancelled,
}

/// Parse `text` as Rust with a fresh parser, within [`limit`] and observing `cancel`.
pub(crate) fn parse(text: &str, cancel: Option<&AtomicBool>) -> anyhow::Result<Parse> {
    install();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let limit = limit(text.len());
    let start = calls();
    let mut beyond = false;
    let mut progress = |_: &tree_sitter::ParseState| {
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
            return ControlFlow::Break(());
        }
        beyond = calls().wrapping_sub(start) > limit;
        if beyond {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| &text.as_bytes()[offset..],
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
    );
    Ok(match tree {
        Some(tree) => Parse::Tree(tree),
        None if beyond => Parse::Beyond,
        None if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) => Parse::Cancelled,
        None => anyhow::bail!("Rust parser failed"),
    })
}

thread_local! {
    static CALLS: Cell<u64> = const { Cell::new(0) };
}

/// tree-sitter allocator calls made on this thread since it started.
fn calls() -> u64 {
    CALLS.try_with(Cell::get).unwrap_or(0)
}

fn count() {
    let _ = CALLS.try_with(|calls| calls.set(calls.get().wrapping_add(1)));
}

/// Install the counting allocator functions, once per process. The binary calls this first
/// thing, before any tree-sitter use, as `ts_set_allocator` asks; [`parse`] calls it too, for
/// tests. Either way the functions only count and then call libc, as tree-sitter's defaults
/// do, so memory allocated before the install is freed correctly after it, and a parse that
/// starts on another thread while it happens works with either set.
pub fn install() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        // SAFETY: the four functions are libc's allocator family, which tree-sitter's defaults
        // also use, and they abort rather than return null, as the defaults do.
        unsafe {
            tree_sitter::set_allocator(Some(tree_sitter::Allocator {
                malloc: counted_malloc,
                calloc: counted_calloc,
                realloc: counted_realloc,
                free: libc_free,
            }))
        }
    });
}

fn checked(pointer: *mut c_void, bytes: usize) -> *mut c_void {
    if pointer.is_null() && bytes > 0 {
        std::process::abort();
    }
    pointer
}

unsafe extern "C" fn counted_malloc(size: usize) -> *mut c_void {
    count();
    // SAFETY: libc's malloc, called as tree-sitter's default does.
    checked(unsafe { libc::malloc(size) }, size)
}

unsafe extern "C" fn counted_calloc(items: usize, size: usize) -> *mut c_void {
    count();
    // SAFETY: libc's calloc, called as tree-sitter's default does.
    checked(
        unsafe { libc::calloc(items, size) },
        items.saturating_mul(size),
    )
}

unsafe extern "C" fn counted_realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    count();
    // SAFETY: `pointer` came from this libc family, as tree-sitter's default requires.
    checked(unsafe { libc::realloc(pointer, size) }, size)
}

unsafe extern "C" fn libc_free(pointer: *mut c_void) {
    // SAFETY: `pointer` came from this libc family, as tree-sitter's default requires.
    unsafe { libc::free(pointer) }
}

/// Pathological inputs for the parse-work budget's tests.
#[cfg(test)]
pub(crate) mod inputs {
    /// `} = ) + ] * ` repeated to 64 KiB: tree-sitter-rust's error recovery takes quadratic
    /// time on it in under a thousand progress ticks (#1282).
    pub(crate) fn recovery() -> String {
        "} = ) + ] * ".repeat((64 << 10) / 12)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls_of(text: &str) -> (bool, u64) {
        install();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let start = calls();
        let tree = parser.parse(text, None).unwrap();
        (tree.root_node().has_error(), calls() - start)
    }

    /// #1282: the 64 KiB recovery input is refused, in a bounded time, the same way on every
    /// run; its first 1 KiB, whose whole parse is under the budget, still parses.
    #[test]
    fn pathological_recovery_is_beyond_the_budget_on_every_run() {
        let text = inputs::recovery();
        for _ in 0..3 {
            let started = std::time::Instant::now();
            assert!(matches!(parse(&text, None).unwrap(), Parse::Beyond));
            // A hang backstop only: unbounded, this parse takes minutes.
            assert!(started.elapsed() < std::time::Duration::from_secs(60));
        }
        let head = &text[..1020];
        let (error, calls) = calls_of(head);
        assert!(error && calls < limit(head.len()), "{calls}");
        assert!(matches!(parse(head, None).unwrap(), Parse::Tree(_)));
    }

    /// A real source's count is the same on every run and far under the budget, and the
    /// bounded parse gives it the very tree an unbounded parse does.
    #[test]
    fn a_real_source_parses_as_before_and_counts_the_same_on_every_run() {
        let text = include_str!("classes.rs");
        let runs: Vec<_> = (0..3).map(|_| calls_of(text)).collect();
        assert!(runs.windows(2).all(|w| w[0] == w[1]), "{runs:?}");
        let (error, calls) = runs[0];
        assert!(
            !error && calls > 0 && calls * 5 < limit(text.len()),
            "{calls}"
        );
        let Parse::Tree(tree) = parse(text, None).unwrap() else {
            panic!("a real source is within the budget");
        };
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let unbounded = parser.parse(text, None).unwrap();
        assert_eq!(tree.root_node().to_sexp(), unbounded.root_node().to_sexp());
    }

    #[test]
    fn a_set_cancel_flag_stops_the_parse() {
        let cancel = AtomicBool::new(true);
        let text = "fn f() {}\n".repeat(10_000);
        assert!(matches!(
            parse(&text, Some(&cancel)).unwrap(),
            Parse::Cancelled
        ));
    }
}
