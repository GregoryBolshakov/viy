#!/usr/bin/env python3
"""
secret_scan.py - refuse to commit secrets.

Checks the staged diff (or, with --all, every tracked file) for:
  - files that should never be committed (.env, private keys, the token vault),
  - token-shaped strings (HuggingFace, RunPod, Tavily, npm, Apify, GitHub, AWS,
    private key blocks),
  - the literal values of the real keys in viy's and rpctl's config files, so
    a token with no recognisable shape is still caught.

Never prints a secret: findings name the file, the line and the rule only.

Usage:
    python3 tools/secret_scan.py          # staged changes (pre-commit hook)
    python3 tools/secret_scan.py --all    # every tracked file

Stdlib only. Python 3.9+.
"""

import argparse
import fnmatch
import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

BLOCKED_NAMES = [".env", ".env.*", "*.env", "config.bak.*", "*.pem", "*.key",
                 "*.p12", "*.pfx", "id_rsa*", "id_ed25519*", "id_ecdsa*",
                 "known_hosts*", ".netrc", "vault.json", "hosts.yml"]
ALLOWED_NAMES = [".env.example"]

# Lengths are set above the fake keys in tests/ (e.g. rpa_SECRETVALUE...).
PATTERNS = {
    "huggingface token": r"\bhf_[A-Za-z0-9]{30,}",
    "runpod key": r"\brpa_[A-Za-z0-9]{30,}",
    "tavily key": r"\btvly-[A-Za-z0-9-]{20,}",
    "npm token": r"\bnpm_[A-Za-z0-9]{30,}",
    "apify token": r"\bapify_api_[A-Za-z0-9]{20,}",
    "github token": r"\b(gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})",
    "anthropic/openai key": r"\bsk-(ant-)?[A-Za-z0-9_-]{32,}",
    "aws access key": r"\bAKIA[0-9A-Z]{16}\b",
    "private key block": r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
}
PATTERNS = {name: re.compile(rx) for name, rx in PATTERNS.items()}

# Env-style files holding the real keys; their values are matched literally.
SECRET_KEY_RX = re.compile(r"(KEY|TOKEN|SECRET|PASSWORD|PAT)\b")
_XDG_CONFIG = pathlib.Path(os.environ.get("XDG_CONFIG_HOME") or pathlib.Path.home() / ".config")
# viy is a port of rpctl and reads the same keys; scan both tools' configs.
CONFIG_DIRS = [_XDG_CONFIG / "viy", _XDG_CONFIG / "rpctl"]
SECRET_FILES = [ROOT / ".env", *ROOT.glob(".env.bak*")]
for _d in CONFIG_DIRS:
    SECRET_FILES += [_d / "config", *_d.glob("config.bak.*"), *_d.glob("profiles/*.env")]


def known_secrets():
    """{value: key name} for every secret-looking key in the config files."""
    found = {}
    for path in SECRET_FILES:
        try:
            lines = path.read_text(errors="replace").splitlines()
        except OSError:
            continue
        for line in lines:
            key, sep, value = line.strip().partition("=")
            key = key.removeprefix("export ").strip()
            value = value.strip().strip("'\"")
            if sep and SECRET_KEY_RX.search(key) and len(value) >= 12:
                found[value] = key
    return found


def git(*args):
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True,
                          text=True, errors="replace", check=True).stdout


def blocked_name(path):
    name = pathlib.PurePosixPath(path).name
    if any(fnmatch.fnmatch(name, p) for p in ALLOWED_NAMES):
        return False
    return any(fnmatch.fnmatch(name, p) for p in BLOCKED_NAMES)


def staged_lines():
    """(path, line number, text) for every line the staged diff adds."""
    path, lineno = None, 0
    for line in git("diff", "--cached", "-U0", "--no-color", "--no-ext-diff").splitlines():
        if line.startswith("+++ "):
            path = line[6:] if line.startswith("+++ b/") else None
        elif line.startswith("@@"):
            lineno = int(re.search(r"\+(\d+)", line).group(1))
        elif line.startswith("+") and path:
            yield path, lineno, line[1:]
            lineno += 1


def tracked_lines():
    for path in git("ls-files", "-z").split("\0"):
        if not path:
            continue
        try:
            text = (ROOT / path).read_text(errors="replace")
        except (OSError, UnicodeDecodeError):
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            yield path, lineno, line


def main():
    ap = argparse.ArgumentParser(description="refuse to commit secrets")
    ap.add_argument("--all", action="store_true", help="scan every tracked file, not the staged diff")
    args = ap.parse_args()

    if args.all:
        paths = git("ls-files", "-z").split("\0")
        lines = tracked_lines()
    else:
        paths = git("diff", "--cached", "--name-only", "--diff-filter=ACMR", "-z").split("\0")
        lines = staged_lines()

    findings = [f"{p}: file type must never be committed" for p in paths if p and blocked_name(p)]
    secrets = known_secrets()
    for path, lineno, text in lines:
        for name, rx in PATTERNS.items():
            if rx.search(text):
                findings.append(f"{path}:{lineno}: looks like a {name}")
        for value, key in secrets.items():
            if value in text:
                findings.append(f"{path}:{lineno}: contains the value of {key} from your viy/rpctl config")

    for f in findings:
        print(f"secret_scan: {f}", file=sys.stderr)
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
