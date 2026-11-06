"use strict";
(() => {
  const host = globalThis.__host;
  delete globalThis.__host;
  const show = (x) => (typeof x === "string" ? x : JSON.stringify(x));
  const call = async (name, input) => {
    const raw = await host.call(String(name), JSON.stringify(input === undefined ? {} : input));
    const r = JSON.parse(raw);
    if (!r.ok) {
      const e = new Error(r.error);
      e.tool = String(name);
      throw e;
    }
    return r.value;
  };
  const log = (...parts) => host.log(parts.map(show).join(" "));
  const define = (name, value) =>
    Object.defineProperty(globalThis, name, { value, writable: false, configurable: false });
  define("callTool", call);
  define("tools", new Proxy({}, {
    get: (_, name) => (typeof name === "string" ? (input) => call(name, input) : undefined),
  }));
  define("agent", async (prompt, opts = {}) => {
    const text = String(prompt);
    const input = { prompt: text, description: opts.label ? String(opts.label) : text.slice(0, 60) };
    if (opts.type) input.subagent_type = String(opts.type);
    if (opts.model) input.model = String(opts.model);
    if (opts.schema) input.output_schema = opts.schema;
    return call("Agent", input);
  });
  define("log", log);
  define("console", { log, info: log, warn: log, error: log, debug: log });
  define("notify", (text) => host.notify(show(text)));
  define("stopRequested", () => host.stopRequested());
  define("messages", () => JSON.parse(host.messages()));
  define("args", JSON.parse(host.args()));
  for (const name of [
    "require", "process", "Buffer", "fetch", "setTimeout", "setInterval", "clearTimeout",
    "clearInterval", "setImmediate", "XMLHttpRequest", "WebSocket", "module", "exports",
    "__dirname", "__filename", "Deno", "Bun",
  ]) {
    Object.defineProperty(globalThis, name, {
      get() {
        throw new ReferenceError(
          `${name} is not available in RunCode: there are no modules, npm packages or ` +
          `Node/browser APIs here — use tools.* (or callTool) for any I/O`);
      },
      configurable: false,
    });
  }
})();
