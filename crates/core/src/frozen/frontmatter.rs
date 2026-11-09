//! YAML frontmatter parsing for SKILL.md files and memory entries.

use std::path::Path;

use super::skill::{SkillEntry, SkillSource};

/// 把 `---\n<yaml>\n---\n<body>` 切成 (Some(yaml), body)；不带 frontmatter 时
/// 返回 (None, content)。
pub fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let bytes = content.as_bytes();
    if !content.starts_with("---") {
        return (None, content);
    }
    // 第一个 `---` 后紧跟换行
    let after_first = &content[3..];
    let after_first = after_first.strip_prefix('\n').unwrap_or(after_first);
    // 在剩余内容里找下一个 `\n---` 行
    let Some(rel) = find_closing_marker(after_first) else {
        return (None, content);
    };
    let yaml = &after_first[..rel];
    let after_close = &after_first[rel..];
    // 跳过 `---` 后的换行
    let after_close = after_close.strip_prefix("---").unwrap_or(after_close);
    let after_close = after_close.strip_prefix('\n').unwrap_or(after_close);
    let _ = bytes;
    (Some(yaml), after_close)
}

/// 找 `\n---` 之后是 \n 或 EOF 的位置；返回相对 yaml 起点的 byte offset。
fn find_closing_marker(s: &str) -> Option<usize> {
    let mut search_from = 0usize;
    while let Some(rel) = s[search_from..].find("\n---") {
        let abs = search_from + rel;
        let after = abs + 4; // `\n---` 4 字节
        let next = s.as_bytes().get(after);
        if next.is_none() || matches!(next, Some(b'\n') | Some(b' ') | Some(b'\r')) {
            // `\n---\n` 命中；返回 abs+1（跳过开头那个 `\n`）
            return Some(abs + 1);
        }
        search_from = abs + 1;
    }
    None
}

/// Fallback description when frontmatter omits it: the first markdown
/// *paragraph* of the body (consecutive non-blank lines, joined with a
/// space), not just the first line. A leading `#` heading prefix is
/// stripped from the paragraph's first line only, so a lone heading line
/// (heading, blank line, then body) still yields just the heading text.
pub fn first_paragraph(body: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for line in body.lines().skip_while(|l| l.trim().is_empty()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        let text = if parts.is_empty() {
            trimmed.trim_start_matches('#').trim()
        } else {
            trimmed
        };
        if !text.is_empty() {
            parts.push(text.to_string());
        }
    }
    parts.join(" ")
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        let last = s.as_bytes()[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Parse a YAML inline list value: `[a, b, c]`, comma-separated `a, b, c`, or
/// space-separated `a b c` — frontmatter fields like `allowed-tools`/
/// `arguments`/`skills` all accept any of the three forms.
pub fn parse_yaml_list(raw: &str) -> Vec<String> {
    let s = raw.trim();
    if s.starts_with('[') && s.ends_with(']') {
        let inner = &s[1..s.len() - 1];
        inner
            .split(',')
            .map(|item| strip_quotes(item.trim()).to_string())
            .filter(|item| !item.is_empty())
            .collect()
    } else if s.contains(',') {
        s.split(',')
            .map(|item| strip_quotes(item.trim()).to_string())
            .filter(|item| !item.is_empty())
            .collect()
    } else if s.split_whitespace().count() > 1 {
        s.split_whitespace()
            .map(|item| strip_quotes(item).to_string())
            .filter(|item| !item.is_empty())
            .collect()
    } else {
        vec![s.to_string()]
    }
}

/// Parse SKILL.md: extract YAML frontmatter (`---` delimited `key: value` lines),
/// extracting name / description / when_to_use and extended fields.
///
/// Handles common YAML scalars: bare strings, quoted strings, and inline lists
/// (`[a, b, c]`). Multi-line values (`|`, `>`) and nested maps are NOT supported.
///
/// Frontmatter missing: description from first markdown line; name from dir name.
/// description still empty -> skill is considered invalid, return None.
pub fn parse_skill_file(
    content: &str,
    dir_name: String,
    path: &Path,
    source: SkillSource,
) -> Option<SkillEntry> {
    let (front, body) = split_frontmatter(content);
    let mut entry = SkillEntry {
        name: dir_name.clone(),
        source,
        path: path.to_path_buf(),
        ..Default::default()
    };
    if let Some(yaml) = front {
        for line in yaml.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let key = k.trim();
            let raw_value = v.trim();
            if raw_value.is_empty() {
                continue;
            }
            let value = strip_quotes(raw_value);
            if value.is_empty() {
                continue;
            }
            match key {
                "name" => entry.name = value.to_string(),
                "description" => entry.description = value.to_string(),
                "when_to_use" | "whenToUse" => entry.when_to_use = Some(value.to_string()),
                "argument_hint" | "argumentHint" => entry.argument_hint = Some(value.to_string()),
                "allowed_tools" | "allowedTools" | "allowed-tools" => {
                    entry.allowed_tools = Some(parse_yaml_list(value));
                }
                "disallowed_tools" | "disallowedTools" | "disallowed-tools" => {
                    entry.disallowed_tools = Some(parse_yaml_list(value));
                }
                "arguments" => {
                    entry.arguments = Some(parse_yaml_list(value));
                }
                "model" => entry.model = Some(value.to_string()),
                "effort" => entry.effort = Some(value.to_string()),
                "context" => entry.context = Some(value.to_string()),
                "agent" => entry.agent = Some(value.to_string()),
                "background" => {
                    entry.background = Some(!matches!(
                        value.to_lowercase().as_str(),
                        "false" | "no" | "0"
                    ));
                }
                "disable_model_invocation" | "disableModelInvocation" => {
                    entry.disable_model_invocation =
                        matches!(value.to_lowercase().as_str(), "true" | "yes" | "1");
                }
                "user_invocable" | "userInvocable" => {
                    entry.user_invocable =
                        !matches!(value.to_lowercase().as_str(), "false" | "no" | "0");
                }
                "paths" => {
                    entry.paths = Some(parse_yaml_list(value));
                }
                "version" => entry.version = Some(value.to_string()),
                "files" => {
                    entry.files = Some(parse_yaml_list(value));
                }
                "hooks" => {
                    entry.hooks = Some(parse_yaml_list(value));
                }
                _ => {}
            }
        }
    }
    if entry.description.is_empty() {
        entry.description = first_paragraph(body);
    }
    if entry.description.is_empty() {
        return None;
    }
    Some(entry)
}

/// Extract a simple YAML key:value from a one-level frontmatter block.
/// Handles `name: my-memory` and `description: "one line"`.
pub(crate) fn extract_yaml_field(yaml: &str, key: &str) -> Option<String> {
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(key) {
            let rest = rest.trim_start().strip_prefix(':')?.trim_start();
            if rest.is_empty() {
                return None;
            }
            return Some(strip_quotes(rest).to_string());
        }
    }
    None
}

/// The value of the top-level `key` in a frontmatter block, with YAML's
/// quoting undone: double-quoted strings and their escapes, single-quoted
/// strings and `''`, plain scalars up to a ` #` comment, and `|` / `>` block
/// scalars. A flat block is all the engine writes; this covers what a person
/// editing one by hand is likely to write too.
pub fn scalar(frontmatter: &str, key: &str) -> Option<String> {
    let lines: Vec<&str> = frontmatter.lines().collect();
    let prefix = format!("{key}:");
    let at = lines
        .iter()
        .position(|l| !l.starts_with([' ', '\t']) && l.starts_with(&prefix))?;
    let rest = lines[at][prefix.len()..].trim();
    match rest.chars().next() {
        Some('|') | Some('>') => Some(block_scalar(&lines[at + 1..], rest.starts_with('>'))),
        _ => Some(inline_scalar(rest)),
    }
}

/// `child` under the top-level mapping `parent` — the layout memory files
/// used before their fields were flattened.
pub fn nested_scalar(frontmatter: &str, parent: &str, child: &str) -> Option<String> {
    let lines: Vec<&str> = frontmatter.lines().collect();
    let header = format!("{parent}:");
    let start = lines.iter().position(|l| l.trim_end() == header)?;
    let prefix = format!("{child}:");
    lines[start + 1..]
        .iter()
        .take_while(|l| l.starts_with([' ', '\t']))
        .find_map(|l| {
            l.trim()
                .strip_prefix(prefix.as_str())
                .map(|v| inline_scalar(v.trim()))
        })
}

fn inline_scalar(raw: &str) -> String {
    if let Some(body) = raw.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('0') => out.push('\0'),
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).collect();
                        if let Some(ch) =
                            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            out.push(ch);
                        }
                    }
                    Some(other) => out.push(other),
                    None => {}
                },
                other => out.push(other),
            }
        }
        return out;
    }
    if let Some(body) = raw.strip_prefix('\'') {
        let mut out = String::new();
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    out.push('\'');
                    continue;
                }
                break;
            }
            out.push(c);
        }
        return out;
    }
    match raw.find(" #") {
        Some(i) => raw[..i].trim_end().to_string(),
        None => raw.to_string(),
    }
}

/// The indented lines after a `|` (kept) or `>` (folded) header.
fn block_scalar(following: &[&str], folded: bool) -> String {
    let body: Vec<&str> = following
        .iter()
        .take_while(|l| l.is_empty() || l.starts_with([' ', '\t']))
        .copied()
        .collect();
    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    let lines: Vec<&str> = body.iter().map(|l| l.get(indent..).unwrap_or("")).collect();
    let joined = if folded {
        lines.join(" ")
    } else {
        lines.join("\n")
    };
    joined.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn scalars_undo_yaml_quoting() {
        let fm = "name: plain value # a comment\n\
                  description: \"says \\\"hi\\\" at C:\\\\x: ok\\u00e9\"\n\
                  single: 'it''s'\n\
                  empty:\n\
                  folded: >\n  one\n  two\n\
                  kept: |\n  a\n  b\n\
                  metadata:\n  type: feedback\n";
        assert_eq!(scalar(fm, "name").as_deref(), Some("plain value"));
        assert_eq!(
            scalar(fm, "description").as_deref(),
            Some("says \"hi\" at C:\\x: ok\u{e9}")
        );
        assert_eq!(scalar(fm, "single").as_deref(), Some("it's"));
        assert_eq!(scalar(fm, "empty").as_deref(), Some(""));
        assert_eq!(scalar(fm, "folded").as_deref(), Some("one two"));
        assert_eq!(scalar(fm, "kept").as_deref(), Some("a\nb"));
        assert_eq!(
            scalar(fm, "type"),
            None,
            "a nested key is not a top-level one"
        );
        assert_eq!(
            nested_scalar(fm, "metadata", "type").as_deref(),
            Some("feedback")
        );
        assert_eq!(scalar(fm, "missing"), None);
    }

    #[test]
    fn a_block_indented_with_wide_whitespace_does_not_split_a_character() {
        let fm = "description: |\n  \u{3000}x\n   y\n    \u{3000}\n";
        assert_eq!(scalar(fm, "description").as_deref(), Some("\u{3000}x\n y"));
    }

    use super::*;

    #[test]
    fn frontmatter_split_basic() {
        let s = "---\nname: foo\ndescription: bar\n---\nbody here\n";
        let (front, body) = split_frontmatter(s);
        assert_eq!(front, Some("name: foo\ndescription: bar\n"));
        assert_eq!(body, "body here\n");
    }

    #[test]
    fn frontmatter_split_no_frontmatter_returns_none() {
        let s = "no frontmatter here\nbody";
        let (front, body) = split_frontmatter(s);
        assert_eq!(front, None);
        assert_eq!(body, s);
    }

    #[test]
    fn frontmatter_split_unclosed_returns_none() {
        let s = "---\nname: foo\nno closing marker";
        let (front, body) = split_frontmatter(s);
        assert_eq!(front, None);
        assert_eq!(body, s);
    }

    #[test]
    fn parse_skill_extracts_all_three_fields() {
        let content = "---\nname: summarize-pr\ndescription: Summarize a PR\nwhen_to_use: When user asks for PR summary\n---\nbody\n";
        let entry = parse_skill_file(
            content,
            "summarize-pr".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.name, "summarize-pr");
        assert_eq!(entry.description, "Summarize a PR");
        assert_eq!(
            entry.when_to_use.as_deref(),
            Some("When user asks for PR summary")
        );
        assert_eq!(entry.source, SkillSource::User);
    }

    #[test]
    fn parse_skill_strips_quotes() {
        let content = "---\nname: foo\ndescription: \"with quotes\"\n---\nbody";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.description, "with quotes");
    }

    #[test]
    fn parse_skill_falls_back_to_first_body_line() {
        let content = "---\nname: foo\n---\n# Heading\n\nactual body";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::Project,
        )
        .unwrap();
        // 取 `# Heading` -> 剥 # 后 `Heading`
        assert_eq!(entry.description, "Heading");
    }

    #[test]
    fn parse_skill_returns_none_when_no_description_anywhere() {
        let content = "---\nname: foo\n---\n\n\n";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        );
        assert!(entry.is_none());
    }

    #[test]
    fn parse_skill_falls_back_to_first_paragraph_not_just_first_line() {
        // Multi-line opening paragraph, no leading heading — must join every
        // line up to the first blank line, not just the first one.
        let content = "---\nname: foo\n---\nThis is a skill\nthat does X.\n\nMore text below.";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.description, "This is a skill that does X.");
    }

    #[test]
    fn parse_yaml_list_accepts_space_separated_bare_string() {
        let content = "---\nname: foo\ndescription: d\nallowed-tools: Read Grep Glob\n---\nbody";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(
            entry.allowed_tools,
            Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string()
            ])
        );
    }

    #[test]
    fn parse_skill_extracts_disallowed_tools_and_arguments() {
        let content = "---\nname: foo\ndescription: d\ndisallowed-tools: AskUserQuestion\narguments: issue, branch\neffort: high\nagent: Explore\nbackground: false\n---\nbody";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(
            entry.disallowed_tools,
            Some(vec!["AskUserQuestion".to_string()])
        );
        assert_eq!(
            entry.arguments,
            Some(vec!["issue".to_string(), "branch".to_string()])
        );
        assert_eq!(entry.effort.as_deref(), Some("high"));
        assert_eq!(entry.agent.as_deref(), Some("Explore"));
        assert_eq!(entry.background, Some(false));
    }

    #[test]
    fn parse_skill_uses_dir_name_when_no_name_in_frontmatter() {
        let content = "---\ndescription: A skill\n---\nbody";
        let entry = parse_skill_file(
            content,
            "my-skill".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.name, "my-skill");
    }
}
