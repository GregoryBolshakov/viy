#!/usr/bin/env python3
"""The oracle: rpctl's behaviour at the pinned commit, as a JSON-lines service.

    python3 -I tools/oracle.py

One request per line on stdin, one answer per line on stdout:

    {"fn": "approve.destructive_reason", "args": ["rm -rf x"], "kwargs": {}}
    {"ok": "recursive/forced delete"}
    {"ok": null, "stdout": "what the function printed\\n"}
    {"raised": {"type": "ValueError", "message": "No closing quotation"}}
    {"refused": "not on the oracle's whitelist: os.system"}

`fn` names an entry of FUNCTIONS: rpctl modules by their name inside the
package (`approve.destructive_reason`). `oracle.hello` answers with the
snapshot's commit and the Python version. viy reproduces rpctl's behaviour,
not Python's semantics (kb d029), so the standard library is not offered.

JSON carries what it can. Other values are tagged, in both directions:
{"$tuple": [...]}, {"$bytes": "<base64>"}, {"$path": "..."}, and in requests
{"$slice": [start, stop, step]}; a result of any other type comes back as
{"$repr": "..."}.

rpctl is imported from target/rpctl-ref/<commit>/ (tools/refsnap.sh), never
the live checkout. Before the import, HOME, every XDG_* directory and the
working directory point into a fresh temporary directory, so rpctl's
import-time paths never reach Gregory's real config or state. Each call gets
an empty stdin (an input() raises EOFError) and its stdout and stderr are
captured into the answer. Implementation plan 3.2.
"""
import atexit
import base64
import contextlib
import importlib
import io
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent

def redact_text(text, literals=()):
    """redact.Redactor(literals).text(text): a method, so called through here."""
    from rpctl import redact
    return redact.Redactor(literals).text(text)


def redact_any_pattern(text):
    """Whether one of redact's token formats occurs in text, as netguard asks."""
    from rpctl import redact
    return any(p.search(text) for p in redact._PATTERNS)


# The callable functions: request name -> (module, attribute), or a function
# defined above. Add entries as work packages need them; every rpctl entry
# must be free of I/O (plan 5.4).
FUNCTIONS = {
    # rpctl
    "approve.destructive_reason": ("rpctl.approve", "destructive_reason"),
    "approve.net_ask_reason": ("rpctl.approve", "net_ask_reason"),
    "approve.plain_pipeline": ("rpctl.approve", "plain_pipeline"),
    "approve.prefix_for": ("rpctl.approve", "prefix_for"),
    "redact.any_pattern": redact_any_pattern,
    "redact.is_secret_name": ("rpctl.redact", "is_secret_name"),
    "redact.literal_secrets": ("rpctl.redact", "literal_secrets"),
    "redact.text": redact_text,
}


def snapshot():
    """The snapshot directory and its commit, extracting it if needed."""
    out = subprocess.run([str(ROOT / "tools" / "refsnap.sh")], cwd=ROOT, check=True,
                         capture_output=True, text=True).stdout.strip()
    path = (ROOT / out).resolve()
    return path, path.name


def isolate():
    """Point HOME, XDG_* and the working directory at a fresh temp dir."""
    base = pathlib.Path(tempfile.mkdtemp(prefix="oracle-"))
    atexit.register(shutil.rmtree, base, ignore_errors=True)
    for name, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                      ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"),
                      ("XDG_CACHE_HOME", "cache"), ("XDG_RUNTIME_DIR", "runtime")):
        d = base / sub
        d.mkdir(mode=0o700)
        os.environ[name] = str(d)
    ws = base / "workspace"
    ws.mkdir()
    os.chdir(ws)


def decode(v):
    if isinstance(v, list):
        return [decode(x) for x in v]
    if isinstance(v, dict):
        if len(v) == 1:
            (k, x), = v.items()
            if k == "$tuple":
                return tuple(decode(y) for y in x)
            if k == "$bytes":
                return base64.b64decode(x)
            if k == "$path":
                return pathlib.Path(x)
            if k == "$slice":
                return slice(*x)
        return {k: decode(x) for k, x in v.items()}
    return v


def encode(v):
    if v is None or isinstance(v, (bool, int, str)):
        return v
    if isinstance(v, float):
        return v if v == v and v not in (float("inf"), float("-inf")) else {"$repr": repr(v)}
    if isinstance(v, tuple):
        return {"$tuple": [encode(x) for x in v]}
    if isinstance(v, list):
        return [encode(x) for x in v]
    if isinstance(v, dict) and all(isinstance(k, str) for k in v):
        return {k: encode(x) for k, x in v.items()}
    if isinstance(v, bytes):
        return {"$bytes": base64.b64encode(v).decode("ascii")}
    if isinstance(v, pathlib.PurePath):
        return {"$path": str(v)}
    return {"$repr": repr(v)}


def answer(req, commit):
    name = req.get("fn")
    if name == "oracle.hello":
        return {"ok": {"rpctl_commit": commit, "python": sys.version.split()[0]}}
    if name not in FUNCTIONS:
        return {"refused": f"not on the oracle's whitelist: {name}"}
    entry = FUNCTIONS[name]
    out, err = io.StringIO(), io.StringIO()
    stdin, sys.stdin = sys.stdin, io.StringIO()
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            if callable(entry):
                fn = entry
            else:
                module, attr = entry
                fn = importlib.import_module(module)
                for part in attr.split("."):
                    fn = getattr(fn, part)
            args = decode(req.get("args", []))
            kwargs = decode(req.get("kwargs", {}))
            result = {"ok": encode(fn(*args, **kwargs))}
    except KeyboardInterrupt:
        raise
    except BaseException as e:  # rpctl reports errors with SystemExit
        result = {"raised": {"type": type(e).__name__, "message": str(e)}}
    finally:
        sys.stdin = stdin
    if out.getvalue():
        result["stdout"] = out.getvalue()
    if err.getvalue():
        result["stderr"] = err.getvalue()
    return result


def main():
    path, commit = snapshot()
    isolate()
    sys.path.insert(0, str(path))
    proto_in, proto_out = sys.stdin, sys.stdout
    for line in proto_in:
        if not line.strip():
            continue
        try:
            req = json.loads(line)
            reply = answer(req, commit) if isinstance(req, dict) else {
                "refused": "a request is a JSON object"}
        except json.JSONDecodeError as e:
            reply = {"refused": f"not JSON: {e}"}
        proto_out.write(json.dumps(reply) + "\n")
        proto_out.flush()


if __name__ == "__main__":
    main()
