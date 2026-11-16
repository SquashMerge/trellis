#!/usr/bin/env python3
"""Offline checks of tools/rules-ratchet.py's comparison and record (#1008)."""
import contextlib
import importlib.util
import re
import io
import json
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "rules_ratchet", Path(__file__).with_name("rules-ratchet.py"))
RR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RR)

# A record of three deviating pairs from two source files. Its unknown references are
# 20 from src/store.rs, 5 from src/http.rs and 25 from files with no deviating pair.
RECORD = {"deviatingPairs": 3, "deviatingReferences": 10, "unknownReferences": 50,
          "partialFiles": 1, "comparablePairs": 40, "conformingPairs": 30, "unknownPairs": 7,
          "deviatingReferencesByPair": {"src/http.rs": {"src/storage.rs": 1},
                                        "src/store.rs": {"src/capture.rs": 6, "src/indexer.rs": 3}},
          "unknownReferencesBySource": {"src/http.rs": 5, "src/store.rs": 20}}
UNRELATED = {"src/cli.rs": 15, "src/daemon.rs": 10}


def figures(pairs=None, unknown=None, **totals):
    """A measurement of RECORD's tree after changes: `pairs` gives (source, destination)
    pairs new reference counts, 0 for a pair that is gone, and `unknown` gives source
    files new unknown-reference counts. The deviation and unknown totals follow."""
    flat = {**RR.pairs(RECORD), **(pairs or {})}
    by_pair = {}
    for (source, destination), references in flat.items():
        if references:
            by_pair.setdefault(source, {})[destination] = references
    by_source = {**RECORD["unknownReferencesBySource"], **UNRELATED, **(unknown or {})}
    return {**RECORD, "deviatingPairs": sum(map(len, by_pair.values())),
            "deviatingReferences": sum(sum(each.values()) for each in by_pair.values()),
            "unknownReferences": sum(by_source.values()),
            "deviatingReferencesByPair": by_pair, "unknownReferencesBySource": by_source,
            **totals}


CAPTURE = ("src/store.rs", "src/capture.rs")
INDEXER = ("src/store.rs", "src/indexer.rs")
STORAGE = ("src/http.rs", "src/storage.rs")

NATIVE = {"sourceSetId": "source-set:v1:0", "language": "rust", "path": "src/storage.rs",
          "sid": "sid:v1:0"}


def stdlib(package, language="python"):
    """A standard-library destination, as `rules check` emits it."""
    return {"kind": "externalPackage", "sourceSetId": "source-set:v1:0",
            "language": language, "package": package, "standardLibrary": True}


def row(status, references, excepted=False, destination=NATIVE, source="src/http.rs"):
    return {"status": status, "referenceCount": references, "excepted": excepted,
            "target": {**NATIVE, "path": source}, "destination": destination}


def payload(*instances):
    """A `rules check` payload whose counts agree with its instances."""
    def count(status):
        return sum(1 for each in instances if each["status"] == status and not each["excepted"])
    return {
        "counts": {"comparable": len(instances), "conforms": count("conforms"),
                   "deviates": count("deviates"), "unknown": count("unknown"),
                   "excepted": sum(1 for each in instances if each["excepted"])},
        "instances": list(instances),
        "partialFiles": [],
    }


# A small measured tree: one deviation, two conforming pairs, one unresolved reference
# and one standard-library import of a tool.
TREE = (row("deviates", 4), row("conforms", 9), row("conforms", 1),
        row("unknown", 2, destination=None),
        row("unknown", 1, destination=stdlib("os"), source="tools/rules-ratchet.py"))


class Ratchet(unittest.TestCase):
    def verdict(self, pairs=None, unknown=None, **totals):
        return RR.compare(RECORD, figures(pairs, unknown, **totals))

    def test_the_same_figures_pass(self):
        verdict, lines = self.verdict()
        self.assertEqual(verdict, "equal")
        self.assertIn("deviatingPairs 3", lines[0])

    def test_a_total_above_the_record_fails(self):
        # The totals alone decide this, whatever the pairs say.
        for key in ("deviatingPairs", "deviatingReferences"):
            verdict, lines = self.verdict(**{key: RECORD[key] + 1})
            self.assertEqual(verdict, "fail", key)
            self.assertIn("deviations rose", lines[0])
            self.assertIn(f"{key}: recorded {RECORD[key]}, measured {RECORD[key] + 1}", lines[1])
        verdict, lines = self.verdict(pairs={CAPTURE: 8})
        self.assertEqual(verdict, "fail")
        self.assertEqual(lines[:2], ["rules ratchet FAILED: deviations rose",
                                     "  deviatingReferences: recorded 10, measured 12"])

    def test_a_new_deviating_pair_fails_though_the_totals_do_not_rise(self):
        # One pair gone, another new: the same totals. A renamed source file looks the
        # same, and fails the same way until the record is updated.
        verdict, lines = self.verdict(pairs={STORAGE: 0, ("src/web.rs", "src/storage.rs"): 1})
        self.assertEqual(verdict, "fail")
        self.assertEqual(lines[:2], [
            "rules ratchet FAILED: deviations rose",
            "  new deviating pair: src/web.rs -> src/storage.rs (1 references)"])
        self.assertFalse(any("lost coverage" in line for line in lines))

    def test_more_partial_files_fail(self):
        verdict, lines = self.verdict(pairs={CAPTURE: 1}, partialFiles=2)
        self.assertEqual(verdict, "fail")
        self.assertEqual(lines[:2], ["rules ratchet FAILED: more files are partial",
                                     "  partialFiles: recorded 1, measured 2"])

    def test_a_fall_in_a_pair_with_a_rise_only_in_unrelated_files_is_a_notice(self):
        # #1140: the fall and the rise are not related. src/http.rs has a deviating pair
        # too, but not the one that fell.
        for unknown in ({"src/cli.rs": 18}, {"src/http.rs": 8}, {"src/new.rs": 3}):
            verdict, lines = self.verdict(pairs={CAPTURE: 4}, unknown=unknown)
            self.assertEqual(verdict, "lower", unknown)
            self.assertIn("NOTICE", lines[0])
            self.assertIn("  src/store.rs -> src/capture.rs: deviating references recorded 6, "
                          "measured 4", lines)
            self.assertTrue(any("unknown references rose" in line and "(+3)" in line
                                and "Not a failure" in line for line in lines), lines)
            self.assertFalse(any("FAILED" in line for line in lines))

    def test_a_fall_in_a_pair_with_a_rise_from_its_source_file_is_lost_coverage(self):
        verdict, lines = self.verdict(pairs={CAPTURE: 4}, unknown={"src/store.rs": 22})
        self.assertEqual(verdict, "fail")
        self.assertIn("lost coverage", lines[0])
        self.assertEqual(lines[1], "  src/store.rs -> src/capture.rs: deviating references "
                         "recorded 6, measured 4; unknown references from src/store.rs: "
                         "recorded 20, measured 22 (+2)")
        # Only the pair that fell is named, not the source file's other pair.
        self.assertFalse(any("src/indexer.rs" in line for line in lines))

    def test_a_removed_pair_with_a_rise_from_its_source_file_is_lost_coverage(self):
        # The pair is gone; the source file still has, or no longer has, other pairs.
        for gone, source in ((CAPTURE, "src/store.rs"), (STORAGE, "src/http.rs")):
            before = RECORD["unknownReferencesBySource"][source]
            verdict, lines = self.verdict(pairs={gone: 0}, unknown={source: before + 1})
            self.assertEqual(verdict, "fail", gone)
            self.assertIn("lost coverage", lines[0])
            self.assertIn(f"  {gone[0]} -> {gone[1]}: deviating references recorded "
                          f"{RECORD['deviatingReferencesByPair'][gone[0]][gone[1]]}, measured 0; "
                          f"unknown references from {source}: recorded {before}, "
                          f"measured {before + 1} (+1)", lines)

    def test_lost_coverage_does_not_need_the_unknown_total_to_rise(self):
        # The source file's rise is hidden in the total by a fall elsewhere.
        verdict, lines = self.verdict(pairs={INDEXER: 2},
                                      unknown={"src/store.rs": 21, "src/cli.rs": 10})
        self.assertEqual(verdict, "fail")
        self.assertIn("lost coverage", lines[0])

    def test_a_rise_in_unknown_references_without_a_fall_is_a_notice(self):
        for unknown in ({"src/cli.rs": 18}, {"src/store.rs": 23}):
            verdict, lines = self.verdict(unknown=unknown)
            self.assertEqual(verdict, "lower", unknown)
            self.assertIn("NOTICE", lines[0])
            self.assertTrue(any("unknown references rose" in line and "(+3)" in line
                                and "Not a failure" in line for line in lines), lines)
        self.assertIn("  unknown references from src/store.rs: recorded 20, measured 23", lines)

    def test_a_fall_on_equal_or_fewer_unknown_references_from_its_source_is_a_notice(self):
        for unknown in (20, 19):
            verdict, lines = self.verdict(pairs={CAPTURE: 1, INDEXER: 0},
                                          unknown={"src/store.rs": unknown})
            self.assertEqual(verdict, "lower", unknown)
            self.assertIn("the record can be updated", lines[0])
            self.assertIn(RR.UPDATE, lines[-1])
            self.assertFalse(any("rose" in line for line in lines))

    def test_an_update_that_is_lost_coverage_is_refused_without_the_flag(self):
        measured = figures(pairs={CAPTURE: 4}, unknown={"src/store.rs": 21})
        refused, lines = RR.update_refusal(RECORD, measured, False)
        self.assertTrue(refused)
        self.assertEqual(lines[0], "rules ratchet: unknownReferences: recorded 50, measured 51 (+1)")
        self.assertIn("not rewritten", lines[1])
        self.assertIn("src/store.rs -> src/capture.rs", lines[2])
        self.assertIn(f"{RR.UPDATE} --accept-unknown-rise", lines[-1])
        accepted, lines = RR.update_refusal(RECORD, measured, True)
        self.assertFalse(accepted)
        self.assertIn("(+1)", lines[0])
        self.assertIn("--accept-unknown-rise", lines[1])
        self.assertIn("src/store.rs -> src/capture.rs", lines[2])

    def test_other_updates_are_not_refused_and_print_the_unknown_change(self):
        for changed, delta in (
                ({"unknown": {"src/cli.rs": 18}}, "(+3)"),
                # A fall with a rise in unrelated files only.
                ({"pairs": {CAPTURE: 4}, "unknown": {"src/cli.rs": 18}}, "(+3)"),
                ({"pairs": {CAPTURE: 4}, "unknown": {"src/store.rs": 18}}, "(-2)"),
                ({"pairs": {CAPTURE: 7}, "unknown": {"src/store.rs": 21}}, "(+1)"),
                ({"pairs": {("src/web.rs", "src/storage.rs"): 1}}, "(+0)"),
                ({}, "(+0)")):
            refused, lines = RR.update_refusal(RECORD, figures(**changed), False)
            self.assertFalse(refused, changed)
            self.assertEqual(len(lines), 1)
            self.assertIn(delta, lines[0])

    def test_pair_totals_never_fail(self):
        # Fewer comparable or conforming pairs (code deleted, files merged) and more
        # unknown pairs (a file split) are notices.
        for changed in ({"comparablePairs": 39}, {"conformingPairs": 29}, {"unknownPairs": 8},
                        {"comparablePairs": 35, "conformingPairs": 26, "unknownPairs": 9}):
            verdict, lines = self.verdict(**changed)
            self.assertEqual(verdict, "lower", changed)
            self.assertIn("NOTICE", lines[0])

    def test_improvements_are_only_a_notice(self):
        for changed in ({"pairs": {STORAGE: 0, CAPTURE: 2}},
                        {"unknown": {"src/cli.rs": 5}, "unknownPairs": 6}, {"partialFiles": 0},
                        {"pairs": {STORAGE: 0}, "unknown": {"src/http.rs": 4},
                         "conformingPairs": 31, "comparablePairs": 41}):
            verdict, lines = self.verdict(**changed)
            self.assertEqual(verdict, "lower", changed)
            self.assertIn("NOTICE", lines[0])
            self.assertIn(RR.UPDATE, lines[-1])

    def test_a_payload_is_summarized_without_its_external_destinations(self):
        measured = payload(
            row("deviates", 4), row("deviates", 6), row("conforms", 9), row("conforms", 1),
            row("unknown", 2, destination=None), row("deviates", 5, excepted=True),
            row("unknown", 1, destination=stdlib("os")),
            row("unknown", 3, destination={**stdlib("serde", "rust"), "standardLibrary": False}))
        measured["partialFiles"] = [{"path": "a.rs"}]
        self.assertEqual(RR.summarize(measured), ({
            "deviatingPairs": 2, "deviatingReferences": 10, "unknownReferences": 2,
            "partialFiles": 1, "comparablePairs": 6, "conformingPairs": 2,
            "unknownPairs": 1,
            "deviatingReferencesByPair": {"src/http.rs": {"src/storage.rs": 10}},
            "unknownReferencesBySource": {"src/http.rs": 2}}, 2))

    def test_a_payload_is_summarized_by_pair_and_by_source_file(self):
        # A pair is the instance's `target` path and its destination's path; an unknown
        # instance's references belong to its `target` path.
        measured, _ = RR.summarize(payload(
            row("deviates", 4), row("deviates", 6, destination={**NATIVE, "path": "src/capture.rs"}),
            row("deviates", 3, source="src/store.rs"),
            row("deviates", 5, source="src/cli.rs", excepted=True),
            row("unknown", 2, destination=None), row("unknown", 7, destination=None, source="src/cli.rs"),
            row("unknown", 1, destination=stdlib("os"), source="src/store.rs")))
        self.assertEqual(measured["deviatingReferencesByPair"], {
            "src/http.rs": {"src/storage.rs": 4, "src/capture.rs": 6},
            "src/store.rs": {"src/storage.rs": 3}})
        self.assertEqual(measured["unknownReferencesBySource"], {"src/http.rs": 2, "src/cli.rs": 7})

    def test_counts_that_disagree_with_the_instances_are_refused(self):
        measured = payload(*TREE)
        measured["counts"]["comparable"] += 1
        with self.assertRaisesRegex(RR.Failed, "counts disagree"):
            RR.summarize(measured)

    def test_a_new_standard_library_import_in_a_tool_does_not_fail(self):
        record, _ = RR.summarize(payload(*TREE))
        added = row("unknown", 1, destination=stdlib("json"), source="tools/rules-ratchet.py")
        measured, external = RR.summarize(payload(*TREE, added))
        self.assertEqual(external, 2)
        self.assertEqual(measured, record)
        self.assertEqual(RR.compare(record, measured)[0], "equal")

    def test_a_new_first_party_upward_reference_fails(self):
        record, _ = RR.summarize(payload(*TREE))
        upward = row("deviates", 1, source="src/storage.rs",
                     destination={**NATIVE, "path": "src/http.rs"})
        measured, _ = RR.summarize(payload(*TREE, upward))
        verdict, lines = RR.compare(record, measured)
        self.assertEqual(verdict, "fail")
        self.assertIn("deviations rose", lines[0])
        self.assertIn("deviatingPairs: recorded 1, measured 2", lines[1])
        self.assertIn("deviatingReferences: recorded 4, measured 5", lines[2])

    def test_a_new_unresolved_first_party_reference_is_a_notice(self):
        record, _ = RR.summarize(payload(*TREE))
        measured, _ = RR.summarize(payload(*TREE, row("unknown", 1, destination=None)))
        verdict, lines = RR.compare(record, measured)
        self.assertEqual(verdict, "lower")
        self.assertIn("  unknownReferences: recorded 2, measured 3", lines)

    def test_a_deviation_that_becomes_unresolved_is_lost_coverage(self):
        record, _ = RR.summarize(payload(*TREE))
        measured, _ = RR.summarize(payload(row("unknown", 4, destination=None), *TREE[1:]))
        verdict, lines = RR.compare(record, measured)
        self.assertEqual(verdict, "fail")
        self.assertIn("lost coverage", lines[0])
        self.assertEqual(lines[1], "  src/http.rs -> src/storage.rs: deviating references "
                         "recorded 4, measured 0; unknown references from src/http.rs: "
                         "recorded 2, measured 6 (+4)")

    def test_a_file_split_that_adds_an_unknown_pair_passes(self):
        # One source's unresolved references become two sources' (#1087): one more
        # unknown pair, the same unknown references.
        whole = row("unknown", 22, destination=None, source="src/scip_admission.rs")
        halves = (row("unknown", 17, destination=None, source="src/producer_pin.rs"),
                  row("unknown", 5, destination=None, source="src/scip_admission.rs"))
        record, _ = RR.summarize(payload(*TREE, whole))
        measured, _ = RR.summarize(payload(*TREE, *halves))
        self.assertEqual(measured["unknownPairs"], record["unknownPairs"] + 1)
        self.assertEqual(measured["unknownReferences"], record["unknownReferences"])
        self.assertEqual(RR.compare(record, measured)[0], "lower")

    def test_the_record_round_trips_byte_for_byte(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "record.json"
            # A measurement: pairs in no order, and every file's unknown references.
            measured = figures()
            measured["deviatingReferencesByPair"] = dict(reversed(
                measured["deviatingReferencesByPair"].items()))
            RR.write_record(path, measured)
            first = path.read_bytes()
            written = RR.read_record(path)
            self.assertEqual({key: written[key] for key in RECORD}, RECORD)
            self.assertEqual(list(written["deviatingReferencesByPair"]), ["src/http.rs", "src/store.rs"])
            self.assertEqual(RR.compare(written, measured)[0], "equal")
            RR.write_record(path, RR.read_record(path))
            self.assertEqual(path.read_bytes(), first)

    def test_the_record_says_what_it_counts(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "record.json"
            RR.write_record(path, RECORD)
            written = json.loads(path.read_text())
            self.assertEqual(list(written), ["schemaVersion", "population", "rule", "measurement", *RR.KEYS,
                                             "deviatingReferencesByPair", "unknownReferencesBySource"])
            self.assertIn("externalPackage", written["population"])
            self.assertIn("--accept-unknown-rise", written["rule"])
            self.assertIn("renames are not detected", written["rule"])
            # A record of an earlier version or shape is refused: version 3 had the
            # totals only (#1140).
            totals = {key: RECORD[key] for key in RR.KEYS}
            for old in ({**written, "schemaVersion": 3}, {"schemaVersion": 3, **totals},
                        {key: value for key, value in written.items()
                         if key != "unknownReferencesBySource"},
                        {**written, "unknownReferencesBySource": {"src/store.rs": 20}}):
                path.write_text(json.dumps(old))
                with self.assertRaisesRegex(RR.Failed, "--update"):
                    RR.read_record(path)

    def test_the_checked_in_record_is_well_formed(self):
        record = RR.read_record(RR.RECORD)
        self.assertEqual(RR.compare(record, record)[0], "equal")
        flat = RR.pairs(record)
        self.assertEqual((len(flat), sum(flat.values())),
                         (record["deviatingPairs"], record["deviatingReferences"]))


class Outcomes(unittest.TestCase):
    """The exit status says whether a comparison was made (spec G1, nightly)."""
    MISSING = ["scip-typescript is missing (no launcher on the declared PATH)"]

    def test_missing_producers_are_a_notice_on_request(self):
        code, line = RR.without_producers(self.MISSING, required=False)
        self.assertEqual(code, 0)
        self.assertTrue(line.startswith("rules ratchet NOT RUN: "), line)
        self.assertIn(self.MISSING[0], line)

    def test_required_producers_that_are_missing_fail_as_not_measured(self):
        code, line = RR.without_producers(self.MISSING, required=True)
        self.assertEqual(code, RR.NOT_MEASURED)
        self.assertTrue(line.startswith("rules ratchet FAILED: not measured: "), line)
        self.assertIn(self.MISSING[0], line)
        self.assertIn("Nothing was compared", line)

    def run_main(self, check=None, argv=()):
        """main() with the measurement replaced by `check`, when one is given; returns
        (status, stderr)."""
        handlers = {signum: signal.getsignal(signum) for signum in (signal.SIGINT, signal.SIGTERM)}
        for name in ("check", "shut_down"):
            self.addCleanup(setattr, RR, name, getattr(RR, name))
        if check is not None:
            RR.check = check
        RR.shut_down = lambda trellis, daemon, home: RR.shutil.rmtree(home) or []
        self.addCleanup(setattr, RR.Signals, "cleaning", False)
        self.addCleanup(setattr, sys, "argv", sys.argv)
        sys.argv = ["rules-ratchet.py", *argv]
        out, err = io.StringIO(), io.StringIO()
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                return RR.main(), err.getvalue()
        finally:
            for signum, handler in handlers.items():
                signal.signal(signum, handler)

    def test_a_measurement_that_could_not_be_made_is_not_a_worse_one(self):
        def fails(*_):
            raise RR.Failed("producer run rust-analyzer: state failed, outcome None, producer_failed")
        code, err = self.run_main(fails)
        self.assertEqual(code, RR.NOT_MEASURED)
        self.assertIn("rules ratchet FAILED: not measured: producer run rust-analyzer", err)
        self.assertNotEqual(RR.NOT_MEASURED, RR.WORSE)

    def test_a_worse_measurement_keeps_its_own_status(self):
        self.assertEqual(self.run_main(lambda *_: RR.WORSE)[0], RR.WORSE)
        self.assertEqual(self.run_main(lambda *_: 0)[0], 0)

    def test_a_missing_record_is_not_measured(self):
        with tempfile.TemporaryDirectory() as directory:
            missing = Path(directory) / "no-record.json"
            code, err = self.run_main(argv=["--record", str(missing), "--binary", "/bin/true"])
        self.assertEqual(code, RR.NOT_MEASURED)
        self.assertIn("rules ratchet FAILED: not measured: FileNotFoundError", err)
        self.assertIn("no-record.json", err)
        self.assertIn("Traceback", err)

    def test_a_missing_binary_is_not_measured(self):
        with tempfile.TemporaryDirectory() as directory:
            missing = Path(directory) / "no-trellis"
            code, err = self.run_main(argv=["--binary", str(missing)])
        self.assertEqual(code, RR.NOT_MEASURED)
        self.assertIn("rules ratchet FAILED: not measured: FileNotFoundError", err)
        self.assertIn("no-trellis", err)

    def test_an_unexpected_exception_is_not_measured(self):
        def breaks(*_):
            raise KeyError("payload")
        code, err = self.run_main(breaks)
        self.assertEqual(code, RR.NOT_MEASURED)
        self.assertIn("rules ratchet FAILED: not measured: KeyError", err)


class NightlyClassifier(unittest.TestCase):
    """The nightly job names a layering outcome only from this script's own FAILED
    lines; the patterns it greps for must be the lines `compare` prints."""
    WORKFLOW = RR.ROOT / ".github/workflows/proptest-nightly.yml"

    def patterns(self):
        text = self.WORKFLOW.read_text()
        found = re.findall(r"if grep -E?q '(\^rules ratchet FAILED: [^']+)' \"\$out/ratchet\.log\"; "
                           r"then outcome=\"?(?:\$\{outcome:\+\$outcome, \})?([a-z-]+)", text)
        return {outcome: re.compile(pattern, re.MULTILINE) for pattern, outcome in found}

    def outcomes(self, log):
        return {outcome for outcome, pattern in self.patterns().items() if pattern.search(log)}

    def compared(self, pairs=None, unknown=None, **totals):
        return "\n".join(RR.compare(RECORD, figures(pairs, unknown, **totals))[1])

    def test_each_comparison_failure_is_named_from_its_own_line(self):
        self.assertEqual(set(self.patterns()), {"record-exceeded", "lost-coverage"})
        self.assertEqual(self.outcomes(self.compared(deviatingPairs=4)), {"record-exceeded"})
        self.assertEqual(self.outcomes(self.compared(partialFiles=2)), {"record-exceeded"})
        self.assertEqual(self.outcomes(self.compared(pairs={("src/web.rs", "src/storage.rs"): 1})),
                         {"record-exceeded"})
        lost = {"pairs": {STORAGE: 0}, "unknown": {"src/http.rs": 6}}
        self.assertEqual(self.outcomes(self.compared(**lost)), {"lost-coverage"})
        self.assertEqual(self.outcomes(self.compared(**lost, partialFiles=2)),
                         {"record-exceeded", "lost-coverage"})
        # The nightly summary is the log's last 40 lines, cut at 600 characters: the
        # line that names the pair is whole in it.
        named = [line for line in self.compared(**lost).splitlines() if "src/http.rs -> " in line]
        self.assertEqual(len(named), 1)
        self.assertLess(len(named[0]), 600)

    def test_an_exit_of_1_without_those_lines_is_not_a_layering_outcome(self):
        for log in ("Traceback (most recent call last):\nFileNotFoundError: no record\n",
                    "rules ratchet FAILED: not measured: producer run rust-analyzer\n",
                    self.compared(), ""):
            self.assertEqual(self.outcomes(log), set(), log)
        text = self.WORKFLOW.read_text()
        self.assertIn('[ -n "$outcome" ] || outcome=not-measured', text)
        self.assertNotIn("worse-than-record", text)

    def test_the_issue_explains_lost_coverage_by_the_per_pair_rule(self):
        # #1140: lost coverage can fail while the repo-wide unknown total falls, so the
        # issue's explanation must not speak of the totals.
        meaning = re.search(r"'lost-coverage': `([^`]+)`", self.WORKFLOW.read_text()).group(1)
        self.assertIn("its source file has more unknown references than recorded for it", meaning)
        self.assertIn("name each such pair", meaning)
        self.assertNotIn("deviations are below", meaning)

    def test_the_issue_explains_a_new_pair_as_the_record_exceeded(self):
        # A new deviating pair is `record-exceeded` even when no total rose.
        log = self.compared(pairs={STORAGE: 0, ("src/web.rs", "src/storage.rs"): 1})
        self.assertEqual(self.outcomes(log), {"record-exceeded"})
        self.assertNotIn("deviatingPairs: recorded", log)
        meaning = re.search(r"'record-exceeded': `([^`]+)`", self.WORKFLOW.read_text()).group(1)
        self.assertIn("a deviating pair was measured that the record does not have", meaning)


def done(code, stdout="", stderr=""):
    return subprocess.CompletedProcess([], code, stdout, stderr)


class Stub:
    """A trellis whose commands return the queued results."""

    def __init__(self, *results):
        self.results, self.calls = list(results), []

    def run(self, *args, timeout=None):
        self.calls.append(args)
        return self.results.pop(0) if len(self.results) > 1 else self.results[0]


class DeadlineRetry(unittest.TestCase):
    DEADLINE = done(1, stderr="trellis: waiting\nError: deadline_exceeded: Architecture is being computed\n")

    def test_a_deadline_is_asked_again_until_the_reply(self):
        stub = Stub(self.DEADLINE, self.DEADLINE, done(1, '{"payload": {}}', "Error: architecture deviations\n"))
        self.assertEqual(RR.rules_check(stub), {"payload": {}})
        self.assertEqual(len(stub.calls), 3)

    def test_another_error_that_mentions_a_deadline_is_not_asked_again(self):
        for other in (done(1, stderr="Error: invalid_rules: layer deadline_exceeded is unknown\n"),
                      done(2, stderr="Error: deadline_exceeded: not the CLI's failure exit\n")):
            stub = Stub(other)
            with self.assertRaisesRegex(RR.Failed, "rules check"):
                RR.rules_check(stub)
            self.assertEqual(len(stub.calls), 1)

    def test_deadlines_are_bounded(self):
        stub = Stub(self.DEADLINE)
        with self.assertRaisesRegex(RR.Failed, "deadline_exceeded"):
            RR.rules_check(stub)
        self.assertEqual(len(stub.calls), RR.RULES_ATTEMPTS)


class ShutDown(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp())
        # A stand-in daemon that runs until its stdin closes, then exits 0.
        self.daemon = subprocess.Popen([sys.executable, "-c", "import sys; sys.stdin.read()"],
                                       stdin=subprocess.PIPE)
        self.addCleanup(self.daemon.stdin.close)
        self.addCleanup(self.daemon.wait)
        self.addCleanup(self.daemon.kill)

    def test_a_failed_stop_is_reported_and_the_daemon_is_ended_by_pid_before_its_home_goes(self):
        seen = []

        class Watched(Stub):
            def run(inner, *args, timeout=None):
                seen.append((self.daemon.poll() is None, self.home.exists()))
                return super().run(*args, timeout=timeout)

        problems = RR.shut_down(Watched(done(1, stderr="Error: daemon_unavailable\n")), self.daemon, self.home)
        self.assertEqual(seen, [(True, True)])
        self.assertIn("daemon stop failed (exit 1)", problems[0])
        self.assertIn(f"daemon {self.daemon.pid} was still running; sent SIGTERM", problems[1])
        self.assertEqual(problems[2], f"daemon {self.daemon.pid} was ended by SIGTERM")
        self.assertEqual(self.daemon.poll(), -15)
        self.assertFalse(self.home.exists())

    def test_a_daemon_that_exits_0_after_the_stop_leaves_no_problem(self):
        class Stops(Stub):
            def run(inner, *args, timeout=None):
                self.daemon.stdin.close()
                return super().run(*args, timeout=timeout)

        self.assertEqual(RR.shut_down(Stops(done(0, '{"stopped": true}')), self.daemon, self.home), [])
        self.assertEqual(self.daemon.poll(), 0)
        self.assertFalse(self.home.exists())

    def test_a_daemon_that_died_abnormally_is_a_problem_though_the_stop_succeeds(self):
        self.daemon.terminate()
        self.daemon.wait()
        seen = []

        class Watched(Stub):
            def run(inner, *args, timeout=None):
                seen.append(self.home.exists())
                return super().run(*args, timeout=timeout)

        problems = RR.shut_down(Watched(done(0, '{"stopped": false}')), self.daemon, self.home)
        self.assertEqual(problems, [f"daemon {self.daemon.pid} was ended by SIGTERM"])
        self.assertEqual(seen, [True])
        self.assertFalse(self.home.exists())

    def test_a_daemon_that_exits_nonzero_is_a_problem(self):
        self.daemon.stdin.close()
        self.daemon.wait()
        self.daemon.returncode = 3
        problems = RR.shut_down(Stub(done(0, '{"stopped": false}')), self.daemon, self.home)
        self.assertEqual(problems, [f"daemon {self.daemon.pid} exited with code 3"])

    def test_without_a_daemon_only_the_home_goes(self):
        self.assertEqual(RR.shut_down(None, None, self.home), [])
        self.assertFalse(self.home.exists())


class Build(unittest.TestCase):
    def test_the_build_writes_only_its_own_target_directory(self):
        calls = []
        original = RR.run
        RR.run = lambda argv, **options: calls.append((argv, options)) or done(0)
        self.addCleanup(setattr, RR, "run", original)
        binary = RR.build(Path("/checkout/target"))
        self.assertEqual(binary, Path("/checkout/target-ratchet/debug/trellis"))
        self.assertEqual(calls[0][0], ["cargo", "build", "--locked", "--bin", "trellis"])
        self.assertEqual(calls[0][1]["env"]["CARGO_TARGET_DIR"], "/checkout/target-ratchet")


if __name__ == "__main__":
    unittest.main()
