"""rpctl's tests of startup.py, run on the copy viy embeds
(crates/viy-pod/assets/startup.py, kb d003). startup.py stays Python, so its
tests do too; the Rust test boot_tests.rs::startup_py_passes_its_tests runs
this file with python3 under cargo test.

Ported from rpctl a51d1ca: tests/test_boot_budget.py, tests/test_startup.py
and tests/test_idle.py, the tests that touch startup.py alone; the ones that
also build the boot script or the pod body are Rust tests in boot_tests.rs.
Only the import differs. The hash in test_foreground_cache_code_is_unchanged
is rpctl's approved value (flags.md F17): a different hash means the
foreground cache code changed, which needs a measured boot first.

    python3 -I crates/viy-pod/src/startup_tests.py
"""
import hashlib
import importlib.util
import inspect
import io
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ASSET = Path(__file__).resolve().parent.parent / "assets" / "startup.py"
_spec = importlib.util.spec_from_file_location("startup", ASSET)
startup = importlib.util.module_from_spec(_spec)
sys.modules["startup"] = startup
_spec.loader.exec_module(startup)

HOW_TO_APPROVE = (
    "\nThe pod's boot path changed. Boot-time budget is a hard 180s (kb d004).\n"
    "1. Measure a real boot with the change and read the breakdown.\n"
    "2. It must not be slower than before, and the total must stay within budget.\n"
    "3. Only then update {name}, here and in rpctl's tests/test_boot_budget.py.\n"
)

# --------------------------------------------------- tests/test_boot_budget.py

# Source hash of the code that runs in the foreground before vLLM starts.
FOREGROUND_FUNCTIONS = (startup.prepare, startup.validated_model,
                        startup.cached_snapshot, startup.archive_path,
                        startup.restore_caches)
# Approved 2026-06-26 after a measured boot: 126s total (restore tolerates
# parent dirs; cache hit, torch.compile 5s). See `rp boots`.
APPROVED_FOREGROUND_SOURCE = "22ceb3f6674cd758"


def foreground_source_hash():
    source = "".join(inspect.getsource(fn) for fn in FOREGROUND_FUNCTIONS)
    return hashlib.sha256(source.encode()).hexdigest()[:16]


class BootBudgetTests(unittest.TestCase):
    def test_foreground_cache_code_is_unchanged(self):
        self.assertEqual(
            foreground_source_hash(), APPROVED_FOREGROUND_SOURCE,
            HOW_TO_APPROVE.format(name="APPROVED_FOREGROUND_SOURCE"))

    def test_prepare_never_touches_the_network(self):
        def refuse(*args, **kwargs):
            raise AssertionError("prepare() opened a network connection")
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(socket.socket, "connect", refuse), \
                patch.object(startup, "archive_path",
                             return_value=Path(directory) / "none.tar"), \
                patch.object(startup, "Path", side_effect=lambda p: Path(directory) / Path(p).name
                             if str(p).startswith("/run") else Path(p)), \
                patch.dict("os.environ", {"RPCTL_CACHE_DIR": directory,
                                          "RPCTL_MODEL_REPO": "o/m",
                                          "HF_HOME": directory,
                                          "RPCTL_STARTUP_CACHE": "1"}):
            startup.prepare()


class PrewarmTests(unittest.TestCase):
    def test_mm_warmup_patch_is_harmless_without_vllm(self):
        self.assertFalse(startup.skip_mm_warmup())

    def test_prewarm_reads_every_shard(self):
        with tempfile.TemporaryDirectory() as directory:
            hf = Path(directory)
            base = hf / "hub/models--o--m"
            snap = base / "snapshots/abc"
            snap.mkdir(parents=True)
            (base / "refs").mkdir()
            (base / "refs/main").write_text("abc")
            for i in range(3):
                (snap / f"model-{i}.safetensors").write_bytes(b"x" * (1000 + i))
            (snap / "config.json").write_text("{}")
            self.assertEqual(startup.prewarm(hf, "o/m"), 3003)
            self.assertEqual(startup.prewarm(hf, "o/other"), 0)


# ------------------------------------------------------ tests/test_startup.py

class StartupTests(unittest.TestCase):
    def test_offline_requires_successful_boot_and_intact_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            hf = root / 'hf'
            model = hf / 'hub/models--owner--model'
            snapshot = model / 'snapshots/abc123'
            snapshot.mkdir(parents=True)
            (model / 'refs').mkdir()
            (model / 'refs/main').write_text('abc123')
            shard = snapshot / 'weights.safetensors'
            shard.write_bytes(b'weights')
            self.assertIsNone(startup.validated_model(root, hf, 'owner/model'))
            startup.remember_model(root, hf, 'owner/model')
            self.assertEqual(startup.validated_model(root, hf, 'owner/model'), snapshot)
            shard.write_bytes(b'truncated')
            self.assertIsNone(startup.validated_model(root, hf, 'owner/model'))
            shard.unlink()
            self.assertIsNone(startup.validated_model(root, hf, 'owner/model'))

    def test_cache_archive_roundtrip_preserves_compiled_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, target = root / 'source', root / 'target'
            compiled = source / 'root/.cache/vllm/graph.so'
            compiled.parent.mkdir(parents=True)
            compiled.write_bytes(b'compiled-artifact')
            compiled.chmod(0o700)
            archive = root / 'cache.tar'
            startup.save_caches(archive, source)
            archive.chmod(0o666)  # Permissions seen on the FUSE volume.
            startup.restore_caches(archive, target)
            restored = target / 'root/.cache/vllm/graph.so'
            self.assertEqual(restored.read_bytes(), compiled.read_bytes())
            self.assertEqual(restored.stat().st_mode & 0o777, 0o700)

    def test_cache_archive_key_ignores_driver_version(self):
        calls = []
        def smi(args, **kw):
            calls.append(args)
            return 'NVIDIA A100-SXM4-80GB, 8.0\n'
        with patch.object(startup.subprocess, 'check_output', side_effect=smi):
            first = startup.archive_path(Path('/c'))
            self.assertEqual(first, startup.archive_path(Path('/c')))
        self.assertNotIn('driver_version', calls[0][1])

    def test_cache_archive_survives_image_and_config_changes(self):
        # The boot-script half (no RPCTL_CACHE_KEY or RPCTL_IMAGE in it) is
        # boot_tests.rs::cache_archive_survives_image_and_config_changes.
        self.assertNotEqual(startup.archive_name('NVIDIA A100-SXM4-80GB, 8.0'),
                            startup.archive_name('NVIDIA H100 NVL, 9.0'))

    def test_cache_archive_tolerates_parent_directories(self):
        # tar -C <dir> root adds root/ and root/.cache/ entries; those must not
        # make the whole cache unusable (FilterError cost a full recompile).
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            compiled = root / 'src/root/.cache/vllm/x.bin'
            compiled.parent.mkdir(parents=True)
            compiled.write_bytes(b'x')
            archive = root / 'cache.tar'
            subprocess.run(['tar', '-cf', str(archive), '-C', str(root / 'src'), 'root'],
                           check=True)
            startup.restore_caches(archive, root / 'target')
            self.assertEqual((root / 'target/root/.cache/vllm/x.bin').read_bytes(), b'x')

    def test_cache_archive_rejects_unrelated_paths(self):
        import tarfile
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / 'cache.tar'
            with tarfile.open(archive, 'w') as output:
                entry = tarfile.TarInfo('etc/ssh/sshd_config')
                entry.size = 4
                output.addfile(entry, io.BytesIO(b'evil'))
            with self.assertRaises(tarfile.FilterError):
                startup.restore_caches(archive, root / 'target')
            self.assertFalse((root / 'target/etc/ssh/sshd_config').exists())


# --------------------------------------------------------- tests/test_idle.py

METRICS = """\
# HELP vllm:prompt_tokens_total Number of prefill tokens processed.
vllm:num_requests_running{engine="0",model_name="m"} 1.0
vllm:prompt_tokens_total{engine="0",model_name="m"} %s
vllm:generation_tokens_total{engine="0",model_name="m"} %s
"""

HOUR = 3600


class TokenCountTests(unittest.TestCase):
    def test_sums_prompt_and_generated_tokens_only(self):
        self.assertEqual(startup.token_count(METRICS % ("120.0", "30.0")), 150.0)
        self.assertEqual(startup.token_count("garbage"), 0.0)


class IdleClockTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.path = Path(self._tmp.name) / "watchdog-p1.json"

    def clock(self, now, pod="p1"):
        return startup.IdleClock(self.path, pod, ttl_s=HOUR, max_s=8 * HOUR, now=now)

    def test_streaming_refreshes_and_silence_expires(self):
        c = self.clock(0)
        self.assertIsNone(c.tick(30, 100))       # first sample is a baseline
        self.assertIsNone(c.tick(3000, 150))     # tokens moved: activity
        self.assertIsNone(c.tick(3000 + HOUR - 1, 150))
        self.assertIn("idle", c.tick(3000 + HOUR, 150))

    def test_unchanged_counters_and_unreadable_metrics_count_as_idle(self):
        c = self.clock(0)
        c.tick(30, 100)
        c.tick(1800, None)
        self.assertIn("idle", c.tick(HOUR, 100))

    def test_a_boot_that_never_serves_expires(self):
        c = self.clock(0)
        self.assertIn("idle", c.tick(HOUR, None))

    def test_hard_cap_despite_activity(self):
        c = self.clock(0)
        tokens = 0
        for t in range(0, 8 * HOUR, 600):
            tokens += 10
            self.assertIsNone(c.tick(t, tokens))
        self.assertIn("hard cap", c.tick(8 * HOUR, tokens + 10))

    def test_a_restarted_container_keeps_the_countdown(self):
        c = self.clock(0)
        c.tick(30, 100)
        c.tick(600, 200)                          # last activity at 600
        restarted = self.clock(2000)              # crash loop: new watchdog
        self.assertEqual(restarted.last_activity, 600)
        self.assertIn("idle", restarted.tick(600 + HOUR, 0))

    def test_another_pods_clock_is_not_inherited(self):
        self.clock(0).tick(30, 100)
        other = self.clock(5000, pod="p2")
        self.assertEqual(other.started, 5000)


class TerminateTests(unittest.TestCase):
    def test_rest_terminate_first(self):
        with patch.object(startup, "runpod_call", return_value=(True, "ok")) as call:
            self.assertEqual(startup.terminate_self("p1", "k"), "terminated")
        self.assertEqual(call.call_args_list[0][0][:2], ("DELETE", startup.REST + "/pods/p1"))

    def test_graphql_terminate_when_rest_is_refused(self):
        with patch.object(startup, "runpod_call", side_effect=[(False, "HTTP 403"), (True, "ok")]) as call:
            self.assertEqual(startup.terminate_self("p1", "k"), "terminated")
        self.assertIn("podTerminate", call.call_args_list[1][0][3])

    def test_stop_when_terminate_is_refused(self):
        with patch.object(startup, "runpod_call",
                          side_effect=[(False, "x"), (False, "x"), (True, "ok")]):
            self.assertEqual(startup.terminate_self("p1", "k"), "stopped")

    def test_raises_when_everything_is_refused(self):
        with patch.object(startup, "runpod_call", return_value=(False, "HTTP 403")):
            with self.assertRaises(RuntimeError) as caught:
                startup.terminate_self("p1", "k")
        self.assertEqual(str(caught.exception).count("HTTP 403"), 4)

    def test_dedicated_key_wins_over_the_injected_one(self):
        seen = []
        with tempfile.TemporaryDirectory() as d, \
                patch.object(startup, "can_manage",
                             side_effect=lambda pod, key: seen.append(key) or (True, "ok")), \
                patch.object(startup.urllib.request, "urlopen", side_effect=OSError), \
                patch.object(startup, "terminate_self", return_value="terminated"), \
                patch.dict("os.environ", {"RUNPOD_POD_ID": "p1", "RUNPOD_API_KEY": "injected",
                                          "RPCTL_WATCHDOG_KEY": "mine",
                                          "RPCTL_IDLE_TTL_S": "60", "RPCTL_CACHE_DIR": d,
                                          "RPCTL_PORT": "8000"}):
            clock = [0.0]
            startup.watchdog(now=lambda: clock[0],
                             sleep=lambda s: clock.__setitem__(0, clock[0] + s))
        self.assertEqual(seen, ["mine", "injected"])  # then the diagnostic probe


class WatchdogLoopTests(unittest.TestCase):
    def test_shuts_down_after_an_idle_hour_and_retries_failures(self):
        clock = [0.0]

        def sleep(s):
            clock[0] += s

        def urlopen(request, timeout):
            return io.BytesIO((METRICS % ("5", "5")).encode())

        calls = []

        def terminate(pod_id, key):
            calls.append(clock[0])
            if len(calls) == 1:
                raise RuntimeError("transient")
            return "terminated"

        with tempfile.TemporaryDirectory() as d, \
                patch.object(startup.urllib.request, "urlopen", urlopen), \
                patch.object(startup, "can_manage", return_value=(True, "ok")), \
                patch.object(startup, "terminate_self", terminate), \
                patch.dict("os.environ", {"RUNPOD_POD_ID": "p1", "RUNPOD_API_KEY": "k",
                                          "RPCTL_IDLE_TTL_S": "3600", "RPCTL_MAX_POD_S": "28800",
                                          "RPCTL_CACHE_DIR": d, "RPCTL_PORT": "8000"}):
            startup.watchdog(now=lambda: clock[0], sleep=sleep)
        self.assertEqual(len(calls), 2)
        self.assertGreaterEqual(calls[0], HOUR)
        self.assertLess(calls[0], HOUR + 2 * startup.WATCHDOG_POLL_S)


if __name__ == "__main__":
    unittest.main()
