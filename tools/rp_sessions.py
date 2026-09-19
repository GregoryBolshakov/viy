#!/usr/bin/env python3
"""rp's side of a session moving between rp and viy: rpctl's sessions.py at
the pin, run on a sessions directory the caller names.

    python3 -I tools/rp_sessions.py ACTION '{"dir": "...", ...}'

prints one JSON value. Actions:

    start     {dir, project, model, system, messages, title?}  a new chat; its id
    continue  {dir, id, system, messages, title?}   open, sync, set_auto_title,
                                                    close; replay() afterwards
    replay    {dir, id}                             replay()
    list      {dir, scope}                          list_sessions(scope)
    search    {dir, query, scope}                   search(query, scope)
    copy      {dir, id}                             SessionLog.copy, closed; the
                                                    copy's id
    trash     {dir, id}                             "ok" or "busy"
    open      {dir, id}                             "ok" (and closed) or "busy"

Run by viy-agent's sessions tests with `--features oracle` (implementation
plan WP 3.5: "a chat from rp resumes in viy and back"). HOME and XDG_* point
into a fresh temporary directory before rpctl is imported, so nothing
reaches Gregory's real config or chats; only `dir` is written.
"""
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import oracle  # noqa: E402  (the snapshot and the isolation)


def main(action, args):
    snap, _ = oracle.snapshot()
    oracle.isolate()
    sys.path.insert(0, str(snap))
    from rpctl import config, sessions
    config.SESSIONS_DIR = pathlib.Path(args["dir"])
    if action == "start":
        log = sessions.SessionLog.new(args.get("project"), args.get("model"))
        log.sync(args.get("system"), args["messages"])
        if args.get("title"):
            log.rename(args["title"])
        log.close("exit")
        return log.id
    if action == "continue":
        log, _ = sessions.SessionLog.open(args["id"])
        log.sync(args.get("system"), args["messages"])
        if args.get("title"):
            log.set_auto_title(args["title"])
        log.close("exit")
        return sessions.replay(log.path)
    if action == "copy":
        twin, _ = sessions.SessionLog.copy(args["id"])
        twin.close("exit")
        return twin.id
    if action == "replay":
        return sessions.replay(sessions.path_of(args["id"]))
    if action == "list":
        return sessions.list_sessions(args.get("scope"))
    if action == "search":
        return sessions.search(args["query"], args.get("scope"))
    if action in ("trash", "open"):
        try:
            if action == "trash":
                sessions.trash(args["id"])
            else:
                sessions.SessionLog.open(args["id"])[0].close("exit")
            return "ok"
        except sessions.SessionBusy:
            return "busy"
    raise SystemExit(f"rp_sessions: unknown action {action}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__.strip().split("\n\n")[1])
    print(json.dumps(main(sys.argv[1], json.loads(sys.argv[2])), ensure_ascii=False))
