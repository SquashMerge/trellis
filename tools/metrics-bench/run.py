#!/usr/bin/env python3
"""Manual, isolated, whole-revision metrics benchmark. No CI performance gate."""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


def emit(record, output):
    with output.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(record, sort_keys=True) + "\n")
        stream.flush()
    print(json.dumps(record, sort_keys=True), flush=True)


def cli(binary, env, args, timeout):
    command = [str(binary), *args]
    started = time.perf_counter()
    try:
        result = subprocess.run(command, env=env, capture_output=True, timeout=timeout)
        elapsed = time.perf_counter() - started
        try:
            data = json.loads(result.stdout)
        except (ValueError, UnicodeError):
            data = None
        return {"command": command, "elapsedSeconds": round(elapsed, 6),
                "exit": result.returncode, "responseBytes": len(result.stdout),
                "error": result.stderr.decode("utf-8", "replace")[-4096:], "data": data}
    except subprocess.TimeoutExpired as error:
        return {"command": command, "elapsedSeconds": round(time.perf_counter() - started, 6),
                "timeoutSeconds": timeout, "partialBytes": len(error.stdout or b""), "data": None}


def http(url, token, method="GET"):
    started = time.perf_counter()
    request = urllib.request.Request(url, headers={"Authorization": "Bearer " + token}, method=method)
    try:
        with urllib.request.urlopen(request, timeout=335) as response:
            raw = response.read()
            status = response.status
    except urllib.error.HTTPError as error:
        raw = error.read()
        status = error.code
    except Exception as error:
        return {"url": url, "elapsedSeconds": round(time.perf_counter() - started, 6),
                "error": repr(error), "data": None}
    try:
        data = json.loads(raw)
    except (ValueError, UnicodeError) as error:
        data = None
        parse_error = repr(error)
    else:
        parse_error = None
    return {"url": url, "elapsedSeconds": round(time.perf_counter() - started, 6),
            "status": status, "responseBytes": len(raw), "parseError": parse_error, "data": data}


def summarize(result, expected, kind):
    data = result.pop("data", None)
    if not isinstance(data, dict) or "payload" not in data:
        result["typedError"] = data
        return result, None
    payload = data["payload"]
    files = payload.get("files", [])
    directories = payload.get("directories", [])
    declarations = payload.get("declarations", [])
    result.update({"catchingUp": data.get("catchingUp"),
                   "pin": [payload.get("indexGeneration"), payload.get("indexRevision")],
                   "metricsVersion": payload.get("metricsVersion"),
                   "selectedMetrics": payload.get("selectedMetrics"),
                   "scope": payload.get("scope"), "aggregate": payload.get("aggregate"),
                   "fileRows": len(files), "directoryRows": len(directories),
                   "declarationRows": len(declarations),
                   "callablesComputed": sum(row["declarationCount"] for row in files),
                   "rootDirectoryRows": sum(row.get("path") == "" for row in directories)})
    valid = (result.get("status", 200) == 200 and result.get("exit", 0) == 0
             and payload.get("scope") == kind and payload.get("metricsVersion") == "metrics:v1"
             and (len(files) == expected if kind == "revision" else len(files) == 1)
             and (len(directories) == {1000: 9, 10000: 44}[expected] if kind == "revision" else not directories)
             and (not declarations if kind == "revision" else bool(declarations))
             and (bool(result["rootDirectoryRows"]) if kind == "revision" else True)
             and len(payload.get("selectedMetrics", [])) == 12)
    result["validFullResponse"] = valid
    return result, payload if valid else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--cohort", choices=["medium", "large"], required=True)
    parser.add_argument("--transport", choices=["http", "cli"], required=True)
    parser.add_argument("--generated", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--resume-index", action="store_true", help="resume an already enqueued index after a failed CLI wait")
    parser.add_argument("--reuse-home", type=Path, help="existing indexed HOME; do not enqueue/index again")
    parser.add_argument("--pin", help="complete generation:decimal-revision pin for a reused index")
    parser.add_argument("--token-file", type=Path, help="existing private token file for reused HOME")
    parser.add_argument("--file-on-error", action="store_true", help="time a pinned file even if full revision fails; never call it a full response")
    parser.add_argument("--file-only", action="store_true", help="one bounded pinned file retry, no revision request")
    parser.add_argument("--file-path", help="exact relative Java file for --file-only")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    generated = args.generated.resolve(strict=True)
    root = generated / args.cohort
    expected = {"medium": 1000, "large": 10000}[args.cohort]
    if sum(path.suffix in (".java", ".py", ".js", ".rs") for path in root.rglob("*")) != expected:
        parser.error("cohort source count differs from canonical manifest")
    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    if args.file_path and not args.file_only:
        parser.error("--file-path requires --file-only")
    if args.file_only and not args.pin:
        parser.error("--file-only requires --pin and --reuse-home")
    if bool(args.reuse_home) != bool(args.pin):
        parser.error("--reuse-home and --pin must be supplied together")
    home = args.reuse_home.resolve() if args.reuse_home else output_dir / (args.cohort + "-" + args.transport + "-home")
    if not args.resume_index and not args.reuse_home:
        home.mkdir(mode=0o700, exist_ok=False)
    elif not home.is_dir():
        parser.error("existing home does not exist")
    output = output_dir / (args.cohort + "-" + args.transport + ".jsonl")
    env = os.environ.copy()
    env["HOME"] = str(home)
    env.pop("XDG_CACHE_HOME", None)
    env.pop("XDG_DATA_HOME", None)
    serve_log = output_dir / (args.cohort + "-" + args.transport + "-serve.log")
    emit({"event": "setup", "cohort": args.cohort, "transport": args.transport,
          "files": expected, "seed": "trellis-synthetic-cohorts-v1",
          "platform": platform.platform(), "processor": platform.processor(),
          "home": str(home), "workspace": str(root), "binary": str(binary),
          "serveLog": str(serve_log)}, output)
    serve = None
    try:
        token_path = (args.token_file.resolve() if args.token_file else
                      (home.parent / (args.cohort + "-http-token") if args.reuse_home
                       else output_dir / (args.cohort + "-" + args.transport + "-token")))
        command = [str(binary), "serve", "--workspace", str(root),
                   "--bind", "127.0.0.1:0", "--token-file", str(token_path)]
        with serve_log.open("wb") as log:
            serve = subprocess.Popen(command, env=env, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.DEVNULL, stderr=log)
        deadline = time.monotonic() + 15
        address = None
        while time.monotonic() < deadline:
            text = serve_log.read_text(errors="replace")
            match = re.search(r"Trellis: http://(127\.0\.0\.1:\d+)/", text)
            if match:
                address = match.group(1)
                break
            if serve.poll() is not None:
                raise RuntimeError("serve exited: " + text[-4096:])
            time.sleep(0.05)  # readiness only; never part of timing
        if address is None:
            raise RuntimeError("serve startup deadline: " + serve_log.read_text()[-4096:])
        token = token_path.read_text().strip()
        listing = http("http://" + address + "/api/checkouts", token)
        checkouts = listing.get("data", {}).get("checkouts", [])
        keys = [row["rootKey"] for row in checkouts if row.get("workspaceRoot") == str(root)]
        if len(keys) != 1:
            raise RuntimeError("selected checkout absent: " + repr(checkouts))
        key = keys[0]
        base_url = "http://" + address + "/api/checkouts/" + key + "/metrics"
        emit({"event": "serve", "command": command, "address": address,
              "rootKey": key, "selectedCheckouts": checkouts}, output)
        # Background HTTP index avoids the CLI's separate 60-second index wait.
        # Neither indexing path computes metrics or warms the source-only memo.
        if args.reuse_home:
            status = http(base_url.replace("/metrics", "/status"), token)
            emit({"event": "indexReused", **status}, output)
            if status.get("status") != 200 or not isinstance(status.get("data"), dict):
                raise RuntimeError("cannot reuse index status")
            requested_pin = args.pin.rsplit(":", 1)
            if len(requested_pin) != 2 or not requested_pin[1].isdecimal():
                raise RuntimeError("invalid reuse pin")
            head = status["data"].get("revision", {})
            if (head.get("indexGeneration") != requested_pin[0]
                    or not isinstance(head.get("indexRevision"), int)
                    or head["indexRevision"] < int(requested_pin[1])):
                raise RuntimeError("reused index generation changed or selected pin is newer than head")
            # A new watcher revision may become head on startup. The supplied
            # retained pin is checked by the real metrics request, not inferred
            # absent merely because head moved forward.
        elif not args.resume_index:
            admitted = http(base_url.replace("/metrics", "/index"), token, "POST")
            emit({"event": "indexAdmission", **admitted}, output)
            if admitted.get("status") != 202 or not admitted.get("data", {}).get("id"):
                raise RuntimeError("index admission failed")
            job_id = admitted["data"]["id"]
        elif args.resume_index:
            current = http(base_url.replace("/metrics", "/jobs/current"), token)
            emit({"event": "indexResume", **current}, output)
            if current.get("status") != 200 or not isinstance(current.get("data"), dict):
                raise RuntimeError("cannot resume index job")
            job_id = current["data"]["id"]
        if not args.reuse_home:
            job_url = base_url.replace("/metrics", "/jobs/") + job_id
            job_deadline = time.monotonic() + 7200
        while not args.reuse_home and time.monotonic() < job_deadline:
            job = http(job_url, token)
            if job.get("status") == 200 and job.get("data", {}).get("finishedAt"):
                emit({"event": "indexTerminal", **job}, output)
                if job["data"]["state"] != "done":
                    raise RuntimeError("index job finished without publication")
                break
            if job.get("status") not in (200, 409):
                emit({"event": "indexPollFailure", **job}, output)
                raise RuntimeError("index job polling failed")
            time.sleep(1)  # setup only; never included in request timing
        else:
            if not args.reuse_home:
                raise RuntimeError("index job exceeded 7200-second setup backstop")
        pin = (args.pin.rsplit(":", 1)[0], int(args.pin.rsplit(":", 1)[1])) if args.pin else None
        revision_failed = False
        first = None
        if args.file_only:
            state = http(base_url.replace("/metrics", "/jobs/current"), token)
            emit({"event": "fileRetryJobState", **state}, output)
            if state.get("status") != 200 or (state.get("data") is not None
                                                  and state["data"].get("finishedAt") is None):
                raise RuntimeError("job not terminal before file retry")
            candidate = root / args.file_path if args.file_path else next(root.glob("java/*.java"), None)
            if candidate is None or not candidate.is_file() or candidate.parent != root / "java":
                raise RuntimeError("no selected Java file for pinned file retry")
            first = {"sourceSetId": "source-set:v1:path-" + key,
                     "language": "java", "path": candidate.relative_to(root).as_posix()}
        else:
            for phase in ("cold", "warm"):
                if args.transport == "http":
                    suffix = "" if pin is None else "?" + urllib.parse.urlencode(
                        {"indexGeneration": pin[0], "indexRevision": pin[1]})
                    result = http(base_url + suffix, token)
                else:
                    arguments = ["metrics", "--json", "--workspace", str(root)]
                    if pin is not None:
                        arguments += ["--pin", pin[0] + ":" + str(pin[1])]
                    result = cli(binary, env, arguments, 335)
                result, payload = summarize(result, expected, "revision")
                result.update({"event": "revision", "phase": phase,
                               "daemonMemo": "fresh (require restarted daemon for --reuse-home)" if phase == "cold" else "same daemon and pin"})
                emit(result, output)
                if payload is None:
                    if args.file_on_error and pin is not None:
                        revision_failed = True
                        candidate = next(root.glob("java/*.java"), None)
                        if candidate is None:
                            raise RuntimeError("no Java file for independent file request")
                        first = {"sourceSetId": "source-set:v1:path-" + key,
                                 "language": "java", "path": candidate.relative_to(root).as_posix()}
                        break  # No warm measurement after a failed cold full revision.
                    raise RuntimeError("full revision response failed; see typed result")
                if pin is not None and list(pin) != result["pin"]:
                    raise RuntimeError("warm pin changed")
                pin = result["pin"]
                if phase == "cold":
                    first = next((row for row in payload["files"]
                                  if row["language"] == "java"), None)
        if not first:
            raise RuntimeError("no Java file row")
        if args.transport == "http":
            query = urllib.parse.urlencode({"scope": "file", "sourceSetId": first["sourceSetId"],
                                           "language": first["language"], "path": first["path"],
                                           "indexGeneration": pin[0], "indexRevision": pin[1]})
            result = http(base_url + "?" + query, token)
        else:
            result = cli(binary, env, ["metrics", "--json", "--workspace", str(root),
                                       "--pin", pin[0] + ":" + str(pin[1]), "--scope", "file",
                                       "--source-set-id", first["sourceSetId"],
                                       "--language", first["language"], "--path", first["path"]], 45)
        result, payload = summarize(result, expected, "file")
        result.update({"event": "file", "phase": "standalone retry" if args.file_only else
                       ("after failed cold" if revision_failed else "after warm"),
                       "selectedPath": first["path"]})
        emit(result, output)
        if payload is None or result["pin"] != list(pin):
            raise RuntimeError("full file response failed")
        if revision_failed:
            raise RuntimeError("full revision failed; independent file result recorded above")
    except Exception as error:
        emit({"event": "failure", "error": repr(error)}, output)
        return 2
    finally:
        if serve is not None:
            serve.terminate()
            try:
                serve.wait(timeout=10)
            except subprocess.TimeoutExpired:
                serve.kill()
                serve.wait(timeout=5)
    return 0


if __name__ == "__main__":
    sys.exit(main())
