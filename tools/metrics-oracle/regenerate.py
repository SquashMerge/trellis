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


def run(*args):
    return subprocess.run(args, check=True, text=True, capture_output=True).stdout.strip()


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
    args = parser.parse_args()
    verify()
    os.chdir(ROOT)
    if not args.regenerate:
        return
    with tempfile.TemporaryDirectory(prefix="trellis-metrics-oracle-") as tmp:
        rca = pathlib.Path(tmp) / "rust-code-analysis"
        run("git", "clone", "https://github.com/mozilla/rust-code-analysis.git", str(rca))
        run("git", "-C", str(rca), "checkout", "--detach", RCA)
        if run("git", "-C", str(rca), "rev-parse", "HEAD") != RCA:
            raise SystemExit("RCA checkout pin mismatch")
        shutil.copyfile(HERE / "rust-code-analysis-v0.0.25.Cargo.lock", rca / "Cargo.lock")
        run("cargo", "build", "--locked", "--manifest-path", str(rca / "Cargo.toml"), "-p", "rust-code-analysis-cli", "--release")
        run("cargo", "build", "--locked", "--manifest-path", str(HERE / "Cargo.toml"), "--release")
        for source in sorted((ROOT / "tests/fixtures/metrics").glob("*/*")):
            if not source.is_file():
                continue
            language = source.parent.name
            raw = run("cargo", "run", "--locked", "--manifest-path", str(HERE / "Cargo.toml"), "--release", "--", language, str(source.relative_to(ROOT)))
            output = ROOT / "tests/fixtures/metrics/golden" / (source.name + ".arborist.json")
            output.write_text(json.dumps(json.loads(raw), indent=2, sort_keys=True) + "\n")
            # RCA emits file-level JSON, not native declaration rows.
            rca_raw = run(str(rca / "target/release/rust-code-analysis-cli"), "--metrics", "--output-format", "json", "--paths", str(source))
            (output.parent / (source.name + ".rca.json")).write_text(json.dumps(json.loads(rca_raw), indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
