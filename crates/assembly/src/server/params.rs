//! 处理函数共用的参数读法：单个参数、`options` 的宽松读取与 `ignored_options` 报告。

use super::*;

/// `params[name]`, parsed; the message says which of missing or malformed.
pub(super) fn parse_param<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
    name: &str,
) -> Result<T, String> {
    match params.get(name).cloned().map(serde_json::from_value) {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(format!("bad {name}: {e}")),
        None => Err(format!("missing {name}")),
    }
}

/// `params.options` as [`crate::rpc::SessionOptions::from_params`] reads it,
/// with a warning for each field it left out — or, when the options ask for
/// `strict`, an `INVALID_PARAMS` response naming them instead.
#[allow(clippy::result_large_err)]
pub(super) fn read_options(
    method: &str,
    id: &serde_json::Value,
    params: &serde_json::Value,
) -> Result<
    (
        Option<crate::rpc::SessionOptions>,
        Vec<crate::rpc::IgnoredOption>,
    ),
    RpcResponse,
> {
    let (options, ignored) = crate::rpc::SessionOptions::from_params(params);
    if !ignored.is_empty() && crate::rpc::SessionOptions::strict_requested(params) {
        let fields: Vec<&str> = ignored
            .iter()
            .map(|i| i.field.as_deref().unwrap_or("options"))
            .collect();
        return Err(RpcResponse::err_with_data(
            id.clone(),
            codes::INVALID_PARAMS,
            format!("options that do not read: {}", fields.join(", ")),
            serde_json::json!({ "invalid_options": ignored }),
        ));
    }
    for left_out in &ignored {
        warn!(
            method,
            field = left_out.field.as_deref().unwrap_or("options"),
            error = %left_out.error,
            "ignored part of `options`"
        );
    }
    Ok((options, ignored))
}

/// `resp` with what `options` left out listed in its result as
/// `ignored_options`.
pub(super) fn reporting_ignored(
    mut resp: RpcResponse,
    ignored: Vec<crate::rpc::IgnoredOption>,
) -> RpcResponse {
    if ignored.is_empty() {
        return resp;
    }
    if let Some(result) = resp.result.as_mut().and_then(|r| r.as_object_mut()) {
        result.insert(
            "ignored_options".to_string(),
            serde_json::to_value(ignored).unwrap_or_default(),
        );
    }
    resp
}
