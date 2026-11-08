//! Runs a case against a daemon, over the daemon's own RPC surface, with
//! either a scripted model or a real one on the other end.
//!
//! The engine has a behavior net (`tests/runner`) that asks what the turn
//! loop decides. This asks a different question: given a settings file, a
//! project and a client speaking JSON-RPC, does the assembled product do what
//! it says — are the tools registered, the scripts bound, the telemetry
//! written, the permission asked. Those are properties of the composition
//! root, and every one of them has been broken at some point by a change that
//! left the libraries themselves passing.
//!
//! Two tiers, one harness. The stub tier answers from a script
//! ([`provider::ProviderStub`]): deterministic, offline, run on every push.
//! The live tier ([`live::LiveProvider`]) forwards to a real model: it asks
//! whether a model actually makes use of what the stub tier proves is wired —
//! sub-agents, tasks, teams, skills — and its cases build only with the
//! `real-model` feature. A case reaches the model through [`ModelEndpoint`]
//! and cannot tell which tier it got except by what it asserts.
//!
//! The pieces:
//!
//! - [`world::World`] is where the run happens — settings roots, projects,
//!   the socket. Nothing under it is shared with the machine.
//! - [`ModelEndpoint`] is the model, as a server, with both tiers keeping
//!   every request they were sent — the only place the effect of a prompt
//!   block or a bound script is visible from outside.
//! - [`handle::Daemon`] is the daemon itself and the connection to it.
//! - [`live::LiveRun`] is the scaffolding a live case repeats.
//!
//! The five surfaces a case can assert on: the RPC response, the
//! `session.event` frames, what reached the model, what is on disk (the
//! working directory, the transcript, the telemetry file), and the script
//! ledger `session.get` reports. The last two exist because the first three
//! cannot tell a script that ran and changed nothing from one that was never
//! bound.

pub mod handle;
pub mod live;
pub mod provider;
pub mod world;

pub use handle::{alternate_daemon_binary, Daemon, DaemonOptions, Host, Mode, Wire};
pub use live::{is_pro_model, Asked, Exchange, LiveConfig, LiveProvider, LiveRun, Tokens, Watcher};
pub use provider::{Block, ProviderStub, Reply, SeenRequest};
pub use world::World;

/// What a daemon is pointed at for its model: an Anthropic Messages endpoint
/// and the model name sessions default to.
pub trait ModelEndpoint {
    fn base_url(&self) -> url::Url;
    fn model(&self) -> &str;
}
