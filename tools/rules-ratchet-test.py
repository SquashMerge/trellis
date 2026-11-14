#!/usr/bin/env python3
"""Offline checks of tools/rules-ratchet.py's comparison and record (#1008)."""
import importlib.util
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "rules_ratchet", Path(__file__).with_name("rules-ratchet.py"))
RR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RR)

RECORD = {"deviatingPairs": 3, "deviatingReferences": 10, "comparablePairs": 40,
          "conformingPairs": 30, "unknownPairs": 7, "partialFiles": 1}


def row(status, references, excepted=False):
    return {"status": status, "referenceCount": references, "excepted": excepted}


class Ratchet(unittest.TestCase):
    def verdict(self, **changed):
        return RR.compare(RECORD, {**RECORD, **changed})

    def test_the_same_figures_pass(self):
        verdict, lines = self.verdict()
        self.assertEqual(verdict, "equal")
        self.assertIn("deviatingPairs 3", lines[0])

    def test_an_added_pair_or_reference_fails(self):
        for key in ("deviatingPairs", "deviatingReferences"):
            verdict, lines = self.verdict(**{key: RECORD[key] + 1})
            self.assertEqual(verdict, "fail", key)
            self.assertIn("deviations rose", lines[0])
            self.assertIn(f"{key}: recorded {RECORD[key]}, measured {RECORD[key] + 1}", lines[1])

    def test_fewer_deviations_on_lost_coverage_fail(self):
        for key, value in (("comparablePairs", 39), ("unknownPairs", 8), ("partialFiles", 2)):
            verdict, lines = self.verdict(deviatingPairs=2, deviatingReferences=5, **{key: value})
            self.assertEqual(verdict, "fail", key)
            self.assertIn("lost coverage", lines[0])

    def test_fewer_deviations_on_no_worse_coverage_are_only_a_notice(self):
        verdict, lines = self.verdict(deviatingPairs=2, deviatingReferences=5,
                                      conformingPairs=31, comparablePairs=41, unknownPairs=7)
        self.assertEqual(verdict, "lower")
        self.assertIn("NOTICE", lines[0])
        self.assertIn(RR.UPDATE, lines[-1])

    def test_a_payload_is_summarized(self):
        payload = {
            "counts": {"comparable": 5, "conforms": 2, "deviates": 2, "unknown": 1, "excepted": 0},
            "instances": [row("deviates", 4), row("deviates", 6), row("conforms", 9),
                          row("conforms", 1), row("unknown", 2)],
            "partialFiles": [{"path": "a.rs"}],
        }
        self.assertEqual(RR.summarize(payload), {
            "deviatingPairs": 2, "deviatingReferences": 10, "comparablePairs": 5,
            "conformingPairs": 2, "unknownPairs": 1, "partialFiles": 1})

    def test_the_record_round_trips_byte_for_byte(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "record.json"
            RR.write_record(path, RECORD)
            first = path.read_bytes()
            self.assertEqual({key: RR.read_record(path)[key] for key in RR.KEYS}, RECORD)
            RR.write_record(path, RR.read_record(path))
            self.assertEqual(path.read_bytes(), first)

    def test_the_checked_in_record_is_well_formed(self):
        record = RR.read_record(RR.RECORD)
        self.assertEqual(RR.compare(record, record)[0], "equal")


if __name__ == "__main__":
    unittest.main()
