//! The declarative scene a package ships in `[scene] dir`.
//!
//! A directory of `scene.json` + `prompt.md`, read by `scene::DeclarativeScene`
//! like a scene in `~/.agix/scenes/`. No component is compiled and no runtime
//! is involved, which is why this lives with the package layer: a package
//! whose contribution is a scene is usable in a build that carries no
//! WebAssembly at all.
//!
//! The id is the plugin's, `plugin:<name>`, as for `[scene.own]` — a plugin
//! owns its own scene and never anyone else's.

use crate::manifest::Plugin;
use ::scene::{DeclarativeScene, SceneLoadError};
use base::interface::scene::Scene;
use std::sync::Arc;

/// The scene this package's `[scene] dir` declares, or `None` when it
/// declares none.
pub fn declared_scene(plugin: &Plugin) -> Option<Result<DeclarativeScene, SceneLoadError>> {
    let dir = plugin.manifest.scene.dir.as_ref()?;
    Some(DeclarativeScene::load_for_plugin(
        &plugin.path(dir),
        plugin.name(),
    ))
}

/// Every declared scene among `plugins` that loads. One that does not is
/// left out with a warning naming the file and the fault; the package's
/// other contributions are unaffected.
pub fn declared_scenes(plugins: &[Plugin]) -> Vec<Arc<dyn Scene>> {
    plugins
        .iter()
        .filter_map(|p| match declared_scene(p)? {
            Ok(scene) => Some(Arc::new(scene) as Arc<dyn Scene>),
            Err(e) => {
                tracing::warn!(
                    plugin = %p.name(),
                    error = %e,
                    "plugin declares a scene directory that does not load; not registering the scene"
                );
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn package(root: &Path, scene_json: &str) -> Plugin {
        std::fs::write(
            root.join("plugin.toml"),
            "[plugin]\nname = \"review\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\n\
             [scene]\ndir = \"scene\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("scene")).unwrap();
        std::fs::write(root.join("scene/scene.json"), scene_json).unwrap();
        std::fs::write(root.join("scene/prompt.md"), "You review.").unwrap();
        Plugin::load(root, &root.join("plugin.toml")).unwrap()
    }

    #[test]
    fn a_package_scene_registers_under_the_plugins_id() {
        let dir = tempfile::tempdir().unwrap();
        let p = package(
            dir.path(),
            r#"{"id": "plugin:review", "name": "Review", "tools": ["Read"]}"#,
        );
        assert_eq!(p.scene_id().as_deref(), Some("plugin:review"));

        let scenes = declared_scenes(&[p]);
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].id(), "plugin:review");
        assert_eq!(scenes[0].tools(), ["Read"]);
    }

    #[test]
    fn a_package_scene_cannot_take_another_id() {
        let dir = tempfile::tempdir().unwrap();
        let p = package(dir.path(), r#"{"id": "coding", "name": "Not coding"}"#);
        let e = declared_scene(&p).unwrap().unwrap_err().to_string();
        assert!(e.contains(r#"must be "plugin:review""#), "{e}");
        assert!(declared_scenes(&[p]).is_empty());
    }

    #[test]
    fn a_package_without_a_scene_dir_declares_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            "[plugin]\nname = \"p\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n",
        )
        .unwrap();
        let p = Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        assert!(declared_scene(&p).is_none());
        assert_eq!(p.scene_id(), None);
    }
}
