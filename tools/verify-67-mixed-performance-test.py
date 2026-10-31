#!/usr/bin/env python3
"""Check private-HOME cleanup without launching Trellis or signaling processes."""
import importlib.util
import json
import os
import re
import signal
import sqlite3
import sys
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location(
    "mixed_runner", Path(__file__).with_name("verify-67-mixed-performance.py"))
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)

HEX = "a" * 64


class HomeProcessCleanup(unittest.TestCase):
    def setUp(self):
        self.home = Path("/tmp/mixed-runner-test-home")
        self.binary = Path("/tmp/mixed-runner-test-binary")

    def result(self, code=0, output="", error=""):
        return subprocess.CompletedProcess([], code, output, error)

    def test_portable_snapshot_finds_only_live_private_home_binary(self):
        matching = f"123 S {self.binary} daemon HOME={self.home} PATH=/bin\n"
        rows = matching + (
            f"124 Z {self.binary} daemon HOME={self.home} PATH=/bin\n"
            f"125 S {self.binary} daemon HOME=/tmp/other-home PATH=/bin\n"
            f"126 S {self.binary} daemon HOME={self.home}-other PATH=/bin\n"
            f"127 S /tmp/other-binary daemon HOME={self.home} PATH=/bin\n"
        )
        rechecks = []

        def ps(args, **kwargs):
            if "-p" in args:
                pid = int(args[args.index("-p") + 1])
                self.assertEqual(pid, 123)
                rechecks.append(pid)
                return self.result(output=matching) if len(rechecks) == 1 else self.result(code=1)
            # procps rejects the original mixed BSD/System V selector.
            if "-a" in args or "-x" in args:
                return self.result(code=1, error="error: must set personality to get -x option")
            self.assertIn("-A", args)
            self.assertIn("eww", args)  # environment-bearing, untruncated HOME evidence
            return self.result(output=rows)

        with patch.object(RUNNER.subprocess, "run", side_effect=ps), \
                patch.object(RUNNER.os, "kill") as kill, \
                patch.object(RUNNER, "record"), \
                patch.object(RUNNER, "election_free", return_value=True) as election:
            RUNNER.stop_home_processes(self.home, self.binary)
        kill.assert_called_once_with(123, signal.SIGTERM)
        self.assertEqual(rechecks, [123, 123])
        election.assert_called_once_with(self.home)

    def test_empty_global_snapshot_requires_success(self):
        for code in (0, 1):
            with self.subTest(returncode=code), \
                    patch.object(RUNNER.subprocess, "run", return_value=self.result(code=code)), \
                    patch.object(RUNNER.os, "kill") as kill:
                if code == 0:
                    RUNNER.stop_home_processes(self.home, self.binary)
                else:
                    with self.assertRaisesRegex(RuntimeError, "cannot enumerate"):
                        RUNNER.stop_home_processes(self.home, self.binary)
                kill.assert_not_called()

    def test_ps_failure_preserves_diagnostic(self):
        error = "error: must set personality to get -x option"
        with patch.object(RUNNER.subprocess, "run", return_value=self.result(code=1, error=error)), \
                patch.object(RUNNER.os, "kill") as kill:
            with self.assertRaisesRegex(RuntimeError, error):
                RUNNER.stop_home_processes(self.home, self.binary)
        kill.assert_not_called()

    def test_real_host_ps_can_take_global_snapshot(self):
        with tempfile.TemporaryDirectory(prefix="mixed-runner-ps-") as scratch:
            home = Path(scratch) / "home"
            binary = Path(scratch) / "nonexistent-trellis"
            with patch.object(RUNNER.os, "kill") as kill:
                RUNNER.stop_home_processes(home, binary)
            kill.assert_not_called()


class Rusage(unittest.TestCase):
    def test_command_records_rusage_when_requested(self):
        with tempfile.TemporaryDirectory() as tmp, \
                patch.object(RUNNER, "EVIDENCE", Path(tmp)), \
                patch.object(RUNNER, "DEADLINE", RUNNER.time.monotonic() + 100), \
                patch.object(RUNNER, "record"):
            usage = {}
            RUNNER.command([sys.executable, "-c", "x=bytearray(64<<20); x[::4096]=b'1'*len(x[::4096])"],
                           env=os.environ.copy(), rusage=usage)
            plain = RUNNER.command([sys.executable, "-c", "print('ok')"], env=os.environ.copy())
        self.assertGreaterEqual(usage["maxRssBytes"], 64 << 20)
        self.assertEqual(plain.strip(), "ok")

    def test_daemon_stop_reaps_with_wait4_when_recording(self):
        proc = Mock(pid=4321, returncode=None)
        usage = Mock(ru_maxrss=2048)
        with tempfile.TemporaryDirectory() as tmp, \
                patch.object(RUNNER, "EVIDENCE", Path(tmp)), \
                patch.object(RUNNER.os, "wait4", return_value=(4321, 0, usage)) as wait4, \
                patch.object(RUNNER, "stop_home_processes"), \
                patch.object(RUNNER, "election_free", return_value=True), \
                patch.object(RUNNER, "record"):
            daemon = RUNNER.Daemon(Path("/tmp/b"), Path("/tmp/h"), "t", record_rusage=True)
            daemon.proc = proc
            daemon.stop()
        wait4.assert_called_with(4321, RUNNER.os.WNOHANG)
        proc.poll.assert_not_called()
        proc.wait.assert_not_called()
        factor = 1024 if sys.platform.startswith("linux") else 1
        self.assertEqual(daemon.rusage, {"maxRssBytes": 2048 * factor})
        self.assertEqual(proc.returncode, 0)


    def test_reap_with_rusage_keeps_usage_of_already_exited_child(self):
        proc = subprocess.Popen([sys.executable, "-c", "pass"])
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if subprocess.run(["ps", "-o", "stat=", "-p", str(proc.pid)], capture_output=True,
                              text=True).stdout.strip().startswith("Z"):
                break
            time.sleep(.02)
        result = RUNNER.reap_with_rusage(proc)
        self.assertGreater(result["maxRssBytes"], 0)
        self.assertEqual(proc.returncode, 0)


class SchemaParity(unittest.TestCase):
    EXCLUDED = RUNNER.CONTROL_TABLES | RUNNER.DEBT_TABLES | RUNNER.FTS_SHADOW_TABLES

    def schema_db(self):
        source = (RUNNER.REPO / "src/store.rs").read_text()
        match = re.search(r'const CACHE_SCHEMA_V(\d+): &str = r#"(.*?)"#;', source, re.S)
        self.assertIsNotNone(match)
        self.assertEqual(int(match.group(1)), RUNNER.SCHEMA_VERSION)
        db = sqlite3.connect(":memory:")
        self.addCleanup(db.close)
        try:
            db.executescript(match.group(2))
        except sqlite3.OperationalError as error:
            self.fail(f"the schema needs FTS5 with the trigram tokenizer: {error}")
        return db

    @staticmethod
    def tables(db):
        return {row[0] for row in db.execute("""SELECT name FROM sqlite_master WHERE type='table'
            AND name NOT LIKE 'sqlite_%' AND name!='index_metadata' AND name NOT LIKE '%trigrams%'""")}

    def test_schema_text_table_set_matches_required_tables(self):
        self.assertEqual(self.tables(self.schema_db()),
                         RUNNER.REQUIRED_TABLES | RUNNER.CONTROL_TABLES | RUNNER.FTS_SHADOW_TABLES)

    def test_every_schema_table_has_a_selected_scope(self):
        for table in sorted(self.tables(self.schema_db()) - self.EXCLUDED):
            self.assertTrue(RUNNER.selected_where(table), table)
        with self.assertRaises(RuntimeError):
            RUNNER.selected_where("not_a_table")

    def populate(self, db, workspace):
        root = Path(workspace).resolve(strict=True)
        root_id = "path-" + RUNNER.hashlib.sha256(str(root).encode()).hexdigest()
        source_set = "source-set:v1:" + root_id
        paths = [("javascript", "js/a.js"), ("javascript", "js/b.js"),
                 ("python", "py/a.py"), ("python", "py/b.py")]
        hashes = {key: f"{index + 1:064x}" for index, key in enumerate(paths)}
        for _language, path in paths:
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_bytes(b"x = 1\n")

        def inventory(revision):
            return json.dumps([{"key": {"sourceSetId": source_set, "language": language, "path": path},
                                "contentHash": hashes[(language, path)], "revisionId": revision}
                               for language, path in paths])
        db.execute("INSERT INTO native_source_sets VALUES(?,?)", (source_set, root_id))
        for ordinal, language in enumerate(("javascript", "python")):
            db.execute("INSERT INTO native_source_set_languages VALUES(?,?,?)", (source_set, language, ordinal))
        pins = {"r1": "pin:v1:gen:1", "import": "pin:v1:gen:2", "r2": "pin:v1:gen:3"}
        natives = {"r1": "revision:v2:one", "import": "revision:v2:two", "r2": "revision:v2:two"}
        for name, pin in pins.items():
            db.execute("INSERT INTO native_revisions VALUES(?,?,?,?,?,?,?,'[]','{}','[]','[]',NULL,'[]',0,'{}','[]',?)",
                       (pin, source_set, HEX, HEX, HEX, natives[name], inventory(natives[name]),
                        int(pin[-1])))
        for ordinal, (language, path) in enumerate(paths):
            db.execute("INSERT INTO revision_documents VALUES(?,?,?,?,?,NULL,NULL,1,1,'complete',NULL,?,0)",
                       (pins["r2"], language, path, ordinal + 1, ordinal + 1, ordinal))
        header = RUNNER.capture_header(db, pins["r1"], 4)
        covered = [row for row in header["source_rows"] if row["relativePath"] == "js/a.js"]
        descriptor, sha = RUNNER.write_import_batch(root, header, b"\x01", covered)
        descriptor_bytes = descriptor.read_bytes()
        artifact = RUNNER.hashlib.sha256(b"\x01").hexdigest()
        db.execute("INSERT INTO semantic_raw_batches VALUES(1,'scip-typescript','1','profile',?,?,?,?,?,?)",
                   (source_set, natives["r1"], header["manifest_hash"], sha, descriptor_bytes,
                    RUNNER.DECODER_VERSION))
        db.execute("INSERT INTO semantic_raw_captures VALUES(1,1,0,'javascript','scip-typescript',?,?,?,x'01',?)",
                   (artifact, source_set, natives["r1"], RUNNER.DECODER_VERSION))
        db.execute("INSERT INTO semantic_raw_exclusions VALUES(1,'javascript','js/b.js','notRequested')")
        db.execute("INSERT INTO semantic_raw_documents VALUES(1,'javascript','js/a.js',?,?,0)",
                   (hashes[paths[0]], HEX))
        db.execute("INSERT INTO semantic_raw_proofs VALUES(1,'proof','occurrence',?,'javascript','js/a.js',x'7b7d')",
                   (HEX,))
        for name in ("import", "r2"):
            db.execute("INSERT INTO semantic_revision_raw_selections VALUES(?,'scip-typescript',1,?)",
                       (pins[name], pins["r1"]))
        db.execute("INSERT INTO semantic_coverage VALUES(1,?,'scip-typescript','1','javascript','js/a.js',1,1,'complete',NULL)",
                   (pins["r2"],))
        db.execute("INSERT INTO semantic_basis VALUES(1,1,'scip-typescript','1',?,?,'javascript',?,?,?,?,?,?)",
                   (HEX, artifact, source_set, natives["r1"], header["manifest_hash"], HEX, HEX, HEX))
        db.execute("INSERT INTO semantic_provenance VALUES(1,'prov',?,'semanticReference','fresh',1,1,'proof','occurrence',?)",
                   (HEX, HEX))
        report = {"producerId": "scip-typescript", "identityKind": "executable",
                  "sourceSetId": source_set, "capturedRevision": natives["r1"],
                  "destinationRevision": natives["r2"], "descriptorSha256": HEX,
                  "artifacts": [], "languages": []}
        db.execute("INSERT INTO semantic_import_reports VALUES(?,?,?)",
                   (pins["import"], "scip-typescript", json.dumps(report)))
        return root, pins["r2"]

    def normalized_rows(self, workspace):
        db = self.schema_db()
        root, revision = self.populate(db, workspace)
        view = RUNNER.SelectedView(db, revision, root)
        rows = {}
        for table in ("semantic_raw_batches", "semantic_raw_captures", "semantic_raw_exclusions",
                      "semantic_raw_documents", "semantic_raw_proofs", "semantic_revision_raw_selections",
                      "semantic_coverage", "semantic_basis", "semantic_provenance",
                      "semantic_import_reports"):
            columns = [row[1] for row in db.execute(f'PRAGMA table_info("{table}")')]
            query = f'SELECT * FROM "{table}" selected WHERE {RUNNER.selected_where(table)}'
            rows[table] = [RUNNER.canonical(RUNNER.typed_cells(view.row(table, dict(zip(columns, row)))))
                           for row in db.execute(query, (revision,))]
            self.assertTrue(rows[table], table)
        return rows

    def test_selected_view_normalizes_a_schema_31_batch_selection_identically_across_roots(self):
        with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second:
            self.assertEqual(self.normalized_rows(first), self.normalized_rows(second))


class ImportBatch(unittest.TestCase):
    def captured(self, root):
        rows = [{"language": "javascript", "relativePath": "javascript/Csmall0010.js", "contentHash": HEX},
                {"language": "python", "relativePath": "python/Csmall0010.py", "contentHash": HEX}]
        for row in rows:
            path = root / row["relativePath"]
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"x = 1\n")
        return {"source_set": {"id": "set", "rootId": "root", "languages": ["javascript", "python"],
                               "dependencies": []},
                "native_id": "revision:v2:test", "source_rows": rows, "manifest_hash": HEX,
                "components": {"toolchainHash": HEX, "configHash": HEX, "dependencyHash": HEX}}, rows

    def test_write_import_batch_emits_one_artifact_reference_batch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            header, rows = self.captured(root)
            descriptor, sha = RUNNER.write_import_batch(root, header, b"scip-bytes", rows[:1])
            self.assertEqual(descriptor, root / ".trellis-semantic-batch/descriptor.json")
            data = json.loads(descriptor.read_bytes())
            self.assertEqual(RUNNER.hashlib.sha256(descriptor.read_bytes()).hexdigest(), sha)
            self.assertEqual(set(data), set(RUNNER.load_batch_module().DESCRIPTOR_KEYS))
            self.assertEqual(data["trustOrigin"], "referenceProfile")
            self.assertEqual(data["decoderVersion"], RUNNER.DECODER_VERSION)
            self.assertEqual(data["producer"]["name"], "scip-typescript")
            self.assertEqual(data["producer"]["identityKind"], "executable")
            self.assertEqual(len(data["artifacts"]), 1)
            self.assertEqual(sorted(p["kind"] for p in data["pages"]), ["coverage", "source"])
            self.assertEqual(data["totals"]["sourceDocuments"], 2)
            self.assertEqual(data["totals"]["coveredDocuments"], 1)
            self.assertEqual(data["totals"]["excludedDocuments"], 0)
            self.assertEqual(data["completion"], "complete")
            coverage = next(p for p in data["pages"] if p["kind"] == "coverage")
            self.assertEqual(coverage["rowCount"], 1)
            self.assertEqual((root / ".trellis-semantic-batch/artifact-0.scip").read_bytes(), b"scip-bytes")

    def test_run_semantic_import_uses_batch_flags(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            daemon = Mock(home=home)
            reply = json.dumps({"publishedRevision": {"indexGeneration": 1, "indexRevision": 2}})
            rows = iter([{}, {"req": ("done", 1, 2, 0, None)}])
            with patch.object(RUNNER, "command", return_value=reply) as command, \
                    patch.object(RUNNER, "request_rows", side_effect=lambda _home: next(rows)):
                RUNNER.run_semantic_import("/b/trellis", "/w", daemon, "/w/d.json", HEX, {"E": "1"})
        command.assert_called_once_with(
            ["/b/trellis", "semantic", "import", "--workspace", "/w", "--batch", "/w/d.json",
             "--batch-sha256", HEX], env={"E": "1"}, timed=False, rusage=None)

    def test_capture_header_without_row_bound_returns_every_source(self):
        db = sqlite3.connect(":memory:")
        self.addCleanup(db.close)
        db.executescript("""
            CREATE TABLE native_revisions(id,source_set_id,toolchain_hash,config_hash,dependency_hash,
                native_revision_id,source_inventory);
            CREATE TABLE native_source_sets(id,root_id);
            CREATE TABLE native_source_set_languages(source_set_id,language,ordinal);
            INSERT INTO native_source_sets VALUES('set','root');
            INSERT INTO native_source_set_languages VALUES('set','java',0);""")
        inventory = json.dumps([{"key": {"language": "java", "path": f"java/C{i}.java"},
                                 "contentHash": HEX} for i in range(6)])
        db.execute("INSERT INTO native_revisions VALUES('r','set','t','c','d','n',?)", (inventory,))
        self.assertEqual(len(RUNNER.capture_header(db, "r")["source_rows"]), 6)
        self.assertEqual(len(RUNNER.capture_header(db, "r", 6)["source_rows"]), 6)
        with self.assertRaises(RuntimeError):
            RUNNER.capture_r1(db, "r")


if __name__ == "__main__":
    unittest.main()
