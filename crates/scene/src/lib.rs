//! Built-in scene implementations, declarative scenes, and the registry hosts
//! resolve them from.
//!
//! Agix provides `code` (full-capability coding agent), `harnix` (the
//! default scene of Harnix-hosted sessions) and `mini` (the smallest working
//! coding agent). [`declarative`] turns a `scene.json` + `prompt.md`
//! directory into a scene.

pub mod code;
mod common;
pub mod declarative;
pub mod harnix;
pub mod mini;

pub use declarative::{DeclaredScene, SceneLoadError};

use base::interface::scene::SceneSpec;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Scene metadata for discovery.
#[derive(Debug, Clone)]
pub struct SceneInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    /// See [`SceneSpec::requires_project`] — hosts use it to decide whether
    /// creating a session needs a project first.
    pub requires_project: bool,
    /// See [`SceneSpec::supports_team`].
    pub supports_team: bool,
}

/// A built-in scene by id: `code` (interactive), `harnix` or `mini`.
pub fn builtin(id: &str) -> Option<SceneSpec> {
    match id {
        "code" => Some(code::spec(code::Interaction::Interactive)),
        "harnix" => Some(harnix::spec()),
        "mini" => Some(mini::spec()),
        _ => None,
    }
}

/// Registry of available scenes. Populated at process startup.
#[derive(Default)]
pub struct SceneRegistry {
    scenes: HashMap<String, Arc<SceneSpec>>,
}

impl SceneRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, scene: Arc<SceneSpec>) {
        self.scenes.insert(scene.id.clone(), scene);
    }
    pub fn resolve(&self, id: &str) -> Option<Arc<SceneSpec>> {
        self.scenes.get(id).cloned()
    }
    pub fn ids(&self) -> Vec<String> {
        self.scenes.keys().cloned().collect()
    }
    pub fn list_all(&self) -> Vec<SceneInfo> {
        self.scenes
            .iter()
            .map(|(id, s)| SceneInfo {
                id: id.clone(),
                name: s.name.clone(),
                description: s.description.clone(),
                requires_project: s.requires_project,
                supports_team: s.supports_team,
            })
            .collect()
    }
    /// Register a declarative scene unless its id is already taken.
    ///
    /// Call after the built-in and host scenes are in: a file in a user's
    /// home directory must not be able to stand in for `code` under the
    /// same name.
    pub fn register_declarative(&mut self, scene: DeclaredScene) -> Result<(), SceneLoadError> {
        if self.scenes.contains_key(&scene.spec.id) {
            return Err(SceneLoadError::IdTaken {
                path: scene.dir.join(declarative::SCENE_FILE),
                id: scene.spec.id,
            });
        }
        self.register(Arc::new(scene.spec));
        Ok(())
    }

    /// Load every scene under `root` (see [`declarative::load_all`]) and
    /// register those whose id is free. Returns what could not be loaded or
    /// registered; the rest are in.
    pub fn register_declarative_dir(&mut self, root: &Path) -> Vec<SceneLoadError> {
        declarative::load_all(root)
            .into_iter()
            .filter_map(|loaded| loaded.and_then(|s| self.register_declarative(s)).err())
            .collect()
    }

    pub fn register_builtin(&mut self) {
        for spec in [
            code::spec(code::Interaction::Interactive),
            harnix::spec(),
            mini::spec(),
        ] {
            self.register(Arc::new(spec));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_scene(root: &Path, id: &str) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(declarative::SCENE_FILE),
            format!(r#"{{"id": "{id}", "name": "{id}"}}"#),
        )
        .unwrap();
        std::fs::write(dir.join(declarative::PROMPT_FILE), "prompt").unwrap();
    }

    #[test]
    fn a_declarative_scene_cannot_shadow_a_built_in_one() {
        let root = tempfile::tempdir().unwrap();
        write_scene(root.path(), "code");
        write_scene(root.path(), "review");

        let mut registry = SceneRegistry::new();
        registry.register_builtin();
        let refused = registry.register_declarative_dir(root.path());

        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(
            matches!(&refused[0], SceneLoadError::IdTaken { id, .. } if id == "code"),
            "{refused:?}"
        );
        assert!(refused[0].to_string().contains("already registered"));
        assert_eq!(
            registry.resolve("code").unwrap().name,
            builtin("code").unwrap().name,
            "the built-in stays in place"
        );
        assert_eq!(registry.resolve("review").unwrap().name, "review");
    }

    #[test]
    fn listing_includes_declarative_scenes() {
        let root = tempfile::tempdir().unwrap();
        write_scene(root.path(), "review");
        let mut registry = SceneRegistry::new();
        assert!(registry.register_declarative_dir(root.path()).is_empty());
        let listed = registry.list_all();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "review");
    }
}
