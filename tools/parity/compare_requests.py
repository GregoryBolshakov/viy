#!/usr/bin/env python3
"""Compare two request logs by content (kb d031).

    python3 -I tools/parity/compare_requests.py A.jsonl B.jsonl

A and B are normalized request logs as tools/drive_rp.py writes them (and
viy's driver will): a first line {"meta": {...}}, then one line per request
fakellm received. Exits 0 when the requests match, 1 with the first
difference and its path when they do not.

What "the same" means, from kb d031:
- requests in the same order, with the same method and path;
- bodies parsed as JSON and equal in content: object key order does not
  count, except inside "tools", where the chat template's `tojson` keeps it;
- a tool result and a tool call's arguments are strings holding JSON, and are
  compared parsed (rp writes json.dumps, viy serde_json);
- every other string, the prompts and notices above all, equal as a string;
- the Authorization header equal; the User-Agent is the HTTP library's and
  is not compared, but one naming viy is an error (CLAUDE.md).

normalize() is shared with the drivers: it replaces the run's temporary paths
and its date with {WORKSPACE}, {BASE} and {TODAY}, so logs from different runs
and days compare. Implementation plan 3.4.
"""
import json
import pathlib
import sys

CLIP = 160


def normalize(value, meta):
    """`value` with the run's paths and date replaced by placeholders."""
    subs = [(meta[k], "{" + k.upper() + "}") for k in ("workspace", "base", "today")
            if meta.get(k)]
    # Longest first: the workspace lies inside the base.
    subs.sort(key=lambda s: -len(s[0]))

    def walk(v):
        if isinstance(v, str):
            for old, new in subs:
                v = v.replace(old, new)
            return v
        if isinstance(v, list):
            return [walk(x) for x in v]
        if isinstance(v, dict):
            return {walk(k): walk(x) for k, x in v.items()}
        return v
    return walk(value)


def load(path):
    """(meta, entries) of a normalized log."""
    lines = pathlib.Path(path).read_text(encoding="utf-8").splitlines()
    first = json.loads(lines[0])
    if "meta" not in first:
        raise SystemExit(f"{path}: the first line is not a meta header")
    return first["meta"], [json.loads(x) for x in lines[1:] if x.strip()]


def write(path, meta, entries):
    with open(path, "w", encoding="utf-8") as f:
        f.write(json.dumps({"meta": meta}, ensure_ascii=False) + "\n")
        for e in entries:
            f.write(json.dumps(e, ensure_ascii=False) + "\n")


def _clip(v):
    s = v if isinstance(v, str) else json.dumps(v, ensure_ascii=False)
    return s if len(s) <= CLIP else s[:CLIP] + f"... ({len(s)} chars)"


def _string_diff(a, b):
    i = next((n for n, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
    lo = max(0, i - 40)
    return (f"strings differ at char {i} (lengths {len(a)}, {len(b)}):\n"
            f"  A: ...{a[lo:i + 80]!r}\n  B: ...{b[lo:i + 80]!r}")


def _as_json(s):
    if isinstance(s, str) and s.lstrip()[:1] in ("{", "["):
        try:
            return json.loads(s)
        except ValueError:
            pass
    return None


def diff(a, b, path, ordered=False):
    """The first difference between two JSON values, or None."""
    if isinstance(a, bool) or isinstance(b, bool):
        if a is not b:
            return f"{path}: {_clip(a)} != {_clip(b)}"
        return None
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        return None if a == b else f"{path}: {a} != {b}"
    if type(a) is not type(b):
        return f"{path}: types differ, {type(a).__name__} {_clip(a)} vs {type(b).__name__} {_clip(b)}"
    if isinstance(a, dict):
        if ordered and list(a) != list(b):
            return f"{path}: key order differs: {list(a)} vs {list(b)}"
        for k in a:
            if k not in b:
                return f"{path}.{k}: only in A ({_clip(a[k])})"
        for k in b:
            if k not in a:
                return f"{path}.{k}: only in B ({_clip(b[k])})"
        for k in a:
            d = diff(a[k], b[k], f"{path}.{k}", ordered or (path == "body" and k == "tools"))
            if d:
                return d
        return None
    if isinstance(a, list):
        for i, (x, y) in enumerate(zip(a, b)):
            d = diff(x, y, f"{path}[{i}]", ordered)
            if d:
                return d
        if len(a) != len(b):
            extra, side = (a, "A") if len(a) > len(b) else (b, "B")
            n = min(len(a), len(b))
            return f"{path}[{n}]: only in {side} ({len(a)} vs {len(b)} items): {_clip(extra[n])}"
        return None
    if a != b:
        return f"{path}: " + _string_diff(a, b)
    return None


def _json_strings(body):
    """Parse the strings that hold JSON in place: tool results, call arguments."""
    for m in body.get("messages") or []:
        if not isinstance(m, dict):
            continue
        if m.get("role") == "tool":
            parsed = _as_json(m.get("content"))
            if parsed is not None:
                m["content"] = {"$json": parsed}
        for tc in m.get("tool_calls") or []:
            fn = tc.get("function") or {}
            parsed = _as_json(fn.get("arguments"))
            if parsed is not None:
                fn["arguments"] = {"$json": parsed}
    return body


def compare(ea, eb):
    """The first difference between two lists of log entries, or None."""
    for n, (a, b) in enumerate(zip(ea, eb)):
        where = f"request {n} ({a.get('method')} {a.get('path')})"
        for side, e in (("A", a), ("B", b)):
            if "viy" in (e.get("headers", {}).get("user-agent") or "").lower():
                return f"{where}: {side}'s User-Agent names viy: {e['headers']['user-agent']}"
            if e.get("mismatch"):
                return f"{where}: {side} hit a scenario mismatch: {e['mismatch']}"
        for key in ("method", "path"):
            if a.get(key) != b.get(key):
                return f"{where}: {key} {a.get(key)!r} != {b.get(key)!r}"
        ha, hb = a.get("headers", {}), b.get("headers", {})
        if ha.get("authorization") != hb.get("authorization"):
            return f"{where}: Authorization headers differ"
        if "body" in a or "body" in b:
            d = diff(_json_strings(json.loads(json.dumps(a.get("body")))),
                     _json_strings(json.loads(json.dumps(b.get("body")))), "body")
            if d:
                return f"{where}: {d}"
    if len(ea) != len(eb):
        n = min(len(ea), len(eb))
        side, extra = ("A", ea) if len(ea) > len(eb) else ("B", eb)
        return (f"{side} has {abs(len(ea) - len(eb))} more request(s), the first: "
                f"{extra[n].get('method')} {extra[n].get('path')}")
    return None


def main(argv):
    if len(argv) != 2:
        print(__doc__.strip().split("\n\n")[1], file=sys.stderr)
        return 2
    (ma, ea), (mb, eb) = load(argv[0]), load(argv[1])
    if ma.get("scenario") != mb.get("scenario"):
        print(f"different scenarios: {ma.get('scenario')} vs {mb.get('scenario')}")
        return 1
    d = compare(ea, eb)
    if d:
        print(f"A = {ma.get('client')} ({argv[0]})\nB = {mb.get('client')} ({argv[1]})\n{d}")
        return 1
    print(f"same content: {len(ea)} requests ({ma.get('client')} vs {mb.get('client')})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
