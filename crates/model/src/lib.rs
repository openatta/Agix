//! LLM backend adapters — one [`base::interface::model::Model`] implementation
//! per wire protocol.
//!
//! - **Anthropic Messages API** ([`adapter::AnthropicModel`] over
//!   [`client::AnthropicClient`]) — the primary path.
//! - **OpenAI Chat Completions** ([`openai::OpenAICompatibleModel`]) — for
//!   OpenAI itself and the many gateways that only expose its shape (vLLM,
//!   Ollama, most third-party relays).
//!
//! Both translate the protocol-agnostic `PromptBlock` / `ToolDef` /
//! `ModelMessage` inputs into their own request format and normalize the
//! response back into a single `ModelEvent` stream, so nothing above this
//! crate needs to know which protocol is in use.

pub mod adapter;
mod cancel;
pub mod client;
pub mod error;
pub mod factory;
pub mod mock;
pub mod openai;
pub mod parser;
pub mod stream;
pub mod tokens;
pub mod types;

pub use openai::OpenAICompatibleModel;
