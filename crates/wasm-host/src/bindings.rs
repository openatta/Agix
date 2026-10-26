//! Generated host-side bindings for `agix:plugin@0.1.0`.
//!
//! The WIT world in `wit/plugin.wit` is the contract; this module is the
//! typed glue `wit-bindgen` derives from it. Nothing here is hand-written on
//! purpose — the moment host and guest types are maintained separately they
//! drift, and a drifting ABI fails at call time rather than at build time.

wasmtime::component::bindgen!({
    path: "wit",
    world: "plugin",
    // Host functions do real I/O (HTTP most obviously), so a component
    // calling one must be able to suspend instead of blocking a runtime
    // thread. That makes every guest export a future too.
    imports: { default: async },
    exports: { default: async },
});

/// The API version this build implements.
///
/// A plugin declares the version it was built against in its manifest, and a
/// mismatch is refused rather than downgraded: the WIT world, the capability
/// semantics and the event whitelist move together, so there is no partial
/// contract for a plugin to fall back to.
pub const API_VERSION: &str = "0.1";

/// The package identifier the WIT world is published under. A component
/// carries this in its own metadata, which is the second check on the
/// version a manifest claims — a manifest can be edited, the component's
/// embedded WIT cannot without rebuilding it.
pub const WIT_PACKAGE: &str = "agix:plugin@0.1.0";

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest crate and this crate name the same version from opposite
    /// sides of a dependency edge that doesn't exist — `plugin` deliberately
    /// doesn't depend on `wasm-host`. This is the check that they agree.
    #[test]
    fn the_declared_api_version_is_the_one_the_manifest_layer_accepts() {
        assert!(
            plugin::manifest::SUPPORTED_API_VERSIONS.contains(&API_VERSION),
            "wasm-host implements {API_VERSION}, which the manifest layer would reject"
        );
    }

    #[test]
    fn the_wit_package_identifier_carries_the_api_version() {
        assert!(
            WIT_PACKAGE.contains(API_VERSION),
            "{WIT_PACKAGE} should embed {API_VERSION}"
        );
    }
}
