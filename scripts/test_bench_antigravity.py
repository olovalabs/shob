import io
import json
import queue
from types import SimpleNamespace
import time
import unittest

from bench_antigravity import PROBE_PREFIX, run_turn, summarize


def probe(status, elapsed_ms):
    return {"type": "extension_ui_request", "method": "notify", "message":
            PROBE_PREFIX + json.dumps({"event": "http_headers", "status": status, "elapsed_ms": elapsed_ms})}


class AntigravityBenchmarkTest(unittest.TestCase):
    def run_fixture(self, *frames):
        pending = queue.Queue()
        start = time.monotonic() - 0.025
        for index, frame in enumerate(frames, start=1):
            pending.put((start + index / 1000, frame))
        return run_turn(SimpleNamespace(stdin=io.BytesIO()), pending, "sample", "fixture", start, 0.05)

    def test_retries_and_missing_reasoning(self):
        turn = self.run_fixture(
            {"type": "response", "id": "sample", "success": True},
            {"type": "turn_start"},
            probe(503, 400),
            {"type": "auto_retry_start"},
            {"type": "turn_start"},
            probe(None, 500),
            probe(200, 600),
            {"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "hello"}},
            {"type": "prompt_result", "id": "sample", "status": "completed"},
        )
        report = summarize([turn])
        self.assertEqual(report["outcomes"], {"completed": 1})
        self.assertEqual(report["admission_ms"]["median"], 1)
        self.assertEqual(report["model_rounds"]["median"], 2)
        self.assertEqual(report["retry_events"], 1)
        self.assertEqual(report["http_status_counts"], {"503": 1, "transport_error": 1, "200": 1})
        self.assertEqual(report["provider_request_to_headers_ms"], {"n": 2, "median": 500, "p95": 600})
        self.assertEqual(report["first_reasoning_ms"], {"n": 0})
        self.assertEqual(report["first_answer_ms"]["median"], 8)

    def test_timeout_retains_http_attempt_but_excludes_success_metrics(self):
        turn = self.run_fixture(probe(200, 400))
        report = summarize([turn])
        self.assertEqual(report["outcomes"], {"timeout": 1})
        self.assertGreaterEqual(turn.total_ms, 49)
        self.assertEqual(report["total_ms"], {"n": 0})
        self.assertEqual(report["http_status_counts"], {"200": 1})

    def test_eof_and_rejection_are_reported(self):
        self.assertEqual(self.run_fixture(None).outcome, "rpc_closed")
        self.assertEqual(self.run_fixture({"type": "response", "id": "sample", "success": False}).outcome,
                         "rejected")

    def test_overlapping_tools_keep_argument_and_execution_boundaries_separate(self):
        turn = self.run_fixture(
            {"type": "extension_ui_request", "method": "notify",
             "message": PROBE_PREFIX + json.dumps({"event": "provider_request"})},
            {"type": "message_update", "assistantMessageEvent":
             {"type": "toolcall_end", "toolCall": {"id": "a"}}},
            {"type": "message_update", "assistantMessageEvent":
             {"type": "toolcall_end", "toolCall": {"id": "b"}}},
            {"type": "tool_execution_start", "toolCallId": "b"},
            {"type": "tool_execution_start", "toolCallId": "a"},
            {"type": "tool_execution_end", "toolCallId": "a", "isError": True},
            {"type": "tool_execution_end", "toolCallId": "b"},
            {"type": "extension_ui_request", "method": "notify",
             "message": PROBE_PREFIX + json.dumps({"event": "provider_request"})},
            {"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "done"}},
            {"type": "prompt_result", "id": "sample", "status": "completed"},
        )
        cycles = summarize([turn])["tool_cycles"]
        self.assertEqual(cycles["completed"], 2)
        self.assertEqual(cycles["failed"], 1)
        self.assertEqual(cycles["request_to_arguments_ms"]["median"], 1.5)
        self.assertEqual(cycles["arguments_to_start_ms"]["median"], 2)
        self.assertEqual(cycles["execution_ms"]["median"], 2)
        self.assertEqual(cycles["end_to_next_request_ms"]["median"], 1.5)
        self.assertEqual(cycles["end_to_next_delta_ms"]["median"], 2.5)

    def test_missing_argument_boundary_is_not_inferred_from_tool_start(self):
        turn = self.run_fixture(
            {"type": "tool_execution_start", "toolCallId": "a"},
            {"type": "tool_execution_end", "toolCallId": "a"},
            {"type": "prompt_result", "id": "sample", "status": "completed"},
        )
        cycles = summarize([turn])["tool_cycles"]
        self.assertEqual(cycles["execution_ms"]["median"], 1)
        self.assertEqual(cycles["request_to_arguments_ms"], {"n": 0})
        self.assertEqual(cycles["arguments_to_start_ms"], {"n": 0})


if __name__ == "__main__":
    unittest.main()
