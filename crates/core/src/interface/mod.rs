//! Injectable contracts — the traits a host implements to replace part of the
//! engine.
//!
//! The entry requirement is narrow on purpose: **every module here defines at
//! least one trait the engine holds as a `dyn` object.** It used to say
//! "traits and types", which is how modules defining no contract at all ended
//! up here — a directory named for extension that a reader could not use to
//! find the extension points. Data types, configuration and concrete stores
//! live at the crate root instead (`base::settings`, `base::event`,
//! `base::prompt`, `base::memory`, `base::rules`); the old
//! `base::interface::*` paths still resolve via the re-exports below.
//!
//! # What is reachable from here, and what is not
//!
//! This crate has **no internal dependencies** — that is what keeps it at the
//! bottom of the graph. So this directory can only host contracts defined in
//! `base` itself:
//!
//! | Contract | Replaces |
//! |---|---|
//! | [`model::Model`] | the LLM backend / wire protocol |
//! | [`model_factory::ModelFactory`] | how a protocol is built from config |
//! | [`model_interceptor::ModelInterceptor`] | the request out, the message back |
//! | [`credentials::CredentialSource`] | where a provider's API key comes from |
//! | [`config_source::ConfigSource`] | where the settings layers come from |
//! | [`tool::Tool`] / [`tool::ToolRegistry`] | a tool, or the whole tool set |
//! | [`tool_middleware::ToolMiddleware`] | a ring around every tool call |
//! | [`tool_result::ToolResultTransformer`] | what a result may look like |
//! | [`scene::AgentScene`] | system prompt, tool surface, budgets |
//! | [`turn_policy::TurnPolicy`] | when a turn has gone on long enough |
//! | [`recovery_policy::RecoveryPolicy`] | what to do when a call goes wrong |
//! | [`budget_policy::BudgetPolicy`] | what a turn may spend and how much it may carry |
//! | [`environment::Environment`] | kept time and kept identifiers |
//! | [`exec::Process`] | where a program runs |
//! | [`exec::FileSystem`] | where a tool's files are |
//! | [`exec::Network`] | every request that leaves the process |
//! | [`exec::Sandbox`] | how a process is constrained |
//! | [`backoff::BackoffPolicy`] | whether a failed request is sent again, and when |
//! | [`permission::Permission`] | tool authorization |
//! | [`agent_spawner::AgentSpawner`] | how sub-agents are launched |
//! | [`import_callback::ImportCallback`] | the host's answer to an import prompt |
//! | [`log::Logger`] | log routing |
//! | [`event_sink::EventSink`] | where the engine's events go |
//! | [`health::HealthCheck`] | whether a subsystem is working |
//! | [`elicitation::Elicitation`] | how the engine asks a person something |
//! | [`tool::SecondaryLlm`] | the small model tools may call |
//! | [`token_counter::TokenCounter`] | how big the context is judged to be |
//! | [`prompt_registry::PromptRegistry`] | what goes into the system prompt |
//! | [`prompt_assembler::PromptAssembler`] | how the whole prompt is put together |
//! | [`prompt_assembly::AssemblyHook`] | a last pass over the assembled prompt |
//! | [`script::ScriptEngine`] | running the operator's own code at a hook point |
//! | [`memory_contracts::MemoryStorage`] | where durable memories are kept |
//! | [`memory_contracts::MemoryRetriever`] | which of them a turn sees |
//! | [`skill_provider::SkillProvider`] | where a session's skills come from |
//! | [`instruction_provider::InstructionProvider`] | where standing instructions come from |
//! | [`instruction_provider::RuleProvider`] | where the rule index comes from |
//!
//! [`catalog`] is the whole picture, including the contracts below that this
//! directory cannot re-export: every extension point as data, with when it
//! fires, what it may change, how often, and who is allowed to use it.
//!
//! The engine's other replaceable contracts live in the crate that owns them
//! and **cannot be re-exported here** without pointing a dependency edge
//! downward: `Compactor` (`compaction`), `HistoryStore` (`history`),
//! `PluginHost` (`runtime`), `TelemetryRecorder` (`telemetry`),
//! `PluginResolver` (`plugin`), `AutoClassifier` (`permissions`),
//! `McpClient` (`mcp`), `AnthropicClient` (`model`), `SearchProvider`
//! (`tools`). Anyone looking for the full map wants the extension-point
//! index, not this directory.

pub mod agent_spawner;
pub mod backoff;
pub mod capabilities;
pub mod catalog;
pub mod config_source;
pub mod credentials;
pub mod elicitation;
pub mod event_sink;
pub mod health;
#[deprecated(note = "moved to `base::event` — event payloads, not an injectable contract")]
pub use crate::event;
pub mod import_callback;
pub mod instruction_provider;
pub mod log;
#[deprecated(note = "moved to `base::memory` — a concrete store, not an injectable contract")]
pub use crate::memory;
pub mod budget_policy;
pub mod environment;
pub mod exec;
pub mod memory_contracts;
pub mod model;
pub mod model_factory;
pub mod model_interceptor;
pub mod permission;
pub mod prompt_assembler;
pub mod prompt_assembly;
pub mod prompt_middleware;
pub mod prompt_registry;
pub mod recovery_policy;
#[deprecated(note = "moved to `base::prompt` — prompt data types and the assembler function")]
pub use crate::prompt;
#[deprecated(note = "moved to `base::rules` — this module defines no extension contract")]
pub use crate::rules;
pub mod scene;
pub mod script;
pub mod script_adapters;
pub mod skill_provider;
pub mod token_counter;
pub mod tool;
pub mod tool_middleware;
pub mod tool_result;
pub mod turn_policy;
#[deprecated(note = "moved to `base::settings` — declarative configuration types")]
pub use crate::settings;

/// The re-exports above are the compatibility half of moving these modules;
/// nothing in this repo imports through them any more, so only a check like
/// this notices if one is dropped. `use` resolves at compile time, which is
/// the whole assertion — an external embedder still on the old path keeps
/// building.
#[cfg(test)]
#[allow(deprecated, unused_imports)]
mod old_paths_still_resolve {
    use crate::interface::event::AgentEvent;
    use crate::interface::memory::MemoryStore;
    use crate::interface::prompt::PromptBlock;
    use crate::interface::rules::RuleEntry;
    use crate::interface::settings::Settings;
    // `tool` and `log` moved *into* this directory; their pre-move paths were
    // at the crate root and are re-exported from `lib.rs`.
    use crate::log::Logger;
    use crate::tool::{Tool, ToolRegistry};
}
