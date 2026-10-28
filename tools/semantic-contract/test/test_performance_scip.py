"""Small correctness tests, not a benchmark or admission bypass.

Run: python3 -m unittest discover -s tools/semantic-contract/test -p test_performance_scip.py
Node is used only to create the canonical source fixture with its own generator.
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


ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "tools/semantic-contract/generate-performance-scip.py"
spec = importlib.util.spec_from_file_location("performance_scip", SCRIPT)
generator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(generator)


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
                    path = f"{size}/{language}/C{size}0000.{extension}"
                    paths = {f"{size}/{language}/C{size}{i:04d}.{extension}" for i in range(3)}
                    encoded, counts = generator.encode_document(language, {"path": path}, source, paths)
                    expected_decls = (39 if language in ("java", "python", "javascript") else 40) if size == "large" else (72 if language == "java" else 74 if language in ("python", "javascript") else 75)
                    self.assertEqual(counts["declarations"], expected_decls)
                    self.assertEqual(len(values(decode(encoded), 3)), expected_decls)
                    self.assertGreaterEqual(counts["referenceOccurrences"], 2 * (36 if size == "large" else 69))
                    self.assertLessEqual(counts["referenceOccurrences"], 2 * counts["callableBodies"])
                    self.assertEqual(counts["relationships"], 0)


class CanonicalPreparationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="trellis-performance-scip-test-")
        cls.work = Path(cls.temp.name)
        cls.cohort = cls.work / "cohort"
        result = subprocess.run(["node", str(ROOT / "tools/synthetic-cohorts/generate.mjs"), "--out", str(cls.cohort)],
                                cwd=ROOT, capture_output=True, text=True, check=False)
        if result.returncode:
            cls.temp.cleanup()
            raise AssertionError(result.stdout + result.stderr)
        cls.out = cls.work / "first"
        cls.command = [sys.executable, str(SCRIPT), "--cohort-root", str(cls.cohort),
                       "--cohort-manifest", str(cls.cohort / "manifest.json"), "--size", "small"]
        result = subprocess.run(cls.command + ["--out", str(cls.out)], cwd=ROOT, capture_output=True, text=True)
        if result.returncode:
            cls.temp.cleanup()
            raise AssertionError(result.stdout + result.stderr)
        cls.manifest = json.loads((cls.out / "preparation-manifest.json").read_bytes())

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def run_cli(self, out, *extra):
        return subprocess.run(self.command + ["--out", str(out), *extra], cwd=ROOT, capture_output=True, text=True)

    def test_four_languages_every_document_spans_counters_and_digests(self):
        manifest = self.manifest
        self.assertEqual(manifest["kind"], "syntheticScipPerformancePreparation")
        self.assertFalse(manifest["admission"]["admitted"])
        self.assertFalse(manifest["admission"]["referenceProfileAdmissionBypassed"])
        self.assertFalse(manifest["density"]["nativeJoinsClaimed"])
        self.assertEqual(manifest["generator"]["sha256"], digest(SCRIPT.read_bytes()))
        self.assertEqual(manifest["input"]["selectedSourceFiles"], 100)
        self.assertEqual(manifest["input"]["selectedSourceBytes"], 1677722)
        self.assertEqual(manifest["input"]["manifestSha256"], digest((self.cohort / "manifest.json").read_bytes()))
        self.assertEqual({row["language"] for row in manifest["artifacts"]}, {"java", "rust", "python", "javascript"})
        canonical = json.loads((self.cohort / "manifest.json").read_bytes())
        input_rows = {row["path"]: row for row in canonical["files"] if row["path"].startswith("small/")}
        all_paths = []
        for artifact in manifest["artifacts"]:
            data = (self.out / artifact["path"]).read_bytes()
            self.assertEqual(len(data), artifact["byteLength"])
            self.assertEqual(digest(data), artifact["sha256"])
            index = decode(data)
            metadata = decode(one(index, 1))
            self.assertNotIn(3, metadata)  # No unbound checkout-root claim.
            tool = decode(one(metadata, 2))
            self.assertEqual(one(tool, 1), b"trellis-synthetic-performance-scip")
            self.assertEqual(one(metadata, 4), 1)
            docs = values(index, 2)
            self.assertEqual(len(docs), 25)
            self.assertEqual(artifact["counters"]["documents"], 25)
            sums = {}
            all_definitions, all_references = set(), []
            for binary, declared in zip(docs, artifact["documents"]):
                doc = decode(binary)
                path = one(doc, 1).decode()
                all_paths.append(path)
                self.assertEqual(path, declared["relativePath"])
                self.assertEqual(one(doc, 4).decode(), artifact["language"])
                self.assertEqual(one(doc, 6), 1)
                source = (self.cohort / path).read_bytes()
                self.assertEqual(digest(source), declared["sourceSha256"])
                self.assertEqual(declared["sourceSha256"], input_rows[path]["sha256"])
                self.assertEqual(len(source), declared["sourceBytes"])
                lines = source.splitlines()
                seen = set()
                definitions, references = 0, 0
                symbols = [decode(raw) for raw in values(doc, 3)]
                names = {one(symbol, 1): one(symbol, 6) for symbol in symbols}
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
                counts = declared["counters"]
                self.assertEqual(definitions, counts["definitionOccurrences"])
                self.assertEqual(references, counts["referenceOccurrences"])
                self.assertEqual(len(symbols), counts["symbolInformation"])
                self.assertEqual(len(symbols) + len(seen), counts["candidateFacts"])
                for key, count in counts.items():
                    sums[key] = sums.get(key, 0) + count
            self.assertEqual(sums, artifact["counters"])
            self.assertTrue(set(all_references).issubset(all_definitions))
        self.assertEqual(set(all_paths), set(input_rows))
        self.assertEqual(len(all_paths), len(set(all_paths)))

    def test_regeneration_is_byte_identical_and_output_must_be_fresh(self):
        second = self.work / "second"
        result = self.run_cli(second)
        self.assertEqual(result.returncode, 0, result.stderr)
        for file in self.out.iterdir():
            self.assertEqual(file.read_bytes(), (second / file.name).read_bytes())
        original = (second / "preparation-manifest.json").read_bytes()
        result = self.run_cli(second)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fresh nonexistent", result.stderr)
        self.assertEqual((second / "preparation-manifest.json").read_bytes(), original)

    def test_exact_manifest_source_hash_length_and_inventory_rejected(self):
        # Each failure occurs before an output is created. Restore the fixture
        # immediately so test ordering does not affect the next assertion.
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
        result = self.run_cli(self.cohort / "forbidden-output")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside the read-only cohort", result.stderr)


if __name__ == "__main__":
    unittest.main()
