#!/usr/bin/env python3
"""Exercise an already packaged binary, with an owned private daemon."""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import selectors
import shutil
import signal
import subprocess
import tempfile
import time
import urllib.request


def run(binary, source, output):
    with tempfile.TemporaryDirectory(prefix="tr-rel-", dir="/tmp") as temporary:
        root = Path(temporary)
        # A unique executable path identifies any daemon auto-started after
        # an ordinary crash of the explicitly owned initial daemon.
        private_binary = root / "trellis"
        shutil.copy2(binary, private_binary)
        binary = str(private_binary.resolve())
        home, workspace = root / "home", root / "workspace"
        home.mkdir()
        workspace.mkdir()
        (workspace / "sample.js").write_text("function releaseSymbol() { return 1; }\n")
        env = dict(os.environ, HOME=str(home))
        for name in ("XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR"):
            env.pop(name, None)
        version = subprocess.check_output([binary, "--version"], env=env, text=True).strip()
        data = (home / "Library/Application Support/dev.squashmerge.trellis"
                if os.uname().sysname == "Darwin" else home / ".local/share/trellis")
        socket = data / "run/daemon.sock"
        daemon_log = root / "daemon.log"
        with daemon_log.open("w+") as log:
            daemon = subprocess.Popen([binary, "daemon"], env=env, stdout=log, stderr=log)
            serve = None
            try:
                deadline = time.monotonic() + 30
                while not socket.exists():
                    if daemon.poll() is not None or time.monotonic() >= deadline:
                        log.seek(0)
                        raise RuntimeError("daemon failed to become ready: " + log.read())
                    time.sleep(0.05)
                def cli(*args):
                    result = json.loads(subprocess.check_output(
                        [binary, *args, "--workspace", str(workspace)], env=env, text=True, timeout=120))
                    if daemon.poll() is not None:
                        raise RuntimeError("owned daemon exited during smoke")
                    return result
                cli("index")
                status = cli("status")
                build = status["build"]
                assert "test-support" not in build["features"], build
                assert build["target"] in version and build["version"] in version, version
                symbols = cli("symbols", "--search", "releaseSymbol")
                assert "releaseSymbol" in json.dumps(symbols), symbols
                token_file = root / "browser.token"
                serve = subprocess.Popen(
                    [binary, "serve", "--workspace", str(workspace), "--bind", "127.0.0.1:0",
                     "--token-file", str(token_file)], env=env, text=True,
                    stdout=log, stderr=subprocess.PIPE)
                selector = selectors.DefaultSelector()
                selector.register(serve.stderr, selectors.EVENT_READ)
                banner = ""
                deadline = time.monotonic() + 30
                try:
                    while True:
                        if not selector.select(max(0, deadline - time.monotonic())):
                            raise RuntimeError("serve readiness banner timed out: " + banner)
                        chunk = os.read(serve.stderr.fileno(), 4096)
                        if not chunk:
                            raise RuntimeError("serve exited before readiness: " + banner)
                        banner += chunk.decode("utf-8", errors="replace")
                        match = re.search(r"http://127\.0\.0\.1:[0-9]+/", banner)
                        if match:
                            break
                finally:
                    selector.close()
                address = match.group(0).rstrip("/")
                asset = "web/vendor/pele-1.10.0-trellis.2.js"
                with urllib.request.urlopen(address + "/vendor/pele-1.10.0-trellis.2.js", timeout=10) as response:
                    actual = response.read()
                digest = hashlib.sha256(actual).hexdigest()
                assert digest == hashlib.sha256((source / asset).read_bytes()).hexdigest()
                token = token_file.read_text().strip()
                request = urllib.request.Request(address + "/api/daemon/status",
                                                 headers={"Authorization": "Bearer " + token})
                with urllib.request.urlopen(request, timeout=10) as response:
                    daemon_status = json.load(response)
                if daemon.poll() is not None:
                    raise RuntimeError("owned daemon exited during browser smoke")
                result = {"build": build, "versionOutput": version, "embeddedPeleSha256": digest,
                          "binarySha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
                          "daemonStatus": daemon_status}
                output.write_text(json.dumps(result, indent=2) + "\n")
            finally:
                for child in (serve, daemon):
                    if child is not None:
                        if child.poll() is None:
                            child.terminate()
                        try:
                            child.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            child.kill()
                            child.wait()
                try:
                    replacements_stopped = stop_replacements(home, binary)
                except BaseException:
                    output.unlink(missing_ok=True)
                    raise
                if replacements_stopped:
                    # A replacement means this smoke lost its owned daemon.
                    output.unlink(missing_ok=True)
                    raise RuntimeError("unexpected detached replacement invalidated smoke")


def stop_replacements(home, binary):
    """Only this smoke's private HOME and exact executable may be stopped.

    ps lstart has second resolution; this and the immediate scope recheck
    narrow the race but cannot make numeric signaling atomic.
    """
    home_token = re.compile(r"(?:^| )HOME=" + re.escape(str(home))
                            + r"(?= [A-Za-z_][A-Za-z0-9_]*=|$)")

    def candidates(pid=None):
        args = ["ps", "eww", *(["-A"] if pid is None else ["-p", str(pid)]),
                "-o", "pid=,lstart=,stat=,command="]
        result = subprocess.run(args, capture_output=True, text=True, timeout=10)
        if result.returncode != 0 and not (pid is not None and result.returncode == 1):
            raise RuntimeError("cannot inspect smoke replacements: " + result.stderr)
        found = {}
        for line in result.stdout.splitlines():
            fields = line.strip().split(None, 7)
            if len(fields) != 8:
                continue
            raw_pid, *start, state, command = fields
            if (int(raw_pid) > 0 and not state.startswith("Z")
                    and command.startswith(binary + " daemon ") and home_token.search(command)):
                found[int(raw_pid)] = " ".join(start)
        return found

    remaining = candidates()
    found = bool(remaining)

    def matching(pid):
        return candidates(pid).get(pid) == remaining[pid]

    def send(pid, sig):
        if matching(pid):
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                pass

    for pid in remaining:
        send(pid, signal.SIGTERM)
    deadline = time.monotonic() + 10
    while remaining:
        remaining = {pid: start for pid, start in remaining.items() if matching(pid)}
        if not remaining:
            break
        if time.monotonic() >= deadline:
            for pid in remaining:
                send(pid, signal.SIGKILL)
            raise RuntimeError("detached smoke daemon did not stop after SIGTERM")
        time.sleep(0.05)
    return found


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(str(args.binary.resolve()), args.source.resolve(), args.output.resolve())
