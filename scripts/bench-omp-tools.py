#!/usr/bin/env python3
"""Exercise the pinned OMP runtime against a local, deterministic tool fixture.

No accounts, credentials, or external inference. Each turn batches two real reads
and one real two-step shell command. Reports inference, scheduling and execution
boundaries, not a claimed speedup against an unmeasured provider.
"""
import argparse
import http.server
import json
from pathlib import Path
import queue
import socket
import subprocess
import sys
import tempfile
import threading
import time

from bench_antigravity import EVENTS, TIMING_EXTENSION, provider_env, read_frames, run_turn, summarize


class ToolFixture(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_POST(self):
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        messages = body.get("messages", [])
        last_user = max((i for i, message in enumerate(messages) if message.get("role") == "user"), default=-1)
        results = [message for message in messages[last_user + 1:] if message.get("role") == "tool"]
        self.server.requests += 1
        delta = {"role": "assistant"}
        reason = "stop"
        if not body.get("tools"):
            # Auxiliary title/cache requests are legitimate model calls, but
            # cannot request tools absent from their advertised tool surface.
            delta["content"] = "Local tool fixture"
        elif not results:
            calls = [
                ("read", {"path": "alpha.txt"}),
                ("read", {"path": "beta.txt"}),
                ("bash", {"command": "printf 'local-a\\n'; printf 'local-b\\n'"}),
            ]
            delta["tool_calls"] = [
                {"index": index, "id": f"fixture_{self.server.requests}_{index}", "type": "function",
                 "function": {"name": name, "arguments": json.dumps(arguments)}}
                for index, (name, arguments) in enumerate(calls)
            ]
            reason = "tool_calls"
        else:
            evidence = "\n".join(str(message.get("content", "")) for message in results)
            valid = len(results) == 3 and all(value in evidence for value in ("113", "127", "local-a", "local-b"))
            self.server.verified_turns += int(valid)
            self.server.invalid_turns += int(not valid)
            delta["content"] = "113 127 local-a local-b" if valid else "Tool fixture failed"
        frame = {"id": "fixture", "object": "chat.completion.chunk", "created": 0,
                 "model": body["model"], "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}
        final = {"choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                 "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}}
        wire = ("data: " + json.dumps(frame) + "\n\ndata: " + json.dumps(final) + "\n\ndata: [DONE]\n\n").encode()
        try:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            # Prompt admission deliberately cancels background title/cache
            # requests; account for peer cancellation, including header writes.
            self.server.cancelled_responses += 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("runtime", type=Path, help="installed kathamo-provider executable")
    parser.add_argument("--samples", type=int, default=3, help="cold/warm pairs (1–20)")
    parser.add_argument("--approval-mode", choices=("write", "yolo"), default="write",
                        help="write exercises real approval round trips; yolo is confined to the disposable fixture")
    args = parser.parse_args()
    runtime = args.runtime.resolve()
    if not runtime.is_file() or not 1 <= args.samples <= 20:
        parser.error("runtime must exist and samples must be 1–20")
    with tempfile.TemporaryDirectory(prefix="kathamo-omp-tools-") as temporary, http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), ToolFixture
    ) as server:
        root = Path(temporary)
        agent = root / "agent"
        agent.mkdir()
        project = root / "project"
        project.mkdir()
        (project / "alpha.txt").write_text("113\n")
        (project / "beta.txt").write_text("127\n")
        model = {"id": "fixture", "name": "Local fixture", "contextWindow": 32768, "maxTokens": 1024,
                 "input": ["text"], "reasoning": False,
                 "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}}
        config = {"providers": {"bench": {"api": "openai-completions", "apiKey": "loopback",
                  "baseUrl": f"http://127.0.0.1:{server.server_port}/v1", "models": [model]}}}
        (agent / "models.yml").write_text(json.dumps(config))
        # Do not let the benchmark discover the user's API keys or subscriptions.
        allowed_environment = {
            "PATH", "LANG", "LC_ALL", "TZ", "TMPDIR", "SYSTEMROOT", "WINDIR",
            "HOME", "USERPROFILE", "PI_CONFIG_DIR", "PI_CODING_AGENT_DIR",
            "OMP_PROFILE", "PI_PROFILE", "OMP_AUTH_BROKER_URL", "OMP_AUTH_BROKER_TOKEN",
            "PI_CONFIG_FILES", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
            "XDG_CACHE_HOME", "APPDATA", "LOCALAPPDATA",
        }
        environment = {key: value for key, value in provider_env(root).items() if key in allowed_environment}
        server.requests = server.verified_turns = server.invalid_turns = 0
        server.cancelled_responses = 0
        server.approvals = 0

        def respond_fixture_approval(process, request):
            if request.get("method") == "notify":
                return
            response = {"type": "extension_ui_response", "id": request.get("id"), "cancelled": True}
            # Only the known, disposable shell operation may be approved. Never
            # choose a session grant or approve unexpected extension interaction.
            title = request.get("title", "")
            if request.get("method") == "select" and "local-a" in title and "local-b" in title:
                options = request.get("options", [])
                once = next((label for label in ("Approve", "Approve Once") if label in options), None)
                if once:
                    response = {"type": "extension_ui_response", "id": request["id"], "value": once}
                    server.approvals += 1
            process.stdin.write((json.dumps(response) + "\n").encode())
            process.stdin.flush()

        threading.Thread(target=server.serve_forever, daemon=True).start()
        samples = {"cold": [], "warm": []}
        for _ in range(args.samples):
            command = [str(runtime), "--mode", "rpc", "--model", "bench/fixture", "--no-session",
                       "--thinking", "off", "--tools", "read,bash", "--approval-mode", args.approval_mode,
                       "--no-extensions", "--extension", str(TIMING_EXTENSION)]
            start = time.monotonic()
            with subprocess.Popen(command, cwd=project, env=environment, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
                frames = queue.Queue()
                reader = threading.Thread(target=read_frames, args=(process.stdout, frames), daemon=True)
                reader.start()
                try:
                    process.stdin.write((json.dumps({"type": "set_event_filter", "events": EVENTS,
                                                    "messageUpdates": "delta"}) + "\n").encode())
                    process.stdin.flush()
                    for phase in ("cold", "warm"):
                        turn = run_turn(process, frames, phase, "Run the local tool fixture.",
                                        start if phase == "cold" else time.monotonic(), 30, respond_fixture_approval)
                        samples[phase].append(turn)
                        if turn.outcome != "completed":
                            break
                finally:
                    process.kill()
                    process.wait()
                    reader.join(timeout=5)
                    diagnostics = process.stderr.read().decode(errors="replace")
            if diagnostics:
                print(diagnostics, end="", file=sys.stderr)
        expected_turns = args.samples * 2
        report = {"scope": "loopback OMP runtime, not remote provider or GPUI",
                  "model": "bench/fixture", "model_requests": server.requests,
                  "verified_tool_turns": server.verified_turns, "invalid_tool_turns": server.invalid_turns,
                  "approval_mode": args.approval_mode, "one_shot_approvals": server.approvals,
                  "workloads": {phase: summarize(turns) for phase, turns in samples.items()}}
        print(json.dumps(report, indent=2))
        valid = server.verified_turns == expected_turns and server.invalid_turns == 0
        valid &= server.approvals == (expected_turns if args.approval_mode == "write" else 0)
        valid &= all(len(turns) == args.samples and all(
            turn.outcome == "completed" and len(turn.tools) == 3 and len(turn.provider_requests) >= 2
            and len(turn.http) >= 2
            and all(cycle.request_ms is not None and cycle.arguments_ms is not None
                    and cycle.start_ms is not None and cycle.end_ms is not None
                    and cycle.request_ms <= cycle.arguments_ms <= cycle.start_ms <= cycle.end_ms
                    and not cycle.failed for cycle in turn.tools.values())
            for turn in turns
        ) for turns in samples.values())
        server.shutdown()
        return int(not valid)


if __name__ == "__main__":
    raise SystemExit(main())
