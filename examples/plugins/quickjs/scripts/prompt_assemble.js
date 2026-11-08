// `prompt.assemble` — the whole prompt, last. A packaged script may only add:
// a block it changed or dropped would be refused and reported.
function onAssemble(blocks) {
  var named = blocks.filter(function (b) { return b.name; }).length;
  return blocks.concat([
    {
      name: "demo-js.checklist",
      content:
        "Checklist (demo-quickjs, " + named + " sections above): before your final " +
        "reply, make sure every file you were asked to write exists.",
    },
  ]);
}
