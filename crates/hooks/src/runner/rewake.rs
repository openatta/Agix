//! `once` and async-rewake bookkeeping for `HookRunner`: which hooks have
//! already fired for good, which asked to be re-run when background work
//! signals completion, and the re-run itself.

use super::{HookOutcome, HookRunner};
use crate::config::{HookConfig, HookEvent};
use crate::payload::{HookInput, HookResponse};
use tokio_util::sync::CancellationToken;

impl HookRunner {
    pub(super) fn pending_indices(&self, event: HookEvent, configs: &[HookConfig]) -> Vec<usize> {
        let once_set = self
            .once_executed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        configs
            .iter()
            .enumerate()
            .filter(|(idx, cfg)| !cfg.is_once() || !once_set.contains(&(event, *idx)))
            .map(|(idx, _)| idx)
            .collect()
    }

    /// A hook with `async_rewake: true` that answers `rewake: true` is re-run
    /// when a wake signal fires.
    pub(super) fn remember_rewakes(
        &self,
        event: HookEvent,
        configs: &[HookConfig],
        indexed: &[(usize, HookOutcome)],
    ) {
        for &(idx, ref outcome) in indexed {
            let HookOutcome::Ran { ref response, .. } = outcome else {
                continue;
            };
            let is_async_rewake = matches!(
                configs.get(idx),
                Some(HookConfig::Command {
                    async_rewake: Some(true),
                    ..
                })
            );
            if response.rewake == Some(true) && is_async_rewake {
                self.pending_rewakes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert((event, idx));
            }
        }
    }

    pub(super) fn remember_once(
        &self,
        event: HookEvent,
        configs: &[HookConfig],
        indexed: &[(usize, HookOutcome)],
    ) {
        let mut once_set = self
            .once_executed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (idx, outcome) in indexed {
            if configs[*idx].is_once() && matches!(outcome, HookOutcome::Ran { .. }) {
                once_set.insert((event, *idx));
            }
        }
    }

    /// Check for wake signals and re-execute any pending rewake hooks.
    ///
    /// Drains the wake receiver; if a wake signal (or channel close) is
    /// detected, re-runs all hooks in `pending_rewakes` with a `WakeEvent`
    /// input built on `ambient` (the session's id, cwd and permission mode)
    /// and returns their responses.
    ///
    /// If no wake signal is available, the pending set is preserved so the
    /// hooks can be re-executed on a later call. Signals that arrive while no
    /// hook is waiting are discarded: work that finished before a hook asked
    /// to be woken is not what it is waiting for. The hooks stop with `stop`.
    pub async fn check_rewakes(
        &self,
        stop: &CancellationToken,
        ambient: HookInput,
    ) -> Vec<HookResponse> {
        let pending: Vec<(HookEvent, usize)> = {
            let mut set = self
                .pending_rewakes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if set.is_empty() {
                drop(set);
                if let Some(rx) = self
                    .wake_receiver
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                {
                    while rx.try_recv().is_ok() {}
                }
                return Vec::new();
            }
            set.drain().collect()
        };

        // Check for a wake signal on the channel
        let should_wake = {
            let mut rx_guard = self
                .wake_receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match rx_guard.as_mut() {
                Some(rx) => match rx.try_recv() {
                    Ok(()) => true,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => false,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        // Channel closed — treat as wake so pending hooks don't
                        // remain stuck forever.
                        true
                    }
                },
                None => false,
            }
        };

        if !should_wake {
            // No wake signal yet — restore the pending set for next time
            let mut set = self
                .pending_rewakes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            set.extend(pending);
            return Vec::new();
        }

        let wake_input = HookInput::lifecycle(
            "WakeEvent",
            ambient.session_id,
            ambient.cwd,
            ambient.permission_mode,
        );

        // Re-execute each pending hook with the wake input
        let mut responses = Vec::new();
        for (event, idx) in pending {
            let configs = match self.settings.get(&event) {
                Some(v) => v,
                None => continue,
            };
            let cfg = match configs.get(idx) {
                Some(c) => c,
                None => continue,
            };
            let outcome = self.run_one(cfg, &wake_input, stop).await;
            if let HookOutcome::Ran { response, .. } = outcome {
                responses.push(response);
            }
        }

        responses
    }
}
