//! What program a simple command really runs, read the way the shell will.
//!
//! A `Bash(rm:*)` deny is a statement about running `rm`, but the text of a
//! command can spell that many ways: `"rm" x`, `\rm x`, `{rm,x}`,
//! `/bin/rm x`, `command -p rm x`, `sudo -u me rm x`, `bash -c 'rm x'`.
//! [`invocations`] turns a segment from [`crate::shell::segments`] into the
//! argument vectors it starts — quotes removed, braces expanded, redirections
//! dropped, known wrappers (`env`, `exec`, `sudo`, `timeout`, `xargs`, ...)
//! unwrapped with their options, `sh -c` / `eval` / `find -exec` looked into —
//! so the rule engine can hold deny and ask rules to what runs rather than to
//! how it was typed.
//!
//! Where the program cannot be told from the text — a command name that is
//! a variable, a glob or a substitution, a wrapper option this does not
//! know — the answer is [`Invocation::Unknown`], which the rule engine never
//! lets through a deny on the strength of the spelling alone.
//!
//! This narrows what a deny rule can be talked past; it does not make deny
//! rules on a shell a sandbox. `python3 -c 'import os; os.remove("x")'` runs
//! no `rm`. The OS sandbox is what bounds a shell command.

use crate::shell::{self, Word};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// The program and its arguments, as it will be started.
    Runs(Vec<String>),
    /// What runs cannot be told without running the line.
    Unknown,
}

/// Everything the simple command `source` starts.
pub fn invocations(source: &str) -> Vec<Invocation> {
    let mut out = Vec::new();
    resolve(shell::lex(source), 0, &mut out);
    out
}

/// How deep `sh -c` / `eval` strings are followed before giving up.
const MAX_DEPTH: usize = 4;
/// Brace expansion beyond this many words is not followed.
const MAX_BRACE_WORDS: usize = 64;

fn resolve(words: Vec<Word>, depth: usize, out: &mut Vec<Invocation>) {
    let mut argv = without_redirections(words);
    let leading = argv
        .iter()
        .take_while(|w| {
            !w.quoted
                && (shell::PREFIX_KEYWORDS.contains(&w.text.as_str())
                    || shell::is_assignment(&w.text))
        })
        .count();
    argv.drain(..leading);
    resolve_command(argv, depth, out);
}

fn without_redirections(words: Vec<Word>) -> Vec<Word> {
    let mut out = Vec::with_capacity(words.len());
    let mut target_next = false;
    for word in words {
        if word.redirect {
            target_next = true;
        } else if std::mem::take(&mut target_next) {
            continue;
        } else {
            out.push(word);
        }
    }
    out
}

fn resolve_command(mut argv: Vec<Word>, depth: usize, out: &mut Vec<Invocation>) {
    for _ in 0..16 {
        let Some(first) = argv.first() else {
            return;
        };
        if first.substitutes || first.globs || first.parens {
            out.push(Invocation::Unknown);
            return;
        }
        if first.braces {
            match expand_braces(first) {
                Some(expanded) => {
                    argv.splice(0..1, expanded);
                    continue;
                }
                None => {
                    out.push(Invocation::Unknown);
                    return;
                }
            }
        }
        let name = program_name(&first.text).to_string();
        let args = &argv[1..];

        if let Some(wrapper) = WRAPPERS.iter().find(|w| w.names.contains(&name.as_str())) {
            match wrapper.command_in(args) {
                Wrapped::At(n) => {
                    argv.drain(..=n);
                    continue;
                }
                Wrapped::Nothing => {}
                Wrapped::Unreadable => {
                    out.push(Invocation::Unknown);
                    return;
                }
            }
        } else if SHELLS.contains(&name.as_str()) {
            match command_string(args) {
                Some(Some(script)) if script.is_literal() => {
                    resolve_line(&script.text, depth, out);
                }
                Some(_) => out.push(Invocation::Unknown),
                None => {}
            }
        } else if name == "eval" {
            if args.iter().all(Word::is_literal) {
                let script: Vec<&str> = args.iter().map(|w| w.text.as_str()).collect();
                resolve_line(&script.join(" "), depth, out);
            } else {
                out.push(Invocation::Unknown);
            }
        } else if matches!(name.as_str(), "find" | "gfind") {
            for command in find_exec_commands(args) {
                resolve_command(command, depth, out);
            }
        }
        out.push(Invocation::Runs(argv.into_iter().map(|w| w.text).collect()));
        return;
    }
    out.push(Invocation::Unknown);
}

fn resolve_line(line: &str, depth: usize, out: &mut Vec<Invocation>) {
    if depth >= MAX_DEPTH {
        out.push(Invocation::Unknown);
        return;
    }
    for segment in shell::segments(line) {
        resolve(shell::lex(&segment.source), depth + 1, out);
    }
}

/// The name a command word runs under: `/bin/rm` and `./rm` run `rm`.
pub fn program_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// `pre{a,b}post` → `prea post`... as the shell expands it. `None` for a
/// sequence (`{1..3}`), a quoted or substituted brace, or too many words.
fn expand_braces(word: &Word) -> Option<Vec<Word>> {
    if word.quoted || word.substitutes {
        return None;
    }
    let mut out = Vec::new();
    expand_into(&word.text, &mut out)?;
    Some(out.into_iter().map(Word::plain).collect())
}

fn expand_into(text: &str, out: &mut Vec<String>) -> Option<()> {
    let chars: Vec<char> = text.chars().collect();
    for open in (0..chars.len()).filter(|&i| chars[i] == '{') {
        let mut depth = 0;
        let mut commas = Vec::new();
        let mut close = None;
        for (j, &c) in chars.iter().enumerate().skip(open) {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(j);
                        break;
                    }
                }
                ',' if depth == 1 => commas.push(j),
                _ => {}
            }
        }
        let Some(close) = close else { continue };
        if commas.is_empty() {
            if chars[open + 1..close].windows(2).any(|w| w == ['.', '.']) {
                return None;
            }
            continue;
        }
        let prefix: String = chars[..open].iter().collect();
        let suffix: String = chars[close + 1..].iter().collect();
        let mut start = open + 1;
        for end in commas.into_iter().chain([close]) {
            let part: String = chars[start..end].iter().collect();
            expand_into(&format!("{prefix}{part}{suffix}"), out)?;
            start = end + 1;
        }
        return Some(());
    }
    out.push(text.to_string());
    (out.len() <= MAX_BRACE_WORDS).then_some(())
}

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "mksh", "ash", "fish"];

/// The string `sh -c STRING` runs: `Some(None)` when there is a `-c` but no
/// string after it, `None` when the shell runs a script file or reads stdin.
fn command_string(args: &[Word]) -> Option<Option<Word>> {
    let mut takes_c = false;
    let mut i = 0;
    while let Some(w) = args.get(i) {
        let t = w.text.as_str();
        if t == "--" {
            i += 1;
            break;
        }
        if matches!(t, "-o" | "+o" | "-O" | "+O" | "--rcfile" | "--init-file") {
            i += 2;
            continue;
        }
        if t.starts_with("--") {
            i += 1;
            continue;
        }
        if t.len() > 1 && (t.starts_with('-') || t.starts_with('+')) {
            takes_c |= t.starts_with('-') && t.contains('c');
            i += 1;
            continue;
        }
        break;
    }
    takes_c.then(|| args.get(i).cloned())
}

/// The commands `find ... -exec CMD ... ;` runs.
fn find_exec_commands(args: &[Word]) -> Vec<Vec<Word>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if matches!(
            args[i].text.as_str(),
            "-exec" | "-execdir" | "-ok" | "-okdir"
        ) {
            let end = (i + 1..args.len())
                .find(|&j| matches!(args[j].text.as_str(), ";" | "+"))
                .unwrap_or(args.len());
            out.push(args[i + 1..end].to_vec());
            i = end;
        }
        i += 1;
    }
    out
}

/// A command that runs another command named in its arguments.
struct Wrapper {
    names: &'static [&'static str],
    /// Short options followed by a value, in the same word or the next.
    short_values: &'static str,
    short_flags: &'static str,
    /// Short options whose value, if any, is attached (`xargs -i{}`).
    short_optional: &'static str,
    long_values: &'static [&'static str],
    long_flags: &'static [&'static str],
    /// Options under which nothing is run, only looked up (`command -v`).
    lookups: &'static str,
    long_lookups: &'static [&'static str],
    /// Options whose value is itself a command line this does not read.
    opaque: &'static str,
    long_opaque: &'static [&'static str],
    /// `-N` is an option (`nice -5`).
    numeric_flags: bool,
    /// Words between the options and the command (`timeout`'s duration).
    operands: usize,
    /// `NAME=value` words may come before the command (`env`).
    assignments: bool,
}

const NO_WRAPPER: Wrapper = Wrapper {
    names: &[],
    short_values: "",
    short_flags: "",
    short_optional: "",
    long_values: &[],
    long_flags: &[],
    lookups: "",
    long_lookups: &[],
    opaque: "",
    long_opaque: &[],
    numeric_flags: false,
    operands: 0,
    assignments: false,
};

const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        names: &["command"],
        short_flags: "p",
        lookups: "vV",
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["builtin", "nohup", "coproc"],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["exec"],
        short_values: "a",
        short_flags: "cl",
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["env"],
        short_values: "uCP",
        short_flags: "i0v",
        long_values: &["unset", "chdir"],
        long_flags: &[
            "ignore-environment",
            "null",
            "debug",
            "default-signal",
            "ignore-signal",
            "block-signal",
            "list-signal-handling",
        ],
        opaque: "S",
        long_opaque: &["split-string"],
        assignments: true,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["nice"],
        short_values: "n",
        long_values: &["adjustment"],
        numeric_flags: true,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["time"],
        short_values: "fo",
        short_flags: "pavq",
        long_values: &["format", "output"],
        long_flags: &["append", "verbose", "portability", "quiet"],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["sudo"],
        short_values: "ugCDhprtTU",
        short_flags: "AbEHiKknPSsBN",
        long_values: &[
            "user",
            "group",
            "close-from",
            "chdir",
            "host",
            "prompt",
            "role",
            "type",
            "command-timeout",
            "other-user",
        ],
        long_flags: &[
            "askpass",
            "background",
            "bell",
            "preserve-env",
            "preserve-groups",
            "set-home",
            "login",
            "remove-timestamp",
            "reset-timestamp",
            "non-interactive",
            "stdin",
            "shell",
        ],
        lookups: "elvV",
        long_lookups: &["edit", "list", "validate", "version", "help"],
        assignments: true,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["doas"],
        short_values: "uC",
        short_flags: "ns",
        lookups: "L",
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["timeout", "gtimeout"],
        short_values: "sk",
        short_flags: "v",
        long_values: &["signal", "kill-after"],
        long_flags: &["preserve-status", "foreground", "verbose"],
        operands: 1,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["stdbuf"],
        short_values: "ioe",
        long_values: &["input", "output", "error"],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["xargs", "gxargs"],
        short_values: "IaELnPsd",
        short_flags: "0prtx",
        short_optional: "iel",
        long_values: &[
            "arg-file",
            "delimiter",
            "max-lines",
            "max-args",
            "max-procs",
            "max-chars",
            "process-slot-var",
        ],
        long_flags: &[
            "null",
            "no-run-if-empty",
            "verbose",
            "interactive",
            "exit",
            "open-tty",
            "show-limits",
            "replace",
            "eof",
        ],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["setsid"],
        short_flags: "cfw",
        long_flags: &["ctty", "fork", "wait"],
        ..NO_WRAPPER
    },
];

enum Wrapped {
    /// The wrapped command starts at this index of the wrapper's arguments.
    At(usize),
    /// The wrapper runs nothing else.
    Nothing,
    /// An option this cannot read: where the command starts is unknown.
    Unreadable,
}

impl Wrapper {
    fn command_in(&self, args: &[Word]) -> Wrapped {
        let mut i = 0;
        while let Some(w) = args.get(i) {
            let t = w.text.as_str();
            if t == "--" {
                i += 1;
                break;
            }
            if let Some(long) = t.strip_prefix("--") {
                let (name, inline_value) = match long.split_once('=') {
                    Some((name, _)) => (name, true),
                    None => (long, false),
                };
                if self.long_opaque.contains(&name) {
                    return Wrapped::Unreadable;
                }
                if self.long_lookups.contains(&name) {
                    return Wrapped::Nothing;
                }
                if self.long_values.contains(&name) {
                    i += if inline_value { 1 } else { 2 };
                } else if self.long_flags.contains(&name) {
                    i += 1;
                } else {
                    return Wrapped::Unreadable;
                }
                continue;
            }
            if t == "-" && self.assignments {
                i += 1;
                continue;
            }
            let Some(cluster) = t.strip_prefix('-').filter(|c| !c.is_empty()) else {
                break;
            };
            if self.numeric_flags && cluster.chars().all(|c| c.is_ascii_digit()) {
                i += 1;
                continue;
            }
            let mut value_in_next = false;
            for (k, c) in cluster.char_indices() {
                if self.opaque.contains(c) {
                    return Wrapped::Unreadable;
                }
                if self.lookups.contains(c) {
                    return Wrapped::Nothing;
                }
                if self.short_optional.contains(c) {
                    break;
                }
                if self.short_values.contains(c) {
                    value_in_next = k + c.len_utf8() == cluster.len();
                    break;
                }
                if !self.short_flags.contains(c) {
                    return Wrapped::Unreadable;
                }
            }
            i += if value_in_next { 2 } else { 1 };
        }
        i += self.operands;
        if self.assignments {
            while args
                .get(i)
                .is_some_and(|w| !w.quoted && shell::is_assignment(&w.text))
            {
                i += 1;
            }
        }
        if i < args.len() {
            Wrapped::At(i)
        } else {
            Wrapped::Nothing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(source: &str) -> Vec<String> {
        invocations(source)
            .into_iter()
            .map(|i| match i {
                Invocation::Runs(argv) => argv.join(" "),
                Invocation::Unknown => "?".into(),
            })
            .collect()
    }

    #[test]
    fn quotes_escapes_and_paths_do_not_hide_the_program() {
        assert_eq!(runs("\"rm\" x"), ["rm x"]);
        assert_eq!(runs("\\rm x"), ["rm x"]);
        assert_eq!(runs("r''m x"), ["rm x"]);
        assert_eq!(runs("/bin/rm x"), ["/bin/rm x"]);
        assert_eq!(program_name("/bin/rm"), "rm");
        assert_eq!(runs(">/dev/null rm x"), ["rm x"]);
        assert_eq!(runs("rm>/dev/null x"), ["rm x"]);
    }

    #[test]
    fn a_brace_expansion_is_expanded() {
        assert_eq!(runs("{rm,x}"), ["rm x"]);
        assert_eq!(runs("{rm,-rf,x}"), ["rm -rf x"]);
        assert_eq!(runs("{r,}m x"), ["rm m x"]);
        assert_eq!(runs("{env,rm} x"), ["rm x"]);
        assert_eq!(runs("{1..3}"), ["?"]);
        assert_eq!(runs("echo {}"), ["echo {}"]);
    }

    #[test]
    fn wrappers_are_unwrapped_with_their_options() {
        for line in [
            "command -p rm x",
            "command rm x",
            "builtin rm x",
            "exec -a name rm x",
            "env -i -- A=1 rm x",
            "sudo A=1 rm x",
            "env -u HOME rm x",
            "nice -n 5 rm x",
            "nice -5 rm x",
            "time -p rm x",
            "nohup rm x",
            "sudo -u root -H rm x",
            "sudo --user=root rm x",
            "doas -u root rm x",
            "timeout -s KILL 5 rm x",
            "timeout 5s rm x",
            "stdbuf -oL rm x",
            "setsid rm x",
            "env nice sudo rm x",
        ] {
            assert_eq!(runs(line), ["rm x"], "{line}");
        }
        assert_eq!(runs("xargs rm"), ["rm"]);
        assert_eq!(runs("xargs -0 -n 1 rm"), ["rm"]);
    }

    #[test]
    fn a_lookup_runs_nothing_else() {
        assert_eq!(runs("command -v rm"), ["command -v rm"]);
        assert_eq!(runs("sudo -l rm"), ["sudo -l rm"]);
        assert_eq!(runs("env"), ["env"]);
    }

    #[test]
    fn an_option_that_cannot_be_read_is_unknown() {
        assert_eq!(runs("sudo --frobnicate rm x"), ["?"]);
        assert_eq!(runs("env -S 'rm x'"), ["?"]);
    }

    #[test]
    fn a_command_string_is_read_too() {
        assert_eq!(runs("bash -c 'rm x'"), ["rm x", "bash -c rm x"]);
        assert_eq!(
            runs("sh -ec 'true; rm x'"),
            ["true", "rm x", "sh -ec true; rm x"]
        );
        assert_eq!(runs("eval rm x"), ["rm x", "eval rm x"]);
        assert_eq!(runs("bash -c \"$CMD\""), ["?", "bash -c $CMD"]);
        assert_eq!(runs("bash script.sh"), ["bash script.sh"]);
        assert_eq!(
            runs("find . -name a -exec rm {} \\;"),
            ["rm {}", "find . -name a -exec rm {} ;"]
        );
    }

    #[test]
    fn a_program_named_by_an_expansion_is_unknown() {
        for line in [
            "$CMD x",
            "${RM} x",
            "$(echo rm) x",
            "`echo rm` x",
            "/bin/r? x",
            "f() x",
        ] {
            assert_eq!(runs(line), ["?"], "{line}");
        }
        assert_eq!(runs("rm $X"), ["rm $X"]);
    }
}
