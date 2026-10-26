/**
 * The slice of a Cordis `Context` a DSH plugin needs in order to register
 * tools.
 *
 * Not a Cordis implementation and not trying to be. A plugin that reaches
 * for anything beyond `tools` is asking for the harness's kernel, and the
 * honest answer is that this bridge does not have one — see `assertInjectable`.
 */

/** Services this bridge can actually provide. */
export const PROVIDED_SERVICES = ['tools', 'logger'];

/**
 * Thrown when a plugin needs a service the bridge does not have. Carries the
 * missing names so the message can say which, rather than "unsupported".
 */
export class UnsupportedServiceError extends Error {
  constructor(missing) {
    const names = missing.join(', ');
    super(
      `this plugin injects ${names}, which the bridge does not provide. ` +
        `It adapts DeepSeek Harness's tool contract, not the Cordis kernel: ` +
        `only ${PROVIDED_SERVICES.join(' and ')} are available. ` +
        `A plugin needing more belongs in the harness itself.`,
    );
    this.name = 'UnsupportedServiceError';
    this.missing = missing;
  }
}

/**
 * Check a plugin's `inject` before loading it.
 *
 * Refusing here rather than at first use is the point: a plugin that gets
 * halfway through `apply` and then touches `ctx.llm` fails at a moment
 * nobody can predict, having already registered some of its tools.
 */
export function assertInjectable(inject) {
  const required = normaliseInject(inject);
  const missing = required.filter((name) => !PROVIDED_SERVICES.includes(name));
  if (missing.length > 0) throw new UnsupportedServiceError(missing);
  return required;
}

/**
 * Cordis accepts `inject` as an array, or as `{required, optional}`. Optional
 * services are not required, so an unavailable one is not a refusal.
 */
export function normaliseInject(inject) {
  if (!inject) return [];
  if (Array.isArray(inject)) return inject.map(String);
  if (typeof inject === 'object' && Array.isArray(inject.required)) {
    return inject.required.map(String);
  }
  return [];
}

/** Registry a plugin's `ctx.tools.register` writes into. */
export class ToolRegistry {
  constructor() {
    this.tools = new Map();
  }

  /**
   * Register a tool, returning the disposer Cordis callers expect. A
   * registration that cannot be undone would make `ctx.effect` a lie.
   */
  register(tool) {
    if (!tool || typeof tool.name !== 'string' || tool.name === '') {
      throw new TypeError('a registered tool needs a name');
    }
    if (typeof tool.execute !== 'function') {
      throw new TypeError(`tool "${tool.name}" has no execute function`);
    }
    this.tools.set(tool.name, tool);
    return () => this.tools.delete(tool.name);
  }

  get(name) {
    return this.tools.get(name);
  }

  list() {
    return [...this.tools.values()];
  }
}

/**
 * Build the context object handed to `apply`.
 *
 * `effect` and `on` are honoured rather than stubbed: a plugin that opens a
 * timer or subscribes to something expects its cleanup to run, and a bridge
 * that silently dropped the disposers would leak them for the process's
 * whole life.
 */
export function createContext({ registry, logger = console } = {}) {
  const disposers = [];

  const ctx = {
    tools: registry,
    logger: {
      // Diagnostics go to stderr: stdout is the MCP transport, and one
      // stray console.log there corrupts the protocol.
      info: (...a) => logger.error('[plugin]', ...a),
      warn: (...a) => logger.error('[plugin]', ...a),
      error: (...a) => logger.error('[plugin]', ...a),
      debug: (...a) => logger.error('[plugin]', ...a),
    },

    /** Run `fn`; if it returns a function, keep it as cleanup. */
    effect(fn) {
      const cleanup = fn();
      if (typeof cleanup === 'function') disposers.push(cleanup);
      return () => {
        const i = disposers.indexOf(cleanup);
        if (i >= 0) disposers.splice(i, 1);
        if (typeof cleanup === 'function') cleanup();
      };
    },

    /**
     * Accepted so a plugin that subscribes during `apply` does not crash.
     * The bridge has no event bus to deliver from — an MCP server is asked
     * questions, it is not told about a session's lifecycle — so a listener
     * registered here is never called. That is a gap in what the bridge
     * adapts, not a silent failure: `tools/call` is the only thing that ever
     * arrives.
     */
    on(_event, _listener) {
      return () => {};
    },

    dispose() {
      while (disposers.length > 0) {
        const cleanup = disposers.pop();
        try {
          cleanup();
        } catch (e) {
          logger.error('[bridge] a plugin cleanup threw:', e);
        }
      }
    },
  };

  return ctx;
}
