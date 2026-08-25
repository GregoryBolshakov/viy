#!/bin/sh
# Read-only calls against the live RunPod API, through viy (the live_api
# example) and through rpctl's api.py at the pin (tools/live_api.py, with the
# installed rp's interpreter), one after the other; prints both and the
# lines that differ once timings are dropped. Leaves every pod as it is.
# Network: RunPod only, from the identity IP (run the safety preflight first).
#
#     tok run --pin=<PIN> -- tools/live_api.sh
set -eu
cd "$(dirname "$0")/.."
[ -n "${RUNPOD_READ_TOKEN:-}" ] || { echo "RUNPOD_READ_TOKEN is not set: run under tok run" >&2; exit 2; }
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
snap=$(tools/refsnap.sh)
rp_python=$(head -1 "$(command -v rp)" | sed 's/^#!//')
cargo build -q --offline -p viy-pod --example live_api
target/debug/examples/live_api >"$out/viy.jsonl"
# shellcheck disable=SC2086 # the shebang may carry arguments
$rp_python -I tools/live_api.py "$snap" >"$out/rp.jsonl"
echo "== viy"; cat "$out/viy.jsonl"
echo "== rp"; cat "$out/rp.jsonl"
strip() { python3 -c 'import json,sys
for l in sys.stdin: d=json.loads(l); d.pop("ms"); print(json.dumps(d, sort_keys=True))' <"$1"; }
strip "$out/viy.jsonl" >"$out/viy.cmp"
strip "$out/rp.jsonl" >"$out/rp.cmp"
echo "== differences (viy <, rp >)"
diff "$out/viy.cmp" "$out/rp.cmp" && echo "none"
