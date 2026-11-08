// `prompt.block` — a fixed block, asked for once when the session is built.
// `{{demo_js_os}}` is filled in by prompt_variable.js at every assembly.
function onBlock() {
  return {
    name: "demo-js.house-rules",
    order: 470,
    content:
      "House rules (demo-quickjs): end every file you write with a newline, and " +
      "never write under generated/ by hand. This machine runs {{demo_js_os}}.",
  };
}
