"""The twin of crates/viy-pod/examples/live_api.rs: the same read-only calls
against the live RunPod API through rpctl's api.py at the pinned snapshot,
printed the same way. Run by tools/live_api.sh with the installed rp's
interpreter, the key in RUNPOD_READ_TOKEN. Never prints a key, a pod's env,
or a log line's text.

    python -I tools/live_api.py <snapshot dir>
"""
import json
import os
import sys
import time

sys.path.insert(0, sys.argv[1])
from rpctl import api  # noqa: E402

MISSING_POD = "zzzzzzzzzzzzzz"
GPU = "NVIDIA A100-SXM4-80GB"


def say(call, started, outcome):
    ms = int((time.monotonic() - started) * 1000)
    print(json.dumps({"call": call, "ms": ms, "outcome": outcome}))


def failed(error):
    return {"error": str(error).splitlines()[0], "status_code": error.status_code,
            "transient": error.transient}


def identity(row):
    return {"id": row.get("id"), "name": row.get("name"),
            "desiredStatus": row.get("desiredStatus"),
            "dataCenterId": row.get("dataCenterId"), "keys": len(row)}


def attempt(call, fn, shape):
    started = time.monotonic()
    try:
        value = fn()
    except api.RunPodError as error:
        say(call, started, failed(error))
        return None
    say(call, started, shape(value))
    return value


def main():
    key = os.environ.get("RUNPOD_READ_TOKEN")
    if not key:
        print("RUNPOD_READ_TOKEN is not set: run under `tok run`", file=sys.stderr)
        return 2
    rp = api.RunPod(key)
    pods = attempt("list_pods", rp.list_pods, lambda pods: [identity(p) for p in pods])
    running = next((p["id"] for p in pods or [] if p.get("desiredStatus") == "RUNNING"), None)
    attempt("list_volumes", rp.list_volumes, lambda vols: [identity(v) for v in vols])
    if running:
        attempt("get_pod", lambda: rp.get_pod(running),
                lambda pod: identity(pod) if pod is not None else None)
    attempt("get_pod_missing", lambda: rp.get_pod(MISSING_POD),
            lambda pod: {"found": pod is not None})
    centers = attempt("data_centers", rp.data_centers, lambda dcs: {
        "count": len(dcs),
        "with_gpu_availability": sum(isinstance(dc.get("gpuAvailability"), list)
                                     for dc in dcs),
        "keys": list(dcs[0]) if dcs else None,
    })
    ids = [dc["id"] for dc in centers or [] if isinstance(dc.get("id"), str)]
    if ids:
        center = min(ids)
        attempt("cuda_stock", lambda: rp.cuda_stock([center], [GPU], "12.8", ["12.4"], True),
                lambda stock: [[dc, gpu, fit is not None, old is not None]
                               for (dc, gpu), (fit, old) in stock.items()])
    if running:
        attempt("pod_logs", lambda: rp.pod_logs(running, tail=20, budget_s=8),
                lambda entries: {
                    "entries": len(entries),
                    "with_ts": sum("ts" in e for e in entries),
                    "sources": sorted({e["source"] for e in entries
                                       if isinstance(e.get("source"), str)}),
                })
    return 0


if __name__ == "__main__":
    sys.exit(main())
