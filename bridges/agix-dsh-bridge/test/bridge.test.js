import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { loadPlugin, resolvePlugin } from '../src/load.js';
import {
  assertInjectable,
  createContext,
  normaliseInject,
  ToolRegistry,
  UnsupportedServiceError,
} from '../src/context.js';
import {
  callTool,
  normaliseContent,
  parametersToSchema,
  renderOutput,
  toolToMcp,
} from '../src/translate.js';
import { handleRequest, serveLine, PROTOCOL_VERSION } from '../src/server.js';
import { Semaphore, DEFAULT_MAX_IN_FLIGHT, maxInFlight } from '../src/limit.js';

const here = dirname(fileURLToPath(import.meta.url));
const fixture = (n) => join(here, 'fixtures', n);

// ── Loading: all three shapes Cordis accepts ──

test('a module exporting apply is a plugin', async () => {
  const { registry } = await loadPlugin(fixture('greet-plugin.js'));
  assert.deepEqual(
    registry.list().map((t) => t.name),
    ['greet', 'explode'],
  );
});

test('a default-exported object with apply is a plugin', async () => {
  const { registry, name } = await loadPlugin(fixture('object-plugin.js'));
  assert.equal(name, 'object-plugin');
  assert.ok(registry.get('ping'));
});

// Which form an author used is a style choice, not a capability difference;
// rejecting two of the three would refuse working plugins for no reason.
test('a default-exported class registers from its constructor', async () => {
  const { registry } = await loadPlugin(fixture('class-plugin.js'));
  assert.ok(registry.get('answer'));
});

test('a module that is not a plugin says what was expected', async () => {
  await assert.rejects(
    () => loadPlugin(fixture('../bridge.test.js')),
    /expected an exported apply/,
  );
});

test('resolvePlugin rejects a module with no recognisable entry point', () => {
  assert.throws(() => resolvePlugin({ something: 1 }), /not a DeepSeek Harness plugin/);
});

// ── The support boundary ──

// Refusing at load rather than at first use is the point: a plugin that
// registers three tools and only then reaches for ctx.llm has already
// half-configured the server.
test('a plugin needing the harness kernel is refused before it registers anything', async () => {
  await assert.rejects(
    () => loadPlugin(fixture('needs-kernel-plugin.js')),
    (e) => {
      assert.ok(e instanceof UnsupportedServiceError);
      assert.deepEqual(e.missing, ['llm', 'sessions']);
      assert.match(e.message, /llm, sessions/);
      assert.match(e.message, /tool contract, not the Cordis kernel/);
      return true;
    },
  );
});

test('injecting only what the bridge provides is accepted', () => {
  assert.deepEqual(assertInjectable(['tools']), ['tools']);
  assert.deepEqual(assertInjectable(undefined), []);
});

// Cordis lets a plugin mark a service optional, and an optional service it
// does without is not grounds for refusing the plugin.
test('optional services are not required', () => {
  assert.deepEqual(normaliseInject({ required: ['tools'], optional: ['llm'] }), ['tools']);
  assert.doesNotThrow(() => assertInjectable({ required: ['tools'], optional: ['llm'] }));
});

// ── The context shim ──

test('registering returns a disposer that really unregisters', () => {
  const r = new ToolRegistry();
  const dispose = r.register({ name: 't', execute: async () => 1 });
  assert.ok(r.get('t'));
  dispose();
  assert.equal(r.get('t'), undefined);
});

test('a tool with no name or no execute is refused', () => {
  const r = new ToolRegistry();
  assert.throws(() => r.register({ execute: async () => 1 }), /needs a name/);
  assert.throws(() => r.register({ name: 'x' }), /no execute function/);
});

// A plugin that opens a timer expects its cleanup to run; a bridge that
// dropped the disposers would leak them for the process's whole life.
test('effect cleanups run on dispose', () => {
  const ctx = createContext({ registry: new ToolRegistry() });
  let cleaned = 0;
  ctx.effect(() => () => {
    cleaned += 1;
  });
  ctx.effect(() => () => {
    cleaned += 1;
  });
  ctx.dispose();
  assert.equal(cleaned, 2);
});

test('a cleanup that throws does not prevent the others', () => {
  const errors = [];
  const ctx = createContext({
    registry: new ToolRegistry(),
    logger: { error: (...a) => errors.push(a) },
  });
  let cleaned = 0;
  ctx.effect(() => () => {
    throw new Error('boom');
  });
  ctx.effect(() => () => {
    cleaned += 1;
  });
  ctx.dispose();
  assert.equal(cleaned, 1);
  assert.equal(errors.length, 1);
});

// ── Parameters → JSON Schema ──

test('flat parameters become a JSON Schema object', () => {
  const schema = parametersToSchema({
    name: { type: 'string', required: true, description: 'who' },
    loud: { type: 'boolean' },
  });
  assert.deepEqual(schema, {
    type: 'object',
    properties: {
      name: { type: 'string', description: 'who' },
      loud: { type: 'boolean' },
    },
    required: ['name'],
  });
});

// `required` is a JSON Schema keyword and also DSH's per-field marker; it
// must not survive into the property it was describing.
test('the required marker does not leak into the property schema', () => {
  const schema = parametersToSchema({ a: { type: 'string', required: true } });
  assert.equal(schema.properties.a.required, undefined);
});

test('a tool that already wrote a full schema is passed through', () => {
  const original = { type: 'object', properties: { x: { type: 'number' } } };
  assert.equal(parametersToSchema(original), original);
});

test('no parameters means an object with no properties', () => {
  assert.deepEqual(parametersToSchema(undefined), { type: 'object', properties: {} });
  assert.equal(parametersToSchema({}).required, undefined);
});

test('a tool becomes an MCP listing with its schema', () => {
  const mcp = toolToMcp({
    name: 'greet',
    description: 'Greet someone.',
    parameters: { name: { type: 'string', required: true } },
  });
  assert.equal(mcp.name, 'greet');
  assert.equal(mcp.description, 'Greet someone.');
  assert.deepEqual(mcp.inputSchema.required, ['name']);
});

// ── Rendering ──

test("the tool's own render wins", () => {
  const tool = { output: { render: (_a, v) => [{ type: 'text', text: `<${v}>` }] } };
  assert.deepEqual(renderOutput(tool, {}, 'hi'), [{ type: 'text', text: '<hi>' }]);
});

// `[object Object]` is the least useful thing to hand a model.
test('without a render, a value is shown as itself or as JSON', () => {
  assert.deepEqual(renderOutput({}, {}, 'plain'), [{ type: 'text', text: 'plain' }]);
  const rendered = renderOutput({}, {}, { answer: 42 });
  assert.match(rendered[0].text, /"answer": 42/);
});

test('an informal render result is wrapped rather than rejected', () => {
  assert.deepEqual(normaliseContent('bare'), [{ type: 'text', text: 'bare' }]);
  assert.deepEqual(normaliseContent(['a', 'b']), [
    { type: 'text', text: 'a' },
    { type: 'text', text: 'b' },
  ]);
});

test('a well-formed content block is left alone', () => {
  const block = { type: 'image', data: 'x', mimeType: 'image/png' };
  assert.deepEqual(normaliseContent([block]), [block]);
});

// ── Calling ──

test('a call renders through the tool and reports success', async () => {
  const { registry } = await loadPlugin(fixture('greet-plugin.js'));
  const out = await callTool(registry.get('greet'), { name: 'Ada' });
  assert.equal(out.isError, false);
  assert.deepEqual(out.content, [{ type: 'text', text: 'Hello, Ada!' }]);
});

test('arguments reach the tool', async () => {
  const { registry } = await loadPlugin(fixture('greet-plugin.js'));
  const out = await callTool(registry.get('greet'), { name: 'Ada', loud: true });
  assert.equal(out.content[0].text, 'HELLO, ADA!');
});

// The model is what has to decide about a tool that failed, and it can only
// do that if it is told rather than the transport erroring out.
test('a throwing tool becomes an error result, not a transport failure', async () => {
  const { registry } = await loadPlugin(fixture('greet-plugin.js'));
  const out = await callTool(registry.get('explode'), {});
  assert.equal(out.isError, true);
  assert.match(out.content[0].text, /explode failed: as promised/);
});

// ── The MCP surface ──

async function deps() {
  const { registry } = await loadPlugin(fixture('greet-plugin.js'));
  return { registry, serverName: 'greet-plugin', callTool };
}

test('initialize declares tools and nothing else', async () => {
  const r = await handleRequest({ method: 'initialize' }, await deps());
  assert.equal(r.protocolVersion, PROTOCOL_VERSION);
  assert.deepEqual(Object.keys(r.capabilities), ['tools']);
  assert.equal(r.serverInfo.name, 'greet-plugin');
});

test('tools/list reports every registered tool', async () => {
  const r = await handleRequest({ method: 'tools/list' }, await deps());
  assert.deepEqual(
    r.tools.map((t) => t.name),
    ['greet', 'explode'],
  );
});

test('tools/call runs the named tool', async () => {
  const r = await handleRequest(
    { method: 'tools/call', params: { name: 'greet', arguments: { name: 'Ada' } } },
    await deps(),
  );
  assert.equal(r.content[0].text, 'Hello, Ada!');
});

test('calling a tool that does not exist names it', async () => {
  const d = await deps();
  await assert.rejects(
    () => handleRequest({ method: 'tools/call', params: { name: 'nope' } }, d),
    /no such tool: nope/,
  );
});

// "This server has none" is true and is what the client is asking; an error
// would read as a fault.
test('resources and prompts answer empty rather than erroring', async () => {
  const d = await deps();
  assert.deepEqual(await handleRequest({ method: 'resources/list' }, d), { resources: [] });
  assert.deepEqual(await handleRequest({ method: 'prompts/list' }, d), { prompts: [] });
});

test('an unknown method is a method-not-found', async () => {
  const d = await deps();
  await assert.rejects(
    () => handleRequest({ method: 'sampling/createMessage' }, d),
    /unsupported method/,
  );
});

// ── The wire ──

test('a request gets a response carrying its id', async () => {
  const written = [];
  await serveLine(
    JSON.stringify({ jsonrpc: '2.0', id: 7, method: 'tools/list' }),
    await deps(),
    (m) => written.push(m),
  );
  assert.equal(written.length, 1);
  assert.equal(written[0].id, 7);
  assert.equal(written[0].result.tools.length, 2);
});

test('a notification gets no response', async () => {
  const written = [];
  await serveLine(
    JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }),
    await deps(),
    (m) => written.push(m),
  );
  assert.equal(written.length, 0);
});

// A client that sent nonsense should be told, not left waiting.
test('unparseable input is answered with a parse error', async () => {
  const written = [];
  await serveLine('{not json', await deps(), (m) => written.push(m));
  assert.equal(written[0].error.code, -32700);
});

test('blank lines are ignored', async () => {
  const written = [];
  await serveLine('   ', await deps(), (m) => written.push(m));
  assert.equal(written.length, 0);
});

test('a failing request becomes a JSON-RPC error with its id', async () => {
  const written = [];
  await serveLine(
    JSON.stringify({ jsonrpc: '2.0', id: 3, method: 'nope' }),
    await deps(),
    (m) => written.push(m),
  );
  assert.equal(written[0].id, 3);
  assert.equal(written[0].error.code, -32601);
});

// Regression: every function inherits Function.prototype.apply, so a
// default-exported class satisfies `typeof d.apply === 'function'`. Checking
// the object shape first found that inherited method and called the
// constructor through it — a failure whose message pointed nowhere near the
// cause.
test('a class is not mistaken for an object with an apply method', () => {
  class Service {
    constructor(ctx) {
      ctx.tools.register({ name: 'x', execute: async () => 1 });
    }
  }
  const plugin = resolvePlugin({ default: Service });
  const registry = new ToolRegistry();
  plugin.apply(createContext({ registry }));
  assert.ok(registry.get('x'));
});

// ── The process, over real stdio ──

test('the bridge serves a plugin over stdio end to end', async () => {
  const { spawn } = await import('node:child_process');
  const child = spawn(
    process.execPath,
    [join(here, '..', 'src', 'main.js'), fixture('greet-plugin.js')],
    { stdio: ['pipe', 'pipe', 'pipe'] },
  );

  const lines = [];
  let buffered = '';
  child.stdout.on('data', (chunk) => {
    buffered += chunk.toString();
    let i;
    while ((i = buffered.indexOf('\n')) >= 0) {
      const line = buffered.slice(0, i);
      buffered = buffered.slice(i + 1);
      if (line.trim()) lines.push(JSON.parse(line));
    }
  });

  const send = (msg) => child.stdin.write(`${JSON.stringify(msg)}\n`);
  send({ jsonrpc: '2.0', id: 1, method: 'initialize' });
  send({ jsonrpc: '2.0', id: 2, method: 'tools/list' });
  send({
    jsonrpc: '2.0',
    id: 3,
    method: 'tools/call',
    params: { name: 'greet', arguments: { name: 'Ada' } },
  });

  await new Promise((resolve) => {
    const check = () => (lines.length >= 3 ? resolve() : setTimeout(check, 10));
    check();
  });

  child.stdin.end();
  await new Promise((resolve) => child.on('close', resolve));

  assert.equal(lines[0].result.serverInfo.name, 'greet-plugin');
  assert.deepEqual(
    lines[1].result.tools.map((t) => t.name),
    ['greet', 'explode'],
  );
  assert.equal(lines[2].result.content[0].text, 'Hello, Ada!');
});

// Everything on stdout is a protocol message; one stray log line there
// corrupts the stream for the client.
test('diagnostics go to stderr, never stdout', async () => {
  const { spawn } = await import('node:child_process');
  const child = spawn(
    process.execPath,
    [join(here, '..', 'src', 'main.js'), fixture('greet-plugin.js')],
    { stdio: ['pipe', 'pipe', 'pipe'] },
  );

  let out = '';
  let err = '';
  child.stdout.on('data', (c) => (out += c.toString()));
  child.stderr.on('data', (c) => (err += c.toString()));

  child.stdin.end();
  await new Promise((resolve) => child.on('close', resolve));

  assert.equal(out, '', 'nothing but protocol messages may reach stdout');
  assert.match(err, /serving 2 tool\(s\)/);
});

// An MCP server that starts and then fails every call is harder to diagnose
// than one that refuses to start and says why.
test('an unsupported plugin makes the process refuse to start', async () => {
  const { spawn } = await import('node:child_process');
  const child = spawn(
    process.execPath,
    [join(here, '..', 'src', 'main.js'), fixture('needs-kernel-plugin.js')],
    { stdio: ['pipe', 'pipe', 'pipe'] },
  );

  let err = '';
  child.stderr.on('data', (c) => (err += c.toString()));
  const code = await new Promise((resolve) => child.on('close', resolve));

  assert.equal(code, 1);
  assert.match(err, /llm, sessions/);
});

// ── Concurrency and deadlines ──

// DSH's contract has no timeout and no abort signal, so without this a tool
// that never returns is indistinguishable from a hung server.
test('a tool that never returns is reported rather than waited on forever', async () => {
  const { registry } = await loadPlugin(fixture('slow-plugin.js'));
  const started = Date.now();
  const out = await callTool(registry.get('never'), {}, { timeoutMs: 60 });

  assert.equal(out.isError, true);
  assert.match(out.content[0].text, /did not return within 60ms/);
  assert.ok(Date.now() - started < 5000, 'it must actually give up');
});

test('a tool finishing inside its deadline is unaffected', async () => {
  const { registry } = await loadPlugin(fixture('slow-plugin.js'));
  const out = await callTool(registry.get('slow'), { ms: 10 }, { timeoutMs: 5000 });
  assert.equal(out.isError, false);
  assert.equal(out.content[0].text, 'waited 10ms');
});

// JSON-RPC pairs a response to its request by id and says nothing about
// ordering, so serialising would only mean one slow tool blocks every
// request behind it — including the tools/list a client sends to find out
// what is going on.
test('a slow request does not block the ones behind it', async () => {
  const { spawn } = await import('node:child_process');
  const child = spawn(
    process.execPath,
    [join(here, '..', 'src', 'main.js'), fixture('slow-plugin.js')],
    { stdio: ['pipe', 'pipe', 'pipe'] },
  );

  const lines = [];
  let buffered = '';
  child.stdout.on('data', (chunk) => {
    buffered += chunk.toString();
    let i;
    while ((i = buffered.indexOf('\n')) >= 0) {
      const line = buffered.slice(0, i);
      buffered = buffered.slice(i + 1);
      if (line.trim()) lines.push(JSON.parse(line));
    }
  });

  const send = (msg) => child.stdin.write(`${JSON.stringify(msg)}\n`);
  send({
    jsonrpc: '2.0',
    id: 'slow',
    method: 'tools/call',
    params: { name: 'slow', arguments: { ms: 300 } },
  });
  send({
    jsonrpc: '2.0',
    id: 'quick',
    method: 'tools/call',
    params: { name: 'quick', arguments: {} },
  });

  await new Promise((resolve) => {
    const check = () => (lines.length >= 1 ? resolve() : setTimeout(check, 5));
    check();
  });

  assert.equal(
    lines[0].id,
    'quick',
    'the fast tool must answer first even though it was asked second',
  );

  await new Promise((resolve) => {
    const check = () => (lines.length >= 2 ? resolve() : setTimeout(check, 10));
    check();
  });
  assert.equal(lines[1].id, 'slow');

  child.stdin.end();
  await new Promise((resolve) => child.on('close', resolve));
});

// Concurrent responses must not interleave halfway through a line.
test('every response is written as one whole line', async () => {
  const { spawn } = await import('node:child_process');
  const child = spawn(
    process.execPath,
    [join(here, '..', 'src', 'main.js'), fixture('slow-plugin.js')],
    { stdio: ['pipe', 'pipe', 'pipe'] },
  );

  let out = '';
  child.stdout.on('data', (c) => (out += c.toString()));

  for (let i = 0; i < 20; i += 1) {
    child.stdin.write(
      `${JSON.stringify({
        jsonrpc: '2.0',
        id: i,
        method: 'tools/call',
        params: { name: 'slow', arguments: { ms: i % 5 } },
      })}\n`,
    );
  }
  child.stdin.end();
  await new Promise((resolve) => child.on('close', resolve));

  const lines = out.split('\n').filter((l) => l.trim());
  assert.equal(lines.length, 20);
  for (const line of lines) {
    assert.doesNotThrow(() => JSON.parse(line), `not a whole JSON line: ${line}`);
  }
});

// ── The in-flight bound ──

test('a semaphore never runs more than its limit at once', async () => {
  const gate = new Semaphore(3);
  let active = 0;
  let peak = 0;

  await Promise.all(
    Array.from({ length: 20 }, () =>
      gate.run(async () => {
        active += 1;
        peak = Math.max(peak, active);
        await new Promise((r) => setTimeout(r, 5));
        active -= 1;
      }),
    ),
  );

  assert.equal(peak, 3, `peak concurrency was ${peak}`);
  assert.equal(active, 0);
});

// Excess work waits rather than being refused: a refusal would be the
// client's error for something it had no way to avoid.
test('work over the limit is queued, not dropped', async () => {
  const gate = new Semaphore(1);
  const order = [];
  await Promise.all([
    gate.run(async () => {
      await new Promise((r) => setTimeout(r, 20));
      order.push('first');
    }),
    gate.run(async () => order.push('second')),
    gate.run(async () => order.push('third')),
  ]);
  assert.deepEqual(order, ['first', 'second', 'third']);
});

test('a task that throws still releases its slot', async () => {
  const gate = new Semaphore(1);
  await assert.rejects(() => gate.run(async () => { throw new Error('boom'); }));
  assert.equal(await gate.run(async () => 'after'), 'after');
});

test('the limit is configurable and has a sane default', () => {
  assert.equal(maxInFlight(), DEFAULT_MAX_IN_FLIGHT);
  process.env.AGIX_DSH_MAX_IN_FLIGHT = '2';
  assert.equal(maxInFlight(), 2);
  process.env.AGIX_DSH_MAX_IN_FLIGHT = 'nonsense';
  assert.equal(maxInFlight(), DEFAULT_MAX_IN_FLIGHT, 'nonsense falls back');
  delete process.env.AGIX_DSH_MAX_IN_FLIGHT;
});

// Everything sent must still be answered, however tight the bound.
test('every request is answered even when the bound is tight', async () => {
  const { spawn } = await import('node:child_process');
  const child = spawn(
    process.execPath,
    [join(here, '..', 'src', 'main.js'), fixture('slow-plugin.js')],
    { stdio: ['pipe', 'pipe', 'pipe'], env: { ...process.env, AGIX_DSH_MAX_IN_FLIGHT: '2' } },
  );

  let out = '';
  child.stdout.on('data', (c) => (out += c.toString()));

  for (let i = 0; i < 12; i += 1) {
    child.stdin.write(
      `${JSON.stringify({
        jsonrpc: '2.0',
        id: i,
        method: 'tools/call',
        params: { name: 'slow', arguments: { ms: 5 } },
      })}\n`,
    );
  }
  child.stdin.end();
  await new Promise((resolve) => child.on('close', resolve));

  const ids = out
    .split('\n')
    .filter((l) => l.trim())
    .map((l) => JSON.parse(l).id)
    .sort((a, b) => a - b);
  assert.deepEqual(ids, [...Array(12).keys()]);
});
