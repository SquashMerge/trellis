#!/usr/bin/env python3
"""tools/verify takes the shared heavy-run lease when MIMIR_HEAVY_LOCK_FILE is set.

Each test runs the real tools/verify and the real lease scripts against a lock in
a temporary directory. Nothing is built: PATH holds only stubs for the tools the
lanes call (plus /usr/bin and /bin), and each stub records how it was called."""
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
# the lease: when that finds the lease already held, it leaves a marker and
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
        self.assertFalse(self.lease.exists())

    def test_the_default_lanes_run_under_the_lease(self):
        # The stubs make some lanes fail (nothing is built); only where they ran matters.
        done = self.verify(self.leased())
        for lane in ("rust", "tests", "semantic", "cohorts", "checks"):
            self.assertIn(f"==> verify lane {lane} (exit ", done.stdout, done.stderr)
        calls = self.calls()
        self.assertIn(LANE + "|held=1|lease=present|mark=", calls)
        self.assertEqual([c for c in calls if "|held=1|lease=present|" not in c], [])
        self.assertFalse(self.lease.exists())

    def test_exit_statuses_pass_through_the_lease(self):
        failed = self.verify(self.leased(HEAVY_TEST_EXIT="7"), "--lane", "semantic")
        self.assertEqual(failed.returncode, 7, failed.stdout + failed.stderr)
        self.assertIn("verify: failed lane(s): semantic", failed.stderr)
        usage = self.verify(self.leased(), "--lane", "nonesuch")
        self.assertEqual(usage.returncode, 2, usage.stdout + usage.stderr)
        self.assertIn("verify: unknown lane: nonesuch", usage.stderr)
        self.assertFalse(self.lease.exists())

    def test_inside_an_outer_lease_verify_runs_directly(self):
        # As the host wrapper does: the lease scripts run verify, which must not wait
        # for the lease its own parent holds.
        done = self.verify(self.leased(HEAVY_TEST_EXIT="3"), "./tools/verify", "--lane",
                           "semantic", command=("sh", "tools/heavy_run.sh"))
        self.assertEqual(done.returncode, 3, done.stdout + done.stderr)
        self.assertEqual(self.lane_call(), LANE + "|held=1|lease=present|mark=")

    def test_a_held_marker_alone_skips_the_lease(self):
        # No interpreter or timeout tool is offered: taking the lease here would fail.
        done = self.verify(self.env(MIMIR_HEAVY_LOCK_FILE=str(self.lock),
                                    MIMIR_HEAVY_LOCK_HELD="1"), "--lane", "semantic")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(self.lane_call(), LANE + "|held=1|lease=absent|mark=")
        self.assertFalse([c for c in self.calls() if c.startswith("python3")])

    def test_an_unusable_lock_path_fails_without_running_verify(self):
        env = self.leased(MIMIR_HEAVY_LOCK_FILE=str(self.dir / "missing" / "heavy.lock"))
        done = self.verify(env, "--lane", "semantic")
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("heavy_run: cannot use configured lease", done.stderr)
        self.assertEqual(self.calls(), [])

    def test_two_runs_on_one_lock_do_not_overlap(self):
        def start(run, **extra):
            return self.start(self.leased(HEAVY_TEST_ID=run, **extra), "--lane", "semantic")

        observer = self.dir / "lease-observer.py"
        observer.write_text(LEASE_OBSERVER)
        observing = self.stub(
            "observing-python", f'#!/bin/sh\nexec "{sys.executable}" "{observer}" "$@"\n')

        first = start("first", HEAVY_TEST_HOLD="1")
        self.wait_for(lambda: self.events() == ["start first"], first)
        # The first run is now paused inside its lane, holding the lease. Wait for
        # the second to try the lease and find it taken. No sleep: without the
        # lease the second never signals, runs to its end, and wait_for fails.
        second = start("second", MIMIR_HEAVY_PYTHON=str(observing))
        self.wait_for((self.dir / "contended").exists, second)
        self.assertEqual(self.events(), ["start first"])
        (self.dir / "release").touch()
        for process in (first, second):
            done = self.finish(process)
            self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(self.events(),
                         ["start first", "end first", "start second", "end second"])
        self.assertFalse(self.lease.exists())

    def wait_for(self, reached, process):
        deadline = time.monotonic() + BACKSTOP
        while not reached():
            self.assertIsNone(process.poll(), "the run ended before the awaited signal")
            self.assertLess(time.monotonic(), deadline, "hang backstop")
            time.sleep(0.05)


if __name__ == "__main__":
    unittest.main()
