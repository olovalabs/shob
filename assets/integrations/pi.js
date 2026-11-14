// Kathamo lifecycle adapter for Pi. Reports only when launched by Kathamo.
import { spawnSync } from "node:child_process";
const executable = process.env.KATHAMO_EXECUTABLE || process.env.MARKUP_EXECUTABLE;
const reporter_generation = `${process.pid}-${Date.now()}`;
let sequence = 0;
let sessionID;
function report(event, extra = {}) {
  if (!executable || !(process.env.KATHAMO_STATUS_SOCKET || process.env.MARKUP_STATUS_SOCKET)) return;
  const record = { version: 1, reporter: "pi", reporter_generation, sequence: ++sequence,
    session_id: sessionID, turn_id: extra.turn_id, event, message: extra.message,
    approval_id: extra.approval_id };
  try { spawnSync(executable, ["status-event"], { input: JSON.stringify(record), stdio: ["pipe", "ignore", "ignore"] }); } catch {}
}
export default function (pi) {
  if (!executable) return;
  let stopReason;
  const ctx = pi.getContext?.();
  sessionID = ctx?.sessionManager?.getSessionId?.();
  report("register");
  pi.on("agent_start", (event) => { stopReason = undefined; report("working", { turn_id: event?.turnId }); });
  pi.on("agent_end", (event) => {
    stopReason = event.messages?.filter((message) => message.role === "assistant").at(-1)?.stopReason;
  });
  pi.on("agent_settled", (event) => report(event.aborted ? "cancelled" : stopReason === "error" ? "error" : "done"));
  pi.on("ui_prompt_start", (event) => report("blocked", { approval_id: event?.id }));
  pi.on("ui_prompt_end", (_event, context) => report(context?.isIdle?.() ? "done" : "working"));
}
