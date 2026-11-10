//! `redact_tests` 原样搬来的用例。

use super::{redact_secret, redact_settings_secrets};

#[test]
fn mcp_headers_and_env_are_redacted_whatever_their_keys() {
    let mut settings = serde_json::json!({
        "mcp_servers": {
            "remote": {"type": "http", "url": "https://x",
                       "headers": {"Authorization": "Bearer sk-live-9999", "X-Trace": "on-1234"}},
            "local": {"type": "stdio", "command": "srv", "env": {"GITHUB_PAT": "ghp_abcd5678"}}
        },
        "providers": {"p": {"API_KEY": "sk-upper-4321", "Client_Secret": "cs-0000"}},
        "model": {"model_name": "keep-me"}
    });
    redact_settings_secrets(&mut settings);
    let dumped = settings.to_string();
    for leaked in ["sk-live", "on-1", "ghp_", "sk-upper", "cs-"] {
        assert!(!dumped.contains(leaked), "{leaked} in {dumped}");
    }
    assert_eq!(settings["model"]["model_name"], "keep-me");
    assert_eq!(settings["mcp_servers"]["remote"]["url"], "https://x");
    assert_eq!(settings["mcp_servers"]["local"]["command"], "srv");
}

#[test]
fn redacts_long_secret_keeping_last_four() {
    assert_eq!(redact_secret("sk-ant-abcdef1234"), "***1234");
}

#[test]
fn short_secret_fully_redacted() {
    assert_eq!(redact_secret("abcd"), "***");
    assert_eq!(redact_secret(""), "***");
}

#[test]
fn handles_multibyte_chars_without_panicking() {
    // 4-byte-per-char emoji tail — a byte-slice `&s[len-4..]` would
    // panic here; char-based slicing must not.
    let s = "sk-😀😀😀😀";
    assert_eq!(redact_secret(s), "***😀😀😀😀");
}
