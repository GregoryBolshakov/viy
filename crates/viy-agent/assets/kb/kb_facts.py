#!/usr/bin/env python3
"""
kb_facts.py - wording and key-hygiene checks that tools/kb_lint.py cannot make.

kb_lint.py enforces the frontmatter contract. This catches the ways a fact can be
contract-valid and still useless to the contradiction check:

  split keys          'list-price' next to 'list_price' - never compared
  false contradiction '49 USD/month' vs '49 usd/month' - flagged as a conflict
  relative time       'currently', 'latest' - true when written, false when read
  non-atomic object   hedges, long values, '$49/mo for the starter tier'

Usage (from the KB root, or pass --root):
    kb_facts.py keys     list every (subject, relation) in use, to reuse spellings
    kb_facts.py check    HARD and SOFT findings; always exits 0
    kb_facts.py check --strict          exit 1 on HARD findings (pre-commit uses this)
    kb_facts.py check --file kb/x.md    only findings that involve x.md (hooks use this)

Stdlib only. Python 3.8+. Kept in sync with the copy kb_init.py embeds in tools/.
"""

import argparse
import collections
import pathlib
import re
import sys

FM_LINE = re.compile(r"^([a-z_]+):\s*(.*)$")
SUBJECT_RE = re.compile(r"^[a-z0-9]+(-[a-z0-9]+)*$")
RELATION_RE = re.compile(r"^[a-z0-9]+(_[a-z0-9]+)*$")
RELATIVE = re.compile(
    r"\b(currently|recently|nowadays|right now|at the moment|as of now|"
    r"today|yesterday|tomorrow|soon|latest|newest|"
    r"(this|last|next) (week|month|quarter|year))\b", re.I)
HEDGE = re.compile(r"(~|\b(approx\w*|about|around|roughly|probably|maybe|likely|"
                   r"or so|circa|some)\b)", re.I)
OBJECT_MAX = 60
BODY_SECTIONS = ("## Evidence", "## Method")


def parse(path):
    text = path.read_text(encoding="utf-8")
    if not text.startswith("---"):
        return None, text
    end = text.find("\n---", 3)
    if end == -1:
        return None, text
    meta = {}
    for line in text[4:end].splitlines():
        m = FM_LINE.match(line.rstrip())
        if not m:
            continue
        key, val = m.group(1), m.group(2).strip()
        if val.lower() in ("null", "~", ""):
            val = None
        elif len(val) > 1 and val[0] in "'\"" and val[-1] in "'\"":
            val = val[1:-1]
        meta[key] = val
    return meta, text[end + 4:]


def prose_lines(body):
    """Body lines outside code fences and verbatim quotes."""
    fenced = False
    for n, line in enumerate(body.splitlines(), 1):
        if line.lstrip().startswith("```"):
            fenced = not fenced
            continue
        if fenced or line.lstrip().startswith(">"):
            continue
        # Text inside "..." on an Evidence line is a quote; its words are the source's.
        yield n, re.sub(r'"[^"]*"', '""', line)


def squash(s):
    return re.sub(r"[^a-z0-9]", "", (s or "").lower())


def load(root):
    docs = []
    kb = root / "kb"
    if not kb.exists():
        sys.exit("no kb/ under %s (run from the KB root or pass --root)" % root)
    for path in sorted(kb.rglob("*.md")):
        meta, body = parse(path)
        if meta and meta.get("subject") and meta.get("relation"):
            docs.append((path.relative_to(root), meta, body))
    return docs


def cmd_keys(docs):
    rows = collections.defaultdict(list)
    for path, m, _ in docs:
        rows[(m["subject"], m["relation"])].append("%s=%s" % (m.get("status"), m.get("id")))
    for (s, r), ids in sorted(rows.items()):
        print("%-32s %-36s %s" % (s, r, ", ".join(ids)))
    return 0


def cmd_check(docs, strict=False, quiet=False, only=None):
    """HARD findings break the conflict check or make a fact wrong over time;
    SOFT ones make it harder to re-verify. --strict exits 1 on HARD only.
    --file limits output to findings that involve that one file."""
    out = []

    def add(level, paths, msg):
        out.append((level, paths, msg))

    for path, m, body in docs:
        if m["subject"] in ("kb", "kb-system"):
            continue  # scaffolding docs, not claims
        if not SUBJECT_RE.match(m["subject"]):
            add("HARD", [path], "subject '%s' is not lowercase kebab-case" % m["subject"])
        if not RELATION_RE.match(m["relation"]):
            add("HARD", [path], "relation '%s' is not lowercase snake_case" % m["relation"])

        obj = m.get("object") or ""
        if RELATIVE.search(obj):
            add("HARD", [path], "object uses relative time '%s'" % RELATIVE.search(obj).group(0))
        if len(obj) > OBJECT_MAX:
            add("SOFT", [path], "object is %d chars; one canonical value, detail goes in the body" % len(obj))
        if HEDGE.search(obj):
            add("SOFT", [path], "object hedges ('%s'); uncertainty belongs in confidence" % HEDGE.search(obj).group(0))
        if obj.startswith("$"):
            add("SOFT", [path], "object '%s': write currency as '<n> USD'" % obj)

        for n, line in prose_lines(body):
            hit = RELATIVE.search(line)
            if hit:
                add("SOFT", [path], "body line %d: relative time '%s' - write the date or version" % (n, hit.group(0)))

        # Facts must be re-checkable; strategy and decision docs have their own shape.
        if m.get("status") == "current" and path.parts[:2] == ("kb", "facts"):
            missing = [s for s in BODY_SECTIONS if s not in body]
            if missing:
                add("SOFT", [path], "body lacks %s - a future agent cannot re-check this" % ", ".join(missing))

    # Split keys: spellings that the linter treats as different but mean the same.
    for field in ("subject", "relation"):
        groups = collections.defaultdict(dict)
        for path, m, _ in docs:
            groups[squash(m[field])].setdefault(m[field], []).append(path)
        for spellings in groups.values():
            if len(spellings) > 1:
                add("HARD", [p for ps in spellings.values() for p in ps],
                    "%s spelled %d ways: %s - pick one, or the conflict check never fires"
                    % (field, len(spellings), ", ".join(sorted(spellings))))

    # False contradictions: different strings, same value once normalized.
    live = collections.defaultdict(list)
    for path, m, _ in docs:
        if m.get("status") == "current":
            live[(m["subject"].lower(), m["relation"].lower())].append((m.get("id"), m.get("object") or "", path))
    for (s, r), entries in sorted(live.items()):
        objs = {o for _, o, _ in entries}
        if len(objs) > 1 and len({squash(o) for o in objs}) == 1:
            add("HARD", [p for _, _, p in entries],
                "(%s, %s) objects differ only in spelling: %s - canonicalize, it is not a real conflict"
                % (s, r, "; ".join("%s='%s'" % (i, o) for i, o, _ in entries)))

    if only is not None:
        out = [f for f in out if only in f[1]]
    hard = [f for f in out if f[0] == "HARD"]
    for level, paths, msg in (hard if quiet else out):
        print("%-4s  %s\n      %s" % (level, ", ".join(str(p) for p in paths), msg))
    if hard or not quiet:
        print("\n%d facts | %d hard | %d soft" % (len(docs), len(hard), len(out) - len(hard)))
    return 1 if strict and hard else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("command", choices=["keys", "check"])
    ap.add_argument("--root", default=".", help="KB root (contains kb/)")
    ap.add_argument("--strict", action="store_true", help="exit 1 on HARD findings")
    ap.add_argument("--quiet", action="store_true", help="HARD findings only")
    ap.add_argument("--file", default=None, help="only findings involving this file")
    args = ap.parse_args()
    root = pathlib.Path(args.root).resolve()
    docs = load(root)
    if args.command == "keys":
        return cmd_keys(docs)
    only = None
    if args.file:
        only = pathlib.Path(args.file).resolve().relative_to(root)
    return cmd_check(docs, args.strict, args.quiet, only)


if __name__ == "__main__":
    sys.exit(main())
