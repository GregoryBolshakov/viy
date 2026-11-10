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


def netguard_parse(url):
    """netguard.parse(url) as (scheme, host, port, path, url)."""
    from rpctl import netguard
    t = netguard.parse(url)
    return (t.scheme, t.host, t.port, t.path, t.url)


def netguard_redirect(base, location):
    """The next URL netguard.get follows: urljoin(target.url, loc.strip())."""
    from urllib.parse import urljoin
    return urljoin(base, location.strip())


def webfetch_link(base, href):
    """The URL _Extractor._link keeps for href on a page at base, or None."""
    from rpctl import webfetch
    p = webfetch._Extractor(base)
    n = p._link(href)
    return p.links[n - 1] if n else None


def webfetch_text(body, headers, url):
    """direct()'s text for a response already read from url (its body and
    lower-case headers): the charset, then HTML, JSON or plain text by
    type, as (kind, title, text, links)."""
    from unittest import mock
    from rpctl import netguard, webfetch
    resp = netguard.Response(200, headers, body, url, [], False)
    with mock.patch.object(webfetch.netguard, "get", return_value=resp):
        d = webfetch.direct(url)
    return (d.kind, d.title, d.text, d.links)


def webfetch_eval_pages():
    """The injection eval's pages (rpctl netaccess/evals/injection_eval.py),
    each its ARTICLE around one injection, by name, with the page's URL."""
    import importlib.util
    path = ROOT / snapshot()[0] / "netaccess" / "evals" / "injection_eval.py"
    spec = importlib.util.spec_from_file_location("injection_eval", path)
    ev = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(ev)
    return {"url": ev.URL,
            "pages": {name: ev.ARTICLE.format(inject=html) for name, html in ev.PAGES.items()}}


def search_tavily(query, k, recency, site, max_chars, status, body):
    """SearchClient.search with a key, requests.post answering `status` and
    `body` (text) and the DuckDuckGo fallback failing with "ddg": the JSON
    body sent (None when nothing was), then the results or the error."""
    from unittest import mock
    from rpctl import config, search
    cfg = dict(config.DEFAULTS, TAVILY_API_KEY="tvly-oracle-key", TAVILY_VAULT_TOKEN="",
               SEARCH_MAX_CHARS_PER_RESULT=str(max_chars))
    resp = mock.MagicMock()
    resp.status_code = status
    resp.text = body
    resp.json.side_effect = lambda: json.loads(body)
    client = search.SearchClient(cfg, ["literal-secret-value"])
    with mock.patch.object(search.requests, "post", return_value=resp) as post, \
            mock.patch.object(client, "_ddg", side_effect=search.SearchError("ddg")):
        try:
            res = client.search(query, k, recency, site)
            out = {"results": res.results, "backend": res.backend, "quality": res.quality}
        except search.SearchError as e:
            out = {"error": str(e)}
    out["sent"] = post.call_args.kwargs["json"] if post.called else None
    return out


def chat_fetch_decision(url, workspace, project_dir, known_urls, known_hosts,
                        allowed_hosts, taint, user_texts):
    """Chat._fetch_decision(url) in a chat whose workspace and project folder,
    URLs and hosts, taint and user messages (the first as this turn's) are
    given: (decision, the parsed URL or None, why), then the known URLs and
    hosts after it, sorted."""
    from pathlib import Path
    from unittest import mock
    from rpctl import chat, config
    cfg = dict(config.DEFAULTS, VLLM_API_KEY="k", SEARCH_ENABLED="1", TOOLS_ENABLED="0",
               INSTRUCTIONS_ENABLED="0", TAVILY_API_KEY="", TAVILY_VAULT_TOKEN="",
               CHAT_AUTO_TITLE="0", TRACE_ENABLED="0")
    c = chat.Chat(cfg, "http://127.0.0.1:1")
    c.known_urls, c.known_hosts = set(known_urls), set(known_hosts)
    c.allowed_hosts, c.web_taint = set(allowed_hosts), taint
    c._turn_user = user_texts[0] if user_texts else None
    c.messages = [{"role": "user", "content": t} for t in user_texts[1:]]
    with mock.patch.object(c, "_workspace", return_value=Path(workspace)), \
            mock.patch.object(chat.config, "PROJECT_DIR", Path(project_dir)):
        d, t, why = c._fetch_decision(url)
    return (d, t.url if t else None, why, sorted(c.known_urls), sorted(c.known_hosts))


def shutdown_parse_when(text, now):
    """shutdown.parse_when(text, now) with `now` as an ISO local time."""
    from datetime import datetime
    from rpctl import shutdown
    return shutdown.parse_when(text, datetime.fromisoformat(now))


def shutdown_is_chat(argv):
    """shutdown._is_chat on a command line given as text, UTF-8 encoded."""
    from rpctl import shutdown
    return shutdown._is_chat([a.encode() for a in argv])


def approve_parse_rules(text):
    """approve.load_rules() over a rules file holding `text`: the one I/O
    entry, a scratch file in the oracle's own temporary home."""
    from rpctl import approve
    path = pathlib.Path(tempfile.mkdtemp()) / "approval.rules"
    path.write_text(text)
    saved, approve.RULES_PATH = approve.RULES_PATH, path
    try:
        return approve.load_rules()
    finally:
        approve.RULES_PATH = saved
        shutil.rmtree(path.parent)


def mounts_note(mounts, gone):
    """mounts.note over [(path, rw)] and gone paths."""
    from rpctl import mounts as m
    return m.note([m.Mount(pathlib.Path(p), rw) for p, rw in mounts],
                  [pathlib.Path(p) for p in gone])


def mounts_parse_arg(arg):
    from rpctl import mounts as m
    return list(m.parse_arg(arg))


def egress_hosts_in_command(cmd):
    """egress.hosts_in_command with each set of ports as a sorted list."""
    from rpctl import egress
    return {h: sorted(p) for h, p in egress.hosts_in_command(cmd).items()}


def egress_parse_allow(text):
    from rpctl import egress
    exact, suffixes = egress.parse_allow(text)
    return [sorted(exact), sorted(suffixes)]


def egress_policy(command, user_hosts, allow, probes):
    """A Policy's describe() and its why_not() for each [host, port]."""
    from rpctl import egress
    p = egress.Policy(command, user_hosts, allow)
    return {"describe": p.describe(),
            "why_not": [p.why_not(h, port) for h, port in probes]}


def _toolbox_tools(manifests):
    """Tools from [[name, scope, TOOL.md text], ...] as toolbox.load reads
    them, without a filesystem: each at /tb/<scope>/<name>, problems not
    checked (a manifest that does not parse raises)."""
    from rpctl import toolbox
    out = []
    for name, scope, text in manifests:
        meta, body = toolbox.parse_manifest(text)
        out.append(toolbox.Tool(name, scope, pathlib.Path("/tb") / scope / name, meta, body))
    return out


def toolbox_index(manifests, limit):
    """toolbox.render_index over the tools, in the order given."""
    from rpctl import toolbox
    return toolbox.render_index(_toolbox_tools(manifests), limit)


def toolbox_search(manifests, query, limit):
    """toolbox.search as [[score, name], ...]."""
    from rpctl import toolbox
    return [[s, t.name] for s, t in toolbox.search(query, _toolbox_tools(manifests), limit)]


def toolbox_describe(manifests, full):
    """toolbox.describe of each tool."""
    from rpctl import toolbox
    return [toolbox.describe(t, full) for t in _toolbox_tools(manifests)]


def toolbox_used_in(names, cmd):
    """The names toolbox.used_in finds in cmd among tools with these names."""
    from rpctl import toolbox
    tools = [toolbox.Tool(n, "global", pathlib.Path("/tb") / n, {}, "") for n in names]
    return [t.name for t in toolbox.used_in(cmd, tools)]


def toolbox_suggest(name, names):
    """toolbox.suggest among tools with these names."""
    from rpctl import toolbox
    tools = [toolbox.Tool(n, "global", pathlib.Path("/tb") / n, {}, "") for n in names]
    return toolbox.suggest(name, tools)


def trace_monitor(calls, rounds):
    """A TurnMonitor with tracing off: on_round for each [reasoning, finish,
    secs], then on_tool for each [name, args_json, payload]. Returns each
    call's note, the alarm codes and what they said, colours stripped."""
    from rpctl import trace
    said = []
    t = trace.Tracer(lambda: "s1", enabled=False, echo=said.append)
    m = trace.TurnMonitor(t)
    for i, (reasoning, finish, secs) in enumerate(rounds):
        m.on_round(i, {"text": "", "reasoning": reasoning, "tool_calls": [],
                       "usage": None, "finish": finish}, secs)
    notes = [m.on_tool(n, a, p, 0) for n, a, p in calls]
    plain = [s.replace(trace.YELLOW, "").replace(trace.RED, "").replace(trace.RST, "")
             for s in said]
    return {"notes": notes, "alarms": m.alarms, "said": plain}


PUBLIC = "93.184.216.34"


def egress_handle(head, command, answers):
    """One connection through rpctl's Proxy._handle, over socketpairs: the
    command sends `head` (bytes) and stops sending. A name resolves to
    answers[name] (a list of addresses, or a string: the lookup fails with
    it), PUBLIC by default; upstream reads what it is sent until the proxy
    half-closes, then closes. Returns what the command read, what upstream
    read (None: never connected), the lookups, the connections, the report
    and the printed lines. No network and no files: local socket pairs."""
    import socket
    import threading
    import time
    from rpctl import egress
    resolved, connected, peers = [], [], []

    def resolver(host, port):
        resolved.append(host)
        a = answers.get(host, [PUBLIC])
        if isinstance(a, str):
            raise egress.netguard.FetchError(a)
        return a

    def connector(ip, port, timeout):
        connected.append([ip, port])
        a, b = socket.socketpair()
        peers.append(b)
        return a

    proxy = egress.Proxy(egress.Policy(command), resolver=resolver,
                         connector=connector, idle_timeout=2.0)
    client, conn = socket.socketpair()
    proxy._active = 1                       # _handle counts itself out
    t = threading.Thread(target=proxy._handle, args=(conn,), daemon=True)
    t.start()
    client.settimeout(5)
    client.sendall(head)
    client.shutdown(socket.SHUT_WR)
    end = time.monotonic() + 5
    while t.is_alive() and not peers and time.monotonic() < end:
        time.sleep(0.002)
    upstream = None
    if peers:
        b = peers[0]
        b.settimeout(5)
        upstream = b""
        while True:
            chunk = b.recv(65536)
            if not chunk:
                break
            upstream += chunk
        b.close()
    out = b""
    while True:
        try:
            chunk = client.recv(65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    t.join(5)
    client.close()
    return {"client": out, "upstream": upstream, "resolved": resolved,
            "connected": connected, "report": proxy.report(), "lines": proxy.lines()}


# The callable functions: request name -> (module, attribute), or a function
# defined above. Add entries as work packages need them; every rpctl entry
# must be free of I/O (plan 5.4).
FUNCTIONS = {
    # rpctl
    # Builds a Chat offline (no requests) in the oracle's temporary home.
    "chat.fetch_decision": chat_fetch_decision,
    "approve.destructive_reason": ("rpctl.approve", "destructive_reason"),
    "approve.net_ask_reason": ("rpctl.approve", "net_ask_reason"),
    # Reads the files a command names under the given workspace.
    "approve.files_for_review": ("rpctl.approve", "files_for_review"),
    "approve.parse_rules": approve_parse_rules,
    "approve.parse_verdict": ("rpctl.approve", "parse_verdict"),
    "approve.pipes_program_into_interpreter": ("rpctl.approve", "pipes_program_into_interpreter"),
    "approve.plain_pipeline": ("rpctl.approve", "plain_pipeline"),
    "approve.prefix_for": ("rpctl.approve", "prefix_for"),
    "approve.rules_allow_net": ("rpctl.approve", "rules_allow_net"),
    # Resolves the workspace and /tmp (file system reads, no writes).
    "approve.sandbox_tmp_rm": ("rpctl.approve", "sandbox_tmp_rm"),
    "egress.canonical_host": ("rpctl.egress", "canonical_host"),
    # Local socket pairs, no network.
    "egress.handle": egress_handle,
    "egress.hosts_in_command": egress_hosts_in_command,
    "egress.parse_allow": egress_parse_allow,
    "egress.policy": egress_policy,
    "egress.response": ("rpctl.egress", "_response"),
    "idle.decide": ("rpctl.idle", "decide"),
    "netguard.address_problem": ("rpctl.netguard", "address_problem"),
    "netguard.check_host": ("rpctl.netguard", "check_host"),
    "netguard.outbound_secret": ("rpctl.netguard", "outbound_secret"),
    "netguard.parse": netguard_parse,
    # get()'s redirect step, the standard library's urljoin as rpctl calls it.
    "netguard.redirect": netguard_redirect,
    "idle.limits": ("rpctl.idle", "limits"),
    "mounts.note": mounts_note,
    "mounts.parse_arg": mounts_parse_arg,
    "redact.any_pattern": redact_any_pattern,
    "redact.is_secret_name": ("rpctl.redact", "is_secret_name"),
    "redact.literal_secrets": ("rpctl.redact", "literal_secrets"),
    "redact.text": redact_text,
    # Resolves paths and asks git about core.hooksPath (no writes).
    "sandbox.protected_reason": ("rpctl.sandbox", "protected_reason"),
    "sandbox.secret_name_reason": ("rpctl.sandbox", "secret_name_reason"),
    "search.normalise": ("rpctl.search", "normalise"),
    "search.site": ("rpctl.search", "SearchClient._site"),
    # requests.post and the DuckDuckGo fallback replaced; no network.
    "search.tavily": search_tavily,
    "shutdown.fmt": ("rpctl.shutdown", "_fmt"),
    "shutdown.is_chat": shutdown_is_chat,
    "shutdown.parse_when": shutdown_parse_when,
    "trace.monitor": trace_monitor,
    "trace.repeated_sentence": ("rpctl.trace", "repeated_sentence"),
    "toolbox.describe": toolbox_describe,
    "toolbox.index": toolbox_index,
    "toolbox.parse_manifest": ("rpctl.toolbox", "parse_manifest"),
    "toolbox.search": toolbox_search,
    "toolbox.suggest": toolbox_suggest,
    "toolbox.used_in": toolbox_used_in,
    "untrusted.clean": ("rpctl.untrusted", "clean"),
    "untrusted.neutralize": ("rpctl.untrusted", "neutralize"),
    "untrusted.suspicious": ("rpctl.untrusted", "suspicious"),
    "untrusted.terminal_safe": ("rpctl.untrusted", "terminal_safe"),
    "untrusted.wrap": ("rpctl.untrusted", "wrap"),
    # Reads the snapshot's eval script for its synthetic pages; no network.
    "webfetch.eval_pages": webfetch_eval_pages,
    "webfetch.extract_html": ("rpctl.webfetch", "extract_html"),
    "webfetch.link": webfetch_link,
    "webfetch.links_in": ("rpctl.webfetch", "links_in"),
    "webfetch.passages": ("rpctl.webfetch", "passages"),
    "webfetch.text": webfetch_text,
    "webfetch.tidy": ("rpctl.webfetch", "_tidy"),
    "webfetch.window": ("rpctl.webfetch", "window"),
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
