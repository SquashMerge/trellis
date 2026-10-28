"""Tests for the semantic import batch emitter (descriptor and page bounds).

Run: python3 -m unittest discover -s tools/semantic-contract/test -p test_semantic_batch.py
"""

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location("semantic_batch", ROOT / "tools/semantic-contract/semantic_batch.py")
batch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(batch)

HEX = "a" * 64


def header():
    return {
        "trustOrigin": "syntheticProfile", "profileId": "trellis-synthetic-scip-v1",
        "decoderVersion": "scip-0.10.0/CapturedScipFactV1/2",
        "producer": {"name": "trellis-synthetic-scip", "version": "1", "identityKind": "generatorSource",
                     "executableHash": HEX, "positionEncoding": 1,
                     "languages": ["java", "rust", "python", "javascript"]},
        "sourceSet": {"id": "set", "rootId": "root", "languages": ["java", "python"], "dependencies": []},
        "sourceRevision": "revision:v2:test", "sourceManifestHash": HEX,
        "components": {"toolchainHash": HEX, "configHash": HEX, "dependencyHash": HEX},
        "metadata": {"protocolVersion": 0, "projectRoot": "file:///w", "textDocumentEncoding": 1},
    }


def sources(count, language="java", width=4):
    return [{"byteLength": 10, "contentHash": HEX, "language": language,
             "relativePath": f"{language}/C{i:0{width}d}.x"} for i in range(count)]


def coverage(rows):
    return [{"artifact": 0, "exclusion": None, "language": row["language"],
             "relativePath": row["relativePath"]} for row in rows]


def emit(directory, source_rows, cover=None, **kwargs):
    return batch.write_batch(directory, header(), source_rows, cover if cover is not None else coverage(source_rows),
                             [{"language": "java", "data": b"scip"}], **kwargs)


class CanonicalTests(unittest.TestCase):
    def test_canonical_is_sorted_compact_utf8(self):
        self.assertEqual(batch.canonical({"b": 1, "a": ["é", None]}), '{"a":["é",null],"b":1}'.encode())

    def test_source_manifest_hash_is_over_canonical_document_rows(self):
        rows = [{"language": "java", "relativePath": "java/A.java", "contentHash": HEX},
                {"language": "python", "relativePath": "python/a.py", "contentHash": "b" * 64}]
        expected = hashlib.sha256(
            b'[{"contentHash":"' + HEX.encode() + b'","document":{"language":"java","path":"java/A.java",'
            b'"sourceSetId":"s"}},{"contentHash":"' + b"b" * 64 + b'","document":{"language":"python",'
            b'"path":"python/a.py","sourceSetId":"s"}}]').hexdigest()
        self.assertEqual(batch.source_manifest_hash("s", rows), expected)

    def test_descriptor_keys_are_closed_set(self):
        self.assertEqual(batch.DESCRIPTOR_KEYS, {
            "formatVersion", "trustOrigin", "profileId", "decoderVersion", "producer", "sourceSet",
            "sourceRevision", "sourceManifestHash", "components", "metadata", "completion", "totals",
            "pages", "artifacts"})


class WriteBatchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.dir = Path(self.temp.name) / "batch"
        self.addCleanup(self.temp.cleanup)

    def read_pages(self, descriptor):
        return [(entry, json.loads((self.dir / Path(entry["relativePath"]).name).read_bytes()))
                for entry in descriptor["pages"]]

    def test_single_page_batch_descriptor_matches_files(self):
        rows = sources(3)
        path, digest = emit(self.dir, rows)
        data = path.read_bytes()
        self.assertEqual(hashlib.sha256(data).hexdigest(), digest)
        descriptor = json.loads(data)
        self.assertEqual(set(descriptor), batch.DESCRIPTOR_KEYS)
        self.assertEqual(descriptor["completion"], "complete")
        self.assertEqual(descriptor["totals"], {
            "sourceDocuments": 3, "coveredDocuments": 3, "excludedDocuments": 0,
            "pageBytes": sum(p["byteLength"] for p in descriptor["pages"]), "artifactBytes": 4})
        self.assertEqual([(p["kind"], p["ordinal"], p["rowCount"]) for p in descriptor["pages"]],
                         [("source", 0, 3), ("coverage", 0, 3)])
        for entry, page in self.read_pages(descriptor):
            raw = (self.dir / entry["relativePath"]).read_bytes()
            self.assertEqual(len(raw), entry["byteLength"])
            self.assertEqual(hashlib.sha256(raw).hexdigest(), entry["sha256"])
            self.assertEqual((page["formatVersion"], page["kind"], page["ordinal"]), (1, entry["kind"], entry["ordinal"]))
            self.assertEqual(len(page["rows"]), entry["rowCount"])
        artifact = descriptor["artifacts"][0]
        self.assertEqual((artifact["ordinal"], artifact["language"], artifact["relativePath"]), (0, "java", "artifact-0.scip"))
        self.assertEqual(artifact["sha256"], hashlib.sha256(b"scip").hexdigest())

    def test_page_closes_before_row_count_would_exceed_bound(self):
        path, _ = emit(self.dir, sources(10), rows_per_page=4)
        descriptor = json.loads(path.read_bytes())
        counts = [(p["kind"], p["ordinal"], p["rowCount"]) for p in descriptor["pages"]]
        self.assertEqual(counts, [("source", 0, 4), ("source", 1, 4), ("source", 2, 2),
                                  ("coverage", 0, 4), ("coverage", 1, 4), ("coverage", 2, 2)])

    def test_default_row_bound_is_one_thousand(self):
        path, _ = emit(self.dir, sources(1001))
        descriptor = json.loads(path.read_bytes())
        self.assertEqual([p["rowCount"] for p in descriptor["pages"] if p["kind"] == "source"], [1000, 1])

    def test_page_closes_before_row_bytes_would_exceed_bound(self):
        rows = sources(12)
        limit = 700
        path, _ = emit(self.dir, rows, rows_per_page=1000, max_page_bytes=limit)
        descriptor = json.loads(path.read_bytes())
        source_pages = [p for p in descriptor["pages"] if p["kind"] == "source"]
        self.assertGreater(len(source_pages), 1)
        for entry in descriptor["pages"]:
            self.assertLessEqual(entry["byteLength"], limit)
        # A page is full: adding the next row would have crossed the byte bound.
        row_length = len(batch.canonical(rows[0]))
        for entry in source_pages[:-1]:
            self.assertGreater(entry["byteLength"] + row_length + 1, limit)
        self.assertEqual(sum(p["rowCount"] for p in source_pages), 12)

    def test_a_row_larger_than_the_page_bound_is_refused(self):
        with self.assertRaisesRegex(ValueError, "single source row"):
            emit(self.dir, sources(1), max_page_bytes=50)

    def test_exclusions_make_the_batch_partial(self):
        rows = sources(2)
        cover = coverage(rows)
        cover[1] = {**cover[1], "artifact": None, "exclusion": "failed"}
        path, _ = emit(self.dir, rows, cover)
        descriptor = json.loads(path.read_bytes())
        self.assertEqual(descriptor["completion"], "partial")
        self.assertEqual((descriptor["totals"]["coveredDocuments"], descriptor["totals"]["excludedDocuments"]), (1, 1))

    def test_unsorted_or_duplicate_rows_are_refused(self):
        rows = sources(2)
        with self.assertRaisesRegex(ValueError, "strictly increasing"):
            emit(self.dir, [rows[1], rows[0]])
        with self.assertRaisesRegex(ValueError, "strictly increasing"):
            emit(self.dir, [rows[0], rows[0]])
        mixed = sources(1, "python") + sources(1, "java")
        with self.assertRaisesRegex(ValueError, "strictly increasing"):
            emit(self.dir, mixed)

    def test_languages_sort_in_admission_order_not_alphabetically(self):
        rows = sources(1, "java") + sources(1, "rust") + sources(1, "python") + sources(1, "javascript")
        path, _ = emit(self.dir, rows)
        self.assertEqual(json.loads(path.read_bytes())["totals"]["sourceDocuments"], 4)

    def test_coverage_row_needs_exactly_one_disposition(self):
        rows = sources(1)
        both = [{**coverage(rows)[0], "exclusion": "failed"}]
        neither = [{**coverage(rows)[0], "artifact": None}]
        for bad in (both, neither):
            with self.assertRaisesRegex(ValueError, "exactly one"):
                emit(self.dir, rows, bad)

    def test_header_must_have_exactly_the_non_derived_keys(self):
        with self.assertRaisesRegex(ValueError, "header keys"):
            batch.write_batch(self.dir, {**header(), "extra": 1}, sources(1), coverage(sources(1)),
                              [{"language": "java", "data": b"x"}])


if __name__ == "__main__":
    unittest.main()
