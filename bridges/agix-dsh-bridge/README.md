# agix-dsh-bridge

Runs an external JS plugin as an MCP server, so Agix can use it without
knowing anything about the framework the plugin was written for.

## What this adapts, and what it does not

**The tool contract, not the plugin framework's kernel.** Such a plugin exports
`apply(ctx)` and registers tools with `ctx.tools.register(defineTool({…}))`.
That contract is stable, enumerable, and maps onto MCP almost directly:
`parameters` is a flat spelling of JSON Schema, `execute` is `tools/call`,
and `output.render`'s `[{type:'text', text}]` is already an MCP content
block.

Everything else the framework exposes — `ctx.llm`, `ctx.sessions`,
`ctx.agentLoop`, `ctx.sandbox`, and the rest — is its internal architecture,
still evolving, with contracts its own documentation says may change.
Following it would tie Agix's lifecycle, state model and event loop to
another project's kernel. So a plugin that injects anything beyond `tools` is
**refused at load**, by name, rather than accepted and left to fail at some
unpredictable later moment.

## Known gaps

The upstream tool contract does not define these, so neither does the bridge:

| Upstream | Here |
|---|---|
| No permission or confirmation model | Agix's `mcp__dsh-<pkg>` permission rules apply |
| No streaming or progress | `execute` returning is the end of the call |
| No abort signal | Cancellation is MCP cancellation, then killing the process |
| `ctx.effect` cleanup | Disposed together when the bridge exits |

## Usage

Declared by an Agix plugin rather than run by hand:

```toml
[[mcp]]
name = "pr-helper"
kind = "dsh"
entry = "dist/index.js"
```

Run directly for debugging:

```sh
node src/main.js ./path/to/plugin.js
```

It speaks newline-delimited JSON-RPC on stdin/stdout. Nothing is written to
stdout that is not a protocol message; diagnostics go to stderr.
