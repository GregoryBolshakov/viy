#!/usr/bin/env python3
"""Run a scenario through rpctl's Chat, against fakellm, and log its requests.

    python3 -I tools/drive_rp.py SCENARIO.json [--out DIR] [--record | --check]
    python3 -I tools/drive_rp.py SCENARIO.json --client viy [--viy BIN] [--check]

rpctl comes from the pinned snapshot (target/rpctl-ref/<commit>/), never the
live checkout. The chat runs in a child `python3 -I` with HOME, every XDG_*
directory and the working directory in a fresh temporary tree, and a minimal
environment, so it never reaches Gregory's config, state or pod: the
last_activity it touches while streaming is the temporary one. The
workspace is built from the scenario's "workspace" files; the config is
rpctl's DEFAULTS plus the scenario's "config".

The child does what `rp chat` does around the requests (the REPL's startup
probes, then each step: a user turn through Chat.send, or a slash command
through handle_command) and answers the approver's questions from the
scenario's "approvals", in order, through the same prompt function a person
would type into. A background title request is waited for after each turn,
so the request order is the same on every run.

With --client viy the same tree, fakellm and workspace go to
`viy dev drive SCENARIO --url URL --result FILE` (default binary
target/debug/viy: build it first), which does the same around the
requests; --check then compares viy's requests with rp's recording.

Writes into DIR (default target/scenario-runs/<scenario>/<client>/): raw.jsonl
(what fakellm logged), requests.jsonl (normalized, meta header first, the
format tools/parity/compare_requests.py reads), meta.json and output.txt
(what the client printed), and session.jsonl: the chat's session log, one
event per line, without times, the session's id as {ID} and the run's paths
and date as placeholders. --record
copies rp's requests.jsonl to tests/scenarios/<scenario>.rp.jsonl and its
session.jsonl to <scenario>.rp.session.jsonl; --check compares with those
recordings, the session's events with their keys in order. Scenario format:
tests/scenarios/README.md. Implementation plan 3.4, WP 3.5.
"""
import argparse
import importlib.util
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCENARIOS = ROOT / "tests" / "scenarios"
ANSI = re.compile(r"\x1b\[[0-9;]*m|[\x01\x02]")


def _compare_module():
    spec = importlib.util.spec_from_file_location(
        "compare_requests", ROOT / "tools" / "parity" / "compare_requests.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def viy_names(value):
    """`value` with rp's model-visible names read as viy's, from
    crates/viy-testkit/rp-names.json (flags F21, kb d131): what --check
    compares viy's run with."""
    pairs = json.loads((ROOT / "crates" / "viy-testkit" / "rp-names.json")
                       .read_text(encoding="utf-8"))["map"]
    def walk(v):
        if isinstance(v, str):
            for rp, viy in pairs:
                v = v.replace(rp, viy)
            return v
        if isinstance(v, list):
            return [walk(x) for x in v]
        if isinstance(v, dict):
            return {walk(k): walk(x) for k, x in v.items()}
        return v
    return walk(value)


def snapshot():
    out = subprocess.run([str(ROOT / "tools" / "refsnap.sh")], cwd=ROOT, check=True,
                         capture_output=True, text=True).stdout.strip()
    path = (ROOT / out).resolve()
    return path, path.name


# ------------------------------------------------------------------ child
def child(job):
    """Runs inside the isolated python3 -I; `job` is the parent's JSON."""
    scenario = job["scenario"]
    sys.path.insert(0, job["snapshot"])
    from rpctl import chat, config, sessions

    answers = list(scenario.get("approvals") or [])
    asked = []

    def ask_line(prompt, prefill=None, on_shift_tab=None):
        question = ANSI.sub("", prompt).strip()
        if not answers:
            raise SystemExit(f"drive_rp: unscripted question: {question!r}")
        a = answers.pop(0)
        if a.get("question") and a["question"] not in question:
            raise SystemExit(f"drive_rp: expected a question with {a['question']!r}, "
                             f"got {question!r}")
        asked.append({"question": question, "answer": a["answer"]})
        print(a["answer"])
        return a["answer"]

    class Tty:
        """stdin as _confirm sees it: a terminal, so it asks."""
        def isatty(self):
            return True

        def read(self, *a):
            return ""

        readline = read

    chat._ask_line = ask_line
    sys.stdin = Tty()
    if scenario.get("web"):
        point_web_at(job["url"], scenario["web"])

    cfg = dict(config.DEFAULTS, VLLM_API_KEY=job["api_key"])
    cfg.update(scenario.get("config") or {})
    c = chat.Chat(cfg, job["url"])
    for path, _text, _cut in c._instructions():
        if not pathlib.Path(path).resolve().is_relative_to(job["base"]):
            raise SystemExit(f"drive_rp: an instruction file outside the run's tree "
                             f"would be sent: {path}")
    # Every chat saved, as viy saves them (it has no CHAT_AUTOSAVE, kb d083).
    c.session = sessions.SessionLog.new(config.PROJECT_DIR, c.model)
    # cmd_chat and repl() before the first prompt.
    c._report_instructions()
    c._discover_max_model_len()
    if c.tools_on:
        c._tools_supported()
    for step in scenario.get("steps") or []:
        if "user" in step:
            c._auto_yessed = []
            c.send(c._user_content(step["user"]))
            c._report_auto_yes()
            c._apply_title(wait=30)
        elif "command" in step:
            c.handle_command(step["command"])
            c._checkpoint()
        else:
            raise SystemExit(f"drive_rp: unknown step {step}")
    c._checkpoint()
    c._end_session("exit")
    if answers:
        raise SystemExit(f"drive_rp: {len(answers)} scripted answer(s) never asked")
    json.dump({"today": c._today, "asked": asked}, open(job["result"], "w"))


def point_web_at(url, web):
    """The internet as the scenario's "web" section: every name resolves to
    a public address (or the one `resolve` gives it), every page connection
    reaches fakellm in plain HTTP with the page's Host, and Tavily's
    endpoints are fakellm's."""
    from rpctl import netguard, search, webfetch
    port = int(url.rsplit(":", 1)[1])
    names = web.get("resolve") or {}
    netguard.resolve = lambda host, _port, timeout=5.0: [names.get(host, "93.184.216.34")]
    open_connection = netguard._open_connection
    netguard._open_connection = (
        lambda ip, _port, _scheme, host, timeout:
        open_connection("127.0.0.1", port, "http", host, timeout))
    search.TAVILY_URL = url + "/tavily/search"
    webfetch.TAVILY_EXTRACT_URL = url + "/tavily/extract"


# ----------------------------------------------------------------- parent
def run(scenario_path, out, viy=None):
    scenario = json.loads(scenario_path.read_text(encoding="utf-8"))
    name = scenario["name"]
    snap, commit = snapshot()
    out.mkdir(parents=True, exist_ok=True)
    for f in ("raw.jsonl", "requests.jsonl", "meta.json", "output.txt"):
        (out / f).unlink(missing_ok=True)
    base = pathlib.Path(tempfile.mkdtemp(prefix="drive-rp-")).resolve()
    try:
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "dumb",
               "PYTHONDONTWRITEBYTECODE": "1"}
        for var, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                         ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"),
                         ("XDG_CACHE_HOME", "cache"), ("XDG_RUNTIME_DIR", "runtime")):
            (base / sub).mkdir(mode=0o700)
            env[var] = str(base / sub)
        ws = base / "workspace"
        ws.mkdir()
        for rel, text in (scenario.get("workspace") or {}).items():
            p = (ws / rel).resolve()
            if not p.is_relative_to(ws):
                raise SystemExit(f"workspace file outside the workspace: {rel}")
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text, encoding="utf-8")
        for tool, files in (scenario.get("toolbox") or {}).items():
            for client in ("rpctl", "viy"):
                d = base / "data" / client / "toolbox" / tool
                d.mkdir(parents=True, mode=0o700)
                for rel, text in files.items():
                    (d / rel).write_text(text, encoding="utf-8")
                    if rel == tool:
                        (d / rel).chmod(0o755)

        server = subprocess.Popen(
            [sys.executable, "-I", str(ROOT / "tools" / "fakellm.py"), str(scenario_path),
             "--log", str(out / "raw.jsonl")],
            stdout=subprocess.PIPE, text=True, env=env, cwd=base)
        try:
            line = server.stdout.readline().strip()
            if not line.startswith("listening "):
                raise SystemExit(f"fakellm did not start: {line!r}")
            job = {"scenario": scenario, "snapshot": str(snap), "url": line.split()[1],
                   "api_key": "fake-key", "base": str(base),
                   "result": str(base / "result.json")}
            if viy:
                cmd = [str(viy), "dev", "drive", str(scenario_path), "--url", job["url"],
                       "--result", job["result"]]
            else:
                cmd = [sys.executable, "-I", str(pathlib.Path(__file__).resolve()),
                       "--child", json.dumps(job)]
            with open(out / "output.txt", "w", encoding="utf-8") as log:
                rc = subprocess.run(
                    cmd, cwd=ws, env=env, stdout=log, stderr=subprocess.STDOUT,
                    stdin=subprocess.DEVNULL, timeout=300).returncode
        finally:
            server.terminate()
            server.wait(10)
        if rc != 0:
            tail = (out / "output.txt").read_text(encoding="utf-8").splitlines()[-15:]
            raise SystemExit(f"{'viy' if viy else 'rp'}'s chat failed (exit {rc}); the end of "
                             f"{out / 'output.txt'}:\n" + "\n".join(tail))
        result = json.loads((base / "result.json").read_text())
        meta = {"client": "viy" if viy else "rp", "rpctl_commit": commit, "scenario": name,
                "base": str(base), "workspace": str(ws), "today": result["today"],
                "asked": result["asked"]}
        session = session_lines(base / "data" / ("viy" if viy else "rpctl") / "sessions", meta)
    finally:
        shutil.rmtree(base, ignore_errors=True)

    raw = [json.loads(x) for x in (out / "raw.jsonl").read_text(encoding="utf-8").splitlines()]
    bad = [e for e in raw if e.get("mismatch")]
    if bad:
        raise SystemExit(f"fakellm: {bad[0]['mismatch']} (request {raw.index(bad[0])}, "
                         f"{bad[0]['method']} {bad[0]['path']})")
    served = sum(1 for e in raw if "response" in e)
    if served != len(scenario.get("responses") or []):
        raise SystemExit(f"{meta['client']} made {served} chat requests; the scenario scripts "
                         f"{len(scenario['responses'])}")
    cmp = _compare_module()
    norm = cmp.normalize(raw, meta)
    header = {k: v for k, v in cmp.normalize(meta, meta).items()
              if k not in ("base", "workspace", "today")}
    cmp.write(out / "requests.jsonl", header, norm)
    (out / "session.jsonl").unlink(missing_ok=True)
    if session is not None:
        (out / "session.jsonl").write_text(session, encoding="utf-8")
    (out / "meta.json").write_text(json.dumps(meta, indent=1, ensure_ascii=False) + "\n")
    return name, out / "requests.jsonl"


def session_lines(sessions, meta):
    """The run's one session log, normalized: no `ts` or `created`, the id as
    {ID}, the run's paths and date as placeholders; None without a log."""
    logs = sorted(sessions.glob("*.jsonl")) if sessions.is_dir() else []
    if not logs:
        return None
    if len(logs) > 1:
        raise SystemExit(f"{len(logs)} session logs in {sessions}; a scenario keeps one")
    cmp = _compare_module()
    lines = []
    for raw in logs[0].read_text(encoding="utf-8").splitlines():
        ev = cmp.normalize(json.loads(raw), meta)
        ev.pop("ts", None)
        ev.pop("created", None)
        lines.append(json.dumps(ev, ensure_ascii=False).replace(logs[0].stem, "{ID}"))
    return "".join(line + "\n" for line in lines)


def same_session(cmp, want, got):
    """Whether two session logs hold the same events with the same content
    (kb d031): keys in order, and a string holding JSON (a tool result)
    compared as the JSON it holds, since viy writes model-visible JSON
    compactly (kb d030)."""
    if want is None or got is None:
        return False
    a, b = want.splitlines(), got.splitlines()
    if len(a) != len(b):
        return False
    for la, lb in zip(a, b):
        ea, eb = json.loads(la), json.loads(lb)
        if list(ea) != list(eb):
            return False
        wrap = lambda e: cmp._json_strings({"messages": [e["message"]], **e}) \
            if isinstance(e.get("message"), dict) else e
        if cmp.diff(wrap(ea), wrap(eb), "event"):
            return False
    return True


def main(argv):
    if argv[:1] == ["--child"]:
        child(json.loads(argv[1]))
        return 0
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("scenario", type=pathlib.Path)
    ap.add_argument("--out", type=pathlib.Path)
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--record", action="store_true",
                   help="save the log as tests/scenarios/<name>.rp.jsonl")
    g.add_argument("--check", action="store_true",
                   help="compare the log with tests/scenarios/<name>.rp.jsonl")
    ap.add_argument("--client", choices=("rp", "viy"), default="rp")
    ap.add_argument("--viy", type=pathlib.Path, default=ROOT / "target" / "debug" / "viy",
                    help="the viy binary (--client viy)")
    a = ap.parse_args(argv)
    viy = a.viy.resolve() if a.client == "viy" else None
    if viy and a.record:
        ap.error("--record keeps rp's requests; viy's are checked against them")
    if viy and not viy.is_file():
        ap.error(f"{viy} does not exist: cargo build -p viy")
    name = json.loads(a.scenario.read_text(encoding="utf-8"))["name"]
    out = (a.out or ROOT / "target" / "scenario-runs" / name / a.client).resolve()
    name, log = run(a.scenario.resolve(), out, viy)
    n = len(log.read_text(encoding="utf-8").splitlines()) - 1
    print(f"{name}: {n} requests logged in {log.relative_to(ROOT) if log.is_relative_to(ROOT) else log}")
    recorded = SCENARIOS / f"{name}.rp.jsonl"
    session = log.with_name("session.jsonl")
    recorded_session = SCENARIOS / f"{name}.rp.session.jsonl"
    if a.record:
        shutil.copyfile(log, recorded)
        print(f"recorded {recorded.relative_to(ROOT)}")
        recorded_session.unlink(missing_ok=True)
        if session.exists():
            shutil.copyfile(session, recorded_session)
            print(f"recorded {recorded_session.relative_to(ROOT)}")
    elif a.check:
        cmp = _compare_module()
        want_log = cmp.load(recorded)[1]
        if viy:
            want_log = viy_names(want_log)
        d = cmp.compare(want_log, cmp.load(log)[1])
        if d:
            print(f"differs from {recorded.relative_to(ROOT)}: {d}")
            return 1
        want = recorded_session.read_text(encoding="utf-8") if recorded_session.exists() else None
        if viy and want is not None:
            want = "".join(json.dumps(viy_names(json.loads(line)), ensure_ascii=False) + "\n"
                           for line in want.splitlines())
        got = session.read_text(encoding="utf-8") if session.exists() else None
        if want != got and not same_session(cmp, want, got):
            print(f"the session log differs from {recorded_session.relative_to(ROOT)}:\n"
                  f"--- recorded\n{want}--- this run\n{got}")
            return 1
        print(f"same content as {recorded.relative_to(ROOT)}"
              + (f" and {recorded_session.name}" if want is not None else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
