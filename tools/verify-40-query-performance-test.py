#!/usr/bin/env python3
"""Offline checks for the #40 query-performance harness; launches no Trellis process."""
import importlib.util
import json
import sqlite3
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "query_perf", Path(__file__).with_name("verify-40-query-performance.py"))
H = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(H)
V67 = H.V67

SAMPLE = (b"import {f0 as peer0} from './x.js';\n"
          b"export function f0(){return 1;}\n"
          b"function f1(){\n"
          b" const o={}, xf0=3;\n"
          b" return f0()+o.f0()+xf0()+f0();\n"
          b"}\n")


def packed(a, b, c):
    def varint(n):
        out = bytearray()
        while n > 127:
            out.append((n & 127) | 128)
            n >>= 7
        out.append(n)
        return bytes(out)
    return varint(a) + varint(b) + varint(c)


class Percentiles(unittest.TestCase):
    def test_nearest_rank_indices_for_200(self):
        values = list(range(200, 0, -1))
        self.assertEqual(H.percentile(values, .95), 190)
        self.assertEqual(H.percentile(values, .99), 198)

    def test_lte_at_the_boundary(self):
        self.assertTrue(H.lte(100, 100))
        self.assertFalse(H.lte(100.0001, 100))

    def test_limits_come_from_the_policy(self):
        policy = H.load_policy()
        stats = {"p95Ms": 0, "p99Ms": 0}
        limits = {kind: (H.evaluate(kind, stats, policy)["limitMs"], H.evaluate(kind, stats, policy)["p99LimitMs"])
                  for kind in H.POLICY_ENTRY}
        self.assertEqual(limits, {
            "defaultBoundedQuery": (100, None), "semanticDefaultBoundedQuery": (100, None),
            "maximumBoundedQuery": (250, 750), "semanticMaximumBoundedQuery": (250, 750),
            "symbolSubstringSearch": (250, None), "status": (25, None), "sourceByKey": (25, None)})

    def test_semantic_kind_miss_fails_the_run(self):
        policy = H.load_policy()
        stats = {"p95Ms": 100.5, "p99Ms": 100.5, "count": 1, "minMs": 1, "maxMs": 1}
        self.assertFalse(H.evaluate("semanticDefaultBoundedQuery", stats, policy)["pass"])
        sizes = {"medium": {"facts": {}, "producer": "p", "anchorPath": "a", "artifactSha256": "h",
                            "coveredRoots": 1, "kinds": {"semanticDefaultBoundedQuery": {
            **stats, "edgeCounts": {**H.empty_counts(), "bound": 1}, "responseBytesReduced": 0}}}}
        report = H.build_report("host", "sha", True, sizes, policy)
        self.assertFalse(report["pass"])
        self.assertIn(H.LIMITATION_IDENTICAL, report["limitations"])
        self.assertEqual(report["evidence"], "semantic")

    def test_p99_limit_gates_maximum_query(self):
        policy = H.load_policy()
        stats = {"p95Ms": 1, "p99Ms": 751}
        self.assertFalse(H.evaluate("maximumBoundedQuery", stats, policy)["pass"])


class Floors(unittest.TestCase):
    def counts(self, **override):
        return {**{language: 50_000 for language in H.LANGUAGES}, **override}

    def test_floors_come_from_cohorts(self):
        self.assertEqual(H.load_floors(), {"medium": (50_000, "perLanguage"), "large": (500_000, "overall")})

    def test_medium_per_language(self):
        H.check_floor(self.counts(), "medium", 50_000, "perLanguage")
        with self.assertRaises(RuntimeError):
            H.check_floor(self.counts(rust=49_999), "medium", 50_000, "perLanguage")
        missing = self.counts()
        del missing["java"]
        with self.assertRaises(RuntimeError):
            H.check_floor(missing, "medium", 50_000, "perLanguage")

    def test_large_overall(self):
        H.check_floor({"java": 125_000, "javascript": 125_000, "python": 125_000, "rust": 125_000},
                      "large", 500_000, "overall")
        with self.assertRaises(RuntimeError):
            H.check_floor({"java": 125_000, "javascript": 125_000, "python": 125_000, "rust": 124_999},
                          "large", 500_000, "overall")

    def test_floor_rejects_zero(self):
        with self.assertRaises(RuntimeError):
            H.check_floor({}, "smoke", 1, "perLanguage")

    def test_floor_miss_raises_before_sampling(self):
        with patch.object(H, "facts_for", return_value=self.counts(rust=1)), \
                patch.object(H, "collect_samples") as sampler:
            with self.assertRaises(RuntimeError):
                H.floored_samples(None, "pin", "medium", 50_000, "perLanguage")
        sampler.assert_not_called()

    def test_floor_pass_then_samples(self):
        with patch.object(H, "facts_for", return_value=self.counts()), \
                patch.object(H, "collect_samples", return_value="samples") as sampler:
            self.assertEqual(H.floored_samples(None, "pin", "medium", 50_000, "perLanguage", "p")[1], "samples")
        sampler.assert_called_once_with(None, "pin", "p")


class Sampling(unittest.TestCase):
    def test_stride_with_1000_items(self):
        items = list(range(1000))
        self.assertEqual(H.stride_sample(items, 200), [i * 1000 // 200 for i in range(200)])

    def test_stride_with_70_items_repeats_in_order(self):
        items = list(range(70))
        picked = H.stride_sample(items, 200)
        self.assertEqual(len(picked), 200)
        self.assertEqual(picked[0], 0)
        self.assertEqual(picked, sorted(picked))
        self.assertEqual(picked, [i * 70 // 200 for i in range(200)])

    def test_stride_rejects_empty(self):
        with self.assertRaises(RuntimeError):
            H.stride_sample([], 200)

    def test_covered_roots(self):
        db = sqlite3.connect(":memory:")
        db.executescript("""
            CREATE TABLE revision_documents(revision_id TEXT,language TEXT,path TEXT,vid INTEGER);
            CREATE TABLE native_version_declarations(vid INTEGER,syntax_id BLOB,kind TEXT);
            CREATE TABLE native_version_calls(vid INTEGER,owner_syntax_id BLOB);
            CREATE TABLE semantic_coverage(revision_id TEXT,producer_id TEXT,language TEXT,path TEXT,
                selected INTEGER,state TEXT);""")
        docs = [("pin", "javascript", "b.js", 2), ("pin", "javascript", "a.js", 1),
                ("pin", "javascript", "c.js", 3), ("pin", "javascript", "d.js", 4),
                ("pin", "javascript", "e.js", 5), ("pin", "python", "a.py", 6)]
        db.executemany("INSERT INTO revision_documents VALUES(?,?,?,?)", docs)
        for vid in range(1, 7):
            db.execute("INSERT INTO native_version_declarations VALUES(?,?,?)", (vid, bytes([2]), "function"))
            db.execute("INSERT INTO native_version_declarations VALUES(?,?,?)", (vid, bytes([1]), "method"))
            db.execute("INSERT INTO native_version_declarations VALUES(?,?,?)", (vid, bytes([9]), "class"))
            db.execute("INSERT INTO native_version_declarations VALUES(?,?,?)", (vid, bytes([7]), "function"))
            for owner in (1, 2, 9):
                db.execute("INSERT INTO native_version_calls VALUES(?,?)", (vid, bytes([owner])))
        coverage = [("pin", "p", "javascript", "b.js", 1, "complete"),
                    ("pin", "p", "javascript", "a.js", 1, "partial"),
                    ("pin", "other", "javascript", "c.js", 1, "complete"),
                    ("pin", "p", "javascript", "d.js", 0, "complete"),
                    ("pin", "p", "javascript", "e.js", 1, "failed"),
                    ("pin", "p", "python", "a.py", 1, "complete")]
        db.executemany("INSERT INTO semantic_coverage VALUES(?,?,?,?,?,?)", coverage)
        sid = lambda n: "sid:v1:" + f"{n:02x}"
        self.assertEqual(H.covered_roots(db, "pin", "p"),
                         [sid(1), sid(2), sid(1), sid(2), sid(1), sid(2)])
        self.assertEqual(len(H.callable_roots(db, "pin")), 12)
        self.assertEqual(H.covered_roots(db, "pin", "nobody"), [])


class CallSiteArtifact(unittest.TestCase):
    def test_positions(self):
        definition, calls = H.call_site_positions(SAMPLE)
        self.assertEqual(definition, (1, 16))
        self.assertEqual(calls, [(4, 8), (4, 26)])

    def test_positions_reject_bad_input(self):
        with self.assertRaises(RuntimeError):
            H.call_site_positions("export function f0(){} f0()é".encode())
        with self.assertRaises(RuntimeError):
            H.call_site_positions(b"function f0(){}\nf0();\n")
        with self.assertRaises(RuntimeError):
            H.call_site_positions(b"export function f0(){}\nvar a=o.f0()+xf0();\n")

    def test_anchor_bytes(self):
        raw = b"export function f0(){return 1;}\nfunction f1(){return f0();}\n"
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "a.js"
            path.write_bytes(raw)
            produced = H.call_site_anchor(path, Path("javascript/a.js"))
        symbol = b"pkg f0()."
        definition = (V67.field(1, packed(0, 16, 18)) + V67.field(2, symbol) + V67.field(3, 1, 0))
        call = V67.field(1, packed(1, 21, 23)) + V67.field(2, symbol) + V67.field(7, packed(1, 21, 25))
        information = V67.field(1, symbol) + V67.field(5, 26, 0) + V67.field(6, b"f0")
        document = (V67.field(1, b"javascript/a.js") + V67.field(2, definition) + V67.field(2, call)
                    + V67.field(3, information) + V67.field(4, b"javascript") + V67.field(6, 2, 0))
        self.assertEqual(produced, V67.field(2, document))
        self.assertIn(b"\x3a\x03\x01\x15\x19", produced)
        self.assertNotIn(V67.field(3, 1, 0) + V67.field(2, symbol), call)
        self.assertEqual(call.count(b"\x3a"), 1)


class EdgeCounts(unittest.TestCase):
    def result(self, bound_count=3):
        def edge(visit, reason, bound):
            return {"visit": visit, "boundaryReason": reason, "binding": {"x": 1} if bound else None}
        edges = [edge("new", "none", True), edge("boundary", "stale", True),
                 edge("boundary", "dispatch", True), edge("boundary", "missingEvidence", False)]
        return {"edges": edges if bound_count else [e for e in edges if e["binding"] is None]}

    def test_counts(self):
        self.assertEqual(H.edge_counts(self.result()), {
            "edges": 4, "bound": 3, "boundary": 3, "new": 1, "seen": 0,
            "reasons": {"none": 1, "stale": 1, "dispatch": 1, "missingEvidence": 1}})

    def test_bound_check_raises_on_zero(self):
        H.check_bound("semanticDefaultBoundedQuery", H.edge_counts(self.result()))
        with self.assertRaisesRegex(RuntimeError, "semantic run consumed no call bindings"):
            H.check_bound("semanticDefaultBoundedQuery", H.edge_counts(self.result(0)))

    def test_counts_sum_across_calls(self):
        total = H.empty_counts()
        for _ in range(2):
            H.add_counts(total, H.edge_counts(self.result()))
        self.assertEqual((total["edges"], total["bound"], total["reasons"]["stale"]), (8, 6, 2))


class ReuseWiring(unittest.TestCase):
    def test_configure_reuse(self):
        saved = (V67.EVIDENCE, V67.DEADLINE, V67.scip_anchor)
        try:
            H.configure_reuse(Path("/tmp/scratch"), 12345.0)
            self.assertIs(V67.scip_anchor, H.call_site_anchor)
            self.assertEqual(V67.EVIDENCE, Path("/tmp/scratch"))
            self.assertEqual(V67.DEADLINE, 12345.0)
        finally:
            V67.EVIDENCE, V67.DEADLINE, V67.scip_anchor = saved

    def test_checkout_path_inits_only_on_bare_mkdir(self):
        with tempfile.TemporaryDirectory() as scratch, patch.object(H, "init_checkout") as init:
            root = H.CheckoutPath(scratch) / "ws"
            root.mkdir()
            (root / "sub").mkdir(parents=True, exist_ok=True)
        init.assert_called_once()

    def test_measure_stops_each_cohort_before_the_next(self):
        events = []
        with tempfile.TemporaryDirectory() as scratch, \
                patch.object(H.platform, "system", return_value="Darwin"), \
                patch.object(H.tempfile, "mkdtemp", return_value=scratch), \
                patch.object(H, "configure_reuse"), \
                patch.object(H, "load_policy", return_value={}), \
                patch.object(H, "load_floors", return_value={"medium": (1, "x"), "large": (1, "x")}), \
                patch.object(H, "init_checkout"), \
                patch.object(H.shutil, "copytree"), \
                patch.object(H, "pin_of", return_value="pin"), \
                patch.object(H, "build_report", return_value={"pass": True}), \
                patch.object(H, "measure_cohort",
                             side_effect=lambda *a: events.append(("measure", a[4])) or {}), \
                patch.object(H.V67, "command", return_value="Mac17,16"), \
                patch.object(H.V67, "env_for", return_value={}), \
                patch.object(H.V67, "stop_home_processes",
                             side_effect=lambda home, binary: events.append(("stop", home.name))), \
                patch.object(Path, "read_bytes", return_value=b""):
            self.assertEqual(H.measure(False), 0)
        self.assertEqual(events, [("measure", "medium"), ("stop", "medium-private-home"),
                                  ("measure", "large"), ("stop", "large-private-home")])

    def test_usage_errors(self):
        for argv in (["--smoke", "--semantic", "--binary", "x"], ["--smoke"], ["--binary", "x"]):
            with self.assertRaises(SystemExit) as caught:
                H.main(argv)
            self.assertEqual(caught.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
