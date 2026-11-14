#!/usr/bin/env python3
"""Run the Kathamo bridge benchmark against a deterministic local SSE endpoint.

Build with cargo test --locked --bin kathamo --no-run, then pass the printed
test executable. Pass --cli to also run a matching rpi RPC workload.
"""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import resource
import socket
import subprocess
import tempfile
import threading
import time

PROMPT = "Say x sixteen times."
BURST_PROMPT = "Burst 512 tokens."
TOKENS = 16
BURST_TOKENS = 512
SAMPLES = 12


class Fixture(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_POST(self):
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        request = self.rfile.read(int(self.headers["Content-Length"]))
        body = json.loads(request)
        canonical = json.dumps(body, sort_keys=True).encode()
        self.server.requests.append(hashlib.sha256(canonical).hexdigest())
        # Only the current (last) user prompt selects the fixture behavior;
        # history may still contain an earlier prompt with a different role.
        prompts = [
            message.get("content")
            for message in reversed(body.get("messages", []))
            if message.get("role") == "user"
            and isinstance(message.get("content"), str)
        ][:1]
        burst = any(BURST_PROMPT in prompt for prompt in prompts)
        slow = any("Slow 200 tokens." in prompt for prompt in prompts)
        gemini = body.get("model") == "gemini-3.8-flash"
        run_tools = any("Run two benchmark tools." in prompt for prompt in prompts)
        messages = body.get("messages", [])
        request_tools = run_tools and messages and messages[-1]["role"] != "tool"
        fail = any("Fail request." in prompt for prompt in prompts)
        if gemini and run_tools:
            fail = body.get("extra_body", {}).get("google", {}).get("thinking_config", {}).get("thinking_level") != "low"
            if not request_tools:
                assistant = next(message for message in reversed(messages) if message["role"] == "assistant")
                signatures = [call["extra_content"]["google"]["thought_signature"] for call in assistant["tool_calls"]]
                results = [message["content"] for message in messages[-2:] if message["role"] == "tool"]
                fail |= signatures != ["fixture-0", "fixture-1"] or not all(
                    any(value in result for result in results) for value in ("113", "127")
                )
        if "Check switched provider history." in prompts:
            prior_turn = any(
                message["role"] == "user" and message.get("content") == "Remember the switch fixture."
                for message in messages
            )
            prior_answer = any(
                message["role"] == "assistant" and message.get("content") == "x" * TOKENS
                for message in messages
            )
            fail = (
                body.get("model") != "switched"
                or not self.path.startswith("/second/")
                or not prior_turn
                or not prior_answer
            )
        if fail:
            self.send_response(400)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", "4")
            self.end_headers()
            self.wfile.write(b"boom")
            return
        tokens = BURST_TOKENS if burst else (200 if slow else TOKENS)
        delay = 0.0 if burst else (0.01 if slow else 0.003)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def send(data):
            try:
                self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
                self.wfile.flush()
                return True
            except (BrokenPipeError, ConnectionResetError):
                return False

        token = {
            "id": "bench",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": body["model"],
            "choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": None}],
        }
        if request_tools:
            tools = [("read", {"path": "alpha.txt"}), ("read", {"path": "beta.txt"})] if gemini else [
                ("read", {"path": "fixture.txt"}),
                ("bash", {"command": "printf ready"}),
            ]
            calls = [
                {
                    "index": index,
                    "id": f"call_bench_{len(messages)}_{index}",
                    "type": "function",
                    "function": {"name": name, "arguments": json.dumps(arguments)},
                }
                for index, (name, arguments) in enumerate(tools)
            ]
            if gemini:
                for index, call in enumerate(calls):
                    del call["index"]
                    call["extra_content"] = {"google": {"thought_signature": f"fixture-{index}"}}
            token["choices"][0]["delta"] = {"tool_calls": calls}
            frame = ("data: " + json.dumps(token) + "\n\n").encode()
            end = (
                b'data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}\n\n'
                b"data: [DONE]\n\n"
            )
            if send(frame) and send(end):
                self.wfile.write(b"0\r\n\r\n")
            return
        frame = ("data: " + json.dumps(token) + "\n\n").encode()
        if burst:
            if not send(frame * tokens):
                return
        else:
            for _ in range(tokens):
                if not send(frame):
                    return
                time.sleep(delay)
        if send(b'data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'):
            self.wfile.write(b"0\r\n\r\n")


def print_resources(before):
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    print(f"cpu user={after.ru_utime - before.ru_utime:.4f}s "
          f"sys={after.ru_stime - before.ru_stime:.4f}s "
          f"children_peak_rss_kib={after.ru_maxrss}", flush=True)


def benchmark_cli(executable, env, cwd):
    source = Path(__file__).resolve().parent.parent / "src" / "agent_ui.rs"
    catalog = re.search(r'CATALOG_PROMPT: &str = r#"(.*?)"#;', source.read_text(), re.S)[1]
    args = [executable, "--mode", "rpc", "--offline", "--no-extensions",
            "--model", "bench/fixture", "--append-system-prompt", catalog]
    for sample in range(SAMPLES):
        cold_start = time.perf_counter_ns()
        with subprocess.Popen(args, cwd=cwd, env=env, stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, text=True) as cli:
            spawn = (time.perf_counter_ns() - cold_start) // 1000
            timeout = threading.Timer(15, cli.kill)
            timeout.start()
            try:
                for turn in range(2):
                    start = cold_start if turn == 0 else time.perf_counter_ns()
                    cli.stdin.write(json.dumps({"type": "prompt", "message": PROMPT}) + "\n")
                    cli.stdin.flush()
                    first = last = max_gap = count = 0
                    for line in cli.stdout:
                        record = json.loads(line)
                        if record["type"] == "message_update":
                            delta = record["assistantMessageEvent"]
                            if delta["type"] == "text_delta":
                                count += len(delta["delta"])
                                now = (time.perf_counter_ns() - start) // 1000
                                first = first or now
                                if last:
                                    max_gap = max(max_gap, now - last)
                                last = now
                        elif record["type"] == "agent_settled":
                            assert record["outcome"] == "completed", record
                            break
                        elif record["type"] == "response" and not record["success"]:
                            raise RuntimeError(record.get("error", "RPC failed"))
                    assert count == TOKENS, (sample, turn, count)
                    total = (time.perf_counter_ns() - start) // 1000
                    print(f"cli sample={sample} turn={turn} spawn_us={spawn} "
                          f"ttft_us={first} last_us={last} total_us={total} max_gap_us={max_gap}")
            finally:
                timeout.cancel()
                cli.terminate()
                cli.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("test_executable", type=lambda value: str(Path(value).resolve()))
    parser.add_argument("--cli", metavar="RPI")
    parser.add_argument("--rpc", action="store_true", help="use the subprocess rpi RPC bridge")
    parser.add_argument("--settle", action="store_true", help="allow baseline completion to settle between turns")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as tmp, http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), Fixture
    ) as server:
        server.requests = []
        threading.Thread(target=server.serve_forever, daemon=True).start()
        config = Path(tmp, "config")
        config.mkdir()
        cwd = Path(tmp, "project")
        cwd.mkdir()
        (cwd / "fixture.txt").write_text("benchmark fixture\n")
        (cwd / "alpha.txt").write_text("113\n")
        (cwd / "beta.txt").write_text("127\n")
        providers = {
            name: {
                "api": "openai-completions",
                "baseUrl": f"http://127.0.0.1:{server.server_port}{route}/v1",
                "apiKey": "loopback",
                "models": [{"id": model}],
            }
            for name, model, route in (
                ("bench", "fixture", ""),
                ("bench-second", "switched", "/second"),
            )
        }
        providers["google"] = {
            "api": "openai-completions",
            "baseUrl": f"http://127.0.0.1:{server.server_port}/google/v1",
            "apiKey": "loopback",
            "models": [{"id": "gemini-3.8-flash", "reasoning": True}],
        }
        (config / "models.json").write_text(json.dumps({"providers": providers}))
        env = dict(os.environ, RPI_CODING_AGENT_DIR=str(config),
                   RPI_CODING_AGENT_SESSION_DIR=str(Path(tmp, "sessions")),
                   KATHAMO_BENCH_CWD=str(cwd))
        for key in ("KATHAMO_RPI_BACKEND", "KATHAMO_BENCH_SETTLE", "KATHAMO_STREAM_TIMINGS"):
            env.pop(key, None)
        if args.settle:
            env["KATHAMO_BENCH_SETTLE"] = "1"
        if args.rpc:
            env["KATHAMO_RPI_BACKEND"] = "rpc"
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        subprocess.run([args.test_executable, "loopback_agent_stream_benchmark",
                        "--ignored", "--nocapture"], env=env, check=True, timeout=60)
        print_resources(before)
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        subprocess.run([args.test_executable, "slow_reader_writer_benchmark",
                        "--ignored", "--nocapture"], env=env, check=True, timeout=60)
        print_resources(before)
        if args.cli:
            expected = SAMPLES * 2 + 8 * 3 + 1 + 2
            assert len(server.requests) == expected
            before = resource.getrusage(resource.RUSAGE_CHILDREN)
            benchmark_cli(args.cli, env, cwd)
            print_resources(before)
            direct = server.requests[expected:]
            primary = SAMPLES * 2
            print(f"matching JSON requests: "
                  f"{sum(left == right for left, right in zip(server.requests[:primary], direct))}"
                  f"/{primary}")
        subprocess.run([args.test_executable, "loopback_tool_latency_benchmark",
                        "--ignored", "--nocapture"],
                       env=dict(env, KATHAMO_STREAM_TIMINGS="1"), check=True, timeout=60)
        subprocess.run([args.test_executable, "loopback_gemini_tool_round_trip",
                        "--ignored", "--nocapture"], env=env, check=True, timeout=60)
        subprocess.run([args.test_executable, "loopback_provider_switch_keeps_context",
                        "--ignored", "--nocapture"], env=env, check=True, timeout=60)


if __name__ == "__main__":
    main()
