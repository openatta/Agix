// `model.request` — the knobs of a request on its way out. This project works
// offline, so the model is not offered the tools that reach the web.
var OFFLINE = ["WebSearch", "WebFetch"];

function onRequest(req) {
  return {
    tools: req.tools.filter(function (t) { return OFFLINE.indexOf(t) < 0; }),
  };
}
