//! Built-in scene implementations, declarative scenes, and the registry hosts
//! resolve them from.
//!
//! Agix provides `coding` (full-capability agent), `chat` (general assistant),
//! `research` (search/compare/conclude), and `demo` (framework showcase).
//! [`declarative`] turns a `scene.json` + `prompt.md` directory into a scene.

pub mod chat;
pub mod coding;
pub mod declarative;
pub mod demo;
pub mod research;

pub use declarative::{DeclarativeScene, SceneLoadError};

use base::interface::scene::Scene;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Scene metadata for discovery.
#[derive(Debug, Clone)]
pub struct SceneInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    /// See [`Scene::requires_project`] — hosts use it to decide whether
    /// creating a session needs a project first.
    pub requires_project: bool,
    /// See [`Scene::supports_team`].
    pub supports_team: bool,
}

/// Registry of available scenes. Populated at process startup.
#[derive(Default)]
pub struct SceneRegistry {
    scenes: HashMap<String, Arc<dyn Scene>>,
}

impl SceneRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, scene: Arc<dyn Scene>) {
        self.scenes.insert(scene.id().to_string(), scene);
    }
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn Scene>> {
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
                name: s.name().to_string(),
                description: s.description().to_string(),
                requires_project: s.requires_project(),
                supports_team: s.supports_team(),
            })
            .collect()
    }
    /// Register a declarative scene unless its id is already taken.
    ///
    /// Call after the built-in and host scenes are in: a file in a user's
    /// home directory must not be able to stand in for `coding` under the
    /// same name.
    pub fn register_declarative(&mut self, scene: DeclarativeScene) -> Result<(), SceneLoadError> {
        if self.scenes.contains_key(scene.id()) {
            return Err(SceneLoadError::IdTaken {
                path: scene.dir().join(declarative::SCENE_FILE),
                id: scene.id().to_string(),
            });
        }
        self.register(Arc::new(scene));
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
        self.register(Arc::new(chat::ChatScene));
        self.register(Arc::new(coding::CodingScene));
        self.register(Arc::new(demo::DemoScene));
        self.register(Arc::new(research::ResearchScene));
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
        write_scene(root.path(), "coding");
        write_scene(root.path(), "review");

        let mut registry = SceneRegistry::new();
        registry.register_builtin();
        let refused = registry.register_declarative_dir(root.path());

        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(
            matches!(&refused[0], SceneLoadError::IdTaken { id, .. } if id == "coding"),
            "{refused:?}"
        );
        assert!(refused[0].to_string().contains("already registered"));
        assert_eq!(
            registry.resolve("coding").unwrap().name(),
            coding::CodingScene.name(),
            "the built-in stays in place"
        );
        assert_eq!(registry.resolve("review").unwrap().name(), "review");
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
