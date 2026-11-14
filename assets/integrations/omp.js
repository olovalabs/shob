// Kathamo lifecycle adapter for Oh My Pi. Reports only inside a Kathamo pane.
import { spawnSync } from "node:child_process";
const executable = process.env.KATHAMO_EXECUTABLE || process.env.MARKUP_EXECUTABLE;
const reporter_generation = `${process.pid}-${Date.now()}`;
let sequence = 0;
let sessionID;
function report(event, extra = {}) {
  if (!executable || !(process.env.KATHAMO_STATUS_SOCKET || process.env.MARKUP_STATUS_SOCKET)) return;
  const record = { version: 1, reporter: "omp", reporter_generation, sequence: ++sequence,
    session_id: sessionID, turn_id: extra.turn_id, event, approval_id: extra.approval_id };
  try { spawnSync(executable, ["status-event"], { input: JSON.stringify(record), stdio: ["pipe", "ignore", "ignore"] }); } catch {}
}
export default function (omp) {
  if (!executable) return;
  let pending = new Set();
  report("register");
  omp.on("session_start", (event) => { sessionID = event?.sessionId ?? event?.session_id; if (sessionID) report("register"); });
  omp.on("agent_start", (event) => report("working", { turn_id: event?.turnId }));
  omp.on("agent_end", (event) => {
    if (event.willContinue || event.awaitingAsyncWork) return report("working", { turn_id: event.turnId });
    const last = event.messages?.filter((message) => message.role === "assistant").at(-1);
    report(last?.stopReason === "error" ? "error" : last?.stopReason === "aborted" ? "cancelled" : "done");
  });
  omp.on("auto_retry_start", () => report("working"));
  for (const name of ["tool_approval_requested", "plan_review_requested"]) {
    omp.on(name, (event) => { const id = event?.id ?? `${name}-${pending.size}`; pending.add(id); report("blocked", { approval_id: id }); });
  }
  for (const name of ["tool_approval_resolved", "plan_review_resolved"]) {
    omp.on(name, (event) => { pending.delete(event?.id); report(pending.size ? "blocked" : "approval_resolved", { approval_id: event?.id }); });
  }
}
