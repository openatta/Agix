//! `demo-rust` — an example Rust plugin that uses every registration
//! `runtime::plugin::Registrar` offers, each for a small job of its own:
//!
//! | Registration | Here |
//! |---|---|
//! | `tool` | `DemoStamp`, a fingerprint of a piece of text (`tool.rs`) |
//! | `agent_type` | `demo-auditor`, a read-only sub-agent (`auditor`, below) |
//! | `tool_middleware` | refuses any file ending in `.secret` (`policy.rs`) |
//! | `tool_result` | redacts `SECRET-…` tokens from what tools return (`policy.rs`) |
//! | `model_interceptor` | caps `max_tokens`, counts replies (`policy.rs`) |
//! | `retrieval_hook` | leaves `draft-` memories out of recall (`policy.rs`) |
//! | `prompt_middleware` | expands `!stamp <text>` (`policy.rs`) |
//! | `prompt_block`, `prompt_variable` | the guide, naming the project (`prompt.rs`) |
//! | `prompt_context`, `prompt_variable_from` | this session's activity (`prompt.rs`) |
//! | `assembly_hook` | an outline of the prompt's sections (`prompt.rs`) |
//! | `async_assembly_hook` | the project's `.demo/notice.md`, read per turn (`prompt.rs`) |
//! | `health_check`, `on_turn_start` | what the session did, for `session.get` (`activity.rs`) |
//!
//! A host adds it with `Builder::plugin`, or for a daemon with
//! `daemon::Assembly { plugins: vec![Arc::new(DemoPlugin::new())], .. }`.
//! `docs/plugin_development.md` walks through it.

mod activity;
mod policy;
mod prompt;
mod tool;

use std::sync::Arc;

use base::agent_type::{AgentTypeDefinition, AgentTypeSource};
use runtime::plugin::{Plugin, PluginSession, Registrar};

pub use activity::Activity;
pub use policy::{redact, SECRET_SUFFIX};
pub use tool::stamp;

pub const NAME: &str = "demo-rust";
pub const AUDITOR: &str = "demo-auditor";
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;

pub struct DemoPlugin {
    max_tokens: u32,
}

impl DemoPlugin {
    pub fn new() -> Self {
        Self {
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

impl Default for DemoPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for DemoPlugin {
    fn name(&self) -> &str {
        NAME
    }

    fn register(&self, session: &PluginSession<'_>, r: &mut Registrar) {
        let activity = Arc::new(Activity::default());

        r.tool(Arc::new(tool::StampTool::new(activity.clone())))
            .agent_type(auditor())
            .tool_middleware(Arc::new(policy::SecretFiles::new(activity.clone())))
            .tool_result(Arc::new(policy::RedactSecrets::new(activity.clone())))
            .model_interceptor(Arc::new(policy::TokenCeiling::new(
                self.max_tokens,
                activity.clone(),
            )))
            .retrieval_hook(Arc::new(policy::SkipDrafts))
            .health_check(Arc::new(activity::ActivityCheck::new(activity.clone())))
            .on_turn_start(activity.turn_counter());

        // A sub-agent has its own scene and prompt, and no user typing into
        // it: what follows it is the policy above, not the prompt below.
        if session.depth > 0 {
            return;
        }
        r.prompt_middleware(Arc::new(policy::StampShortcut));
        prompt::register(r, session.project_root, activity);
    }
}

fn auditor() -> AgentTypeDefinition {
    AgentTypeDefinition {
        name: AUDITOR.into(),
        // Compiled into the host, so trusted as the built-in types are;
        // `Plugin` is for definitions that arrived in a package.
        source: AgentTypeSource::Builtin,
        description: "Reads files looking for leftover secrets and reports what it found. \
                      Read-only."
            .into(),
        allowed_tools: ["Read", "Grep", "Glob", tool::NAME]
            .map(String::from)
            .to_vec(),
        max_turns: Some(20),
        system_prompt: format!(
            "You are the demo auditor. Look through the files you are pointed at for \
             credentials, tokens and keys left behind. Do not change anything. When you \
             finish, reply with one line per finding, then the line `AUDIT: <n> findings`, \
             then a line `STAMP: <stamp>` where <stamp> is what the {} tool returns for the \
             text `audited`.",
            tool::NAME
        ),
        ..Default::default()
    }
}
