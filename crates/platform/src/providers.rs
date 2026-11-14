//! The set of this machine's providers a `ToolContext` reaches the machine
//! through, and the contexts built on it.

use std::path::PathBuf;
use std::sync::Arc;

use base::interface::exec::{ExecProviders, SubprocessEnv};
use base::tool::ToolContext;

use crate::{GroupLedger, LocalFileSystem, LocalProcess, PlatformSandbox};

pub fn local_providers() -> ExecProviders {
    local_providers_with(SubprocessEnv::default())
}

/// Child processes start from `env`.
pub fn local_providers_with(env: SubprocessEnv) -> ExecProviders {
    ExecProviders {
        process: Arc::new(LocalProcess::new(env)),
        filesystem: Arc::new(LocalFileSystem),
        #[cfg(feature = "http")]
        network: Arc::new(crate::LocalNetwork::default()),
        #[cfg(not(feature = "http"))]
        network: Arc::new(NoNetwork),
        sandbox: Arc::new(PlatformSandbox),
    }
}

/// [`local_providers_with`], with every process group a command starts
/// recorded in `ledger`.
pub fn local_providers_with_ledger(env: SubprocessEnv, ledger: Arc<GroupLedger>) -> ExecProviders {
    ExecProviders {
        process: Arc::new(LocalProcess::with_ledger(env, ledger)),
        ..local_providers()
    }
}

pub fn tool_context_for_test(cwd: PathBuf) -> ToolContext {
    ToolContext::for_test_with(cwd, local_providers())
}

/// The network of a `platform` built without `http`: every request is refused
/// as a provider that is not there, never sent some other way.
#[cfg(not(feature = "http"))]
struct NoNetwork;

#[cfg(not(feature = "http"))]
#[async_trait::async_trait]
impl base::interface::exec::Network for NoNetwork {
    async fn send(
        &self,
        _: base::interface::exec::HttpRequest,
        _: base::interface::exec::Origin,
    ) -> Result<base::interface::exec::HttpResponse, base::interface::exec::ExecError> {
        Err(base::interface::exec::ExecError::Unavailable(
            "this build of `platform` has no HTTP client (feature `http`)".into(),
        ))
    }
    fn permits(&self, _: &str, _: base::interface::exec::Origin) -> bool {
        false
    }
}
