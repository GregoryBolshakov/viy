#!/usr/bin/env python3
"""fakellm: a scripted OpenAI-compatible model server on 127.0.0.1.

    python3 -I tools/fakellm.py SCENARIO.json --log REQUESTS.jsonl [--port N]

Serves what rp and viy call on a vLLM pod: GET /health, GET /v1/models and
POST /v1/chat/completions, streaming or not. The replies come from the
scenario's "responses" list, one per chat-completions request, in order
(format: tests/scenarios/README.md). Every request is appended to the log
as one JSON line: method, path, the headers that matter, and the parsed
body. A request the scenario did not expect, or one that fails a
response's "expect", gets HTTP 500 and a "mismatch" line in the log.

A scenario's "web" section stands in for the internet: Tavily's search and
extract answers in order at /tavily/search and /tavily/extract, and pages
for requests whose Host is not this server's (the clients' connections are
pointed here). Those requests are logged too, a page's with its "host".

The first stdout line is `listening http://127.0.0.1:PORT`; the server runs
until killed. Stdlib only, no network beyond the loopback socket. The Rust
twin for viy's own tests is viy_testkit::mock_llm, which serves the same
scenario format. Implementation plan 3.4.
"""
import argparse
import http.server
import json
import pathlib
import sys
import threading
import time

# Request headers kept in the log; the rest (host, length, encoding) say
# nothing about the client's behaviour.
LOGGED_HEADERS = ("authorization", "content-type", "accept", "user-agent")


def dumps(value):
    """JSON as vLLM writes it (pydantic): compact, raw UTF-8. serde_json's
    Display gives the same bytes, which viy_testkit::mock_llm relies on."""
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def chunk(model, delta=None, finish=None, usage=None):
    """One chat.completion.chunk as vLLM 0.27 frames it."""
    body = {"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 0,
            "model": model}
    if usage is not None:
        body["choices"] = []
        body["usage"] = usage
    else:
        body["choices"] = [{"index": 0, "delta": delta or {}, "logprobs": None,
                            "finish_reason": finish}]
    return "data: " + dumps(body) + "\n\n"


DEFAULT_USAGE = {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110}


def stream_events(model, events):
    """The SSE text of one streamed reply, as (text | ("delay", s) | "drop")."""
    yield chunk(model, {"role": "assistant", "content": ""})
    finish, usage = "stop", DEFAULT_USAGE
    for ev in events:
        if "reasoning" in ev:
            yield chunk(model, {"reasoning": ev["reasoning"]})
        elif "content" in ev:
            yield chunk(model, {"content": ev["content"]})
        elif "tool_call" in ev:
            tc = ev["tool_call"]
            head = {"index": tc.get("index", 0), "id": tc["id"], "type": "function",
                    "function": {"name": tc["name"], "arguments": ""}}
            yield chunk(model, {"tool_calls": [head]})
            for part in tc.get("argument_parts") or [tc.get("arguments", "")]:
                yield chunk(model, {"tool_calls": [{"index": head["index"],
                                                    "function": {"arguments": part}}]})
        elif "finish" in ev:
            finish = ev["finish"]
        elif "usage" in ev:
            usage = ev["usage"]
        elif "delay" in ev:
            yield ("delay", ev["delay"])
        elif "drop" in ev:
            yield "drop"
            return
        elif "raw" in ev:
            yield ev["raw"]
        else:
            raise ValueError(f"unknown stream event: {ev}")
    yield chunk(model, {"content": ""}, finish=finish)
    yield chunk(model, usage=usage)
    yield "data: [DONE]\n\n"


def tools_mode(body):
    """How the request offers tools: auto, none (in the prompt, not callable), omit."""
    if "tools" not in body:
        return "omit"
    return body.get("tool_choice", "auto")


def check_expect(expect, body):
    """The first way `body` differs from a response's "expect", or None."""
    for key, want in (expect or {}).items():
        if key == "stream":
            got = bool(body.get("stream"))
        elif key == "tools":
            got = tools_mode(body)
        elif key == "last_user":
            users = [m for m in body.get("messages", []) if m.get("role") == "user"]
            got = users[-1].get("content") if users else None
        elif key == "last_role":
            msgs = body.get("messages") or [{}]
            got = msgs[-1].get("role")
        else:
            return f"unknown expect key {key!r}"
        if got != want:
            return f"expected {key}={want!r}, got {got!r}"
    return None


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, scenario, log_path, port=0):
        super().__init__(("127.0.0.1", port), Handler)
        self.scenario = scenario
        self.model = scenario.get("model", "qwen3.8-abliterated")
        self.responses = list(scenario.get("responses") or [])
        self.n = 0
        web = scenario.get("web") or {}
        self.pages = web.get("pages") or {}
        self.tavily = {"search": list(web.get("search") or []),
                       "extract": list(web.get("extract") or [])}
        self.lock = threading.Lock()
        self.log = open(log_path, "a", encoding="utf-8")

    def record(self, entry):
        with self.lock:
            self.log.write(json.dumps(entry, ensure_ascii=False) + "\n")
            self.log.flush()

    def next_tavily(self, kind):
        with self.lock:
            queue = self.tavily[kind]
            return queue.pop(0) if queue else None

    def next_response(self):
        with self.lock:
            i = self.n
            self.n += 1
            return i, (self.responses[i] if i < len(self.responses) else None)


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _entry(self, body=None):
        headers = {k: self.headers[k] for k in LOGGED_HEADERS if self.headers[k] is not None}
        entry = {"method": self.command, "path": self.path, "headers": headers}
        if body is not None:
            entry["body"] = body
        return entry

    def _send(self, status, payload, ctype="application/json"):
        data = payload.encode() if isinstance(payload, str) else payload
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _is_page(self):
        own = f"127.0.0.1:{self.server.server_address[1]}"
        return (self.headers.get("Host") or own) != own

    def _page(self):
        """A scenario page for a request the client meant for the internet."""
        srv = self.server
        host = self.headers.get("Host")
        srv.record(dict(self._entry(), host=host))
        page = srv.pages.get(host + self.path)
        if page is None:
            return self._send(404, "not found", "text/plain")
        if isinstance(page, str):
            page = {"body": page}
        self.send_response(page.get("status", 200))
        data = page.get("body", "").encode()
        self.send_header("Content-Type", page.get("type", "text/html; charset=utf-8"))
        for k, v in (page.get("headers") or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        srv = self.server
        if self._is_page():
            return self._page()
        srv.record(self._entry())
        if self.path == "/health":
            self._send(200, b"", "text/plain")
        elif self.path == "/v1/models":
            models = {"object": "list", "data": [{
                "id": srv.model, "object": "model", "created": 0, "owned_by": "vllm",
                "root": srv.model, "parent": None,
                "max_model_len": srv.scenario.get("max_model_len", 65536)}]}
            self._send(200, dumps(models))
        else:
            self._send(404, "not found", "text/plain")

    def do_POST(self):
        srv = self.server
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        try:
            body = json.loads(raw)
        except ValueError:
            srv.record(dict(self._entry(), raw=raw.decode("utf-8", "replace"),
                            mismatch="body is not JSON"))
            return self._send(400, "body is not JSON", "text/plain")
        if self.path in ("/tavily/search", "/tavily/extract"):
            reply = srv.next_tavily(self.path.rsplit("/", 1)[1])
            if reply is None:
                srv.record(dict(self._entry(body), mismatch="no Tavily answer queued"))
                return self._send(500, "fakellm: no Tavily answer queued", "text/plain")
            srv.record(self._entry(body))
            return self._send(reply.get("status", 200), dumps(reply.get("body", {})))
        if self.path != "/v1/chat/completions":
            srv.record(dict(self._entry(body), mismatch="unknown path"))
            return self._send(404, "not found", "text/plain")
        i, resp = srv.next_response()
        entry = dict(self._entry(body), response=i)
        problem = ("no response queued" if resp is None
                   else check_expect(resp.get("expect"), body))
        if problem:
            srv.record(dict(entry, mismatch=problem))
            return self._send(500, f"fakellm scenario mismatch at request {i}: {problem}",
                              "text/plain")
        srv.record(entry)
        if resp.get("delay"):
            time.sleep(resp["delay"])
        if "status" in resp:
            return self._send(resp["status"], resp.get("body", ""), "text/plain")
        if "message" in resp:
            message = {"role": "assistant", "content": resp["message"]}
            finish = resp.get("finish", "stop")
            if resp.get("tool_calls"):
                message["tool_calls"] = [
                    {"id": c["id"], "type": "function",
                     "function": {"name": c["name"], "arguments": c["arguments"]}}
                    for c in resp["tool_calls"]]
                finish = resp.get("finish", "tool_calls")
            return self._send(200, dumps({
                "id": "chatcmpl-fake", "object": "chat.completion", "created": 0,
                "model": srv.model,
                "choices": [{"index": 0, "finish_reason": finish, "message": message}],
                "usage": resp.get("usage", DEFAULT_USAGE)}))
        self._stream(resp.get("stream") or [])

    def _stream(self, events):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        for piece in stream_events(self.server.model, events):
            if piece == "drop":
                # Cut the connection mid-reply, as a dying pod or tunnel does.
                self.wfile.flush()
                self.connection.shutdown(2)
                return
            if isinstance(piece, tuple):
                self.wfile.flush()
                time.sleep(piece[1])
                continue
            self.wfile.write(piece.encode())
        self.wfile.flush()


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("scenario", type=pathlib.Path)
    ap.add_argument("--log", type=pathlib.Path, required=True)
    ap.add_argument("--port", type=int, default=0)
    a = ap.parse_args(argv)
    srv = Server(json.loads(a.scenario.read_text()), a.log, a.port)
    print(f"listening http://127.0.0.1:{srv.server_address[1]}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
