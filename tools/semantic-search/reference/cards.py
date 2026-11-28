"""Build symbol cards from a Trellis index.db (read-only).

usage: python -I cards.py <index.db> <out.jsonl>

Reads the head revision's declarations, headers, parameters, calls and captured
source bytes. Native extraction does not store doc comments, string literals or
body type names today; those three are approximated with a small lexical scan
of the captured bytes (marked "approx" below) as a stand-in for the tree-sitter
captures an implementation would add to extraction.
"""
import json
import re
import sqlite3
import sys

CONTAINER_KINDS = {"type", "implementation", "namespace", "module"}
# Candidate declarations: callables and types. Parameters, local variables, fields,
# aliases and anonymous functions are folded into their nearest candidate owner.
CANDIDATE_KINDS = {"function", "method", "type", "implementation", "namespace", "module"}
CAP_CALLS = 40
CAP_TYPES = 30
CAP_STRINGS = 20
CAP_STRING_LEN = 100
CAP_ERRORS = 15
CAP_MEMBERS = 40
CAP_DOC = 600
CAP_HEAD = 1200

STR_RE = re.compile(rb'"((?:[^"\\\n]|\\.){2,200})"')
JS_STR_RE = re.compile(rb"'((?:[^'\\\n]|\\.){2,200})'|`((?:[^`\\]|\\.){2,200})`")
TYPE_RE = re.compile(rb"\b[A-Z][A-Za-z0-9]*[a-z][A-Za-z0-9]*\b")
CODE_RE = re.compile(r"\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b")
IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
# approx: Rust extraction stores impl blocks without a name; recover "Trait for Type" / "Type"
# from the impl header line for the card's qualified name.
IMPL_RE = re.compile(rb"^\s*(?:unsafe\s+)?impl\b(?:\s*<[^{]*?>)?\s+(?:([A-Za-z_][\w:]*)(?:<[^{]*?>)?\s+for\s+)?&?(?:'\w+\s+)?(?:mut\s+)?([A-Za-z_][\w:]*)")


def split_ident(tok):
    parts = []
    for p in tok.split("_"):
        parts += re.findall(r"[A-Z]+(?=[A-Z][a-z]|[0-9]|$)|[A-Z]?[a-z]+|[A-Z]+|[0-9]+", p)
    return [p.lower() for p in parts if p]


def split_text(text):
    out = []
    for tok in IDENT_RE.findall(text):
        sp = split_ident(tok)
        if len(sp) > 1:
            out.extend(sp)
    return " ".join(out)


def doc_comment(src, start):
    """approx: comment/attribute lines immediately above the declaration."""
    line_start = src.rfind(b"\n", 0, start) + 1
    lines = []
    pos = line_start
    while pos > 0:
        prev_end = pos - 1
        prev_start = src.rfind(b"\n", 0, prev_end) + 1
        raw = src[prev_start:prev_end].strip()
        if raw.startswith((b"///", b"//!", b"//", b"/**", b"/*", b"*", b"*/")):
            text = raw.lstrip(b"/!*").strip()
            if text:
                lines.append(text)
            pos = prev_start
        elif raw.startswith((b"#[", b"#![", b"@")):
            pos = prev_start
        else:
            break
    lines.reverse()
    return b" ".join(lines).decode("utf-8", "replace")[:CAP_DOC]


def uniq(seq, cap):
    seen, out = set(), []
    for x in seq:
        if x and x not in seen:
            seen.add(x)
            out.append(x)
            if len(out) >= cap:
                break
    return out


def own_bytes(src, d, nested):
    """Declaration bytes minus nested declarations' ranges."""
    pieces, cur = [], d["start"]
    for s, e in sorted(nested):
        if s >= cur:
            pieces.append(src[cur:s])
            cur = max(cur, e)
    pieces.append(src[cur:d["end"]])
    return b"\n".join(pieces)


def main(db_path, out_path):
    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    rev_id, pin = db.execute(
        "SELECT id, published_index_revision FROM native_revisions ORDER BY published_index_revision DESC LIMIT 1"
    ).fetchone()
    docs = db.execute(
        "SELECT rd.path, rd.language, rd.document_version_id, dv.source_bytes FROM revision_documents rd "
        "JOIN document_versions dv ON dv.id = rd.document_version_id WHERE rd.revision_id = ? ORDER BY rd.ordinal",
        (rev_id,),
    ).fetchall()
    n = 0
    with open(out_path, "w") as out:
        for path, language, vid, src in docs:
            decls = {}
            for sid, owner, kind, role, name, s, e in db.execute(
                "SELECT syntax_id, owner_syntax_id, kind, role, name, start_byte, end_byte "
                "FROM native_version_declarations WHERE version_id = ?",
                (vid,),
            ):
                decls[sid] = dict(id=sid, owner=owner, kind=kind, role=role, name=name, start=s, end=e,
                                  ancestors=[], header=None, items=[], params=[], calls=[], children=[])
            for sid, ordinal, kind, name, sib in db.execute(
                "SELECT syntax_id, ordinal, kind, name, sibling_ordinal FROM native_version_declaration_ancestors "
                "WHERE version_id = ? ORDER BY syntax_id, ordinal", (vid,)):
                decls[sid]["ancestors"].append((kind, name, sib))
            for sid, kind, name, result in db.execute(
                "SELECT syntax_id, kind, name, result_type FROM native_version_headers WHERE version_id = ?", (vid,)):
                decls[sid]["header"] = (kind, name, result)
            for sid, item_kind, value in db.execute(
                "SELECT syntax_id, item_kind, value FROM native_version_header_items WHERE version_id = ? "
                "ORDER BY syntax_id, item_kind, ordinal", (vid,)):
                decls[sid]["items"].append((item_kind, value))
            for sid, pname, ptype, variadic in db.execute(
                "SELECT syntax_id, name, type_name, variadic FROM native_version_parameters WHERE version_id = ? "
                "ORDER BY syntax_id, ordinal", (vid,)):
                decls[sid]["params"].append((pname, ptype, variadic))
            for owner, spelling in db.execute(
                "SELECT owner_syntax_id, spelling FROM native_version_calls WHERE version_id = ? "
                "ORDER BY owner_syntax_id, ordinal", (vid,)):
                if owner in decls and spelling:
                    decls[owner]["calls"].append(spelling)
            def candidate_owner(d):
                o = decls.get(d["owner"])
                while o is not None and o["kind"] not in CANDIDATE_KINDS:
                    o = decls.get(o["owner"])
                return o
            for d in decls.values():
                o = candidate_owner(d)
                if o is None:
                    continue
                if d["kind"] in CANDIDATE_KINDS:
                    o["children"].append(d)
                else:
                    o["calls"].extend(d["calls"])
            for d in decls.values():
                if d["name"] is None or d["kind"] not in CANDIDATE_KINDS:
                    continue
                # #24 displayKey rule: an unnamed ancestor segment is kind@siblingOrdinal.
                anc = [a[1] if a[1] is not None else f"{a[0]}@{a[2]}" for a in d["ancestors"]]
                qualified = ".".join(anc + [d["name"]])
                # Card text names impl owners by type (approx); `qualified` keeps the #24 displayKey rule.
                card_anc = []
                o, chain = decls.get(d["owner"]), []
                while o is not None:
                    chain.append(o)
                    o = decls.get(o["owner"])
                for o in reversed(chain):
                    if o["name"] is not None:
                        card_anc.append(o["name"])
                    elif o["kind"] == "implementation":
                        m = IMPL_RE.match(src[o["start"]:o["end"]][:300])
                        if m:
                            card_anc.append(m.group(2).decode().split("::")[-1] + (f" ({m.group(1).decode()})" if m.group(1) else ""))
                card_name = "::".join(card_anc + [d["name"]])
                display = f"{path}#{qualified}"
                mods = [v for k, v in d["items"] if k == "modifier"]
                tparams = [v for k, v in d["items"] if k == "typeParameter"]
                bases = [v for k, v in d["items"] if k == "base"]
                params = ", ".join(
                    (("..." if v else "") + (p or "_") + (f": {t}" if t else "")) for p, t, v in d["params"])
                if d["header"] is not None:
                    sig = " ".join(mods + [d["kind"], d["name"]])
                    if tparams:
                        sig += "<" + ", ".join(tparams) + ">"
                    if d["kind"] not in CONTAINER_KINDS or d["params"]:
                        sig += f"({params})"
                    if d["header"][2]:
                        sig += f" -> {d['header'][2]}"
                    if bases:
                        sig += " : " + ", ".join(bases)
                else:
                    first = src[d["start"]:d["end"]].split(b"\n", 1)[0][:200]
                    sig = first.decode("utf-8", "replace").strip()
                doc = doc_comment(src, d["start"])
                nested = [(c["start"], c["end"]) for c in d["children"]]
                body = own_bytes(src, d, nested)
                strings = [m.decode("utf-8", "replace")[:CAP_STRING_LEN] for m in STR_RE.findall(body)]
                if language == "javascript":
                    for a, b in JS_STR_RE.findall(body):
                        strings.append((a or b).decode("utf-8", "replace")[:CAP_STRING_LEN])
                strings = uniq(strings, CAP_STRINGS)
                errors = uniq([c for s in strings for c in CODE_RE.findall(s)], CAP_ERRORS)
                sig_types = [t for _, t, _ in d["params"] if t] + ([d["header"][2]] if d["header"] and d["header"][2] else [])
                types = uniq(sig_types + [t.decode() for t in TYPE_RE.findall(body)], CAP_TYPES)
                calls = uniq([re.sub(r"\s+", "", c)[:80] for c in d["calls"]], CAP_CALLS)
                members = uniq([c["name"] for c in sorted(d["children"], key=lambda c: c["start"]) if c["name"]], CAP_MEMBERS)
                head = re.sub(rb"[ \t]+", b" ", src[d["start"]:d["end"]][:CAP_HEAD]).decode("utf-8", "replace")

                card_a = [f"{d['kind']} {card_name}", f"path: {path}", f"language: {language}", f"signature: {sig}"]
                if doc:
                    card_a.append(f"doc: {doc}")
                facts = []
                if calls:
                    facts.append("calls: " + ", ".join(calls))
                if types:
                    facts.append("types: " + ", ".join(types))
                if strings:
                    facts.append("strings: " + " | ".join(strings))
                if errors:
                    facts.append("error codes: " + ", ".join(errors))
                if members:
                    facts.append("members: " + ", ".join(members))
                a = "\n".join(card_a)
                b = a + ("\n" + "\n".join(facts) if facts else "")
                c = b + "\nbody:\n" + head
                rec = dict(id=d["id"], path=path, language=language, kind=d["kind"], role=d["role"], name=d["name"],
                           qualified=qualified, display=display, start=d["start"], end=d["end"],
                           cards={"a": a, "b": b, "c": c},
                           bm25={k: v + "\n" + split_text(v) for k, v in {"a": a, "b": b, "c": c}.items()})
                out.write(json.dumps(rec) + "\n")
                n += 1
    print(json.dumps({"revision": rev_id, "indexRevision": pin, "documents": len(docs), "cards": n}))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
