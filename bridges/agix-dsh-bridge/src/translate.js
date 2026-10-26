/**
 * Translating between DSH's tool contract and MCP.
 *
 * The two are close enough that this is mapping rather than adaptation:
 * `parameters` is a flat spelling of JSON Schema, `execute` is `tools/call`,
 * and `output.render` already produces something shaped like an MCP content
 * block. Everything here is about the seams where they differ.
 */

/**
 * `defineTool`'s `parameters` — `{name: {type, required, description}}` —
 * as a JSON Schema object.
 *
 * A tool that already wrote a full schema (`{type: 'object', properties}`)
 * is passed through untouched: some plugins do, and re-wrapping it would
 * produce a schema describing a schema.
 */
export function parametersToSchema(parameters) {
  if (!parameters || typeof parameters !== 'object') {
    return { type: 'object', properties: {} };
  }
  if (parameters.type === 'object' && parameters.properties) {
    return parameters;
  }

  const properties = {};
  const required = [];
  for (const [name, spec] of Object.entries(parameters)) {
    if (!spec || typeof spec !== 'object') continue;
    const { required: isRequired, ...rest } = spec;
    properties[name] = rest;
    if (isRequired) required.push(name);
  }

  const schema = { type: 'object', properties };
  // An empty `required` and an absent one mean the same thing to a
  // validator, but only the absent one reads that way to a person.
  if (required.length > 0) schema.required = required;
  return schema;
}

/** One DSH tool as an MCP `tools/list` entry. */
export function toolToMcp(tool) {
  return {
    name: tool.name,
    description: tool.description ?? '',
    inputSchema: parametersToSchema(tool.parameters),
  };
}

/**
 * Turn what `execute` returned into MCP content blocks.
 *
 * `output.render` is the tool's own answer to "how should this look to a
 * model", so it wins whenever it exists. Without one, the canonical value is
 * rendered generically — a string as itself, anything else as JSON, because
 * `[object Object]` is the least useful thing to hand a model.
 */
export function renderOutput(tool, args, value) {
  if (tool.output && typeof tool.output.render === 'function') {
    return normaliseContent(tool.output.render(args, value));
  }
  return [{ type: 'text', text: stringify(value) }];
}

/**
 * Coerce a render result into MCP content blocks.
 *
 * DSH renders return `[{type:'text', text}]`, which MCP accepts as-is. A
 * plugin returning a bare string is being informal rather than wrong, so it
 * is wrapped instead of rejected.
 */
export function normaliseContent(rendered) {
  if (typeof rendered === 'string') {
    return [{ type: 'text', text: rendered }];
  }
  if (!Array.isArray(rendered)) {
    return [{ type: 'text', text: stringify(rendered) }];
  }
  return rendered.map((block) => {
    if (typeof block === 'string') return { type: 'text', text: block };
    if (block && typeof block === 'object' && typeof block.type === 'string') {
      return block;
    }
    return { type: 'text', text: stringify(block) };
  });
}

function stringify(value) {
  if (typeof value === 'string') return value;
  if (value === undefined) return '';
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    return String(value);
  }
}

/**
 * How long a single `execute` may run before the bridge gives up on it.
 *
 * DSH's tool contract defines no timeout and no abort signal, so a tool that
 * never returns would otherwise hold its request open forever. The bridge
 * cannot stop the work — there is nothing in the contract to cancel with —
 * but it can stop waiting, which is what keeps one bad tool from being
 * indistinguishable from a hung server.
 */
export const DEFAULT_TOOL_TIMEOUT_MS = 120_000;

export function toolTimeoutMs() {
  const raw = Number(process.env.AGIX_DSH_TOOL_TIMEOUT_MS);
  return Number.isFinite(raw) && raw > 0 ? raw : DEFAULT_TOOL_TIMEOUT_MS;
}

/**
 * Run a tool and shape the result the way MCP expects.
 *
 * A throwing tool becomes `isError: true` with its message rather than a
 * transport-level failure: the model is the one that has to decide what to
 * do about a tool that did not work, and it can only do that if it is told.
 * A tool that never returns is reported the same way, for the same reason.
 */
export async function callTool(tool, args, { timeoutMs = toolTimeoutMs() } = {}) {
  let timer;
  const deadline = new Promise((_, reject) => {
    timer = setTimeout(
      () => reject(new Error(`did not return within ${timeoutMs}ms`)),
      timeoutMs,
    );
    // The timer must not be what keeps the process alive once stdin closes.
    if (typeof timer.unref === 'function') timer.unref();
  });

  try {
    const value = await Promise.race([tool.execute(args ?? {}), deadline]);
    return { content: renderOutput(tool, args ?? {}, value), isError: false };
  } catch (e) {
    const message = e && e.message ? e.message : String(e);
    return {
      content: [{ type: 'text', text: `${tool.name} failed: ${message}` }],
      isError: true,
    };
  } finally {
    clearTimeout(timer);
  }
}
