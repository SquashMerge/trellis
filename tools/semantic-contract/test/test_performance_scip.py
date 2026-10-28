"""SMALL-only correctness tests for the synthetic SCIP generator; no MEDIUM or LARGE generation.

Run: python3 -m unittest discover -s tools/semantic-contract/test -p test_performance_scip.py
Node is used only to create the SMALL source fixture with the cohort generator.
The binary generator and independent decoder use only Python's standard library.
"""

import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "tools/semantic-contract/generate-performance-scip.py"
spec = importlib.util.spec_from_file_location("performance_scip", SCRIPT)
generator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(generator)
CANONICAL_MANIFEST = ROOT / "tools/synthetic-cohorts/manifest-v2.json"
PROFILE = ROOT / "docs/semantic-evidence/synthetic-profiles/trellis-synthetic-scip-v1.json"
PREPARATION = ROOT / "docs/semantic-evidence/performance-gate-preparation.json"


def read_integer(data, cursor):
    value = 0
    for shift in range(0, 70, 7):
        if cursor >= len(data):
            raise ValueError("truncated varint")
        byte = data[cursor]
        cursor += 1
        value |= (byte & 127) << shift
        if byte < 128:
            return value, cursor
    raise ValueError("oversized varint")


def decode(data):
    """Independent test wire reader: retain repeated fields and wire types."""
    fields = {}
    cursor = 0
    while cursor < len(data):
        tag, cursor = read_integer(data, cursor)
        number, wire = tag >> 3, tag & 7
        if number == 0:
            raise ValueError("zero field")
        if wire == 0:
            value, cursor = read_integer(data, cursor)
        elif wire == 2:
            length, cursor = read_integer(data, cursor)
            value = data[cursor:cursor + length]
            cursor += length
            if cursor > len(data):
                raise ValueError("truncated field")
        else:
            raise ValueError("unexpected wire type")
        fields.setdefault(number, []).append((wire, value))
    return fields


def values(fields, number):
    return [value for _, value in fields.get(number, [])]


def one(fields, number):
    result = values(fields, number)
    if len(result) != 1:
        raise AssertionError(f"expected one field {number}, got {len(result)}")
    return result[0]


def packed(data):
    result, cursor = [], 0
    while cursor < len(data):
        value, cursor = read_integer(data, cursor)
        result.append(value)
    return result


def digest(data):
    return hashlib.sha256(data).hexdigest()


class WireTests(unittest.TestCase):
    def test_known_wire_vector(self):
        # Hand-authored SCIP 0.10 Index.documents(2) containing Document
        # path(1), occurrence(2), SymbolInformation(3), language(4), encoding(6).
        # Occurrence range(1)=[0,4,5], symbol(2)="s", roles(3)=Definition(1).
        # SymbolInformation symbol(1)="s", kind(5)=Function(17), display(6)="f".
        expected = bytes.fromhex(
            "1226" "0a04612e7079" "120a0a030004051201731801"
            "1a080a01732811320166" "2206707974686f6e" "3001")
        doc = generator.field(1, "a.py")
        doc += generator.field(2, generator.occurrence(0, 4, 5, "s", True))
        doc += generator.field(3, generator.field(1, "s") + generator.scalar(5, 17) + generator.field(6, "f"))
        doc += generator.field(4, "python") + generator.scalar(6, 1)
        self.assertEqual(generator.field(2, doc), expected)
        decoded = decode(one(decode(expected), 2))
        self.assertEqual(one(decoded, 1), b"a.py")
        self.assertEqual(packed(one(decode(one(decoded, 2)), 1)), [0, 4, 5])
        self.assertEqual(generator.varint(300), b"\xac\x02")

    def test_all_size_templates_have_bounded_source_backed_density(self):
        extensions = {"java": "java", "rust": "rs", "python": "py", "javascript": "js"}
        for size in ("small", "medium", "large"):
            for language, extension in extensions.items():
                with self.subTest(size=size, language=language):
                    source = (ROOT / f"tools/synthetic-cohorts/templates/{size}/{language}.{extension}").read_bytes()
                    path = f"{language}/C{size}0000.{extension}"
                    paths = {f"{language}/C{size}{i:04d}.{extension}" for i in range(3)}
                    encoded, counts = generator.encode_document(language, {"relativePath": path}, source, paths)
                    self.assertEqual(len(values(decode(encoded), 3)), counts["declarations"])
                    self.assertGreater(counts["declarations"], 0)
                    self.assertLessEqual(counts["referenceOccurrences"], 2 * counts["callableBodies"])
                    self.assertEqual(counts["relationships"], 0)


class SyntheticGenerationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="trellis-performance-scip-test-")
        cls.work = Path(cls.temp.name)
        cls.cohort = cls.work / "cohort"
        result = subprocess.run(["node", str(ROOT / "tools/synthetic-cohorts/generate.mjs"), "--out", str(cls.cohort),
                                 "--size", "small"], cwd=ROOT, capture_output=True, text=True, check=False)
        if result.returncode:
            cls.temp.cleanup()
            raise AssertionError(result.stdout + result.stderr)
        cls.out = cls.work / "first"
        cls.command = [sys.executable, str(SCRIPT), "--cohort", str(cls.cohort), "--size", "small"]
        result = subprocess.run(cls.command + ["--out", str(cls.out)], cwd=ROOT, capture_output=True, text=True)
        if result.returncode:
            cls.temp.cleanup()
            raise AssertionError(result.stdout + result.stderr)
        cls.inventory = json.loads((cls.out / "inventory.json").read_bytes())

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def run_cli(self, out, *extra):
        return subprocess.run(self.command + ["--out", str(out), *extra], cwd=ROOT, capture_output=True, text=True)

    def test_small_generation_is_deterministic_and_matches_preparation_record(self):
        second = self.work / "second"
        result = self.run_cli(second)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sorted(p.name for p in self.out.iterdir()), sorted(p.name for p in second.iterdir()))
        for file in self.out.iterdir():
            self.assertEqual(file.read_bytes(), (second / file.name).read_bytes())
        record = json.loads(PREPARATION.read_bytes())
        self.assertEqual(record["formatVersion"], 1)
        self.assertEqual(record["generator"]["sha256"], digest(SCRIPT.read_bytes()))
        self.assertEqual(record["cohortManifest"]["sha256"], digest(CANONICAL_MANIFEST.read_bytes()))
        small = record["sizes"]["small"]
        self.assertEqual(small["inventorySha256"], digest((self.out / "inventory.json").read_bytes()))
        self.assertEqual(small["artifacts"], self.inventory["artifacts"])

    def test_one_binary_artifact_per_language_and_separate_inventory(self):
        self.assertEqual(sorted(p.name for p in self.out.iterdir()),
                         ["inventory.json", "java.scip", "javascript.scip", "python.scip", "rust.scip"])
        self.assertEqual([a["language"] for a in self.inventory["artifacts"]], list(generator.LANGUAGES))
        documents = self.inventory["documents"]
        self.assertEqual(len(documents), 100)
        self.assertEqual(len({(d["language"], d["relativePath"]) for d in documents}), 100)
        for ordinal, artifact in enumerate(self.inventory["artifacts"]):
            data = (self.out / artifact["relativePath"]).read_bytes()
            self.assertEqual(len(data), artifact["bytes"])
            self.assertEqual(digest(data), artifact["sha256"])
            docs = values(decode(data), 2)
            self.assertEqual(len(docs), artifact["documents"])
            owned = [d for d in documents if d["artifact"] == ordinal]
            self.assertEqual([d["relativePath"] for d in owned],
                             [one(decode(doc), 1).decode() for doc in docs])
            self.assertEqual({d["language"] for d in owned}, {artifact["language"]})

    def test_inventory_labels_itself_synthetic(self):
        inventory = self.inventory
        self.assertIs(inventory["synthetic"], True)
        self.assertEqual(inventory["profileId"], "trellis-synthetic-scip-v1")
        self.assertEqual(inventory["positionEncoding"], "utf8")
        self.assertEqual(inventory["size"], "small")
        self.assertEqual(inventory["generator"], {"name": "trellis-synthetic-scip", "version": "1",
                                                  "sourceSha256": digest(SCRIPT.read_bytes())})
        for artifact in inventory["artifacts"]:
            self.assertEqual(artifact["relationships"], 0)
            self.assertEqual(set(artifact), {"language", "relativePath", "bytes", "sha256", "documents",
                                             "occurrences", "symbols", "relationships"})

    def test_document_paths_are_cohort_root_relative(self):
        manifest = json.loads(CANONICAL_MANIFEST.read_bytes())
        expected = {row["path"][len("small/"):] for row in manifest["files"] if row["path"].startswith("small/")}
        found = set()
        for artifact in self.inventory["artifacts"]:
            for binary in values(decode((self.out / artifact["relativePath"]).read_bytes()), 2):
                doc = decode(binary)
                path = one(doc, 1).decode()
                self.assertFalse(path.startswith("small/"), path)
                self.assertTrue(path.startswith(artifact["language"] + "/"), path)
                self.assertTrue(b"`small/" not in b"".join(values(doc, 3)))
                found.add(path)
        self.assertEqual(found, expected)

    def test_tool_info_and_encoding_match_synthetic_profile(self):
        profile = json.loads(PROFILE.read_bytes())
        self.assertEqual(profile["generator"]["name"], generator.GENERATOR_NAME)
        self.assertEqual(profile["generator"]["version"], generator.GENERATOR_VERSION)
        for artifact in self.inventory["artifacts"]:
            index = decode((self.out / artifact["relativePath"]).read_bytes())
            metadata = decode(one(index, 1))
            self.assertNotIn(3, metadata)  # No workspace-bound project root.
            tool = decode(one(metadata, 2))
            self.assertEqual(one(tool, 1), b"trellis-synthetic-scip")
            self.assertEqual(one(tool, 2), b"1")
            self.assertEqual(one(metadata, 4), 1)
            for binary in values(index, 2):
                self.assertEqual(one(decode(binary), 6), 1)

    def test_profile_pins_generator_source_sha256(self):
        profile = json.loads(PROFILE.read_bytes())
        self.assertEqual(profile["profileId"], "trellis-synthetic-scip-v1")
        self.assertIs(profile["synthetic"], True)
        self.assertEqual(profile["identityKind"], "generatorSource")
        self.assertEqual(profile["generator"]["sourcePath"], "tools/semantic-contract/generate-performance-scip.py")
        self.assertEqual(profile["generator"]["sourceSha256"], digest(SCRIPT.read_bytes()))
        self.assertEqual(profile["generator"]["positionEncoding"], "utf8")
        self.assertEqual(profile["generator"]["languages"], list(generator.LANGUAGES))

    def test_density_is_definitions_plus_at_most_two_direct_calls(self):
        all_definitions, all_references = set(), []
        for artifact in self.inventory["artifacts"]:
            index = decode((self.out / artifact["relativePath"]).read_bytes())
            definitions = references = symbol_count = 0
            for binary in values(index, 2):
                doc = decode(binary)
                path = one(doc, 1).decode()
                lines = (self.cohort / "small" / path).read_bytes().splitlines()
                symbols = [decode(raw) for raw in values(doc, 3)]
                names = {one(symbol, 1): one(symbol, 6) for symbol in symbols}
                symbol_count += len(symbols)
                seen, per_symbol = set(), {}
                for raw in values(doc, 2):
                    self.assertNotIn(raw, seen)
                    seen.add(raw)
                    occ = decode(raw)
                    line, start, end = packed(one(occ, 1))
                    spelling = lines[line][start:end]
                    symbol = one(occ, 2)
                    if values(occ, 3) == [1]:
                        definitions += 1
                        self.assertEqual(spelling, names[symbol])
                        self.assertNotIn(symbol, all_definitions)
                        all_definitions.add(symbol)
                    else:
                        references += 1
                        self.assertIn(spelling, (b"f0", b"peer0", b"peer1"))
                        self.assertEqual(lines[line][end:end + 1], b"(")
                        all_references.append(symbol)
            # Every declaration has a definition and SymbolInformation; calls are bounded per body.
            self.assertEqual(definitions, symbol_count)
            self.assertEqual(definitions, artifact["symbols"])
            self.assertEqual(definitions + references, artifact["occurrences"])
            self.assertGreater(references, 0)
            self.assertLessEqual(references, 2 * definitions)
        self.assertTrue(set(all_references).issubset(all_definitions))

    def test_tampered_cohort_source_is_refused_before_emitting(self):
        target = next((self.cohort / "small/java").glob("*.java"))
        original = target.read_bytes()
        for label, content in (("hash", bytes([original[0] ^ 1]) + original[1:]),
                               ("length", original + b"\n")):
            with self.subTest(label=label):
                try:
                    target.write_bytes(content)
                    out = self.work / ("bad-" + label)
                    result = self.run_cli(out)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("source bytes/hash mismatch", result.stderr)
                    self.assertFalse(out.exists())
                finally:
                    target.write_bytes(original)
        extra = self.cohort / "small/python/extra.py"
        try:
            extra.write_text("def extra(): pass\n")
            result = self.run_cli(self.work / "bad-inventory")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("inventory mismatch", result.stderr)
        finally:
            extra.unlink()
        missing = target.with_suffix(".saved")
        target.rename(missing)
        try:
            result = self.run_cli(self.work / "bad-missing")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("inventory mismatch", result.stderr)
        finally:
            missing.rename(target)
        manifest = self.cohort / "manifest.json"
        original_manifest = manifest.read_bytes()
        try:
            manifest.write_bytes(original_manifest + b"\n")
            result = self.run_cli(self.work / "bad-manifest")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("canonical manifest-v2.json bytes", result.stderr)
        finally:
            manifest.write_bytes(original_manifest)
        self.assertFalse((self.work / "bad-manifest").exists())

    def test_fresh_output_directory_is_required_and_failure_cleans_up(self):
        second = self.work / "fresh"
        self.assertEqual(self.run_cli(second).returncode, 0)
        original = (second / "inventory.json").read_bytes()
        result = self.run_cli(second)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fresh nonexistent", result.stderr)
        self.assertEqual((second / "inventory.json").read_bytes(), original)
        result = self.run_cli(self.cohort / "forbidden-output")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside the read-only cohort", result.stderr)
        # A failure after the output directory exists removes everything written.
        failing = self.work / "failing"
        with mock.patch.object(generator, "encode_document", side_effect=ValueError("injected")):
            with self.assertRaises(ValueError):
                generator.generate(self.cohort, "small", failing)
        self.assertFalse(failing.exists())


if __name__ == "__main__":
    unittest.main()
