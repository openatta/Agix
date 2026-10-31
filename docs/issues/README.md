# 下游项目提的需求

这个目录放用 Agix 的项目提过来的需求：缺陷、要加的能力、文档该补的地方。一个需求一个文件，文件名是
`<项目>-NN-<英文短名>.md`，前缀是提出的项目，`NN` 由那个项目在本仓库里自己编号。

每份的结构相同：类型与优先级；现象或需求（带代码位置）；建议（接口草图，只加不改）；兼容性；验收（本仓库的测试怎么证明）；
依赖；来源（提出方在哪里撞到的，一两句）。需求、接口、验收只用 Agix 自己的概念；提出方的业务只出现在「来源」里。

落地时，提交说明里写上对应的编号（如 `ipdrun-03`）。

## IPDrun

提出于 2026-10-27，代码位置对照 `e8a1dd8`（`Agix-v0.2.9-2026102603`）；ipdrun-05 对照 `2e5ade3`（`Agix-v0.2.9-2026102701`）。

| 编号 | 标题 | 类型 | 优先级 | 依赖 |
|---|---|---|---|---|
| [ipdrun-01](ipdrun-01-session-process-groups.md) | 会话结束后，命令留在后台的进程没有人收 | 缺陷 | 高 | — |
| [ipdrun-02](ipdrun-02-resumed-session-name.md) | 恢复的会话不报存着的名字，下一轮还会重新自动起名 | 缺陷 | 中 | — |
| [ipdrun-03](ipdrun-03-prompt-middleware.md) | 进程内嵌入没有「消息进一轮之前」的原生拦截 | 新能力 | 中 | 拒收 steer 的回告依赖 ipdrun-04 |
| [ipdrun-04](ipdrun-04-steer-bypasses-prompt-hook.md) | 步骤边界送进去的消息（steer）不经 UserPromptSubmit | 缺陷 | 中 | — |
| [ipdrun-05](ipdrun-05-prompt-middleware-attachments.md) | prompt 中间件判断一条消息时看不到它带的附件 | 新能力 | 中 | ipdrun-03、ipdrun-04 |

Harnix 那边与之相关的：Harnix `ipdrun-07`（spawn 实例的进程组，对应 ipdrun-01 的另一半）、Harnix `ipdrun-13`（经 `EngineContrib`
把 ipdrun-03 交给模块）。

## Grabix

提出于 2026-10-31，代码位置对照 `a228cf2`（`Agix-v0.2.9-2026102702`）。

| 编号 | 标题 | 类型 | 优先级 | 依赖 |
|---|---|---|---|---|
| [grabix-01](grabix-01-relative-paths-for-replay.md) | 文件工具的说明与结果都以绝对路径为准，录像换了工作目录就回放不了 | 新能力 | 中 | — |
