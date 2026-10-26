/** The plugin shape DeepSeek Harness's own documentation shows. */
export const name = 'greet-plugin';
export const inject = ['tools'];

export function apply(ctx) {
  ctx.tools.register({
    name: 'greet',
    description: 'Greet someone by name.',
    parameters: {
      name: { type: 'string', required: true, description: 'The name to greet' },
      loud: { type: 'boolean', description: 'Shout it' },
    },
    output: {
      schema: { type: 'string' },
      render: (_args, value) => [{ type: 'text', text: value }],
    },
    async execute(args) {
      const greeting = `Hello, ${args.name}!`;
      return args.loud ? greeting.toUpperCase() : greeting;
    },
  });

  ctx.tools.register({
    name: 'explode',
    description: 'Always throws.',
    parameters: {},
    async execute() {
      throw new Error('as promised');
    },
  });
}
