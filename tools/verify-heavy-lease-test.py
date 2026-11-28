#!/usr/bin/env python3
"""tools/verify takes the shared heavy-run lease when MIMIR_HEAVY_LOCK_FILE is set.

Each test runs the real tools/verify and the real lease scripts against a lock in
a temporary directory. Nothing is built: PATH holds only stubs for the tools the
lanes call (plus /usr/bin and /bin), and each stub records how it was called.

The lease has two slots (<lock>.lease and <lock>.lease2). Verify asks for the
medium tier, which holds one slot, unless the caller set MIMIR_HEAVY_TIER."""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LANE = "node tools/semantic-contract/test/run.mjs"  # what `--lane semantic` runs
BACKSTOP = 120  # seconds; a hang backstop, not a speed bound

# One stub stands in for every tool. It appends one record per call and, for the
# semantic lane's command, can hold until the test releases it and exit as told.
STUB = r"""#!/bin/sh
name=$(basename "$0")
lease=absent
[ -d "${MIMIR_HEAVY_LOCK_FILE:-/nonexistent}.lease" ] && lease=present
printf '%s %s|held=%s|lease=%s|mark=%s\n' "$name" "$*" "${MIMIR_HEAVY_LOCK_HELD:-}" \
  "$lease" "${HEAVY_TEST_MARK:-}" >>"$HEAVY_TEST_DIR/calls"
case "$name $*" in
  "node tools/verify-env.mjs") echo /nonexistent-browsers ;;
  "node tools/semantic-contract/test/run.mjs")
    echo "start ${HEAVY_TEST_ID:-}" >>"$HEAVY_TEST_DIR/events"
    if [ -n "${HEAVY_TEST_HOLD:-}" ]; then
      while [ ! -e "$HEAVY_TEST_DIR/release" ]; do sleep 0.05; done
    fi
    echo "end ${HEAVY_TEST_ID:-}" >>"$HEAVY_TEST_DIR/events"
    exit "${HEAVY_TEST_EXIT:-0}" ;;
esac
"""
# Stands in for the interpreter (MIMIR_HEAVY_PYTHON) of one run. It loads the
# unmodified lease script and wraps its _publish_generation, the atomic mkdir of
# a lease directory: when that finds slot 1 already held, it leaves a marker and
# re-raises. Everything else is the real code. The marker is the signal that a
# run has reached the lease and been turned away.
LEASE_OBSERVER = r"""import importlib.util, os, sys
spec = importlib.util.spec_from_file_location("heavy_run", sys.argv[1])
heavy_run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(heavy_run)
lease = os.environ["MIMIR_HEAVY_LOCK_FILE"] + ".lease"
publish = heavy_run._publish_generation
def observed(path):
    try:
        publish(path)
    except FileExistsError:
        if str(path) == lease:
            open(os.path.join(os.environ["HEAVY_TEST_DIR"], "contended"), "a").close()
        raise
heavy_run._publish_generation = observed
sys.exit(heavy_run.run(sys.argv[2:]))
"""
# A command run by heavy_run.py: it records the lock directory, the queue's tickets
# and the held marker it was given, then exits 5.
SNAPSHOT = r"""import json, os, sys
locks, out = sys.argv[1:3]
queue = os.path.join(locks, "heavy.lock.queue")
tickets = sorted(n for n in os.listdir(queue) if n.isdigit()) if os.path.isdir(queue) else []
with open(out, "w") as snapshot:
    json.dump({"locks": sorted(os.listdir(locks)), "tickets": tickets,
               "held": os.environ.get("MIMIR_HEAVY_LOCK_HELD")}, snapshot)
sys.exit(5)
"""
# Used only where the machine has neither gtimeout nor timeout.
TIMEOUT_STUB = '#!/bin/sh\nshift 3\nexec "$@"\n'


class VerifyHeavyLease(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.dir = Path(temp.name).resolve()
        self.bin = self.dir / "bin"
        self.bin.mkdir()
        for name in ("rustc", "node", "cargo", "python3"):
            self.stub(name, STUB)
        (self.dir / "locks").mkdir()
        self.lock = self.dir / "locks" / "heavy.lock"
        self.lease = Path(str(self.lock) + ".lease")
        self.lease2 = Path(str(self.lock) + ".lease2")

    def stub(self, name, text):
        path = self.bin / name
        path.write_text(text)
        path.chmod(0o755)
        return path

    def env(self, **extra):
        """The environment of a run with no lease configured."""
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("MIMIR_HEAVY_", "HEAVY_TEST_"))}
        return {**env, "PATH": f"{self.bin}:/usr/bin:/bin", "TMPDIR": str(self.dir),
                "HEAVY_TEST_DIR": str(self.dir), **extra}

    def leased(self, **extra):
        """The environment of a run with the lease configured and not yet held. The
        stub python3 must not run the lease, so the lease is given this interpreter."""
        tool = (shutil.which("gtimeout") or shutil.which("timeout")
                or str(self.stub("timeout-stub", TIMEOUT_STUB)))
        return self.env(**{"MIMIR_HEAVY_LOCK_FILE": str(self.lock),
                           "MIMIR_HEAVY_PYTHON": sys.executable,
                           "MIMIR_HEAVY_TIMEOUT_TOOL": tool, **extra})

    def start(self, env, *args, command=("./tools/verify",)):
        """Start a run with its output in files, and make sure it is gone before
        the temporary directory is (cleanups run last-registered first, and the
        directory's was registered in setUp)."""
        self.runs = getattr(self, "runs", 0) + 1
        out, err = self.dir / f"run-{self.runs}.out", self.dir / f"run-{self.runs}.err"
        with open(out, "w") as stdout, open(err, "w") as stderr:
            process = subprocess.Popen([*command, *args], cwd=ROOT, env=env,
                                       stdout=stdout, stderr=stderr)
        process.out, process.err = out, err
        self.addCleanup(self.stop, process)
        return process

    def stop(self, process):
        """Reap a run on every exit path, including a failed assertion while a lane
        is held. Release the hold first so the run can end by itself: killing only
        the process we started would leave a held lane waiting for a release file
        in a directory that is about to be removed."""
        (self.dir / "release").touch()
        try:
            process.wait(timeout=BACKSTOP)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    def finish(self, process):
        """Wait for a run and return it with its output read."""
        process.wait(timeout=BACKSTOP)
        process.stdout, process.stderr = process.out.read_text(), process.err.read_text()
        return process

    def verify(self, env, *args, command=("./tools/verify",)):
        return self.finish(self.start(env, *args, command=command))

    def calls(self):
        path = self.dir / "calls"
        return path.read_text().splitlines() if path.exists() else []

    def events(self):
        path = self.dir / "events"
        return path.read_text().splitlines() if path.exists() else []

    def holders(self):
        """The tier each held slot's owner recorded, by slot."""
        found = {}
        for slot in (self.lease, self.lease2):
            owner = slot / "owner"
            if owner.exists():
                found[slot.name.rsplit(".", 1)[1]] = json.loads(owner.read_text())["tier"]
        return found

    def assert_released(self):
        self.assertFalse(self.lease.exists())
        self.assertFalse(self.lease2.exists())

    def lane_call(self):
        found = [call for call in self.calls() if call.startswith(LANE + "|")]
        self.assertEqual(len(found), 1, self.calls())
        return found[0]

    def test_unset_or_empty_runs_directly_without_python(self):
        for env in (self.env(), self.env(MIMIR_HEAVY_LOCK_FILE="")):
            with self.subTest(env.get("MIMIR_HEAVY_LOCK_FILE")):
                (self.dir / "calls").unlink(missing_ok=True)
                done = self.verify(env, "--lane", "semantic")
                self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
                self.assertEqual(self.lane_call(), LANE + "|held=|lease=absent|mark=")
                self.assertFalse([c for c in self.calls() if c.startswith("python3")])
                self.assertEqual(list((self.dir / "locks").iterdir()), [])

    def test_a_named_lane_runs_under_the_lease_and_releases_it(self):
        done = self.verify(self.leased(HEAVY_TEST_MARK="two words"), "--lane=semantic",
                           command=(str(ROOT / "tools" / "verify"),))
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(self.lane_call(), LANE + "|held=1|lease=present|mark=two words")
        self.assertIn("==> verify lane semantic (exit 0)", done.stdout)
        self.assert_released()

    def test_the_default_lanes_run_under_the_lease(self):
        # The stubs make some lanes fail (nothing is built); only where they ran matters.
        done = self.verify(self.leased())
        for lane in ("rust", "tests", "semantic", "cohorts", "checks"):
            self.assertIn(f"==> verify lane {lane} (exit ", done.stdout, done.stderr)
        calls = self.calls()
        self.assertIn(LANE + "|held=1|lease=present|mark=", calls)
        self.assertEqual([c for c in calls if "|held=1|lease=present|" not in c], [])
        self.assert_released()

    def test_exit_statuses_pass_through_the_lease(self):
        failed = self.verify(self.leased(HEAVY_TEST_EXIT="7"), "--lane", "semantic")
        self.assertEqual(failed.returncode, 7, failed.stdout + failed.stderr)
        self.assertIn("verify: failed lane(s): semantic", failed.stderr)
        usage = self.verify(self.leased(), "--lane", "nonesuch")
        self.assertEqual(usage.returncode, 2, usage.stdout + usage.stderr)
        self.assertIn("verify: unknown lane: nonesuch", usage.stderr)
        self.assert_released()

    def test_inside_an_outer_lease_verify_runs_directly(self):
        # As the host wrapper does: the lease scripts run verify, which must not wait
        # for the lease its own parent holds.
        done = self.verify(self.leased(HEAVY_TEST_EXIT="3"), "./tools/verify", "--lane",
                           "semantic", command=("sh", "tools/heavy_run.sh"))
        self.assertEqual(done.returncode, 3, done.stdout + done.stderr)
        self.assertEqual(self.lane_call(), LANE + "|held=1|lease=present|mark=")

    def test_inside_an_outer_medium_lease_verify_takes_no_second_slot(self):
        # As the medium host wrapper does: a nested verify that took the lease again
        # would get slot 2 beside its own parent instead of waiting.
        run = self.start(self.leased(MIMIR_HEAVY_TIER="medium", HEAVY_TEST_ID="nested",
                                     HEAVY_TEST_HOLD="1"),
                         "./tools/verify", "--lane", "semantic",
                         command=("sh", "tools/heavy_run.sh"))
        self.wait_for(lambda: self.events() == ["start nested"], run)
        self.assertEqual(self.holders(), {"lease": "medium"})
        self.finish_all(run)
        self.assertEqual(self.lane_call(), LANE + "|held=1|lease=present|mark=")

    def test_a_held_marker_alone_skips_the_lease(self):
        # No interpreter or timeout tool is offered: taking the lease here would fail.
        done = self.verify(self.env(MIMIR_HEAVY_LOCK_FILE=str(self.lock),
                                    MIMIR_HEAVY_LOCK_HELD="1"), "--lane", "semantic")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(self.lane_call(), LANE + "|held=1|lease=absent|mark=")
        self.assertFalse([c for c in self.calls() if c.startswith("python3")])

    def test_heavy_run_py_under_a_held_lease_runs_its_command_directly(self):
        # A caller already inside the lease runs heavy_run.py itself. At the medium
        # tier it must not take slot 2 beside its own parent; at the large tier it
        # must not wait for the slot its parent holds.
        outer = self.held_run("outer")
        self.wait_for(lambda: self.events() == ["start outer"], outer)
        snapshot = self.dir / "snapshot.json"
        for tier in ("medium", "large"):
            with self.subTest(tier):
                done = subprocess.run(
                    [sys.executable, str(ROOT / "tools" / "heavy_run.py"),
                     sys.executable, "-c", SNAPSHOT, str(self.dir / "locks"), str(snapshot)],
                    cwd=ROOT, env=self.leased(MIMIR_HEAVY_LOCK_HELD="1",
                                              MIMIR_HEAVY_TIER=tier),
                    capture_output=True, text=True, timeout=BACKSTOP)
                self.assertEqual(done.returncode, 5, done.stdout + done.stderr)
                seen = json.loads(snapshot.read_text())
                self.assertEqual(seen["held"], "1")
                self.assertEqual(seen["locks"], ["heavy.lock.lease", "heavy.lock.queue"])
                self.assertEqual(seen["tickets"], [])
                self.assertEqual(self.holders(), {"lease": "medium"})
        self.finish_all(outer)

    def test_an_unusable_lock_path_fails_without_running_verify(self):
        env = self.leased(MIMIR_HEAVY_LOCK_FILE=str(self.dir / "missing" / "heavy.lock"))
        done = self.verify(env, "--lane", "semantic")
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("heavy_run: cannot use configured lease", done.stderr)
        self.assertEqual(self.calls(), [])

    def held_run(self, run, **extra):
        """Start a run whose semantic lane pauses until the test releases it."""
        return self.start(self.leased(HEAVY_TEST_ID=run, HEAVY_TEST_HOLD="1", **extra),
                          "--lane", "semantic")

    def finish_all(self, *processes):
        (self.dir / "release").touch()
        for process in processes:
            done = self.finish(process)
            self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assert_released()

    def test_verify_takes_one_slot_at_the_medium_tier(self):
        run = self.held_run("only")
        self.wait_for(lambda: self.events() == ["start only"], run)
        self.assertEqual(self.holders(), {"lease": "medium"})
        self.finish_all(run)

    def test_the_environment_can_ask_for_the_large_tier(self):
        run = self.held_run("only", MIMIR_HEAVY_TIER="large")
        self.wait_for(lambda: self.events() == ["start only"], run)
        self.assertEqual(self.holders(), {"lease": "large", "lease2": "large"})
        self.finish_all(run)

    def test_two_runs_on_one_lock_share_it_at_the_medium_tier(self):
        first = self.held_run("first")
        self.wait_for(lambda: self.events() == ["start first"], first)
        # The first run is paused inside its lane on slot 1. The second starts
        # beside it on slot 2 instead of waiting for the release.
        second = self.held_run("second")
        self.wait_for(lambda: self.events() == ["start first", "start second"], second)
        self.assertEqual(self.holders(), {"lease": "medium", "lease2": "medium"})
        self.finish_all(first, second)

    def test_a_large_run_waits_for_a_medium_one(self):
        observer = self.dir / "lease-observer.py"
        observer.write_text(LEASE_OBSERVER)
        observing = self.stub(
            "observing-python", f'#!/bin/sh\nexec "{sys.executable}" "{observer}" "$@"\n')

        first = self.held_run("first")
        self.wait_for(lambda: self.events() == ["start first"], first)
        # The first run is now paused inside its lane, holding slot 1. Wait for the
        # large second run to try slot 1 and find it taken. No sleep: without the
        # lease the second never signals, runs to its end, and wait_for fails.
        second = self.start(self.leased(HEAVY_TEST_ID="second", MIMIR_HEAVY_TIER="large",
                                        MIMIR_HEAVY_PYTHON=str(observing)),
                            "--lane", "semantic")
        self.wait_for((self.dir / "contended").exists, second)
        self.assertEqual(self.events(), ["start first"])
        self.finish_all(first, second)
        self.assertEqual(self.events(),
                         ["start first", "end first", "start second", "end second"])

    def wait_for(self, reached, process):
        deadline = time.monotonic() + BACKSTOP
        while not reached():
            self.assertIsNone(process.poll(), "the run ended before the awaited signal")
            self.assertLess(time.monotonic(), deadline, "hang backstop")
            time.sleep(0.05)


if __name__ == "__main__":
    unittest.main()
