#!/usr/bin/env python3
"""Effective-config diff: rp's real config layers through rpctl and through viy.

    python3 -I tools/parity/config_diff.py [PROJECT_DIR]

Loads the config rp would use in PROJECT_DIR (default: the current
directory), with this shell's environment, twice: through rpctl's
config.load() at the pinned commit, and through viy's layering
(crates/viy-config/examples/config_dump.rs) pointed at the same files: rp's
global config, its profiles and the project's .rpctl/config. Prints the
NAMES of keys that are missing on one side, differ in value or sit in a
different position, never a value: the files hold API keys. Exits 0 when
the two agree, 1 otherwise. Implementation plan 2.2, check 3.

Known differences that are not reported as such but would show here:
Python's strip() and splitlines() treat U+001C-U+001F (and splitlines a
lone CR) as whitespace or line breaks where Rust does not (kb d032); and
rpctl's legacy in-repo .env fallback, which viy does not have.
"""
import json
import os
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent.parent

RPCTL_RUNNER = r"""
import json, sys
sys.path.insert(0, sys.argv[1])
from rpctl import config
json.dump({"config": config.load(), "global": str(config.CONFIG_PATH),
           "profiles": str(config.PROFILES_DIR),
           "project": str(config.rpctl_dir() / "config"),
           "legacy": config.LEGACY_ENV.exists()}, sys.stdout)
"""


def snapshot():
    out = subprocess.run([str(ROOT / "tools" / "refsnap.sh")], cwd=ROOT, check=True,
                         capture_output=True, text=True).stdout.strip()
    return (ROOT / out).resolve()


def main(argv):
    project = pathlib.Path(argv[0] if argv else os.getcwd()).resolve()
    rp = json.loads(subprocess.run(
        [sys.executable, "-I", "-c", RPCTL_RUNNER, str(snapshot())],
        cwd=project, check=True, capture_output=True, text=True).stdout)
    subprocess.run(["cargo", "build", "-q", "--offline", "--locked", "-p", "viy-config",
                    "--example", "config_dump"], cwd=ROOT, check=True)
    viy = json.loads(subprocess.run(
        [str(ROOT / "target" / "debug" / "examples" / "config_dump"),
         rp["global"], rp["profiles"], rp["project"]],
        cwd=project, check=True, capture_output=True, text=True).stdout)

    a, b = rp["config"], viy
    only_rp = [k for k in a if k not in b]
    only_viy = [k for k in b if k not in a]
    differ = [k for k in a if k in b and a[k] != b[k]]
    common_a = [k for k in a if k in b]
    common_b = [k for k in b if k in a]
    moved = [k for k, j in zip(common_a, common_b) if k != j]
    print(f"project {project}; {len(a)} keys through rpctl, {len(b)} through viy")
    if rp["legacy"]:
        print("note: rpctl also read its legacy in-repo .env, which viy does not")
    for label, names in (("only in rpctl", only_rp), ("only in viy", only_viy),
                         ("values differ", differ), ("order differs from", moved[:1])):
        if names:
            print(f"{label}: {', '.join(names)}")
    if only_rp or only_viy or differ or moved:
        return 1
    print("same keys, same values, same order")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
