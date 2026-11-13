//! 一个 `(project, scene)` 对解析出的一切：合并后的设置、对话模型与按任务的路由、
//! skill/agent 类型/命令目录，以及它们来自哪些设置文件。

use base::settings::{LoadedSettings, Settings};
use model::adapter::AnthropicModel;
use model::client::AnthropicClient;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;

/// What a per-session settings entry is keyed by: the project it belongs to
/// (`None` = the global no-project tier) and the scene it runs under.
///
/// Both halves matter. The scene contributes a real settings layer
/// (`~/.agix/scenes/<scene>/settings.json`, plus the skills, agents and
/// plugins beside it), so one project under two scenes is two different
/// resolutions, not one.
pub(in crate::session_pool) type ProjectSceneKey = (Option<PathBuf>, String);

/// Which pair a binding was built for, and that pair's epoch in the
/// [`super::BindingCache`] when the build began. The cache bumps a pair's
/// epoch on every invalidation, so a binding — and every session built on
/// it — is current exactly while the two still agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session_pool) struct BindingStamp {
    pub(in crate::session_pool) key: ProjectSceneKey,
    pub(in crate::session_pool) epoch: u64,
}

/// Whether what `key` resolves to was merged from `tier`. The global tier is
/// under every pair; a scene or project tier only under its own.
pub(in crate::session_pool) fn tier_covers(
    tier: &crate::watch_hub::Tier,
    key: &ProjectSceneKey,
) -> bool {
    use crate::watch_hub::Tier;
    match tier {
        Tier::Global => true,
        Tier::Scene(scene) => &key.1 == scene,
        Tier::Project(root) => key.0.as_deref() == Some(root.as_path()),
    }
}

/// Everything a session reads that the pair `(project, scene)` decides: the
/// merged settings, and the skills, agent types and commands that live in the
/// directories those settings name.
///
/// One per pair, shared by every session on it. They are here together
/// because they are the same fact stated four times: a binding is built from
/// one `Settings`, and rebuilding it is how any of them changes.
pub(in crate::session_pool) struct ProjectBinding {
    pub(in crate::session_pool) settings: Arc<LoadedSettings>,
    /// The conversation's model for a session on this pair, and the
    /// per-task-type routing behind it. `providers` and `task_models` are
    /// settings fields like any other, so they are decided by the pair: a
    /// project that configures its own provider is served by it.
    pub(in crate::session_pool) model: Arc<dyn base::interface::model::Model>,
    pub(in crate::session_pool) task_router: Option<Arc<base::provider::TaskRouter>>,
    /// What building `task_router` had to say — reported by `config.reload`
    /// and `config.setProvider` for the pair they targeted.
    pub(in crate::session_pool) routing: RoutingReport,
    pub(in crate::session_pool) modules: crate::modules::ProjectModules,
    pub(in crate::session_pool) commands: Arc<runtime::commands::CommandRegistry>,
    /// Subscriptions to the settings files this binding was merged from.
    /// Held, not read: dropping them is how a binding that gets invalidated
    /// releases its share of the watch (`WatchHub` refcounts, so the tiers
    /// several projects have in common are watched once).
    pub(super) _watch: Vec<crate::watch_hub::WatchToken>,
    /// The settings files, skill and agent-type directories of this
    /// binding's tiers that did not exist when it was built, so are not
    /// watched — see [`ProjectBinding::appeared`].
    pub(super) unwatched_settings: Vec<(crate::watch_hub::Tier, PathBuf)>,
    /// Every session built on this binding keeps a copy, and rebuilds before
    /// its next turn once the cache no longer considers it current.
    pub(in crate::session_pool) stamp: BindingStamp,
}

impl ProjectBinding {
    pub(in crate::session_pool) fn build(
        stamp: BindingStamp,
        settings: Arc<LoadedSettings>,
        plugins: &crate::plugins::Contributions,
        watch: Vec<crate::watch_hub::WatchToken>,
        client: &Arc<dyn AnthropicClient>,
        last_good_router: Option<Arc<base::provider::TaskRouter>>,
        credentials: &Arc<dyn base::interface::credentials::CredentialSource>,
    ) -> Self {
        let (task_router, routing) = resolve_router(&settings, last_good_router, credentials);
        // `task_models.main` is what points the conversation itself at a
        // configured provider; with none configured this is the plain
        // Anthropic client the daemon was started with.
        let model: Arc<dyn base::interface::model::Model> = match task_router.as_ref() {
            Some(router) => router.model_for("main"),
            None => Arc::new(AnthropicModel::new(client.clone())),
        };
        let modules = crate::modules::ProjectModules::build(&settings, plugins);
        let commands = modules.commands();
        Self {
            settings,
            model,
            task_router,
            routing,
            modules,
            commands,
            _watch: watch,
            unwatched_settings: Vec::new(),
            stamp,
        }
    }

    /// The tier of a settings file, skill or agent-type directory that has
    /// been created since this binding was built. Nothing watched it — a
    /// missing path is not subscribed, see `BindingCache::watch_tiers_of` — so
    /// this is the only way its first contents (a rule `permit_always` wrote
    /// to a new `settings.local.json`, a project's first skill or agent type)
    /// reach the sessions without a `config.reload`.
    pub(in crate::session_pool) fn appeared(&self) -> Option<&crate::watch_hub::Tier> {
        self.unwatched_settings
            .iter()
            .find(|(_, path)| path.exists())
            .map(|(tier, _)| tier)
    }

    /// The client for provider `id`, when the settings configure one that
    /// built.
    pub(in crate::session_pool) fn provider_client(
        &self,
        id: &str,
    ) -> Option<Arc<dyn base::interface::model::Model>> {
        self.task_router.as_ref()?.provider(id)
    }

    /// Whether a session on this pair may talk to `provider` with `model`.
    ///
    /// A model is checked against the `models` allow-list of the provider it
    /// would be sent to — the one named, else the one the conversation is
    /// routed to. An empty list, or no provider at all, accepts any name: the
    /// daemon has no catalog to check against.
    pub(in crate::session_pool) fn check_model_choice(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), String> {
        if let Some(id) = provider {
            if self.provider_client(id).is_none() {
                return Err(format!(
                    "provider `{id}` is not configured for this session's project"
                ));
            }
        }
        let Some(model) = model else {
            return Ok(());
        };
        let routed = provider
            .map(str::to_string)
            .or_else(|| {
                self.task_router
                    .as_ref()
                    .and_then(|r| r.provider_id_for("main").map(str::to_string))
            })
            .or_else(|| self.settings.default_provider.clone());
        let Some(id) = routed else {
            return Ok(());
        };
        match self.settings.providers.get(&id) {
            Some(config)
                if !config.models.is_empty() && !config.models.iter().any(|m| m == model) =>
            {
                Err(format!(
                    "model `{model}` is not one of provider `{id}`'s models"
                ))
            }
            _ => Ok(()),
        }
    }

    pub(in crate::session_pool) fn reload_skills(&self) {
        self.modules.reload_skills(&self.settings);
    }

    /// Re-merge what the plugin set now contributes, in place: every session
    /// on the pair shares these catalogs, and `commands` is a view onto the
    /// skill one.
    pub(in crate::session_pool) fn refresh_plugins(&self, plugins: &crate::plugins::Contributions) {
        self.modules.refresh_plugins(&self.settings, plugins);
    }

    pub(in crate::session_pool) fn refresh_agent_types(
        &self,
        plugin_agent_types: &[base::agent_type::AgentTypeDefinition],
    ) {
        self.modules
            .refresh_agent_types(&self.settings, plugin_agent_types);
    }
}

/// How re-resolving a pair's routing went, for `config.setProvider` and
/// `config.reload`.
///
/// Two genuinely different checks, kept in separate fields on purpose:
/// `warnings`/`error` are `base::provider::resolve_task_models`'s own
/// soft-fallback outcome — pure config-shape validation (does every
/// `task_models` entry resolve to a provider that exists, etc.), unaware of
/// `api_type` or whether a real client could actually be built. A config can
/// resolve cleanly (`error: None`) yet still fail to produce a live
/// `TaskRouter` (`router_rebuilt: false`, `router_error: Some(..)`) — e.g.
/// `api_type: openai_compatible`, which the schema accepts but has no
/// runtime implementation. Collapsing these into one `ok` flag would be a
/// breaking change to what `routing.ok` has always meant to callers (see
/// `set_provider_writes_project_settings_and_doctor_sees_it`'s test, which
/// deliberately uses `openai_compatible` and asserts `routing.ok == true`).
#[derive(Debug, Default, Clone)]
pub(in crate::session_pool) struct RoutingReport {
    pub(in crate::session_pool) warnings: Vec<String>,
    pub(in crate::session_pool) error: Option<String>,
    pub(in crate::session_pool) router_rebuilt: bool,
    pub(in crate::session_pool) router_error: Option<String>,
}

/// Build the router a settings tier's `providers`/`task_models` describe.
///
/// `last_good` is what this pair was already being served by. A config that
/// no longer resolves, or that resolves but cannot produce a live client
/// (`api_type: openai_compatible` is accepted by the schema and has no
/// runtime implementation), keeps it rather than tearing working routing down
/// over a bad edit — the failure is reported, not acted on.
fn resolve_router(
    settings: &Settings,
    last_good: Option<Arc<base::provider::TaskRouter>>,
    credentials: &Arc<dyn base::interface::credentials::CredentialSource>,
) -> (Option<Arc<base::provider::TaskRouter>>, RoutingReport) {
    let mut report = RoutingReport::default();
    if settings.providers.is_empty() {
        // No providers configured (or the last one was just deleted) —
        // routing reverts to "no router", same as if multi-provider config
        // was never touched.
        return (None, report);
    }
    match base::provider::resolve_task_models(
        &settings.providers,
        settings.default_provider.as_deref(),
        &settings.task_models,
    ) {
        Ok((resolved, warnings)) => {
            report.warnings = warnings;
            // `resolve_task_models` succeeding guarantees `default_provider`
            // is `Some` and non-empty (one of its own hard-error checks).
            let default_provider = settings
                .default_provider
                .as_deref()
                .expect("resolve_task_models already validated default_provider is set");
            match crate::model_router::build_task_router(
                &settings.providers,
                default_provider,
                resolved,
                credentials,
            ) {
                Ok(router) => {
                    report.router_rebuilt = true;
                    (Some(Arc::new(router)), report)
                }
                Err(e) => {
                    // Deliberately `router_error`, not `error` —
                    // `resolve_task_models` itself succeeded (the warnings
                    // above already reflect that), only the build step
                    // failed; see `RoutingReport`'s doc comment.
                    warn!(error = %e, "task router build failed; keeping the previous router");
                    report.router_error = Some(e);
                    (last_good, report)
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "provider config no longer resolves; keeping the previous router");
            report.error = Some(e);
            (last_good, report)
        }
    }
}
