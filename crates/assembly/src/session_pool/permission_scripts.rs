//! 会话的有效权限模式、权限对象的解析，以及脚本绑定。

use super::*;

/// The permission mode a session will actually run under.
///
/// The caller's `options.permission_mode` is honored, but **clamped** to the
/// daemon's configured `settings.permission_mode`: a client may tighten the
/// mode for its own session and may not loosen it, since there is no
/// per-method authorization on this socket (see `server.rs`).
///
/// `settings.allow_client_permission_override` (default `false`) lets the
/// client win outright, for deployments where every client is as trusted as
/// the daemon.
///
/// With nothing configured anywhere the result is `PermissionMode::Default`
/// — i.e. *ask*.
pub(super) fn effective_permission_mode(
    settings: &Settings,
    options: Option<&SessionOptions>,
) -> base::interface::settings::PermissionMode {
    let Some(requested) = options.and_then(|o| o.permission_mode) else {
        return settings.permission_mode;
    };
    if settings.allow_client_permission_override {
        return requested;
    }
    let clamped = requested.min_permissive(settings.permission_mode);
    if clamped != requested {
        warn!(
            requested = ?requested,
            configured = ?settings.permission_mode,
            effective = ?clamped,
            "session requested a more permissive mode than this daemon allows; clamped \
             (set `allow_client_permission_override: true` to permit it)"
        );
    }
    clamped
}

/// Which `Permission` a new session gets.
///
/// Resolves a mode (`effective_permission_mode`) and builds a
/// `RuleSetPermission` for it; with nothing configured that mode is
/// `Default`, so an unlisted tool call raises a `kind:"prompt"` event and
/// waits for `session.respondToPrompt`.
///
/// `bypassPermissions` — from settings.json (daemon-wide) or
/// `options.permission_mode` (one session) — is the explicit opt-out, and it
/// returns the *literal same* shared `Arc` the pool was built with, not an
/// equivalent instance: a host that sandboxes the daemon itself gets a
/// zero-overhead path, and the `Arc::ptr_eq` assertion in the
/// unit test below keeps it that way.
///
/// Rules come from every tier, settings first — `settings.permission_rules`
/// and the `settings.local.json` overlay, each tagged with its own
/// `RuleSource` (see `RuleSetPermission::from_settings`) — then
/// `options.permission_rules` appended, so a session-scoped rule is
/// evaluated alongside the configured ones rather than replacing them.
///
/// An `ask` verdict from the rule engine becomes `PermissionOutcome::Prompt`;
/// the engine raises `AgentEvent::PermissionPrompt`, `run_turn` broadcasts it
/// to the session's subscribers, and `session.respondToPrompt` feeds the
/// answer back in.
///
/// Free function (not inlined into `create()`) specifically so this decision
/// is unit-testable without spinning up a real session.
#[allow(clippy::too_many_arguments)]
/// Turn `settings.scripts` into a prompt registry, or nothing.
///
/// `None` when no scripts are configured, which is the overwhelming case and
/// costs the session nothing — the builder then uses its own default and no
/// registry is allocated.
///
/// A binding that cannot be honored is logged and the whole set is dropped,
/// rather than half of it being applied. That is louder than it looks on
/// purpose: a script that silently never runs sends its author looking for a
/// bug in their JavaScript.
#[cfg(feature = "scripts")]
pub(super) fn bind_scripts(
    settings: &Arc<base::interface::settings::Settings>,
    project_root: &std::path::Path,
    packages: &[crate::plugins::PackageScripts],
) -> (
    Option<script_host::bindings::BoundScripts>,
    Vec<(String, String)>,
) {
    use script_host::bindings::{bind_into, bind_lenient, BindingSource, BoundScripts};

    if settings.scripts.is_empty() && packages.is_empty() {
        return (None, Vec::new());
    }

    // One value, so one ledger: the operator's scripts and the packages'
    // share the record a turn resets and `session.scripts` reads. Two would
    // have left whichever the session did not hold running on a per-turn
    // budget nothing ever reset.
    let mut bound = BoundScripts::default();
    let engine = script_host::bindings::engine();

    if let Err(e) = bind_into(
        &mut bound,
        engine.clone(),
        &operator_script_bindings(settings),
        &BindingSource::Project { root: project_root },
    ) {
        // All-or-nothing, and `bind_into` already left `bound` untouched.
        // Louder than it looks on purpose: a script that silently never runs
        // sends its author looking for a bug in their JavaScript.
        error!(error = %e, "a script binding is invalid; no scripts from settings were bound");
    }

    let mut faults = Vec::new();
    for package in packages {
        for e in bind_lenient(
            &mut bound,
            engine.clone(),
            &package.bindings,
            &BindingSource::Package {
                name: &package.name,
                root: &package.root,
            },
        ) {
            // One binding at a time, and the failure is recorded against the
            // package that shipped it: a set assembled from several packages
            // has no single author, so dropping it whole would let one bad
            // package take away every other package's contributions.
            warn!(plugin = %package.name, error = %e, "a plugin script binding was not honored");
            faults.push((package.name.clone(), e.to_string()));
        }
    }

    if bound.carriers.is_empty() {
        return (None, faults);
    }
    info!(
        scripts = bound.carriers.len(),
        "bound scripts to extension points"
    );
    (Some(bound), faults)
}

/// `settings.scripts`, less — while the project is not trusted — each binding
/// with a relative path. The project's own bindings are already withheld, so
/// these come from the global or scene tier, and a relative path there would
/// load whatever file the repository puts at it, with the authority of a
/// script inside the project.
#[cfg_attr(not(feature = "scripts"), allow(dead_code))]
pub(super) fn operator_script_bindings(
    settings: &Settings,
) -> Vec<base::interface::settings::ScriptBinding> {
    if !settings.project_layer.trust.is_untrusted() {
        return settings.scripts.clone();
    }
    settings
        .scripts
        .iter()
        .filter(|binding| {
            let absolute = binding.path.is_absolute();
            if !absolute {
                warn!(
                    path = %binding.path.display(),
                    point = %binding.point,
                    "the project is not trusted; a script binding with a relative path, which \
                     would load a file from it, is not bound until it is"
                );
            }
            absolute
        })
        .cloned()
        .collect()
}

/// Without the script carrier compiled in, a `scripts` section is refused
/// loudly rather than ignored — and so is a package that ships one.
#[cfg(not(feature = "scripts"))]
pub(super) fn bind_scripts(
    settings: &Arc<base::interface::settings::Settings>,
    _project_root: &std::path::Path,
    packages: &[crate::plugins::PackageScripts],
) -> (Option<()>, Vec<(String, String)>) {
    const NO_ENGINE: &str = "this build carries no script engine";

    if !settings.scripts.is_empty() {
        error!(
            scripts = settings.scripts.len(),
            "this build carries no script engine, so a `scripts` section cannot be honored; \
             rebuild with the `scripts` feature or remove the section"
        );
    }
    let faults = packages
        .iter()
        .flat_map(|p| {
            p.bindings
                .iter()
                .map(|b| {
                    (
                        p.name.clone(),
                        format!("{NO_ENGINE}, so `{}` is not bound", b.point),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();
    (None, faults)
}

pub(super) fn resolve_session_permission(
    host: Option<&Arc<dyn Permission>>,
    tools: &Arc<base::tool::InMemoryToolRegistry>,
    settings: &Settings,
    mode: base::interface::settings::PermissionMode,
    options: Option<&SessionOptions>,
) -> Arc<dyn Permission> {
    if let (Some(host), base::interface::settings::PermissionMode::BypassPermissions) = (host, mode)
    {
        return host.clone();
    }

    let extra = options
        .map(|o| o.permission_rules.clone())
        .unwrap_or_default();

    let rule_set: Arc<dyn Permission> = Arc::new(
        permissions::rule_set_permission::RuleSetPermission::from_settings(
            settings,
            mode.into(),
            tools.clone(),
            extra,
        ),
    );

    rule_set
}
