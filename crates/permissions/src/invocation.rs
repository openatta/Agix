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
//! how it was typed. The Bash tool's read-only fast path reads a line the
//! same way, through [`traced_invocations`], which also says which wrappers
//! and assignments each program came through.
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

/// One [`Invocation`], with what the command line put around it on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traced {
    pub invocation: Invocation,
    /// The wrappers it was unwrapped from, outermost first, each as the words
    /// it took: `env -u X` and `nice -n 5` in `env -u X nice -n 5 ls`.
    pub wrappers: Vec<Vec<String>>,
    /// The `NAME=value` words it runs under: its own leading ones and those a
    /// wrapper (`env`, `sudo`) passes on.
    pub assignments: Vec<String>,
}

/// Everything the simple command `source` starts.
pub fn invocations(source: &str) -> Vec<Invocation> {
    traced_invocations(source)
        .into_iter()
        .map(|t| t.invocation)
        .collect()
}

/// [`invocations`], each with the wrappers and assignments it was found under.
pub fn traced_invocations(source: &str) -> Vec<Traced> {
    let mut out = Vec::new();
    resolve(shell::lex(source), 0, &mut out);
    out
}

#[derive(Debug, Clone, Default)]
struct Trail {
    wrappers: Vec<Vec<String>>,
    assignments: Vec<String>,
}

impl Trail {
    fn found(&self, invocation: Invocation) -> Traced {
        Traced {
            invocation,
            wrappers: self.wrappers.clone(),
            assignments: self.assignments.clone(),
        }
    }
}

/// How deep `sh -c` / `eval` strings are followed before giving up.
const MAX_DEPTH: usize = 4;
/// Brace expansion beyond this many words is not followed.
const MAX_BRACE_WORDS: usize = 64;

fn resolve(words: Vec<Word>, depth: usize, out: &mut Vec<Traced>) {
    let mut argv = without_redirections(words);
    let leading = argv
        .iter()
        .take_while(|w| {
            !w.quoted
                && (shell::PREFIX_KEYWORDS.contains(&w.text.as_str())
                    || shell::is_assignment(&w.text))
        })
        .count();
    let trail = Trail {
        wrappers: Vec::new(),
        assignments: argv
            .drain(..leading)
            .filter(|w| shell::is_assignment(&w.text))
            .map(|w| w.text)
            .collect(),
    };
    resolve_command(argv, trail, depth, out);
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

fn resolve_command(mut argv: Vec<Word>, mut trail: Trail, depth: usize, out: &mut Vec<Traced>) {
    for _ in 0..16 {
        let Some(first) = argv.first() else {
            return;
        };
        if first.substitutes || first.globs || first.parens {
            out.push(trail.found(Invocation::Unknown));
            return;
        }
        if first.braces {
            match expand_braces(first) {
                Some(expanded) => {
                    argv.splice(0..1, expanded);
                    continue;
                }
                None => {
                    out.push(trail.found(Invocation::Unknown));
                    return;
                }
            }
        }
        let name = program_name(&first.text).to_string();
        let args = &argv[1..];

        // A shell keyword that opens a block: what runs inside it (a `case`
        // branch, a function body) is not this word, and the scanner does not
        // follow it in. Reading the program off the first word here would hand
        // a deny `case x in *) rm;; esac` on the strength of its spelling.
        if !first.quoted && matches!(name.as_str(), "case" | "function" | "select") {
            out.push(trail.found(Invocation::Unknown));
            return;
        }

        if let Some(wrapper) = WRAPPERS.iter().find(|w| w.names.contains(&name.as_str())) {
            match wrapper.command_in(args) {
                Wrapped::At(n) => {
                    let taken: Vec<String> = argv.drain(..=n).map(|w| w.text).collect();
                    if wrapper.assignments {
                        trail.assignments.extend(
                            taken[1..]
                                .iter()
                                .filter(|w| passes_on_assignment(w))
                                .cloned(),
                        );
                    }
                    trail.wrappers.push(taken);
                    continue;
                }
                Wrapped::Nothing => {}
                Wrapped::Unreadable => {
                    out.push(trail.found(Invocation::Unknown));
                    return;
                }
            }
        } else if SHELLS.contains(&name.as_str()) {
            match shell_target(args) {
                ShellTarget::Command(Some(script)) if script.is_literal() => {
                    resolve_line(&script.text, depth, out);
                }
                // A `-c` whose string the line builds, or a shell that reads
                // its script from stdin / a here-string: what it runs cannot be
                // told from the text, so a deny must not be waved past it.
                ShellTarget::Command(_) | ShellTarget::Stdin => {
                    out.push(trail.found(Invocation::Unknown));
                }
                // A script file argument — recorded as the shell itself below,
                // the same as a program whose source this cannot read.
                ShellTarget::File => {}
            }
        } else if matches!(name.as_str(), "su" | "runuser") {
            match dash_c_command(args) {
                Some(Some(script)) if script.is_literal() => {
                    resolve_line(&script.text, depth, out);
                }
                Some(_) => out.push(trail.found(Invocation::Unknown)),
                None => match after_double_dash(args) {
                    Some(command) => resolve_command(command, Trail::default(), depth, out),
                    None => out.push(trail.found(Invocation::Unknown)),
                },
            }
        } else if name == "eval" {
            if args.iter().all(Word::is_literal) {
                let script: Vec<&str> = args.iter().map(|w| w.text.as_str()).collect();
                resolve_line(&script.join(" "), depth, out);
            } else {
                out.push(trail.found(Invocation::Unknown));
            }
        } else if matches!(name.as_str(), "find" | "gfind") {
            for command in find_exec_commands(args) {
                resolve_command(command, Trail::default(), depth, out);
            }
        }
        out.push(trail.found(Invocation::Runs(argv.into_iter().map(|w| w.text).collect())));
        return;
    }
    out.push(trail.found(Invocation::Unknown));
}

fn resolve_line(line: &str, depth: usize, out: &mut Vec<Traced>) {
    if depth >= MAX_DEPTH {
        out.push(Trail::default().found(Invocation::Unknown));
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

/// What a shell started with `args` runs.
enum ShellTarget {
    /// `sh -c STRING`: the `STRING` word, or `None` when `-c` has none after it.
    Command(Option<Word>),
    /// A script file argument (`sh script.sh`).
    File,
    /// Nothing named — the shell reads its script from stdin or a here-string.
    Stdin,
}

fn shell_target(args: &[Word]) -> ShellTarget {
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
    if takes_c {
        ShellTarget::Command(args.get(i).cloned())
    } else if i < args.len() {
        ShellTarget::File
    } else {
        ShellTarget::Stdin
    }
}

/// The command string of a `-c` / `--command` option (`su -c 'rm x' user`):
/// `Some(Some(w))` the string, `Some(None)` a `-c` with none after it, `None`
/// no such option.
fn dash_c_command(args: &[Word]) -> Option<Option<Word>> {
    let mut i = 0;
    while let Some(w) = args.get(i) {
        let t = w.text.as_str();
        if t == "--" {
            return None;
        }
        if let Some(rest) = t.strip_prefix("--command") {
            return Some(
                rest.strip_prefix('=')
                    .map(Word::plain)
                    .or_else(|| args.get(i + 1).cloned()),
            );
        }
        if let Some(cluster) = t
            .strip_prefix('-')
            .filter(|c| !c.is_empty() && !c.starts_with('-'))
        {
            if let Some(pos) = cluster.find('c') {
                let after = &cluster[pos + 1..];
                return Some(if after.is_empty() {
                    args.get(i + 1).cloned()
                } else {
                    Some(Word::plain(after))
                });
            }
        }
        i += 1;
    }
    None
}

/// The command vector after a `--` separator (`runuser -u me -- rm x`).
fn after_double_dash(args: &[Word]) -> Option<Vec<Word>> {
    let at = args.iter().position(|w| w.text == "--" && !w.quoted)?;
    let rest = &args[at + 1..];
    (!rest.is_empty()).then(|| rest.to_vec())
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
        names: &["builtin", "nohup", "coproc", "busybox"],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["pkexec"],
        long_values: &["user"],
        long_flags: &["disable-internal-agent", "keep-cwd", "version", "help"],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["caffeinate"],
        short_values: "tw",
        short_flags: "dimsu",
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["ionice"],
        short_values: "cnp",
        short_flags: "t",
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["chrt"],
        short_values: "pTPD",
        short_flags: "ofrbidRmav",
        long_flags: &["reset-on-fork"],
        operands: 1,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["taskset"],
        short_values: "p",
        short_flags: "ac",
        operands: 1,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["flock"],
        short_values: "wE",
        short_flags: "sxunoF",
        operands: 1,
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

/// A word `env` / `sudo` take as `NAME=value` rather than as the command:
/// any word with an `=` in it, quoted or not, whether or not the shell would
/// call `NAME` a variable name.
fn passes_on_assignment(word: &str) -> bool {
    word.contains('=') && !word.starts_with('-')
}

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
            while args.get(i).is_some_and(|w| passes_on_assignment(&w.text)) {
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
    fn env_takes_any_word_with_an_equals_sign_as_an_assignment() {
        assert_eq!(runs("env 'A=1' rm x"), ["rm x"]);
        assert_eq!(runs("env ./x=1 rm x"), ["rm x"]);
        assert_eq!(runs("sudo 1=2 rm x"), ["rm x"]);
    }

    #[test]
    fn each_invocation_carries_the_wrappers_and_assignments_it_came_through() {
        let found = traced_invocations("A=1 env -u X B=2 nice -n 5 ls");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].invocation, Invocation::Runs(vec!["ls".into()]));
        assert_eq!(
            found[0].wrappers,
            [vec!["env", "-u", "X", "B=2"], vec!["nice", "-n", "5"]]
        );
        assert_eq!(found[0].assignments, ["A=1", "B=2"]);
        let found = traced_invocations("ls");
        assert!(found[0].wrappers.is_empty() && found[0].assignments.is_empty());
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
    fn more_wrappers_are_unwrapped() {
        for line in [
            "busybox rm x",
            "flock /tmp/lock rm x",
            "flock -w 5 /tmp/lock rm x",
            "ionice -c 3 rm x",
            "chrt -o 0 rm x",
            "taskset -c 0 rm x",
            "taskset 0x3 rm x",
            "pkexec rm x",
            "caffeinate -i rm x",
            "su -c 'rm x' root",
            "su -c 'rm x'",
            "runuser -u me -- rm x",
        ] {
            assert!(
                runs(line).iter().any(|r| r == "rm x"),
                "{line}: {:?}",
                runs(line)
            );
        }
    }

    #[test]
    fn a_block_keyword_and_a_shell_reading_stdin_are_unknown() {
        for line in [
            "case x in *) rm -rf b;; esac",
            "function f { rm -rf b; }",
            "select x in a b; do rm; done",
            "sh",
            "bash <<< 'rm -rf b'",
            "su root",
        ] {
            assert!(
                runs(line).iter().any(|r| r == "?"),
                "{line}: {:?}",
                runs(line)
            );
        }
        // A real script-file argument is not stdin, and stays readable as the
        // shell itself.
        assert_eq!(runs("bash script.sh"), ["bash script.sh"]);
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
