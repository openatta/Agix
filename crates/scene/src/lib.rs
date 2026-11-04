//! Built-in scene implementations and the registry hosts resolve them from.
//!
//! Agix provides `coding` (full-capability agent), `chat` (general assistant),
//! `research` (search/compare/conclude), and `demo` (framework showcase).

pub mod chat;
pub mod coding;
pub mod demo;
pub mod research;

use base::interface::scene::Scene;
use std::collections::HashMap;
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
    pub fn register_builtin(&mut self) {
        self.register(Arc::new(chat::ChatScene));
        self.register(Arc::new(coding::CodingScene));
        self.register(Arc::new(demo::DemoScene));
        self.register(Arc::new(research::ResearchScene));
    }
}
