/**
 * The MCP server side: newline-delimited JSON-RPC over stdio.
 *
 * Only the methods a tool-providing server has to answer are implemented.
 * An MCP client asking for resources or prompts gets an empty list rather
 * than an error, because "this server has none" is true and is what the
 * client is really asking.
 */

import { toolToMcp } from './translate.js';

export const PROTOCOL_VERSION = '2025-06-18';

/** Errors defined by JSON-RPC itself. */
const METHOD_NOT_FOUND = -32601;
const INVALID_PARAMS = -32602;
const INTERNAL_ERROR = -32603;

/**
 * Handle one request, returning the response body.
 *
 * Pure over `{registry, serverName}` so it can be tested without a
 * transport — the wire format is the easy part, the semantics are not.
 */
export async function handleRequest(request, { registry, serverName, callTool }) {
  const { method, params } = request;

  switch (method) {
    case 'initialize':
      return {
        protocolVersion: PROTOCOL_VERSION,
        // Tools only. Declaring capabilities this bridge does not have would
        // invite calls it cannot answer.
        capabilities: { tools: {} },
        serverInfo: { name: serverName, version: '0.1.0' },
      };

    case 'tools/list':
      return { tools: registry.list().map(toolToMcp) };

    case 'tools/call': {
      const name = params?.name;
      const tool = name ? registry.get(name) : undefined;
      if (!tool) {
        throw rpcError(INVALID_PARAMS, `no such tool: ${name ?? '(unnamed)'}`);
      }
      return callTool(tool, params?.arguments ?? {});
    }

    // A tool-only server genuinely has none of these. Answering with an
    // empty list is the truthful reply; an error would read as a fault.
    case 'resources/list':
      return { resources: [] };
    case 'prompts/list':
      return { prompts: [] };

    // Notifications carry no reply.
    case 'notifications/initialized':
    case 'notifications/cancelled':
      return null;

    default:
      throw rpcError(METHOD_NOT_FOUND, `unsupported method: ${method}`);
  }
}

export function rpcError(code, message) {
  const e = new Error(message);
  e.rpcCode = code;
  return e;
}

/**
 * Drive the protocol over a line-oriented stream.
 *
 * `write` receives one complete JSON line at a time. Anything this function
 * cannot parse is answered with a JSON-RPC error rather than dropped: a
 * client that sent nonsense should be told, not left waiting.
 */
export async function serveLine(line, deps, write) {
  const trimmed = line.trim();
  if (trimmed === '') return;

  let request;
  try {
    request = JSON.parse(trimmed);
  } catch (e) {
    write({
      jsonrpc: '2.0',
      id: null,
      error: { code: -32700, message: `parse error: ${e.message}` },
    });
    return;
  }

  let result;
  try {
    result = await handleRequest(request, deps);
  } catch (e) {
    // A notification that fails has no one to tell.
    if (request.id === undefined || request.id === null) return;
    write({
      jsonrpc: '2.0',
      id: request.id,
      error: { code: e.rpcCode ?? INTERNAL_ERROR, message: e.message },
    });
    return;
  }

  // `null` marks a notification the bridge handled and owes no reply for.
  if (result === null) return;
  if (request.id === undefined || request.id === null) return;
  write({ jsonrpc: '2.0', id: request.id, result });
}
