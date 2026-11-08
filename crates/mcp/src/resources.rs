//! MCP resources → `@` references in user messages.
//!
//! # Syntax
//!
//! ```text
//! @<server>:<scheme>://<path>
//! ```
//!
//! e.g. `@github:repo://acme/widgets/README.md`, `@docs:file:///etc/hosts`,
//! `@db:postgres://main/schema`. The `<server>` segment names a connected MCP
//! server; everything after the first `:` is the resource URI passed verbatim
//! to `resources/read`.
//!
//! # Why this shape
//!
//! `@` is heavily overloaded in text people paste into a chat, so the matcher
//! is deliberately narrow. All three parts are required:
//!
//! 1. The `@` must start a "word" — preceded by start-of-input, whitespace, or
//!    one of `([{<"'` — which alone rules out `user@example.com`,
//!    `foo@bar.baz`, and git's `HEAD@{1}`-style suffixes.
//! 2. A server segment of `[A-Za-z0-9_.-]+` followed by `:` — which rules out
//!    `@Component` / `@Override` (Java/TS decorators), `@scope/package` (npm),
//!    `@media` / `@import` (CSS), and `@param` (doc tags).
//! 3. A `<scheme>://` immediately after that `:` — which rules out
//!    `@user:pass` style tokens and bare `@label:value` annotations.
//!
//! Rust lifetimes (`&'a`, `<'a>`) contain no `@` at all and can't match.
//!
//! A reference that *looks* right but names an unknown server, or whose URI
//! the server rejects, resolves to a visible error block rather than failing
//! the turn.

use crate::client::McpContent;

/// Longest inlined body per resolved resource. A single MCP resource can be a
/// whole file or database dump; without a cap one `@` reference could displace
/// the rest of the conversation.
pub const MAX_RESOURCE_BYTES: usize = 20_000;

/// Longest combined inlined body across every reference in one message.
pub const MAX_TOTAL_RESOURCE_BYTES: usize = 60_000;

/// One `@server:uri` reference located in a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRef {
    /// The exact matched text, including the leading `@`.
    pub raw: String,
    pub server: String,
    pub uri: String,
}

fn is_server_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
}

fn is_scheme_start(c: char) -> bool {
    c.is_ascii_alphabetic()
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'
}

/// Characters that end a URI when they appear at its very end — trailing
/// sentence punctuation shouldn't be swallowed into the URI.
fn is_trailing_punctuation(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '"' | '\''
    )
}

/// Scan `text` for `@server:scheme://path` references. Returns them in order
/// of appearance, de-duplicated by `(server, uri)`.
pub fn find_resource_refs(text: &str) -> Vec<ResourceRef> {
    let bytes = text.as_bytes();
    let mut out: Vec<ResourceRef> = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        // Rule 1: the `@` must start a word.
        let preceded_ok = if i == 0 {
            true
        } else {
            let prev = text[..i].chars().next_back().unwrap();
            prev.is_whitespace() || matches!(prev, '(' | '[' | '{' | '<' | '"' | '\'')
        };
        if !preceded_ok {
            i += 1;
            continue;
        }

        let rest = &text[i + 1..];

        // Rule 2: server segment then `:`.
        let server_len = rest
            .find(|c: char| !is_server_char(c))
            .unwrap_or(rest.len());
        if server_len == 0 || !rest[server_len..].starts_with(':') {
            i += 1;
            continue;
        }
        let server = &rest[..server_len];
        let after_colon = &rest[server_len + 1..];

        // Rule 3: `scheme://` immediately after the `:`.
        let scheme_len = after_colon
            .find(|c: char| !is_scheme_char(c))
            .unwrap_or(after_colon.len());
        if scheme_len == 0
            || !after_colon.starts_with(is_scheme_start)
            || !after_colon[scheme_len..].starts_with("://")
        {
            i += 1;
            continue;
        }

        // The URI runs to the next whitespace, minus trailing punctuation.
        let uri_start = server_len + 1; // relative to `rest`
        let uri_end_rel = rest[uri_start..]
            .find(char::is_whitespace)
            .unwrap_or(rest.len() - uri_start);
        let mut uri = &rest[uri_start..uri_start + uri_end_rel];
        while let Some(last) = uri.chars().next_back() {
            if is_trailing_punctuation(last) {
                uri = &uri[..uri.len() - last.len_utf8()];
            } else {
                break;
            }
        }
        // A URI must have something after `scheme://`.
        if uri.len() <= scheme_len + 3 {
            i += 1;
            continue;
        }

        let raw = format!("@{server}:{uri}");
        let consumed = raw.len();
        if !out.iter().any(|r| r.server == server && r.uri == uri) {
            out.push(ResourceRef {
                raw,
                server: server.to_string(),
                uri: uri.to_string(),
            });
        }
        i += consumed;
    }

    out
}

/// Flatten `resources/read` content blocks into text. Non-text blocks become
/// short placeholders — the `@`-reference path inlines into a text message,
/// so there is nowhere for binary content to go.
pub fn flatten_resource_content(content: &[McpContent]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(content.len());
    for block in content {
        match block {
            McpContent::Text(t) => parts.push(t.clone()),
            McpContent::Image { media_type, data } => parts.push(format!(
                "[image resource omitted: {media_type}, {} base64 bytes]",
                data.len()
            )),
            McpContent::Other(v) => parts.push(format!("[non-text resource block: {v}]")),
        }
    }
    parts.join("\n")
}

/// Render one resolved resource as an inlinable block.
pub fn render_resource_block(server: &str, uri: &str, body: &str, truncated: bool) -> String {
    format!(
        "<mcp-resource server=\"{server}\" uri=\"{uri}\">\n{body}{note}\n</mcp-resource>",
        note = if truncated {
            "\n[... truncated: resource exceeded the inline size limit]"
        } else {
            ""
        }
    )
}

/// Render a reference that could not be resolved. Visible text, never an
/// error that fails the turn.
pub fn render_resource_error(server: &str, uri: &str, reason: &str) -> String {
    format!(
        "<mcp-resource server=\"{server}\" uri=\"{uri}\" error=\"true\">\n\
         Could not read this MCP resource: {reason}\n\
         </mcp-resource>"
    )
}

/// Wrap the per-reference blocks into the section appended to the user
/// message.
pub fn wrap_resource_blocks(blocks: &[String]) -> String {
    format!(
        "\n\n<mcp-resources>\nThe user's message referenced these MCP resources; \
         their contents are inlined below.\n\n{}\n</mcp-resources>",
        blocks.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(s: &str) -> Vec<(String, String)> {
        find_resource_refs(s)
            .into_iter()
            .map(|r| (r.server, r.uri))
            .collect()
    }

    #[test]
    fn matches_a_well_formed_reference() {
        assert_eq!(
            refs("please read @github:repo://acme/widgets/README.md now"),
            vec![(
                "github".to_string(),
                "repo://acme/widgets/README.md".to_string()
            )]
        );
    }

    #[test]
    fn matches_at_start_and_end_of_input() {
        assert_eq!(
            refs("@docs:file:///a/b"),
            vec![("docs".to_string(), "file:///a/b".to_string())]
        );
        assert_eq!(
            refs("look at @docs:file:///a/b"),
            vec![("docs".to_string(), "file:///a/b".to_string())]
        );
    }

    #[test]
    fn trailing_sentence_punctuation_is_not_part_of_the_uri() {
        assert_eq!(
            refs("see @docs:file:///a/b."),
            vec![("docs".to_string(), "file:///a/b".to_string())]
        );
        assert_eq!(
            refs("see (@docs:file:///a/b), ok"),
            vec![("docs".to_string(), "file:///a/b".to_string())]
        );
    }

    #[test]
    fn multiple_refs_are_found_and_deduplicated() {
        let got = refs("@a:x://1 and @b:y://2 and @a:x://1 again");
        assert_eq!(
            got,
            vec![
                ("a".to_string(), "x://1".to_string()),
                ("b".to_string(), "y://2".to_string()),
            ]
        );
    }

    #[test]
    fn email_addresses_do_not_match() {
        assert!(refs("mail me at xbitshans@gmail.com").is_empty());
        assert!(refs("first.last@sub.example.co.uk").is_empty());
        // Even an email whose domain looks scheme-ish.
        assert!(refs("someone@http://example.com").is_empty());
    }

    #[test]
    fn decorators_do_not_match() {
        assert!(refs("@Component({ selector: 'app' })").is_empty());
        assert!(refs("@Override\npublic void run() {}").is_empty());
        assert!(refs("use @property and @staticmethod").is_empty());
        assert!(refs("/// @param name the name").is_empty());
    }

    #[test]
    fn rust_lifetimes_do_not_match() {
        assert!(refs("fn f<'a>(x: &'a str) -> &'a str { x }").is_empty());
        assert!(refs("struct S<'de> { v: &'de [u8] }").is_empty());
    }

    #[test]
    fn npm_scopes_and_css_at_rules_do_not_match() {
        assert!(refs("npm i @anthropic-ai/sdk @types/node").is_empty());
        assert!(refs("@media (min-width: 600px) { }").is_empty());
        assert!(refs("@import url(\"x.css\");").is_empty());
    }

    #[test]
    fn server_colon_without_a_scheme_does_not_match() {
        assert!(refs("@todo: fix this").is_empty());
        assert!(refs("@note:something").is_empty());
        assert!(refs("@user:password").is_empty());
        // scheme present but no `//`
        assert!(refs("@srv:mailto:someone").is_empty());
    }

    #[test]
    fn empty_path_after_scheme_does_not_match() {
        assert!(refs("@srv:file://").is_empty());
    }

    #[test]
    fn social_handles_do_not_match() {
        assert!(refs("cc @alice and @bob-smith on this").is_empty());
    }

    #[test]
    fn flatten_handles_mixed_content() {
        let flat = flatten_resource_content(&[
            McpContent::Text("hello".into()),
            McpContent::Image {
                data: "AAAA".into(),
                media_type: "image/png".into(),
            },
        ]);
        assert!(flat.starts_with("hello\n"));
        assert!(flat.contains("[image resource omitted: image/png"));
    }

    #[test]
    fn error_block_is_plain_visible_text() {
        let block = render_resource_error("ghost", "x://y", "server not connected");
        assert!(block.contains("error=\"true\""));
        assert!(block.contains("server not connected"));
    }
}
