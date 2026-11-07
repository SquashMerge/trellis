#!/usr/bin/env python3
"""Flags flaky-test patterns in test code and points at the helper to use (#720).

Test code is everything under tests/ plus #[cfg(test)] items and modules in src/.
Patterns:

  store-open       Store::open / Store::open_for_tests* whose result is unwrapped
                   or expected. A test daemon (or other owner) can hold admission,
                   and typed storage_busy then panics the test (#712). Use
                   trellis::testing::retry_admission, which retries only the
                   CLI-read contention class.
  global-counter   A static atomic integer in test code or test-support code. A
                   process-global count also counts every other thread in the
                   process (#700). Scope the count to the thread, store or call
                   under test (#702).
  short-wait       A wall-clock deadline below HANG_BACKSTOP for a functional
                   wait: Instant::now() + Duration::from_secs/millis(N), or a
                   tokio::time::timeout(Duration::..(N), ..). Use
                   trellis::testing::wait_until, the settled helper
                   (tests/support/child.rs `settled`, or
                   CheckoutRuntime::await_settled_for_tests), or HANG_BACKSTOP.
  blocking-accept  A socket accept() / incoming() that blocks with no bound, so a
                   hook that never connects hangs the test until nextest kills
                   it (#742). Use trellis::testing::accept_within_backstop.
                   An accept inside wait_until(..) or a (tokio) timeout(..), one
                   that asserts nothing is pending (unwrap_err / is_err), and one
                   in a function that makes the listener non-blocking or bounds it
                   with poll(2), is not flagged.
  speed-assert     An assert! whose condition bounds elapsed time from above
                   (`x.elapsed() < N`, or `t < N` where `let t = x.elapsed()..`)
                   by anything but HANG_BACKSTOP. Test bounds are hang backstops
                   only (owner ruling); assert the outcome, and gate performance
                   separately.
  short-read-timeout
                   set_read_timeout(Some(Duration::from_secs/millis(N))) below
                   HANG_BACKSTOP. A short read window either flakes under load or
                   proves absence only by waiting; bound the read by HANG_BACKSTOP
                   and prove absence with a later reply.

Exceptions live in tools/flaky-patterns.allow, one per line:

  <pattern> <path> <fn-name|*> -- <one-line reason>

<fn-name> is the innermost named `fn` around the site; `*` covers the whole file.
Every entry needs a reason, and an entry that matches nothing fails the check, so
the list cannot go stale.

Usage: tools/flaky-patterns.py [--root DIR] [--allow FILE]
"""
import argparse
import re
import sys
from pathlib import Path

PATTERNS = {
    "store-open": "retry typed admission contention: trellis::testing::retry_admission (#712)",
    "global-counter": "scope the count to the thread, store or call under test, not the process (#700, #702)",
    "short-wait": "wait with trellis::testing::wait_until, the settled helper, or HANG_BACKSTOP (#720)",
    "blocking-accept": "bound the accept: trellis::testing::accept_within_backstop (#742)",
    "speed-assert": "assert the outcome; bound elapsed time only by HANG_BACKSTOP (#755)",
    "short-read-timeout": "bound the read by HANG_BACKSTOP; prove absence with a later reply (#755)",
}
# HANG_BACKSTOP in src/testing.rs, in milliseconds.
BACKSTOP_MS = 120_000


def code_only(text):
    """`text` with comments and string/char literal contents blanked (newlines kept),
    so offsets still line up with the original."""
    out = list(text)
    n = len(text)
    i = 0

    def blank(a, b):
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif c in "rb" and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            m = re.match(r'b?r(#*)"', text[i:])
            if m:
                close = '"' + m.group(1)
                j = text.find(close, i + m.end())
                j = n if j < 0 else j + len(close)
                blank(i + m.end(), j - len(close))
                i = j
            elif text.startswith('b"', i) or text.startswith("b'", i):
                i += 1
            else:
                i += 1
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            blank(i + 1, j)
            i = j + 1
        elif c == "'":
            if i + 1 < n and text[i + 1] == "\\":
                j = text.find("'", i + 2)
                j = n if j < 0 else j
                blank(i + 1, j)
                i = j + 1
            elif i + 2 < n and text[i + 2] == "'":
                blank(i + 1, i + 2)
                i += 3
            else:
                i += 1  # a lifetime
        else:
            i += 1
    return "".join(out)


def matching(code, open_at):
    """Offset just past the bracket that closes the one at `open_at`."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = [pairs[code[open_at]]]
    i = open_at + 1
    while i < len(code) and stack:
        c = code[i]
        if c in pairs:
            stack.append(pairs[c])
        elif stack and c == stack[-1]:
            stack.pop()
        i += 1
    return i


CFG = re.compile(r"#\[cfg\(\s*(.*?)\)\]\s*$")
ATTR = re.compile(r"#!?\[")


def cfg_kind(expr):
    """'test' for test-only cfgs, 'support' for test-support ones, else None."""
    expr = re.sub(r"\s+", "", expr)
    if expr in ("test", "any(test)") or re.fullmatch(r"all\((.*,)?test(,.*)?\)", expr):
        return "test"
    if "not(" in expr:
        return None
    if 'feature="test-support"' in expr:
        return "support"
    return None


class Source:
    def __init__(self, path, rel, whole=None):
        self.path, self.rel = path, rel
        self.text = path.read_text(encoding="utf-8")
        self.code = code_only(self.text)
        self.whole = whole  # 'test', 'support' or None: the whole file's kind
        self.regions = []  # (start, end, kind)
        self.children = []  # (Path, kind) modules this file gates
        self._fns = None
        self.line_starts = [0] + [m.end() for m in re.finditer("\n", self.text)]
        self._scan_cfg()

    def _scan_cfg(self):
        for m in re.finditer(r"#\[cfg\(", self.code):
            end = matching(self.code, m.end() - 1)
            if end >= len(self.code) or self.code[end] != "]":
                continue
            kind = cfg_kind(self.text[m.end():end - 1])
            if kind is None:
                continue
            # Skip whitespace and further attributes to the item.
            i = end + 1
            path_attr = None
            while True:
                while i < len(self.code) and self.code[i].isspace():
                    i += 1
                if ATTR.match(self.code, i):
                    close = matching(self.code, self.code.index("[", i))
                    pm = re.match(r'#\[path\s*=\s*"([^"]*)"\]', self.text[i:close])
                    if pm:
                        path_attr = pm.group(1)
                    i = close
                    continue
                break
            j, depth = i, 0
            while j < len(self.code):
                c = self.code[j]
                if c in "([":
                    depth += 1
                elif c in ")]":
                    depth -= 1
                elif depth == 0 and c in "{;":
                    break
                j += 1
            if j >= len(self.code):
                continue
            if self.code[j] == ";":
                mm = re.search(r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*$", self.code[i:j])
                if mm:
                    self.children.append((self.module_path(mm.group(1), path_attr), kind))
                self.regions.append((m.start(), j + 1, kind))
            else:
                self.regions.append((m.start(), matching(self.code, j), kind))

    def module_path(self, name, path_attr):
        base = self.path.parent
        if path_attr:
            return (base / path_attr).resolve()
        if self.path.name not in ("lib.rs", "main.rs", "mod.rs"):
            base = base / self.path.stem
        if (base / name / "mod.rs").exists() and not (base / f"{name}.rs").exists():
            return (base / name / "mod.rs").resolve()
        return (base / f"{name}.rs").resolve()

    def kind_at(self, offset):
        kinds = {k for a, b, k in self.regions if a <= offset < b} | {self.whole}
        return "test" if "test" in kinds else ("support" if "support" in kinds else None)

    def line(self, offset):
        lo, hi = 0, len(self.line_starts)
        while lo + 1 < hi:
            mid = (lo + hi) // 2
            if self.line_starts[mid] <= offset:
                lo = mid
            else:
                hi = mid
        return lo + 1

    def fn_spans(self):
        """(name, body start, body end) for every named fn with a body."""
        if self._fns is None:
            self._fns = []
            code = self.code
            for m in re.finditer(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)", code):
                j, depth = m.end(), 0
                while j < len(code):
                    c = code[j]
                    if c in "([<":
                        depth += 1
                    elif c in ")]>":
                        depth -= 1
                    elif c == ";" and depth <= 0:
                        break
                    elif c == "{" and depth <= 0:
                        self._fns.append((m.group(1), j, matching(code, j)))
                        break
                    j += 1
        return self._fns

    def enclosing_fn(self, offset):
        """The innermost named fn whose body contains `offset`, else '*'."""
        best, start = "*", -1
        for name, a, b in self.fn_spans():
            if a < offset < b and a > start:
                best, start = name, a
        return best

    def fn_body(self, offset):
        """The text of the innermost fn body before `offset`."""
        start = -1
        for _, a, b in self.fn_spans():
            if a < offset < b and a > start:
                start = a
        return self.code[max(start, 0):offset] if start >= 0 else ""


def duration_ms(unit, value):
    value = int(value.replace("_", ""))
    return value * 1000 if unit == "secs" else value


DURATION = r"Duration::from_(secs|millis)\(\s*([0-9_]+)\s*\)"
STORE_OPEN = re.compile(r"\bStore::(open|open_for_tests|open_for_tests_with_index_stage_hook)\s*\(")
COUNTER = re.compile(
    r"\bstatic\s+(?:mut\s+)?([A-Z_][A-Z0-9_]*)\s*:\s*(?:[a-z_:]*::)?Atomic(?:Usize|Isize|U8|U16|U32|U64|I8|I16|I32|I64)\b"
)
DEADLINE = re.compile(r"Instant::now\(\)\s*\+\s*(?:std::time::)?" + DURATION)
TOKIO_TIMEOUT = re.compile(r"\btimeout\(\s*(?:std::time::)?" + DURATION + r"\s*,")
ACCEPT = re.compile(r"\.(accept|incoming)\(\s*\)")
ASSERT = re.compile(r"\b(?:debug_)?assert!\s*\(")
ELAPSED = r"\.elapsed\(\)(?:\.as_[a-z0-9_]+\(\))?"
# An upper bound on elapsed time: `<` or `<=`, never `<<` or a `<-` arrow.
UPPER = r"\s*<=?(?![<-])\s*"
READ_TIMEOUT = re.compile(r"\.set_read_timeout\(\s*Some\(\s*(?:std::time::)?" + DURATION + r"\s*\)")


def first_argument(code, open_at):
    """The text of a macro call's first top-level argument."""
    end = matching(code, open_at) - 1
    depth, i = 0, open_at + 1
    while i < end:
        c = code[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            break
        i += 1
    return code[open_at + 1:i]


def speed_bound(condition, body):
    """Whether an assert condition bounds elapsed time above by anything but HANG_BACKSTOP."""
    for m in re.finditer(ELAPSED + UPPER, condition):
        if "HANG_BACKSTOP" not in condition[m.end():]:
            return True
    for m in re.finditer(r"\b([a-z_][a-z0-9_]*)" + UPPER, condition):
        name = m.group(1)
        bound = re.search(r"\blet\s+(?:mut\s+)?" + name + r"\s*=[^;]*" + ELAPSED + r"\s*;", body)
        if bound and "HANG_BACKSTOP" not in condition[m.end():]:
            return True
    return False


def findings(src):
    code = src.code
    found = []

    def add(pattern, offset):
        found.append((pattern, src.rel, src.enclosing_fn(offset), src.line(offset)))

    for m in STORE_OPEN.finditer(code):
        if src.kind_at(m.start()) != "test":
            continue
        tail = code[matching(code, m.end() - 1):].lstrip()
        if tail.startswith(".unwrap()") or tail.startswith(".expect("):
            add("store-open", m.start())
    for m in COUNTER.finditer(code):
        if src.kind_at(m.start()) in ("test", "support"):
            add("global-counter", m.start())
    for regex in (DEADLINE, TOKIO_TIMEOUT):
        for m in regex.finditer(code):
            if src.kind_at(m.start()) == "test" and duration_ms(m.group(1), m.group(2)) < BACKSTOP_MS:
                add("short-wait", m.start())
    # A wait_until(..) poll or a (tokio) timeout(..) around the accept bounds it.
    bounds = [m.end() - 1 for m in re.finditer(r"\b(?:wait_until|timeout|timeout_at)\s*\(", code)]
    for m in ACCEPT.finditer(code):
        if src.kind_at(m.start()) != "test":
            continue
        if any(b < m.start() < matching(code, b) for b in bounds):
            continue
        # Asserting that nothing is pending, on a non-blocking listener.
        if re.match(r"\s*\.(?:unwrap_err|is_err|is_err_and)\b", code[m.end():]):
            continue
        # A non-blocking listener, or one bounded with poll(2), never hangs.
        if re.search(r"\bpoll\s*\(|set_nonblocking\(\s*true\s*\)", src.fn_body(m.start())):
            continue
        add("blocking-accept", m.start())
    for m in ASSERT.finditer(code):
        if src.kind_at(m.start()) != "test":
            continue
        if speed_bound(first_argument(code, m.end() - 1), src.fn_body(m.start())):
            add("speed-assert", m.start())
    for m in READ_TIMEOUT.finditer(code):
        if src.kind_at(m.start()) == "test" and duration_ms(m.group(1), m.group(2)) < BACKSTOP_MS:
            add("short-read-timeout", m.start())
    return found


def sources(root):
    out = {}
    for path in sorted((root / "tests").rglob("*.rs")):
        out[path.resolve()] = Source(path, path.relative_to(root).as_posix(), whole="test")
    pending = sorted((root / "src").rglob("*.rs"))
    for path in pending:
        rp = path.resolve()
        if rp not in out:
            out[rp] = Source(path, path.relative_to(root).as_posix())
    # A file gated by its parent's `#[cfg(test)] mod x;` is test code throughout.
    changed = True
    while changed:
        changed = False
        for src in list(out.values()):
            for child, kind in src.children:
                c = out.get(child)
                if c is None or c.whole == "test" or (kind == "support" and c.whole == "support"):
                    continue
                c.whole = "test" if kind == "test" or src.whole == "test" else "support"
                changed = True
    return list(out.values())


def load_allow(path):
    entries, errors = [], []
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        head, sep, reason = line.partition(" -- ")
        fields = head.split()
        if not sep or not reason.strip():
            errors.append(f"{path.name}:{number}: an exception needs ' -- <reason>'")
        elif len(fields) != 3 or fields[0] not in PATTERNS:
            errors.append(f"{path.name}:{number}: expected '<pattern> <path> <fn|*> -- <reason>'")
        else:
            entries.append((tuple(fields), number))
    return entries, errors


def check(root, allow_path):
    entries, errors = load_allow(allow_path)
    used = set()
    flagged = []
    for src in sources(root):
        for pattern, rel, fn, line in findings(src):
            hit = [n for (p, f, name), n in entries if p == pattern and f == rel and name in (fn, "*")]
            if hit:
                used.update(hit)
            else:
                flagged.append(f"{rel}:{line}: {pattern} in fn {fn}: {PATTERNS[pattern]}")
    for (pattern, rel, fn), number in entries:
        if number not in used:
            errors.append(f"{allow_path.name}:{number}: stale exception ({pattern} {rel} {fn}) matches nothing")
    return flagged, errors


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    root = Path(__file__).resolve().parent.parent
    parser.add_argument("--root", type=Path, default=root)
    parser.add_argument("--allow", type=Path, default=root / "tools" / "flaky-patterns.allow")
    args = parser.parse_args(argv)
    flagged, errors = check(args.root, args.allow)
    for line in flagged + errors:
        print(line)
    if flagged or errors:
        print(f"flaky-patterns: {len(flagged)} site(s), {len(errors)} allowlist error(s); "
              f"fix them, or add '<pattern> <path> <fn|*> -- <reason>' to {args.allow.name}", file=sys.stderr)
        return 1
    print("flaky-patterns: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
