//! Whether the path arguments of a command line stay inside the directories the session may read.

use base::tool::ToolContext;

/// Whether every path-looking argument of every command in `line` resolves
/// inside the session's cwd or one of its additional directories.
///
/// Only a literal can be resolved here: a word the shell still expands — a
/// `$VAR`, a `~` (whatever `HOME` the line set), a substitution — may name
/// anything, and so may a relative path after a `cd` whose target is not a
/// literal directory inside. Relative paths after a literal `cd` are checked
/// against every directory the line may be in by then.
pub(super) fn path_args_stay_within(line: &str, ctx: &ToolContext) -> bool {
    if ctx.session.read_anywhere {
        return true;
    }
    let roots = crate::security::readable_roots(ctx);
    let inside = |arg: &str, base: &std::path::Path| {
        permissions::path_rule::ResolvedPath::new(arg, base)
            .all()
            .all(|p| {
                roots.iter().any(|root| {
                    p.starts_with(root) || crate::security::is_path_within_root(p, root)
                })
            })
    };
    let mut bases = vec![ctx.cwd.clone()];
    // Set once the line moves somewhere it does not name (`$OLDPWD`, the dir
    // stack): any relative read after that could land anywhere, so it is asked
    // about — but a line that only ends on such a move reads nothing more.
    let mut base_unknown = false;
    for segment in permissions::shell::simple_commands(line) {
        let words = permissions::shell::lex(&segment);
        let Some((command, args)) = words.split_first() else {
            continue;
        };
        let mut redirect_target = false;
        let mut operands = Vec::new();
        for word in args {
            if word.redirect {
                redirect_target = true;
                continue;
            }
            if word.substitutes || word.tilde {
                return false;
            }
            if std::mem::take(&mut redirect_target) {
                operands.push((word.text.as_str(), true));
                continue;
            }
            match word.text.split_once('=') {
                Some((flag, value)) if flag.starts_with('-') => {
                    if value.starts_with('~') {
                        return false;
                    }
                    operands.push((value, false));
                }
                _ if word.text.starts_with('-') && word.text != "-" => {}
                _ => operands.push((word.text.as_str(), false)),
            }
        }
        // `popd`, `cd -` and `pushd +N` move to a directory the line does not
        // name (`$OLDPWD`, the stack). Mark the base unknown rather than fail
        // outright: a trailing `popd` reads nothing, but a relative read after
        // one is checked below and asked about.
        if command.text == "popd" {
            base_unknown = true;
            continue;
        }
        if matches!(command.text.as_str(), "cd" | "pushd") {
            let Some((target, _)) = operands.first() else {
                return false;
            };
            if *target == "-" || target.starts_with('+') {
                base_unknown = true;
                continue;
            }
            if !bases.iter().all(|base| inside(target, base)) {
                return false;
            }
            let entered: Vec<_> = bases
                .iter()
                .map(|base| permissions::path_rule::ResolvedPath::new(target, base).primary)
                .collect();
            bases.extend(entered);
            // A literal cd names where we are again.
            base_unknown = false;
            continue;
        }
        // After a move to an unnamed directory, any operand here — even a bare
        // filename, which would otherwise be assumed to sit in a known cwd —
        // could be read from it. (Only non-flag words reach `operands`.)
        if base_unknown {
            if !operands.is_empty() {
                return false;
            }
            continue;
        }
        let stays = operands
            .iter()
            .filter(|(arg, is_target)| *is_target || looks_like_path(arg))
            .all(|(arg, _)| {
                if arg.starts_with('/') {
                    inside(arg, &ctx.cwd)
                } else {
                    bases.iter().all(|base| inside(arg, base))
                }
            });
        if !stays {
            return false;
        }
    }
    true
}

fn looks_like_path(arg: &str) -> bool {
    arg.starts_with(['/', '~']) || arg.contains('/') || arg == ".."
}
