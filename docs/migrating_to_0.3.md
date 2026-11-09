# 从 0.2.x 迁移到 0.3

0.3 把引擎拆成了**内核 + 模块**(`ARCHITECTURE.md` §1):子 agent、MCP、skill 不再
硬编在 `runtime` 里,而是以 `runtime::module::Module` 装进会话。**默认构建的 `agixd`
功能与 0.2.x 相同**,RPC 协议只增不改。要改的是三类人:进程内嵌入引擎的、自己选
cargo feature 构建的、部署 `agixd` 的。

## 进程内嵌入方

### 改名与路径

| 0.2.x | 0.3 |
|---|---|
| `base::interface::scene::AgentScene` | `base::interface::scene::Scene` |
| `scene::scene::{CodingScene, ChatScene, …}`、`scene::scene::SceneRegistry` | `scene::{CodingScene, …}`、`scene::SceneRegistry` |
| `runtime::agent_tool::*` | `agents::agent_tool::*` |
| `runtime::agent_spawner_impl::RuntimeAgentSpawner` | `agents::spawner::RuntimeAgentSpawner` |
| `runtime::agent_tool::{AgentTypeDefinition, AgentTypeSource, parse_permission_mode}` | `base::agent_type::*`(`agents::agent_tool` 也重新导出) |
| `runtime::agent::{build_default_skill_manager, build_skill_manager_unwatched}` | `skills::module::*` |
| `tools::skill_tool::*`、`tools::register_skill_tool` | `skills::tool::*`、`skills::tool::register` |
| `tools::saas_stubs::McpAuthTool` | 删除(0.3.10 起;OAuth 见 `mcp_auth.md`) |
| `runtime::commands::CommandRegistry::from_skill_manager` | `from_skill_catalog`(收 `Arc<dyn SkillCatalog>`,`Arc<SkillManager>` 直接可传) |
| `agents::agent_tool::AgentTool::set_skill_manager` | `set_skill_catalog` |
| `model::tokens::TiktokenCounter` | `HeuristicCounter`;`estimate_input_tokens` / `estimate_block_tokens` 不再收 BPE 参数 |
| `daemon::{SessionPool, Assembly, config, …}` | 搬到 `assembly` crate;`daemon::…` 路径仍然可用 |

前两行是机械替换:

```sh
grep -rl 'AgentScene\|scene::scene::' --include='*.rs' . \
  | xargs perl -pi -e 's/AgentScene/Scene/g; s/scene::scene::/scene::/g'
```

### `Builder` 不再自带子 agent、skill、MCP

0.2.x 的 `runtime::agent::Builder::build()` 无条件注册 `Agent`、`Team*`、`Skill` 与
MCP 工具。0.3 里它们是模块,要的就装:

```rust
let builder = runtime::agent::Builder::new()
    // …scene / model / settings / tools…
    .module(Arc::new(mcp::McpModule::new(manager)))              // 0.2:.mcp_manager(manager)
    .module(Arc::new(agents::AgentsModule::new()))               // 0.2:自动
    .module(Arc::new(skills::SkillsModule::new()));              // 0.2:自动扫三层目录
```

| 0.2.x | 0.3 |
|---|---|
| `.mcp_manager(m)` | `.module(Arc::new(mcp::McpModule::new(m)))` |
| `.shared_agent_types(h)` | `.module(Arc::new(AgentsModule::new().shared_agent_types(h)))` |
| `.skill_catalog(m)` | `.module(Arc::new(SkillsModule::shared(m)))` |
| `.skill_provider(p)` | `SkillsModule::new().provider(p)` |
| `.mcp_servers(..)` | 删除(从未被读) |
| `Agent::init_mcp_skills()` | 删除(没有调用方);要把 MCP 工具当 skill,调 `SkillManager::register_mcp_skills` |

顺序要按 **mcp → agents → skills** 装:agents 要看到可以授予子 agent 的 MCP 工具,
fork 型 skill 要 agents 提供的 spawner。不装 agents 的会话没有 `Agent` 工具,
`"agent"` 型 hook 与 fork 型 skill 的派生被拒绝;不装 skills 的会话没有 `Skill` 工具与
skill 命令。

`Agent::skills()` 返回 `&dyn SkillCatalog`,`Agent::mcp()` 返回 `&dyn McpServers`
(都在 `base::interface`),不再是具体类型。自己写模块见 `extension_points.md`
「一组能力打成一个模块」。

### 依赖 `tools`、`runtime`、`base` 的 feature

- workspace 里 `tools` 与 `runtime` 的依赖改为 `default-features = false`。`web`
  (WebFetch / WebSearch)与 `structured-output`(StructuredOutput)是 feature;
  要它们的显式打开,例如 `runtime = { …, features = ["web", "structured-output"] }`。
- `base` 新增默认开启的 `http`(本机 HTTP 客户端)。只实现契约、不要 HTTP 栈的 crate
  可以 `default-features = false`。
- 进程内自己构建 reqwest client 的,先调一次
  `base::interface::exec::local::install_tls_provider()`:reqwest 0.13 用 ring,
  没装 provider 时第一个 client 就会 panic。证书根改为系统信任库,企业代理装在
  系统钥匙串里的根证书现在会被信任。

### 子 Agent 的并发与通信

- 前台子 Agent 现在跑在自己的 tokio 任务上(`ARCHITECTURE.md` §3.8),同一轮的几个
  `Agent` 调用、`TeamCreate` 同阶段成员真正并行。自己实现的 `Model`、`Tool`、hook 若
  暗含"子 Agent 与父 Agent 在同一个任务里"的假设(比如用 task-local 传状态),要改。
- 同时在跑的子 Agent 默认最多 6 个(`execution.max_concurrent_subagents`,整棵会话树
  共用),超出的 spawn 返回工具错误。`TeamCreate` 成员与常驻成员不计入。
- 后台子 Agent 结束时,父会话的对话里会多一条 `<task-notification>` 用户消息
  (`MessageOrigin::Injected`)。读 transcript 的代码按 origin 区分它与用户输入。
  自己的代码也可以经 `SessionState::post_notice` 给会话的 Agent 递话。
- `base::context::RunningTask` 新增 `ended`(配 `wait_ended()`)与 `inbox`
  (`TaskInbox`);`runtime::agent::Agent::steer_handle()` 把一个不经 `Agent::run` 驱动
  的 Agent 的 steering 队列交出来。
- 新工具 `TaskWait`、`TaskMessage`,在 `CodingScene` 里延迟加载;`TeamCreate` 成员另有
  `TeamMemorySave`、`TeamMemoryRead`。
- `team::prompt::WorkerContext` 新增 `team_memory` 字段,结构体字面量要补上(空串即可)。
- `Agent` 工具的 `model` 参数以前被忽略,现在生效,压过 agent 类型的 `model`。
- 子 Agent 的工具输入按它自己的工具校验(带 `output_schema` 的 `StructuredOutput` 以前
  按父会话那个不带 schema 的校验,什么都收);它的权限询问也受
  `permission_prompt_timeout_secs` 约束,超时按拒绝,以前一直等。
- 子 Agent 不再继承父场景 `Scene::extra_tools` 贡献的那份工具,而由自己的场景重新贡献,
  并受 agent 类型的 `allowed_tools` / `disallowed_tools` 约束。
- `RunningTask` 新增 `usage` 与 `structured_output`;`AgentSpawner` 新增有默认实现的
  `start_agent`。自己实现 `AgentSpawner` 的不用改。

### 会话级提示词

`Builder::session_prompt(SessionPrompt { append, override_text })` 给一个会话单独的
追加 / 覆盖提示词,不写任何 settings.json,也不传给它派生的子 Agent。对应 RPC 见下。

### WebSearch 只有一条路

0.2.x 在 Anthropic 协议的提供方上,适配层把 `WebSearch` 从工具表里拿掉,换成提供方自带
的 `web_search` 发出去;提供方搜索的调用又被当成一次普通工具调用交给引擎,结果块被丢掉。
0.3 里 `WebSearch` 在所有提供方下都是引擎的普通工具:模型调它,它另发一次调用让提供方
自己搜,结果作为工具结果回给模型。hook、权限规则、宿主看到的帧与 `inspectContext` 都只
见到 `WebSearch` 这一个名字;代价是每次搜索多一次模型调用。

- 默认用会话自己的模型搜;`task_models.web_search` 可以换一个提供方。没有自带搜索的
  提供方(所有 `openai_compatible` 的)对 `WebSearch` 回错误,说明要怎么配。
- `tools::register_web_search(reg, model, model_name)`:原来收 `&Settings`、自己从
  设置与环境变量拼 HTTP 客户端;`NativeSearchProvider::new(model, model_name)` 同理,
  `from_settings` 与 `UnavailableSearchProvider` 删除。
- `SearchProvider::search` 多一个 `session_id` 参数。
- `StreamParams` 新增 `provider_search`(结构体字面量补 `provider_search: None`);
  `ModelEvent` 新增 `SearchResults`,`ModelError` 新增 `Unsupported`,穷尽匹配要补分支。
  自己实现 `Model` 的,收到 `provider_search` 而做不到时回 `Unsupported`。

### Monitor 真的在后台跑

0.2.x 的 `Monitor` 说是后台监视，实际挡住这一轮直到脚本退出或超时(常驻的一直挂着),
打出的行也没有送到模型,工具结果只有行数。0.3 里它登记成会话的任务后立即返回任务 id;
打出的行 200 毫秒内的合成一条 `<task-notification>`,模型在下一步(或下一轮开头)读到,
没读的通知会并进下一条;脚本退出、超时、事件过多时最后报一次;`TaskStop` /
`session.stopTask` 停掉它,本轮被中断或会话关闭时也一并停掉。宿主经 `task_progress`
边跑边看到它的输出。

### token 估算

`model::tokens` 不再加载 cl100k 词表,改为按字符类估算:相对 cl100k,代码与 JSON
偏高 4–12%,中文偏低约 4%。依赖估算值的阈值(压缩触发)会有小幅变化;真实用量
仍以 API 返回的 usage 为准。需要精确计数的,经 `TokenCounter` 接入自己的实现。

## 自己选 feature 构建 `agixd` 的

`daemon` 的 feature 改按档位组织(`ARCHITECTURE.md` §1.2)。原来的命令一对一换成
带 `full` 的写法,含义不变:

| 0.2.x | 0.3 |
|---|---|
| `cargo build -p daemon` | 不变 |
| `--no-default-features --features plugins` | `--no-default-features --features full,plugins` |
| `--no-default-features --features plugin-compile` | `--no-default-features --features full,plugin-compile` |
| `--no-default-features --features plugin-packages` | `--no-default-features --features full,plugin-packages` |
| `--no-default-features`(两个载体都不带) | `--no-default-features --features full` |

单独 `--no-default-features` 现在是 **minimal** 档:只有内核,没有 MCP、子 agent、
skill、web 工具、WebSocket、载体与包方案。

`release-small` profile(fat LTO、`opt-level = "s"`、`panic = "abort"`)是新增的,
`release` 不变。

## 部署 `agixd` 的

- `agixd` 处理 SIGTERM,与 Ctrl-C 走同一条正常关闭路径。systemd 单元请用
  `KillMode=process`,让 daemon 自己决定哪些后台命令停、哪些留。
- 用 `release-small` 构建的,panic 会中止进程,需要 supervisor 拉起;配置示例、
  崩溃记录(`~/.agix/daemon/crashes/`)与进程组记录(`~/.agix/daemon/groups/`)
  见 `ARCHITECTURE.md` §1.3 与 README 的「Running `agixd`」。
- `--help` 的文字改为简短说明;`--version` 输出 `agixd <版本>`(原先是 `daemon <版本>`)。
  选项与默认值不变。
- 声明式场景:在 `~/.agix/scenes/<id>/` 放 `scene.json` + `prompt.md` 即可定义一个
  场景,经 `scene.activate` / `--scenes` 启用(格式见 `extension_points.md`
  「声明式场景」)。插件包可以用 `[scene] dir` 带一个。

## RPC 客户端

协议只增不改:

- `daemon.ping` / `daemon.status` 增加 `modules`:这个构建编入的模块。
- 在没编入某个模块的构建里,属于它的方法(目前是 `mcp.*`)返回 `METHOD_NOT_FOUND`,
  `error.data.module` 指明模块。默认构建不受影响。
- `session.create` 的 `options.prompt` 与 `session.setPrompt`:会话自己的系统提示词,
  从下一轮起生效,重建与重启后保留。
- `scene.reload`、`scene.register`、`scene.unregister`:不重启 daemon 增减场景。
- `session.messageTask`(capability `task_messages`):给运行中的后台子 Agent 发消息。
- 后台子 Agent 结束后,父会话下一次调模型时带着它的 `<task-notification>`,宿主不必
  再替模型轮询 `session.taskOutput`。
- `session.startAgent`(capability `host_agents`):宿主不经模型在会话下起子 Agent;
  新错误码 `TASK_LIMIT`(-32019)。
- 子 Agent 任务的条目多了 `detail.agent_session_id`、`usage`,`session.startAgent` 起的
  还有 `detail.tag` 与 `structured_output`。
- `prompt`、`subagent_progress`、`team_progress`、`skills_changed` 帧在两轮之间也会到,
  `turn_id` 为 `null`。以前它们攒到下一轮开始时才发,而且带着那一轮的 `turn_id`。
  子 Agent 的询问一直是用父会话的 `session_id` 回答。
- 任务边跑边推(capability `task_progress`):新帧 `task_progress`(Shell 命令与监视器
  新增的输出)与 `task_updated`(子 Agent 的用量、等待宿主回答);`subagent_progress`
  多了 `task_id`。子 Agent 有询问在等时,任务的 `status` 是 `waiting`,不认识这个值的
  客户端把它当 `running` 即可。

## 0.3.0 之后:到 0.3.6

0.3.x 之间不改协议版本,下面这些是嵌入方和客户端升级时要看的。

### 嵌入方

- **内置场景只剩 `code`、`harnix`、`mini`。** `coding` 改名 `code`,不留别名,状态目录随之是
  `~/.agix/scenes/code/`;`chat`、`research`、`demo` 删掉,用过它们的改用 `harnix`、`mini`
  或写一个声明式场景。`CodeScene::default()` 是有人值守的,`CodeScene::new(Interaction::Unattended)`
  是无人值守的那一版。
- **`Scene` 新增几个有缺省实现的方法**:`interactive()`(缺省 `false`,即无人值守)、
  `build_session_context()`、`bundled_skills()`、`path_style()`,含义见 `extension_points.md`
  「一个 agent 怎么呈现自己」。**自己写的 Rust 场景升级后缺省就是无人值守**:有人在场的,
  要覆盖 `interactive()` 返回 `true`,否则需要批准的调用会被直接拒绝。声明式场景对应
  `scene.json` 的 `"interactive"`。
- **会话事实不在系统提示里了。** 工作目录、平台、草稿目录由 `build_session_context()` 在会话
  开头作为一条上下文消息注入一次;技能清单(`skills.catalog`)是 `User` 块,每次调用作为对话
  开头的一条消息送出。从系统提示里读这些、或在组装拦截器里按位置找它们的,改成按块名找。
- **`TaskCreate` / `TaskGet` / `TaskList` / `TaskUpdate` 删掉。** "Task"只指运行中的任务
  (子 Agent、Shell 命令、监视器、程序);记账用 `TodoWrite`。`TaskCreated` hook 改在运行任务
  开始时触发。
- **会话目标**:新工具 `GoalCreate` / `GoalGet` / `GoalUpdate`;自己调度 turn 的嵌入方用
  `Builder::goal_rounds_from_owner()` 接管下一轮的开启,见 `extension_points.md`「会话目标」。
- **`RunCode`**:新 crate `ptc`、特性 `ptc`(`coding` 档及以上默认开)。`Tool` 新增
  `callable_from_code()` 与 `code_result()`,缺省不可从程序调;自己的工具要让程序用,覆盖它们。
- **`StructuredOutput`** 在 `code` 场景里只在宿主给了 `output_schema` 时注册;参数就是结果本身,一个字符串字段
  里装着整份符合 schema 的结果时会被拒绝。
- **模型缺省**:thinking 开、`ModelSettings::effort` 缺省 `High`、`max_tokens` 缺省 32000
  (`base::settings::DEFAULT_MAX_TOKENS`)。`StreamParams` 新增 `effort`,结构体字面量补上。
  按 token 计费敏感的部署,在设置里显式写回自己要的值。

### RPC 客户端

- `session.goal` / `session.setGoal` / `session.updateGoal` 与 `goal_changed` 帧;`turn_started`
  的 `initiator` 多了 `goal`。
- `session.tasks` 的 `kind` 多了 `code`(`RunCode` 的程序);不认识的 `kind` 照常显示即可。
- `session.inspectContext` 的 `system` 各块带 `role`。
- 场景 id `coding` 不再存在,`session.create` 带它会得到 `SCENE_NOT_FOUND`。

## 0.3.6 之后:到 0.3.7

### 嵌入方

- **`bypassPermissions` 不看任何权限规则。** `PermissionGate` 在 `validate_input` 之后对 bypass
  直接放行:deny / ask 规则、免 bypass 的路径、`.git` / `.agix` 的确认、工具自己的权限判定
  一概跳过。`~`、`$(`、反引号、UNC 这类会被误读的路径属于输入校验,bypass 下照样拒。
  `agixd` 的 bypass 会话也装规则 gate(这样输入校验、会话中途进计划模式都还在);只有宿主给了
  `Assembly.permission` 才用宿主的。要 deny 仍然算数,用 `acceptEdits` 或 `dontAsk` 加 allow
  规则。只想要"deny 规则挡在前面、其余照旧"的,用 `PermissionGate::deny_rules_only`。
- **`acceptEdits` 只多放行只读调用。** 工具自己没放行的 `Edit` / `Write`(写到根外、写
  `.git` / `.agix` 下)照样询问,无人值守的会话里就是拒绝。
- **沙箱的可写范围收窄了。** 项目的 `.git`、`.agix` 本身,`.git/{config,config.worktree,commondir}`、
  `.git/hooks/`,`.agix` 下的设置与 `mcp`、`hooks`、`agents`、`skills`、`plugins`、`rules`、`scenes`
  等配置目录都只读;`~/.agix` 只有 `base::paths::STATE_ROOT_WORKING_DIRS`(`scratchpad`、`memory`)
  可写;`/private/tmp` 等临时区不再整体放开,沙箱里的命令拿到会话自己的 `TMPDIR`。依赖在沙箱里
  `git config`、`git remote add` 的流程要挪到沙箱外。引擎自己跑的 git 一律带
  `base::frozen::git::ENGINE_GIT_OPTIONS`,会话的命令进沙箱时它也进同一个沙箱,由
  `base::frozen::git::EngineGit` 决定怎么起(`tools::bash::engine_git` / `engine_git_for` 给出
  会话的那一个):`FrozenContext::collect` / `collect_with_options` / `refresh_git` /
  `refresh_git_status` 与 `tools::worktree::create_worktree` / `prune_orphan_worktrees` 多一个
  `&EngineGit` 参数,不想进沙箱的传 `EngineGit::Direct`。`SandboxPolicy` 多了
  `only_additional_writable`;`PlatformSandbox::confine` 包得住任何程序,不再只认 `bash -c`。
- **`Monitor` 进沙箱**,与 `Bash` 共用包装与 `require_enforcement`。
- **自己调度目标轮的**(`Builder::goal_rounds_from_owner()`):一轮开跑前调用
  `SessionState::goal_claim_round(turn_id)`,返回 `false` 就不跑——目标已暂停、清除、改过,或
  这一轮已经有人跑了。同一时刻只会发出一个待跑轮。
- **`AgentSpawner::start_agent` 多一个取消令牌参数**,宿主起的子 Agent 随它取消;会话池传会话的
  子令牌。
- **`Builder::question_prompts(bool)`。** 有人值守的场景里,`AskUserQuestion` 只在打开它、或嵌入方
  自己给了 `.elicitation(...)` 时注册。`base::interface::scene::NEEDS_A_USER` 里的工具由 Builder
  在无人值守场景里统一摘掉,场景不必自己过滤。
- **`settings.compaction.threshold_tokens` / `keep_recent` 生效了**(之前写了没人读),字段改成
  可选,不写就用场景的 `token_budget()`。已经写了这两项的部署,升级后会按写的值压缩。
- **`ModelError::NotRecorded`**:回放的录像里没有这次辅助调用时返回;自己穷举 `ModelError` 的补一支。
  会话自动起名走会话自己的模型与项目绑定,录制时记成 `purpose: "title"` 的辅助调用。
- **`AgentEvent::PermissionPrompt` 带 `task_id`,新增 `AgentEvent::QuestionPrompt`**;穷举匹配
  `AgentEvent` 的补上。`SetModelError::Busy.current_turn_id` 改成 `Option<String>`,并多了
  `running_task_ids`(挡住重建的后台任务)。
- **RunCode**:程序里的 `agent()` 超过子 Agent 并发上限时排队等空槽;程序起的子 Agent 需要询问时
  一律拒绝;`Agent` 的 `background` / `team_name` / `worktree` 从程序里调用被拒绝。`ToolContext`
  多了 `from_code`。
- **`RunningTask` 的输出不再是公开字段。** 写用 `append_output`,读用 `output_text()`(全部保留的)、
  `output_tail(n)` 或 `output_from(偏移)`,后两者返回 `OutputSlice {text, from_byte, next_byte,
  dropped, closed}`;`output_dropped` 去掉了,`from_byte > 0` 就是前面有没给的。`report_progress()`
  不带参数;`finish_task` 关闭输出(`close_output`),之后写的不再记下。`cancel_running_task` 只记下
  停止,`task_finished` 由工作自己走到 `finish_task` 时发,至多等 15 秒。
- **`HistoryStore::snapshot_lineage`**:一个会话连同任意深度的侧链,`session.export` 用它取快照。
  有缺省实现,但缺省是逐个 `load`,不在同一时刻;自己实现 store 的,能挡一下写入就覆盖它。

### RPC 客户端

- **提问**:`session.create` 的 `options.prompt_types` 加上 `"question"`(能力位 `question_prompts`)
  后,模型的 `AskUserQuestion` 以 `prompt_type: "question"` 的 `prompt` 帧到达,用
  `session.respondToPrompt` 的 `answer` 回答;没打开的会话里模型看不到这个工具。
- **能力位**多了 `goals`、`ptc`(带这个特性构建时)、`question_prompts`、`task_output_offsets`。
- **任务输出按字节偏移读**(能力位 `task_output_offsets`):`session.taskOutput` 接受 `from_byte`
  (与 `tail_bytes` 二选一),应答多了 `from_byte` / `next_byte` / `dropped` / `final`;`task_progress`
  帧多了 `from_byte` / `next_byte`。跟随输出的宿主按 `next_byte` 增量拼接,`dropped > 0` 时明示
  中间丢了多少(做法见协议文档 §6.3 `session.taskOutput`)。老 daemon 不认 `from_byte`,会把它当成
  只要尾部 —— 先看能力位。
- **被停掉的任务,`task_finished` 晚到。** `session.stopTask` 之后 `session.tasks` 立刻是 `stopped`,
  `task_finished` 却要等工作收完尾(至多 15 秒),之前还会有带收尾输出的 `task_progress`。拿
  `task_finished` 当"停止已生效"的宿主,改看 `stopTask` 的应答或 `session.tasks`。
- `session.run_turn` 的 `turn_id` 不能是目标轮的形式(`goal-<修订>-<轮次>-…`),也不能以 `schedule-`、`cron-` 开头(`INVALID_PARAMS`)。
- 会话有一轮占着时 `setModel` / `setPrompt` 等需要重建的调用一律 `SESSION_BUSY`,`current_turn_id`
  在那一轮还没正式开始时为 `null`;被拒的改动不会记下。
- 还有后台任务在跑的会话不重建:`setPrompt`、换厂商的 `setModel` 返回 `SESSION_BUSY`,`data`
  多一项 `running_task_ids`;设置文件变化引起的重建推迟到任务都结束之后。重建之后会话结束了的
  任务和还没投给模型的 `<task-notification>` 都还在。
- 后台子 Agent 的询问不随前台一轮结束而从 `pending_prompts` 里消失,答了或任务结束才去掉。
- 同一连接重复 `daemon.subscribeEvents` 不再重复投递。
- **导出**:`session.export {session_id, path, format?, include?}`(能力位 `session_export`)把会话
  连同侧链、图片本体在同一时刻打成 zip 或目录,写到 daemon 所在机器上的绝对路径,带
  `manifest.json`(`format_version: 1`)。还没有 `session.import`。

## 0.3.7 之后:到 0.3.8

### 部署 `agixd` 的

- **`release` 就是小体积构建**:fat LTO、`opt-level = "s"`、`panic = "abort"`,`agixd` 约 11 MB。
  `release-small` profile 没有了,原来用它的改成 `--release`。要 panic 时展开、任务里的 panic
  只结束那个任务的,用 `--profile release-fast`(`opt-level = 3`,体积翻倍)。按 `--release`
  构建的部署现在 panic 会中止进程,需要 supervisor 拉起(`ARCHITECTURE.md` §1.3)。
- **工作线程数可配**:`--worker-threads <N>` 或环境变量 `AGIX_WORKER_THREADS`(命令行优先),
  不配时等于 CPU 核数。只管异步工作线程;文件读写、搜索、脚本走各自的阻塞线程池,不受影响。
  嵌入式板子上 `--worker-threads 1` 即可。

## 0.3.8 之后:到 0.3.9

扩展只剩两种:编译进宿主的 Rust 插件,和 QuickJS 脚本(与 `RunCode` 同一套虚拟机)。
WebAssembly 载体整个移除。

### 自己选 feature 构建 `agixd` 的

- `plugins`、`plugin-compile` 两个 feature 没有了,`agix-plugin-compile` 二进制也没有了。
  原来用 `--no-default-features --features full,plugin-compile` 的构建,改回默认构建。
- `scripts` 与插件包不再互斥:默认构建两样都带。

### 插件包作者

- `[[wasm]]` 段**拒装**,安装时报错。给宿主看的内容放 `x-` 前缀的段。
- `[scene.own]` 段**拒装**,改用 `[scene] dir`(`scene.json` + `prompt.md`,格式见
  `extension_points.md`「声明式场景」),它是 `own` 的超集。`[scene] visible_in` 删除。
- `[plugin.config]` 不再读取(它只服务组件的 `init`)。
- `[[agent]]` 现在在默认构建上生效;以前只有带 WASM 载体的构建兑现它。
- 包格式见新的 `plugin_packages.md`;`extending_wasm.md` 删除。

### 嵌入方

- `crates/wasm-host`、`crates/plugin-host`、`crates/plugin-compiler` 删除;
  `base::interface::capabilities` 删除(它只服务 WASM 组件的能力声明)。
- `hooks::config::HookConfig::Wasm`、`hooks::runner::WasmHookExecutor`、
  `HookRunner::with_wasm_executor`、`PluginHost::hook_executor` 删除。
- `SessionPool::load_plugin_components` 改名 `load_plugins`(现在只接管插件包的场景)。
- `plugin::locate::locate_tool` 删除。
- **Rust 插件接口定为 `runtime::plugin::Plugin`**:`Builder::plugin(..)` 可以加多个,
  `Assembly { plugins, .. }` 加给池里每个会话;插件随会话一起交给子 Agent。
  `runtime::plugin_host::PluginHost` 与 `Builder::plugin_host` 删除;只为加几个工具实现
  `PluginHost` 的,改成一个只调 `Registrar::tool` 的 `Plugin`。
- **同一个点上 Rust 与 QuickJS 的先后对调了。** 以前串行的点上(`tool.result`、
  `model.request`、`model.message`、`memory.retrieval_hook`)脚本碰巧跑在 `SessionCustomizer`
  加的 Rust 实现**之前**;现在 Rust 先、脚本后,与交给 `Builder` 的先后无关。`tool.around`
  上脚本仍在外层。见 `extension_points.md`「叠加顺序」。
- 在 `SessionCustomizer` 里设 `Builder::prompt_registry` 不再把脚本的块、变量、组装钩子
  冲掉:脚本叠在宿主的注册表上。
- **QuickJS 也走 `Plugin`。** `Builder::bound_scripts`、`Builder::script_carriers` 删除,改成
  `builder.plugin(Arc::new(script_host::ScriptPlugin::new(bound)))`;`AgentTool::with_delegate_scripts`
  删除,子 Agent 随插件继承。
- `Plugin` 有 `name`、`layer`(默认 `Layer::Rust`)、`register` 三个方法。插件名在会话里唯一;
  工具名、agent 类型名跨插件唯一;提示块名与变量名同层唯一——撞了 `build()` 失败。
  脚本之间撞名在绑定时报 `BindingError::NameTaken`。
- `runtime::module::SessionContext` 的 `plugin_host`、`delegate_scripts` 字段删除,换成
  `plugin_agent_types`、`delegate_plugins` 与 `plugin_tools`。
- `BoundScripts::prompt_variables` 的元素多了第三项 `BlockOrigin`。
- `Builder::scene_tool_filter` 改名 `contributed_tool_filter`,现在也收窄插件登记的工具。
  子 Agent 不再从父会话抄插件的工具,由它自己那次登记给出;`AgentTool::with_delegate_plugins`
  多一个参数,是这些插件在父会话登记的工具名。
- `plugin` crate 收拢成包的格式、发现、缓存、安装与披露:
  - `plugin::cli`、`plugin::marketplace`、`plugin::resolver` 删除(按名字从市场安装、`update`、
    依赖解析,daemon 从未接上);
  - `plugin::fetch` 改名 `plugin::install`,`install(cache, name, source)` 与
    `uninstall(cache, name, version)` 是函数,`PluginSource` 也挪到这里;
  - `plugin::homograph` 改名 `plugin::blocklist`,只留 `check_blocklist`;
    `PluginError::Homograph` 改名 `PluginError::Blocked`;
  - `PluginCache` 去掉 `cache_registry_index`、`load_registry_index`、`total_cached`。
- `assembly::plugins::PluginSubsystem::new` 去掉没用的工作区参数。
- 两种插件的开发指南是新的 `plugin_development.md`,样例在 `examples/plugins/`。

### RPC 客户端

- `plugin.install` 的 `disclosure` 去掉 `wasm`(`components` / `runnable`)与 `events`。
- `plugin.list` 的条目去掉 `component_faults`。
- `daemon.doctor` 的 `plugins.status` 不再出现 `packages-only`;带插件包的构建报 `enabled`。
- `session.get` 多一个 `health`:会话的插件注册了健康检查时才有。

## 0.3.9 之后:到 0.3.10

### 部署 `agixd` 的、RPC 客户端

- **`auto`、`yolo` 就是 `bypassPermissions`。** settings.json 的 `permission_mode`、
  `options.permission_mode`、agent 类型 frontmatter 里写这两个,得到的都是完全放行,报回来是
  `bypassPermissions`。以前 settings.json 里的 `auto` 被退回 `default`,`yolo` 与经 RPC 传进来的
  `auto` 实际按 `default` 询问——配了它们的现在不再询问。只想少问几句的,用 `acceptEdits`
  或 `default` 加 allow 规则。
- Agent 工具(团队成员)和 `TeamCreate` 的 `permission_mode` 不再拒收 `auto`,按
  `bypassPermissions` 处理;`bubble` 照旧拒收。
- `ExitPlanMode` 回到 `EnterPlanMode` 之前的模式,不再一律回 `default`。
- 计划模式里的 `ExitPlanMode` 不再被"plan mode forbids non-readonly tools"拒掉,而是照常询问
  (`prompt`,`tool_name: "ExitPlanMode"`):`permit` 退出,`deny` 留在计划模式。以前计划模式
  经 RPC 退不出来。
- `session.resume` 对活着的会话报它此刻的 `permission.mode`(进了、退了计划模式之后的),
  不再报建会话时的。
- `Bash` 的 deny / ask 规则按实际启动的程序再比一次(`"rm"`、`\rm`、`{rm,x}`、`/bin/rm`、
  `command -p rm`、`env` / `exec` / `sudo` / `nice` / `time` / `timeout` / `xargs` 包着的、
  `sh -c` / `eval` / `find -exec` 里的都算 `rm`);程序名读不出来的命令(`$CMD x`)在有带
  内容的 deny / ask 规则时改为询问(`dontAsk` 下拒绝)。以前它们按原文前缀匹配,都绕得过去。
  allow 规则不变。带引号的受保护路径(`~/".ssh"/id_rsa`)同样按受保护路径拒绝。
- 只读 `Bash` 命令的免询问更窄:参数里有 `$VAR`、`~`、替换的,`cd` / `pushd` 到不是项目内
  字面路径之后再读的,现在要询问。`echo $HOME` 这类也在其中。
- agent 类型 frontmatter 的 `permission_mode` 真正作用到子 Agent 的权限判定上,但只能比父会话
  严:以前写 `plan` 不起作用;项目或用户目录里写 `bypassPermissions` 的类型,以前会被当成放开,
  现在按父会话的模式。团队成员(`TeamCreate`、`Agent` 的 `team_name`)同样:继承父会话的全部
  规则,`permission_mode` 只能收紧,询问到父会话的宿主那里问。以前成员没有任何规则,
  `bypassPermissions` 整队免确认。
- 子 Agent 的询问等待上限跟会话的 `--permission-prompt-timeout`,不再固定 300 秒;在子 Agent 的
  询问上答 `permit_always`,记下的是那一条命令或路径,不再是整个工具。
- 子 Agent 能改它自己读过的已有文件了:以前读后再写的检查查的是父会话的读取记录,子 Agent 的
  `Edit` / `Write` 改已有文件总被"File must be read before editing"拒掉。
- skill 正文里的 `` !`cmd` `` 先过会话权限(按一次 `Bash` 调用):被 deny 规则拒的、要询问的
  不再执行,替换成写明原因的一行文字。要它照跑,给那条命令写 allow 规则;项目内的只读命令照旧
  直接执行。skill 自己的 `allowed-tools` 不覆盖它自己的 `` !`cmd` ``。
- 运行中新建或改动的 `<project>/.agents/skills/` 下的 skill 按项目 skill 对待(不带内容的
  `allowed-tools` 授权不生效),即使用户层有同名的 skill。以前热重载把它记成用户 skill。
- `permit_always` 的 `local` 第一次建出 `settings.local.json` 后,之后新建的会话不用
  `config.reload` 就能看到那条规则。
- `feature_flags` 只剩 `cached_microcompact` 与 `reactive_compact`。`team_mode`、
  `plugin_marketplace`、`extended_memory`、`experimental_agent`、`recorder_auto_detect`、
  `dream_task` 从来没有生效,删除;配置里留着照样能加载,只是被忽略。`AGIX_FEATURE_*`
  环境变量同样从未生效。
- 工具 `VerifyPlanExecution` 删除:它要读的计划正文从没有地方写入,调用总是报
  "No active plan found"。
- `session.export` 的 `runtime.json` 去掉 `plan_text`(一直是 `null`)。
- **MCP 的 OAuth 换了一套**(`mcp_auth.md`)。`mcp_servers` 里的 `oauth_provider` 删除:写了它
  的 server 配置整条被拒绝(启动与重载时发 `mcp_connect_failed`,`mcp.addServer` 报
  `INVALID_PARAMS`),报错里给出新写法。改成 `"oauth": {"client_id", "client_secret_env",
  "scopes", "callback_port"}`(都可省);只是想让要求登录的 server 能登录,什么都不用写。
  以前 `oauth_providers` 里的密钥字面量没有对应写法:放进环境变量,`client_secret_env` 写变量名。
  以前存在 `scenes/<id>/mcp/oauth/<provider>.json` 的令牌不再读取,要重新登录一次。
- 新方法 `mcp.auth.start` / `mcp.auth.complete` / `mcp.auth.cancel` / `mcp.auth.logout`,新通知
  `mcp_auth_required` / `mcp_auth_completed` / `mcp_auth_failed`,新错误码 `MCP_AUTH_FAILED`
  (-32021)。`mcp.status` 的元素多了 `auth`,`state` 多了 `auth_required`。
- 传输错误不再清掉 OAuth 令牌(以前任何一次重连都会清);`mcp.addServer` / `mcp.updateServer`
  加进来的 server 和启动时连的一样走 OAuth。
- 无人值守的 MCP server:`oauth.grant` 为 `client_credentials`(`client_secret_env` 或
  `private_key_jwt`)或 `enterprise`(企业托管授权;身份经新方法 `mcp.auth.setIdentity` 给)。
- MCP 策略文件多了两个可选键:`allowed_authorization_servers`(授权服务器 issuer 的 glob)与
  `enterprise_idp`。`daemon.doctor` 的 `mcp.policy` 详情相应多了这两项。
- **MCP 工具名按 API 的规则造。** 工具名(与 prompt 斜杠命令)是 `mcp__<server>__<tool>`,
  两段里 `[A-Za-z0-9_-]` 以外的字符换成 `_`;server 名有这种字符或长过 40 的,后面再接 `_`
  加六位哈希(`my server` → `mcp__my_server_<hash>__…`);整个名字超过 64 时工具段截短并接哈希。
  只用这些字符、不长的 server 名不变。名字怪的 server,权限规则要按新名字写,`MCP` 工具会列出
  每个工具的名字。工具的 `inputSchema` 顶层不是 `type: object` 的,
  按 object 报给模型。
- **MCP 有了时限。** `mcp_servers.<name>` 可带 `startup_timeout_ms` / `tool_timeout_ms`;
  缺省取环境变量 `MCP_TIMEOUT` / `MCP_TOOL_TIMEOUT`,再缺省 30 秒 / 10 分钟。握手不回、只回
  非 JSON 的 server 到时被停掉、报 `mcp_connect_failed`,不再把会话创建卡死;工具调用超时在
  server 端取消,报错。会话创建最多等 MCP 10 秒,没连完的后台接着连,连上后发 `mcp_connected`,
  会话在下一轮前换上。
- **MCP 工具调用断在途中不再重发。** server 在调用中途退出,这次调用报错(以前重连后再发一次,
  有副作用的工具会执行两遍);下一个请求重连。
- `mcp.status` 的 `state` 多了 `disconnected`、`connecting`、`failed`(带 `error`);
  `mcp_connect_failed` 的 `error` 写明原因,不再是"看 daemon 日志"。
- `session.run_turn` 的 `message` 为空或只有空白、又没有附件,报 `INVALID_PARAMS`,不再当作
  一轮发给模型。
- 模型调用的恢复对真实厂商生效了:过载(重试用尽的 529,或流里的 `overloaded_error`)配了
  `model.fallback_model` 就换它重发;尺寸被拒(400「prompt is too long」、413、流里的同类错误)
  压缩后重发。以前这些都直接以 `ENGINE_ERROR` 结束。
- 一次调用在回复要工具之前失败,它推过的 `text_delta` 不进 transcript;重发前会再发一次同
  `step` 的 `step_start`(协议 §7)。流在消息结尾之前断开,以前当作一个空回答正常结束
  (`stop_reason` 为空、用量为 0),现在是 `ENGINE_ERROR`。
- `pause_turn` 不再结束一轮:引擎把那条回复发回去让模型接着写。
- `session.interrupt` 在模型还没开始回答时(等响应、退避重试之间)也立即生效。回复写到一半被
  打断的一轮以 `stop_reason: "cancelled"` 结束,不再走正常收尾(`stop_reason` 为空、照跑 Stop
  hook、送达排队消息)。
- 并发安全的工具在它的 `tool_use` 到齐时就开始跑,`tool_result` 与权限询问可能夹在同一步后面的
  `text_delta` 之间到达。
- `plugin.install` / `plugin.uninstall` 的 `name`、`version` 只收 1–128 个 ASCII 字母、数字、
  `.`、`_`、`-`、`+`,不以 `.` 开头,别的报 `INVALID_PARAMS`(以前 `{"name": "/"}` 会把根目录
  下的目录逐个当版本删掉);`plugin.install` 还要求包里 `[plugin] name` 等于 `name`。
- 团队的 `SendMessage` 只投给团队声明过的成员,`peer` 不在其中时报错并列出成员;提示词改成与
  schema 一致的 `{"peer", "message"}`(以前教的 `to` / `summary` / `"*"` 广播从来不被接受)。
  不是纯 ASCII 文件名的成员标签,邮箱文件改名为 `mailbox/~<hash>.jsonl`。

### 嵌入方

- `AnthropicModel::stream` / `OpenAICompatibleModel::stream` 在返回前等到厂商开始回答:请求被拒
  (状态码、连不上、重试用尽)从 `stream()` 返回 `Err`,不再是流的第一项;两者都认传进来的
  `CancellationToken`,取消时 `stream()` 返回 `ModelError::Cancelled`,流里出 `Err(Cancelled)` 后结束。
  一个什么都没发就结束的 Anthropic 响应是 `ModelError::Network`。
- 流里的 `error` 事件按它的 `type` 分类(`overloaded_error` → `Overloaded`、`rate_limit_error` →
  `RateLimited`、`invalid_request_error` → `Api{400}`……),不再一律 `Api{500}`;Chat Completions
  流里的 `{"error":…}` 块同样成为 `Err`(`ChatCompletionChunk` 多了 `error` 字段)。
  `AnthropicError::StreamInterrupted` 映射为 `ModelError::Network`(原为 `Internal`)。
- `model::stream::ContentBlockStart` 与 `BlockDelta` 多了 `Unknown`:不认识的块类型、增量类型
  跳过这一块,不再让整条流解析失败;穷举匹配它们的要补一支。
- 自己实现 `Model` 的:流必须以 `ModelEvent::EndTurn` 收尾,没有它的流被当作中途断开。
- `runtime::streaming::execute_stream` 返回 `Result<StreamResult, StreamFailure>`(原为
  `Result<_, TurnError>`);`StreamResult` 多了 `cancelled`。
- `StopRecovery` 多了 `Continue`,`DefaultRecovery` 对 `pause_turn` 返回它;穷举匹配
  `StopRecovery` 的要补一支。

- `PermissionMode::Auto`、`PermissionMode::Yolo` 删除(`base::permission` 与
  `base::settings` 两份都删),用 `BypassPermissions`。`permissiveness()` 的取值跟着变了。
- 权限分类器整个删除:`permissions::gate::{AutoClassifier, ClassifyDecision}`、
  `PermissionGate::with_auto_classifier` / `with_yolo_classifier`、`permissions::llm_classifier`、
  `permissions::yolo`、`DecisionReason::Classifier`、`call_purpose::CLASSIFY`;一并删掉只服务它的
  `PermissionGate::{record_denial, record_success, consecutive_denials, total_denials,
  should_fallback_to_prompting}`。
- `SessionState` 多了 `enter_plan_mode()` / `exit_plan_mode()`;`set_permission_mode` 会忘掉
  进入计划模式前记下的模式。
- `Tool` 多了 `leaves_plan_mode()`(缺省 `false`),`Permission` 多了 `check_for_delegate()`
  (按子 Agent 自己的 `SessionState` 判;缺省忽略它、照 `check` 判),都有缺省实现,已有实现
  不用改。自己包一层 `Permission` 的,转发 `check_for_delegate`,子 Agent 的模式和它读过的
  文件才传得到里层。
- `PermissionGate::check` 的模式取 `ctx.permission_mode`,不再读 `ctx.session`;直接调 gate 的
  要把它填成会话当前的模式(`RuleSetPermission` 已经这么做)。
- `permissions::ruleset::RuleHit` 多了 `Unclear`(按 ask 处理),穷举匹配要补一支。
  新增 `permissions::invocation`、`permissions::shell::{segments, Segment, lex, Word}`。
- `AgentTypeSource::may_loosen` 只管 `max_turns`;`permission_mode` 一律钳到父会话。
- `base::permission::PermissionMode` 多了 `min_permissive`,并能 `into()` 成
  `base::settings::PermissionMode`。
- `base` 的 cargo feature `team-mode`、`plugin-marketplace`、`experimental-agent` 删除
  (什么也不控制)。`FeatureFlags::is_enabled`、`enabled_flags`、`apply_env_overrides` 删除。
- `SessionState` 去掉 `plan_text` / `set_plan_text` / `clear_plan_text` 与 `plan_slug` 那三个;
  `compaction::compact::PostCompactContext` 去掉 `plan_content`。
- `runtime::agent::Agent` 去掉 `set_plan_mode`、`register_running_task`、`clear_running_tasks`:
  压缩后的提醒现在直接读会话状态——处于 plan 模式、还在跑的后台任务,在丢掉或总结了历史的
  压缩(不含只清工具结果的 MicroCompact)之后各补一条。
- `tools::plan_verify` 模块与 `tools::plan_mode::{plan_text, plan_state_for_test}` 删除。
- `base::frozen::find_relevant_memories`、`base::memory::select_memories_with_llm`、
  `MemoryStore::scan_memory_headers` 删除;按相关性挑记忆用 `base::memory::select_memories_from`
  (生产路径 `MemoryRetriever` 一直走它)。
- `hooks::HookRunner::set_network`、`auth::OAuth2Client::with_network` 删除;后者要换出口直接设
  `net` 字段。
- `auth` crate 整个删除(`OAuth2Client`、`TokenStore`、`CallbackListener`、`PkceVerifier`、
  `ProviderConfig`……),它只有 MCP 在用;OAuth 现在是 `mcp::oauth`,协议部分用 rmcp。
- `mcp::oauth::{McpOAuth, McpOAuthStore, McpOAuthResolver}` 删除,`McpAuth` 取代 `McpOAuth`;
  `mcp::auth_tool::McpAuthTool`(从未注册)删除。
- `Assembly::mcp_oauth_resolver` 删除,换成 `mcp_credential_store: Option<Arc<dyn
  mcp::oauth::McpCredentialStore>>`(令牌存哪,默认 0600 文件)与 `mcp_oauth_client:
  Option<mcp::oauth::McpOAuthClient>`(动态注册时的客户端名、CIMD 地址)。
  `SessionPool::with_mcp_oauth_resolver` / `mcp_oauth()` 换成 `with_mcp_auth(store, client)` /
  `mcp_auth()`。
- `McpServerConfig::{StreamableHttp, Sse}` 的 `oauth_provider: Option<String>` 换成
  `oauth: Option<McpOAuthConfig>`;`oauth_provider()` 删除,有 `oauth()` 与 `oauth_capable()`。
- `McpManager::connect_all_with_oauth` 换成 `connect_all_with(servers, env, auth: Option<Arc<McpAuth>>)`;
  新增 `share()`(会话用的、共享连接的那一份)、`pending()`(等登录的 server)。
  `McpError` 多了 `AuthRequired` / `InsufficientScope`。
- `McpManager::from_clients` 改成 `async`,读好工具与 prompt 再返回。新增
  `connect_all_within(.., wait)`(到点先返回,其余经 `LateConnects` / `absorb()` 陆续进来)、
  `connecting()`、`failed()`;`ServerStatus` 多了 `connected`。
- `McpServerConfig` 每个变体多了 `timeouts: McpTimeouts`(`#[serde(flatten)]`;结构体字面量要补
  `timeouts: Default::default()`)。`McpError` 多了 `ConnectionLost`、`Timeout`;
  `McpClient` 多了带默认实现的 `is_connected()`。
- 新模块 `mcp::naming`(`tool_name`、`tool_names`、`server_segment`);
  `mcp::prompts::{split_command_name, MCP_COMMAND_PREFIX}` 删除,`McpToolAdapter` 多了 `named()`。
- rmcp 升到 3.9,开了 `auth`、`auth-client-credentials-jwt`、`auth-enterprise-managed`。
- `Assembly` 多了 `mcp_enterprise_idp: Option<mcp::config::EnterpriseIdp>`;`McpPolicy` 多了
  `allowed_authorization_servers`、`enterprise_idp` 两个字段(结构体字面量要补,或用
  `..Default::default()`)。`McpOAuthConfig` 多了 `grant`、`private_key_jwt`。
- `team::mailbox::MailboxStore::send` 返回 `Result<(), UnknownPeer>`,不是成员的收件人被拒;
  新增 `MailboxStore::check_peer` 与 `team::mailbox::mailbox_file`(标签到文件名)。
- `plugin::cache::check_component` 新增;`plugin::install::{install, uninstall}` 与
  `PluginCache::remove_version` 对不合规的 `name` / `version` 返回 `PluginError::Schema`,
  `remove_version` 还拒绝解析到 cache 之外的插件目录。
