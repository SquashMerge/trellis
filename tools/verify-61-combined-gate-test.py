#!/usr/bin/env python3
"""Check the #61 combined-gate runner without a Trellis binary, daemon or lock."""
import importlib.util
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import closing
from pathlib import Path
from unittest.mock import patch

TOOLS = Path(__file__).resolve().parent
REPO = TOOLS.parent
SPEC = importlib.util.spec_from_file_location("combined_gate", TOOLS / "verify-61-combined-gate.py")
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)
M = GATE.m


def load_json(relative):
    return json.loads((REPO / relative).read_text())


def run_smoke(campaign):
    result = subprocess.run([sys.executable, str(TOOLS / "verify-61-combined-gate.py"), "--smoke",
                             "--campaign-dir", str(campaign)], capture_output=True, text=True)
    assert result.returncode == 0, result.stdout + result.stderr
    return (campaign / "report.json").read_bytes()


class Smoke(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.first = run_smoke(Path(cls.temp.name) / "c1")
        cls.second = run_smoke(Path(cls.temp.name) / "c2")
        cls.report = json.loads(cls.first)

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def test_smoke_report_is_not_measured_with_pinned_fields(self):
        report = self.report
        self.assertEqual(report["status"], "notMeasured")
        self.assertIn("smoke", report["statusReasons"])
        self.assertIs(report["authoritativeAcceptance"], False)
        self.assertEqual(report["issue11"], "open")
        self.assertEqual(report["kind"], "trellis.combined-gate-report")
        campaign = report["campaign"]
        for name, digest in campaign["hashes"].items():
            self.assertRegex(digest, r"^[0-9a-f]{64}$", name)
        self.assertEqual(campaign["decoderVersion"], M.DECODER_VERSION)
        self.assertEqual(campaign["schemaVersion"], M.SCHEMA_VERSION)
        policy = load_json("docs/semantic-evidence/benchmark-policy-v1.json")
        host = campaign["host"]
        profile_id = policy["referenceHost"]["profileId"]
        profile_path = REPO / "docs/semantic-evidence/hardware" / f"{profile_id}.json"
        self.assertEqual(host["profileId"], profile_id)
        self.assertEqual(host["profilePath"], f"docs/semantic-evidence/hardware/{profile_id}.json")
        self.assertEqual(host["profileSha256"], GATE.sha256_bytes(profile_path.read_bytes()))
        self.assertEqual(host["modelIdentifier"], json.loads(profile_path.read_text())["observed"]["modelIdentifier"])
        self.assertIs(host["reference"], False)
        self.assertIsNone(host["inventory"])
        self.assertEqual([entry["size"] for entry in report["sizes"]], ["small", "medium", "large"])
        for entry in report["sizes"]:
            self.assertEqual([r["status"] for r in entry["repetitions"]], ["notAttempted"] * 3)
            for rep in entry["repetitions"]:
                self.assertIsNone(rep["startedAt"])
                self.assertIsNone(rep["finishedAt"])
            self.assertIsNone(entry["median"]["wallSeconds"])
            self.assertIsNone(entry["peakWorkerRssBytes"])
        self.assertTrue(all(v is None for v in report["preparation"]["seconds"].values()))
        self.assertEqual(report["statements"], GATE.STATEMENTS)

    def test_smoke_batch_respects_page_bounds(self):
        built = self.report["preparation"]["smokeBatch"]
        self.assertLessEqual(built["pageMaxRows"], 1000)
        self.assertLessEqual(built["pageMaxBytes"], 1 << 20)
        self.assertRegex(built["descriptorSha256"], r"^[0-9a-f]{64}$")

    def test_smoke_report_is_byte_identical_on_rerun(self):
        self.assertEqual(self.first, self.second)


class Fakes:
    """Stand-ins for the #90 command surface."""

    def __init__(self):
        self.calls = []
        self.clock = 100.0

    def command(self, argv, env=None, timed=False, rusage=None):
        self.calls.append(argv)
        duration = 2.0 if argv[1] == "index" else 3.0
        self.clock += duration + (0.5 if argv[1] != "index" else 0)
        rusage["maxRssBytes"] = 1000 + len(self.calls)
        return duration, f"{argv[1]}-out", "", self.clock, 1_700_000_000_000 + len(self.calls)


class Campaign(unittest.TestCase):
    def test_campaign_plan_is_three_fixed_repetitions_small_medium_large(self):
        plan = GATE.campaign_plan()
        self.assertEqual(plan, [(size, n) for size in ("small", "medium", "large") for n in (1, 2, 3)])

    def test_interval_runs_from_index_request_to_final_cli_return(self):
        fake = Fakes()
        usage = {"index": {}, "import": {}}
        interval, index_out, import_out = GATE.timed_interval(
            fake, Path("/bin/trellis"), Path("/w"), Path("/w/d.json"), "a" * 64, {}, usage)
        # index ran 2 s ending at 102; the import ended at 105.5: 105.5 - (102 - 2).
        self.assertAlmostEqual(interval["wallSeconds"], 5.5)
        self.assertEqual([c["argv"][1] for c in interval["commands"]], ["index", "semantic"])
        self.assertEqual((index_out, import_out), ("index-out", "semantic-out"))
        self.assertEqual(interval["commands"][1]["argv"][-2:], ["--batch-sha256", "a" * 64])
        self.assertEqual(usage["index"]["maxRssBytes"], 1001)

    def test_preparation_and_probe_are_recorded_outside_interval(self):
        self.assertIn('rep["probe"]', (TOOLS / "verify-61-combined-gate.py").read_text())
        self.assertEqual(GATE.INCLUSION["excluded"][:1], ["cohort generation"])
        self.assertIn("header probe", GATE.INCLUSION["excluded"])
        self.assertIn("batch preparation", GATE.INCLUSION["excluded"])
        fake = Fakes()
        interval, _, _ = GATE.timed_interval(fake, Path("/b"), Path("/w"), Path("/d"), "b" * 64, {},
                                             {"index": {}, "import": {}})
        self.assertEqual(len(interval["commands"]), 2)

    def completed(self, wall, rss=1, steady=1, peak=1):
        return {"status": "completed", "interval": {"wallSeconds": wall}, "rss": {"workerMaxRssBytes": rss},
                "storage": {"steady": {"total": steady}, "peak": {"total": peak},
                            "denominator": {"bytes": None}}}

    def qualified_report(self, sizes, reference=True):
        policy_read = GATE.read_policy()
        report = {"statusReasons": [], "smoke": False, "campaign": {"host": {"reference": reference}},
                  "sizes": sizes}
        return GATE.qualifier.qualify(report, policy_read[1], policy_read[2])

    def sizes(self, walls):
        return [{"size": size, "repetitions": [self.completed(w) for w in walls[size]]}
                for size in GATE.SIZES]

    def test_median_and_peak_use_all_raw_samples(self):
        sizes = self.sizes({"small": (1, 9, 1.5), "medium": (1, 2, 3), "large": (1, 2, 3)})
        sizes[0]["repetitions"][1]["rss"]["workerMaxRssBytes"] = 600 * 2**20
        result = self.qualified_report(sizes)
        small = result["sizes"][0]
        self.assertEqual(small["median"]["wallSeconds"], 1.5)
        self.assertEqual(small["evaluations"]["wall"]["status"], "passed")
        self.assertEqual(small["peakWorkerRssBytes"], 600 * 2**20)
        self.assertEqual(small["evaluations"]["rss"]["status"], "failed")
        self.assertEqual(result["status"], "failed")

    def test_worker_rss_comes_from_wait4_of_the_process(self):
        with tempfile.TemporaryDirectory() as temp:
            M.EVIDENCE = Path(temp)
            code = ("import sys,time\nb=bytearray(96<<20)\nfor i in range(0,len(b),4096): b[i]=1\n"
                    "print('ready',flush=True)\ntime.sleep(60)")
            daemon = M.Daemon(sys.executable, Path(temp) / "home", "rss", record_rusage=True)
            daemon.proc = subprocess.Popen([sys.executable, "-c", code], stdout=subprocess.PIPE, text=True)
            daemon.log = None
            self.assertEqual(daemon.proc.stdout.readline().strip(), "ready")
            with patch.object(M, "stop_home_processes"), patch.object(M, "election_free", return_value=True):
                daemon.stop()
            daemon.proc.stdout.close()
            self.assertGreaterEqual(daemon.rusage["maxRssBytes"], 96 * 2**20)

    def test_sqlite_peak_sampler_records_database_wal_shm_high_water(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "idx").mkdir()
            (root / "idx/semantic-stage-req1-inc1").mkdir()
            (root / "idx/stage-1").mkdir()
            (root / "idx/index.db").write_bytes(b"x" * 100)
            (root / "idx/index.db-wal").write_bytes(b"x" * 50)
            (root / "idx/index.db-shm").write_bytes(b"x" * 10)
            (root / "idx/semantic-stage-req1-inc1/rows").write_bytes(b"x" * 70)
            (root / "idx/stage-1/rows").write_bytes(b"x" * 888)
            (root / "idx/other").write_bytes(b"x" * 999)
            sampler = GATE.StorageSampler(root, interval=0.01).start()
            (root / "idx/index.db-wal").write_bytes(b"x" * 500)
            time.sleep(0.05)
            (root / "idx/index.db-wal").write_bytes(b"x")
            peak = sampler.stop()
            self.assertEqual(peak["total"], 610)
            self.assertEqual(sampler.stage_peak, 70)
            self.assertEqual(peak["method"], "poll 50 ms plus final stat")

    def test_sqlite_steady_reports_sizes_before_runner_opens_database(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "index.db"
            db = sqlite3.connect(path)
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("CREATE TABLE t(x)")
            db.commit()
            db.close()
            for suffix in ("-wal", "-shm"):
                Path(str(path) + suffix).unlink(missing_ok=True)
            steady, _ = GATE.sqlite_steady(path)
            self.assertEqual(steady["wal"], 0)
            self.assertEqual(steady["shm"], 0)
            self.assertEqual(steady["total"], steady["db"])

    def test_storage_ratio_is_not_measured_until_denominator_ratified(self):
        block = GATE.storage_block({"total": 10}, {"total": 15}, 0, {})
        self.assertIsNone(block["ratios"]["steady"])
        self.assertEqual(block["denominator"]["status"], "notMeasured")
        self.assertIsNone(block["denominator"]["bytes"])
        self.assertEqual(block["ratios"]["peak"]["observed"], 1.5)
        result = self.qualified_report(self.sizes({s: (1, 1, 1) for s in GATE.SIZES}))
        medium = result["sizes"][1]["evaluations"]
        self.assertEqual(medium["steadyRatio"]["status"], "notMeasured")
        self.assertEqual(medium["peakRatio"]["status"], "notMeasured")
        self.assertEqual(medium["steadyAbsolute"]["status"], "passed")
        self.assertFalse(result["qualification"]["denominatorRatified"])

    def test_statements_define_cold_and_synthetic_scope(self):
        self.assertIn("virgin Trellis state", GATE.STATEMENTS["cold"])
        self.assertIn("OS and SSD caches are not purged", GATE.STATEMENTS["cold"])
        self.assertIs(GATE.STATEMENTS["producerGenerationIncluded"], False)
        self.assertIn("no real-producer qualification", GATE.STATEMENTS["artifacts"])

    def test_off_reference_host_is_not_measured(self):
        profile = {"modelIdentifier": "Mac17,16"}
        self.assertTrue(GATE.is_reference(profile, {"hwModel": "Mac17,16"}, system="Darwin"))
        self.assertFalse(GATE.is_reference(profile, {"hwModel": "Mac17,16"}, system="Linux"))
        self.assertFalse(GATE.is_reference(profile, {"hwModel": "Other"}, system="Darwin"))
        result = self.qualified_report(self.sizes({s: (1, 1, 1) for s in GATE.SIZES}), reference=False)
        self.assertEqual(result["status"], "notMeasured")
        self.assertEqual(result["sizes"][0]["evaluations"]["wall"]["status"], "notMeasured")

    def execute(self, behavior):
        ran = []

        def runner(size, ordinal, rep):
            ran.append((size, ordinal))
            rep["probe"] = {"seconds": 1}
            behavior(size, ordinal, rep)

        sizes, faulted = GATE.execute_plan(runner, False, clock=lambda: "t")
        return sizes, faulted, ran

    def test_fault_stops_campaign_unqualified_and_keeps_attempts(self):
        def behavior(size, ordinal, rep):
            if (size, ordinal) == ("medium", 2):
                raise GATE.GateFault("native-only result")
            rep.update(self.completed(1))
        sizes, faulted, ran = self.execute(behavior)
        self.assertEqual(ran, [("small", 1), ("small", 2), ("small", 3), ("medium", 1), ("medium", 2)])
        self.assertIn("medium repetition 2", faulted)
        statuses = [[r["status"] for r in s["repetitions"]] for s in sizes]
        self.assertEqual(statuses, [["completed"] * 3, ["completed", "fault", "notAttempted"],
                                    ["notAttempted"] * 3])
        self.assertEqual(sizes[1]["repetitions"][1]["probe"], {"seconds": 1})
        report = {"statusReasons": [], "smoke": False, "campaign": {"host": {"reference": True}},
                  "sizes": sizes}
        policy = GATE.read_policy()
        result = GATE.qualifier.qualify(report, policy[1], policy[2])
        self.assertEqual(result["status"], "unqualified")
        self.assertEqual(result["issue11"], "open")

    def test_threshold_failure_is_failed_and_leaves_issue_11_open(self):
        def behavior(size, ordinal, rep):
            rep.update(self.completed(1000 if size == "large" else 1))
        sizes, faulted, ran = self.execute(behavior)
        self.assertIsNone(faulted)
        self.assertEqual(len(ran), 9)
        result = self.qualified_report(sizes)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["issue11"], "open")
        self.assertIs(result["authoritativeAcceptance"], False)

    def test_thresholds_are_read_from_ratified_policy_files(self):
        read, policy, cohorts, profile_id = GATE.read_policy()
        full = policy["performance"]["fullIndex"]
        self.assertEqual(profile_id, policy["referenceHost"]["profileId"])
        self.assertEqual(read["fullIndex"]["sizes"]["small"]["rssBytes"], full["small"]["rssMiB"] * 2**20)
        self.assertEqual(read["fullIndex"]["sizes"]["large"]["rssBytes"], full["large"]["rssGiB"] * 2**30)
        for size in GATE.SIZES:
            self.assertEqual(read["fullIndex"]["sizes"][size]["medianSeconds"], full[size]["medianSeconds"])
            self.assertEqual(read["cohorts"][size]["minimumNormalizedFacts"],
                             cohorts["synthetic"][size]["minimumNormalizedFacts"])
            self.assertEqual(read["cohorts"][size]["factsScope"], cohorts["synthetic"][size]["factsScope"])
        for key, value in read["storage"].items():
            self.assertEqual(value, policy["performance"]["storage"][key])
        self.assertEqual(read["fullIndex"]["includedStages"], full["includedStages"])

    def test_policy_with_both_rss_keys_is_a_preflight_failure(self):
        policy = load_json("docs/semantic-evidence/benchmark-policy-v1.json")
        policy["performance"]["fullIndex"]["small"]["rssGiB"] = 1
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "policy.json"
            path.write_text(json.dumps(policy))
            with patch.object(GATE, "POLICY_PATH", path):
                with self.assertRaisesRegex(GATE.GateFault, "^preflight:"):
                    GATE.read_policy()

    def test_native_floor_uses_revision_documents_and_small_floor(self):
        db = sqlite3.connect(":memory:")
        db.executescript("""
            CREATE TABLE revision_documents(revision_id TEXT, language TEXT, vid INTEGER);
            CREATE TABLE native_version_declarations(vid INTEGER);
            CREATE TABLE native_version_calls(vid INTEGER);
            CREATE TABLE native_version_control_regions(vid INTEGER);""")
        for vid, language in enumerate(GATE.LANGUAGES):
            db.execute("INSERT INTO revision_documents VALUES('r',?,?)", (language, vid))
            db.executemany("INSERT INTO native_version_declarations VALUES(?)", [(vid,)] * 3000)
            db.executemany("INSERT INTO native_version_calls VALUES(?)", [(vid,)] * 2000)
            db.executemany("INSERT INTO native_version_control_regions VALUES(?)", [(vid,)] * 5)
        floor = {"factsScope": "perLanguage", "minimumNormalizedFacts": 5000}
        facts = GATE.native_facts(db, "r", floor)
        self.assertEqual(facts["perLanguage"]["java"], 5005)
        self.assertEqual(facts["overall"], 4 * 5005)
        with self.assertRaises(GATE.GateFault):
            GATE.native_facts(db, "r", {**floor, "minimumNormalizedFacts": 5006})
        GATE.native_facts(db, "r", {"factsScope": "overall", "minimumNormalizedFacts": 20020})
        db.close()

    def test_adapter_reuses_90_lifecycle_objects(self):
        adapter = GATE.Gate90Adapter(Path("/bin/trellis"))
        self.assertIs(adapter.Daemon, M.Daemon)
        for name in ("command", "env_for", "db_for", "canonical", "sha_file", "copy_cohort",
                     "capture_header", "run_semantic_import"):
            self.assertIs(getattr(adapter, name), getattr(M, name), name)
        self.assertIs(adapter.FACTS, M.FACTS)
        with tempfile.TemporaryDirectory() as temp:
            adapter.begin_repetition(Path(temp) / "rep1")
            self.assertEqual(M.EVIDENCE, Path(temp) / "rep1")
            self.assertEqual(M.BINARY, Path("/bin/trellis"))
            self.assertGreater(M.DEADLINE, time.monotonic() + 1600)
            daemon = adapter.daemon(Path(temp) / "home", "timed", record_rusage=True)
            self.assertTrue(daemon.record_rusage)

    def test_campaign_dir_rules(self):
        with self.assertRaises(SystemExit):
            GATE.check_campaign_dir(Path("relative"), False)
        with self.assertRaises(SystemExit):
            GATE.check_campaign_dir(Path("/private/tmp/x-campaign"), False)
        with tempfile.TemporaryDirectory() as temp:
            GATE.check_campaign_dir(Path(temp) / "new", True)
            with self.assertRaises(SystemExit):
                GATE.check_campaign_dir(Path(temp), True)

    def test_dirty_tree_is_unqualified_with_zero_repetitions(self):
        def fake_capture(argv, cwd=GATE.REPO):
            if argv[:2] == ["git", "status"]:
                return " M file"
            return "x"
        args = GATE.parse_args(["--campaign-dir", "/nowhere", "--binary", sys.executable])
        _campaign, _read, _p, _c, failures = GATE.preflight(args, Path("/nowhere"), False, fake_capture)
        self.assertIn("preflight: working tree is not clean", failures)

    def test_runner_stop_reasons_are_prefixed_preflight(self):
        with tempfile.TemporaryDirectory() as temp, patch.object(GATE, "PREPARATION_PATH", Path(temp) / "p.json"):
            record = json.loads((REPO / "docs/semantic-evidence/performance-gate-preparation.json").read_text())
            for entry in record["sizes"]["small"]["artifacts"]:
                entry["sha256"] = "0" * 64
            GATE.PREPARATION_PATH.write_text(json.dumps(record))
            with self.assertRaisesRegex(GATE.GateFault, "^preflight: small artifact hashes differ"):
                GATE.prepare(Path(temp) / "c", ("small",), True)

class FreshState(unittest.TestCase):
    """AC11: every repetition gets its own HOME/XDG tree, daemon and workspace, removed afterwards."""

    def run_rep(self, campaign, ordinal, homes):
        class FakeDaemon:
            def __init__(self, home, label):
                self.home, self.label = home, label
                self.path = home.parent / f"{label}-daemon.log"
                self.rusage = {"maxRssBytes": 7}
                self.started = 0

            def start(self):
                self.started += 1
                homes.append((self.label, self.home, list(self.home.iterdir())))

            def stop(self):
                self.path.write_text("log")

        class FakeAdapter:
            def begin_repetition(self, rep_dir):
                rep_dir.mkdir(parents=True, exist_ok=True)

            def copy_cohort(self, source, destination):
                destination.mkdir(parents=True)

            def daemon(self, home, label, record_rusage=False):
                return FakeDaemon(home, label)

            command = staticmethod(lambda argv, env=None, **k: "out")
            env_for = staticmethod(M.env_for)
            db_for = staticmethod(lambda home: home / "index.db")
            capture_header = staticmethod(lambda db, revision: {
                "native_id": "n", "source_set": {"id": "s", "rootId": "r"}, "manifest_hash": "h",
                "source_rows": []})
            sha_file = staticmethod(lambda path: "0" * 64)

        ctx = {"adapter": FakeAdapter(), "binary": Path("/bin/trellis"), "campaign_dir": campaign,
               "cohort": campaign / "cohort", "synthetic_profile": {},
               "prepared": {"small": {"inventory": [], "dir": campaign}},
               "policy": {"cohorts": {"small": {}}}}
        built = {"descriptorPath": campaign / "d.json", "descriptorSha256": "a" * 64, "pageCount": 1,
                 "pageBytes": 1, "sourceBytes": 1, "artifactBytes": 1, "descriptorBytes": 1}

        class Sampler:
            stage_peak = 0
            _thread = type("T", (), {"is_alive": staticmethod(lambda: False)})()

            def __init__(self, root):
                pass

            def start(self):
                pass

            def stop(self):
                return {}

        rep = {"ordinal": ordinal}
        with patch.object(GATE, "pin_revision", lambda out: ("pin", "rev")), \
                patch.object(GATE, "build_batch", lambda *a, **k: built), \
                patch.object(GATE, "StorageSampler", Sampler), \
                patch.object(GATE, "timed_interval", lambda *a: ({"wallSeconds": 1}, "i", "m")), \
                patch.object(GATE, "validate_run", lambda *a: {
                    "pins": {}, "requests": {}, "nativeFacts": {}, "coverage": {}}), \
                patch.object(GATE, "sqlite_steady", lambda path: ({}, None)), \
                patch.object(GATE, "storage_block", lambda *a: {}), \
                patch.object(GATE.sqlite3, "connect", lambda *a, real=sqlite3.connect, **k: real(":memory:")):
            GATE.run_repetition(ctx, "small", ordinal, rep)
        return rep

    def test_each_repetition_starts_a_fresh_daemon_in_its_own_empty_home(self):
        with tempfile.TemporaryDirectory() as temp:
            campaign = Path(temp)
            homes = []
            reps = [self.run_rep(campaign, ordinal, homes) for ordinal in (1, 2, 3)]
            # probe and timed daemons per repetition, each started on a home that held nothing.
            self.assertEqual(len(homes), 6)
            self.assertEqual(len({home for _label, home, _ in homes}), 6)
            for label, home, content in homes:
                self.assertEqual(content, [], f"{label} home {home} was not virgin")
            for ordinal, rep in enumerate(reps, 1):
                self.assertEqual(rep["status"], "completed")
                self.assertTrue(all(home.parent.name == f"rep{ordinal}" for _l, home, _c in homes[(ordinal - 1) * 2:ordinal * 2]))
                self.assertEqual([c["outcome"] for c in rep["cleanup"]], ["removed"] * 3)
                self.assertEqual([c["path"] for c in rep["cleanup"]], ["probe-home", "home", "workspace"])
                self.assertFalse((campaign / "small" / f"rep{ordinal}" / "home").exists())

    def test_isolated_environment_points_home_and_xdg_at_the_repetition_home(self):
        home = Path("/tmp/rep-home")
        env = GATE.Gate90Adapter.env_for(home)
        self.assertEqual(env["HOME"], str(home))
        self.assertEqual(env["XDG_CACHE_HOME"], str(home / ".cache"))
        self.assertEqual(env["XDG_DATA_HOME"], str(home / ".local/share"))


if __name__ == "__main__":
    unittest.main()
