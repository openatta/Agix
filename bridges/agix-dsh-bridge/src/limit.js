/**
 * A bound on how much work is in flight at once.
 *
 * MCP has no backpressure: a client that sends a hundred `tools/call`
 * messages is not told to slow down, so without a bound the bridge would
 * start a hundred `execute` calls and let Node sort it out. Refusing the
 * excess is not the answer either — a refusal becomes the client's error for
 * something it had no way to avoid. So they queue.
 */
export const DEFAULT_MAX_IN_FLIGHT = 8;

export function maxInFlight() {
  const raw = Number(process.env.AGIX_DSH_MAX_IN_FLIGHT);
  return Number.isFinite(raw) && raw > 0 ? Math.floor(raw) : DEFAULT_MAX_IN_FLIGHT;
}

/** Runs at most `limit` tasks concurrently, queueing the rest in order. */
export class Semaphore {
  constructor(limit = maxInFlight()) {
    this.limit = limit;
    this.active = 0;
    this.waiting = [];
  }

  /** Run `fn` when there is room. Resolves with whatever `fn` resolves to. */
  async run(fn) {
    if (this.active >= this.limit) {
      await new Promise((resolve) => this.waiting.push(resolve));
    }
    this.active += 1;
    try {
      return await fn();
    } finally {
      this.active -= 1;
      const next = this.waiting.shift();
      if (next) next();
    }
  }
}
