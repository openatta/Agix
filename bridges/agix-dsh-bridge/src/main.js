#!/usr/bin/env node
/**
 * Entry point: load a DSH plugin, serve its tools over MCP on stdio.
 *
 * Usage: `agix-dsh-bridge <path-to-plugin.js>`
 *
 * stdout carries protocol messages and nothing else. Every diagnostic goes
 * to stderr, because one stray line on stdout corrupts the stream for the
 * client.
 */

import { createInterface } from 'node:readline';
import { basename } from 'node:path';
import { loadPlugin } from './load.js';
import { serveLine } from './server.js';
import { callTool } from './translate.js';
import { Semaphore } from './limit.js';

async function main(argv) {
  const entry = argv[2];
  if (!entry) {
    process.stderr.write('usage: agix-dsh-bridge <path-to-plugin.js>\n');
    process.exit(2);
  }

  let loaded;
  try {
    loaded = await loadPlugin(entry);
  } catch (e) {
    // Refusing to start is the right outcome for an unsupported plugin: an
    // MCP server that came up and then failed every call would be harder to
    // diagnose than one that never came up and said why.
    process.stderr.write(`agix-dsh-bridge: ${e.message}\n`);
    process.exit(1);
  }

  const { registry, ctx, name } = loaded;
  const serverName = name || basename(entry);
  process.stderr.write(
    `agix-dsh-bridge: serving ${registry.list().length} tool(s) from ${serverName}\n`,
  );

  // One `write` call per message, so concurrent responses cannot interleave
  // halfway through a line.
  const write = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);
  const deps = { registry, serverName, callTool };

  // Requests are not awaited in order. JSON-RPC pairs a response to its
  // request by id and says nothing about ordering, so serialising here would
  // only mean that one slow tool blocks every request behind it — including
  // the `tools/list` a client sends to find out what is going on.
  //
  // Bounded, though: MCP has no backpressure, so a client that sends a
  // hundred calls would otherwise have a hundred `execute` bodies running.
  // Excess requests wait their turn rather than being refused — a refusal
  // would be the client's error for something it had no way to avoid.
  const gate = new Semaphore();
  const inFlight = new Set();
  const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of rl) {
    const task = gate
      .run(() => serveLine(line, deps, write))
      .finally(() => inFlight.delete(task));
    inFlight.add(task);
  }

  // stdin closed: the client is gone. Let whatever is still running finish
  // answering before the plugin's cleanup runs underneath it.
  await Promise.allSettled(inFlight);
  ctx.dispose();
}

main(process.argv).catch((e) => {
  process.stderr.write(`agix-dsh-bridge: ${e.stack ?? e}\n`);
  process.exit(1);
});
