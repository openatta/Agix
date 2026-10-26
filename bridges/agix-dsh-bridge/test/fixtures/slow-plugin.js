/** Tools that take their time, for the concurrency and deadline tests. */
export const inject = ['tools'];

export function apply(ctx) {
  ctx.tools.register({
    name: 'slow',
    description: 'Resolve after a delay.',
    parameters: { ms: { type: 'number' } },
    async execute(args) {
      await new Promise((r) => setTimeout(r, args.ms ?? 50));
      return `waited ${args.ms ?? 50}ms`;
    },
  });

  ctx.tools.register({
    name: 'never',
    description: 'Never resolve.',
    parameters: {},
    execute() {
      return new Promise(() => {});
    },
  });

  ctx.tools.register({
    name: 'quick',
    description: 'Resolve immediately.',
    parameters: {},
    async execute() {
      return 'quick';
    },
  });
}
