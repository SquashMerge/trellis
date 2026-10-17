#!/usr/bin/env python3
"""Manual, locked oracle regeneration; never invoked by CI."""
import argparse
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent.parent
ARBORIST = "d246d2e465346cbc2a7cc468d34ddbb7d77d5a0a"
RCA = "57ab3973b53e7bf392d925cd5e6c6e889b8c22c0"
CHECKSUMS = {
    "Cargo.lock": "adb7081bf5aa0cd44dbc7044656c1c0ef75d99e75cb3cbff2d8253b2dd1e5682",
    "rust-code-analysis-v0.0.25.Cargo.lock": "69c4a0417d22f15143a3683060132421f9add3550ba53f2d43861a94850c9379",
}

# The complete reviewed comparison inventory. Stems preserve committed filenames.
SOURCES = (
    ("java", "tests/fixtures/metrics/java/CallbackTest.java", "CallbackTest.java"),
    ("java", "tests/fixtures/metrics/java/Controls.java", "Controls.java"),
    ("java", "tests/fixtures/metrics/java/Example.java", "Example.java"),
    ("python", "tests/fixtures/metrics/python/controls.py", "controls.py"),
    ("python", "tests/fixtures/metrics/python/example.py", "example.py"),
    ("python", "tests/fixtures/metrics/python/test_callback.py", "test_callback.py"),
    ("javascript", "tests/fixtures/metrics/javascript/callback.test.js", "callback.test.js"),
    ("javascript", "tests/fixtures/metrics/javascript/controls.js", "controls.js"),
    ("javascript", "tests/fixtures/metrics/javascript/example.js", "example.js"),
    ("rust", "tests/fixtures/metrics/rust/callback.rs", "callback.rs"),
    ("rust", "tests/fixtures/metrics/rust/controls.rs", "controls.rs"),
    ("rust", "tests/fixtures/metrics/rust/example.rs", "example.rs"),
    ("javascript", "tests/fixtures/extraction/fixture/flow.js", "extraction-flow.js"),
    ("javascript", "tests/fixtures/extraction/edge-fixture/cases.js", "extraction-cases.js"),
    ("java", "tools/synthetic-cohorts/templates/medium/java.java", "medium-java.java"),
    ("python", "tools/synthetic-cohorts/templates/medium/python.py", "medium-python.py"),
    ("javascript", "tools/synthetic-cohorts/templates/medium/javascript.js", "medium-javascript.js"),
    ("rust", "tools/synthetic-cohorts/templates/medium/rust.rs", "medium-rust.rs"),
)


def inventory():
    if len(SOURCES) != 18 or len({stem for _, _, stem in SOURCES}) != len(SOURCES):
        raise SystemExit("oracle fixture inventory must have 18 unique golden stems")
    for _, relative, _ in SOURCES:
        if not (ROOT / relative).is_file():
            raise SystemExit(f"missing oracle source: {relative}")
    return SOURCES



def run(*args, env=None):
    return subprocess.run(args, check=True, text=True, capture_output=True, env=env).stdout.strip()


def verify():
    for name, expected in CHECKSUMS.items():
        actual = hashlib.sha256((HERE / name).read_bytes()).hexdigest()
        if actual != expected:
            raise SystemExit(f"{name}: SHA-256 mismatch: {actual}")
    lock = (HERE / "Cargo.lock").read_text()
    if ARBORIST not in lock:
        raise SystemExit("adapter lock does not contain Arborist pin")
    print("Pinned lock checksums and Arborist revision verified")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--regenerate", action="store_true", help="build both pinned oracles outside the repository and regenerate source fixture raw JSON")
    parser.add_argument("--inventory-json", action="store_true", help="print the reviewed source/stem inventory without running oracles")
    args = parser.parse_args()
    verify()
    os.chdir(ROOT)
    if args.inventory_json:
        print(json.dumps([{"language": language, "path": relative, "stem": stem} for language, relative, stem in inventory()], sort_keys=True))
        return
    if not args.regenerate:
        return
    inventory()
    with tempfile.TemporaryDirectory(prefix="trellis-metrics-oracle-") as tmp:
        rca = pathlib.Path(tmp) / "rust-code-analysis"
        rca_env = dict(os.environ)
        rca_env.pop("CARGO_TARGET_DIR", None)
        adapter_env = dict(os.environ, CARGO_TARGET_DIR=str(pathlib.Path(tmp) / "adapter-target"))
        run("git", "clone", "https://github.com/mozilla/rust-code-analysis.git", str(rca))
        run("git", "-C", str(rca), "checkout", "--detach", RCA)
        if run("git", "-C", str(rca), "rev-parse", "HEAD") != RCA:
            raise SystemExit("RCA checkout pin mismatch")
        shutil.copyfile(HERE / "rust-code-analysis-v0.0.25.Cargo.lock", rca / "Cargo.lock")
        run("cargo", "build", "--locked", "--manifest-path", str(rca / "Cargo.toml"), "-p", "rust-code-analysis-cli", "--release", env=rca_env)
        run("cargo", "build", "--locked", "--manifest-path", str(HERE / "Cargo.toml"), "--release", env=adapter_env)
        for language, relative, stem in inventory():
            raw = run("cargo", "run", "--locked", "--manifest-path", str(HERE / "Cargo.toml"), "--release", "--", language, relative, env=adapter_env)
            output = ROOT / "tests/fixtures/metrics/golden" / (stem + ".arborist.json")
            output.write_text(json.dumps(json.loads(raw), indent=2, sort_keys=True) + "\n")
            # RCA emits file-level JSON, not native declaration rows.
            rca_raw = run(str(rca / "target/release/rust-code-analysis-cli"), "--metrics", "--output-format", "json", "--paths", relative)
            (output.parent / (stem + ".rca.json")).write_text(json.dumps(json.loads(rca_raw), indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
