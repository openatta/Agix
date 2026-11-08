// `tool.result` — what the model reads of a result. Long results are cut
// down, and the cut is said out loud so the model knows to read in parts.
var LIMIT = 6000;

function onResult(result) {
  if (result.text.length <= LIMIT) {
    return null;
  }
  var cut = result.text.length - LIMIT;
  return result.text.slice(0, LIMIT) +
    "\n[demo-quickjs: trimmed " + cut + " characters; read the rest in parts]";
}
