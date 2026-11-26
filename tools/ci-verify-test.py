#!/usr/bin/env python3
"""Offline checks of tools/ci-verify.sh, the factory's repository verify on CI.

Each test runs the real script with fake `git` and `gh` first on PATH in a
temporary directory: nothing reaches the network and nothing is pushed. The
fake gh answers each call with the next scripted response (the last one
repeats) and applies the script's own --jq filter with the real jq. The first
gh call is the script's look at the commit's runs before it pushes."""
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "tools" / "ci-verify.sh"
SHA = "0123456789abcdef0123456789abcdef01234567"
REF = f"refs/heads/qualify/factory-{SHA}"
REQUIRED = [f"{job} ({os_})" for os_ in ("ubuntu-latest", "macos-latest")
            for job in ("browser", "tests", "checks")]
JQ = shutil.which("jq")
BACKSTOP = 60  # seconds; a hang backstop, not a speed bound
TOKEN = "fake-token-never-printed"

FAKE_GIT = r"""#!/bin/sh
printf '%s\n' "$*" >>"$FAKE_DIR/git-calls"
case "$1 $2" in
  "rev-parse HEAD") echo "$FAKE_SHA" ;;
  "remote get-url") echo "$FAKE_PUSH_URL" ;;
  "push --porcelain")
    if [ "${FAKE_PUSH_EXIT:-0}" != 0 ]; then echo "fatal: unable to access remote" >&2; exit 128; fi
    flag='*'; note='[new branch]'
    if [ "${FAKE_PUSH_STATE:-new}" = uptodate ]; then flag='='; note='[up to date]'; fi
    printf 'To remote\n%s\t%s\t%s\nDone\n' "$flag" "$4" "$note" ;;
esac
"""

FAKE_GH = r"""#!{python}
import json, os, subprocess, sys
d = os.environ["FAKE_DIR"]
with open(os.path.join(d, "gh-calls"), "a") as f:
    f.write(json.dumps(sys.argv[1:]) + "\n")
with open(os.path.join(d, "gh-calls")) as f:
    n = sum(1 for _ in f)
with open(os.path.join(d, "gh-responses.json")) as f:
    responses = json.load(f)
r = responses[min(n, len(responses)) - 1]
if r == "down":
    sys.stderr.write("error connecting to api.github.com\n")
    sys.exit(1)
args = sys.argv[1:]
body = {{"total_count": len(r), "check_runs": r}}
out = subprocess.run([{jq!r}, "-r", args[args.index("--jq") + 1]], input=json.dumps(body),
                     capture_output=True, text=True, check=True)
sys.stdout.write(out.stdout)
"""


def run_(id_, name, status="completed", conclusion="success", app="github-actions"):
    return {"id": id_, "name": name, "status": status,
            "conclusion": conclusion if status == "completed" else None,
            "app": {"slug": app}}


def six(conclusion="success", start=100, **overrides):
    """One run of each required check; overrides maps a name to a conclusion,
    or to "queued"/"in_progress" for a run that has not finished."""
    runs = []
    for i, name in enumerate(REQUIRED):
        c = overrides.get(name, conclusion)
        if c in ("queued", "in_progress"):
            runs.append(run_(start + i, name, status=c))
        else:
            runs.append(run_(start + i, name, conclusion=c))
    return runs


# CI's runners have jq; there a missing jq fails these tests instead of skipping them.
@unittest.skipUnless(JQ or os.environ.get("CI"), "jq is needed to apply the script's --jq filter")
class CiVerify(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.dir = Path(temp.name).resolve()
        bin_ = self.dir / "bin"
        bin_.mkdir()
        (bin_ / "git").write_text(FAKE_GIT)
        (bin_ / "gh").write_text(FAKE_GH.format(python=sys.executable, jq=JQ or "jq"))
        for name in ("git", "gh"):
            (bin_ / name).chmod(0o755)
        self.env = {
            "PATH": f"{bin_}:/usr/bin:/bin",
            "HOME": str(self.dir),
            "TMPDIR": str(self.dir),
            "FAKE_DIR": str(self.dir),
            "FAKE_SHA": SHA,
            "FAKE_PUSH_URL": "https://github.com/SquashMerge/trellis.git",
            "GH_TOKEN": TOKEN,
            "CI_VERIFY_POLL_S": "0.05",
            "CI_VERIFY_OUTAGE_S": "1",
            "CI_VERIFY_START_WAIT_S": "30",
            "CI_VERIFY_DEADLINE_S": "40",
            "CI_VERIFY_CALL_TIMEOUT_S": "20",
        }

    def start(self, responses, **env):
        (self.dir / "gh-responses.json").write_text(json.dumps(responses))
        return subprocess.Popen(["sh", str(SCRIPT)], cwd=self.dir, env={**self.env, **env},
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def run_script(self, responses, **env):
        proc = self.start(responses, **env)
        out, err = proc.communicate(timeout=BACKSTOP)
        self.assertNotIn(TOKEN, out + err)
        return proc.returncode, err

    def calls(self, tool):
        path = self.dir / f"{tool}-calls"
        if not path.exists():
            return []
        lines = path.read_text().splitlines()
        return [json.loads(line) for line in lines] if tool == "gh" else lines

    def pushes(self):
        return [c for c in self.calls("git") if c.startswith("push")]

    def test_six_successes_pass_and_the_pushed_branch_is_deleted(self):
        code, err = self.run_script([[], six()])
        self.assertEqual(code, 0, err)
        self.assertEqual(self.pushes(), [f"push --porcelain origin {SHA}:{REF}",
                                         f"push origin --delete {REF}"])
        # The repository comes from the push URL and is named in every call.
        for call in self.calls("gh"):
            self.assertEqual(call[:3], ["api", "--paginate",
                                        f"repos/SquashMerge/trellis/commits/{SHA}/check-runs?filter=all&per_page=100"])
            self.assertFalse({"-R", "--repo"} & set(call))

    def test_a_failed_or_timed_out_check_fails_verify(self):
        for conclusion in ("failure", "timed_out"):
            with self.subTest(conclusion):
                (self.dir / "gh-calls").unlink(missing_ok=True)
                # The failure decides at once, while other checks still run.
                runs = six(**{"tests (macos-latest)": conclusion, "browser (ubuntu-latest)": "in_progress"})
                code, err = self.run_script([[], runs])
                self.assertEqual(code, 1, err)
                self.assertIn(f"tests (macos-latest) concluded {conclusion}", err)

    def test_only_the_newest_run_of_each_check_counts(self):
        name = "checks (ubuntu-latest)"
        old_failure = six() + [run_(50, name, conclusion="failure")]
        code, err = self.run_script([[], old_failure])
        self.assertEqual(code, 0, err)
        (self.dir / "gh-calls").unlink()
        new_failure = six() + [run_(500, name, conclusion="failure")]
        code, err = self.run_script([[], new_failure])
        self.assertEqual(code, 1, err)
        self.assertIn(f"{name} concluded failure", err)

    def test_runs_from_before_the_push_do_not_answer_it(self):
        earlier = six(start=10)
        # The push started nothing: the earlier successes do not pass it.
        code, err = self.run_script([earlier, earlier], CI_VERIFY_START_WAIT_S="1")
        self.assertEqual(code, 75, err)
        # Nor do earlier failures fail it.
        (self.dir / "gh-calls").unlink()
        failed = six("failure", start=10)
        code, err = self.run_script([failed, failed + six()])
        self.assertEqual(code, 0, err)
        # A branch an earlier verify left behind already pointed here: its runs count.
        (self.dir / "gh-calls").unlink()
        code, err = self.run_script([earlier, earlier], FAKE_PUSH_STATE="uptodate")
        self.assertEqual(code, 0, err)

    def test_verify_waits_for_running_checks(self):
        pending = six(**{"browser (macos-latest)": "queued", "tests (ubuntu-latest)": "in_progress"})
        code, err = self.run_script([[], [], pending, pending, six()])
        self.assertEqual(code, 0, err)
        self.assertEqual(len(self.calls("gh")), 5)

    def test_a_missing_check_never_counts_as_a_pass(self):
        five = [r for r in six() if r["name"] != "checks (macos-latest)"]
        # Another app's run of the same name is not the Actions check.
        five.append(run_(900, "checks (macos-latest)", app="some-other-app"))
        code, err = self.run_script([[], five], CI_VERIFY_START_WAIT_S="1")
        self.assertEqual(code, 75, err)
        self.assertIn("not started after 1 s: checks (macos-latest)", err)

    def test_nothing_started_is_unavailable(self):
        code, err = self.run_script([[], []], CI_VERIFY_START_WAIT_S="1")
        self.assertEqual(code, 75, err)
        self.assertIn("not started after 1 s", err)

    def test_a_check_that_could_not_run_is_unavailable(self):
        for conclusion in ("cancelled", "startup_failure", "stale", "action_required", "skipped", "neutral"):
            with self.subTest(conclusion):
                (self.dir / "gh-calls").unlink(missing_ok=True)
                code, err = self.run_script([[], six(**{"browser (ubuntu-latest)": conclusion})])
                self.assertEqual(code, 75, err)
                self.assertIn(f"browser (ubuntu-latest) concluded {conclusion}", err)

    def test_single_api_errors_are_retried(self):
        code, err = self.run_script(["down", [], "down", "down", six()], CI_VERIFY_OUTAGE_S="30")
        self.assertEqual(code, 0, err)

    def test_a_sustained_outage_is_unavailable(self):
        code, err = self.run_script([[], "down"])
        self.assertEqual(code, 75, err)
        self.assertIn("GitHub unreachable for 1 s", err)
        self.assertEqual(self.pushes()[-1], f"push origin --delete {REF}")

    def test_the_deadline_is_unavailable(self):
        code, err = self.run_script([[], six(**{"tests (ubuntu-latest)": "in_progress"})],
                                    CI_VERIFY_DEADLINE_S="1")
        self.assertEqual(code, 75, err)
        self.assertIn("deadline of 1 s reached; still waiting: tests (ubuntu-latest)", err)

    def test_only_github_push_urls_are_accepted(self):
        accepted = {
            "https://github.com/O/R": "O/R",
            "https://github.com/O-1/r.x.git": "O-1/r.x",
            "git@github.com:O/R.git": "O/R",
            "ssh://git@github.com/O/R.git": "O/R",
        }
        for url, repo in accepted.items():
            with self.subTest(url):
                (self.dir / "gh-calls").unlink(missing_ok=True)
                code, err = self.run_script([[], six()], FAKE_PUSH_URL=url)
                self.assertEqual(code, 0, err)
                self.assertTrue(self.calls("gh")[0][2].startswith(f"repos/{repo}/commits/{SHA}/"))
        rejected = [
            "/Users/someone/trellis",
            "file:///srv/trellis.git",
            "https://gitlab.com/O/R.git",
            "https://x-access-token:secret@github.com/O/R.git",
            "https://github.com/O/R/extra",
            "https://github.com/O",
            "git@github.com:O/R;rm.git",
            "",
        ]
        for url in rejected:
            with self.subTest(url):
                (self.dir / "git-calls").unlink(missing_ok=True)
                (self.dir / "gh-calls").unlink(missing_ok=True)
                code, err = self.run_script([[], six()], FAKE_PUSH_URL=url)
                self.assertEqual(code, 75, err)
                self.assertIn("not a GitHub repository URL", err)
                if url:
                    self.assertNotIn(url, err)
                self.assertEqual(self.pushes(), [])
                self.assertEqual(self.calls("gh"), [])

    def test_a_failed_push_deletes_nothing(self):
        code, err = self.run_script([[], six()], FAKE_PUSH_EXIT="1")
        self.assertEqual(code, 75, err)
        self.assertEqual(self.pushes(), [f"push --porcelain origin {SHA}:{REF}"])

    def test_a_signal_while_waiting_deletes_the_pushed_branch(self):
        for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            with self.subTest(sig.name):
                (self.dir / "git-calls").unlink(missing_ok=True)
                (self.dir / "gh-calls").unlink(missing_ok=True)
                proc = self.start([[], []])
                # Two gh calls: the look before the push, then a poll after it.
                deadline = time.monotonic() + BACKSTOP
                while len(self.calls("gh")) < 2:
                    self.assertLess(time.monotonic(), deadline, "the script never polled")
                    time.sleep(0.02)
                proc.send_signal(sig)
                out, err = proc.communicate(timeout=BACKSTOP)
                self.assertEqual(proc.returncode, 75, err)
                self.assertEqual(self.pushes(), [f"push --porcelain origin {SHA}:{REF}",
                                                 f"push origin --delete {REF}"])


class CiVerifyConfig(unittest.TestCase):
    def test_required_checks_match_the_rust_workflow_matrix(self):
        workflow = (ROOT / ".github" / "workflows" / "rust.yml").read_text()
        oses = re.search(r"^\s*os: \[(.*)\]$", workflow, re.M).group(1).split(", ")
        jobs = re.search(r"^\s*job: \[(.*)\]$", workflow, re.M).group(1).split(", ")
        self.assertIn('name: ${{ matrix.job }} (${{ matrix.os }})', workflow)
        script = SCRIPT.read_text()
        listed = re.search(r'^required="(.*?)"$', script, re.M | re.S).group(1).split("\n")
        self.assertEqual(sorted(listed), sorted(f"{j} ({o})" for j in jobs for o in oses))
        self.assertEqual(sorted(listed), sorted(REQUIRED))

    def test_factory_config_runs_this_script_within_its_timeout(self):
        config = json.loads((ROOT / ".factory.json").read_text())
        self.assertEqual(config["verify"], "sh tools/ci-verify.sh")
        self.assertEqual(config["verify_unavailable_exit"], 75)
        self.assertEqual(config["verify_scope"], "integration")
        deadline_s = int(re.search(r"CI_VERIFY_DEADLINE_S:-(\d+)", SCRIPT.read_text()).group(1))
        call_s = int(re.search(r"CI_VERIFY_CALL_TIMEOUT_S:-(\d+)", SCRIPT.read_text()).group(1))
        poll_s = int(re.search(r"CI_VERIFY_POLL_S:-(\d+)", SCRIPT.read_text()).group(1))
        # Past the deadline the script still finishes one poll, one call and the delete.
        self.assertLess((deadline_s + poll_s + 3 * call_s) * 1000, config["verify_timeout_ms"])


if __name__ == "__main__":
    unittest.main()
