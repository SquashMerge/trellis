#!/usr/bin/env python3
"""Package release bytes, record smoke-verified provenance, and audit Linux ELF imports."""
import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile


TARGETS = {
    "x86_64-unknown-linux-gnu": ("Advanced Micro Devices X86-64", "/lib64/ld-linux-x86-64.so.2"),
    "aarch64-unknown-linux-gnu": ("AArch64", "/lib/ld-linux-aarch64.so.1"),
    "aarch64-apple-darwin": None,
}
GLIBC_BASELINE = (2, 31)
ALLOWED_NEEDED = {
    "libc.so.6", "libm.so.6", "libdl.so.2", "libpthread.so.0", "librt.so.1", "libgcc_s.so.1",
}
PELE_ASSET = "web/vendor/pele-1.10.0-trellis.2.js"


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def archive_name(version, target):
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("version must be a semantic release version without the v prefix")
    if target not in TARGETS:
        raise ValueError("unsupported release target: " + target)
    return "trellis-{}-{}.tar.gz".format(version, target)


def package(binary, license_path, version, target, epoch, output_dir):
    name = archive_name(version, target)
    if not 0 <= epoch <= 0xffffffff:
        raise ValueError("source date epoch must fit the gzip timestamp")
    output_dir.mkdir(parents=True, exist_ok=True)
    archive = output_dir / name
    # Explicit metadata excludes filesystem ownership, times and permissions.
    with archive.open("wb") as output:
        with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=epoch, compresslevel=9) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as tar:
                for path, member, mode in ((binary, "trellis", 0o755), (license_path, "LICENSE", 0o644)):
                    data = path.read_bytes()
                    info = tarfile.TarInfo(member)
                    info.size, info.mode, info.mtime = len(data), mode, epoch
                    tar.addfile(info, io.BytesIO(data))
    return archive


def archived_files(archive):
    with tarfile.open(archive, "r:gz") as tar:
        members = tar.getmembers()
        if [member.name for member in members] != ["trellis", "LICENSE"] or not all(m.isfile() for m in members):
            raise ValueError("release archive must contain only trellis and LICENSE regular files")
        return {member.name: tar.extractfile(member).read() for member in members}


def tagged_inputs(source_root, commit):
    def git(*args):
        return subprocess.check_output(["git", "-C", str(source_root), *args])
    resolved = git("rev-parse", "--verify", commit + "^{commit}").decode().strip()
    if resolved != commit:
        raise ValueError("commit must be the full tagged commit hash")
    web_paths = git("ls-tree", "-r", "--name-only", "-z", commit, "--", "web/").decode().split("\0")
    paths = ["Cargo.lock", "Cargo.toml", "rust-toolchain.toml", "build.rs"] + [p for p in web_paths if p]
    dockerfile = "tools/release-linux.Dockerfile"
    if git("ls-tree", "--name-only", commit, "--", dockerfile).strip():
        paths.append(dockerfile)
    data = {path: git("show", commit + ":" + path) for path in sorted(paths)}
    if PELE_ASSET not in data:
        raise ValueError("tagged Pele asset missing")
    return data


def provenance(archive, source_root, version, target, commit, smoke_report):
    if archive.name != archive_name(version, target):
        raise ValueError("archive name does not match version and target")
    files = archived_files(archive)
    report = json.loads(smoke_report.read_text())
    build = report["build"]
    for key, expected in (("version", version), ("target", target), ("commit", commit)):
        if build[key] != expected:
            raise ValueError("binary build {} differs from release: {} != {}".format(key, build[key], expected))
    expected_features = ["metal"] if target.endswith("apple-darwin") else []
    if build["features"] != expected_features:
        raise ValueError("shipping features must be " + repr(expected_features))
    expected_capabilities = ["cpu", "metal"] if expected_features else ["cpu"]
    if build["embeddingCapabilities"] != expected_capabilities or not build["toolchain"]:
        raise ValueError("unexpected build capabilities or missing actual toolchain")
    binary_hash = sha256(files["trellis"])
    if report["binarySha256"] != binary_hash:
        raise ValueError("smoke report does not describe the archived binary")
    inputs = tagged_inputs(source_root, commit)
    if files["LICENSE"] != subprocess.check_output(["git", "-C", str(source_root), "show", commit + ":LICENSE"]):
        raise ValueError("archived LICENSE differs from tagged LICENSE")
    if report["embeddedPeleSha256"] != sha256(inputs[PELE_ASSET]):
        raise ValueError("smoked embedded Pele differs from tagged asset")
    digest = sha256(archive.read_bytes())
    result = {
        "schemaVersion": 1,
        "archive": archive.name,
        "archiveSha256": digest,
        "binarySha256": binary_hash,
        "licenseSha256": sha256(files["LICENSE"]),
        "build": build,
        "cargoLockSha256": sha256(inputs["Cargo.lock"]),
        "inputHashes": {path: sha256(data) for path, data in inputs.items()},
        "webAssetsSha256": {path: sha256(data) for path, data in inputs.items() if path.startswith("web/")},
        "smokeReportSha256": sha256(smoke_report.read_bytes()),
    }
    Path(str(archive) + ".sha256").write_text(digest + "  " + archive.name + "\n")
    Path(str(archive) + ".provenance.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return result


def validate_tag(source_root, tag, commit):
    def git(*args):
        return subprocess.check_output(["git", "-C", str(source_root), *args], text=True).strip()
    if git("rev-parse", "--verify", commit + "^{commit}") != commit:
        raise ValueError("commit must be the full tagged commit hash")
    if git("rev-parse", "HEAD") != commit or git("rev-parse", "--verify", "refs/tags/" + tag + "^{commit}") != commit:
        raise ValueError("checkout, tag and event commit must match")
    import tomllib
    version = tomllib.loads(git("show", commit + ":Cargo.toml"))["package"]["version"]
    archive_name(version, next(iter(TARGETS)))
    if tag != "v" + version:
        raise ValueError("release tag must be v<Cargo package version>: v" + version)
    return {"tag": tag, "version": version, "commit": commit}


def release_assets(artifact_dir, version, commit):
    expected_dirs = {"candidate-" + target for target in TARGETS}
    if {path.name for path in artifact_dir.iterdir()} != expected_dirs:
        raise ValueError("release requires exactly the three native candidate artifacts")
    assets = []
    for target in TARGETS:
        archive = artifact_dir / ("candidate-" + target) / archive_name(version, target)
        checksum = Path(str(archive) + ".sha256")
        provenance_path = Path(str(archive) + ".provenance.json")
        digest = sha256(archive.read_bytes())
        if checksum.read_text() != digest + "  " + archive.name + "\n":
            raise ValueError("release checksum mismatch: " + archive.name)
        recorded = json.loads(provenance_path.read_text())
        if (recorded["schemaVersion"] != 1 or recorded["archive"] != archive.name
                or recorded["archiveSha256"] != digest):
            raise ValueError("release provenance archive mismatch: " + archive.name)
        for key, expected in (("version", version), ("target", target), ("commit", commit)):
            if recorded["build"][key] != expected:
                raise ValueError("release provenance {} mismatch: {}".format(key, archive.name))
        assets.extend((archive, checksum, provenance_path))
    return assets


def parse_elf(header, program_headers, versions, dynamic, target):
    if target not in TARGETS or TARGETS[target] is None:
        raise ValueError("ELF audit requires a supported Linux target")
    machine, interpreter = TARGETS[target]
    fields = dict(re.findall(r"^\s*(Class|Data|Type|Machine):\s*(.*?)\s*$", header, re.M))
    if (fields.get("Class") != "ELF64" or fields.get("Machine") != machine
            or fields.get("Data") != "2's complement, little endian"
            or not re.match(r"(?:DYN|EXEC)\b", fields.get("Type", ""))):
        raise ValueError("unexpected ELF architecture/header: " + repr(fields))
    interpreters = re.findall(r"\[Requesting program interpreter: ([^]\n]+)\]", program_headers)
    if interpreters != [interpreter]:
        raise ValueError("unexpected ELF interpreter: " + repr(interpreters))
    needed_lines = [line for line in dynamic.splitlines() if "(NEEDED)" in line]
    needed = []
    for line in needed_lines:
        match = re.search(r"\(NEEDED\)\s+Shared library: \[([^]\n]+)\]\s*$", line)
        if not match:
            raise ValueError("malformed DT_NEEDED: " + line)
        needed.append(match.group(1))
    if not needed or "libc.so.6" not in needed:
        raise ValueError("expected dynamically linked glibc binary")
    allowed = ALLOWED_NEEDED | {Path(interpreter).name}
    unexpected = sorted(set(needed) - allowed)
    if unexpected:
        raise ValueError("unsupported DT_NEEDED dependencies: " + ", ".join(unexpected))
    sections = versions.split("Version needs section ")
    if len(sections) != 2:
        raise ValueError("missing or malformed ELF version needs section")
    needs_section = sections[1].split("\nVersion ", 1)[0]
    imports = re.findall(r"\bName:\s+(\S+)\s+Flags:", needs_section)
    name_lines = [line for line in needs_section.splitlines() if "Name:" in line]
    if not imports or len(name_lines) != len(imports):
        raise ValueError("malformed ELF version imports")
    glibc = []
    for name in imports:
        if name.startswith("GLIBC_"):
            if not re.fullmatch(r"GLIBC_[0-9]+(?:\.[0-9]+)+", name):
                raise ValueError("unsupported GLIBC import: " + name)
            version = tuple(int(part) for part in name[6:].split("."))
            while len(version) > 2 and version[-1] == 0:
                version = version[:-1]
            if version > GLIBC_BASELINE:
                raise ValueError("GLIBC import exceeds 2.31: " + name)
            glibc.append((version, name))
    if not glibc:
        raise ValueError("no GLIBC version imports found")
    return {
        "target": target, "machine": machine, "interpreter": interpreter,
        "needed": sorted(set(needed)), "glibcImports": sorted(set(name for _, name in glibc)),
        "maximumImportedGlibc": max(glibc)[1], "glibcImportCeiling": "2.31",
    }


def audit_elf(binary, target):
    def readelf(flag):
        # Stable English headings and one-line headers are part of the parser interface.
        import os
        return subprocess.check_output(["readelf", "--wide", flag, str(binary)],
                                       text=True, env=dict(os.environ, LC_ALL="C"))
    return parse_elf(readelf("-h"), readelf("-l"), readelf("-V"), readelf("-d"), target)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    pack = commands.add_parser("package")
    pack.add_argument("--binary", type=Path, required=True)
    pack.add_argument("--license", dest="license_path", type=Path, required=True)
    pack.add_argument("--version", required=True)
    pack.add_argument("--target", choices=TARGETS, required=True)
    pack.add_argument("--source-date-epoch", dest="epoch", type=int, required=True)
    pack.add_argument("--output-dir", type=Path, required=True)
    prov = commands.add_parser("provenance")
    prov.add_argument("--archive", type=Path, required=True)
    prov.add_argument("--source-root", type=Path, required=True)
    prov.add_argument("--version", required=True)
    prov.add_argument("--target", choices=TARGETS, required=True)
    prov.add_argument("--commit", required=True)
    prov.add_argument("--smoke-report", type=Path, required=True)
    audit = commands.add_parser("audit-elf")
    audit.add_argument("--binary", type=Path, required=True)
    audit.add_argument("--target", choices=TARGETS, required=True)
    tag = commands.add_parser("validate-tag")
    tag.add_argument("--source-root", type=Path, required=True)
    tag.add_argument("--tag", required=True)
    tag.add_argument("--commit", required=True)
    assets = commands.add_parser("assets")
    assets.add_argument("--artifact-dir", type=Path, required=True)
    assets.add_argument("--version", required=True)
    assets.add_argument("--commit", required=True)
    args = vars(parser.parse_args())
    command = args.pop("command")
    try:
        result = {"package": package, "provenance": provenance, "audit-elf": audit_elf,
                  "validate-tag": validate_tag, "assets": release_assets}[command](**args)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError, tarfile.TarError) as error:
        parser.exit(1, "release: {}\n".format(error))
    if isinstance(result, list):
        print("\n".join(str(path) for path in result))
    else:
        print(json.dumps(result, indent=2, sort_keys=True) if isinstance(result, dict) else str(result))


if __name__ == "__main__":
    main()
