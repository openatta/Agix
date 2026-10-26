/** The object form Cordis also accepts. */
export default {
  name: 'object-plugin',
  inject: ['tools'],
  apply(ctx) {
    ctx.tools.register({
      name: 'ping',
      description: 'Reply pong.',
      parameters: {},
      async execute() {
        return 'pong';
      },
    });
  },
};
