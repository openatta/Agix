//! A network that is not there.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::interface::exec::{ExecError, HttpRequest, HttpResponse, Network, Origin};

/// Answers only what it was given, and refuses everything else.
///
/// Refusing rather than returning an empty 200: a tool that got an empty page
/// for a url nobody scripted would report that the page was empty, which is a
/// different and much harder failure to notice than "nothing answers here".
#[derive(Clone, Default)]
pub struct OfflineNetwork {
    fixtures: Arc<Mutex<BTreeMap<String, HttpResponse>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl OfflineNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(self, url: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
        self.fixtures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                url.into(),
                HttpResponse {
                    status,
                    headers: BTreeMap::new(),
                    body: body.into().into_bytes(),
                },
            );
        self
    }

    /// Every url that was asked for, in order.
    pub fn asked(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl Network for OfflineNetwork {
    async fn send(&self, req: HttpRequest, _origin: Origin) -> Result<HttpResponse, ExecError> {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(req.url.clone());
        self.fixtures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&req.url)
            .cloned()
            .ok_or_else(|| ExecError::unavailable(format!("offline: {}", req.url)))
    }

    fn permits(&self, _host: &str, _origin: Origin) -> bool {
        // Permitted and unreachable are different answers. Saying "not
        // permitted" here would make a caller report a policy refusal for
        // something no policy refused.
        true
    }
}
