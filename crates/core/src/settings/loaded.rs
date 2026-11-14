//! What loading the settings tiers produced: the merged [`Settings`], where
//! this scene's data lives, every permission rule with the tier it came from,
//! the deployment's managed layer, and what was decided about the project
//! tier. A settings file can set none of these but the values.
//!
//! [`LoadedSettings::load`] is the single canonical settings.json loader —
//! `daemon` and any other embedder call it rather than parsing settings.json
//! themselves. [`LoadedSettings::load_from`] takes the layers from any
//! [`ConfigSource`](crate::interface::config_source::ConfigSource) and runs the
//! same merge.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use crate::interface::config_source::{ConfigSource, FileTiers, Tier};
use crate::managed_settings::ManagedSettings;
use crate::permission::RuleBehavior;
use crate::project_trust::{ProjectLayer, ProjectTrust};

use super::{
    merge_fitting, merge_json_values, read_permission_rules, unknown_settings_keys, IgnoredValue,
    PathSettings, PermissionMode, PermissionRuleEntry, Settings, Workspace,
};

/// A permission rule as loaded: the rule, the tier whose file it came from,
/// and whether that file was the tier's `settings.local.json`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LoadedRule {
    pub rule: PermissionRuleEntry,
    pub tier: Tier,
    pub machine_local: bool,
}

impl LoadedRule {
    pub fn new(rule: PermissionRuleEntry, tier: Tier, machine_local: bool) -> Self {
        Self {
            rule,
            tier,
            machine_local,
        }
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LoadedSettings {
    pub values: Settings,
    pub paths: PathSettings,
    /// Every tier's `permission_rules`, lowest tier first. They are
    /// concatenated, never overridden: a project's rules are added to the
    /// global and scene ones.
    pub rules: Vec<LoadedRule>,
    /// The deployment's layer, above every other — see
    /// [`crate::managed_settings`].
    pub managed: Option<ManagedSettings>,
    /// For each of `values.mcp_servers`, the directory of the tier its
    /// `command` was declared in: the global or scene configuration
    /// directory, or the project root. A stdio server starts there.
    pub mcp_server_dirs: HashMap<String, PathBuf>,
    /// The values the loader left out because they do not fit their setting.
    pub ignored: Vec<IgnoredValue>,
    project_layer: ProjectLayer,
}

impl LoadedSettings {
    /// Settings that were not loaded from any tier: no rules, no managed
    /// layer, and no project tier of their own.
    pub fn new(values: Settings, paths: PathSettings) -> Self {
        Self {
            values,
            paths,
            rules: Vec::new(),
            managed: None,
            mcp_server_dirs: HashMap::new(),
            ignored: Vec::new(),
            project_layer: ProjectLayer::default(),
        }
    }

    /// Load settings from the three tiers on disk. Priority (low → high):
    /// `global_dir/settings.json` (shared by every scene) →
    /// `scene_dir/settings.json` → `local_dir/settings.json` (project). Each
    /// tier is immediately followed by its own gitignored
    /// `settings.local.json` overlay, which outranks the `settings.json`
    /// beside it. An embedder whose configuration does not live on this
    /// machine's disk calls [`load_from`](Self::load_from) with its own source
    /// and gets the same merge.
    ///
    /// Merging is a **generic recursive JSON merge** (later layer's object
    /// keys override/extend the earlier layer's, non-object values fully
    /// replace) applied directly to the JSON representation, not a
    /// hand-written field-by-field merge — this is why adding a new field to
    /// `Settings` needs no changes here. A file's `paths` is dropped before
    /// merging: a settings.json file cannot override where its own layers
    /// live, that's decided by the `global_dir`/`scene_dir`/`local_dir`/
    /// `scope` arguments alone.
    ///
    /// The project tier is the one exception to "every layer merges the same
    /// way": unless the global tier's `project_trust` trusts the project root
    /// (`local_dir`'s parent), what in it could loosen anything is held back
    /// before it merges — see [`crate::project_trust`]. `project_trust`
    /// itself is read from the global tier only. The decision, and what it
    /// held back, is [`trust`](Self::trust) and
    /// [`requires_trust`](Self::requires_trust).
    ///
    /// **Never fails** — a layer that doesn't exist is skipped; a layer that
    /// fails to parse is skipped with a `tracing::warn!` (path + error)
    /// rather than aborting startup or silently losing the problem. A value
    /// that parses but does not fit its setting (a string where an object
    /// was expected) is left out by itself, with a warning and an entry in
    /// `ignored`; the rest of its layer and every other layer apply.
    ///
    /// `scope` identifies which product instance `scene_dir` belongs to (see
    /// `base::paths::ConfigPaths`) — no default at this layer; callers decide.
    ///
    /// `default_model` seeds `model.model_name` **before** any layer is
    /// merged in — lower priority than every settings.json layer. This lets
    /// a caller's CLI `--model` flag act as a fallback rather than a hard
    /// override.
    pub fn load(
        global_dir: PathBuf,
        scene_dir: PathBuf,
        local_dir: PathBuf,
        scope: &str,
        default_model: &str,
    ) -> Self {
        let source = FileTiers::new(global_dir.clone(), scene_dir.clone(), local_dir.clone());
        Self::load_from(
            &source,
            global_dir,
            scene_dir,
            local_dir,
            scope,
            default_model,
        )
    }

    /// [`load`](Self::load) for a session without a project: no project tier,
    /// and a [`Workspace::Scratch`] in the state root's scratchpad until the
    /// session is given a directory of its own
    /// ([`crate::paths::session_scratch_dir`]).
    pub fn load_without_project(
        global_dir: PathBuf,
        scene_dir: PathBuf,
        scope: &str,
        default_model: &str,
    ) -> Self {
        let scratch = global_dir.join(crate::paths::STATE_ROOT_SCRATCHPAD);
        let mut loaded = Self::load(
            global_dir.clone(),
            scene_dir,
            global_dir,
            scope,
            default_model,
        );
        loaded.paths.workspace = Workspace::Scratch(scratch);
        loaded
    }

    /// [`load`](Self::load) with the layers coming from wherever `source` says
    /// instead of from the six files on disk.
    ///
    /// The merge is identical — that is the point of the split. `source`
    /// decides only which layers exist, in what order, and what JSON is in
    /// them; everything after that happens here and is the same for every
    /// source.
    ///
    /// The directory arguments are not where configuration is read from —
    /// they are where this scene's *data* lives, which `paths` reports and a
    /// settings file is never allowed to override — and they say which
    /// project the project tier is, and whether it is one of its own.
    pub fn load_from(
        source: &dyn ConfigSource,
        global_dir: PathBuf,
        scene_dir: PathBuf,
        local_dir: PathBuf,
        scope: &str,
        default_model: &str,
    ) -> Self {
        use crate::project_trust::ProjectTrustSettings;

        let base = Settings::defaults_for(default_model);
        let merged_json = serde_json::to_value(&base).unwrap_or_else(|_| serde_json::json!({}));

        let layers = source.layers();
        let own_project =
            crate::project_trust::is_own_project_dir(&local_dir, &global_dir, &scene_dir);
        let tiers = TierView {
            own_project,
            scene_is_global: crate::project_trust::same_dir(&scene_dir, &global_dir),
            project_is_global: !own_project
                && crate::project_trust::same_dir(&local_dir, &global_dir),
        };

        let project_trust = ProjectTrustSettings::from_layers(
            layers
                .iter()
                .filter(|l| tiers.is_global(l.tier))
                .map(|l| &l.value),
        );
        let (trust, matched) = if own_project {
            project_trust.decide(local_dir.parent().unwrap_or(&local_dir))
        } else {
            (ProjectTrust::NoProject, None)
        };
        let withhold = trust.is_untrusted();
        let mut fold = LayerFold {
            tiers,
            withhold,
            global_dir: &global_dir,
            scene_dir: &scene_dir,
            local_dir: &local_dir,
            merged_json,
            loaded_rules: Vec::new(),
            project_layer: ProjectLayer {
                trust,
                matched,
                ..Default::default()
            },
            ignored: Vec::new(),
            managed: None,
            mcp_server_dirs: HashMap::new(),
            mcp_secret_vars: Vec::new(),
        };
        for layer in layers {
            fold.apply(layer);
        }
        let LayerFold {
            merged_json,
            loaded_rules,
            mut project_layer,
            ignored,
            managed,
            mcp_server_dirs,
            mcp_secret_vars,
            ..
        } = fold;
        let mut seen = std::collections::HashSet::new();
        project_layer
            .requires_trust
            .retain(|key| seen.insert(key.clone()));

        // Every merge step above was checked to deserialize.
        let mut values: Settings = serde_json::from_value(merged_json).unwrap_or(base);
        values.project_trust = project_trust;
        guard_loaded_values(&mut values, &mcp_secret_vars);

        let paths = PathSettings {
            user_data_dir: scene_dir,
            global_data_dir: global_dir,
            workspace: Workspace::project_of(&local_dir),
            local_data_dir: local_dir,
            scope: scope.to_string(),
        };
        let loaded = Self {
            values,
            paths,
            rules: loaded_rules,
            managed: None,
            mcp_server_dirs,
            ignored,
            project_layer,
        };
        if managed.is_some() {
            loaded.with_managed(managed)
        } else {
            loaded
        }
    }

    /// Put the deployment's layer on top of these settings, replacing any
    /// managed layer they already carry. With bypass disabled, a
    /// `bypassPermissions` the lower tiers configured becomes `default`.
    pub fn with_managed(mut self, managed: Option<ManagedSettings>) -> Self {
        if let Some(m) = &managed {
            if m.disable_bypass_permissions_mode
                && self.values.permission_mode == PermissionMode::BypassPermissions
            {
                tracing::warn!(origin = %m.origin, "managed settings disable bypassPermissions; permission_mode falls back to default");
                self.values.permission_mode = PermissionMode::Default;
            }
        }
        self.managed = managed;
        self
    }

    /// These settings as if the loader had decided `layer` about the project
    /// tier.
    pub fn with_project_layer(mut self, layer: ProjectLayer) -> Self {
        self.project_layer = layer;
        self
    }

    /// Whether the project tier is in effect as written.
    pub fn trust(&self) -> ProjectTrust {
        self.project_layer.trust
    }

    /// The `project_trust.trusted` pattern that matched the project root;
    /// `None` when the default decided, or there is no project tier.
    pub fn trust_pattern(&self) -> Option<&str> {
        self.project_layer.matched.as_deref()
    }

    /// The project-tier settings that take effect only while the project is
    /// trusted, as dotted keys (`hooks_config`, `mcp_servers.github`,
    /// `permission_rules.allow(Bash)`). Withheld while it is not.
    pub fn requires_trust(&self) -> &[String] {
        &self.project_layer.requires_trust
    }

    /// The project tier's own `mcp_servers` entries, both of its files
    /// merged — whether or not they were withheld.
    pub fn project_mcp_servers(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.project_layer.mcp_servers
    }

    /// Whether the deployment has taken `bypassPermissions` away.
    pub fn bypass_disabled(&self) -> bool {
        self.managed
            .as_ref()
            .is_some_and(|m| m.disable_bypass_permissions_mode)
    }
}

/// Two tiers sharing a directory are read once, at the higher tier: a project
/// rooted at `$HOME` reads `~/.agix` as its project tier, and those files are
/// still the user's global settings.
struct TierView {
    own_project: bool,
    scene_is_global: bool,
    project_is_global: bool,
}

impl TierView {
    fn is_global(&self, tier: Tier) -> bool {
        match tier {
            Tier::Global => true,
            Tier::Scene => self.scene_is_global,
            Tier::Project => self.project_is_global,
            Tier::Managed => false,
        }
    }

    fn is_own_project(&self, tier: Tier) -> bool {
        tier == Tier::Project && self.own_project
    }
}

struct LayerFold<'a> {
    tiers: TierView,
    withhold: bool,
    global_dir: &'a std::path::Path,
    scene_dir: &'a std::path::Path,
    local_dir: &'a std::path::Path,
    merged_json: serde_json::Value,
    loaded_rules: Vec<LoadedRule>,
    project_layer: ProjectLayer,
    ignored: Vec<IgnoredValue>,
    managed: Option<ManagedSettings>,
    mcp_server_dirs: HashMap<String, PathBuf>,
    mcp_secret_vars: Vec<String>,
}

impl LayerFold<'_> {
    fn apply(&mut self, layer: crate::interface::config_source::ConfigLayer) {
        use crate::project_trust::gate_layer;
        if layer.tier == Tier::Managed {
            let this = ManagedSettings::from_value(layer.origin, layer.value);
            self.managed = Some(match self.managed.take() {
                Some(earlier) => earlier.followed_by(this),
                None => this,
            });
            return;
        }
        for key in unknown_settings_keys(&layer.value) {
            tracing::warn!(layer = layer.tier.name(), origin = %layer.origin, key, "settings key not recognised, ignoring it");
        }
        let is_global = self.tiers.is_global(layer.tier);
        let is_own_project = self.tiers.is_own_project(layer.tier);
        let mut layer_json = layer.value;
        if let Some(obj) = layer_json.as_object_mut() {
            obj.remove("paths");
            if !is_global && obj.remove("project_trust").is_some() {
                tracing::warn!(layer = layer.tier.name(), origin = %layer.origin, "project_trust is read from the global tier only, ignoring it here");
            }
            if is_own_project {
                for (name, server) in obj
                    .get("mcp_servers")
                    .and_then(|v| v.as_object())
                    .into_iter()
                    .flatten()
                {
                    let entry = self
                        .project_layer
                        .mcp_servers
                        .entry(name.clone())
                        .or_insert(serde_json::Value::Null);
                    merge_json_values(entry, server.clone());
                }
                let gated = gate_layer(obj, &self.merged_json, self.withhold);
                if self.withhold && !gated.is_empty() {
                    tracing::warn!(origin = %layer.origin, withheld = ?gated, "project is not trusted; settings only a trusted project may set are ignored");
                }
                self.project_layer.requires_trust.extend(gated);
            }
            if let Some(rules) = obj.remove("permission_rules") {
                self.take_permission_rules(&layer.origin, layer.tier, layer.machine_local, rules);
            }
        }
        self.note_mcp_servers(layer.tier, &layer_json);
        for (key, error) in merge_fitting(&mut self.merged_json, layer_json) {
            tracing::warn!(layer = layer.tier.name(), origin = %layer.origin, key, error, "settings value does not fit its setting, ignoring it");
            self.ignored.push(IgnoredValue {
                tier: layer.tier.name(),
                origin: layer.origin.clone(),
                key,
                error,
            });
        }
    }

    fn take_permission_rules(
        &mut self,
        origin: &str,
        tier: Tier,
        machine_local: bool,
        rules: serde_json::Value,
    ) {
        let (rules, unread) = read_permission_rules(rules);
        for (key, error) in unread {
            tracing::warn!(layer = tier.name(), origin = %origin, key, error, "permission rule does not read, ignoring it");
            self.ignored.push(IgnoredValue {
                tier: tier.name(),
                origin: origin.to_string(),
                key,
                error,
            });
        }
        for rule in rules {
            if self.tiers.is_own_project(tier) && rule.action == RuleBehavior::Allow {
                self.project_layer
                    .requires_trust
                    .push(format!("permission_rules.allow({})", rule.tool));
                if self.withhold {
                    continue;
                }
            }
            self.loaded_rules
                .push(LoadedRule::new(rule, tier, machine_local));
        }
    }

    fn note_mcp_servers(&mut self, tier: Tier, layer_json: &serde_json::Value) {
        let declared_in = if self.tiers.is_global(tier) {
            self.global_dir
        } else if self.tiers.is_own_project(tier) {
            self.local_dir.parent().unwrap_or(self.local_dir)
        } else {
            self.scene_dir
        };
        for (name, server) in layer_json
            .get("mcp_servers")
            .and_then(|v| v.as_object())
            .into_iter()
            .flatten()
        {
            if server.get("command").is_some() || !self.mcp_server_dirs.contains_key(name) {
                self.mcp_server_dirs
                    .insert(name.clone(), declared_in.to_path_buf());
            }
            // A project's own servers read nothing from this process's
            // environment (`confined_to_own_env`), so the names they
            // write are not secrets of the daemon's.
            if !self.tiers.is_own_project(tier) {
                self.mcp_secret_vars.extend(
                    crate::interface::exec::env::variables_an_mcp_server_reads(server),
                );
            }
        }
    }
}

/// Before any session these settings start runs a child process: a provider
/// key is not read until a client is built, an MCP secret not until its server
/// connects, and a hook or `Bash` can run before that.
fn guard_loaded_values(values: &mut Settings, mcp_secret_vars: &[String]) {
    for name in values
        .providers
        .values()
        .filter_map(|p| p.api_key_env.as_deref())
        .chain(mcp_secret_vars.iter().map(String::as_str))
    {
        crate::interface::exec::env::hold_back_credential(name);
    }
    if let Err(reason) = values.validate() {
        tracing::warn!("Settings validation: {reason}");
        values.permission_mode = PermissionMode::default();
    }
}
