//! MCP prompts → slash commands.
//!
//! An MCP server's `prompts/list` entries are exposed to the user as slash
//! commands named `mcp__<server>__<prompt>` (the same names the tool
//! adapters get — see [`crate::naming`]). Invoking one runs `prompts/get`
//! on the owning server and the rendered messages are injected into the
//! conversation as the user turn's content.
//!
//! # Argument syntax
//!
//! MCP prompts declare *named* arguments (`McpPromptArg`), but a slash command
//! carries one free-form argument string. The convention implemented here:
//!
//! ```text
//! /mcp__github__review_pr repo=acme/widgets pr=42
//! /mcp__github__review_pr repo="acme/widgets" note='needs a second look'
//! ```
//!
//! - `name=value` pairs, whitespace-separated. `name` must be one of the
//!   prompt's declared argument names — an unknown name is an error, not a
//!   silently-forwarded key.
//! - Values may be wrapped in `"` or `'` to contain whitespace or `=`.
//! - **Single-argument shorthand**: when the prompt declares exactly one
//!   argument and the text does not already start with `<that name>=`, the
//!   entire argument string is that argument's value. So
//!   `/mcp__docs__search how do I install` == `query=how do I install`.
//! - Every declared argument with `required: true` must be present, otherwise
//!   the invocation fails with a message naming the missing arguments — the
//!   server is never called with a silently-empty argument set.
//!
//! Everything here is pure and synchronous so it can be unit-tested without a
//! live server; the network half lives in `McpManager::invoke_prompt_command`.

use crate::client::McpPromptArg;
use std::collections::HashMap;
use std::fmt;

/// An MCP server prompt exposed as a slash command.
#[derive(Debug, Clone)]
pub struct McpPromptEntry {
    pub server: String,
    pub name: String,
    pub description: String,
    /// Declared arguments, straight from `prompts/list`. Used to map and
    /// validate the slash command's argument string.
    pub arguments: Vec<McpPromptArg>,
}

impl McpPromptEntry {
    /// The slash command name this prompt is registered under:
    /// `mcp__<server>__<prompt>` (no leading `/`).
    pub fn command_name(&self) -> String {
        crate::naming::tool_name(&self.server, &self.name)
    }

    /// A one-line `argument_hint`-style summary for `/help` output.
    pub fn argument_hint(&self) -> String {
        self.arguments
            .iter()
            .map(|a| {
                if a.required.unwrap_or(false) {
                    format!("{}=<value>", a.name)
                } else {
                    format!("[{}=<value>]", a.name)
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Why a slash command's argument string could not be mapped onto a prompt's
/// declared arguments. Rendered to the user (and to the model) via `Display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptArgError {
    /// One or more `required: true` arguments were not supplied.
    MissingRequired {
        missing: Vec<String>,
        declared: Vec<String>,
    },
    /// A `name=value` pair named an argument the prompt does not declare.
    UnknownArgument { name: String, declared: Vec<String> },
    /// A token was neither `name=value` nor eligible for the single-argument
    /// shorthand (prompt declares 0 or 2+ arguments).
    Unparsable {
        token: String,
        declared: Vec<String>,
    },
    /// Arguments were supplied to a prompt that declares none.
    NoArgumentsAccepted { supplied: String },
    /// A quoted value was never closed.
    UnterminatedQuote { quote: char },
}

fn join_names(names: &[String]) -> String {
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    }
}

impl fmt::Display for PromptArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromptArgError::MissingRequired { missing, declared } => write!(
                f,
                "missing required argument{}: {}. This prompt accepts: {}. \
                 Use `name=value` pairs, e.g. `{}=<value>`.",
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", "),
                join_names(declared),
                missing.first().map(String::as_str).unwrap_or("arg"),
            ),
            PromptArgError::UnknownArgument { name, declared } => write!(
                f,
                "unknown argument `{name}`. This prompt accepts: {}.",
                join_names(declared)
            ),
            PromptArgError::Unparsable { token, declared } => write!(
                f,
                "could not parse `{token}` as an argument. Use `name=value` \
                 pairs. This prompt accepts: {}.",
                join_names(declared)
            ),
            PromptArgError::NoArgumentsAccepted { supplied } => write!(
                f,
                "this prompt takes no arguments, but `{supplied}` was supplied."
            ),
            PromptArgError::UnterminatedQuote { quote } => {
                write!(f, "unterminated {quote} quote in the argument string.")
            }
        }
    }
}

impl std::error::Error for PromptArgError {}

/// Split an argument string into whitespace-separated tokens, honouring `"`
/// and `'` quoting (quotes are stripped, and quoted regions may contain
/// whitespace or `=`).
fn tokenize(raw: &str) -> Result<Vec<String>, PromptArgError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for ch in raw.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => {
                started = true;
                quote = Some(ch);
            }
            None if ch.is_whitespace() => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                started = true;
                current.push(ch);
            }
        }
    }
    if let Some(q) = quote {
        return Err(PromptArgError::UnterminatedQuote { quote: q });
    }
    if started {
        tokens.push(current);
    }
    Ok(tokens)
}

/// Map a slash command's argument string onto a prompt's declared arguments.
/// See the module docs for the syntax. The returned map is exactly what gets
/// sent as `prompts/get` `arguments`.
pub fn parse_prompt_args(
    declared: &[McpPromptArg],
    raw: &str,
) -> Result<HashMap<String, String>, PromptArgError> {
    let raw = raw.trim();
    let declared_names: Vec<String> = declared.iter().map(|a| a.name.clone()).collect();

    let mut out: HashMap<String, String> = HashMap::new();

    if declared.is_empty() {
        if raw.is_empty() {
            return Ok(out);
        }
        return Err(PromptArgError::NoArgumentsAccepted {
            supplied: raw.to_string(),
        });
    }

    // Single-argument shorthand: one declared argument, and the text isn't
    // already written as `<that name>=...`. The whole string is the value.
    if declared.len() == 1 && !raw.is_empty() {
        let only = &declared[0].name;
        let is_explicit = raw
            .strip_prefix(only.as_str())
            .is_some_and(|rest| rest.starts_with('='));
        if !is_explicit {
            out.insert(only.clone(), raw.to_string());
            return Ok(out);
        }
    }

    for token in tokenize(raw)? {
        let Some((name, value)) = token.split_once('=') else {
            return Err(PromptArgError::Unparsable {
                token,
                declared: declared_names,
            });
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(PromptArgError::Unparsable {
                token: token.clone(),
                declared: declared_names,
            });
        }
        if !declared_names.iter().any(|d| d == name) {
            return Err(PromptArgError::UnknownArgument {
                name: name.to_string(),
                declared: declared_names,
            });
        }
        out.insert(name.to_string(), value.to_string());
    }

    let missing: Vec<String> = declared
        .iter()
        .filter(|a| a.required.unwrap_or(false) && !out.contains_key(&a.name))
        .map(|a| a.name.clone())
        .collect();
    if !missing.is_empty() {
        return Err(PromptArgError::MissingRequired {
            missing,
            declared: declared_names,
        });
    }

    Ok(out)
}

/// The outcome of invoking an MCP prompt slash command: text ready to be
/// pushed as the user turn's content. Failures are *also* text — an
/// unreachable server or a prompt that errors must never fail the turn, it
/// must produce something the model can see and explain.
#[derive(Debug, Clone)]
pub struct PromptInvocation {
    pub text: String,
    pub is_error: bool,
}

/// Wrap a successful `prompts/get` rendering with the same
/// `<command-message>`/`<command-name>` provenance header skill slash
/// commands use (see `runtime::commands::expand_skill_for_command`), so the
/// model can tell injected prompt content from something the user typed.
pub fn render_prompt_success(command: &str, raw_args: &str, body: &str) -> String {
    format!(
        "\n<command-message>{command} is running...</command-message>\n\
         <command-name>{command}{suffix}</command-name>\n\
         {body}",
        suffix = if raw_args.trim().is_empty() {
            String::new()
        } else {
            format!(" {}", raw_args.trim())
        },
    )
}

/// Render a failure as visible text. The turn continues with this as its
/// content so the model can relay the problem to the user.
pub fn render_prompt_failure(command: &str, reason: &str) -> String {
    format!(
        "\n<command-message>{command} failed</command-message>\n\
         <command-name>{command}</command-name>\n\
         The MCP prompt `/{command}` could not be run: {reason}\n\
         Tell the user what went wrong; do not attempt to guess the prompt's content."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(name: &str, required: bool) -> McpPromptArg {
        McpPromptArg {
            name: name.into(),
            description: None,
            required: Some(required),
        }
    }

    fn entry() -> McpPromptEntry {
        McpPromptEntry {
            server: "github".into(),
            name: "review_pr".into(),
            description: "Review a PR".into(),
            arguments: vec![arg("repo", true), arg("pr", true), arg("note", false)],
        }
    }

    #[test]
    fn command_name_uses_mcp_double_underscore_scheme() {
        assert_eq!(entry().command_name(), "mcp__github__review_pr");
    }

    #[test]
    fn an_odd_server_name_makes_the_command_its_tools_are_named_like() {
        let mut e = entry();
        e.server = "issue tracker".into();
        assert_eq!(
            e.command_name(),
            crate::naming::tool_name("issue tracker", "review_pr")
        );
    }

    #[test]
    fn named_pairs_are_mapped() {
        let d = entry().arguments;
        let got = parse_prompt_args(&d, "repo=acme/widgets pr=42").unwrap();
        assert_eq!(got.get("repo").unwrap(), "acme/widgets");
        assert_eq!(got.get("pr").unwrap(), "42");
        assert!(!got.contains_key("note"));
    }

    #[test]
    fn quoted_values_may_contain_spaces_and_equals() {
        let d = entry().arguments;
        let got =
            parse_prompt_args(&d, "repo=a/b pr=1 note=\"needs a second look = maybe\"").unwrap();
        assert_eq!(got.get("note").unwrap(), "needs a second look = maybe");
        let got = parse_prompt_args(&d, "repo=a/b pr=1 note='single quoted'").unwrap();
        assert_eq!(got.get("note").unwrap(), "single quoted");
    }

    #[test]
    fn unterminated_quote_is_an_error() {
        let d = entry().arguments;
        let err = parse_prompt_args(&d, "repo=a/b pr=1 note=\"oops").unwrap_err();
        assert!(matches!(err, PromptArgError::UnterminatedQuote { .. }));
    }

    #[test]
    fn missing_required_argument_is_a_clear_error_not_an_empty_call() {
        let d = entry().arguments;
        let err = parse_prompt_args(&d, "repo=acme/widgets").unwrap_err();
        match &err {
            PromptArgError::MissingRequired { missing, .. } => assert_eq!(missing, &["pr"]),
            other => panic!("wrong error: {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("missing required argument: pr"), "{msg}");
        assert!(msg.contains("repo, pr, note"), "{msg}");

        // The empty argument string is the important case: it must NOT
        // silently produce an empty, "successful" argument map.
        let err = parse_prompt_args(&d, "").unwrap_err();
        match err {
            PromptArgError::MissingRequired { missing, .. } => {
                assert_eq!(missing, &["repo", "pr"])
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn unknown_argument_is_rejected_rather_than_passed_through() {
        let d = entry().arguments;
        let err = parse_prompt_args(&d, "repo=a/b pr=1 branch=main").unwrap_err();
        match &err {
            PromptArgError::UnknownArgument { name, .. } => assert_eq!(name, "branch"),
            other => panic!("wrong error: {other:?}"),
        }
        assert!(err.to_string().contains("unknown argument `branch`"));
    }

    #[test]
    fn bare_token_with_multiple_declared_args_is_unparsable() {
        let d = entry().arguments;
        let err = parse_prompt_args(&d, "acme/widgets").unwrap_err();
        assert!(matches!(err, PromptArgError::Unparsable { .. }));
    }

    #[test]
    fn single_argument_shorthand_takes_the_whole_string() {
        let d = vec![arg("query", true)];
        let got = parse_prompt_args(&d, "how do I install this thing").unwrap();
        assert_eq!(got.get("query").unwrap(), "how do I install this thing");
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn single_argument_explicit_form_still_works() {
        let d = vec![arg("query", true)];
        let got = parse_prompt_args(&d, "query=install").unwrap();
        assert_eq!(got.get("query").unwrap(), "install");
    }

    #[test]
    fn single_optional_argument_may_be_omitted() {
        let d = vec![arg("query", false)];
        assert!(parse_prompt_args(&d, "").unwrap().is_empty());
    }

    #[test]
    fn prompt_without_arguments_rejects_arguments() {
        assert!(parse_prompt_args(&[], "").unwrap().is_empty());
        let err = parse_prompt_args(&[], "stuff").unwrap_err();
        assert!(matches!(err, PromptArgError::NoArgumentsAccepted { .. }));
    }

    #[test]
    fn argument_hint_marks_optionality() {
        assert_eq!(
            entry().argument_hint(),
            "repo=<value> pr=<value> [note=<value>]"
        );
    }

    #[test]
    fn rendered_success_carries_provenance() {
        let text = render_prompt_success("mcp__github__review_pr", "repo=a/b", "BODY");
        assert!(text.contains("<command-name>mcp__github__review_pr repo=a/b</command-name>"));
        assert!(text.contains("BODY"));
    }

    #[test]
    fn rendered_failure_is_visible_text() {
        let text = render_prompt_failure("mcp__github__review_pr", "server is down");
        assert!(text.contains("could not be run: server is down"));
    }
}
