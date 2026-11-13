//! `multi_scene_tests` 原样搬来的用例。

use super::mcp_for_project::test_pool;
use super::*;

/// Two scenes on one daemon must each read their own settings tier: a
/// `harnix` session on a daemon started as `--scene code` runs on
/// harnix's tier, not code's.
#[tokio::test]
async fn each_scene_resolves_its_own_settings_tier() {
    let (pool, dir) = test_pool().await;

    // `scenes/harnix/settings.json` sets a model the pool's own tier never
    // mentions — if the harnix session picks it up, it read harnix's tier.
    let harnix_tier = dir.path().join("scenes").join("harnix");
    std::fs::create_dir_all(&harnix_tier).unwrap();
    std::fs::write(
        harnix_tier.join("settings.json"),
        r#"{"model": {"model_name": "model-from-harnix-tier"}}"#,
    )
    .unwrap();

    let code = pool.settings_for_project(None, "code").await;
    let harnix = pool.settings_for_project(None, "harnix").await;

    assert_eq!(harnix.model.model_name, "model-from-harnix-tier");
    assert_ne!(
        code.model.model_name, harnix.model.model_name,
        "two scenes must not share one settings entry"
    );
    assert_eq!(
        harnix.paths.scope, "harnix",
        "scope must be the session's scene"
    );
    assert_eq!(code.paths.scope, "code");
}

/// The same project under two scenes gets two cache entries, and each is
/// stable across calls.
#[tokio::test]
async fn settings_cache_is_keyed_by_project_and_scene_together() {
    let (pool, dir) = test_pool().await;
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();

    let a1 = pool.settings_for_project(Some(&other), "code").await;
    let a2 = pool.settings_for_project(Some(&other), "code").await;
    let b = pool.settings_for_project(Some(&other), "harnix").await;

    assert!(
        Arc::ptr_eq(&a1, &a2),
        "same (project, scene) must be cached"
    );
    assert!(
        !Arc::ptr_eq(&a1, &b),
        "one project under two scenes must not collapse to one entry"
    );
    assert_eq!(a1.paths.scope, "code");
    assert_eq!(b.paths.scope, "harnix");
}

/// The daemon's own scene keeps the injected `config_root()`; any other
/// scene is derived under `global_root()/scenes/`.
///
/// Tests inject a `StaticDaemonPaths` whose config root is a tempdir that
/// bears no relation to `global_root()/scenes/<id>`, so recomputing the
/// startup scene's directory would move its settings out from under them.
#[tokio::test]
async fn the_startup_scene_keeps_its_injected_config_root() {
    let (pool, _dir) = test_pool().await;

    assert_eq!(
        pool.bindings.scene_root("code", "code"),
        pool.bindings.paths().config_root(),
        "the startup scene must not be recomputed"
    );
    assert_eq!(
        pool.bindings.scene_root("harnix", "code"),
        pool.bindings
            .paths()
            .global_root()
            .join("scenes")
            .join("harnix"),
        "another scene is derived the way DefaultDaemonPaths derives one"
    );
}

/// A scene that was never activated must be refused, not silently served
/// by the daemon's default.
#[tokio::test]
async fn creating_a_session_in_an_inactive_scene_is_refused() {
    let (pool, _dir) = test_pool().await;
    assert!(
        pool.scenes.resolve(Some("mini")).is_none(),
        "mini was never activated on this pool"
    );
    assert!(
        pool.scenes.resolve(Some("harnix")).is_none(),
        "harnix was never activated either"
    );
    assert!(
        pool.scenes.resolve(None).is_some(),
        "no scene named falls back to the daemon's own"
    );
}

/// `scene.activate` makes a second scene usable in the same process, and
/// sessions created under each are recorded under their own scene.
#[tokio::test]
async fn two_scenes_can_be_active_in_one_process() {
    let (pool, _dir) = test_pool().await;

    pool.activate_scene("harnix")
        .await
        .expect("activate harnix");

    let code = pool.scenes.resolve(Some("code")).expect("code");
    let harnix = pool.scenes.resolve(Some("harnix")).expect("harnix");
    assert_eq!(code.id, "code");
    assert_eq!(harnix.id, "harnix");

    // Distinct scenes, so distinct tool surfaces — this is what makes
    // running both in one process meaningful rather than cosmetic.
    assert!(
        code.tools.allow.is_none(),
        "code allows every registered tool"
    );
    assert!(
        harnix.tools.allow.is_some(),
        "harnix restricts its tools to a whitelist"
    );
}
