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
| `tools::saas_stubs::McpAuthTool` | `mcp::auth_tool::McpAuthTool` |
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

### RPC 客户端

- `plugin.install` 的 `disclosure` 去掉 `wasm`(`components` / `runnable`)与 `events`。
- `plugin.list` 的条目去掉 `component_faults`。
- `daemon.doctor` 的 `plugins.status` 不再出现 `packages-only`;带插件包的构建报 `enabled`。
