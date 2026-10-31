#!/usr/bin/env python3
"""Check CI's Linux nextest partitions (#495) lose no test.

split ALL PART... : the partition lists (`cargo nextest list --message-format
    oneline`, one `binary-id test-name` per line) are disjoint and together
    equal the full list.
junit JUNIT PART ALL : the partition job's JUnit report names exactly the
    tests in its partition list.
"""
import sys
import xml.etree.ElementTree as ET


def read_list(path):
    with open(path, encoding="utf-8") as f:
        tests = [line.rstrip("\n") for line in f if line.strip()]
    unique = set(tests)
    if len(unique) != len(tests):
        sys.exit(f"{path}: lists a test more than once")
    return unique


def show(label, tests, limit=20):
    for test in sorted(tests)[:limit]:
        print(f"  {label}: {test}", file=sys.stderr)
    if len(tests) > limit:
        print(f"  ... and {len(tests) - limit} more", file=sys.stderr)


def split(all_path, part_paths):
    full = read_list(all_path)
    if not full:
        sys.exit(f"{all_path}: no tests listed")
    seen = set()
    ok = True
    for index, path in enumerate(part_paths, 1):
        part = read_list(path)
        overlap = part & seen
        if overlap:
            print(f"partition {index} repeats tests of an earlier partition:", file=sys.stderr)
            show("repeated", overlap)
            ok = False
        seen |= part
        print(f"- partition {index}/{len(part_paths)}: {len(part)} tests")
    missing, extra = full - seen, seen - full
    if missing:
        print("tests in no partition:", file=sys.stderr)
        show("missing", missing)
        ok = False
    if extra:
        print("partition tests absent from the full list:", file=sys.stderr)
        show("extra", extra)
        ok = False
    if not ok:
        sys.exit(1)
    print(f"- all partitions: {len(seen)} tests, the full list of {len(full)}")


def junit(junit_path, part_path, all_path):
    expected = read_list(part_path)
    full = read_list(all_path)
    ran = set()
    for suite in ET.parse(junit_path).getroot().iter("testsuite"):
        for case in suite.iter("testcase"):
            ran.add(f"{suite.get('name')} {case.get('name')}")
    missing, extra = expected - ran, ran - expected
    if missing:
        print("partition tests that did not run:", file=sys.stderr)
        show("missing", missing)
    if extra:
        print("tests that ran outside the partition list:", file=sys.stderr)
        show("extra", extra)
    if missing or extra:
        sys.exit(1)
    print(f"- this partition ran {len(ran)} of the {len(full)} tests, exactly its share")


def main(argv):
    if len(argv) >= 3 and argv[0] == "split":
        split(argv[1], argv[2:])
    elif len(argv) == 4 and argv[0] == "junit":
        junit(*argv[1:])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
