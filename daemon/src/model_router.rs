//! Builds the `Arc<dyn Model>` instances multi-provider LLM routing needs,
//! from parsed `settings.json` provider config — the piece
//! `base::provider::resolve_task_models` deliberately leaves undone (it only
//! resolves *which* provider/model a task should use, as pure data; it
//! can't construct a model itself without `base` depending on `model`,
//! inverting the crate layering).
//!
//! Which protocols can be built is no longer decided here. This assembles a
//! router from whatever is in a
//! [`ModelFactoryRegistry`](base::interface::model_factory::ModelFactoryRegistry);
//! `model::factory::builtin_registry()` is the default contents, and a host
//! that wants a third protocol adds to it rather than editing this file.

use base::interface::credentials::CredentialSource;
use base::interface::model_factory::ModelFactoryRegistry;
use base::provider::{ProviderConfig, ResolvedModel, TaskRouter};
use std::collections::HashMap;
use std::sync::Arc;

/// Build a [`TaskRouter`] from parsed provider config + the result of
/// `base::provider::resolve_task_models` (already validated — this is the
/// only place that turns config into live client instances).
///
/// Builds one model instance per entry in `providers_cfg`, not just the
/// ones actually referenced by `resolved` — the map is expected to be small
/// (a handful of providers at most), and building all of them up front
/// means a later `config.setProvider` call that starts routing traffic to a
/// previously-unused provider doesn't need any extra plumbing. A
/// misconfigured provider (bad `api_key`/`base_url`/unsupported
/// `api_type`) fails the whole call — same "fail at startup, not on first
/// use" rationale as `resolve_task_models`'s hard-error paths.
pub fn build_task_router(
    providers_cfg: &HashMap<String, ProviderConfig>,
    default_provider: &str,
    resolved: HashMap<String, ResolvedModel>,
    credentials: &Arc<dyn CredentialSource>,
) -> Result<TaskRouter, String> {
    build_task_router_with(
        providers_cfg,
        default_provider,
        resolved,
        &model::factory::builtin_registry().with_credentials(credentials.clone()),
    )
}

/// [`build_task_router`] against a registry the caller chose — the entry
/// point for a host that registered a protocol of its own.
pub fn build_task_router_with(
    providers_cfg: &HashMap<String, ProviderConfig>,
    default_provider: &str,
    resolved: HashMap<String, ResolvedModel>,
    factories: &ModelFactoryRegistry,
) -> Result<TaskRouter, String> {
    let mut providers = HashMap::with_capacity(providers_cfg.len());
    for (id, cfg) in providers_cfg {
        providers.insert(id.clone(), factories.build(id, cfg)?);
    }
    let default = providers
        .get(default_provider)
        .cloned()
        .ok_or_else(|| format!("default_provider '{default_provider}' not present in providers"))?;
    Ok(TaskRouter::new(providers, resolved, default))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Result::unwrap_err()` requires the `Ok` type to be `Debug` (for the
    /// panic message if it turns out to be `Ok`) — `Arc<dyn Model>` and
    /// `TaskRouter` aren't, so this stands in.
    fn err_of<T>(r: Result<T, String>) -> String {
        match r {
            Ok(_) => panic!("expected Err"),
            Err(e) => e,
        }
    }

    fn builtins() -> ModelFactoryRegistry {
        model::factory::builtin_registry()
    }

    fn settings_only() -> Arc<dyn CredentialSource> {
        Arc::new(base::interface::credentials::ConfigCredentials)
    }

    fn anthropic_provider(api_key: &str) -> ProviderConfig {
        ProviderConfig {
            api_type: Some("anthropic".into()),
            base_url: None,
            api_key: Some(api_key.into()),
            api_key_env: None,
            default_model: Some("claude-sonnet-4-6".into()),
            models: vec![],
        }
    }

    #[test]
    fn provider_config_succeeds_for_anthropic() {
        let cfg = anthropic_provider("sk-ant-test");
        assert!(builtins().build("anthropic", &cfg).is_ok());
    }

    #[test]
    fn provider_config_defaults_missing_api_type_to_anthropic() {
        let mut cfg = anthropic_provider("sk-ant-test");
        cfg.api_type = None;
        assert!(builtins().build("anthropic", &cfg).is_ok());
    }

    /// N-16: `openai_compatible` builds a real model now. It needs an
    /// explicit `base_url` (there is no sensible default host), and that
    /// requirement has to be reported clearly rather than as an auth failure
    /// on the first call.
    #[test]
    fn provider_config_accepts_openai_compatible_with_a_base_url() {
        let mut cfg = anthropic_provider("key");
        cfg.api_type = Some("openai_compatible".into());
        cfg.base_url = Some("https://api.example.com/v1".into());
        let built = builtins().build("oa", &cfg).expect("should build");
        assert_eq!(built.api_type(), base::provider::ApiType::OpenAICompatible);
    }

    #[test]
    fn provider_config_requires_base_url_for_openai_compatible() {
        let mut cfg = anthropic_provider("key");
        cfg.api_type = Some("openai_compatible".into());
        cfg.base_url = None;
        let err = err_of(builtins().build("oa", &cfg));
        assert!(err.contains("base_url"), "{err}");
    }

    #[test]
    fn provider_config_rejects_unknown_api_type() {
        let mut cfg = anthropic_provider("key");
        cfg.api_type = Some("bedrock".into());
        let err = err_of(builtins().build("b", &cfg));
        assert!(err.contains("bedrock"));
    }

    #[test]
    fn provider_config_requires_api_key() {
        let mut cfg = anthropic_provider("");
        cfg.api_key = None;
        let err = err_of(builtins().build("anthropic", &cfg));
        assert!(err.contains("api_key"));

        cfg.api_key = Some(String::new());
        let err = err_of(builtins().build("anthropic", &cfg));
        assert!(err.contains("api_key"));
    }

    #[test]
    fn provider_config_rejects_invalid_base_url() {
        let mut cfg = anthropic_provider("key");
        cfg.base_url = Some("not a url".into());
        let err = err_of(builtins().build("anthropic", &cfg));
        assert!(err.contains("invalid base_url"));
    }

    #[test]
    fn build_task_router_succeeds_with_valid_config() {
        let mut providers = HashMap::new();
        providers.insert("anthropic".to_string(), anthropic_provider("key1"));
        providers.insert("deepseek".to_string(), {
            let mut p = anthropic_provider("key2");
            p.default_model = Some("deepseek-pro".into());
            p
        });

        let router =
            build_task_router(&providers, "anthropic", HashMap::new(), &settings_only()).unwrap();
        // model_for on an unresolved task falls back to the default
        // provider — just confirming this doesn't panic/error is the point
        // (identity checks live in `base::provider`'s own tests).
        let _ = router.model_for("main");
    }

    #[test]
    fn build_task_router_fails_when_default_provider_missing() {
        let providers = HashMap::new();
        let err = err_of(build_task_router(
            &providers,
            "anthropic",
            HashMap::new(),
            &settings_only(),
        ));
        assert!(err.contains("anthropic"));
    }

    #[test]
    fn build_task_router_propagates_a_single_bad_providers_error() {
        let mut providers = HashMap::new();
        providers.insert("anthropic".to_string(), anthropic_provider("key1"));
        providers.insert("broken".to_string(), {
            let mut p = anthropic_provider("key2");
            p.api_type = Some("bedrock".into());
            p
        });
        let err = err_of(build_task_router(
            &providers,
            "anthropic",
            HashMap::new(),
            &settings_only(),
        ));
        assert!(err.contains("broken"));
        assert!(err.contains("bedrock"));
    }

    /// The point of the registry: a protocol the engine does not implement,
    /// reachable by configuration alone. No kernel edit, no new `ApiType`
    /// variant, and the router hands it out for the tasks that name it.
    #[test]
    fn a_protocol_the_engine_never_heard_of_can_be_registered_and_routed_to() {
        struct Fictional;

        #[async_trait::async_trait]
        impl base::interface::model::Model for Fictional {
            fn api_type(&self) -> base::provider::ApiType {
                base::provider::ApiType::OpenAICompatible
            }
            async fn stream(
                &self,
                _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
                _tools: Vec<base::interface::model::ToolDef>,
                _messages: Vec<base::interface::model::ModelMessage>,
                _params: base::interface::model::StreamParams,
                _cancel: tokio_util::sync::CancellationToken,
            ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
            {
                unimplemented!("never called — this test is about construction and routing")
            }
        }

        struct FictionalFactory;

        impl base::interface::model_factory::ModelFactory for FictionalFactory {
            fn api_type(&self) -> &str {
                "fictional"
            }
            fn build(
                &self,
                _provider_id: &str,
                _cfg: &ProviderConfig,
                _credentials: &dyn base::interface::credentials::CredentialSource,
            ) -> Result<std::sync::Arc<dyn base::interface::model::Model>, String> {
                Ok(std::sync::Arc::new(Fictional))
            }
        }

        let mut factories = builtins();
        factories.register(std::sync::Arc::new(FictionalFactory));

        let mut cfg = anthropic_provider("key");
        cfg.api_type = Some("fictional".into());
        let providers = HashMap::from([("mine".to_string(), cfg)]);

        let router = build_task_router_with(
            &providers,
            "mine",
            HashMap::from([(
                "main".to_string(),
                ResolvedModel {
                    provider_id: "mine".into(),
                    model: "whatever".into(),
                },
            )]),
            &factories,
        )
        .expect("a registered protocol must build");

        assert_eq!(
            router.model_for("main").api_type(),
            base::provider::ApiType::OpenAICompatible,
            "the task must be routed to the model the registered factory built"
        );
        assert_eq!(router.model_name_for("main"), Some("whatever"));
    }

    /// A host that must not keep credentials on disk: the key is absent from
    /// config entirely, and the provider still builds.
    #[test]
    fn a_credential_source_can_supply_what_settings_json_does_not_hold() {
        use base::interface::credentials::{Secret, StaticCredentials};

        let mut cfg = anthropic_provider("");
        cfg.api_key = None;
        assert!(
            builtins().build("vaulted", &cfg).is_err(),
            "with no key anywhere, this must still fail"
        );

        let factories =
            builtins().with_credentials(std::sync::Arc::new(StaticCredentials::new([(
                "vaulted".to_string(),
                Secret::new("sk-from-the-vault"),
            )])));
        assert!(
            factories.build("vaulted", &cfg).is_ok(),
            "the registry must ask the source, not the config field"
        );
    }

    /// What the daemon builds every binding's router with when the host hands
    /// a source in: entries that carry a key keep using it, the rest ask the
    /// host.
    #[test]
    fn the_router_asks_the_host_for_keys_the_settings_leave_out() {
        use base::interface::credentials::{ConfigThen, Secret, StaticCredentials};

        let mut providers = HashMap::new();
        providers.insert("anthropic".to_string(), anthropic_provider("key1"));
        providers.insert("vaulted".to_string(), {
            let mut p = anthropic_provider("");
            p.api_key = None;
            p
        });
        assert!(
            build_task_router(&providers, "anthropic", HashMap::new(), &settings_only()).is_err()
        );

        let host: Arc<dyn CredentialSource> = Arc::new(ConfigThen(Arc::new(
            StaticCredentials::new([("vaulted".to_string(), Secret::new("sk-from-the-vault"))]),
        )));
        assert!(build_task_router(&providers, "anthropic", HashMap::new(), &host).is_ok());
    }
}
