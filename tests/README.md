# Agix 测试系统

分几层，各自回答不同的问题：

| 位置 | 是什么 | 怎么跑 |
|---|---|---|
| `crates/*/src/**` 的 `mod tests` | 单元测试：一个函数/一个类型的行为 | `cargo test -p <crate>` |
| `crates/*/tests/*.rs` | crate 验收：从公开的缝进去，这个 crate 的承诺还成立吗 | `cargo test` |
| `daemon/tests/*.rs` | daemon 的 e2e：真的起一个 server，走真的 socket 收发 JSON-RPC | `cargo test -p daemon` |
| `tests/runner/tests/*.rs` | 引擎行为网：真的驱动 Agent，模型那端是写死的脚本 | `cargo test -p test-runner` |
| `tests/live/tests/*.rs` | 装配面：从 JSON-RPC 进去，模型那端是一个 HTTP 桩；同一批用例跑「本进程」和「另起一个 agixd」两遍 | `cargo test -p live`（跨进程那半在 `-- --ignored` 里） |
| `tests/live/tests/live_*.rs` | LIVE 层：同一套框架，模型那端换成真实模型，以二次开发者的身份黑盒地把特性用一遍 | `cargo test -p live --features real-model`，见下文「LIVE 层」 |
| `tests/scripts/*.sh` | 构建面的断言（无插件构建里没有插件依赖等） | `tests/scripts/locked_build.sh` |

其中三条要的不是同一个 daemon：一个构建只带一个载体，所以「没有脚本引擎的构建拿到
`scripts` 段怎么办」和「一个组件到底会不会被调起来」都只能由另一个构建回答。用例会
说清楚怎么建、往哪个环境变量里塞：`AGIX_TEST_DAEMON_BIN_NO_CARRIER` 和
`AGIX_TEST_DAEMON_BIN_PLUGINS`。CI 两个都建。

> **手工建别的载体时务必带上自己的 `CARGO_TARGET_DIR`。** 每种载体建出来的都叫
> `agixd`，而跨进程用例默认捡的就是 `target/debug/agixd` —— 往默认目录建一个
> `--no-default-features` 的 daemon，等于把那批用例脚下的二进制换掉了。换掉之后它们
> 不会说「你换了二进制」，只会说「脚本没进提示词」。用例里给出的两条建法都带着
> `CARGO_TARGET_DIR=...`，不是为了省空间，是为了这个。

除 LIVE 层外都不联网、不需要凭据，`cargo test --workspace` 一条命令跑完，CI 每次
push 都跑。LIVE 层要凭据、要花 token，不进 CI，改动涉及模型怎么用这些能力时手工跑。

脚本载体（QuickJS）的那一张网横跨其中几层，单独有一份设计说明：
`tests/testing_scripts.md`。要点是它为什么需要一本账——脚本每次调用拿到全新运行时、
没有文件系统，所以「这个点被触发了吗」只能从外面回答，而「失败的脚本什么都改不了」
意味着失败和从没绑上在外面长得一模一样。

## LIVE 层

桩层证明的是「模型这样回答时，产品做对了」；它答不了「真实模型拿到这套工具、提示词
和协议，能不能把事做成」。LIVE 层答后一个问题：`tests/live` 同一套 `World` /
`Daemon` / RPC 客户端，模型那端从 `ProviderStub` 换成 `LiveProvider`。用例只经
JSON-RPC 与盘上的文件跟产品打交道，和下游二次开发的用法相同。

**`LiveProvider` 是一个本机录制代理**，daemon 像指向桩一样指向它，它把请求转发给真实
端点、把 SSE 原样流回来。走代理而不是直接把端点交给 daemon，换来两样东西：

- 每个请求照样被记下，「子 agent 用的是哪个模型」「通知有没有进对话」仍然能从外面读；
- 凭据只在测试进程里，daemon 拿到的是假 key，由代理换成真的。

**凭据**从工作区的 `.env`（已在 `.gitignore`）读，读不到再看进程环境：

| 变量 | 含义 |
|---|---|
| `ANTHROPIC_BASE_URL`、`ANTHROPIC_AUTH_TOKEN`（或 `ANTHROPIC_API_KEY`） | Anthropic 兼容端点 |
| `ANTHROPIC_MODEL`（`AGIX_LIVE_MODEL` 优先） | 会话的默认模型 |
| `AGIX_LIVE_MODELS` | 逗号分隔的可用模型，给「每个子 agent 一个模型」的用例分配；缺省只有默认模型 |
| `AGIX_LIVE_MAX_CALLS` | 一个用例最多调几次模型，默认 400；超出后代理直接回错误，不再花钱 |
| `AGIX_LIVE_TURN_TIMEOUT_SECS` | 一轮的上限，默认 900 |
| `AGIX_LIVE_ENV_FILE` | 换一个 `.env` |

文件优先于进程环境，因为跑测试的那个 shell 很可能带着给别的工具用的 `ANTHROPIC_*`。

**真实层只用 Flash。** 配置里出现 Pro 模型名，`LiveConfig::load` 直接报错；代理也拒绝转发
任何会落到 Pro 上的请求——DeepSeek 的 Anthropic 端点把 `claude-opus*` 也映射到 Pro，所以
按名字判断时这一类同样算 Pro。下面例子里的三个名字都落在 Flash 上，名字不同只是为了让
「每个子 agent 一个模型」的用例分得出谁是谁。

```sh
cargo build -p daemon                       # 另起进程的那条用例要 target/debug/agixd
AGIX_LIVE_MODELS=deepseek-flash,deepseek-v4-flash,claude-haiku-4-5 \
  cargo test -p live --features real-model --test 'live_*' -- --nocapture
```

`--nocapture` 会打出每一轮的耗时、用到的工具、模型的回答，以及每个用例的调用次数和
token 用量。改了 `tests/live` 后 lint 也要带上 feature：
`cargo clippy -p live --features real-model --all-targets -- -D warnings`。

| 用例 | 用一遍什么 |
|---|---|
| `live_basics.rs` | Read / Write / Edit / Grep / Bash 改项目；`WebSearch` 是引擎的工具，经会话自己的模型让提供方搜索；`Monitor` 立即返回，日志里的 ERROR 在模型干活途中以通知送达、模型据此处理后用 `TaskStop` 停掉它，宿主边跑边看到那一行，并按 `task_progress` 帧的字节偏移拼出、再用 `session.taskOutput {from_byte}` 补齐的输出与整段输出一致；会话提示词与 resume；Plan 模式不改文件；模型用 `GoalCreate` 设目标后，自动轮一轮推进一步，直到它自己标完成；宿主用 `session.setGoal` 设的目标不需要再发消息就跑完；达不成的目标以 `blocked` 结束、之后不再开轮；带 `prompt_types` 开了提问的会话里，模型用 `AskUserQuestion` 问的问题经 `prompt` 帧到达宿主，宿主选的选项决定了写进文件的内容；`RunCode` 一个程序做完逐文件检索；另起进程的 `agixd` |
| `live_agents.rs` | 两个前台子 agent 各用自己类型指定的模型；前台子 agent 的询问在父会话这一轮里到达宿主（带这一轮的 `turn_id`），批准前它的任务是 `waiting`、批准后回到 `running`；后台子 agent 收宿主的消息、结束后通知父会话；`session.startAgent` 起的子 agent 交回满足 schema 的结构化输出，任务上的用量与代理记下的相等 |
| `live_team.rs` | `TeamCreate` 两个阶段，后一阶段从团队记忆里拿前一阶段存的东西 |
| `live_extensions.rs` | 项目 skill；MCP server 的工具；`PreToolUse` hook 看到工具调用 |
| `live_multi_agent.rs` | 主控 + 三个不同模型的后台 worker，每个 worker 里多个后台任务；人为制造一个任务失败、一个 worker 故障，修复后重跑，主控汇总 |
| `live_host.rs` | 宿主那一侧：父会话空闲时 `session.startAgent` 起修复子 agent，宿主边看边答它的询问（询问期间任务是 `waiting`）、有 deny 规则护着的测试脚本保持原样、用量与代理记下的相等，父会话正在跑一轮时再起一个评审子 agent（`notify_parent: false`）；旁观连接跟着一次构建收 `task_progress`，`daemon.event` 看见每一轮，重启引擎后同一项目两种路径写法的会话各列一次；宿主停掉跑飞的命令、这一轮照常收尾；宿主场景的 `extra_tools` 在前台和后台子 agent 里都能用；宿主改写用户消息、去掉工具结果、写扩展条目，`inspectContext` 报的工具与随后真实请求带的逐个同名 |
| `live_programs.rs` | `RunCode` 的三种用法：一个程序经 `agent()` 并行起四个子 Agent、按 schema 收回结果再合并写盘（只有输入文件里有的封签一路传上来）；后台运行的程序边跑边推 `log()` 行，结束时以通知回到会话，模型据此交回结果；`acceptEdits` 下 deny 规则在程序里照样拦住写入，程序看得见拒绝 |
| `live_unattended.rs` | `harnix` 场景、缺省权限、无人值守：项目内 Edit 与沙箱里的 Bash 照常做完，往实例授权的根之外写被拒绝且没有任何询问发给宿主；`AskUserQuestion` 与计划模式连名字都不出现 |
| `live_context.rs` | 手动压缩由模型写摘要（`FullCompact`），压缩前给的事实之后还答得出；一轮里读的文件撑过 `compaction.threshold_tokens` 时自己压缩、照样做完这一轮；`session.fork` 的分叉记得分叉前的、不知道源会话之后的；带 `output_schema` 建的会话一轮以符合 schema 的 `structured_output` 结束 |

**权限模式。** `LiveRun` 缺省用 `bypassPermissions`。这个模式下一条规则都不看，deny
规则也不生效（只剩输入校验与 `Bash` 的 OS 沙箱），所以要验证规则的用例得换成 `acceptEdits`
或 `default`（`live_programs`、`live_unattended` 就是这样）。

**断言只看结果，不看措辞。** 真实模型每次说法不同，能断言的是：盘上的文件、任务状态、
代理记下的请求，以及只有真跑过才拿得到的值——用例里的随机令牌由脚本在运行时从输入文件
算出来，汇报里出现它，说明结果确实是一层层传上来的。断言模型「说了某句话」的用例会
随模型版本时好时坏，不要写。

**和 DSH 对比：`compare_dsh.rs`。** 同一组任务、同一个模型，Agix 和 DeepSeek Harness 各自
接一个录制代理，各跑一遍：短任务（照规格写一个小程序）、长流程（修一个包里的三个缺陷让测试
通过，再加一个命令行入口）、长输入（回答一个约 1.4 MB 访问日志里的问题），以及日常用法——
同一会话两轮，在一个陌生的多模块项目（`tests/fixtures/compare_dsh/shop`）里先按缺陷报告定位
修复，再加一个功能；还有一个 workflow 式的审计（`tests/fixtures/compare_dsh/audit`）：四个包
二十四个模块，六个违背了自己的 docstring，要求按包分给四个子 Agent 并行审、修好、汇总成
`findings.json`，由项目之外的 `audit_check.py` 逐个校验契约，并核对没坏的模块一字未动、清单
恰好是那六个；以及一个批量改动（`tests/fixtures/compare_dsh/migrate`）：三十个模块里 58 处调用
从弃用的 `fetch_json(url, timeout_ms)` 迁到 `get_json(url, timeout_s=…)`，毫秒换成秒，调用写法有
位置参数、关键字、跨行、常量四种，一次文本替换做不完；由 `migrate_check.py` 拿新接口的桩把每个
服务重放一遍核对。产物用模型没见过的输入实际运行来判定；token 取 DeepSeek 在每个响应里报的数，工具调用数取这些响应里的
`tool_use` 块，都从代理读、子 Agent 的也算在内，两边计量口径相同。它要一个
`dsh` 启动器，所以不在 `live_*` 的通配里：

```sh
npm i --prefix /some/dir @deepseek-ai/dsh
AGIX_DSH_BIN=/some/dir/node_modules/.bin/dsh \
  cargo test -p live --features real-model --test compare_dsh -- --nocapture
```

`AGIX_COMPARE_CASES=stats,biglog` 只跑其中几项；`AGIX_COMPARE_REPEAT=3` 每项跑三遍，表里
是中位数——单次读数波动很大，下结论至少跑三遍。汇总表打到 stderr，明细（含每次调用的未命中
/命中/输出 token）写进 `tests/output/compare_dsh/<seed>.json`，两边各自第一个带工具的请求
原样存在旁边的 `<seed>-<case>-<harness>.request.json`，用来对比系统提示和工具表；每一遍最长的那个
存成 `<seed>-<case>-<harness>-<第几遍>.last.json`，主对话的整个过程都在里面。断言只有
两条：两边都没用到 Pro，Agix 的产物全部通过；DSH 的结果只记录不断言。两边都是 thinking
开、effort high（表后每行的 `request` 里能看到实际发出的字段）。Agix 跑的是 `harnix` 场景、
缺省权限策略——和 DSH headless 一样无人值守，权限检查不绕过。

**宿主的那一侧用 `Watcher` 看。** `LiveRun::watch` 开第二条连接只订阅会话，按到达顺序
留下每一帧，可以让它照批准回答每个询问（回答前先读一次 `session.tasks`，留下当时的任务
状态）——和宿主的监控视图是同一种用法。`LiveRun::start_with_scenes` 让本进程的 daemon
带上宿主自己的场景；`LiveRun::restart` 换一个 daemon 接着用同一个世界。

**提示词写成操作步骤。** 用例要验证的是产品能力，不是模型的规划能力，所以让模型做什么、
用哪个工具、失败了怎么办都写明。用例里的「操作员」是测试代码：它盯着 `runs.log` 和
`session.tasks`，看到故障就修复，再经 `session.messageTask`（给 worker）或
`on_busy: "steer"`（给主控）告诉模型；主控的一轮在活还没干完时就结束了，它就再推一轮，
次数会打印出来——这个数本身就是一个观测值。

## 四个项目那条流水线

`tests/live/tests/four_project_pipeline.rs` 是唯一一条**把各层放到一起
互相为难**的用例：一个 daemon 同时服务四个项目（A 产出数据 → 人工评审提交 → B/C
并行消费 → D 判定 → 打回重跑），每个项目有自己的 settings、skill、子代理类型，D 还
跑在另一个场景上。

单看每一层「B 能看见 B 的」都成立，问题出在放到一起之后：断言永远是「B 看得见自己
的，且看不见 A/C/D 的」，而且四个会话同时活着、其中两个同时在跑。配套的：无项目
会话的 transcript 不进任何项目目录；D 的场景被停用之后，它的会话必须报
`SCENE_MISMATCH` 而不是被默认场景静默接管。

**两个并发的 turn 不能共用一条脚本队列。** `ProviderStub::script()` 是单条 FIFO，
两个 turn 在飞的时候谁先到谁拿走下一条，一个要两次调用的 turn 会拿到另一个 turn 的
第二条回答。`script_when(needle, replies)` 按请求内容分队列——用项目自己的那个标记
当 needle，每个会话就有自己的脚本。

## 几个约定

**模型那一端总是假的，但假法有四种，各答一个问题。**

- `test_runner::scripted_model::ScriptedModel` —— 回答写在测试里。问的是「引擎拿到
  这串回答会怎么决策」，包括 529 过载、`prompt_too_long`、`max_tokens` 续写这些
  没有哪个 provider 会按需产出的结局。
- `daemon/tests/session_lifecycle.rs` 的脚本化 `AnthropicClient` —— 扎在 adapter
  **下面**，所以工具循环、权限门、流式帧都是真的在跑，只有网络那一端是假的。
- `telemetry` 的 `RecorderModel` —— 录制真实对话再按位置回放。它是**产品能力**
  （运维者可以录自己的会话拿去排查），不再是测试基础设施：本仓库不再录制、也不再
  提交任何录像。用它的方式见 `docs/ARCHITECTURE.md` 与 `settings.json` 的
  `recorder` 段。
- `live::ProviderStub` —— 一个说 Anthropic SSE 的 HTTP 服务器。它扎得
  最深：请求序列化、SSE 解码、退避重试全都真的在跑，而这三样在前三种假法里都被跳
  过了。它也是**唯一能给另一个进程用的**假法——`Arc<dyn AnthropicClient>` 递不进
  子进程，`ANTHROPIC_BASE_URL` 可以。桩记下收到的每个请求，「脚本/插件改了发出去
  的东西吗」在 daemon 这一层只有这一个观测点。

  它不是录像：回答写在 Rust 里，对提示词措辞不敏感。**不要**对整个请求做全量比
  对——那会让它退化成一份每次改提示词就全挂的录像。

**黄金轨迹要读 diff 再提交。** `turn_behavior_net.rs` 把事件流和会话日志归一化后比
对黄金文件，`AGIX_UPDATE_GOLDEN=1` 重新生成——反射式重生成的黄金文件是一种更慢的
「没有测试」。

**测试进程不碰你的 `~/.agix`。** 每个测试自己拿 tempdir 当状态根；
`daemon/tests/home_is_never_discovered.rs` 保证没有哪个 crate 会自己去找 `$HOME`。
跑测试之后 `~/.agix` 里不该多出任何东西——如果多了，那是 bug。同理，起子进程的测试
要自己给子进程一个 cwd：daemon 的「项目」就是它的工作目录，继承下来的那个是仓库本身。

**文档不许漂移。** 这些都由测试比对，文档写错了会挂：

| 测试 | 盯住什么 |
|---|---|
| `daemon/tests/protocol_doc_matches_dispatch.rs` | `daemon_rpc_protocol.md` §6 与 `dispatch` 的方法集合一致 |
| `daemon/tests/protocol_doc_examples.rs` | 文档里每个响应示例都和真 daemon 的回答逐字段一致 |
| `daemon/tests/extension_points_doc.rs` | `extension_points.md` 的表由 `catalog` 生成 |
| `crates/core` 的 `settings_schema_matches_committed_file` | `docs/schemas/settings.schema.json` 由 `Settings` 类型生成（`#[ignore]`，CI 的 ignored 那一步跑） |
| `daemon/tests/readme_matches_the_code.rs` | `README.md` 里可数的那些声称——workspace 成员数、RPC 方法数、扩展点数、hook 事件数、遥测事件类型数、内建工具数，以及工具表里每个名字都还实装着 |

**每个 RPC 方法都得有人调。** 上面那两条盯的是「文档和 dispatch 说的是同一批方法」，
都答不了「这个方法有没有被跑过」。`protocol_doc_matches_dispatch.rs` 的
`every_method_a_client_can_send_is_driven_by_a_test` 答这个：dispatch 里的每个方法，
加上握手用的 `daemon.auth`，都得有某个测试发过它——直接写方法名，或者调
`rpc-client` 里发它的那个包装。

这条是有来历的：`plugin.reload` 的两个调用点分别在两个互斥的 `#[cfg]` 里，没有哪个
构建能同时编进去，而 CI 当时对这两种配置**只 `cargo check` 不 `cargo test`**——方法
在，文档在，测试也在，就是从来没跑过。所以 `carriers` job 里那两步现在跑的是
`cargo test`。

## fixture 项目

`tests/fixtures/template_project/` 是模板项目：有 `.agix/settings.json`（hooks +
MCP）、`.agents/skills/`、`AGENTS.md`。用例跑之前会整个拷贝到临时目录，改动不会污染
这份原件。`tests/fixtures/scripts/` 是脚本载体九个点各自的 fixture，
`tests/fixtures/scripts_outside/` 那一个故意放在项目根之外，用来验出身判定。

## 磁盘

一次干净构建 **~13 GB**，这不是浪费：每个集成测试文件都是一个独立二进制，静态链接
整张依赖图（光 wasmtime + cranelift 的 rlib 就 507 MB），所以单个测试二进制约 190 MB，
而这样的二进制有几十个。strip 只能省 18 MB——macOS 上调试信息本来就不在二进制里，
那 190 MB 是实打实的机器码。

**变成浪费的是 cargo 从不回收。** 被取代的那一套会原地留着，和活着的那套一样久。

两条规矩：

| 时机 | 做什么 |
|---|---|
| **改完 workspace 版本号** | `cargo clean` |
| 每周 | `cargo sweep --time 7`（`cargo install cargo-sweep`） |

第一条是硬性的：版本号一变，23 个 crate 的指纹全变，整套重编而旧的一份不会被删——
这就是本仓库反复出现「几十 GB 突然爆掉」的直接原因。全量重建约 1 分 10 秒，比排查磁盘
满便宜得多。

`tests/scripts/disk_report.sh` 会报告当前占用、**7 天没被碰过的字节数**（即
`cargo sweep` 能回收的量），以及每一种「多出一套产物」的成因。
