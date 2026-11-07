#!/usr/bin/env python3
"""Offline checks of tools/flaky-patterns.py on small synthetic trees (#720)."""
import importlib.util
import tempfile
import textwrap
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "flaky_patterns", Path(__file__).with_name("flaky-patterns.py"))
FP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FP)


class Tree:
    made = []

    def __init__(self, files, allow=""):
        self.dir = tempfile.TemporaryDirectory()
        Tree.made.append(self.dir)
        self.root = Path(self.dir.name)
        for rel, text in files.items():
            path = self.root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(textwrap.dedent(text))
        self.allow = self.root / "allow"
        self.allow.write_text(textwrap.dedent(allow))

    def check(self):
        return FP.check(self.root, self.allow)

    def flagged(self):
        flagged, errors = self.check()
        assert not errors, errors
        return [line.split(": ")[1].split(" in fn ")[0] + " " + line.split(" in fn ")[1].split(":")[0]
                for line in flagged]


class Patterns(unittest.TestCase):
    def test_store_open_unwrapped_in_tests_is_flagged_and_retried_is_not(self):
        flagged = Tree({"tests/a.rs": """
            fn bare() { let s = Store::open(roots.clone(), id(&p)).unwrap(); }
            fn expected() { let s = Store::open_for_tests(a, b)
                .expect("open"); }
            fn retried() { let s = trellis::testing::retry_admission(|| Store::open(r.clone(), id(&p))); }
            fn refused() { assert!(Store::open(r, i).unwrap_err().to_string().contains("busy")); }
        """}).flagged()
        self.assertEqual(flagged, ["store-open bare", "store-open expected"])

    def test_cfg_test_items_and_gated_module_files_are_test_code(self):
        flagged = Tree({
            "src/lib.rs": """
                fn production() { Store::open(r, i).unwrap(); }
                #[cfg(test)]
                mod tests { fn inline() { Store::open(r, i).unwrap(); } }
                #[cfg(test)]
                mod gated_tests;
                #[cfg(all(test, feature = "test-support"))]
                #[path = "elsewhere.rs"]
                mod moved;
            """,
            "src/gated_tests.rs": "fn gated() { Store::open(r, i).unwrap(); }\n",
            "src/elsewhere.rs": "fn moved() { Store::open(r, i).unwrap(); }\n",
        }).flagged()
        self.assertEqual(sorted(flagged), ["store-open gated", "store-open inline", "store-open moved"])

    def test_strings_and_comments_are_not_code(self):
        flagged = Tree({"tests/a.rs": """
            // Store::open(r, i).unwrap()
            fn text() { let s = "Store::open(r, i).unwrap()"; let r = r#"listener.accept()"#; let c = '('; }
        """}).flagged()
        self.assertEqual(flagged, [])

    def test_static_counters_in_test_and_test_support_code(self):
        flagged = Tree({
            "src/lib.rs": """
                static LIVE: AtomicUsize = AtomicUsize::new(0);
                #[cfg(feature = "test-support")]
                static PROJECTIONS: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                #[cfg(test)]
                static FLAG: AtomicBool = AtomicBool::new(false);
            """,
            "tests/a.rs": "fn helper() { static NEXT: AtomicU64 = AtomicU64::new(1); }\n",
        }).flagged()
        self.assertEqual(sorted(flagged), ["global-counter *", "global-counter helper"])

    def test_short_wall_clock_deadlines_below_the_backstop(self):
        flagged = Tree({"tests/a.rs": """
            fn short() { let d = Instant::now() + Duration::from_secs(5); }
            fn tokio_short() { tokio::time::timeout(std::time::Duration::from_millis(180), f).await; }
            fn backstop() { let d = Instant::now() + trellis::testing::HANG_BACKSTOP; }
            fn long() { let d = Instant::now() + Duration::from_secs(120); }
            fn poll_interval() { std::thread::sleep(Duration::from_millis(10)); }
        """}).flagged()
        self.assertEqual(flagged, ["short-wait short", "short-wait tokio_short"])

    def test_only_unbounded_accepts_are_flagged(self):
        flagged = Tree({"tests/a.rs": """
            fn blocking() { let (s, _) = listener.accept().unwrap(); }
            fn served() { for s in listener.incoming() {} }
            fn helper() { let s = trellis::testing::accept_within_backstop(&listener); }
            fn polled() { trellis::testing::wait_until("hook", || listener.accept().is_ok()); }
            async fn timed() { tokio::time::timeout(HANG_BACKSTOP, listener.accept()).await; }
            fn nothing_pending() { assert!(listener.accept().is_err_and(|e| true)); }
            fn nonblocking() { listener.set_nonblocking(true).unwrap(); loop { listener.accept(); } }
            fn poll2() { unsafe { libc::poll(&mut fd, 1, ms) }; listener.accept().unwrap(); }
        """}).flagged()
        self.assertEqual(flagged, ["blocking-accept blocking", "blocking-accept served"])

    def test_upper_bounds_on_elapsed_time_below_the_backstop(self):
        flagged = Tree({"tests/a.rs": """
            fn direct() { assert!(started.elapsed() < Duration::from_millis(1100), "slow"); }
            fn computed() { assert!(start.elapsed() <= READ_DEADLINE + Duration::from_secs(2)); }
            fn bound() { let waited = started.elapsed(); assert!(waited < Duration::from_secs(4)); }
            fn seconds() { let s = t.elapsed().as_secs_f64(); assert!(
                s <= upper,
                "too slow"
            ); }
            fn backstop() { assert!(started.elapsed() < trellis::testing::HANG_BACKSTOP); }
            fn bound_backstop() { let r = began.elapsed(); assert!(r < HANG_BACKSTOP, "hang"); }
            fn lower() { assert!(start.elapsed() >= READ_DEADLINE); assert!(r > held); }
            fn unrelated() { let n = v.len(); assert!(n < 3); assert!(a << 2 < b); }
            fn message() { assert!(ok, "took {:?} < {:?}", t.elapsed(), limit); }
            fn production_loop() { while started.elapsed() < Duration::from_millis(5) {} }
        """}).flagged()
        self.assertEqual(flagged, ["speed-assert direct", "speed-assert computed",
                                   "speed-assert bound", "speed-assert seconds"])

    def test_short_socket_read_timeouts(self):
        flagged = Tree({
            "tests/a.rs": """
                fn short() { s.set_read_timeout(Some(Duration::from_millis(500))).unwrap(); }
                fn secs() { s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap(); }
                fn backstop() { s.set_read_timeout(Some(trellis::testing::HANG_BACKSTOP)).unwrap(); }
                fn computed() { s.set_read_timeout(Some(remaining)).unwrap(); }
                fn long() { s.set_read_timeout(Some(Duration::from_secs(120))).unwrap(); }
            """,
            "src/lib.rs": "fn client() { s.set_read_timeout(Some(Duration::from_secs(5))).unwrap(); }\n",
        }).flagged()
        self.assertEqual(flagged, ["short-read-timeout short", "short-read-timeout secs"])


class Allowlist(unittest.TestCase):
    FILES = {"tests/a.rs": """
        fn one() { Store::open(r, i).unwrap(); }
        fn two() { Store::open(r, i).unwrap(); }
    """}

    def test_entries_cover_one_fn_or_a_whole_file(self):
        tree = Tree(self.FILES, "store-open tests/a.rs one -- the only owner\n")
        flagged, errors = tree.check()
        self.assertEqual(errors, [])
        self.assertEqual(len(flagged), 1)
        self.assertIn("in fn two", flagged[0])
        tree = Tree(self.FILES, "# comment\n\nstore-open tests/a.rs * -- the only owner\n")
        self.assertEqual(tree.check(), ([], []))

    def test_an_entry_needs_a_reason_and_a_known_pattern(self):
        tree = Tree(self.FILES, "store-open tests/a.rs *\nstore-open tests/a.rs * --  \nslow tests/a.rs * -- why\n")
        _, errors = tree.check()
        self.assertEqual(len(errors), 3, errors)

    def test_a_stale_entry_fails(self):
        tree = Tree(self.FILES, "store-open tests/a.rs * -- ok\nshort-wait tests/a.rs one -- gone\n")
        _, errors = tree.check()
        self.assertEqual(len(errors), 1)
        self.assertIn("stale", errors[0])


def tearDownModule():
    for made in Tree.made:
        made.cleanup()


if __name__ == "__main__":
    unittest.main()
