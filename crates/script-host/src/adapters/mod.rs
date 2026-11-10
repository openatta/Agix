//! Scripts, bound to the points they can be bound to.
//!
//! Each adapter here is the same three steps: encode what the point offers as
//! JSON, call the script, and apply what came back through the point's own
//! API so the script is subject to the same rules as any other extension.
//!
//! # The rules every adapter follows
//!
//! **A script that fails changes nothing.** Not politeness — a prompt half
//! edited by a script that died mid-pass is worse than an unedited one,
//! because nothing downstream can tell which it is looking at. Every adapter
//! computes its change fully, then applies it, and on any error leaves the
//! point exactly as it found it.
//!
//! **A script never widens its own authority.** Provenance travels on the
//! carrier: a script the operator wrote may rewrite, one that arrived with a
//! downloaded plugin may add. An adapter reads that; it does not decide it.
//!
//! **Synchronous points call synchronously.** Most hook points are `Fn` or
//! `&self` by contract and cannot await. Those use
//! [`ScriptCarrier::call_blocking`](crate::carrier::ScriptCarrier::call_blocking),
//! whose cost is one thread held for at most the script's timeout — bounded
//! inside the interpreter rather than by a future somebody can drop.
//!
//! # What is deliberately not bound
//!
//! `history.append_observer` fires once per log entry, which is the frequency
//! band the catalog closes to scripts on purpose: the cost of a callback there
//! is invisible to whoever writes one. `history.extension_entry` is a write
//! capability rather than a hook — a script needs an API to emit an entry, not
//! a callback to receive one. `hooks` is its own subsystem with its own
//! process model. And `script.carrier` is the carrier itself.

mod bound;
mod memory_retrieval;
mod model_message;
mod model_request;
mod prompt_assembly;
mod prompt_contributions;
mod tool_around;
mod tool_result;

pub use bound::BoundScripts;
pub use memory_retrieval::RetrievalHookScript;
pub use model_message::ModelMessageScript;
pub use model_request::ModelRequestScript;
pub use prompt_assembly::PromptAssemblyScript;
pub use prompt_contributions::{
    prompt_block_from_script, prompt_context_from_script, prompt_variable_from_script,
};
pub use tool_around::ToolAroundScript;
pub use tool_result::ToolResultScript;
