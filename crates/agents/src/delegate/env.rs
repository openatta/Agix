//! `DelegateEnv`: the parent session as its delegates see it, taken whole from
//! the module's `SessionContext` when the session is built and never changed
//! after.

use std::sync::{Arc, Weak};

use base::context::{EngineConfig, SessionState};
use base::interface::mcp_servers::McpServers;
use base::interface::model::Model;
use base::interface::scene::Scene;
use base::interface::skill_catalog::SkillCatalog;
use base::settings::LoadedSettings;
use runtime::agent::{EventSender, HostContracts, PendingPermissions};
use runtime::module::SessionContext;
use tokio_util::sync::CancellationToken;

pub(crate) struct DelegateEnv {
    pub(crate) host: HostContracts,
    pub(crate) parent: ParentSession,
    /// Weak: the runner's `"agent"` hooks spawn through this session's
    /// delegates, and a strong edge back would keep both alive past it.
    pub(crate) hooks: Weak<hooks::HookRunner>,
    pub(crate) scene: Arc<dyn Scene>,
    pub(crate) settings: Arc<LoadedSettings>,
    pub(crate) config: Arc<EngineConfig>,
    pub(crate) model: Arc<dyn Model>,
    /// A share of the parent's connections as the session was built; servers
    /// that finish connecting later are not in it.
    pub(crate) mcp: Option<Arc<dyn McpServers>>,
    pub(crate) skills: Arc<dyn SkillCatalog>,
    pub(crate) depth: u32,
}

pub(crate) struct ParentSession {
    pub(crate) id: String,
    pub(crate) state: Arc<SessionState>,
    pub(crate) events: EventSender,
    pub(crate) pending_permissions: PendingPermissions,
    pub(crate) ended: CancellationToken,
}

impl DelegateEnv {
    pub(crate) fn capture(cx: &SessionContext<'_>) -> Self {
        Self {
            host: cx.host.clone(),
            parent: ParentSession {
                id: cx.session_id.to_string(),
                state: cx.session_state.clone(),
                events: cx.events.clone(),
                pending_permissions: cx.pending_permissions.clone(),
                ended: cx.session_ended(),
            },
            hooks: Arc::downgrade(cx.hooks),
            scene: cx.scene.clone(),
            settings: cx.settings.clone(),
            config: Arc::new(EngineConfig::from_loaded(cx.settings)),
            model: cx.model.clone(),
            mcp: cx.mcp_for_delegates(),
            skills: cx.skill_catalog(),
            depth: cx.depth,
        }
    }

    pub(crate) fn hooks(&self) -> Option<Arc<hooks::HookRunner>> {
        self.hooks.upgrade()
    }
}
