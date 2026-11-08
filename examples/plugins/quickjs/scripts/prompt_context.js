// `prompt.context` — a block recomputed at every assembly. The first call,
// with `null`, names and places it; later ones answer its text.
function onContext(ctx) {
  if (ctx === null) {
    return { name: "demo-js.today", order: 910 };
  }
  var tools = ctx.availableTools ? ctx.availableTools.split(",").length : 0;
  return "demo-quickjs: today is " + ctx.date + ", with " + tools + " tools on offer.";
}
