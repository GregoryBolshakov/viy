#!/usr/bin/env python3
"""
kb_lint.py - deterministic freshness + contradiction checker for the KB.

Design rationale (see kb-system/design.md):
  No LLM runs on the read path. Every check here is a deterministic rule over
  explicit frontmatter fields. This is the "policy execution" stage that the
  research says must be separated from LLM evidence extraction.

Usage:
    python3 tools/kb_lint.py            # human output, exit 1 on ERROR
    python3 tools/kb_lint.py --json     # machine output
    python3 tools/kb_lint.py --quiet    # errors only

Stdlib only. Python 3.8+.
"""

import argparse
import datetime as dt
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
# kb-system docs are linted too: they carry TTLs and are cross-referenced from kb/.
SCAN_DIRS = [ROOT / "kb", ROOT / "kb-system"]

REQUIRED = ["id", "status", "recorded_at", "review_by", "confidence", "source_type"]
FACT_REQUIRED = ["subject", "relation", "object"]

VALID_STATUS = {"current", "superseded", "disputed", "retired"}
VALID_CONF = {"high", "medium", "low"}
VALID_SRC = {"primary", "vendor-marketing", "community", "inference", "self"}

WIKILINK = re.compile(r"\[\[([^\]|]+)")
FM_LINE = re.compile(r"^([a-z_]+):\s*(.*)$")


def parse_frontmatter(path):
    """Minimal, dependency-free frontmatter parser. Flat key: value only."""
    text = path.read_text(encoding="utf-8")
    if not text.startswith("---"):
        return None, text
    end = text.find("\n---", 3)
    if end == -1:
        return None, text
    raw, body = text[4:end], text[end + 4:]
    meta = {}
    for line in raw.splitlines():
        line = line.rstrip()
        if not line or line.lstrip().startswith("#"):
            continue
        m = FM_LINE.match(line)
        if not m:
            continue
        key, val = m.group(1), m.group(2).strip()
        if val.lower() in ("null", "~", ""):
            val = None
        elif val.startswith(("'", '"')) and val.endswith(("'", '"')) and len(val) > 1:
            val = val[1:-1]
        meta[key] = val
    return meta, body


def as_date(val):
    try:
        return dt.date.fromisoformat(val)
    except (ValueError, TypeError):
        return None


def lint(scan_dirs, today, base):
    findings = []
    docs = {}

    def add(level, path, code, msg):
        findings.append({
            "level": level,
            "file": str(path.relative_to(base)),
            "code": code,
            "message": msg,
        })

    paths = sorted(q for d in scan_dirs if d.exists() for q in d.rglob("*.md"))
    for path in paths:
        meta, body = parse_frontmatter(path)
        if meta is None:
            add("WARN", path, "NO_FRONTMATTER", "no frontmatter block; not tracked for freshness")
            continue

        for field in REQUIRED:
            if not meta.get(field):
                add("ERROR", path, "MISSING_FIELD", f"required field '{field}' missing or empty")

        doc_id = meta.get("id")
        if doc_id:
            if doc_id in docs:
                add("ERROR", path, "DUPLICATE_ID", f"id '{doc_id}' already used by {docs[doc_id]['path'].name}")
            else:
                docs[doc_id] = {"meta": meta, "body": body, "path": path}

        status = meta.get("status")
        if status and status not in VALID_STATUS:
            add("ERROR", path, "BAD_STATUS", f"status '{status}' not in {sorted(VALID_STATUS)}")
        if meta.get("confidence") and meta["confidence"] not in VALID_CONF:
            add("ERROR", path, "BAD_CONFIDENCE", f"confidence '{meta['confidence']}' not in {sorted(VALID_CONF)}")
        if meta.get("source_type") and meta["source_type"] not in VALID_SRC:
            add("ERROR", path, "BAD_SOURCE_TYPE", f"source_type '{meta['source_type']}' not in {sorted(VALID_SRC)}")

        # Staleness: an explicit TTL, not a model's judgement.
        review_by = as_date(meta.get("review_by"))
        if meta.get("review_by") and review_by is None:
            add("ERROR", path, "BAD_DATE", f"review_by '{meta['review_by']}' is not ISO YYYY-MM-DD")
        elif review_by and status == "current":
            if review_by < today:
                days = (today - review_by).days
                add("ERROR", path, "STALE", f"review_by passed {days}d ago; re-verify or supersede")
            elif (review_by - today).days <= 14:
                add("WARN", path, "EXPIRING", f"review_by in {(review_by - today).days}d")

        # A superseded fact must say what replaced it, or history is broken.
        if status == "superseded" and not meta.get("superseded_by"):
            add("ERROR", path, "ORPHAN_SUPERSEDED", "status=superseded but superseded_by is empty")
        if status == "current" and meta.get("invalidated_at"):
            add("ERROR", path, "CURRENT_BUT_INVALIDATED", "status=current but invalidated_at is set")

        # Confidence must be earned by the source class.
        if meta.get("confidence") == "high" and meta.get("source_type") in ("vendor-marketing", "inference"):
            add("WARN", path, "OVERCONFIDENT", f"confidence=high on source_type={meta['source_type']}")

    # Cross-document checks -------------------------------------------------
    ids = set(docs)

    for doc_id, d in docs.items():
        meta, path = d["meta"], d["path"]
        for field in ("supersedes", "superseded_by"):
            ref = meta.get(field)
            if ref and ref not in ids:
                add("ERROR", path, "DANGLING_REF", f"{field} -> '{ref}' which does not exist")
        # Supersession must be symmetric, or the history chain lies.
        target = meta.get("supersedes")
        if target and target in ids:
            back = docs[target]["meta"]
            if back.get("status") != "superseded":
                add("ERROR", path, "ASYMMETRIC_SUPERSEDE",
                    f"supersedes '{target}' but that doc's status is '{back.get('status')}', not 'superseded'")
            if back.get("superseded_by") != doc_id:
                add("ERROR", path, "ASYMMETRIC_SUPERSEDE",
                    f"supersedes '{target}' but its superseded_by is '{back.get('superseded_by')}', not '{doc_id}'")

        for link in WIKILINK.findall(d["body"]):
            if link.strip() not in ids:
                add("WARN", path, "BROKEN_LINK", f"[[{link.strip()}]] has no matching id")

    # The core rule: same (subject, relation), two live values = contradiction.
    claims = {}
    for doc_id, d in docs.items():
        meta = d["meta"]
        if meta.get("status") != "current":
            continue
        if not all(meta.get(f) for f in FACT_REQUIRED):
            continue
        key = (meta["subject"].lower(), meta["relation"].lower())
        claims.setdefault(key, []).append((doc_id, meta["object"], d["path"]))

    for (subj, rel), entries in sorted(claims.items()):
        objects = {o for _, o, _ in entries}
        if len(objects) > 1:
            detail = "; ".join(f"{i}='{o}'" for i, o, _ in entries)
            for _, _, path in entries:
                add("ERROR", path, "CONTRADICTION",
                    f"({subj}, {rel}) has {len(objects)} live values -> {detail}")

    return findings, docs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--quiet", action="store_true", help="errors only")
    ap.add_argument("--today", default=None, help="override today (YYYY-MM-DD) for testing")
    args = ap.parse_args()

    today = as_date(args.today) or dt.date.today()

    if not any(d.exists() for d in SCAN_DIRS):
        print(f"no kb dirs found under {ROOT}", file=sys.stderr)
        return 2

    findings, docs = lint(SCAN_DIRS, today, ROOT)
    errors = [f for f in findings if f["level"] == "ERROR"]
    warns = [f for f in findings if f["level"] == "WARN"]

    if args.json:
        print(json.dumps({
            "checked": len(docs), "errors": len(errors), "warnings": len(warns),
            "findings": findings,
        }, indent=2))
        return 1 if errors else 0

    shown = errors if args.quiet else findings
    for f in shown:
        print(f"{f['level']:5} {f['code']:22} {f['file']}\n      {f['message']}")

    print(f"\n{len(docs)} documents | {len(errors)} errors | {len(warns)} warnings  (as of {today})")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
