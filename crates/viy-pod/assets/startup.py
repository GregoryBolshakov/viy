"""Standard-library-only helper embedded into the GPU pod's boot script.

Compilation runs on local disk: network volumes need not support executable
files or POSIX modes. A small archive preserves caches across pod recreation.
Only a model that passed /health is eligible for offline reuse.
"""
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.request

CACHE_DIRS = (
    "root/.cache/vllm",
    "root/.cache/torch/inductor",
    "root/.triton/cache",
    "root/.cache/flashinfer",
    "root/.nv/ComputeCache",
)


def phase(message):
    print(f"RPCTL_PHASE {message}", flush=True)


def atomic_json(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_suffix(f".{os.getpid()}.tmp")
    temp.write_text(json.dumps(data))
    temp.replace(path)


def model_manifest_path(root, repo):
    key = hashlib.sha256(repo.encode()).hexdigest()[:20]
    return root / f"model-{key}.json"


def cached_snapshot(hf_home, repo):
    base = hf_home / "hub" / ("models--" + repo.replace("/", "--"))
    try:
        revision = (base / "refs/main").read_text().strip()
        if not revision or any(c not in "0123456789abcdef" for c in revision):
            return None
        snapshot = base / "snapshots" / revision
        return snapshot if snapshot.is_dir() else None
    except OSError:
        return None


def validated_model(root, hf_home, repo):
    """A successful previous boot plus intact cached files permits offline use.

    Not keyed on the image: the files do not depend on it, and every fall back
    to online mode changes vLLM's compile hash (offline hands vLLM the snapshot
    path instead of the repo name), which costs a full recompile."""
    try:
        manifest = json.loads(model_manifest_path(root, repo).read_text())
        snapshot = cached_snapshot(hf_home, repo)
        if (snapshot is None
                or manifest["snapshot"] != str(snapshot)
                or not manifest["files"]):
            return None
        for name, size in manifest["files"].items():
            file = snapshot / name
            if not file.is_file() or file.stat().st_size != size:
                return None
        return snapshot
    except (OSError, ValueError, KeyError, TypeError):
        return None


def remember_model(root, hf_home, repo):
    snapshot = cached_snapshot(hf_home, repo)
    if snapshot is None:
        return
    files = {str(p.relative_to(snapshot)): p.stat().st_size
             for p in snapshot.rglob("*") if p.is_file()}
    if files:
        atomic_json(model_manifest_path(root, repo),
                    {"snapshot": str(snapshot), "files": files})


def archive_name(gpu):
    return f"compile-gpu-{hashlib.sha256(gpu.encode()).hexdigest()[:24]}.tar"


def archive_path(root):
    # One archive per GPU model, nothing else. vLLM names each compiled variant
    # by its own hash of the image's versions and the serving config, and
    # Triton/Inductor entries are content-addressed, so variants coexist in one
    # archive and vLLM picks the matching one. Keying on the image or the
    # arguments too threw the whole cache away on every such change. Not the
    # driver version: each new pod tends to land on a host with another driver.
    gpu = subprocess.check_output(
        ["nvidia-smi", "--query-gpu=name,compute_cap", "--format=csv,noheader"],
        text=True, timeout=10,
    ).strip()
    phase(f"gpu {gpu}")  # for rp up's boot report
    return root / archive_name(gpu)


def restore_caches(archive, local_root=Path("/")):
    # Only restore our known cache trees. data_filter rejects traversal and
    # links outside the destination even if an archive was accidentally damaged
    # or replaced on the shared volume.
    parents = {str(Path(name).parents[i]) for name in CACHE_DIRS
               for i in range(len(Path(name).parents) - 1)}

    def cache_filter(member, destination):
        name = member.name.rstrip("/")
        if member.isdir() and name in parents:
            return None  # a parent of a cache tree; created as needed anyway
        if not any(name == tree or name.startswith(tree + "/") for tree in CACHE_DIRS):
            raise tarfile.FilterError("unexpected path in compilation cache")
        return tarfile.data_filter(member, destination)

    with tarfile.open(archive, "r:") as source:
        source.extractall(local_root, filter=cache_filter)


def save_caches(archive, local_root=Path("/")):
    paths = [name for name in CACHE_DIRS if (local_root / name).is_dir()]
    if not paths:
        return
    temp = str(archive) + f".{os.getpid()}.tmp"
    try:
        subprocess.run(["tar", "-cpf", temp, "-C", str(local_root), *paths],
                       check=True, timeout=120)
        os.replace(temp, archive)
    finally:
        Path(temp).unlink(missing_ok=True)


def prepare():
    root = Path(os.environ["RPCTL_CACHE_DIR"])
    root.mkdir(parents=True, exist_ok=True)
    repo = os.environ["RPCTL_MODEL_REPO"]
    values = {}
    if os.environ.get("RPCTL_MODEL_CACHE_MODE") != "online":
        snapshot = validated_model(root, Path(os.environ["HF_HOME"]), repo)
        if snapshot:
            # Offline mode makes vLLM load the snapshot path instead of the
            # repo name, which gives a different compile hash than an online
            # boot. Both variants live in the same per-GPU archive, so the
            # steady state (always offline) hits the cache.
            values.update(HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1")
            phase("verified model cache; using offline metadata")
        else:
            phase("checking model cache with Hugging Face")
    if os.environ.get("RPCTL_STARTUP_CACHE") == "1":
        try:
            archive = archive_path(root)
            values["RPCTL_CACHE_ARCHIVE"] = str(archive)
            if archive.is_file():
                phase("restoring compilation caches")
                restore_caches(archive)
                phase("compilation caches restored")
            else:
                phase("first compilation for this hardware/configuration")
        except (OSError, subprocess.SubprocessError, tarfile.TarError) as error:
            phase(f"cache restore unavailable; compiling normally ({type(error).__name__})")
    Path("/run/rpctl-runtime.env").write_text(
        "".join(f"export {key}={shlex.quote(value)}\n" for key, value in values.items()))


PREWARM_THREADS = 8
PREWARM_BLOCK = 16 << 20


def prewarm(hf_home=None, repo=None):
    """Read the cached weights into the page cache from the first second of the
    boot, while vLLM spends ~60s importing and spawning its engine. vLLM's own
    prefetch only starts once the engine reaches weight loading, and on the
    network volume the read itself takes 60-200s, so overlapping the two is the
    largest saving available. It reads the same files vLLM would read moments
    later, so it costs no extra memory, and it adds nothing to the boot path:
    it runs in the background and vLLM never waits for it."""
    from concurrent.futures import ThreadPoolExecutor
    hf_home = Path(hf_home or os.environ["HF_HOME"])
    snapshot = cached_snapshot(hf_home, repo or os.environ["RPCTL_MODEL_REPO"])
    if snapshot is None:
        phase("no cached weights to prewarm")
        return 0
    # Small files first, read serially: vLLM reads config, tokenizer and
    # processor files ~20s into the boot, and behind 8 streams of shard reads
    # on the network volume those took 20s instead of 1s (2026-06-26).
    files = [p for p in snapshot.iterdir() if p.is_file()]
    shards = sorted(p for p in files if p.suffix == ".safetensors")
    small = [p for p in files if p.suffix != ".safetensors"]
    started = time.monotonic()
    for path in small:
        _read(path)
    # vLLM loads shards in name order; warming them in that order lets its
    # loader overtake the prewarm on cached shards instead of waiting.
    with ThreadPoolExecutor(PREWARM_THREADS) as pool:
        total = sum(pool.map(_read, shards))
    took = time.monotonic() - started
    phase(f"weights prewarmed: {total / 2**30:.2f} GiB in {took:.1f}s")
    return total


def _read(path):
    total = 0
    try:
        with open(path, "rb", buffering=0) as handle:
            while True:
                chunk = handle.read(PREWARM_BLOCK)
                if not chunk:
                    return total
                total += len(chunk)
    except OSError:
        return total


def skip_mm_warmup():
    """Drop vLLM's startup multimodal warmup: it runs one dummy image of the
    maximum size through the processor on a single CPU thread before the server
    may start, 16.5s of every boot (2026-06-26). It only exists to make the
    first image request faster; image input works the same without it. If this
    vLLM version has no such method, nothing changes and the warmup runs."""
    try:
        from vllm.renderers import base
    except Exception:
        return False
    if not hasattr(base.BaseRenderer, "_warmup_mm_processor"):
        return False

    def skipped(self, processor, *, log_prefix):
        base.logger.info("%s warmup skipped by rpctl (boot-time budget)", log_prefix)

    base.BaseRenderer._warmup_mm_processor = skipped
    return True


def run_vllm():
    """`vllm ...` with rpctl's startup patches applied, in the same process."""
    if os.environ.get("RPCTL_MM_WARMUP", "0") != "1":
        skip_mm_warmup()
    from vllm.entrypoints.cli.main import main
    sys.argv = ["vllm", *sys.argv[2:]]
    sys.exit(main())


ACTIVITY_METRICS = ("vllm:prompt_tokens_total", "vllm:generation_tokens_total")
WATCHDOG_POLL_S = 30
GRAPHQL = "https://api.runpod.io/graphql"
REST = "https://rest.runpod.io/v1"


def token_count(metrics_text):
    """Tokens vLLM has taken in and produced so far. Any change between two
    polls is activity: a streaming answer moves it every poll, while health
    checks, `rp status` and an open but idle chat never do. A request that is
    stuck (running, but producing nothing) does not move it either."""
    total = 0.0
    for line in metrics_text.splitlines():
        if line.startswith(ACTIVITY_METRICS):
            try:
                total += float(line.rsplit(None, 1)[1])
            except (IndexError, ValueError):
                pass
    return total


class IdleClock:
    """Decides when the pod has been idle for ttl_s, or alive for max_s.

    Wall-clock times are persisted per pod id on the network volume, so a
    container that crash-loops (RunPod restarts it, and the new container
    starts a new watchdog) carries on the same countdown instead of
    restarting it forever."""

    def __init__(self, path, pod_id, ttl_s, max_s, now):
        self.path, self.pod_id, self.ttl_s, self.max_s = path, pod_id, ttl_s, max_s
        self.last_tokens = None
        try:
            saved = json.loads(path.read_text())
            if saved["pod_id"] != pod_id:
                raise ValueError("another pod")
            self.started, self.last_activity = float(saved["started"]), float(saved["last_activity"])
        except (OSError, ValueError, KeyError, TypeError):
            # The clock starts with the container, not when vLLM is ready, so a
            # boot that never serves is shut down too.
            self.started = self.last_activity = now
        self.save(now)

    def tick(self, now, tokens):
        """tokens is None when /metrics could not be read, which counts as
        idle: an error must lead to a shutdown, never to running forever."""
        if tokens is not None:
            if self.last_tokens is not None and tokens != self.last_tokens:
                self.last_activity = now
            self.last_tokens = tokens
        self.save(now)
        return self.expired(now)

    def expired(self, now):
        if now - self.last_activity >= self.ttl_s:
            return f"idle for {(now - self.last_activity) / 60:.0f} min"
        if now - self.started >= self.max_s:
            return f"running for {(now - self.started) / 3600:.1f} h (hard cap)"
        return None

    def state(self, now):
        return {"pod_id": self.pod_id, "started": self.started,
                "last_activity": self.last_activity, "checked": now,
                "ttl_s": self.ttl_s, "max_s": self.max_s}

    def save(self, now):
        try:
            atomic_json(self.path, self.state(now))
        except OSError:
            pass  # the in-memory clock still works


def runpod_call(method, url, key, query=None):
    """(ok, detail) for one RunPod API call. Never raises."""
    data = json.dumps({"query": query}).encode() if query else None
    request = urllib.request.Request(
        url, method=method, data=data,
        # Cloudflare in front of RunPod answers 403 to urllib's default
        # User-Agent, whatever the key: that, not the key, was the 403 seen on
        # 2026-06-26.
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + key,
                 "User-Agent": "rpctl-watchdog/1"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            body = response.read()
    except urllib.error.HTTPError as error:
        return False, f"HTTP {error.code}"
    except OSError as error:
        return False, type(error).__name__
    if query:
        try:
            errors = json.loads(body or b"{}").get("errors")
        except ValueError:
            return False, "bad response"
        if errors:
            return False, str(errors[0].get("message", "error"))[:160]
    return True, "ok"


def can_manage(pod_id, key):
    """Whether RunPod lets this key see the pod: checked once at boot so a key
    that cannot stop the pod is reported straight away, not an hour later."""
    ok, detail = runpod_call("GET", f"{REST}/pods/{pod_id}", key)
    if ok:
        return True, detail
    ok, gql_detail = runpod_call("POST", GRAPHQL, key,
                                 'query { pod(input: {podId: "%s"}) { id } }' % pod_id)
    return ok, "ok" if ok else f"REST {detail}, GraphQL {gql_detail}"


def terminate_self(pod_id, key):
    """Terminate this pod, the same as `rp down`; if RunPod refuses that, stop
    it, which ends GPU billing too (rp reap terminates it later). A restricted
    key may be allowed on only one of the two APIs, so both are tried."""
    tried = []
    for label, method, url, query in (
            ("REST terminate", "DELETE", f"{REST}/pods/{pod_id}", None),
            ("GraphQL terminate", "POST", GRAPHQL,
             'mutation { podTerminate(input: {podId: "%s"}) }' % pod_id)):
        ok, detail = runpod_call(method, url, key, query)
        if ok:
            return "terminated"
        tried.append(f"{label}: {detail}")
    for label, method, url, query in (
            ("REST stop", "POST", f"{REST}/pods/{pod_id}/stop", None),
            ("GraphQL stop", "POST", GRAPHQL,
             'mutation { podStop(input: {podId: "%s"}) { id } }' % pod_id)):
        ok, detail = runpod_call(method, url, key, query)
        if ok:
            return "stopped"
        tried.append(f"{label}: {detail}")
    raise RuntimeError("; ".join(tried))


def watchdog(now=time.time, sleep=time.sleep):
    """Shuts the pod down after RPCTL_IDLE_TTL_S without tokens flowing, or
    after RPCTL_MAX_POD_S in total. Runs in the background from second 0 and
    is never waited on, so it adds nothing to the boot."""
    pod_id = os.environ.get("RUNPOD_POD_ID", "")
    # The pod-scoped key RunPod injects can terminate this pod (verified
    # 2026-06-26). RPCTL_WATCHDOG_KEY (RUNPOD_WATCHDOG_API_KEY) overrides it.
    injected = os.environ.get("RUNPOD_API_KEY", "")
    key = os.environ.get("RPCTL_WATCHDOG_KEY") or injected
    ttl_s = float(os.environ.get("RPCTL_IDLE_TTL_S") or 0)
    max_s = float(os.environ.get("RPCTL_MAX_POD_S") or 0) or float("inf")
    if not ttl_s:
        phase("watchdog off (IDLE_TTL_MIN=0)")
        return
    if not (pod_id and key):
        phase("WATCHDOG CANNOT STOP THIS POD: no RunPod key (set RUNPOD_WATCHDOG_API_KEY)")
        return
    clock = IdleClock(Path(os.environ["RPCTL_CACHE_DIR"]) / f"watchdog-{pod_id}.json",
                      pod_id, ttl_s, max_s, now())
    can_stop, detail = can_manage(pod_id, key)
    if injected and key != injected:
        # Diagnostic only: if RunPod's own key works, no dedicated key is needed.
        phase("watchdog: the pod's injected key %s"
              % ("can see this pod" if can_manage(pod_id, injected)[0] else "is refused"))
    if can_stop:
        phase(f"watchdog armed: idle {ttl_s / 60:.0f} min, cap {max_s / 3600:.1f} h")
    else:
        # The clock still runs and is persisted, so rp status can show it.
        phase(f"WATCHDOG CANNOT STOP THIS POD: RunPod refused its key ({detail})")
    request = urllib.request.Request(
        f"http://127.0.0.1:{os.environ['RPCTL_PORT']}/metrics",
        headers={"Authorization": "Bearer " + os.environ.get("VLLM_API_KEY", "")})
    reported = False
    while True:
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                tokens = token_count(response.read().decode(errors="replace"))
        except (OSError, ValueError):
            tokens = None
        reason = clock.tick(now(), tokens)
        if reason and not can_stop:
            if not reported:
                phase(f"watchdog: {reason}, but this pod cannot stop itself")
                reported = True
        elif reason:
            phase(f"watchdog: {reason}; shutting the pod down")
            try:
                phase(f"watchdog: pod {terminate_self(pod_id, key)}")
                return
            except RuntimeError as error:
                phase(f"WATCHDOG FAILED TO STOP THIS POD ({error}); retrying")
        sleep(WATCHDOG_POLL_S)


def _watchdog_forever():
    # A bug in the watchdog must not end it: that would leave the pod billing.
    while True:
        try:
            return watchdog()
        except Exception as error:
            phase(f"watchdog error ({type(error).__name__}); restarting it")
            time.sleep(WATCHDOG_POLL_S)


def watch():
    import threading
    threading.Thread(target=_watchdog_forever, name="watchdog").start()
    root = Path(os.environ["RPCTL_CACHE_DIR"])
    started = time.monotonic()
    request = urllib.request.Request(
        f"http://127.0.0.1:{os.environ['RPCTL_PORT']}/health",
        headers={"Authorization": "Bearer " + os.environ.get("VLLM_API_KEY", "")},
    )
    while time.monotonic() - started < 3600:
        try:
            with urllib.request.urlopen(request, timeout=2) as response:
                if response.status == 200:
                    break
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(2)
    else:
        return
    phase("model ready; saving startup caches")
    try:
        remember_model(root, Path(os.environ["HF_HOME"]), os.environ["RPCTL_MODEL_REPO"])
        archive = os.environ.get("RPCTL_CACHE_ARCHIVE")
        if archive:
            save_caches(archive)
        phase("startup caches saved")
        Path("/run/rpctl-cache-saved").touch()
    except (OSError, subprocess.SubprocessError) as error:
        phase(f"cache save failed ({type(error).__name__}); model is still serving")


if __name__ == "__main__":
    {"prepare": prepare, "watch": watch, "prewarm": prewarm,
     "vllm": run_vllm}[sys.argv[1]]()
