# Agix

**English: [README.md](../README.md)**

**一个拿来做产品的 agent 引擎,而不是一个拿来聊天的助手。**

---

## 目的

Agix 是 agent 产品底下的那台引擎:它运行一个**会话**——装配提示词、调模型、执行工具、
把上下文控制在预算内、写历史、把活委派给子 Agent——并把这一路上的每个决策都留给建在
它上面的产品。这里没有交互界面,那是宿主的事。

它面向的是在它之上做二次开发的人:

- **Rust 应用**把引擎当库链接进来,直接驱动一个 `Agent`。
- **其它任何东西**——IDE 插件、Web 前端、用别的语言写的编排器——运行仓库里附带的
  参考宿主 `agixd`,经 Unix socket、TCP 或 WebSocket 用 JSON-RPC 和它说话。

两者是同一台引擎。

---

## 为什么选 Agix

**一套引擎,机房里成百上千地铺开,板子上照样跑得动。** Agix 为大规模部署和嵌入式部署而
生:一个小小的二进制,心里有数的资源占用,没人盯着也能一直干下去的循环。

1. **小到哪里都放得下。** `agixd` 是一个 11 MB 的二进制,不用装任何运行时;空闲时内存约
   10 MB,启动约 30 ms。一个真实任务——四个子 Agent 并行外加一段程序,6 分钟里 91 次模型
   调用——内存峰值不到 30 MB,CPU 总共约 8 秒(M2 Max 上 `release` 构建实测)。异步工作
   线程用 `--worker-threads` 按硬件配,小板子上一个就够;用不到的模块在构建档位里整个去掉。
2. **一个进程,成批会话。** 一个 daemon 同时服务几十个会话,跨项目、跨场景,走 Unix
   socket、TCP 或 WebSocket。闲置的会话被回收,用到时从磁盘恢复。崩溃留下记录,残留的命令
   在下次启动时清掉,和别的服务一样交给 systemd 或 launchd 托管。
3. **放手不管也安全。** 场景标成无人值守,就没有任何东西等人:要审批的事直接拒绝并说明原因,
   写入不出你给的根目录,`Bash` 只在操作系统沙箱里跑(macOS 用 Seatbelt,Linux 用
   bubblewrap)。权限只来自配置,不来自任务文本,注入的内容给不了自己任何权限。
4. **同样的 token,干更多的活。** 有了 `RunCode`,模型写一小段程序来驱动工具——并行、循环、
   分发给子 Agent 并按 JSON Schema 校验结果——只把结果带回对话。目标模式让会话一轮接一轮干到
   完成为止。固定前缀约 5.8K token,跨会话共享厂商的前缀缓存;长会话自己压缩、接着干。在六个
   工程任务上,与另一个开源 agent 框架用同一个模型对比:**总耗时少 34%,提示 token 少 13%,
   每次都做对**([测法](../tests/live/tests/compare_dsh.rs))。
5. **主导权在你的产品手里。** 每个会话、任务、子 Agent、目标和询问,都能通过同一套 JSON-RPC
   看到、叫停、修改。45 个扩展点,用 Rust 或 QuickJS 脚本替换或拦截任何子系统,不用
   fork。每个会话都是只追加的日志,可以恢复、分叉、导出,也能不联网逐字节回放。

---

## 特性

**部署**
- `agixd`:JSON-RPC 2.0(NDJSON)daemon,走 Unix socket、TCP 或 WebSocket;后两者带
  令牌认证。
- 也可以把引擎嵌进自己的进程:用 builder 建一个 `Agent`,或者直接用 daemon 的整个会话池。
- `release` 构建出 11 MB 的二进制;`release-fast` 体积翻倍,换一点速度和 panic 时展开。
- 构建档位(`minimal`、`coding`、`full`),按模块开关:MCP、子 Agent、skill、web 工具、
  结构化输出、`RunCode`、WebSocket、脚本、插件、沙箱、OpenTelemetry。
- `--worker-threads` / `AGIX_WORKER_THREADS`;会话上限,超出时回收最久未用的;闲置超时;
  一个进程服务多个场景。
- 发现文件,客户端按场景找 daemon。SIGTERM 正常关闭;崩溃记录;被强杀后清理进程组。

**模型**
- Anthropic 与 OpenAI 兼容两种接口,托管模型和本地模型(llama.cpp、Ollama、vLLM 等)都能接。
- 按调用种类路由(`main`、`subagent`、`compact`、`memory`、`web_search`、`web_fetch`),
  会话可单独选厂商,中途可换模型。
- 退避重试,外加恢复策略:换模型、压缩后重试、调高 `max_tokens`。
- 对缓存友好的提示词:会话自己的信息放在第一条消息里,不常用的工具用 `ToolSearch` 按需加载,
  压缩时发出缓存编辑。

**自主运行**
- 目标模式:自动一轮接一轮,直到完成、受阻或预算用尽。
- 定时任务;带校验的计划模式;待办清单。
- 后台跑的 shell 命令、监视器、子 Agent 和程序,结束时都发通知回来。
- `RunCode`:一段调用会话工具(包括子 Agent)的 JavaScript 程序,跑在内嵌的 QuickJS 里。

**多 Agent**
- `Agent` 工具,带类型的 Agent:前台或后台,各有自己的模型和工具,结果按 JSON Schema 校验。
- 团队,有信箱和共享记忆。
- 子 Agent 跑在挂在父会话下的侧链会话里。
- 并发和深度上限;权限、hook、预算随委派传下去。

**工具**——40 个内建:文件、shell、web、任务、计划、目标、定时、git worktree、skill、团队、
MCP、询问、通知、监视、结构化输出(见下文[能力](#能力))。

**上下文**
- 先做 LLM 摘要,再退到更便宜的手段;有预测式触发、熔断,阈值可配。
- 会话记忆与召回。
- skill 目录。
- token 和输出预算。

**安全**
- 固定的权限顺序:先是工具自身的检查,再是规则(deny 永远优先),再是路径安全,最后是权限模式。
- 权限模式:`default`、`acceptEdits`、`plan`、`dontAsk`、`bypassPermissions`。
- shell 命令跑在操作系统沙箱里,git hook、git 配置、Agent 设置在里面只读;引擎自己的 git
  也在沙箱里跑。
- 无人值守场景只拒绝、不询问。
- 权限询问超时一律按拒绝处理。
- worktree 隔离。

**宿主控制**
- 六十多个 RPC 方法:创建、恢复、分叉、重建、导出会话;读历史,读下一次模型调用的确切上下文;
  答复权限询问和问题。
- 启动、发消息、停止、跟随任务,按字节偏移读任务输出。
- 换模型或提示词;管理场景、MCP 服务器、插件、定时任务。

**扩展**
- 公开的 45 个扩展点:30 个契约、9 个拦截点、6 个注册点。
- 编译进宿主的 Rust 插件,与 QuickJS 脚本;可以打成插件包安装,安装时强制披露。
- 三十个时机的 hook,后端有 command、prompt、HTTP、agent 四种。
- MCP 客户端(stdio、SSE、streamable HTTP、WebSocket);声明式场景;自定义 Agent 类型。
- MCP 授权(2025-11-25 规范,OAuth 2.1):发现、动态客户端注册、PKCE、刷新;登录由宿主经 RPC
  驱动,回调走回环或宿主自己的地址;无人值守的 server 用 client credentials 或企业托管授权;
  令牌存在 0600 文件或宿主自己的存储里([mcp_auth.md](mcp_auth.md))。
- 企业 MCP 策略:按 server 名、命令、URL 的允许与拒绝列表,从用户写不了的系统路径读
  (或 `--mcp-policy`);策略读不了就拒绝所有 server([ARCHITECTURE.md](ARCHITECTURE.md) §6.4)。

**记录**
- 只追加的 JSONL 历史:可恢复、可分叉,崩溃后可重建。
- 37 种遥测事件,可导出到 OpenTelemetry。
- 录制器:不联网,逐字节回放。
- 会话导出成 zip 或目录,子 Agent 一起带上。

---

## 架构

### 分层

一个分五层的 Rust 工作区。依赖只向下指,反过来的依赖会让
`daemon/tests/dependency_direction.rs` 失败。

```
 外壳      daemon ─────────── agixd:命令行、进程生命周期(锁、信号、崩溃收尾)
             │
 装配      assembly ───────── 会话池、(项目, 场景) 绑定、provider 路由、插件包、模块清单;
             │                JSON-RPC 服务与实例发现
             │
 模块      agents(+team) · mcp · skills · script-host · plugin
             │                经 runtime::module::Module 进入会话
 内核      runtime ────────── turn loop、提示词装配、工具调度、Module 接口
             │
           model · scene · history · session · compaction · telemetry · hooks · permissions · tools
             │
 契约      core(包名 `base`)── 所有 trait 与所有共享数据类型
```

- **契约。** `core` 定义 trait(`Model`、`Scene`、`Permission`、`Tool`、`HistoryStore`、
  `Compactor` ……)与数据类型,不依赖工作区里任何别的 crate。换掉一个子系统就是实现
  `core` 里的一个 trait,永远不用伸手进别的 crate 的内部。
- **内核。** 一个会话跑一轮所需的一切:turn loop(`runtime`)、模型适配(Anthropic
  Messages 与 OpenAI 兼容的 Chat Completions)、内建工具、场景、历史日志、上下文压缩、
  权限、hooks、遥测。内核 crate 从不依赖模块 crate。
- **模块。** 构建会话时插进去的可选能力:子 Agent 与团队、MCP、skills、脚本载体与插件
  包。只有装配层与外壳知道有哪些模块。
- **装配。** 把配置变成运行中的会话:合并各层设置、解析 provider 路由、把每个会话绑到
  它的项目和场景、管理会话池、经 JSON-RPC 对外服务。想把 daemon 的引擎放进自己进程的
  宿主,调用的是和 `agixd` 同一个入口。
- **外壳。** `agixd` 本身:参数、实例锁、信号、崩溃后的收尾。

### 一个会话怎么建起来

```rust
let (mut agent, events, input) = runtime::agent::Builder::new()
    .scene(scene)          // 提示词、工具白名单、预算 —— Arc<dyn Scene>
    .model(model)          // 任意后端 —— Arc<dyn Model>
    .settings(settings)
    .module(Arc::new(mcp::McpModule::new(manager)))   // 可选,按这个顺序:
    .module(Arc::new(agents::AgentsModule::new()))    //   mcp → agents → skills
    .module(Arc::new(skills::SkillsModule::new()))
    .build()?;
```

`build()` 先按场景收窄模块清单、检查每个模块声明的依赖,再建好这个会话自己的工具表、
事件总线、权限与 hooks,然后让各模块贡献自己的东西(`on_session`)。模块注册的工具和
内建工具受同一个场景白名单约束。有两样东西是**晚绑定**的,因为读它们的东西比提供它们的
先建好:agent spawner(由 `agents` 提供,给 agent 型 hook 与 fork 型 skill 用)和 skill
目录(由 `skills` 提供)。子 Agent 用父会话收窄后的模块清单装配,所以每一层嵌套都一样。

### 一轮是什么

`crates/runtime/src/turn.rs` 是主循环。**步骤顺序是骨架,写死;每一步上的决策开放。**

```
用户消息 ─ UserPromptSubmit hook · 斜杠命令 · 记忆召回 · 待读的通知
循环:
  1. 停不停          TurnPolicy::before_model_call
  2. 压不压          Compactor::should_compact
  3. 硬顶            ContextBudget::hard_cap
  4. 提示词          PromptAssembler · 注册的块 · 装配拦截
  5. 调模型          Model + ModelInterceptor + BackoffPolicy
                     └─ 出错:RecoveryPolicy —— 换模型 / 压缩重试 / 抬 max_tokens / 失败
  6. 复查            TurnPolicy::after_model_call
  7. 记账            BudgetPolicy::on_usage
  8. 工具            Permission → ToolMiddleware → Tool → ToolResultTransformer
                     └─ 然后送达这一步期间到的消息与通知
  9. 还有工具调用 → 再来一轮;否则 BudgetPolicy::on_output_target 决定是否续写
出口:finish_turn —— 先落盘,再宣布 TurnComplete
```

一轮只有一个出口:无论是 hook、预算、取消还是错误结束了它,transcript 都在同一处写盘、
`TurnComplete` 在同一处宣布,所以宿主可以把 `TurnComplete` 读作"空闲了,而且已经在盘
上"。一轮在跑时到的消息在步骤边界送达,永远不会落在一对 `tool_use` / `tool_result`
中间。

### 会话、场景、历史

- **场景**是无状态的行为:系统提示词、工具白名单、延迟加载的工具、预算、允许哪些模块。
  一个进程同时服务多个场景;每个会话选一个,按 `(项目, 场景)` 解析设置。场景可以用 Rust
  写,也可以是声明式的(`scene.json` + `prompt.md`)。
- **历史是日志。** 会话的状态是一份只追加的 JSONL transcript,由投影变成模型看到的
  消息。扩展把自己的条目写进同一条时间线;不认识的跳过,所以卸载插件不会让旧会话打不开。
  会话可以恢复、分叉,重启后照日志重建。
- **子 Agent 跑在侧链会话里**,挂在父会话下,经同一套 API 列出和读取。

### 并发

每个 agent——会话自己的、每个前台或后台子 Agent、每个团队成员——都是多线程 tokio
运行时上的一个独立任务。Agent 之间不共享可变状态,只经通道说话:

| 方向 | 通道 |
|---|---|
| 父 → 子 | 子 Agent 的收件箱,在它下一个步骤边界读到(`TaskMessage`、`session.messageTask`) |
| 子 → 父 | 结束时投一条 `<task-notification>`,父 Agent 下一个步骤边界读到 |
| 父等子 | `TaskWait`、`TaskOutput` |
| 成员之间 | 团队 mailbox 与团队共享记忆 |

上限各在一处:委派深度、每棵会话树同时在跑的子 Agent(默认 6)、团队每阶段并发。超了
就以模型看得见的错误拒绝,不排队。

### 执行与安全

工具经四个契约碰到机器——`Process`、`FileSystem`、`Network`、`Sandbox`——默认是本机
实现,测试用进程内实现。每条命令领一个进程组,停的时候整组停。权限按固定顺序裁决:工具
自己的判定,然后规则(**deny 永远优先**,不管来自哪一层),路径安全,最后是权限模式。
子 Agent 的询问经父会话到达宿主。

### 扩展面

引擎公开一份从代码生成的接缝清单:**45 个扩展点**——30 个**契约**(换掉一个子系统)、
9 个**拦截**(坐在进行中的某件事的路径上)、6 个**注册**(往一个集合里贡献)。用 Rust
就实现 trait 交给 builder;不重新编译引擎的,经下面几条路够到其中一部分:

| 途径 | 够到什么 | 代价 |
|---|---|---|
| **脚本**(QuickJS,默认) | 九个点:四个写提示词、两个围着工具调用、两个围着模型调用、记忆召回 | 一个 `.js` 文件加一行配置;每次调用微秒级 |
| **插件包** | MCP 服务器、脚本、声明式场景、agent 类型 | 一个带 `plugin.toml` 的 zip;要给模型看的文本安装时全部披露 |
| **Hooks** | 一轮里的三十个时点、四种后端:命令、提示词、HTTP、agent | 观察、改写输入、拦下工具调用、结束一轮 |

另外,任何说 MCP 的东西本来就是扩展。脚本和 `RunCode` 跑在同一套内嵌 QuickJS 里:限额
限的是开销,不是恶意代码,所以脚本能做什么按它从哪来定。引擎不带 WebAssembly 运行时,
需要的宿主在 Rust 接口上自己做。

### 构建档位

| 档 | 装配什么 |
|---|---|
| `minimal`(`--no-default-features`) | 只有内核:模型、turn loop、场景、会话、遥测、hooks、文件与 shell 工具 |
| `coding` | + MCP、子 Agent 与团队、skills、web 工具、structured output |
| `full`(默认,另带 `scripts` 与 `plugin-packages`) | + WebSocket 监听 |

`daemon.ping` 报出构建带了哪些模块;属于没编入的模块的方法回 `METHOD_NOT_FOUND`,并
指明是哪个模块。

### crate 一览

| 层 | crate | 职责 |
|---|---|---|
| 契约 | `core`(`base`) | trait、设置、消息、提示词类型、扩展点清单 |
| 内核 | `runtime` | turn loop、流式工具执行、`Builder`、`Module` |
| 内核 | `model` | Anthropic 与 OpenAI 兼容适配,重试与退避 |
| 内核 | `tools` | 内建工具(`web`、`structured-output` 两组是 feature) |
| 内核 | `scene` | 内建场景、`SceneRegistry`、声明式场景 |
| 内核 | `history` · `session` · `compaction` | transcript 日志、会话状态、上下文压缩 |
| 内核 | `permissions` · `hooks` · `telemetry` | 规则引擎、生命周期 hooks、遥测与录制 |
| 模块 | `agents` · `team` | `Agent` 工具、agent 类型、团队 |
| 模块 | `mcp` | MCP 客户端与工具适配、MCP OAuth 授权 |
| 模块 | `skills` | skill 目录与 `Skill` 工具 |
| 模块 | `script-host` | QuickJS 载体 |
| 内核 | `ptc` | `RunCode`:模型写的程序,在内嵌的 QuickJS 里跑([`ptc.md`](ptc.md)) |
| 模块 | `plugin` | 插件包:manifest、安装、缓存、披露 |
| 装配 | `assembly` | 会话池、配置、provider 路由、JSON-RPC 服务 |
| 外壳 | `daemon` | `agixd` 二进制 |

`crates/` 下 19 个 crate,加上 `daemon/`、五个测试成员与 `examples/plugins/rust` 的示例插件,
工作区共 26 个成员。完整的
图景——会话与场景、历史日志、执行层、锁、磁盘布局——在 [`ARCHITECTURE.md`](ARCHITECTURE.md)。

---

## 快速开始

### 运行 daemon

```sh
git clone https://github.com/openatta/agix && cd agix
export ANTHROPIC_AUTH_TOKEN=sk-...        # 或 ANTHROPIC_API_KEY;兼容端点另设 ANTHROPIC_BASE_URL
cargo run --release -p daemon --bin agixd # 监听 ~/.agix/scenes/code/daemon.sock
```

和它说话——每行一个 JSON 对象:

```sh
sock=$HOME/.agix/scenes/code/daemon.sock
echo '{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"project_root":"'$PWD'"}}' \
  | socat - UNIX-CONNECT:$sock
# → {"jsonrpc":"2.0","id":1,"result":{"session_id":"…",…}}

echo '{"jsonrpc":"2.0","id":2,"method":"session.run_turn",
       "params":{"session_id":"<id>","message":"列出这个工作区里的 crate"}}' \
  | socat - UNIX-CONNECT:$sock
```

一轮在跑时,`session.event` 帧不断推回来(文字增量、工具调用、工具结果、询问、任务
进度);这一轮结束时响应到达。常见的客户端用一条连接订阅会话(`session.subscribe`),
用另一条驱动它。协议参考 [`daemon_rpc_protocol.md`](daemon_rpc_protocol.md) 里记了
六十八个方法——会话、历史与上下文、任务与子 Agent、场景、配置、MCP、插件、定时任务
——每个文档里写的响应形状都有测试拿真实 daemon 的回答对照。

常用参数:`--scene` / `--scenes`(服务哪些场景)、`--listen` / `--listen-ws` 加
`--token`(带认证的 TCP / WebSocket)、`--session-cap`、`--worker-threads`、`--permission-prompt-timeout`、`--mcp-policy`。
每个运行中的 daemon 在 `~/.agix/daemon/instances.d/` 下写一个发现文件,客户端按场景
找到它,不用拼路径。

### 嵌入引擎

当库用——一个 agent,直接驱动:

```rust
let (mut agent, mut events, _input) = runtime::agent::Builder::new()
    .scene(Arc::new(scene::code::CodeScene) as Arc<dyn Scene>)
    .model(Arc::new(model::adapter::AnthropicModel::new(client)))
    .settings(Arc::new(settings))
    .build()?;

tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        if let AgentEvent::TextDelta { text, .. } = event { print!("{text}"); }
    }
});
agent.run_turn("列出这里的文件".into(), "turn-1".into(), CancellationToken::new()).await?;
```

场景、模型、设置三样必需;其余都有能用的默认,每个默认都是可以替换的 trait。完整程序
在 [`crates/runtime/examples/minimal.rs`](../crates/runtime/examples/minimal.rs):

```sh
ANTHROPIC_API_KEY=... cargo run --release -p runtime --example minimal -- "列出这里的文件"
```

当会话池用——把 daemon 的引擎放进自己的进程,设置分层、路由、历史布局、模块都相同,
再加上自己的场景:

```rust
let pool = daemon::assemble::pool(
    &load_daemon_config("claude-sonnet-4-6", 8192, None, "code", &paths),
    Arc::new(scene::code::CodeScene),
    daemon::Assembly { scenes: vec![my_scene], ..Default::default() },
)
.await?;
```

`agixd` 就是这一调用加上命令行与监听器;工作区里有别的地方用第二种方式组装会话池,
测试就会让构建失败。

### 扩展它

- 给某一个点换个做法 → 写脚本:一个 `.js` 文件加 `settings.json` 里一行
  ([`extending_quickjs.md`](extending_quickjs.md))。
- 打包一组能力——MCP 服务器、脚本、场景、agent 类型 → 做成插件包
  ([`plugin_packages.md`](plugin_packages.md))。
- 在自己的构建里加工具、拦截或提示词贡献 → 写 Rust 插件(`runtime::plugin::Plugin`),交给
  builder,或经 `Assembly` 加给每个会话。同一个点上 Rust 插件和脚本都有时,Rust 先、脚本
  说最后一句。两种插件怎么写见 [`plugin_development.md`](plugin_development.md),
  `examples/plugins/` 下各有一个能跑的样例。
- 换掉一个子系统 → 实现它的 trait([`extension_points.md`](extension_points.md) 列出了
  每一个,以及代价和谁能用)。

---

## 能力

### 工具——40 个内建

| 类别 | 工具 |
|---|---|
| **文件** | `Read` `Write` `Edit` `Glob` `Grep` |
| **Shell** | `Bash`(沙箱、路径安全、可控超时) |
| **Web** | `WebFetch` `WebSearch` |
| **任务**（运行中的子 Agent、Shell 命令、监视器） | `TaskOutput` `TaskWait` `TaskStop` `TaskMessage` |
| **规划** | `EnterPlanMode` `ExitPlanMode` `TodoWrite` |
| **目标** | `GoalCreate` `GoalGet` `GoalUpdate`(目标让会话一轮轮自动推进下去) |
| **定时** | `CronCreate` `CronDelete` `CronList` |
| **工作区** | `EnterWorktree` `ExitWorktree`(每个会话独立的 git worktree) |
| **协作** | `Skill` `Agent` `TeamCreate` `TeamList` `TeamDelete` |
| **协议** | `MCP` `ListMcpResources` `ReadMcpResource`,另外每个发现的 MCP 工具一个 `mcp__<server>__<tool>` |
| **交互** | `AskUserQuestion` `PushNotification` `Monitor` `StructuredOutput` |
| **程序** | `RunCode`(一段调用本会话工具的 JavaScript 程序,只把返回值交回) |
| **编辑 / 元 / 诊断** | `NotebookEdit` · `ToolSearch` · `Ping` `Sleep` |

`tools::register_builtin_tools` 装上其中 21 个自给自足的;其余需要会话状态,由 builder
或模块注册。延迟加载的工具只报名字,要用时经 `ToolSearch` 取 schema。`code` 场景不提供
`NotebookEdit`、worktree 两个工具、`Ping`、`Sleep` 和 `PushNotification`;
`StructuredOutput` 只在宿主给了输出 schema 时才有。场景分有人值守
(有人应答提示)和无人值守:`harnix`、`mini` 无人值守,`code` 创建时二选一。无人值守的会话
没有 `AskUserQuestion` 和计划模式,需要批准的调用直接拒绝而不是去问,`Bash` 只在 OS 沙箱里跑——
见 `docs/ARCHITECTURE.md` §3.2.3、§6.3。

### 模型路由

```json
{
  "providers": {
    "anthropic": { "api_type": "anthropic", "default_model": "claude-sonnet-4-6" },
    "local":     { "api_type": "openai_compatible", "base_url": "http://localhost:11434",
                   "default_model": "qwen3" }
  },
  "default_provider": "anthropic",
  "task_models": { "subagent": "local", "compact": "local" }
}
```

任务键 `main`、`subagent`、`compact`、`memory`、`web_search`、`web_fetch` 各自路由一类
调用;会话也能选自己的 provider。`WebSearch` 默认经会话自己的模型让提供方搜索,
`task_models.web_search` 可以换一个提供方。`WebFetch` 默认返回页面转成的纯文本(超过
50,000 字符截断);只有 `task_models.web_fetch` 配了 provider 时,才由那个模型读页面、按调用
的 `prompt` 作答。它不会退回到会话自己的模型。

### 上下文、遥测、录制

- **压缩**:先让 LLM 写摘要,不行再退到更便宜的办法(截断、微压缩、折叠),前面有预测式
  触发与熔断,以 cache edit 的形式下发,不白扔已经付过钱的提示缓存。
- **遥测**:37 种结构化事件,可导出到 OpenTelemetry,每次调用四个 token 计数,不属于任何
  一轮的调用——压缩、记忆、hooks、搜索——也算进去。
- **录制**:把任意 `Model` 包一层,一个会话就成了自包含的目录;回放逐字节一致、不用联网。

---

## 运行 `agixd`

| profile | 命令 | panic 时 |
|---|---|---|
| `release`(默认) | `cargo build --release -p daemon --bin agixd` | 中止进程;约 11 MB |
| `release-fast` | `cargo build --profile release-fast -p daemon --bin agixd` | 展开;任务里的 panic 只结束那个任务;体积翻倍,略快 |

agent 跑的命令各有自己的进程组,daemon 崩了它们还活着,所以 daemon 收两次尾:panic hook
在中止前停掉它们;被 `SIGKILL`、OOM 带走的,同名实例下次启动时停掉遗留的。崩溃记录在
`~/.agix/daemon/crashes/`。daemon 不会自己重启——放在 launchd 或 systemd 下、失败时
重启;systemd 下用 `KillMode=process`,由 daemon 而不是 systemd 决定停哪些后台命令。

配置按层叠加,优先级从低到高:内建默认 → `~/.agix/settings.json` →
`~/.agix/scenes/<scene>/settings.json` → `<项目>/.agix/settings.json`,每层可再带一个
`settings.local.json`;各层的 `permission_rules` 并列生效。完整的配置面是生成的
[JSON Schema](schemas/settings.schema.json)。

| 变量 | 作用 |
|---|---|
| `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` | 模型凭据(或由宿主提供自己的凭据来源) |
| `ANTHROPIC_BASE_URL` | 兼容端点 |
| `AGIX_DAEMON_TOKEN` | TCP / WebSocket 握手 token |
| `AGIX_CONFIG_HOME` | 全局根目录,默认 `~/.agix` |

---

## 构建与测试

Rust 1.80+。测试不需要网络,也不需要 key。

```sh
cargo build --workspace
cargo test  --workspace                                      # 全部离线
cargo test  -p live --features real-model --test real_model  # LIVE 层,对真实模型
```

六层,从单元测试到组装好的产品:各 crate 的测试;一张行为网,让真实的 `Agent` 对着写好的
模型回答跑;`daemon/tests`,走真实 socket;`tests/live`,经 JSON-RPC 驱动整个产品——一次
对脚本化的模型服务器,一次对录制代理后面的真实模型,既从模型那侧也从宿主那侧用。每层怎么
工作、怎么跑:[`tests/README.md`](../tests/README.md)。

---

## 文档

| 文档 | 用途 |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | 引擎怎么拼起来——分层、模块、一轮、会话与场景、历史、执行、并发、磁盘布局 |
| [extension_points.md](extension_points.md) | 每个接缝:能换什么、能往哪里贡献、能拦截什么,代价与谁能用 |
| [daemon_rpc_protocol.md](daemon_rpc_protocol.md) | JSON-RPC 参考——方法、事件、错误码、认证 |
| [plugin_development.md](plugin_development.md) | 写 Rust 插件与 QuickJS 插件,样例在 `examples/plugins/` |
| [extending_quickjs.md](extending_quickjs.md) | 写脚本扩展 |
| [plugin_packages.md](plugin_packages.md) | 插件包的格式、安装与信任 |
| [ptc.md](ptc.md) | `RunCode`:模型写的程序能调什么、权限、限额与提示词 |
| [mcp_auth.md](mcp_auth.md) | 远端 MCP server 的 OAuth:配置、登录流程、RPC 与通知、凭据存储、安全 |
| [migrating_to_0.3.md](migrating_to_0.3.md) | 嵌入方与客户端从 0.2.x 升级 |
| [schemas/settings.schema.json](schemas/settings.schema.json) | 生成的 `settings.json` schema |
| [tests/README.md](../tests/README.md) | 测试分层与 LIVE 层 |

## 设计原则

1. **库优先。** daemon 是参考宿主,不是产品。
2. **引擎管机制,你管策略。** 每个决策都在一个 trait 后面。
3. **扩展弄不坏引擎。** 失败只损失它自己那一份。
4. **默认安全。** 你可以主动放宽,不会意外变松。
5. **没有经过检查的说法不写**——这些文档也一样。

## 许可

Apache-2.0
