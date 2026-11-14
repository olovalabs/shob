// Loaded only for KATHAMO_STREAM_TIMINGS. Never inspect or serialize payloads,
// response headers, request IDs, credentials, tool arguments, or model output.
const PREFIX = "kathamo-timing:";

export default function (omp) {
  let requestStarted;
  const report = (ctx, metric) => {
    try {
      ctx.ui.notify(PREFIX + JSON.stringify(metric), "info");
    } catch {
      // Diagnostics must not change provider execution or error handling.
    }
  };
  omp.on("agent_start", () => { requestStarted = undefined; });
  omp.on("before_provider_request", (_event, ctx) => {
    requestStarted = performance.now();
    report(ctx, { event: "provider_request" });
  });
  omp.on("after_provider_response", (event, ctx) => {
    if (requestStarted === undefined) return;
    // Hook-to-headers elapsed includes provider preparation and any intervening
    // retries; it is not a server-only or individual HTTP-attempt measurement.
    report(ctx, {
      event: "http_headers",
      elapsed_ms: performance.now() - requestStarted,
      status: event.status,
    });
  });
}
