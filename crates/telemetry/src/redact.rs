//! Redaction policy: what is replaced before telemetry events leave the
//! process. Error messages are the only free text an event carries, so the
//! policy is a single switch; [`sanitize_path`] is the helper for callers that
//! keep a path but not the username in it.

#[derive(Debug, Clone, Copy)]
pub struct RedactionPolicy {
    pub redact_error_messages: bool,
}

impl RedactionPolicy {
    pub fn none() -> Self {
        Self {
            redact_error_messages: false,
        }
    }

    pub fn all() -> Self {
        Self {
            redact_error_messages: true,
        }
    }

    pub fn scrub_message(&self, message: &str) -> String {
        match (self.redact_error_messages, message.is_empty()) {
            (_, true) => String::new(),
            (true, false) => "[REDACTED]".to_string(),
            (false, false) => message.to_string(),
        }
    }
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        Self::all()
    }
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
    fn a_message_is_replaced_whole_or_passed_through() {
        let message = "mail ops@example.com about /Users/me/x";
        assert_eq!(RedactionPolicy::all().scrub_message(message), "[REDACTED]");
        assert_eq!(RedactionPolicy::none().scrub_message(message), message);
        assert_eq!(RedactionPolicy::all().scrub_message(""), "");
    }
}
