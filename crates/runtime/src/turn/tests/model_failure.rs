//! 模型失败的分类。

use crate::turn::request::model_failure;

use base::interface::model::ModelError;
use base::interface::recovery_policy::ModelFailure;

fn classify(e: &ModelError) -> ModelFailure<'_> {
    // Rendered text is only carried by `Other`; the tests below never
    // look inside it.
    model_failure(e, "")
}

/// What real providers answer a request too big for the model with.
#[test]
fn the_providers_size_refusals_are_context_too_long() {
    for refusal in [
            ModelError::Api {
                status: 400,
                message: r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 212034 tokens > 200000 maximum"}}"#.into(),
            },
            ModelError::Api {
                status: 400,
                message: "This model's maximum context length is 65536 tokens. However, you requested 70000 tokens".into(),
            },
            ModelError::Api {
                status: 413,
                message: "request entity too large".into(),
            },
            ModelError::Internal("prompt too long".into()),
        ] {
            assert!(
                matches!(classify(&refusal), ModelFailure::ContextTooLong { .. }),
                "{refusal:?}"
            );
        }
}

#[test]
fn other_bad_requests_and_server_errors_are_not() {
    for other in [
        ModelError::Api {
            status: 400,
            message: "max_tokens: must be a positive integer".into(),
        },
        ModelError::Api {
            status: 500,
            message: "prompt is too long".into(),
        },
        ModelError::Network("the response ended before the model finished it".into()),
        ModelError::RateLimited,
    ] {
        assert!(
            matches!(classify(&other), ModelFailure::Other { .. }),
            "{other:?}"
        );
    }
    assert_eq!(classify(&ModelError::Overloaded), ModelFailure::Overloaded);
}
