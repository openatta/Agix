/** The Service-subclass form, which registers from its constructor. */
export default class ClassPlugin {
  static inject = ['tools'];
  static name = 'class-plugin';

  constructor(ctx) {
    ctx.tools.register({
      name: 'answer',
      description: 'Return the answer.',
      parameters: {},
      async execute() {
        return { answer: 42 };
      },
    });
  }
}
