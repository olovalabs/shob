#!/usr/bin/env python3
"""Compare cold and warm turns using Kathamo's installed Antigravity runtime.

Pass the installed kathamo-provider executable as the first argument.
Uses the existing Antigravity login and makes real inference requests.
Each pair runs the same prompt twice in one process. Cold timing includes
process startup. Warm timing includes the context from the first turn.
HTTP header times are per fetch attempt. Other times are per turn.
"""

import argparse
from collections import Counter
from dataclasses import dataclass, field
import json
import math
import os
from pathlib import Path
import queue
import statistics
import subprocess
import tempfile
import threading
import time


MODEL = "google-antigravity/gemini-3.8-flash"
PROBE_PREFIX = "kathamo-timing:"
TIMING_EXTENSION = Path(__file__).resolve().parents[1] / "assets/integrations/omp-timing.mjs"
EVENTS = [
    "turn_start", "message_update", "auto_retry_start", "tool_execution_start", "tool_execution_end",
]
WORKLOADS = {
    "short": "Find the smallest positive integer with exactly twelve positive divisors. Return just the integer.",
    "read": "Use the read tool to inspect alpha.txt and beta.txt. Return their two numbers in order. Do not change any files.",
}
@dataclass
class ToolCycle:
    request_ms: float | None = None
    arguments_ms: float | None = None
    start_ms: float | None = None
    end_ms: float | None = None
    next_request_ms: float | None = None
    next_delta_ms: float | None = None
    failed: bool = False


@dataclass
class Turn:
    admission_ms: float | None = None
    first_reasoning_ms: float | None = None
    first_answer_ms: float | None = None
    total_ms: float | None = None
    model_rounds: int = 0
    retries: int = 0
    reads: int = 0
    outcome: str = "timeout"
    http: list = field(default_factory=list)
    provider_requests: list = field(default_factory=list)
    tools: dict = field(default_factory=dict)

    def tool_cycle(self, tool_id):
        if not isinstance(tool_id, str) or not tool_id:
            return None
        if tool_id not in self.tools:
            self.tools[tool_id] = ToolCycle(
                request_ms=self.provider_requests[-1] if self.provider_requests else None,
            )
        return self.tools[tool_id]

    def consume(self, frame, prompt_id, elapsed):
        kind = frame.get("type")
        if kind == "response" and frame.get("id") == prompt_id:
            if frame.get("success") is False:
                return self.finish("rejected", elapsed)
            self.admission_ms = elapsed
        elif kind == "turn_start":
            self.model_rounds += 1
        elif kind == "auto_retry_start":
            self.retries += 1
        elif kind == "tool_execution_start":
            if cycle := self.tool_cycle(frame.get("toolCallId")):
                cycle.start_ms = elapsed
        elif kind == "tool_execution_end":
            if frame.get("toolName") == "read" and not frame.get("isError"):
                self.reads += 1
            if cycle := self.tool_cycle(frame.get("toolCallId")):
                cycle.end_ms = elapsed
                cycle.failed = bool(frame.get("isError"))
        elif kind == "message_update":
            event = frame.get("assistantMessageEvent", {})
            if event.get("type") == "toolcall_end":
                if cycle := self.tool_cycle(event.get("toolCall", {}).get("id")):
                    cycle.arguments_ms = elapsed
            if event.get("delta", "").strip():
                if event.get("type") == "thinking_delta" and self.first_reasoning_ms is None:
                    self.first_reasoning_ms = elapsed
                if event.get("type") == "text_delta" and self.first_answer_ms is None:
                    self.first_answer_ms = elapsed
                for cycle in self.tools.values():
                    if cycle.end_ms is not None and cycle.next_delta_ms is None:
                        cycle.next_delta_ms = elapsed
        elif kind == "extension_ui_request" and frame.get("method") == "notify":
            message = frame.get("message", "")
            if message.startswith(PROBE_PREFIX):
                metric = json.loads(message[len(PROBE_PREFIX):])
                if metric.get("event") == "provider_request":
                    self.provider_requests.append(elapsed)
                    for cycle in self.tools.values():
                        if cycle.end_ms is not None and cycle.next_request_ms is None:
                            cycle.next_request_ms = elapsed
                elif metric.get("event") == "http_headers":
                    self.http.append((metric.get("status"), metric["elapsed_ms"]))
        elif kind == "prompt_result" and frame.get("id") == prompt_id:
            status = frame.get("status")
            return self.finish(status if status in {"completed", "aborted", "cancelled", "error"} else "unknown", elapsed)
        return False

    def finish(self, outcome, elapsed):
        self.outcome = outcome
        self.total_ms = elapsed
        return True


def provider_env(root):
    env = {k: v for k, v in os.environ.items() if not k.upper().startswith(("OMP_", "PI_"))}
    home = str(root / "home")
    env.update(HOME=home, USERPROFILE=home, PI_CONFIG_DIR=".kathamo-provider",
               PI_CODING_AGENT_DIR=str(root / "agent"), OMP_PROFILE="", PI_PROFILE="",
               OMP_AUTH_BROKER_URL="", OMP_AUTH_BROKER_TOKEN="", PI_CONFIG_FILES="")
    for key, folder in {
        "XDG_CONFIG_HOME": "config", "XDG_DATA_HOME": "data",
        "XDG_STATE_HOME": "state", "XDG_CACHE_HOME": "cache",
        "APPDATA": "config", "LOCALAPPDATA": "data",
    }.items():
        env[key] = str(root / folder)
    return env


def read_frames(stdout, frames):
    try:
        for line in stdout:
            received = time.monotonic()
            try:
                frames.put((received, json.loads(line)))
            except (ValueError, UnicodeError):
                frames.put((received, None))
                return
    finally:
        frames.put((time.monotonic(), None))


def run_turn(process, frames, prompt_id, prompt, start, timeout, ui_handler=None):
    result = Turn()
    try:
        process.stdin.write((json.dumps({"id": prompt_id, "type": "prompt", "message": prompt}) + "\n").encode())
        process.stdin.flush()
    except BrokenPipeError:
        result.finish("rpc_closed", (time.monotonic() - start) * 1000)
        return result
    while (remaining := start + timeout - time.monotonic()) > 0:
        try:
            received, frame = frames.get(timeout=remaining)
        except queue.Empty:
            break
        elapsed = (received - start) * 1000
        if frame is None:
            result.finish("rpc_closed", elapsed)
            return result
        if elapsed >= 0 and ui_handler and frame.get("type") == "extension_ui_request":
            ui_handler(process, frame)
        if elapsed >= 0 and result.consume(frame, prompt_id, elapsed):
            return result
    result.finish("timeout", (time.monotonic() - start) * 1000)
    return result


def run_pair(runtime, root, workload, effort, timeout):
    with tempfile.TemporaryDirectory(prefix="kathamo-antigravity-bench-") as path:
        workspace = Path(path)
        (workspace / "alpha.txt").write_text("113\n", encoding="utf-8")
        (workspace / "beta.txt").write_text("127\n", encoding="utf-8")
        extension = workspace / "http-probe.mjs"
        extension.write_text(TIMING_EXTENSION.read_text(encoding="utf-8"), encoding="utf-8")
        tool_args = ["--no-tools"] if workload == "short" else ["--tools", "read"]
        command = [str(runtime), "--mode", "rpc", "--model", MODEL, "--no-session",
                   "--thinking", effort, "--extension", str(extension), *tool_args]
        cold_start = time.monotonic()
        with subprocess.Popen(command, cwd=workspace, env=provider_env(root), stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL) as process:
            frames = queue.Queue()
            reader = threading.Thread(target=read_frames, args=(process.stdout, frames), daemon=True)
            reader.start()
            try:
                command = {"type": "set_event_filter", "events": EVENTS, "messageUpdates": "delta"}
                process.stdin.write((json.dumps(command) + "\n").encode())
                cold = run_turn(process, frames, "cold", WORKLOADS[workload], cold_start, timeout)
                if cold.outcome != "completed":
                    return cold, Turn(outcome="skipped_after_cold_failure")
                warm = run_turn(process, frames, "warm", WORKLOADS[workload], time.monotonic(), timeout)
                return cold, warm
            finally:
                process.kill()
                process.wait()
                reader.join()


def stats(values):
    values = sorted(value for value in values if value is not None)
    if not values:
        return {"n": 0}
    return {"n": len(values), "median": round(statistics.median(values), 2),
            "p95": round(values[math.ceil(len(values) * 0.95) - 1], 2)}


def summarize(turns):
    successful = [turn for turn in turns if turn.outcome == "completed"]
    http = [(status, elapsed) for turn in turns for status, elapsed in turn.http]
    cycles = [cycle for turn in successful for cycle in turn.tools.values()]
    def durations(start, end):
        return stats(
            getattr(cycle, end) - getattr(cycle, start)
            for cycle in cycles
            if getattr(cycle, start) is not None and getattr(cycle, end) is not None
        )
    return {
        "samples": len(turns),
        "outcomes": dict(Counter(turn.outcome for turn in turns)),
        "admission_ms": stats(turn.admission_ms for turn in successful),
        "first_reasoning_ms": stats(turn.first_reasoning_ms for turn in successful),
        "first_answer_ms": stats(turn.first_answer_ms for turn in successful),
        "total_ms": stats(turn.total_ms for turn in successful),
        "model_rounds": stats(turn.model_rounds for turn in successful),
        "read_calls": stats(turn.reads for turn in successful),
        "provider_requests": stats(len(turn.provider_requests) for turn in successful),
        "tool_cycles": {
            "completed": sum(cycle.end_ms is not None for cycle in cycles),
            "failed": sum(cycle.failed for cycle in cycles),
            "request_to_arguments_ms": durations("request_ms", "arguments_ms"),
            "arguments_to_start_ms": durations("arguments_ms", "start_ms"),
            "execution_ms": durations("start_ms", "end_ms"),
            "request_to_end_ms": durations("request_ms", "end_ms"),
            "end_to_next_request_ms": durations("end_ms", "next_request_ms"),
            "end_to_next_delta_ms": durations("end_ms", "next_delta_ms"),
        },
        "retry_events": sum(turn.retries for turn in turns),
        "http_status_counts": dict(Counter(str(status) if status is not None else "transport_error"
                                          for status, _ in http)),
        "provider_request_to_headers_ms": stats(elapsed for status, elapsed in http if status is not None),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("runtime", type=Path, help="path to Kathamo's installed kathamo-provider")
    parser.add_argument("--provider-root", type=Path, help="defaults to the runtime's provider directory")
    parser.add_argument("--samples", type=int, default=3, help="cold/warm pairs per workload, 1–20")
    parser.add_argument("--timeout", type=float, default=75, help="maximum seconds per turn, 1–180")
    parser.add_argument("--effort", choices=["medium", "high"], default="medium")
    args = parser.parse_args()
    if not 1 <= args.samples <= 20 or not 1 <= args.timeout <= 180:
        parser.error("samples must be 1–20 and timeout must be 1–180 seconds")
    runtime = args.runtime.resolve()
    if not runtime.is_file():
        parser.error("runtime executable not found")
    if args.provider_root is None and runtime.parent.parent.name != "runtime":
        parser.error("pass --provider-root for a runtime outside the installed directory layout")
    root = args.provider_root.resolve() if args.provider_root else runtime.parents[2]
    if not (root / "agent").is_dir():
        parser.error("Kathamo provider agent directory not found")
    result = {}
    for workload in WORKLOADS:
        pairs = [run_pair(runtime, root, workload, args.effort, args.timeout) for _ in range(args.samples)]
        result[workload] = {label: summarize([pair[i] for pair in pairs])
                            for i, label in enumerate(("cold", "warm"))}
    print(json.dumps({"model": MODEL, "effort": args.effort, "workloads": result}, indent=2))
    return int(any(group["outcomes"].get("completed", 0) != args.samples
                   for workload in result.values() for group in workload.values()))


if __name__ == "__main__":
    raise SystemExit(main())
