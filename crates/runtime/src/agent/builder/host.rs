//! `HostContracts`: what the host answered for this machine and deployment,
//! resolved once by `Builder::build`, shown to every module, and inherited
//! whole by every delegate the session spawns through `Builder::inherit`.

use super::*;

use base::interface::environment::Environment;
use base::interface::exec::ExecProviders;
use base::interface::instruction_provider::{InstructionProvider, RuleProvider};
use base::interface::memory_contracts::{MemoryRetriever, RetrievalHook};
use base::interface::model_interceptor::ModelInterceptor;
use base::interface::prompt_middleware::PromptMiddleware;
use base::interface::recovery_policy::RecoveryPolicy;
use base::interface::token_counter::TokenCounter;
use base::interface::tool_middleware::ToolMiddleware;
use base::interface::tool_result::ToolResultTransformer;

#[derive(Clone)]
pub struct HostContracts {
    pub exec: ExecProviders,
    pub token_counter: Arc<dyn TokenCounter>,
    pub rules: Arc<dyn RuleProvider>,
    pub environment: Arc<dyn Environment>,
    /// The session's own; a delegate is then given one narrowed from it.
    pub permission: PermissionSet,
    pub history_store: Option<Arc<dyn history::store::HistoryStore>>,
    pub telemetry: TelemetryHandle,
    pub task_router: Option<Arc<base::provider::TaskRouter>>,
    pub scene_registry: Option<Arc<scene::SceneRegistry>>,
    pub memory_store: Arc<MemoryStore>,
    /// `None` is the engine's own retriever.
    pub memory_retriever: Option<Arc<dyn MemoryRetriever>>,
    pub retrieval_hooks: Vec<Arc<dyn RetrievalHook>>,
    pub instruction_provider: Arc<dyn InstructionProvider>,
    pub plugins: Vec<Arc<dyn crate::plugin::Plugin>>,
    /// Already narrowed by the scene and checked for order.
    pub modules: Vec<Arc<dyn crate::module::Module>>,
    pub host_layers: HostLayers,
}

/// What the host wraps around a session's calls, before the plugins' layers
/// join them.
#[derive(Clone, Default)]
pub struct HostLayers {
    pub tool_middleware: Vec<Arc<dyn ToolMiddleware>>,
    pub prompt_middleware: Vec<Arc<dyn PromptMiddleware>>,
    pub result_transformers: Vec<Arc<dyn ToolResultTransformer>>,
    pub model_interceptors: Vec<Arc<dyn ModelInterceptor>>,
    /// `None` is the engine's default for the session's own settings.
    pub recovery_policy: Option<Arc<dyn RecoveryPolicy>>,
    pub compactor: Option<Arc<dyn Compactor>>,
}

impl Builder {
    pub(super) fn host_layers(&self) -> HostLayers {
        HostLayers {
            tool_middleware: self.tool_middleware.clone(),
            prompt_middleware: self.prompt_middleware.clone(),
            result_transformers: self.result_transformers.clone(),
            model_interceptors: self.model_interceptors.clone(),
            recovery_policy: self.recovery_policy.clone(),
            compactor: self.compactor.clone(),
        }
    }

    /// Build this session on `host`, as the session that resolved it was.
    ///
    /// Call it before the delegate's own setters: whatever of these the
    /// delegate decides for itself — its permission, narrowed from the
    /// parent's — is set after and replaces what this set.
    pub fn inherit(mut self, host: HostContracts) -> Self {
        let HostContracts {
            exec,
            token_counter,
            rules,
            environment,
            permission,
            history_store,
            telemetry,
            task_router,
            scene_registry,
            memory_store,
            memory_retriever,
            retrieval_hooks,
            instruction_provider,
            plugins,
            modules,
            host_layers:
                HostLayers {
                    tool_middleware,
                    prompt_middleware,
                    result_transformers,
                    model_interceptors,
                    recovery_policy,
                    compactor,
                },
        } = host;
        self.exec = Some(exec);
        self.token_counter = Some(token_counter);
        self.rule_provider = Some(rules);
        self.environment = Some(environment);
        self.permission = Some(permission);
        self.history_store = history_store;
        self.telemetry_handle_override = Some(telemetry);
        self.task_router = task_router;
        self.scene_registry = scene_registry;
        self.memory_store = Some(memory_store);
        self.memory_retriever = memory_retriever;
        self.retrieval_hooks = retrieval_hooks;
        self.instruction_provider = Some(instruction_provider);
        self.plugins = plugins;
        self.modules = modules;
        self.tool_middleware = tool_middleware;
        self.prompt_middleware = prompt_middleware;
        self.result_transformers = result_transformers;
        self.model_interceptors = model_interceptors;
        self.recovery_policy = recovery_policy;
        self.compactor = compactor;
        self
    }
}
