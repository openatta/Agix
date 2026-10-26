//! The `host` interface a component imports.
//!
//! Every function here is a place where a plugin asks the host for something
//! it cannot do itself, so every one of them is a place the capability list
//! is enforced. The pattern is uniform on purpose: check the declaration,
//! and on a miss return the guest an error that names what was refused —
//! silently returning empty would leave a plugin author debugging the wrong
//! thing.

use crate::bindings::agix::plugin::host::Host;
use crate::state::PluginState;

/// The `types` interface declares only records, so its generated `Host`
/// trait carries no methods — it exists so the linker can be sure the shape
/// definitions are accounted for. Implementing it is the acknowledgement.
impl crate::bindings::agix::plugin::types::Host for PluginState {}

impl Host for PluginState {
    async fn log(&mut self, level: String, msg: String) {
        match level.as_str() {
            "error" => tracing::error!(plugin = %self.plugin, "{msg}"),
            "warn" => tracing::warn!(plugin = %self.plugin, "{msg}"),
            "debug" | "trace" => tracing::debug!(plugin = %self.plugin, "{msg}"),
            _ => tracing::info!(plugin = %self.plugin, "{msg}"),
        }
    }

    async fn progress(&mut self, call_id: String, text: String) {
        if let Some(sink) = &self.progress {
            sink.on_progress(&call_id, &text);
        }
    }

    async fn now_ms(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    async fn http_request(
        &mut self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, String> {
        if !self.caps.allows_url(&url) {
            // Names the host, never the URL. A plugin may have built the URL
            // from a token it fetched through `secret`, and this string is
            // returned to the guest — which typically hands it straight back
            // as a tool result, into the model's context and the transcript.
            let refused = crate::capabilities::host_of(&url)
                .unwrap_or_else(|| "(not an http(s) URL)".to_string());
            return Err(format!(
                "network access to `{refused}` is not in this plugin's declared `net` capability"
            ));
        }
        let mut request = base::interface::exec::HttpRequest::new(&method, &url);
        for (k, v) in headers {
            request = request.header(k, v);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        // `Origin::Operator`: the hosts a plugin may reach are the ones its
        // own manifest declares and an operator accepted at install time, and
        // `allows_url` above is what enforces that.
        let response = base::interface::exec::Network::send(
            &*self.net,
            request,
            base::interface::exec::Origin::Operator,
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(response.body)
    }

    async fn secret(&mut self, key: String) -> Option<String> {
        if !self.caps.allows_env(&key) {
            tracing::warn!(
                plugin = %self.plugin,
                key = %key,
                "plugin asked for an environment variable it did not declare"
            );
            return None;
        }
        std::env::var(&key).ok()
    }

    async fn kv_get(&mut self, key: String) -> Option<Vec<u8>> {
        self.kv.get(&key)
    }

    async fn kv_set(&mut self, key: String, value: Vec<u8>) {
        self.kv.set(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{KvNamespace, ProgressSink};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    fn state_with(net: Vec<String>, env: Vec<String>) -> PluginState {
        let c = plugin::manifest::Capabilities {
            net,
            env,
            ..Default::default()
        };
        let caps = crate::capabilities::resolve(&c, Path::new("/ws"), Path::new("/plug")).unwrap();
        PluginState::new("p", Arc::new(caps), Arc::new(KvNamespace::new()), None).unwrap()
    }

    #[tokio::test]
    async fn an_undeclared_host_is_refused_before_any_request_is_made() {
        let mut s = state_with(vec!["allowed.example".into()], vec![]);
        let err = s
            .http_request("GET".into(), "https://evil.example/x".into(), vec![], None)
            .await
            .unwrap_err();
        assert!(err.contains("evil.example"), "{err}");
        assert!(
            err.contains("net"),
            "the message should name the capability that would have allowed it: {err}"
        );
    }

    /// The guest usually returns this string as its tool result, so anything
    /// in it reaches the model's context and the session transcript. A URL a
    /// plugin built from a token it fetched through `secret` must not be
    /// echoed back.
    #[tokio::test]
    async fn a_refusal_names_the_host_and_never_the_credentials() {
        let mut s = state_with(vec!["allowed.example".into()], vec![]);
        let err = s
            .http_request(
                "GET".into(),
                "https://user:sup3r-secret@evil.example/path?token=also-secret".into(),
                vec![],
                None,
            )
            .await
            .unwrap_err();

        assert!(
            err.contains("evil.example"),
            "the host is what was refused: {err}"
        );
        assert!(!err.contains("sup3r-secret"), "credentials leaked: {err}");
        assert!(!err.contains("also-secret"), "query secrets leaked: {err}");
        assert!(
            !err.contains("/path"),
            "the path is not needed to explain this: {err}"
        );
    }

    #[tokio::test]
    async fn a_refusal_for_something_that_is_not_a_url_says_so() {
        let mut s = state_with(vec!["allowed.example".into()], vec![]);
        let err = s
            .http_request("GET".into(), "file:///etc/passwd".into(), vec![], None)
            .await
            .unwrap_err();
        assert!(!err.contains("/etc/passwd"), "{err}");
        assert!(err.contains("not an http"), "{err}");
    }

    /// A plugin with no `net` declaration cannot reach anything, which is the
    /// default the whole capability model rests on.
    #[tokio::test]
    async fn no_net_declaration_means_no_network_at_all() {
        let mut s = state_with(vec![], vec![]);
        assert!(s
            .http_request("GET".into(), "https://example.com/".into(), vec![], None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_undeclared_environment_variable_reads_as_absent() {
        std::env::set_var("AGIX_TEST_DECLARED", "yes");
        std::env::set_var("AGIX_TEST_UNDECLARED", "also-yes");

        let mut s = state_with(vec![], vec!["AGIX_TEST_DECLARED".into()]);
        assert_eq!(
            s.secret("AGIX_TEST_DECLARED".into()).await.as_deref(),
            Some("yes")
        );
        assert_eq!(
            s.secret("AGIX_TEST_UNDECLARED".into()).await,
            None,
            "a variable that exists but wasn't declared must still be invisible"
        );
    }

    #[tokio::test]
    async fn kv_round_trips_through_the_host() {
        let mut s = state_with(vec![], vec![]);
        assert_eq!(s.kv_get("k".into()).await, None);
        s.kv_set("k".into(), b"v".to_vec()).await;
        assert_eq!(s.kv_get("k".into()).await.as_deref(), Some(&b"v"[..]));
    }

    #[tokio::test]
    async fn progress_reaches_the_sink_with_its_call_id() {
        #[derive(Default)]
        struct Recorder(Mutex<Vec<(String, String)>>);
        impl ProgressSink for Recorder {
            fn on_progress(&self, call_id: &str, text: &str) {
                self.0
                    .lock()
                    .unwrap()
                    .push((call_id.to_string(), text.to_string()));
            }
        }

        let recorder = Arc::new(Recorder::default());
        let caps = crate::capabilities::resolve(
            &plugin::manifest::Capabilities::default(),
            Path::new("/ws"),
            Path::new("/plug"),
        )
        .unwrap();
        let mut s = PluginState::new(
            "p",
            Arc::new(caps),
            Arc::new(KvNamespace::new()),
            Some(recorder.clone()),
        )
        .unwrap();

        s.progress("call-1".into(), "half way".into()).await;
        assert_eq!(
            recorder.0.lock().unwrap().as_slice(),
            &[("call-1".to_string(), "half way".to_string())]
        );
    }

    /// A malformed method is rejected without reaching the network, and the
    /// capability check still runs first — the order matters, because the
    /// error a plugin sees should be about what it was denied, not about a
    /// typo further down.
    #[tokio::test]
    async fn the_capability_check_precedes_request_construction() {
        let mut s = state_with(vec![], vec![]);
        let err = s
            .http_request(
                "NOT A METHOD".into(),
                "https://example.com/".into(),
                vec![],
                None,
            )
            .await
            .unwrap_err();
        assert!(err.contains("net"), "{err}");
    }
}
