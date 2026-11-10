//! Read-only / destructive classification of a command line.

mod lists;

use super::CmdClassification;
use lists::{DESTRUCTIVE_PREFIXES, READ_ONLY_COMMANDS, READ_ONLY_PREFIXES};
use permissions::invocation::{program_name, traced_invocations, Invocation, Traced};
use permissions::shell::is_dangerous_variable;

/// 命令分类。**保守**：未识别 → read_only=false, destructive=false（不算
/// 安全也不算危险，走默认 ask 路径）。
///
/// What a line runs is read the way the deny rules read it: split into
/// simple commands ([`permissions::shell::segments`]), and each one resolved
/// to the programs it starts ([`permissions::invocation::traced_invocations`])
/// — quotes removed, wrappers unwrapped together with their options,
/// `sh -c` / `eval` / `find -exec` looked into. Every program the line starts
/// must be read-only for the line to be; any destructive one makes the line
/// destructive. A program that cannot be read off the line is never
/// read-only.
///
/// # `read_only` 的语义
///
/// `read_only == true` 会让 `BashTool::check_permissions` 直接返回 `Allow`，
/// 也就是**不弹权限窗直接执行**。所以这个判定必须真的意味着"这条命令不会写
/// 任何东西、不会执行别的程序、不会读凭据"。只拿名字去撞白名单、不看参数与
/// 包在外面的东西，就等价于任意命令执行：
///
/// ```text
/// find . -name '*.rs' -exec sh -c '...' \;   # find 在白名单里
/// env -u ls sh -c '...'                      # `-u` 吃掉 `ls`，跑的是 sh
/// ./ls                                       # 跑的是仓库里叫 ls 的文件
/// cat ~/.ssh/id_rsa                          # cat 在白名单里
/// cat a >&Makefile                           # 写文件
/// ```
///
/// 所以分三层判定：
///
/// 1. **整串**：命令替换（`$(`、反引号）、写文件的重定向（见
///    [`has_file_redirect`]）、进程替换（`<(`）、子 shell / 花括号组 /
///    here-document（见 [`has_unsplit_construct`]）、设危险变量（见
///    [`is_dangerous_variable`]）出现即判非只读。必须在分段**之前**跑 ——
///    `$(...)` 可以跨越 `|`/`;` 边界，按段看会漏。
/// 2. **调用**层：每个被启动的程序都得按名字（不带路径）在名单里，经过的每层
///    wrapper 都不改变它跑什么、在哪跑（见 [`wrapper_keeps_read_only`]），
///    带进去的变量都不危险。
/// 3. **参数**层：名字在名单里、但参数让它拉起别的程序或写文件（见
///    [`args_forfeit_read_only`]），或任何一个词指向凭据目录。
pub fn classify(cmd: &str) -> CmdClassification {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return CmdClassification {
            read_only: true,
            destructive: false,
        };
    }

    let mut read_only = !has_shell_escape_hatch(trimmed)
        && !has_unsplit_construct(trimmed)
        && !sets_a_dangerous_variable(trimmed);
    let mut destructive = false;
    for segment in permissions::shell::segments(trimmed) {
        read_only &= !permissions::shell::lex(&segment.source)
            .iter()
            .any(|w| path_arg_hits_denied_read(&w.text));
        for found in traced_invocations(&segment.source) {
            read_only &= runs_read_only(&found);
            destructive |= runs_destructive(&found);
        }
    }
    CmdClassification {
        read_only,
        destructive,
    }
}

fn runs_read_only(found: &Traced) -> bool {
    let Invocation::Runs(argv) = &found.invocation else {
        return false;
    };
    found.wrappers.iter().all(|w| wrapper_keeps_read_only(w))
        && !found.assignments.iter().any(|a| {
            a.split_once('=')
                .is_some_and(|(name, _)| is_dangerous_variable(name))
        })
        && program_is_read_only(argv)
}

fn program_is_read_only(argv: &[String]) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return true;
    };
    // A path runs that file, whatever it is called: `./ls` is not `ls`.
    if program.contains('/') {
        return false;
    }
    let listed = READ_ONLY_COMMANDS.contains(&program.as_str())
        || READ_ONLY_PREFIXES.iter().any(|p| spells(p, program, args));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    listed && !args_forfeit_read_only(program, &args)
}

fn runs_destructive(found: &Traced) -> bool {
    let through_one = found
        .wrappers
        .iter()
        .filter_map(|w| w.first())
        .any(|w| DESTRUCTIVE_PREFIXES.contains(&program_name(w)));
    through_one
        || match &found.invocation {
            Invocation::Runs(argv) => argv.split_first().is_some_and(|(program, args)| {
                DESTRUCTIVE_PREFIXES
                    .iter()
                    .any(|p| spells(p, program_name(program), args))
            }),
            Invocation::Unknown => false,
        }
}

/// Whether `program args…` begins with the words of `prefix` (`git status`).
fn spells(prefix: &str, program: &str, args: &[String]) -> bool {
    let mut words = prefix.split(' ');
    words.next() == Some(program)
        && words
            .enumerate()
            .all(|(k, w)| args.get(k).is_some_and(|a| a == w))
}

/// Whether a read-only command run under this wrapper stays read-only.
///
/// `nice`, `timeout`, `nohup`, `stdbuf`… change when or for how long it runs,
/// not what runs or where. `env` may only clear or pass on variables — `-C`
/// moves it to another directory, `-P` looks the command up on another
/// `PATH`. `time` may not write its report to a file. The rest are not
/// read-only whatever they run: `xargs` hands it arguments nobody has seen,
/// `sudo` runs it as someone else, `flock` creates its lock file.
fn wrapper_keeps_read_only(words: &[String]) -> bool {
    let Some((wrapper, options)) = words.split_first() else {
        return true;
    };
    if wrapper.contains('/') {
        return false;
    }
    match wrapper.as_str() {
        "command" | "builtin" | "exec" | "nohup" | "nice" | "timeout" | "gtimeout" | "stdbuf" => {
            true
        }
        "time" => !options.iter().any(|o| {
            long_option_among(o, &["output", "append"])
                || short_cluster(o).is_some_and(|c| c.contains(['o', 'a']))
        }),
        "env" => env_only_passes_variables(options),
        _ => false,
    }
}

fn env_only_passes_variables(options: &[String]) -> bool {
    let mut i = 0;
    while let Some(o) = options.get(i).map(String::as_str) {
        if let Some(long) = o.strip_prefix("--").filter(|l| !l.is_empty()) {
            let (name, inline_value) = match long.split_once('=') {
                Some((name, _)) => (name, true),
                None => (long, false),
            };
            if "unset".starts_with(name) {
                i += usize::from(!inline_value);
            } else if !["ignore-environment", "null", "debug"]
                .iter()
                .any(|known| known.starts_with(name))
            {
                return false;
            }
        } else if let Some(cluster) = short_cluster(o) {
            for (k, c) in cluster.char_indices() {
                match c {
                    'i' | '0' | 'v' => {}
                    'u' => {
                        i += usize::from(k + c.len_utf8() == cluster.len());
                        break;
                    }
                    _ => return false,
                }
            }
        }
        i += 1;
    }
    true
}

/// The letters of a short option cluster: `iz` for `-iz`; `None` for a long
/// option, `-` or an operand.
fn short_cluster(arg: &str) -> Option<&str> {
    arg.strip_prefix('-')
        .filter(|c| !c.is_empty() && !c.starts_with('-'))
}

/// Whether `arg` is a long option naming one of `names` — in full or, since
/// GNU `getopt_long` and git's own parser take any unambiguous prefix
/// (`sort --outp=f`, `git grep --open-files-in=x`), abbreviated.
fn long_option_among(arg: &str, names: &[&str]) -> bool {
    arg.strip_prefix("--")
        .map(|long| long.split('=').next().unwrap_or(long))
        .filter(|given| !given.is_empty())
        .is_some_and(|given| names.iter().any(|name| name.starts_with(given)))
}

/// Shell syntax [`permissions::shell::simple_commands`] folds into its
/// neighbours instead of splitting on: a subshell or brace group, `|&`,
/// a here-document. What runs inside them is not what the segments say,
/// so a line holding one is never read-only.
fn has_unsplit_construct(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let (mut in_single, mut in_double) = (false, false);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\\' if !in_single => i += 1,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ if in_single || in_double => {}
            '(' | ')' => return true,
            '{' if i == 0 || chars[i - 1] != '$' => return true,
            '|' if next == Some('&') => return true,
            '<' if next == Some('<') => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// 命令替换 / 写文件的重定向 / 进程替换 / 展开里的赋值，存在即取消只读资格。
///
/// - `$(` 和反引号：替换出来的内容可以是任意命令，名单里的名字完全不能代表
///   这条命令实际会做什么。
/// - 写文件的重定向：按定义就是写。
/// - `<(`：进程替换同样会派生任意子命令。
/// - `${X:=v}`：给变量赋值，`CDPATH` 一类会改变之后 `cd` 落在哪。
///
/// 故意**不**区分引号内外：`'... > ...'` 这种引号里的重定向虽然不生效，但
/// 把它误判成"需要确认"只是多弹一次窗，而漏判是静默执行。保守方向优先。
fn has_shell_escape_hatch(cmd: &str) -> bool {
    if cmd.contains("$(") || cmd.contains('`') || cmd.contains("<(") {
        return true;
    }
    has_file_redirect(cmd) || assigns_through_an_expansion(cmd)
}

fn assigns_through_an_expansion(cmd: &str) -> bool {
    cmd.match_indices("${").any(|(at, _)| {
        cmd[at + 2..]
            .split('}')
            .next()
            .is_some_and(|inner| inner.contains('='))
    })
}

/// Does `cmd` redirect into a **file**?
///
/// Plain `>`-containment is too blunt: `cargo check 2>&1`, `cmd >&2` and
/// `ls 2>/dev/null` are everyday shapes, and the first two write nothing
/// at all — they only rewire one fd onto another. Treating every `>` as a
/// write meant nearly every real command the model issues lost read-only
/// status, which is a large, avoidable increase in prompts for no safety
/// gain.
///
/// So only `>&N`, `N>&M` and `>&-` (duplicating or closing a descriptor)
/// are skipped. `>&word` with anything else after it writes stdout and
/// stderr to the file `word`, and everything else — `>`, `>>`, `N>file`,
/// `&>file`, `>|file`, `<>file` — opens a file for writing. `2>/dev/null`
/// is deliberately *not* special-cased: it names a path, and carving out
/// individual "harmless" destinations is the kind of allow-list that
/// eventually gets something wrong.
fn has_file_redirect(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let skip_spaces = |mut j: usize| {
        while bytes.get(j) == Some(&b' ') {
            j += 1;
        }
        j
    };
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'>' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if bytes.get(j) == Some(&b'>') {
            j += 1;
        }
        j = skip_spaces(j);
        if bytes.get(j) != Some(&b'&') {
            return true;
        }
        let start = skip_spaces(j + 1);
        let mut end = start
            + bytes[start..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
        if bytes.get(end) == Some(&b'-') {
            end += 1;
        }
        let ends_the_word = bytes
            .get(end)
            .is_none_or(|b| b.is_ascii_whitespace() || b";|&<>)".contains(b));
        if end == start || !ends_the_word {
            return true;
        }
        i = end;
    }
    false
}

/// `find` 里能派生任意进程或直接删/写文件的谓词。命中任意一个，这条 `find`
/// 就不再是"只读"了 —— `find . -name '*.rs' -exec sh -c '...' \;` 的名字
/// 依然是 `find`，光看名字永远拦不住。
///
/// `-fls` / `-fprint` / `-fprint0` / `-fprintf` 不执行程序，但会写文件，
/// 同样取消只读资格。
const FIND_UNSAFE_PREDICATES: &[&str] = &[
    "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fls", "-fprint", "-fprint0", "-fprintf",
];

/// 参数层的一票否决：名字在白名单里、但参数出卖了它。
///
/// 1. `find` 的 `-exec` 家族 —— 见 [`FIND_UNSAFE_PREDICATES`]。
/// 2. 某个选项会**派生另一个程序**（`rg --pre`、`fd -x`、`git grep -O`、
///    `man -P`、`sort --compress-program`…）或**写文件 / 改仓库 / 设变量**
///    （`sort -o`、`uniq IN OUT`、`yq -i`、`git config k v`、`git branch -D`、
///    `printf -v`…）。见 [`option_runs_or_writes`] 与 [`git_forfeits_read_only`]。
/// 3. `ps` 打印环境。
fn args_forfeit_read_only(name: &str, args: &[&str]) -> bool {
    match name {
        "find" => args.iter().any(|a| FIND_UNSAFE_PREDICATES.contains(a)),
        "ps" => args.iter().any(|a| ps_arg_shows_environment(a)),
        "printf" => args
            .iter()
            .take_while(|a| a.starts_with('-') && **a != "--")
            .any(|a| a.starts_with("-v")),
        "git" => git_forfeits_read_only(args),
        _ => option_runs_or_writes(name, args),
    }
}

/// Per-command option table: a read-only name whose options run another
/// program or write a file forfeits read-only. The dangerous option
/// differs by command, so each is named rather than guessed.
struct OptionPolicy {
    /// Long options (without `--`) that run another program or write.
    bad_long: &'static [&'static str],
    /// Long options the command spells with a single dash (`lshw -dump`),
    /// which would otherwise read as a cluster of short letters.
    bad_single_dash: &'static [&'static str],
    /// Short letters that run another program or write.
    bad_short: &'static str,
    /// Short letters that take the next word as their value (so it is not
    /// a positional operand).
    value_short: &'static str,
    /// Two or more positional operands means the last is an output file
    /// (`uniq IN OUT`, `xxd IN OUT`).
    writes_with_two_operands: bool,
}

fn option_policy(name: &str) -> Option<OptionPolicy> {
    let p = |bad_long, bad_short, value_short, writes_with_two_operands| {
        Some(OptionPolicy {
            bad_long,
            bad_single_dash: &[],
            bad_short,
            value_short,
            writes_with_two_operands,
        })
    };
    match name {
        "rg" => p(&["pre", "search-zip", "hostname-bin"], "z", "", false),
        "fd" => p(&["exec", "exec-batch"], "xX", "", false),
        "man" => p(&["pager", "html", "config-file"], "PHC", "", false),
        "info" => p(&["output", "dribble"], "o", "", false),
        "file" => p(&["compile"], "C", "", false),
        "nm" => p(&["plugin"], "", "", false),
        "ack" => p(&["output", "pager", "ackrc"], "", "", false),
        "ag" => p(&["pager"], "", "", false),
        "bat" => p(&["pager"], "", "", false),
        "sort" => p(&["output", "compress-program"], "o", "o", false),
        "shuf" => p(&["output"], "o", "o", false),
        "tree" => p(&["output"], "oR", "o", false),
        "yq" => p(&["inplace", "in-place", "split-exp"], "is", "", false),
        "ss" => p(&["diag"], "D", "", false),
        "lshw" => Some(OptionPolicy {
            bad_long: &["dump"],
            bad_single_dash: &["dump"],
            bad_short: "",
            value_short: "",
            writes_with_two_operands: false,
        }),
        "uniq" => p(&[], "", "fsw", true),
        "xxd" => p(&[], "", "cglosR", true),
        "kubectl" => p(&["kubeconfig", "server"], "s", "olnfLc", false),
        _ => None,
    }
}

/// Does any option make `name` run another program or write a file?
fn option_runs_or_writes(name: &str, args: &[&str]) -> bool {
    let Some(p) = option_policy(name) else {
        return false;
    };
    let mut positionals = 0usize;
    let mut i = 0;
    while let Some(a) = args.get(i).copied() {
        if a.starts_with("--") {
            if long_option_among(a, p.bad_long) {
                return true;
            }
        } else if let Some(cluster) = a.strip_prefix('-').filter(|c| !c.is_empty()) {
            let word = cluster.split('=').next().unwrap_or(cluster);
            if p.bad_single_dash.contains(&word) {
                return true;
            }
            for (k, c) in cluster.char_indices() {
                if p.bad_short.contains(c) {
                    return true;
                }
                if p.value_short.contains(c) {
                    // The rest of the cluster, or the next word, is its value.
                    if k + c.len_utf8() == cluster.len() {
                        i += 1;
                    }
                    break;
                }
            }
        } else {
            positionals += 1;
        }
        i += 1;
    }
    p.writes_with_two_operands && positionals >= 2
}

/// `git <subcommand> …` — read-only in general — under an option or operand
/// that writes the repository or runs a program. `git remote` keeps it only
/// in the forms that read the configuration (bare, `-v`, `get-url`): `show`
/// and the rest contact a host, which `show <URL>` lets the argument pick.
///
/// Only the form with nothing between `git` and the subcommand is on the
/// read-only list (`READ_ONLY_PREFIXES`), so the global options that hand git
/// another repository or configuration (`-C`, `-c`, `--git-dir`,
/// `--config-env`, `--exec-path`…) never get this far.
fn git_forfeits_read_only(args: &[&str]) -> bool {
    let Some((&sub, rest)) = args.split_first() else {
        return false;
    };
    let wants_external =
        |a: &&str| long_option_among(a, &["ext-diff", "output", "open-files-in-pager"]);
    match sub {
        "config" => git_config_writes(rest),
        "branch" => git_ref_edits(
            rest,
            "dDmMcCfu",
            "",
            &[
                "delete",
                "move",
                "copy",
                "force",
                "set-upstream-to",
                "unset-upstream",
                "edit-description",
            ],
            &[
                "contains",
                "no-contains",
                "merged",
                "no-merged",
                "points-at",
                "list",
                "format",
            ],
        ),
        "tag" => git_ref_edits(
            rest,
            "df",
            "l",
            &["delete", "force"],
            &[
                "contains",
                "no-contains",
                "merged",
                "no-merged",
                "points-at",
                "list",
                "format",
            ],
        ),
        "remote" => {
            let lists = rest.iter().all(|a| matches!(*a, "-v" | "--verbose"));
            !(lists || rest.first() == Some(&"get-url"))
        }
        "grep" => {
            rest.iter().any(wants_external)
                || rest
                    .iter()
                    .any(|a| short_cluster(a).is_some_and(|c| c.contains('O')))
        }
        _ => rest.iter().any(wants_external),
    }
}

/// `git config` writes when it has a value to set, rather than a key to
/// read. The read forms (`--get`, `--list`, bare `key`) keep read-only.
fn git_config_writes(rest: &[&str]) -> bool {
    let mut positionals = Vec::new();
    for a in rest {
        if let Some(long) = a.strip_prefix("--") {
            match long.split('=').next().unwrap_or(long) {
                "get" | "get-all" | "get-regexp" | "get-urlmatch" | "get-color"
                | "get-colorbool" | "list" | "show-origin" | "show-scope" | "name-only"
                | "null" | "default" | "type" | "includes" | "no-includes" | "global"
                | "system" | "local" | "worktree" | "file" | "blob" | "show-names" => {}
                _ => return true,
            }
        } else if let Some(cluster) = a.strip_prefix('-').filter(|c| !c.is_empty()) {
            if cluster.contains('e') {
                return true; // -e / --edit
            }
        } else {
            positionals.push(*a);
        }
    }
    match positionals.split_first() {
        Some((first, tail)) => match *first {
            "get" | "get-all" | "get-regexp" | "get-urlmatch" | "list" => false,
            "set" | "unset" | "unset-all" | "add" | "replace-all" | "remove-section"
            | "rename-section" | "edit" => true,
            // Classic form: a key alone reads, a key and a value writes.
            _ => !tail.is_empty(),
        },
        None => false,
    }
}

/// `git branch` / `git tag`: a mutating flag, or a bare operand that names
/// a ref to create, forfeits read-only. A listing flag that takes a pattern
/// (`--list 'v*'`, `--contains HEAD`) keeps it.
fn git_ref_edits(
    rest: &[&str],
    mutate_short: &str,
    list_short: &str,
    mutate_long: &[&str],
    list_long: &[&str],
) -> bool {
    let mut has_operand = false;
    let mut has_list_context = false;
    for a in rest {
        if a.starts_with("--") {
            if long_option_among(a, mutate_long) {
                return true;
            }
            if long_option_among(a, list_long) {
                has_list_context = true;
            }
        } else if let Some(cluster) = a.strip_prefix('-').filter(|c| !c.is_empty()) {
            for c in cluster.chars() {
                if mutate_short.contains(c) {
                    return true;
                }
                if list_short.contains(c) {
                    has_list_context = true;
                }
            }
        } else {
            has_operand = true;
        }
    }
    has_operand && !has_list_context
}

/// Does `line` assign a dangerous variable ([`is_dangerous_variable`]) at a
/// command position? Setting any of them takes the line off the read-only
/// fast path.
fn sets_a_dangerous_variable(line: &str) -> bool {
    permissions::shell::assignments(line)
        .iter()
        .any(|(name, _)| is_dangerous_variable(name))
}

/// `ps eww`、macOS 的 `ps -E`、`-o environ` 会印出同一用户其它进程的环境变量——
/// daemon 自己的 API key 也在里面。宁可多问：不带 `-` 的 BSD 风格选项里有 `e`、
/// 带 `-` 的里有 `E`、格式里出现 `env`，都算。
fn ps_arg_shows_environment(arg: &str) -> bool {
    match arg.strip_prefix('-') {
        None => arg.contains('e'),
        Some(flags) => flags.contains('E') || flags.contains("env"),
    }
}

/// `default_deny_read()` 的路径拆成"相对 `$HOME` 的组件序列"，用于和命令参数
/// 做组件级比较。缓存一次即可 —— `HOME` 在进程生命周期内不变。
///
/// 比"字符串前缀匹配绝对路径"稳健：命令行里的写法五花八门
/// （`~/.ssh/id_rsa`、`.ssh/id_rsa`、`../.ssh/id_rsa`、`/Users/me/.ssh/id_rsa`），
/// 但它们都含有 `.ssh` 这一段组件。
fn denied_read_suffixes() -> &'static [Vec<String>] {
    static SUFFIXES: std::sync::OnceLock<Vec<Vec<String>>> = std::sync::OnceLock::new();
    SUFFIXES.get_or_init(|| {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        platform::sandbox::default_deny_read()
            .into_iter()
            .map(|p| {
                let rel = home
                    .as_ref()
                    .and_then(|h| p.strip_prefix(h).ok())
                    .unwrap_or(p.as_path());
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
                    .collect::<Vec<String>>()
            })
            .filter(|components| !components.is_empty())
            .collect()
    })
}

/// 单个词是否落在凭据目录里。按 `/` 拆组件后找连续子序列 —— `~` 和 `.`
/// 这类无信息组件先丢掉，所以前导 `~` 天然被解析成 `$HOME`。
fn path_arg_hits_denied_read(token: &str) -> bool {
    let cleaned = token.trim_matches(|c| c == '"' || c == '\'');
    if cleaned.is_empty() || cleaned.starts_with('-') {
        return false;
    }
    let components: Vec<String> = cleaned
        .split('/')
        .filter(|c| !c.is_empty() && *c != "." && *c != ".." && *c != "~")
        .map(|c| c.to_lowercase())
        .collect();
    if components.is_empty() {
        return false;
    }
    denied_read_suffixes().iter().any(|suffix| {
        suffix.len() <= components.len()
            && components
                .windows(suffix.len())
                .any(|w| w == suffix.as_slice())
    })
}

/// True if read only.
pub fn is_read_only(cmd: &str) -> bool {
    classify(cmd).read_only
}
/// True if destructive.
pub fn is_destructive(cmd: &str) -> bool {
    classify(cmd).destructive
}
