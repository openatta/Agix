//! 模型失败交给恢复策略时的形状：原样带过去，不再分类。

use crate::turn::request::model_failure;

use base::interface::model::{ModelError, ModelErrorKind};
use std::time::Duration;

#[test]
fn the_clients_classification_reaches_the_policy_unchanged() {
    let error = ModelError::new(ModelErrorKind::RateLimited, "slow down")
        .with_status(429)
        .with_retry_after(Some(Duration::from_secs(30)));
    let failure = model_failure(&error);
    assert_eq!(failure.kind, ModelErrorKind::RateLimited);
    assert_eq!(failure.message, "slow down");
    assert_eq!(failure.retry_after, Some(Duration::from_secs(30)));
}

#[test]
fn wording_alone_does_not_make_a_size_refusal() {
    let error = ModelError::protocol("prompt is too long");
    assert_eq!(model_failure(&error).kind, ModelErrorKind::Protocol);
}
