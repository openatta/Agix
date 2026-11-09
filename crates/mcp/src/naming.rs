//! The names an MCP server's tools and prompts go by: `mcp__<server>__<tool>`,
//! made from the configured server name and the server's own tool name.
//!
//! A model API takes tool names matching `^[a-zA-Z0-9_-]{1,64}$` and refuses
//! the whole request over one that does not, while server names are settings
//! keys and tool names are whatever a server sends. So each part is made to
//! fit, deterministically and from that part alone, which keeps a permission
//! rule `mcp__<server>` or `mcp__<server>__<tool>` naming the same tools in
//! every session and on every connect:
//!
//! - A server name that already fits (those characters, at most
//!   [`MAX_SERVER_SEGMENT`] long) is used as it is. Any other has every
//!   other character replaced by `_`, is cut to fit, and ends in `_` plus six
//!   hex digits of its SHA-256, so two names that differ only in what was
//!   replaced still differ.
//! - A tool or prompt name has the same characters replaced. When the whole
//!   name would pass 64 characters its tool part is cut and ends in the same
//!   kind of hash; two tools of one server that come out the same are told
//!   apart by [`tool_names`].

use sha2::{Digest, Sha256};

pub const MAX_TOOL_NAME: usize = 64;
pub const MAX_SERVER_SEGMENT: usize = 40;
const PREFIX: &str = "mcp__";
const SEPARATOR: &str = "__";
const HASH_HEX: usize = 6;

fn allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn sanitize(raw: &str) -> String {
    if raw.is_empty() {
        return "_".into();
    }
    raw.chars()
        .map(|c| if allowed(c) { c } else { '_' })
        .collect()
}

fn hash_suffix(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    let mut hex = hex::encode(digest);
    hex.truncate(HASH_HEX);
    format!("_{hex}")
}

/// `sanitized` cut to `max` with `raw`'s hash at its end.
fn hashed(sanitized: &str, raw: &str, max: usize) -> String {
    let suffix = hash_suffix(raw);
    let keep = max.saturating_sub(suffix.len());
    let mut out: String = sanitized.chars().take(keep).collect();
    out.push_str(&suffix);
    out
}

/// The `<server>` in `mcp__<server>__<tool>`.
pub fn server_segment(server: &str) -> String {
    if !server.is_empty() && server.len() <= MAX_SERVER_SEGMENT && server.chars().all(allowed) {
        return server.to_string();
    }
    hashed(&sanitize(server), server, MAX_SERVER_SEGMENT)
}

/// `mcp__<server>__<tool>` for one tool or prompt of `server`.
pub fn tool_name(server: &str, tool: &str) -> String {
    let head = format!("{PREFIX}{}{SEPARATOR}", server_segment(server));
    let tool_part = sanitize(tool);
    let room = MAX_TOOL_NAME - head.len();
    if tool_part.len() <= room {
        format!("{head}{tool_part}")
    } else {
        format!("{head}{}", hashed(&tool_part, tool, room))
    }
}

/// [`tool_name`] for each of one server's `tools`, in order. A tool whose
/// name comes out the same as an earlier, differently named tool's gets its
/// own name's hash instead, so neither shadows the other; a tool listed
/// twice under one name keeps that name both times.
pub fn tool_names<'a>(server: &str, tools: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut taken: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for tool in tools {
        let mut name = tool_name(server, tool);
        if taken.get(&name).is_some_and(|first| *first != tool) {
            let head_len = PREFIX.len() + server_segment(server).len() + SEPARATOR.len();
            let room = MAX_TOOL_NAME - head_len;
            name = format!(
                "{}{}",
                &name[..head_len],
                hashed(&sanitize(tool), tool, room)
            );
        }
        taken.entry(name.clone()).or_insert(tool);
        out.push(name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_accepts(name: &str) -> bool {
        (1..=MAX_TOOL_NAME).contains(&name.len()) && name.chars().all(allowed)
    }

    #[test]
    fn a_name_that_already_fits_is_left_alone() {
        assert_eq!(
            tool_name("github", "create_issue"),
            "mcp__github__create_issue"
        );
        assert_eq!(tool_name("my-srv", "a-b"), "mcp__my-srv__a-b");
    }

    #[test]
    fn odd_server_names_fit_and_stay_apart() {
        let names: Vec<String> = [
            "with space",
            "with.space",
            "ünïcode",
            "",
            &"long".repeat(16),
        ]
        .iter()
        .map(|s| tool_name(s, "ping"))
        .collect();
        for n in &names {
            assert!(api_accepts(n), "{n}");
            assert!(n.ends_with("__ping"), "{n}");
        }
        assert!(names[0].starts_with("mcp__with_space_"), "{}", names[0]);
        assert_ne!(names[0], names[1]);
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn the_same_name_always_comes_out_the_same() {
        assert_eq!(server_segment("dot.ted"), server_segment("dot.ted"));
        assert_eq!(tool_name("dot.ted", "x"), tool_name("dot.ted", "x"));
    }

    #[test]
    fn a_long_tool_name_is_cut_to_the_limit_and_keeps_its_server() {
        let long = "t".repeat(100);
        let other = format!("{}u", "t".repeat(99));
        let a = tool_name("github", &long);
        let b = tool_name("github", &other);
        assert!(api_accepts(&a), "{a}");
        assert_eq!(a.len(), MAX_TOOL_NAME);
        assert!(a.starts_with("mcp__github__"));
        assert_ne!(a, b);
    }

    #[test]
    fn two_tools_of_one_server_that_sanitize_alike_get_different_names() {
        let names = tool_names("s", ["files.read", "files read", "files.read"]);
        assert_eq!(names[0], "mcp__s__files_read");
        assert_ne!(names[1], names[0]);
        assert!(api_accepts(&names[1]), "{}", names[1]);
        assert_eq!(names[2], names[0]);
    }

    #[test]
    fn the_server_segment_is_a_prefix_a_permission_rule_can_name() {
        let seg = server_segment("dot.ted");
        for tool in ["a", &"z".repeat(80)] {
            assert!(tool_name("dot.ted", tool).starts_with(&format!("mcp__{seg}__")));
        }
    }
}
