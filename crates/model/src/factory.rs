//! The two protocols this crate implements, as registrable factories.
//!
//! The composition root used to match on `api_type` and build these inline,
//! which put "which protocols exist" in the daemon rather than in the crate
//! that implements them, and made a third protocol impossible to add without
//! editing that match. These are the same two constructions, moved next to
//! the models they construct and behind
//! [`ModelFactory`](base::interface::model_factory::ModelFactory) so a host
//! can add a third — or replace one of these — without touching the kernel.

use std::sync::Arc;

use base::interface::credentials::CredentialSource;
use base::interface::model::Model;
use base::interface::model_factory::{ModelFactory, ModelFactoryRegistry};
use base::provider::ProviderConfig;

use crate::adapter::AnthropicModel;
use crate::client::{AnthropicClient, AuthMode, HttpAnthropicClient};

/// Anthropic's Messages API. Also what an unset `api_type` means.
pub struct AnthropicFactory;

impl ModelFactory for AnthropicFactory {
    fn api_type(&self) -> &str {
        "anthropic"
    }

    fn build(
        &self,
        provider_id: &str,
        cfg: &ProviderConfig,
        credentials: &dyn CredentialSource,
    ) -> Result<Arc<dyn Model>, String> {
        let api_key = credentials
            .api_key(provider_id, cfg)
            .map_err(|e| format!("provider '{provider_id}': {e}"))?;
        let auth = AuthMode::ApiKey(api_key.expose().to_string());
        let client: Arc<dyn AnthropicClient> = match cfg.base_url.as_deref() {
            Some(url) if !url.is_empty() => {
                let mut url = url.to_string();
                if !url.ends_with('/') {
                    url.push('/');
                }
                let base = url::Url::parse(&url).map_err(|e| {
                    format!("provider '{provider_id}': invalid base_url '{url}': {e}")
                })?;
                Arc::new(
                    HttpAnthropicClient::with_base(auth, base)
                        .map_err(|e| format!("provider '{provider_id}': {e}"))?,
                )
            }
            _ => Arc::new(
                HttpAnthropicClient::new(auth)
                    .map_err(|e| format!("provider '{provider_id}': {e}"))?,
            ),
        };
        Ok(Arc::new(
            AnthropicModel::new(client).with_provenance(provider_id),
        ))
    }
}

/// OpenAI Chat Completions, and the gateways that only speak its shape.
pub struct OpenAICompatibleFactory;

impl ModelFactory for OpenAICompatibleFactory {
    fn api_type(&self) -> &str {
        "openai_compatible"
    }

    fn build(
        &self,
        provider_id: &str,
        cfg: &ProviderConfig,
        credentials: &dyn CredentialSource,
    ) -> Result<Arc<dyn Model>, String> {
        // Required here, unlike the Anthropic factory: there is no default
        // OpenAI-compatible host to fall back to, and guessing one produces a
        // confusing auth failure at first use instead of a clear config error
        // at startup.
        let base_url = cfg
            .base_url
            .clone()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                format!("provider '{provider_id}': api_type 'openai_compatible' requires base_url")
            })?;
        let api_key = credentials
            .api_key(provider_id, cfg)
            .map_err(|e| format!("provider '{provider_id}': {e}"))?;
        let default_model = cfg.default_model.clone().unwrap_or_default();
        Ok(Arc::new(
            crate::OpenAICompatibleModel::new(&base_url, api_key.expose(), default_model)
                .map_err(|e| format!("provider '{provider_id}': {e}"))?
                .with_provenance(provider_id)
                .with_reasoning(cfg.reasoning_passback, cfg.thinking_toggle),
        ))
    }
}

/// The protocols the engine ships with. A host adds to this rather than
/// building its own from scratch, so registering a third protocol does not
/// mean re-registering the first two.
pub fn builtin_registry() -> ModelFactoryRegistry {
    let mut registry = ModelFactoryRegistry::new();
    registry.register(Arc::new(AnthropicFactory));
    registry.register(Arc::new(OpenAICompatibleFactory));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtins_are_the_two_protocols_this_crate_implements() {
        assert_eq!(
            builtin_registry().api_types(),
            ["anthropic", "openai_compatible"]
        );
    }

    /// Same message the hand-written match produced, still produced at the
    /// point config is read.
    #[test]
    fn openai_compatible_without_a_base_url_fails_with_the_reason() {
        let Err(err) = builtin_registry().build(
            "relay",
            &ProviderConfig {
                api_type: Some("openai_compatible".into()),
                api_key: Some("k".into()),
                ..Default::default()
            },
        ) else {
            panic!("a config with no base_url must not build");
        };
        assert_eq!(
            err,
            "provider 'relay': api_type 'openai_compatible' requires base_url"
        );
    }

    #[test]
    fn a_provider_with_no_api_key_says_which_provider() {
        let Err(err) = builtin_registry().build("main", &ProviderConfig::default()) else {
            panic!("a config with no api_key must not build");
        };
        assert_eq!(err, "provider 'main': missing api_key");
    }

    #[test]
    fn built_clients_know_which_provider_they_are() {
        let registry = builtin_registry();
        for (id, api_type) in [("claude", "anthropic"), ("deepseek", "openai_compatible")] {
            let model = registry
                .build(
                    id,
                    &ProviderConfig {
                        api_type: Some(api_type.into()),
                        base_url: Some("https://example.invalid".into()),
                        api_key: Some("k".into()),
                        reasoning_passback: true,
                        ..Default::default()
                    },
                )
                .unwrap_or_else(|e| panic!("{api_type}: {e}"));
            assert_eq!(model.provenance(), Some(id));
        }
    }
}
