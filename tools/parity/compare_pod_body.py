"""Compares two create-pod bodies (rpctl's and viy's, kb f116) without
printing a secret: every env value whose name holds KEY, TOKEN or SECRET is
replaced, in both bodies and everywhere it occurs, by <NAME>, after saying
whether the two sides' values are equal. Then the bodies must be equal
byte for byte as JSON in rpctl's key order; where they are not, the paths
that differ and a line diff of differing strings are printed.

    python3 tools/parity/compare_pod_body.py RPCTL.json VIY.json
"""
import difflib
import json
import sys

SECRET = ("KEY", "TOKEN", "SECRET")


def secrets(body):
    env = body.get("env") or {}
    return {k: v for k, v in env.items() if any(s in k.upper() for s in SECRET) and v}


def masked(text, table):
    for value, name in sorted(table.items(), key=lambda kv: -len(kv[0])):
        text = text.replace(value, name)
    return text


def walk(a, b, path, out):
    if isinstance(a, dict) and isinstance(b, dict):
        if list(a) != list(b):
            out.append(f"{path}: keys {list(a)} vs {list(b)}")
        for k in a:
            if k in b:
                walk(a[k], b[k], f"{path}.{k}", out)
    elif isinstance(a, list) and isinstance(b, list) and len(a) == len(b):
        for i, (x, y) in enumerate(zip(a, b)):
            walk(x, y, f"{path}[{i}]", out)
    elif a != b or type(a) is not type(b):
        if isinstance(a, str) and isinstance(b, str) and ("\n" in a or "\n" in b):
            diff = "\n".join(difflib.unified_diff(a.splitlines(), b.splitlines(), "rpctl",
                                                  "viy", lineterm="", n=1))
            out.append(f"{path}:\n{diff}")
        else:
            out.append(f"{path}: rpctl {a!r} viy {b!r}")


def main(argv):
    ra, va = (json.loads(open(p).read()) for p in argv)
    rs, vs = secrets(ra), secrets(va)
    table = {}
    for name in sorted(set(rs) | set(vs)):
        same = rs.get(name) == vs.get(name)
        print(f"secret {name}: {'same value' if same else 'DIFFERENT values'} on both sides")
        for side, s in (("rpctl", rs), ("viy", vs)):
            if name in s:
                table[s[name]] = f"<{name}>" if same else f"<{name} of {side}>"
    a = json.loads(masked(json.dumps(ra), table))
    b = json.loads(masked(json.dumps(va), table))
    if json.dumps(a) == json.dumps(b):
        print(f"bodies byte-identical as JSON ({len(json.dumps(ra))} bytes), secrets aside as said")
        return 0
    out = []
    walk(a, b, "body", out)
    print("bodies DIFFER:")
    for line in out:
        print(line)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
