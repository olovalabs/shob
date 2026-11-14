// Kathamo lifecycle adapter for Command Code's ModApi.
import { spawnSync } from "node:child_process";

export default function (cmd) {
  const executable = process.env.KATHAMO_EXECUTABLE || process.env.MARKUP_EXECUTABLE;
  if (!executable || !(process.env.KATHAMO_STATUS_SOCKET || process.env.MARKUP_STATUS_SOCKET)) return;
  const reporter_generation = `${process.pid}-${Date.now()}`;
  let sequence = 0;
  let sessionID;
  let runs = 0;
  let compactions = 0;
  let requestSequence = 0;
  const approvals = [];

  const report = (event, extra = {}) => {
    try {
      spawnSync(executable, ["status-event"], {
        input: JSON.stringify({ version: 1, reporter: "commandcode",
          reporter_generation, sequence: ++sequence, session_id: sessionID,
          event, ...extra }),
        stdio: ["pipe", "ignore", "ignore"], timeout: 1000,
      });
    } catch {}
  };
  const settle = (result) => {
    if (approvals.length) return;
    if (runs || compactions) return report("working");
    const reason = result?.stopReason;
    report(result?.aborted || reason === "interrupted" || reason === "aborted"
      ? "cancelled" : reason === "error" ? "error" : "done");
  };
  cmd.hooks({
    onSessionStart({ sessionId }) {
      sessionID = sessionId;
      runs = compactions = 0;
      approvals.length = 0;
      report("register");
    },
    onSessionEnd() { report("clear"); },
  });
  cmd.on("run_start", (event) => {
    if (event.sessionId && event.sessionId !== sessionID) {
      sessionID = event.sessionId;
      report("register");
    }
    runs++;
    if (!approvals.length) report("working");
  });
  cmd.on("run_end", (event) => {
    runs = Math.max(0, runs - 1);
    settle(event.result);
  });
  cmd.on("compaction_start", () => {
    compactions++;
    if (!approvals.length) report("working");
  });
  cmd.on("compaction_done", () => {
    compactions = Math.max(0, compactions - 1);
    settle();
  });
  cmd.on("interaction_requested", (event) => {
    const request = { id: `request-${++requestSequence}`, kind: event.kind,
      tool: event.toolName };
    approvals.push(request);
    report("blocked", { approval_id: request.id });
  });
  cmd.on("interaction_resolved", (event) => {
    const index = approvals.findIndex((request) =>
      request.kind === event.kind && request.tool === event.toolName);
    if (index < 0) return;
    const [request] = approvals.splice(index, 1);
    report("approval_resolved", { approval_id: request.id });
    if (!approvals.length) settle();
  });
}
