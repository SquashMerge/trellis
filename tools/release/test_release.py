#!/usr/bin/env python3
"""Focused release tooling tests; run with python3 -m unittest discover -s tools/release."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("release", Path(__file__).with_name("release.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)

LINUX = "x86_64-unknown-linux-gnu"
HEADER = """ELF Header:
  Class:                             ELF64
  Data:                              2's complement, little endian
  Type:                              DYN (Position-Independent Executable file)
  Machine:                           Advanced Micro Devices X86-64
"""
PROGRAM = "  [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]"
DYNAMIC = """Dynamic section at offset 0x1 contains 3 entries:
 0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]
 0x0000000000000001 (NEEDED)             Shared library: [libgcc_s.so.1]
 0x0000000000000000 (NULL)               0x0
"""
VERSIONS = """Version symbols section '.gnu.version' contains 5 entries:
  000: 0 (*local*) 2 (GLIBC_2.31)
Version needs section '.gnu.version_r' contains 1 entry:
 Addr: 0x00000000  Offset: 0x000000  Link: 7 (.dynstr)
  000000: Version: 1  File: libc.so.6  Cnt: 2
  0x0010: Name: GLIBC_2.31  Flags: none  Version: 3
  0x0020: Name: GLIBC_2.9  Flags: none  Version: 2
"""


class ElfTests(unittest.TestCase):
    def audit(self, header=HEADER, program=PROGRAM, versions=VERSIONS, dynamic=DYNAMIC, target=LINUX):
        return release.parse_elf(header, program, versions, dynamic, target)

    def test_imports_are_numeric_not_lexicographic(self):
        result = self.audit()
        self.assertEqual(result["maximumImportedGlibc"], "GLIBC_2.31")
        self.assertEqual(result["needed"], ["libc.so.6", "libgcc_s.so.1"])
        self.audit(versions=VERSIONS.replace("GLIBC_2.9", "GLIBC_2.3.4"))
        for symbol in ("GLIBC_2.32", "GLIBC_2.100", "GLIBC_2.31.1", "GLIBC_PRIVATE", "GLIBC_2.bad"):
            with self.subTest(symbol=symbol), self.assertRaisesRegex(ValueError, "GLIBC"):
                self.audit(versions=VERSIONS.replace("GLIBC_2.9", symbol))

    def test_architecture_and_interpreter(self):
        self.audit(header=HEADER.replace("Advanced Micro Devices X86-64", "AArch64"),
                   program=PROGRAM.replace("/lib64/ld-linux-x86-64.so.2", "/lib/ld-linux-aarch64.so.1"),
                   target="aarch64-unknown-linux-gnu")
        cases = [
            {"header": HEADER.replace("ELF64", "ELF32")},
            {"header": HEADER.replace("little endian", "big endian")},
            {"header": HEADER.replace("Advanced Micro Devices X86-64", "AArch64")},
            {"header": HEADER.replace("DYN", "REL")},
            {"program": ""},
            {"program": PROGRAM.replace("ld-linux-x86-64.so.2", "ld-musl-x86_64.so.1")},
        ]
        for case in cases:
            with self.subTest(case=case), self.assertRaises(ValueError):
                self.audit(**case)

    def test_dependencies_and_malformed_output(self):
        for dependency in ("libz.so.1", "libsqlite3.so.0", "libgit2.so.1.9", "libstdc++.so.6"):
            with self.subTest(dependency=dependency), self.assertRaisesRegex(ValueError, "DT_NEEDED"):
                self.audit(dynamic=DYNAMIC.replace("libgcc_s.so.1", dependency))
        cases = [
            {"dynamic": ""},
            {"dynamic": DYNAMIC.replace("Shared library: [libc.so.6]", "Shared library: libc.so.6")},
            {"versions": ""},
            {"versions": VERSIONS.replace("Version needs section", "Wrong section")},
            {"versions": VERSIONS.replace("Name: GLIBC_2.9  Flags: none", "Name: GLIBC_2.9")},
            {"versions": VERSIONS.replace("GLIBC_", "OTHER_")},
        ]
        for case in cases:
            with self.subTest(case=case), self.assertRaises(ValueError):
                self.audit(**case)

    def test_readelf_runs_all_audits_in_c_locale(self):
        with patch.object(release.subprocess, "check_output", side_effect=[HEADER, PROGRAM, VERSIONS, DYNAMIC]) as command:
            release.audit_elf(Path("trellis"), LINUX)
        self.assertEqual([c.args[0][2] for c in command.call_args_list], ["-h", "-l", "-V", "-d"])
        self.assertTrue(all(c.kwargs["env"]["LC_ALL"] == "C" for c in command.call_args_list))


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "built-binary"
        self.binary.write_bytes(b"release binary bytes\0\xff")
        self.license = self.root / "LICENSE"
        self.license.write_text("release license\n")

    def pack(self, output="out", target=LINUX):
        return release.package(self.binary, self.license, "0.1.0", target, 1710000000, self.root / output)

    def test_archive_bytes_ignore_input_and_output_metadata(self):
        with self.assertRaisesRegex(ValueError, "unsupported release target"):
            self.pack(target="x86_64-apple-darwin")
        first = self.pack("first")
        os.utime(self.binary, (1800000000, 1800000000))
        self.binary.chmod(0o600)
        second = self.pack("second")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(first.read_bytes()[4:8], (1710000000).to_bytes(4, "little"))
        with tarfile.open(first, "r:gz") as archive:
            members = archive.getmembers()
            self.assertEqual([(m.name, m.mode) for m in members], [("trellis", 0o755), ("LICENSE", 0o644)])
            self.assertTrue(all((m.uid, m.gid, m.uname, m.gname, m.mtime) == (0, 0, "", "", 1710000000) for m in members))
        self.assertEqual(release.archived_files(first), {"trellis": self.binary.read_bytes(), "LICENSE": self.license.read_bytes()})

    def tagged_repo(self):
        source = self.root / "source"
        source.mkdir()
        for path in ("Cargo.lock", "Cargo.toml", "rust-toolchain.toml", "build.rs", release.PELE_ASSET,
                     "web/index.html", "tools/release-linux.Dockerfile", "LICENSE"):
            file = source / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_bytes(self.license.read_bytes() if path == "LICENSE" else ("tagged " + path).encode())
        def git(*args):
            return subprocess.check_output(["git", "-C", str(source), *args], stderr=subprocess.DEVNULL).decode().strip()
        git("init", "-q")
        git("add", ".")
        git("-c", "user.name=Release Test", "-c", "user.email=release@example.invalid", "commit", "-qm", "tagged inputs")
        commit = git("rev-parse", "HEAD")
        git("tag", "v0.1.0")
        return source, commit

    def report(self, source, commit, target=LINUX):
        metal = target.endswith("apple-darwin")
        data = {
            "build": {"version": "0.1.0", "target": target, "commit": commit,
                      "features": ["metal"] if metal else [], "toolchain": "rustc 1.92.0",
                      "embeddingCapabilities": ["cpu", "metal"] if metal else ["cpu"]},
            "binarySha256": hashlib.sha256(self.binary.read_bytes()).hexdigest(),
            "embeddedPeleSha256": hashlib.sha256((source / release.PELE_ASSET).read_bytes()).hexdigest(),
        }
        return data

    def test_provenance_hashes_tagged_inputs_and_archived_smoke_bytes(self):
        source, commit = self.tagged_repo()
        data = self.report(source, commit)
        report = self.root / "smoke.json"
        report.write_text(json.dumps(data))
        archive = self.pack()
        # A later working-tree edit must not be misreported as the tagged input.
        (source / "Cargo.lock").write_text("later dirty lockfile")
        result = release.provenance(archive, source, "0.1.0", LINUX, commit, report)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        self.assertEqual(result["archiveSha256"], digest)
        self.assertEqual(Path(str(archive) + ".sha256").read_text(), digest + "  " + archive.name + "\n")
        self.assertEqual(json.loads(Path(str(archive) + ".provenance.json").read_text()), result)
        self.assertEqual(result["cargoLockSha256"], hashlib.sha256(b"tagged Cargo.lock").hexdigest())
        self.assertEqual(set(result["inputHashes"]), {"Cargo.lock", "Cargo.toml", "rust-toolchain.toml", "build.rs",
                                                    release.PELE_ASSET, "web/index.html", "tools/release-linux.Dockerfile"})
        self.assertEqual(set(result["webAssetsSha256"]), {release.PELE_ASSET, "web/index.html"})
        self.assertEqual(result["build"], data["build"])
        self.assertEqual(result["smokeReportSha256"], hashlib.sha256(report.read_bytes()).hexdigest())

    def test_provenance_rejects_mismatched_release_and_smoke(self):
        source, commit = self.tagged_repo()
        archive = self.pack()
        report = self.root / "smoke.json"
        cases = [("version", "0.2.0"), ("target", "aarch64-unknown-linux-gnu"), ("commit", "wrong"),
                 ("features", ["test-support"]), ("embeddingCapabilities", ["cpu", "metal"]), ("toolchain", "")]
        for key, value in cases:
            data = self.report(source, commit)
            data["build"][key] = value
            report.write_text(json.dumps(data))
            with self.subTest(key=key), self.assertRaises(ValueError):
                release.provenance(archive, source, "0.1.0", LINUX, commit, report)
        for key in ("binarySha256", "embeddedPeleSha256"):
            data = self.report(source, commit)
            data[key] = "different bytes"
            report.write_text(json.dumps(data))
            with self.subTest(key=key), self.assertRaises(ValueError):
                release.provenance(archive, source, "0.1.0", LINUX, commit, report)
        self.assertFalse(Path(str(archive) + ".provenance.json").exists())

    def test_macos_requires_metal_and_records_actual_descriptor(self):
        source, commit = self.tagged_repo()
        target = "aarch64-apple-darwin"
        archive = self.pack(target=target)
        report = self.root / "smoke.json"
        data = self.report(source, commit, target)
        report.write_text(json.dumps(data))
        result = release.provenance(archive, source, "0.1.0", target, commit, report)
        self.assertEqual(result["build"]["features"], ["metal"])
        data["build"]["features"] = []
        report.write_text(json.dumps(data))
        with self.assertRaisesRegex(ValueError, "shipping features"):
            release.provenance(archive, source, "0.1.0", target, commit, report)


if __name__ == "__main__":
    unittest.main()
