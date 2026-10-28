# ipdrun-01：会话结束后，命令留在后台的进程没有人收

类型：缺陷｜优先级：高

## 现象

`LocalProcess` 让每条命令自成一个进程组（`crates/core/src/interface/exec/local/process.rs:63`），停止时对整组发信号
（同文件 `:170-180`）。但句柄 drop 时，只有**组长还在跑**才 `killpg`（`:154-163`）。命令本身很快结束、组里留下的进程
（`nohup sleep 600 >/dev/null 2>&1 &`、`npm run dev &` 这类）从此没有人记着：

- `session.close`（`daemon/src/session_pool.rs:4377` 的 `shutdown_session`）、会话池满时的 LRU 逐出（`:6104`）、
  daemon 收尾（`:5467` 的 `shutdown_all`）都不碰它们；
- 嵌入方停掉整个引擎后它们还在，父进程变成 1。

这些进程会一直占端口、写文件、吃 CPU，下一次在同一目录里起的会话还可能和它们抢资源。嵌入方只能自己用 `ps`
按工作目录去找，是启发式，`cd` 出去的进程就找不到。

## 建议

会话记下它起过的每个进程组，会话结束时把还活着的组收掉。只加一个开关，缺省关。

- `LocalProcess` 可以带一份进程组台账：
  ```rust
  pub struct GroupLedger { /* Mutex<BTreeSet<pid_t>> */ }
  impl GroupLedger {
      pub fn new() -> Arc<Self>;
      pub fn groups(&self) -> Vec<libc::pid_t>;      // 还活着的（killpg(g, 0) 成功的）
      pub async fn reap(&self, grace: Duration);      // TERM，等 grace，KILL
  }
  impl LocalProcess {
      pub fn with_ledger(env: SubprocessEnv, ledger: Arc<GroupLedger>) -> Self;  // 新增；`new` 不变
  }
  ```
  spawn 成功后把组号记进台账；每次记新组时顺手丢掉已经空了的组（`killpg(g, 0)` 返回 `ESRCH`），
  把「组空了以后组号被系统重用」的窗口压到最小。
- 设置层加 `sandbox.reap_process_groups: bool`（与 `sandbox.subprocess_env` 并列，`crates/core/src/settings.rs:578`），
  缺省 `false`。打开时会话池给每个会话一份台账（子 Agent、后台 Shell 等任务用所属会话的那份），在
  `shutdown_session`、LRU 逐出、`shutdown_all` 里调 `reap`。
- 进程内嵌入（`runtime::agent::Builder`）同样可以自己建 `GroupLedger`、经 `ExecProviders` 交进去，引擎退出时调 `reap`。
- `session.get` / `daemon.status` 可选地报出某会话台账里还活着的组数，方便宿主确认收干净了（可后做）。

daemon 自己被 `kill -9` 时台账来不及用，那一半要由起 daemon 的宿主负责（例如把 daemon 放进自己的会话、停时对整个会话
发信号），不在本条范围内。

## 兼容性

- 缺省关：不设 `sandbox.reap_process_groups` 的应用行为完全不变，故意留在后台的服务照旧活着。
- `LocalProcess::new`、`ExecProviders::local_with` 签名不变；台账只在新构造函数里出现。
- 协议不变；`session.get` 多出的字段（若做）是可选的新字段。
- 打开后，会话结束会杀掉它留下的后台服务。这是开关的本意，文档里写明。

## 验收

- 单元（`process.rs` 的测试模块，仿 `kill_after_exit_still_reaches_what_was_left_running`）：`sh -c "sleep 300 & echo $!"`
  跑完、句柄 drop 之后，`ledger.groups()` 里还有这个组；`reap` 后 `sleep` 在 3 s 内消失。
- 组已空时 `groups()` 不再列出它。
- daemon（`daemon/tests/session_lifecycle.rs`）：开关打开，会话里 Bash 起一个后台 `sleep`，`session.close` 后它消失；
  开关关着时它还在（缺省行为不变的回归）。
- `shutdown_all` 与 LRU 逐出各一条同样的用例。

## 依赖

无。Harnix 侧对应的是 Harnix `ipdrun-07`（spawn 实例的进程组），两条可以分开落地。

## 来源

IPDrun 的执行 Agent 用 Bash 起了 `nohup … &` 的后台进程，宿主停掉实例后它们一直活着；IPDrun 现在靠 `ps` + 工作目录
的启发式清理（`crates/agent/src/reap.rs`），想换成引擎自己记账。
