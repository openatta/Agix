// `model.message` — a finished message before it is recorded. Runs of blank
// lines are folded to one; the array returned has one entry per text block.
function onMessage(msg) {
  return msg.text.map(function (t) {
    return t.replace(/\n{3,}/g, "\n\n");
  });
}
