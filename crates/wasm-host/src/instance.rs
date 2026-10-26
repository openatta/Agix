//! Running a component: one `Store` per call, with a deadline and a
//! cancellation point.
//!
//! The isolation story is entirely here. A `Store` owns a component's linear
//! memory, its tables, and its resource limits; creating one per call and
//! dropping it afterwards means a plugin that traps, spins or over-allocates
//! damages nothing that outlives the call. The cost is that a component has
//! no memory across calls — see [`crate::KvNamespace`] for where state goes
//! instead.
//!
//! Epoch interruption is what makes the deadline enforceable. It doesn't
//! terminate anything by itself here: it is configured to make the guest
//! *yield*, which turns an otherwise uninterruptible loop into a future the
//! host can drop. Both the timeout and the user's cancellation are then the
//! same operation — stop polling — rather than two mechanisms with different
//! failure modes.

use crate::bindings::PluginPre;
use crate::capabilities::ResolvedCapabilities;
use crate::engine::{ComponentHandle, WasmEngine};
use crate::state::{KvNamespace, PluginState, ProgressSink};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;
use wasmtime::component::Linker;
use wasmtime::Store;

/// How often the engine's epoch advances. Every tick is a point where a
/// running guest can yield, so it bounds how long a cancellation waits.
pub const EPOCH_TICK: Duration = Duration::from_millis(10);

/// Why a call ended without a result.
#[derive(Debug, PartialEq, Eq)]
pub enum CallFailure {
    /// The call outlived the plugin's declared `timeout_ms`.
    TimedOut,
    /// The caller withdrew — a cancelled turn, a closing session.
    Cancelled,
    /// The guest trapped, or the host could not run it.
    Faulted(String),
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => write!(f, "plugin call exceeded its declared timeout"),
            Self::Cancelled => write!(f, "plugin call was cancelled"),
            Self::Faulted(e) => write!(f, "plugin call failed: {e}"),
        }
    }
}

/// A loaded, linked component ready to be called.
///
/// Linking happens once (`InstancePre`); instantiation happens per call.
/// That split is what keeps per-call cost to the store rather than to
/// resolving imports again every time.
pub struct PluginInstance {
    engine: WasmEngine,
    pre: PluginPre<PluginState>,
    name: String,
    caps: Arc<ResolvedCapabilities>,
    kv: Arc<KvNamespace>,
    health: Arc<crate::health::Health>,
}

impl PluginInstance {
    pub fn link(
        engine: &WasmEngine,
        component: &ComponentHandle,
        name: String,
        caps: Arc<ResolvedCapabilities>,
    ) -> Result<Self> {
        Self::link_with_health(
            engine,
            component,
            name,
            caps,
            Arc::new(crate::health::Health::new()),
        )
    }

    /// Link, reusing an existing fault record.
    ///
    /// A caller that rebuilds instances — which is every install, uninstall,
    /// enable and disable — passes the record it already had, so a plugin
    /// that disabled itself does not come back because the user touched a
    /// different plugin. See [`crate::health::HealthRegistry`].
    pub fn link_with_health(
        engine: &WasmEngine,
        component: &ComponentHandle,
        name: String,
        caps: Arc<ResolvedCapabilities>,
        health: Arc<crate::health::Health>,
    ) -> Result<Self> {
        let mut linker: Linker<PluginState> = Linker::new(engine.inner());
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .map_err(|e| anyhow!("adding WASI to the plugin linker: {e}"))?;
        crate::bindings::Plugin::add_to_linker::<_, wasmtime::component::HasSelf<PluginState>>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| anyhow!("adding the agix:plugin host interface to the linker: {e}"))?;

        let pre = PluginPre::new(
            linker
                .instantiate_pre(component.component())
                .map_err(|e| anyhow!("linking plugin `{name}`: {e}"))?,
        )
        .map_err(|e| anyhow!("plugin `{name}` does not match the agix:plugin world: {e}"))?;

        Ok(Self {
            engine: engine.clone(),
            pre,
            name,
            caps,
            kv: Arc::new(KvNamespace::new()),
            health,
        })
    }

    /// This plugin's fault record — see [`crate::health`].
    pub fn health(&self) -> &Arc<crate::health::Health> {
        &self.health
    }

    /// Fold a call's outcome into the fault record. Faults are the plugin
    /// failing to answer; anything else is not held against it.
    fn note<T>(&self, outcome: Result<T, CallFailure>) -> Result<T, CallFailure> {
        match &outcome {
            Ok(_) => self.health.record_success(),
            Err(CallFailure::Faulted(_)) => self.health.record_fault(),
            Err(CallFailure::TimedOut) | Err(CallFailure::Cancelled) => {
                self.health.record_abandoned()
            }
        }
        outcome
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kv(&self) -> &Arc<KvNamespace> {
        &self.kv
    }

    /// The tools this component exports.
    ///
    /// The manifest's `tools` list is what the installer showed the user;
    /// this is what the engine registers. They can disagree — a component
    /// that grew a tool since packaging, say — and when they do, this wins,
    /// because it is the only one that reflects what will actually run.
    pub async fn list_tools(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Vec<crate::bindings::agix::plugin::types::ToolDef>, CallFailure> {
        let mut store = self
            .store()
            .map_err(|e| CallFailure::Faulted(e.to_string()))?;
        let call = async {
            let world = self.pre.instantiate_async(&mut store).await?;
            let tools = world
                .agix_plugin_tools()
                .call_list_tools(&mut store)
                .await?;
            Ok(tools)
        };
        self.note(with_deadline(call, self.caps.timeout, cancel).await)
    }

    /// Run one tool. The store this builds is dropped when the call returns,
    /// however it returns.
    pub async fn call_tool(
        &self,
        name: &str,
        input_json: &str,
        call_id: &str,
        progress: Option<Arc<dyn ProgressSink>>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<crate::bindings::agix::plugin::types::ToolOutput, CallFailure> {
        let mut store = self
            .store_with(progress)
            .map_err(|e| CallFailure::Faulted(e.to_string()))?;
        let call = async {
            let world = self.pre.instantiate_async(&mut store).await?;
            let out = world
                .agix_plugin_tools()
                .call_call_tool(&mut store, name, input_json, call_id)
                .await?;
            Ok(out)
        };
        self.note(with_deadline(call, self.caps.timeout, cancel).await)
    }

    /// Hand the component its validated configuration.
    pub async fn init(
        &self,
        config_json: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), CallFailure> {
        let mut store = self
            .store()
            .map_err(|e| CallFailure::Faulted(e.to_string()))?;
        let call = async {
            let world = self.pre.instantiate_async(&mut store).await?;
            match world.call_init(&mut store, config_json).await? {
                Ok(()) => Ok(()),
                Err(msg) => Err(anyhow!("plugin rejected its configuration: {msg}")),
            }
        };
        with_deadline(call, self.caps.timeout, cancel).await
    }

    /// Offer the component a lifecycle event.
    ///
    /// A component that does not export the `events` interface answers
    /// `Proceed`, which is also what a manifest declaring no events means.
    pub async fn on_event(
        &self,
        event: &str,
        payload_json: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<crate::bindings::agix::plugin::types::HookDecision, CallFailure> {
        let mut store = self
            .store()
            .map_err(|e| CallFailure::Faulted(e.to_string()))?;
        let call = async {
            let world = self.pre.instantiate_async(&mut store).await?;
            let decision = world
                .agix_plugin_events()
                .call_on_event(&mut store, event, payload_json)
                .await?;
            Ok(decision)
        };
        self.note(with_deadline(call, self.caps.timeout, cancel).await)
    }

    fn store(&self) -> Result<Store<PluginState>> {
        self.store_with(None)
    }

    /// Build the store for a single call.
    fn store_with(&self, progress: Option<Arc<dyn ProgressSink>>) -> Result<Store<PluginState>> {
        let state = PluginState::new(
            self.name.clone(),
            self.caps.clone(),
            self.kv.clone(),
            progress,
        )?;
        let mut store = Store::new(self.engine.inner(), state);
        store.limiter(|s| s.limiter());
        // Yield rather than trap: the host decides what a missed deadline
        // means, and it decides by dropping the future. A guest that yields
        // regularly is one the host can stop at any tick.
        store.set_epoch_deadline(1);
        store.epoch_deadline_async_yield_and_update(1);
        Ok(store)
    }
}

/// Run `call` against a deadline and a cancellation signal.
///
/// Timeout and cancellation converge on the same action — stop polling the
/// future, drop the store — so there is one code path for both rather than a
/// trap for one and a drop for the other.
pub async fn with_deadline<F, T>(
    call: F,
    timeout: Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<T, CallFailure>
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(CallFailure::Cancelled),
        _ = tokio::time::sleep(timeout) => Err(CallFailure::TimedOut),
        result = call => result.map_err(|e| CallFailure::Faulted(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn a_call_that_finishes_in_time_returns_its_value() {
        let cancel = CancellationToken::new();
        let out = with_deadline(async { Ok(7) }, Duration::from_secs(5), &cancel)
            .await
            .unwrap();
        assert_eq!(out, 7);
    }

    #[tokio::test]
    async fn a_call_that_outlives_its_deadline_times_out() {
        let cancel = CancellationToken::new();
        let never = async {
            std::future::pending::<()>().await;
            Ok(())
        };
        let err = with_deadline(never, Duration::from_millis(20), &cancel)
            .await
            .unwrap_err();
        assert_eq!(err, CallFailure::TimedOut);
    }

    /// Cancellation is checked first: a session that is closing should not
    /// wait out a plugin's timeout before it can shut down.
    #[tokio::test]
    async fn an_already_cancelled_caller_wins_over_the_deadline() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let never = async {
            std::future::pending::<()>().await;
            Ok(())
        };
        let err = with_deadline(never, Duration::from_secs(60), &cancel)
            .await
            .unwrap_err();
        assert_eq!(err, CallFailure::Cancelled);
    }

    #[tokio::test]
    async fn cancelling_mid_call_stops_it() {
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            token.cancel();
        });
        let never = async {
            std::future::pending::<()>().await;
            Ok(())
        };
        let err = with_deadline(never, Duration::from_secs(60), &cancel)
            .await
            .unwrap_err();
        assert_eq!(err, CallFailure::Cancelled);
    }

    #[tokio::test]
    async fn a_guest_fault_is_reported_with_its_message() {
        let cancel = CancellationToken::new();
        let err = with_deadline(
            async { Err::<(), _>(anyhow!("unreachable executed")) },
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap_err();
        match err {
            CallFailure::Faulted(msg) => assert!(msg.contains("unreachable"), "{msg}"),
            other => panic!("expected Faulted, got {other:?}"),
        }
    }

    #[test]
    fn the_failure_modes_read_differently_to_a_user() {
        let messages = [
            CallFailure::TimedOut.to_string(),
            CallFailure::Cancelled.to_string(),
            CallFailure::Faulted("trap".into()).to_string(),
        ];
        let unique: std::collections::HashSet<&String> = messages.iter().collect();
        assert_eq!(unique.len(), 3, "each failure mode needs its own wording");
    }
}
