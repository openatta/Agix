// `memory.retrieval_hook` — both ends of a recall, told apart by `phase`.
// Before: ask for at most three. After: leave `scratch-` notes out.
function onRetrieval(recall) {
  if (recall.phase === "before") {
    return { limit: Math.min(recall.limit, 3) };
  }
  return recall.names.filter(function (name) {
    return name.indexOf("scratch-") !== 0;
  });
}
