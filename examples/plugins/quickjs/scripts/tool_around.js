// `tool.around` — a decision before a tool call is dispatched: refuse it,
// answer it, or let it run on a shorter clock.
function onAround(call) {
  var path = call.input && (call.input.file_path || call.input.notebook_path);
  if ((call.tool === "Write" || call.tool === "Edit") && typeof path === "string" &&
      /(^|\/)generated\//.test(path)) {
    return {
      action: "deny",
      reason: "demo-quickjs: generated/ is produced by the build; edit its sources instead",
    };
  }
  if (call.tool === "Bash") {
    return { action: "proceed", timeoutMs: 120000 };
  }
  return null;
}
