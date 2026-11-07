#!/usr/bin/env python3
"""Tests of compare_requests: what counts as the same request (kb d031).

    python3 -I tools/parity/test_compare_requests.py

Runs on the recorded rp log of tests/scenarios/read-then-protected-write,
changed one way at a time. The pre-commit hook runs it.
"""
import copy
import importlib.util
import json
import pathlib
import unittest

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent.parent
spec = importlib.util.spec_from_file_location("compare_requests", HERE / "compare_requests.py")
cr = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cr)

_, LOG = cr.load(ROOT / "tests" / "scenarios" / "read-then-protected-write.rp.jsonl")


def turn_request():
    """A copy of the log and the index of the last streamed request (has tool results)."""
    log = copy.deepcopy(LOG)
    i = max(n for n, e in enumerate(log) if (e.get("body") or {}).get("stream"))
    return log, i


class SameContent(unittest.TestCase):
    def test_log_equals_itself(self):
        self.assertIsNone(cr.compare(LOG, copy.deepcopy(LOG)))

    def test_message_key_order_does_not_count(self):
        log, i = turn_request()
        body = log[i]["body"]
        body["messages"] = [dict(reversed(list(m.items()))) for m in body["messages"]]
        self.assertIsNone(cr.compare(LOG, log))

    def test_tool_result_formatting_does_not_count(self):
        log, i = turn_request()
        for m in log[i]["body"]["messages"]:
            if m["role"] == "tool":
                # serde_json's compact form, raw UTF-8 (kb d030)
                m["content"] = json.dumps(json.loads(m["content"]), separators=(",", ":"),
                                          ensure_ascii=False)
        self.assertIsNone(cr.compare(LOG, log))

    def test_call_arguments_formatting_does_not_count(self):
        log, i = turn_request()
        for m in log[i]["body"]["messages"]:
            for tc in m.get("tool_calls") or []:
                tc["function"]["arguments"] = json.dumps(json.loads(tc["function"]["arguments"]),
                                                         separators=(",", ":"))
        self.assertIsNone(cr.compare(LOG, log))

    def test_int_and_float_of_one_value_are_equal(self):
        log, i = turn_request()
        log[i]["body"]["temperature"] = int(log[i]["body"]["temperature"])
        self.assertIsNone(cr.compare(LOG, log))


class Differs(unittest.TestCase):
    def test_tool_schema_key_order_counts(self):
        log, i = turn_request()
        fn = log[i]["body"]["tools"][0]["function"]
        log[i]["body"]["tools"][0]["function"] = dict(reversed(list(fn.items())))
        d = cr.compare(LOG, log)
        self.assertIn("key order differs", d)
        self.assertIn(f"request {i} ", d)
        self.assertIn("body.tools[0].function", d)

    def test_one_character_of_the_system_prompt_counts(self):
        log, i = turn_request()
        sys_msg = log[i]["body"]["messages"][0]
        sys_msg["content"] = sys_msg["content"].replace("Today", "today", 1)
        self.assertNotEqual(sys_msg["content"], LOG[i]["body"]["messages"][0]["content"])
        d = cr.compare(LOG, log)
        self.assertIn("body.messages[0].content: strings differ at char", d)

    def test_a_tool_result_value_counts(self):
        log, i = turn_request()
        m = next(m for m in log[i]["body"]["messages"] if m["role"] == "tool")
        m["content"] = json.dumps(dict(json.loads(m["content"]), path="other.txt"))
        self.assertIn(".content.$json.path", cr.compare(LOG, log))

    def test_bool_is_not_one(self):
        log, i = turn_request()
        log[i]["body"]["stream"] = 1
        self.assertIn("body.stream", cr.compare(LOG, log))

    def test_a_missing_field_counts(self):
        log, i = turn_request()
        del log[i]["body"]["stream_options"]
        self.assertIn("body.stream_options: only in A", cr.compare(LOG, log))

    def test_an_extra_request_counts(self):
        log = copy.deepcopy(LOG) + [copy.deepcopy(LOG[-1])]
        self.assertIn("B has 1 more request", cr.compare(LOG, log))

    def test_authorization_counts(self):
        log, i = turn_request()
        log[i]["headers"]["authorization"] = "Bearer other"
        self.assertIn("Authorization", cr.compare(LOG, log))

    def test_a_user_agent_naming_viy_is_refused(self):
        log, i = turn_request()
        log[i]["headers"]["user-agent"] = "viy/0.1"
        self.assertIn("names viy", cr.compare(LOG, log))


    def test_a_tool_result_with_a_notice_compares_its_json_and_its_notice(self):
        notice = "\n\n[rpctl: tool-call budget for this turn is used up.]"
        a = {"messages": [{"role": "tool", "content": '{"a": 1, "b": [2]}' + notice}]}
        b = {"messages": [{"role": "tool", "content": '{"a":1,"b":[2]}' + notice}]}
        self.assertIsNone(cr.diff(cr._json_strings(a), cr._json_strings(b), "body"))
        c = {"messages": [{"role": "tool", "content": '{"a":1,"b":[2]}\n\n[other]'}]}
        a = {"messages": [{"role": "tool", "content": '{"a": 1, "b": [2]}' + notice}]}
        self.assertIsNotNone(cr.diff(cr._json_strings(a), cr._json_strings(c), "body"))


class Normalize(unittest.TestCase):
    def test_workspace_before_base_and_today(self):
        meta = {"base": "/tmp/r", "workspace": "/tmp/r/workspace", "today": "2026-07-12 (Sunday)"}
        got = cr.normalize({"k /tmp/r/workspace": ["in /tmp/r/workspace/a, /tmp/r/home",
                                                    "Today is 2026-07-12 (Sunday)."]}, meta)
        self.assertEqual(got, {"k {WORKSPACE}": ["in {WORKSPACE}/a, {BASE}/home",
                                                 "Today is {TODAY}."]})


if __name__ == "__main__":
    unittest.main(verbosity=0)
