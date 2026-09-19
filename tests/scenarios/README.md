# Scenarios

Scripted chats for the request-parity harness (implementation plan 3.4).
**All synthetic**: none is built from a real chat. If a scenario needs a real
chat's shape, Gregory provides a mock one.

Per scenario `<name>`:

- `<name>.json`: the script, read by `tools/fakellm.py` (the model side),
  `tools/drive_rp.py` (rp's side), `viy_testkit::mock_llm` and, from phase 3,
  `viy dev drive`.
- `<name>.rp.jsonl`: rp's requests for it at the rpctl pin, normalized
  (`python3 -I tools/drive_rp.py tests/scenarios/<name>.json --record`). The
  pre-commit hook re-runs rp and fails if they changed (`--check`).
- `<name>.rp.session.jsonl`: rp's session log for the same run (written by
  `--record` too, absent when the scenario turns CHAT_AUTOSAVE off): one
  event per line, without times, the session's id as `{ID}`, the run's
  paths and date as placeholders. `--check` compares it, keys in order.
- `<name>.rp.context.txt`: the same requests as a readable model-context
  snapshot (`viy_testkit::context_snapshot`; rewrite with
  `UPDATE_SNAPSHOTS=1 cargo test -p viy-testkit`).

Compare two logs with `python3 -I tools/parity/compare_requests.py A B`
(content, not bytes: kb d031). `python3 -I tools/drive_rp.py <name>.json
--client viy --check` runs the same scenario through `viy dev drive` (build
`target/debug/viy` first) and compares viy's requests and session log with
rp's recordings; `cargo test -p viy` does the same against `viy_testkit::mock_llm` for every
scenario with build tools off. Keep stream text ASCII: fakellm's
`text/event-stream` names no charset, so rp decodes it as ISO-8859-1.

## Format

```json
{
  "name": "plain-two-turns",
  "about": "what it covers",
  "config": {"TOOLS_ENABLED": "0"},
  "workspace": {"notes.txt": "file contents"},
  "max_model_len": 65536,
  "model": "qwen3.8-abliterated",
  "steps": [{"user": "a user turn"}, {"command": "/maxtok 2048"}],
  "approvals": [{"question": "let the model write it?", "answer": "n"}],
  "responses": [ ... ]
}
```

- `config`: on top of rpctl's DEFAULTS (and viy's), with `VLLM_API_KEY` set
  to `fake-key`. Set what would otherwise depend on the machine, such as
  `CONSULT_ENABLED` (a `claude` on PATH adds a tool).
- `workspace`: files created in the empty workspace before the chat starts.
- `steps`: in order; a `user` line goes through a turn, a `command` through
  the slash-command handler. Before the first step the driver does what
  `rp chat` does at startup: GET /v1/models and, with build tools on, the
  tool-parser preflight.
- `approvals`: the answers to the approver's questions, in the order asked;
  `question` must be part of the question asked. An unscripted question, or
  an answer never asked for, fails the run.
- `responses`: one per POST /v1/chat/completions, in order, including the
  preflight and the background title request (the driver waits for the
  title after each turn, so the order is fixed). Each is one of:
  - `{"stream": [events]}`: an SSE reply framed as vLLM 0.27 does (role
    chunk first, then one chunk per event, the finish reason, usage,
    `[DONE]`). Events: `{"reasoning": "..."}`, `{"content": "..."}`,
    `{"tool_call": {"id", "name", "arguments" | "argument_parts": [...],
    "index"}}`, `{"finish": "length"}` (default `stop`), `{"usage": {...}}`,
    `{"delay": seconds}`, `{"drop": true}` (cut the connection, no
    `[DONE]`), `{"raw": "data: ...\n\n"}`.
  - `{"message": "text"}`: a non-streamed chat.completion.
  - `{"status": 503, "body": "..."}`: an HTTP error.
  - Any of them may carry `"delay": seconds` before replying, and
    `"expect"`: checks on the request (`stream`: bool; `tools`: `auto`,
    `none` or `omit`; `last_user`: the last user message's content;
    `last_role`). A failed check gets HTTP 500 and fails the run.
