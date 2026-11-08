//! PII redaction policy — controls which sensitive patterns are scrubbed from
//! telemetry events before export.

use regex::Regex;
use std::sync::LazyLock;

/// Match common email addresses.
#[allow(clippy::unwrap_used)]
static EMAIL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap());

/// Match IPv4 addresses.
#[allow(clippy::unwrap_used)]
static IPV4_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\b")
        .unwrap()
});

/// Match IPv6 addresses (simplified — covers most common forms).
///
/// Covers: full address (`2001:0db8:85a3:0000:0000:8a2e:0370:7334`),
/// loopback (`::1`), and various compressed forms.
///
/// Note: the Rust `regex` crate does not support lookbehind, so the
/// pattern for `::`-prefixed forms omits the leading word boundary.
/// This may (very rarely) cause a false positive inside hex-like content,
/// which is acceptable for telemetry redaction.
#[allow(clippy::unwrap_used)]
static IPV6_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:[0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}\b|", // full
        r"\b(?:[0-9a-fA-F]{1,4}:){1,7}:|",                // trailing ::
        r"\b(?:[0-9a-fA-F]{1,4}:){1,6}:[0-9a-fA-F]{1,4}\b|", // partial hex
        r"::(?:[0-9a-fA-F]{1,4}:){0,6}[0-9a-fA-F]{1,4}\b", // ::1 and other :: forms
    ))
    .unwrap()
});

/// Match paths containing home directory-like segments: `/Users/name` or `/home/name`.
#[allow(clippy::unwrap_used)]
static HOME_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(/Users/[^/\s]+|/home/[^/\s]+|C:\\Users\\[^\\\s]+)").unwrap()
});

const SENSITIVE_ENV_NAMES: &[&str] = &[
    "api_key",
    "api_secret",
    "api_token",
    "access_key",
    "secret_key",
    "secret_access_key",
    "token",
    "auth_token",
    "bearer",
    "bearer_token",
    "password",
    "passwd",
    "pwd",
    "secret",
    "secrets",
    "private_key",
    "private",
    "ssh_key",
    "signing_key",
    "signing_secret",
    "consumer_key",
    "consumer_secret",
    "client_secret",
    "client_id",
    "app_secret",
    "app_token",
    "session_token",
    "csrf_token",
    "authorization",
    "x-api-key",
    "pat",
    "github_token",
    "gitlab_token",
    "db_password",
    "database_url",
    "redis_url",
    "connection_string",
    "jwt",
    "jwt_secret",
    "jwt_key",
    "encryption_key",
    "master_key",
    "slack_token",
    "slack_secret",
    "webhook_secret",
    "webhook_token",
    "npm_token",
    "pypi_token",
    "docker_token",
    "kube_config",
    "terraform_token",
    "vault_token",
    "mfa_secret",
    "otp_secret",
    "totp_secret",
    "refresh_token",
    "id_token",
    "proxy_password",
    "proxy_auth",
    "cloudflare_api_key",
    "aws_secret",
    "gcp_secret",
    "azure_secret",
];

/// Sensitive env-var name suffix patterns (e.g., `_KEY`, `_TOKEN`, `_SECRET`, `_PASSWORD`).
const SENSITIVE_ENV_SUFFIXES: &[&str] = &[
    "_key",
    "_token",
    "_secret",
    "_password",
    "_passwd",
    "_auth",
    "_credential",
    "_cert",
];

/// Redaction policy: controls which fields and patterns are replaced with
/// `[REDACTED]` before telemetry events leave the process.
#[derive(Debug, Clone, Copy)]
pub struct RedactionPolicy {
    /// Whether to replace prompt/user text content.
    pub redact_prompts: bool,
    /// Whether to replace tool input/result content.
    pub redact_tool_content: bool,
    /// Whether to replace error message content (may contain paths/PII).
    pub redact_error_messages: bool,
    /// Whether to replace auth/secret/key related fields.
    pub redact_secrets: bool,
    /// Whether to replace file paths containing usernames
    /// (e.g. `/Users/xbits/` -> `[REDACTED_PATH]`).
    pub redact_paths: bool,
    /// Whether to replace email addresses via regex.
    pub redact_emails: bool,
    /// Whether to replace IPv4/IPv6 addresses.
    pub redact_ip_addresses: bool,
    /// Whether to strip values of known sensitive env vars
    /// (API_KEY, TOKEN, SECRET, PASSWORD, etc.).
    pub redact_env_vars: bool,
}

impl RedactionPolicy {
    /// All redactions disabled — no PII scrubbing.
    pub fn none() -> Self {
        Self {
            redact_prompts: false,
            redact_tool_content: false,
            redact_error_messages: false,
            redact_secrets: false,
            redact_paths: false,
            redact_emails: false,
            redact_ip_addresses: false,
            redact_env_vars: false,
        }
    }

    /// All redactions enabled — strictest policy.
    pub fn all() -> Self {
        Self {
            redact_prompts: true,
            redact_tool_content: true,
            redact_error_messages: true,
            redact_secrets: true,
            redact_paths: true,
            redact_emails: true,
            redact_ip_addresses: true,
            redact_env_vars: true,
        }
    }

    /// Safe defaults for production use.
    ///
    /// Enables all redactions except `redact_ip_addresses` (which could
    /// interfere with legitimate IP logs) and `redact_paths` (which uses
    /// a broad pattern that may match non-PII paths).
    pub fn recommended() -> Self {
        Self {
            redact_prompts: true,
            redact_tool_content: true,
            redact_error_messages: true,
            redact_secrets: true,
            redact_paths: false,
            redact_emails: true,
            redact_ip_addresses: false,
            redact_env_vars: true,
        }
    }

    /// Apply all enabled redaction rules to a string.
    ///
    /// This is called on each field that may contain PII. The order of
    /// rules is: emails → IPs → paths → env vars.
    pub fn apply(&self, input: &str) -> String {
        if input.is_empty() {
            return input.to_string();
        }

        let mut result = input.to_string();

        if self.redact_emails {
            result = EMAIL_RE
                .replace_all(&result, "[REDACTED_EMAIL]")
                .to_string();
        }

        if self.redact_ip_addresses {
            result = IPV4_RE.replace_all(&result, "[REDACTED_IP]").to_string();
            result = IPV6_RE.replace_all(&result, "[REDACTED_IP]").to_string();
        }

        if self.redact_paths {
            result = HOME_PATH_RE
                .replace_all(&result, "[REDACTED_PATH]")
                .to_string();
        }

        if self.redact_env_vars {
            result = redact_env_var_values(&result);
        }

        result
    }

    /// Check if any content-level redaction is active.
    pub fn has_content_redaction(&self) -> bool {
        self.redact_prompts || self.redact_tool_content
    }
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        Self {
            redact_prompts: true,
            redact_tool_content: true,
            redact_error_messages: true,
            redact_secrets: true,
            redact_paths: false,
            redact_emails: true,
            redact_ip_addresses: false,
            redact_env_vars: true,
        }
    }
}

/// Scan a string for `KEY=VALUE` patterns where the key matches a known
/// sensitive variable name, and replace the value portion with `[REDACTED]`.
fn redact_env_var_values(input: &str) -> String {
    // We use a simple state-machine approach:
    // 1. Split on newlines first, then on '=' within each line
    // 2. For each `key=value` pair, check if key matches a sensitive pattern
    let mut result = String::with_capacity(input.len());

    for line in input.split('\n') {
        if let Some(eq_pos) = line.find('=') {
            let var_name = &line[..eq_pos];
            let trimmed = var_name.trim();

            if is_sensitive_env_name(trimmed) {
                // Redact the value portion after '='
                result.push_str(&line[..=eq_pos]);
                result.push_str("[REDACTED_ENV]");
            } else {
                result.push_str(line);
            }
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    // Remove trailing newline if input didn't have one
    if !input.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }

    result
}

/// Check if an environment variable name is considered sensitive.
fn is_sensitive_env_name(name: &str) -> bool {
    let lower = name.to_lowercase();

    // Exact match against known sensitive variable names
    if SENSITIVE_ENV_NAMES.contains(&lower.as_str()) {
        return true;
    }

    // Suffix matching: e.g. `OPENAI_API_KEY`, `DB_PASSWORD`, `GITHUB_TOKEN`
    for suffix in SENSITIVE_ENV_SUFFIXES {
        if lower.ends_with(suffix) {
            return true;
        }
    }

    false
}

/// Replace home directory paths with `~`.
///
/// - `/Users/xbits/project/file.rs` → `~/project/file.rs`
/// - `/home/alice/.config/app.toml` → `~/.config/app.toml`
/// - `C:\Users\bob\Documents\notes.txt` → `~\Documents\notes.txt`
///
/// Returns the input unchanged if it does not contain a recognised home pattern.
pub fn sanitize_path(path: &str) -> String {
    // Unix: /Users/<username>/... or /home/<username>/...
    if let Some(rest) = path
        .strip_prefix("/Users/")
        .or_else(|| path.strip_prefix("/home/"))
    {
        // Skip the username: find the next '/' after the username
        if let Some(pos) = rest.find('/') {
            return format!("~{}", &rest[pos..]);
        }
        // No further path after username (e.g., just "/Users/bob")
        return "~".to_string();
    }

    // Windows: C:\Users\<username>\...
    // Match patterns like C:\Users\bob\... or D:\Users\alice\...
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        let prefix = &bytes[0..2]; // e.g. "C:"
        let rest = &bytes[3..]; // skip "C:\"
        if rest.len() >= 6
            && (rest[0..6].eq_ignore_ascii_case(b"Users\\")
                || rest[0..6].eq_ignore_ascii_case(b"Users/"))
        {
            let after_users = &rest[6..]; // skip "Users\"
            if let Some(pos) = after_users.iter().position(|&b| b == b'\\' || b == b'/') {
                let remaining = std::str::from_utf8(&after_users[pos..]).unwrap_or("");
                return format!(
                    "{prefix}~{remaining}",
                    prefix = std::str::from_utf8(prefix).unwrap_or("C:")
                );
            }
            return format!("{}~", std::str::from_utf8(prefix).unwrap_or("C:"));
        }
    }

    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_email_simple() {
        let policy = RedactionPolicy {
            redact_emails: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("contact alice@example.com for info");
        assert_eq!(result, "contact [REDACTED_EMAIL] for info");
    }

    #[test]
    fn redact_email_multiple() {
        let policy = RedactionPolicy {
            redact_emails: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("alice@a.com, bob@b.org, charlie@c.net");
        assert_eq!(
            result,
            "[REDACTED_EMAIL], [REDACTED_EMAIL], [REDACTED_EMAIL]"
        );
    }

    #[test]
    fn redact_email_with_plus() {
        let policy = RedactionPolicy {
            redact_emails: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("email: alice+tag@example.co.uk");
        assert_eq!(result, "email: [REDACTED_EMAIL]");
    }

    #[test]
    fn no_email_redaction_when_disabled() {
        let policy = RedactionPolicy::none();
        let result = policy.apply("contact alice@example.com");
        assert_eq!(result, "contact alice@example.com");
    }

    #[test]
    fn redact_ipv4() {
        let policy = RedactionPolicy {
            redact_ip_addresses: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("connect from 192.168.1.1:8080");
        assert_eq!(result, "connect from [REDACTED_IP]:8080");
    }

    #[test]
    fn redact_ipv4_loopback() {
        let policy = RedactionPolicy {
            redact_ip_addresses: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("localhost is 127.0.0.1");
        assert_eq!(result, "localhost is [REDACTED_IP]");
    }

    #[test]
    fn redact_ipv6_loopback() {
        let policy = RedactionPolicy {
            redact_ip_addresses: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("ipv6: ::1");
        assert_eq!(result, "ipv6: [REDACTED_IP]");
    }

    #[test]
    fn redact_ipv6_full() {
        let policy = RedactionPolicy {
            redact_ip_addresses: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("ipv6: 2001:0db8:85a3:0000:0000:8a2e:0370:7334");
        assert!(result.contains("[REDACTED_IP]"));
    }

    #[test]
    fn no_ip_redaction_when_disabled() {
        let policy = RedactionPolicy::none();
        let result = policy.apply("ip: 192.168.1.1");
        assert_eq!(result, "ip: 192.168.1.1");
    }

    #[test]
    fn redact_users_path_unix() {
        let policy = RedactionPolicy {
            redact_paths: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("config at /Users/xbits/project/file.rs");
        assert_eq!(result, "config at [REDACTED_PATH]/project/file.rs");
    }

    #[test]
    fn redact_home_path_unix() {
        let policy = RedactionPolicy {
            redact_paths: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("stored at /home/alice/.config");
        assert_eq!(result, "stored at [REDACTED_PATH]/.config");
    }

    #[test]
    fn no_path_redaction_when_disabled() {
        let policy = RedactionPolicy::none();
        let result = policy.apply("path: /Users/bob/file.txt");
        assert_eq!(result, "path: /Users/bob/file.txt");
    }

    #[test]
    fn redact_env_var_api_key() {
        let policy = RedactionPolicy {
            redact_env_vars: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("OPENAI_API_KEY=sk-1234567890abcdef");
        assert_eq!(result, "OPENAI_API_KEY=[REDACTED_ENV]");
    }

    #[test]
    fn redact_env_var_password() {
        let policy = RedactionPolicy {
            redact_env_vars: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("DB_PASSWORD=hunter2");
        assert_eq!(result, "DB_PASSWORD=[REDACTED_ENV]");
    }

    #[test]
    fn redact_env_var_simple_token() {
        let policy = RedactionPolicy {
            redact_env_vars: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("TOKEN=abc123");
        assert_eq!(result, "TOKEN=[REDACTED_ENV]");
    }

    #[test]
    fn redact_env_var_multi_line() {
        let policy = RedactionPolicy {
            redact_env_vars: true,
            ..RedactionPolicy::none()
        };
        let input = "DB_HOST=localhost\nDB_PASSWORD=secret123\nDB_NAME=test";
        let result = policy.apply(input);
        assert!(result.contains("DB_HOST=localhost"));
        assert!(result.contains("DB_PASSWORD=[REDACTED_ENV]"));
        assert!(result.contains("DB_NAME=test"));
    }

    #[test]
    fn no_env_var_redaction_when_disabled() {
        let policy = RedactionPolicy::none();
        let result = policy.apply("API_KEY=secret");
        assert_eq!(result, "API_KEY=secret");
    }

    #[test]
    fn non_sensitive_env_var_left_alone() {
        let policy = RedactionPolicy {
            redact_env_vars: true,
            ..RedactionPolicy::none()
        };
        let result = policy.apply("MY_VAR=hello");
        assert_eq!(result, "MY_VAR=hello");
    }

    #[test]
    fn multiple_rules_apply_together() {
        let policy = RedactionPolicy {
            redact_emails: true,
            redact_ip_addresses: true,
            redact_paths: true,
            ..RedactionPolicy::none()
        };
        let input = "user@example.com from 10.0.0.1 at /Users/me/file";
        let result = policy.apply(input);
        assert_eq!(
            result,
            "[REDACTED_EMAIL] from [REDACTED_IP] at [REDACTED_PATH]/file"
        );
    }

    #[test]
    fn recommended_policy_has_expected_flags() {
        let r = RedactionPolicy::recommended();
        assert!(r.redact_prompts);
        assert!(r.redact_tool_content);
        assert!(r.redact_error_messages);
        assert!(r.redact_secrets);
        assert!(!r.redact_paths);
        assert!(r.redact_emails);
        assert!(!r.redact_ip_addresses);
        assert!(r.redact_env_vars);
    }

    #[test]
    fn sanitize_users_path() {
        assert_eq!(
            sanitize_path("/Users/xbits/project/file.rs"),
            "~/project/file.rs"
        );
    }

    #[test]
    fn sanitize_home_path() {
        assert_eq!(
            sanitize_path("/home/alice/.config/app.toml"),
            "~/.config/app.toml"
        );
    }

    #[test]
    fn sanitize_nested_users_path() {
        assert_eq!(
            sanitize_path("/Users/bob/Documents/work/notes.txt"),
            "~/Documents/work/notes.txt"
        );
    }

    #[test]
    fn sanitize_windows_path() {
        // The function handles Windows-style paths on all platforms.
        let result = sanitize_path("C:\\Users\\bob\\Documents\\notes.txt");
        assert_eq!(result, "C:~\\Documents\\notes.txt");
    }

    #[test]
    fn sanitize_non_home_path_unchanged() {
        let path = "/opt/app/config.json";
        assert_eq!(sanitize_path(path), path);
    }

    #[test]
    fn sanitize_relative_path_unchanged() {
        let path = "./src/main.rs";
        assert_eq!(sanitize_path(path), path);
    }

    #[test]
    fn sanitize_empty_string_unchanged() {
        assert_eq!(sanitize_path(""), "");
    }

    #[test]
    fn content_redation_true_when_prompts_enabled() {
        let policy = RedactionPolicy {
            redact_prompts: true,
            ..RedactionPolicy::none()
        };
        assert!(policy.has_content_redaction());
    }

    #[test]
    fn content_redation_false_when_disabled() {
        let policy = RedactionPolicy::none();
        assert!(!policy.has_content_redaction());
    }
}
