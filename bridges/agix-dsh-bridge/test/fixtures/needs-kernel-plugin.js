/** Needs the harness's model service, which the bridge does not have. */
export const inject = ['tools', 'llm', 'sessions'];

export function apply(ctx) {
  ctx.tools.register({
    name: 'never-reached',
    description: 'This plugin must be refused before it can register.',
    parameters: {},
    async execute() {
      return 'unreachable';
    },
  });
}
