# ipdrun-06：回放时提示里的日期是今天；宿主设不了会话的时钟

类型：新能力｜优先级：中

## 需求

严格回放（`recorder.strict: true`，`Divergence::Strict`）逐块比对这次的请求与录下的请求（`crates/telemetry/src/recorder/mod.rs` 的 `diverges`）。
组装的提示里有今天的日期（`crates/core/src/frozen/mod.rs` 的 `today`、`crates/runtime/src/turn.rs` 注入 claudeMd 时的 `currentDate`），
都取自 agent 的 `Environment`；经 daemon 建的会话从不设它（`crates/assembly/src/session_pool.rs` 组装 `Builder` 处），于是是机器的时钟。
结果：一份录像第二天就严格回放不了，宿主只能开非严格、把发散当噪声数。`Builder::environment` 这个接缝在，但只有进程内嵌入方够得着。

## 建议（只加不改）

- `base::interface::environment::ShiftedEnvironment`：从给定时刻起、按墙上时钟往前走的时钟（`starting_at` / `starting_at_ms`），id 照旧新鲜。
- `session.create` 的 `options.clock: { start: "<RFC 3339>" }`：daemon 给这个会话装从 `start` 起的 `ShiftedEnvironment`。
- 回放（`recorder.mode = replay`）且没给 `clock`：从录像开录的时刻（录像头的 `created_at`）起——提示里的日期与录下来的那天相同。
  `telemetry::recorder::recorded_at(root, name)` 只读录像的第一行。
- `start` 写错只记警告、用机器的时钟（`ClockOptions` 的字段带缺省，不会让整份 `options` 因形状不对被当成没传）。

## 兼容性

不给 `clock`、不回放的会话：行为不变。回放的会话：日期从今天变成录像那天——非严格回放的发散只会变少；会话日志里的时间戳也从录像那天起
（相对顺序与间隔不变）。录制不受影响。协议只多一个可选字段。

## 验收

- `crates/core` 单测：`ShiftedEnvironment` 从给的时刻起、往前走、id 不重复；毫秒换算的日期对。
- `daemon` 单测 `a_session_clock_is_the_hosts_start_else_the_recordings_else_the_machines`：宿主给的起点优先；回放取录像头的日期；录制、
  没有录像、起点写错 ⇒ 不换时钟。

## 依赖

无。宿主一侧打开严格回放见 Harnix ipdrun-45。

## 来源

IPDrun 的回放档（Harnix 的 `--replay-dir`）只能非严格：提示里的日期与绝对路径让录像隔天就对不上（Harnix `with_recording` 的注释），
发散数靠阈值兜着。

## 实现记录

- `base::interface::environment::ShiftedEnvironment`（`starting_at` / `starting_at_ms`）；`telemetry::recorder::recorded_at(root, name)`；
  daemon `rpc::ClockOptions { start }`（字段带缺省）、`SessionOptions.clock`；`session_pool.rs` 的 `session_clock` 在组装 `Builder` 时装上
  （宿主给的起点优先，其次回放的录像头 `created_at`，都没有 ⇒ 不装，即机器的时钟）。与草图一致。
- 测试：`crates/core` 的 `a_shifted_clock_starts_where_it_is_told_and_keeps_running`、daemon 的
  `a_session_clock_is_the_hosts_start_else_the_recordings_else_the_machines`。
- 提交：54f3938。
