"""rp's side of the pod-state interop tests (interop_tests.rs): rpctl's own
pod.py and idle.py, imported from the pinned snapshot, acting on the state
directory the test points XDG_STATE_HOME at.

    python3 -I -B rp_pod_state.py <snapshot> <op> [args...]

hold <verb>      hold pod.lock as `rp <verb>` until stdin closes
run <verb>       `rp <verb>` with nothing to do, or the refusal it prints
lock_free        reap's check, as JSON
preempt          rp down's preempt on a fast clock; JSON {killed, lock_file}
save <json>      save_state(**json)
save_many <prefix> <n>
                 n saves, one key each: <prefix>0 ... <prefix>{n-1}
load             load_state(), as JSON
clear            clear_state()
"""
import json
import sys
from unittest import mock

snapshot, op, *args = sys.argv[1:]
sys.path.insert(0, snapshot)
from rpctl import idle, pod  # noqa: E402

if op == "hold":
    def held():
        print("held", flush=True)
        sys.stdin.read()
    pod._exclusive(args[0])(held)()
elif op == "run":
    try:
        pod._exclusive(args[0])(lambda: print("ran"))()
    except SystemExit as e:
        print(e)
elif op == "lock_free":
    print(json.dumps(idle._lock_free()))
elif op == "preempt":
    # Each clock read is a second later, and sleeps return at once, so the
    # 15 s deadline passes in a few loops.
    ticks = iter(range(1, 10**6))
    with mock.patch.object(pod.time, "monotonic", lambda: float(next(ticks))), \
            mock.patch.object(pod.time, "sleep", lambda s: None):
        with open(pod.POD_LOCK, "a+") as f:
            killed = pod._preempt(f)
    print(json.dumps({"killed": killed, "lock_file": pod.POD_LOCK.read_text()}))
elif op == "save":
    pod.save_state(**json.loads(args[0]))
elif op == "save_many":
    for i in range(int(args[1])):
        pod.save_state(**{f"{args[0]}{i}": i})
elif op == "load":
    print(json.dumps(pod.load_state()))
elif op == "clear":
    pod.clear_state()
else:
    sys.exit(f"unknown op {op}")
