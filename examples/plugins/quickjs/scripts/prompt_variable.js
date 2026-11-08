// `prompt.variable` — `{{demo_js_os}}`. The first call, with `null`, names the
// variable; every later one gets the session's environment and answers its value.
function onVariable(ctx) {
  if (ctx === null) {
    return { name: "demo_js_os" };
  }
  return ctx.os;
}
