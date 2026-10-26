/**
 * Loading a DSH plugin module.
 *
 * Cordis accepts three shapes and so does this: a module exporting `apply`,
 * a default export object with `apply`, or a Service subclass whose
 * constructor takes the context. Recognising all three matters because
 * which one a plugin used is an authorship choice, not a capability
 * difference — refusing two of them would reject working plugins for style.
 */

import { pathToFileURL } from 'node:url';
import { assertInjectable, createContext, ToolRegistry } from './context.js';

/**
 * A class's `name` is its identifier, which is a reasonable plugin name, but
 * only if the author did not set a static one.
 */
function pluginName(cls) {
  return Object.prototype.hasOwnProperty.call(cls, 'name') ? cls.name : undefined;
}

/**
 * Pull the callable form out of whatever a module exported.
 *
 * Returns `{ apply, inject, name }`, where `apply(ctx)` performs the
 * registration regardless of which shape was used.
 */
export function resolvePlugin(mod) {
  // 1. A module exporting `apply` directly.
  if (typeof mod.apply === 'function') {
    return { apply: mod.apply, inject: mod.inject, name: mod.name };
  }

  const d = mod.default;
  if (d) {
    // 2. A class. Checked *before* the object case, because every function —
    // classes included — inherits `Function.prototype.apply`. Testing for
    // `d.apply` first would find that inherited method and call the
    // constructor through it, which throws "cannot be invoked without new"
    // from somewhere that looks nothing like the cause.
    //
    // Cordis mounts a Service by construction, so constructing it *is*
    // applying it: the subclass registers from its constructor.
    if (typeof d === 'function') {
      return {
        apply: (ctx) => {
          new d(ctx);
        },
        inject: d.inject,
        name: pluginName(d),
      };
    }
    // 3. An object with its own `apply`.
    if (typeof d.apply === 'function') {
      return { apply: d.apply.bind(d), inject: d.inject, name: d.name };
    }
  }

  throw new TypeError(
    'not a DeepSeek Harness plugin: expected an exported apply(ctx), ' +
      'a default export with apply(ctx), or a default-exported Service class',
  );
}

/**
 * Import `entry`, check what it needs, and let it register its tools.
 *
 * Returns the registry and the context, so the caller can serve the tools
 * and dispose the plugin when the process ends.
 */
export async function loadPlugin(entry, { logger = console } = {}) {
  const url = entry.startsWith('file:') ? entry : pathToFileURL(entry).href;
  const mod = await import(url);
  const plugin = resolvePlugin(mod);

  // Before `apply` runs, not during: a plugin that registers three tools and
  // then asks for ctx.llm has already half-configured the server.
  assertInjectable(plugin.inject);

  const registry = new ToolRegistry();
  const ctx = createContext({ registry, logger });
  await plugin.apply(ctx);

  return { registry, ctx, name: plugin.name ?? 'dsh-plugin' };
}
