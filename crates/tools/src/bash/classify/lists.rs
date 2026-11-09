//! The command lists classification matches against.

// 父模块 OBVIOUSLY_SAFE_PROGRAMS 中已有的程序应同步加到这里。
//
// **进这个名单的门槛**：命令必须做到"不管带什么参数都不会
// 写文件、不会执行别的程序"。只要存在一个参数能让它派生 shell，就不能进来 ——
// `classify_single` 只对名字做匹配，参数层的兜底只有 `args_forfeit_read_only`
// 那两条通用规则。因此不在名单里的：
//
// - `awk`：`awk 'BEGIN{system("...")}'` 直接执行任意命令，还有 `print > "file"`、
//   `| "sh"` 等多条写/执行路径。想靠扫描 program text 里的 `system(` / `print >`
//   把它捞回来并不稳（引号拼接、`ENVIRON`、`--source` 等绕法太多），干脆不收。
//   `gawk` / `mawk` 同理。
// - `less` / `more`：都支持 `!cmd` shell 逃逸（less 还有 `-s`/lesspipe）。
// - `env`：它是命令**前缀**不是命令，由 `classify::SAFE_WRAPPERS` 剥掉后分类被
//   包住的真命令。
//
// 留在名单里但受参数约束的：`find`（`-exec` 家族见 `FIND_UNSAFE_PREDICATES`）、
// 以及所有会读文件的命令（`cat`/`head`/`tail`/`xxd`/`strings`/`od`/`hexdump`/
// `bat`/`jq`/`grep`…）—— 参数指向凭据目录时由 `args_forfeit_read_only` 否决。
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
    // 网络诊断（只读查询）
    "nslookup",
    "traceroute",
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
    // `cargo build` / `cargo check` are deliberately absent: both compile the
    // crate, which runs its `build.rs` and proc-macros — arbitrary code.
    "cargo metadata",
    "cargo tree",
    "cargo --version",
    "cargo version",
    "rustc --version",
    "node --version",
    // 包管理查询
    "pip list",
    "pip show",
    "npm list",
    "npm view",
    "brew list",
    "brew info",
    "brew search",
    // Rustup 信息
    "rustup show",
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
