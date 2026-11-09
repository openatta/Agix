//! The QuickJS layer as a plugin: what a set of script bindings produced,
//! handed to the session through the same `runtime::plugin::Plugin` interface
//! a Rust plugin uses.
//!
//! A delegate gets what `BoundScripts::for_delegate` leaves it: the rings
//! around tool and model calls, none of the prompt contributions, and none of
//! the carriers, so its calls count against the parent turn's quota.

use std::sync::Arc;

use base::interface::script_adapters::BoundScripts;
use runtime::plugin::{Layer, Plugin, PluginSession, Registrar};

pub struct ScriptPlugin {
    bound: BoundScripts,
}

impl ScriptPlugin {
    pub fn new(bound: BoundScripts) -> Self {
        Self { bound }
    }
}

impl Plugin for ScriptPlugin {
    fn name(&self) -> &str {
        "quickjs"
    }

    fn layer(&self) -> Layer {
        Layer::QuickJs
    }

    fn register(&self, session: &PluginSession<'_>, r: &mut Registrar) {
        if session.depth > 0 {
            install(&self.bound.for_delegate(), r);
        } else {
            install(&self.bound, r);
        }
    }
}

fn install(bound: &BoundScripts, r: &mut Registrar) {
    for m in &bound.tool_middleware {
        r.tool_middleware(m.clone());
    }
    for t in &bound.tool_results {
        r.tool_result(t.clone());
    }
    for i in &bound.model_interceptors {
        r.model_interceptor(i.clone());
    }
    for h in &bound.retrieval_hooks {
        r.retrieval_hook(h.clone());
    }
    for block in &bound.prompt_blocks {
        r.prompt_block(block.clone());
    }
    for (name, provider, origin) in &bound.prompt_variables {
        r.prompt_variable_from(name.clone(), provider.clone(), origin.clone());
    }
    for (hook, authority) in &bound.assembly_hooks {
        r.async_assembly_hook(hook.clone(), authority.clone());
    }
    if !bound.carriers.is_empty() {
        let carriers = bound.carriers.clone();
        r.on_turn_start(Arc::new(move || {
            for carrier in &carriers {
                carrier.begin_turn();
            }
        }));
    }
}
