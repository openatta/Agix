//! Shared registry of teams created via `TeamCreate`.
//!
//! `TeamCreateTool`, `TeamListTool`, and `TeamDeleteTool` are three separate
//! `Tool` instances, each wrapping its own `DefaultCoordinator` — team state
//! doesn't live inside any single one of them, it has to live somewhere all
//! three can reach. This is that somewhere: an `Arc`-shared, `RwLock`-guarded
//! map, constructed once per session (`runtime::agent::Builder::build()`) and
//! handed to all three tools' coordinators via `DefaultCoordinator::with_registry`.

use crate::coordinator::TeammateLifecycle;
use base::interface::settings::PermissionMode;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

/// A single team member's recorded state. `TeamCreate`'s batch mode runs
/// every member to completion synchronously before returning, so for those,
/// this is always `Completed` by the time it's visible. Persistent members
/// (`Agent` tool's `team_name`+`name` mode) transition through
/// `Idle`/`Active`/`Shutdown` for real, over their actual lifetime — for
/// them, `idle_since_secs` is how long (epoch seconds) the member has been
/// sitting idle, so `TeamList` can show it without needing any push
/// notification back to the lead (polling this instead of pushing was the
/// deliberate choice). `None` while `Active`, or
/// for batch members (which don't track this at all).
#[derive(Debug, Clone)]
pub struct TeamMemberInfo {
    pub label: String,
    pub lifecycle: TeammateLifecycle,
    pub idle_since_secs: Option<u64>,
}

/// A team's recorded state: enough for `TeamList` to answer "what teams
/// exist and what shape are they" and for `TeamDelete` to find what it's
/// cleaning up.
#[derive(Debug, Clone)]
pub struct TeamInfo {
    pub team_id: String,
    pub name: String,
    pub created_at_secs: u64,
    pub team_dir: PathBuf,
    pub members: Vec<TeamMemberInfo>,
    /// The permission grant this team's persistent members were spawned
    /// under — established once, by whichever call spawns the *first*
    /// member under a given team name, and reused by every later call under
    /// the same name that doesn't explicitly override it ("one team, one
    /// authorization", not a per-call decision). `None` for teams that only
    /// ever went through `TeamCreate`'s batch mode, which doesn't use this
    /// field (batch mode's `permission_mode` is scoped to that one call, not
    /// stored here — see `OrchestrateRequest::permission_mode`).
    pub permission_mode: Option<PermissionMode>,
}

/// Thread-safe registry, keyed by the team `name` the model chose (the same
/// string it passes to both `TeamCreate.name` and `TeamDelete.name`) — not
/// the internally-generated `team_id`, since callers only ever know the name
/// they picked.
#[derive(Default)]
pub struct TeamRegistry {
    teams: RwLock<HashMap<String, TeamInfo>>,
}

impl TeamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a team's recorded state. A second `TeamCreate` call
    /// reusing the same `name` overwrites the earlier entry — the model
    /// chose the name, so a collision is the model's own bookkeeping to
    /// avoid, not something this registry arbitrates.
    pub fn upsert(&self, info: TeamInfo) {
        self.teams
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(info.name.clone(), info);
    }

    /// All currently-registered teams, sorted by name for stable output.
    pub fn list(&self) -> Vec<TeamInfo> {
        let mut teams: Vec<TeamInfo> = self
            .teams
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        teams.sort_by(|a, b| a.name.cmp(&b.name));
        teams
    }

    /// Remove and return a team's state by name, if it exists.
    pub fn remove(&self, name: &str) -> Option<TeamInfo> {
        self.teams
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
    }

    /// Update (or add) one member's lifecycle (and, for persistent members,
    /// its idle-since timestamp) within `team_name`. Used by the
    /// persistent-member path (`Agent` tool's `team_name`+`name` mode,
    /// `runtime::agent_tool`) to keep `TeamList` accurate as a member goes
    /// Active/Idle/Shutdown across many turns — unlike `TeamCreate`'s batch
    /// path, which only ever writes a team's final state once via
    /// `upsert()` after every member has already finished. `idle_since_secs`
    /// should be `None` when `lifecycle` is `Active` (nothing to show) and
    /// `Some(now)` when it just went `Idle`.
    ///
    /// If `team_name` isn't registered yet (a persistent member can be the
    /// *first* thing to reference a team name, without a prior `TeamCreate`
    /// call), a minimal entry is created so `TeamList` still shows it —
    /// `team_dir` is left empty in that case (no `.agix/teams/` directory
    /// exists for a team that was never `TeamCreate`d).
    pub fn update_member_lifecycle(
        &self,
        team_name: &str,
        label: &str,
        lifecycle: TeammateLifecycle,
        idle_since_secs: Option<u64>,
    ) {
        let mut teams = self.teams.write().unwrap_or_else(|e| e.into_inner());
        match teams.get_mut(team_name) {
            Some(team) => match team.members.iter_mut().find(|m| m.label == label) {
                Some(m) => {
                    m.lifecycle = lifecycle;
                    m.idle_since_secs = idle_since_secs;
                }
                None => team.members.push(TeamMemberInfo {
                    label: label.to_string(),
                    lifecycle,
                    idle_since_secs,
                }),
            },
            None => {
                teams.insert(
                    team_name.to_string(),
                    TeamInfo {
                        team_id: team_name.to_string(),
                        name: team_name.to_string(),
                        created_at_secs: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        team_dir: PathBuf::new(),
                        members: vec![TeamMemberInfo {
                            label: label.to_string(),
                            lifecycle,
                            idle_since_secs,
                        }],
                        permission_mode: None,
                    },
                );
            }
        }
    }

    /// The permission grant already established for this team's persistent
    /// members, if any — see `TeamInfo::permission_mode`'s doc comment.
    pub fn team_permission_mode(&self, team_name: &str) -> Option<PermissionMode> {
        self.teams
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(team_name)
            .and_then(|t| t.permission_mode)
    }

    /// Establish `team_name`'s permission grant if it doesn't have one yet
    /// ("one team, one authorization" — first spawn under a name wins,
    /// silently a no-op for every later spawn under the same name that
    /// doesn't pass an explicit override). Creates a minimal entry if the
    /// team isn't registered yet, same as `update_member_lifecycle`.
    pub fn set_team_permission_mode_if_absent(&self, team_name: &str, mode: PermissionMode) {
        let mut teams = self.teams.write().unwrap_or_else(|e| e.into_inner());
        match teams.get_mut(team_name) {
            Some(team) => {
                if team.permission_mode.is_none() {
                    team.permission_mode = Some(mode);
                }
            }
            None => {
                teams.insert(
                    team_name.to_string(),
                    TeamInfo {
                        team_id: team_name.to_string(),
                        name: team_name.to_string(),
                        created_at_secs: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        team_dir: PathBuf::new(),
                        members: Vec::new(),
                        permission_mode: Some(mode),
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str) -> TeamInfo {
        TeamInfo {
            team_id: format!("team-{name}-1"),
            name: name.to_string(),
            created_at_secs: 0,
            team_dir: PathBuf::from("/tmp/x"),
            members: vec![TeamMemberInfo {
                label: "worker".into(),
                lifecycle: TeammateLifecycle::Completed,
                idle_since_secs: None,
            }],
            permission_mode: None,
        }
    }

    #[test]
    fn upsert_then_list_returns_it_sorted_by_name() {
        let reg = TeamRegistry::new();
        reg.upsert(info("b-team"));
        reg.upsert(info("a-team"));
        let names: Vec<String> = reg.list().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["a-team".to_string(), "b-team".to_string()]);
    }

    #[test]
    fn upsert_same_name_twice_overwrites_not_duplicates() {
        let reg = TeamRegistry::new();
        reg.upsert(info("dup"));
        reg.upsert(info("dup"));
        assert_eq!(reg.list().len(), 1);
    }

    #[test]
    fn remove_returns_the_entry_and_it_stops_appearing_in_list() {
        let reg = TeamRegistry::new();
        reg.upsert(info("gone-soon"));
        let removed = reg.remove("gone-soon");
        assert!(removed.is_some());
        assert!(reg.list().is_empty());
    }

    #[test]
    fn remove_nonexistent_name_returns_none() {
        let reg = TeamRegistry::new();
        assert!(reg.remove("never-existed").is_none());
    }

    #[test]
    fn update_member_lifecycle_creates_a_minimal_team_entry_when_none_exists() {
        let reg = TeamRegistry::new();
        reg.update_member_lifecycle("new-team", "worker", TeammateLifecycle::Active, None);
        let teams = reg.list();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].name, "new-team");
        assert_eq!(teams[0].members.len(), 1);
        assert_eq!(teams[0].members[0].label, "worker");
        assert_eq!(teams[0].members[0].lifecycle, TeammateLifecycle::Active);
    }

    #[test]
    fn update_member_lifecycle_updates_an_existing_member_in_place() {
        let reg = TeamRegistry::new();
        reg.upsert(info("existing"));
        reg.update_member_lifecycle("existing", "worker", TeammateLifecycle::Idle, Some(123));
        let teams = reg.list();
        assert_eq!(
            teams[0].members.len(),
            1,
            "must update in place, not duplicate"
        );
        assert_eq!(teams[0].members[0].lifecycle, TeammateLifecycle::Idle);
        assert_eq!(teams[0].members[0].idle_since_secs, Some(123));
    }

    #[test]
    fn update_member_lifecycle_adds_a_new_member_to_an_existing_team() {
        let reg = TeamRegistry::new();
        reg.upsert(info("existing"));
        reg.update_member_lifecycle("existing", "second-worker", TeammateLifecycle::Active, None);
        let teams = reg.list();
        assert_eq!(teams[0].members.len(), 2);
    }

    #[test]
    fn team_permission_mode_is_none_until_established() {
        let reg = TeamRegistry::new();
        assert_eq!(reg.team_permission_mode("nonexistent"), None);
        reg.update_member_lifecycle("t", "worker", TeammateLifecycle::Active, None);
        assert_eq!(reg.team_permission_mode("t"), None);
    }

    #[test]
    fn set_team_permission_mode_if_absent_is_first_call_wins() {
        let reg = TeamRegistry::new();
        reg.set_team_permission_mode_if_absent("t", PermissionMode::BypassPermissions);
        reg.set_team_permission_mode_if_absent("t", PermissionMode::Plan);
        assert_eq!(
            reg.team_permission_mode("t"),
            Some(PermissionMode::BypassPermissions),
            "second call must not override the first team-wide grant"
        );
    }
}
