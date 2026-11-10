# Agix 架构

Agix 是一个**库**,不是一个应用。它提供的是"跑一个 agent 会话"这件事的引擎:
装配提示词、调模型、执行工具、管理上下文与历史。**交互界面不在这里**——那是使用方
应用的事。`daemon` 是仓库里附带的一个参考宿主,
把引擎包成 JSON-RPC 服务,同时也是集成测试的载体。

这份文档讲**组件、概念、和它们之间的约束**。总览与快速开始见仓库根目录的 `README.md`
(中文版 `README.zh-CN.md`)。想知道怎么用请看:

- `daemon_rpc_protocol.md` —— 通过 RPC 使用它
- `extension_points.md` —— 能扩展什么(总揽)
- `extending_quickjs.md` —— 怎么用脚本扩展;`extension_points.md` —— 全部扩展点
- `plugin_development.md` —— 怎么写 Rust 插件与 QuickJS 插件,样例在 `examples/plugins/`
- `migrating_to_0.3.md` —— 从 0.2.x 迁移

---

## 1. 分层

```
 外壳    daemon ─────────── agixd:参数、进程(锁、信号、崩溃收尾)
           │
 装配    assembly ───────── 会话池、(项目, 场景) 绑定与失效、provider 路由、插件包、模块清单;
           │                JSON-RPC 服务(Unix socket / TCP / WS)与实例发现
           │
 模块    agents(+team) · mcp · skills · script-host · plugin
           │                经 runtime::module::Module 进入会话
 内核    runtime ────────── turn loop、提示词组装、工具调度、扩展点调度、Module 接口
           │
         model · scene · history · session · compaction · telemetry · hooks · permissions · tools
           │
 本机    platform ───────── 执行层的本机实现(进程、文件、网络、OS 沙箱)、崩溃钩子、进程身份
           │
 契约    core(包名 base)── 所有 trait、所有共享数据类型
```

**依赖方向**(`daemon/tests/dependency_direction.rs` 检查):

- 内核 crate 只依赖契约与其它内核 crate,**不依赖任何模块 crate**。
- 模块依赖契约与内核;模块之间的依赖只经 `Module::requires` 声明的顺序。
- 只有装配层与外壳知道有哪些模块;模块 feature 的 `#[cfg]` 只出现在
  `assembly`(模块清单、会话池的 MCP 部分、RPC 服务)与 `daemon` 里。
- `platform` 只依赖契约,契约不依赖它。用它的是真要碰本机的 crate——`tools`、`runtime`、
  `model`、`hooks`、`permissions`、`mcp`、`plugin`、`agents`、`assembly`、`daemon`,清单与
  各自的理由在 `MAY_USE_PLATFORM`;`scene`、`history`、`session`、`compaction`、
  `telemetry`、`ptc`、`team`、`skills`、`script-host` 只经契约,测试里用到的除外。

RPC 服务和会话池在同一个 crate 里是体积上的取舍:两者共用大量泛型实例(serde_json、
tokio、rmcp),分在两个 crate 时各编一份,release 的 `agixd` 因此大约 2.8 MB。

**`core` 里没有任何第一方依赖。** 它定义 trait 与数据类型,别的 crate 实现它们。
这条约束是整个扩展面成立的前提:一个宿主要替换某个子系统时,面对的是 `core` 里的
一个 trait,而不是某个具体 crate 的内部结构。执行层的本机实现也不在 `core`:
"本机"是 `platform` 对这些 trait 的一种回答,`in_process()` 是另一种。本机 HTTP 客户端
(`LocalNetwork`)在 `platform` 默认开启的 `http` feature 下,OS 沙箱后端在它的 `sandbox`
feature 下(`assembly` / `daemon` 的同名 feature 转发过去)。

| crate | 层 | 职责 |
|---|---|---|
| `core`(包名 `base`) | 契约 | 所有 `trait`、`Settings`、消息与提示词类型、扩展点清单、`SkillCatalog` / `McpServers` 等内核面向模块的接口 |
| `platform` | 本机 | 执行层四个契约的本机实现(`LocalProcess`、`LocalFileSystem`、`LocalNetwork`、`PlatformSandbox`)与 `local_providers*`;进程组台账与孤儿收尾、崩溃钩子、进程身份 |
| `runtime` | 内核 | 轮次循环、流式解析、工具调度、`Module` / `SessionContext` |
| `model` | 内核 | 模型协议适配(Anthropic Messages / OpenAI Chat Completions),重试与退避 |
| `tools` | 内核 | 内建工具;`web`、`structured-output` 两组是 feature |
| `scene` | 内核 | 内建场景、`SceneRegistry`、声明式场景(§3.7) |
| `history` / `session` / `compaction` / `telemetry` / `hooks` / `permissions` | 内核 | 日志、会话状态、上下文压缩、遥测与录制回放、hooks、权限 |
| `agents` | 模块 | 子 agent(`Agent` 工具)、agent 类型目录、团队工具;依赖 `team` |
| `mcp` | 模块 | MCP 连接、工具适配器、资源工具、prompt 命令、OAuth 授权 |
| `skills` | 模块 | skill 目录与 `Skill` 工具 |
| `script-host` | 模块 | QuickJS 脚本载体 |
| `ptc` | 内核 | `RunCode`:模型写的一次性程序,在内嵌的 QuickJS 里跑,经 `ToolContext::dispatch` 调本会话的工具(`docs/ptc.md`);`ptc` feature,与脚本载体用同一套 QuickJS,两者各自独立开关 |
| `plugin` | 模块 | 插件包:manifest、获取、缓存、披露;包里的 MCP、脚本、场景、agent 类型 |
| `assembly` | 装配 | 会话池、配置加载、provider 路由、自检、模块清单,以及其上的 JSON-RPC 服务与实例发现 |
| `daemon` | 外壳 | `agixd` 二进制:参数、锁、信号;库部分按旧路径重新导出 assembly |

### 1.1 模块怎么进入会话

`runtime::agent::Builder::module(..)` 按顺序装入模块。`build()` 先按场景收窄
(§3.6)、再检查 `requires`(缺失、排在后面、重复都拒绝),在会话的工具表、事件总线、
权限、hooks 都建好之后、场景的工具策略生效之前,依次调用 `on_session`——模块注册的
工具和内建工具受同一套场景约束。

`SessionContext` 把正在构建的会话交给模块。两样东西是**晚绑定**的,因为读它们的
东西比提供它们的模块先建好:

- **agent spawner**:`"agent"` 型 hook 与 `context: fork` 的 skill 拿到的是一个之后才
  填入实现的 spawner,由 agents 模块 `provide_agent_spawner`;没有人提供时派生被拒绝。
- **skill 目录**:命令表、agent、其它模块拿到的都是同一个晚绑定的 `SkillCatalog`,
  由 skills 模块提供;没有时为空。

MCP 连接由 mcp 模块交给会话独占(`provide_mcp`),和原先会话独占 `McpManager` 一样。
连接按项目缓存在会话池里;一对 `(项目, 场景)` 第一次连时会话创建最多等
`MCP_SESSION_CONNECT_WAIT`(10 秒),没连完的 server 在后台接着连,结果并进缓存后作废这个
项目的绑定,会话在下一轮之前重建到新连接上。每个 server 的起进程加握手、每个请求都有时限
(`mcp::config::McpTimeouts`);工具调用断在途中只报错不重发(`mcp/src/connect.rs` 模块头)。
工具与 prompt 的名字由 `mcp::naming` 从 server 名与工具名造出,保证符合模型 API 的
`^[a-zA-Z0-9_-]{1,64}$`,权限规则按这个名字写。

装配顺序是 **mcp → agents → skills**:agents 要看到它可以授予子 agent 的 MCP 工具,
fork 型 skill 要 agents 提供的 spawner。委派出去的子会话用父会话(收窄后)的模块
清单装配,所以嵌套委派照旧;MCP 连接只给第一个装配它的会话,子会话经 `Agent` 工具
按 agent 类型授予父会话已连接的服务器。

### 1.2 档位

`daemon` 的 feature 按档位组织,脚本载体(`scripts`)、包方案(`plugin-packages`)与
`Bash` 的进程沙箱(`sandbox`)另选:

| 档 | 装配的模块 |
|---|---|
| `minimal`(`--no-default-features`) | 只有内核:模型、turn loop、场景、会话、遥测、hooks、文件与 shell 工具 |
| `coding` | + mcp、agents、skills、web 工具、structured-output、ptc(`RunCode`) |
| `full` | + WebSocket 监听 |

默认 `full + scripts + plugin-packages + sandbox`。`daemon.ping` / `daemon.status` 的 `modules`
列出编入的模块;属于未编入模块的方法返回 `METHOD_NOT_FOUND`,`data.module` 指明模块;
配置了未编入模块的设置段(如 minimal 下的 `mcp_servers`)启动时警告并忽略;没编入
`scripts` 时出现 `scripts` 段记 error,会话同样照常起来、不带脚本。两者都不阻止启动,
级别不同是因为缺了 MCP 服务器或团队工具,模型和宿主看得见(工具不在);脚本没生效却
只是输出悄悄变了样,作者会先去 JavaScript 里找 bug。

### 1.3 失败策略

`release` profile(fat LTO、`opt-level = "s"`、`panic = "abort"`)在 panic 时中止进程,
由 supervisor 拉起;只有明确要 `release-fast`(`opt-level = 3`、`panic = "unwind"`)时
才展开,任务里的 panic 只结束那个任务;工具调用里的 panic 变成这次调用的错误结果,这一轮照常往下走。
也就是说,**`release` 构建里这层兜底不生效**:一个工具 panic 会带走整个进程和它上面所有会话,
不是只带走这一次调用。这是有意的取舍(`release` 要小、要快,崩了由 supervisor 拉起、会话从日志恢复),
不想要的部署用 `release-fast`,或者保证工具代码不 panic。`daemon.doctor` 的 `build.panic` 一项
报告这个构建是哪一种。中止前 panic hook 停掉
命令所在的进程组并写崩溃记录(`~/.agix/daemon/crashes/`);来不及跑 hook 的(SIGKILL、
OOM)由同名实例下次启动、拿到锁之后按落盘的进程组记录(`~/.agix/daemon/groups/`)
清理:先 `SIGTERM`,500 ms 后 `SIGKILL`。会话历史每个条目整行写进日志,每一步结束和
一轮结束时 fsync 一次,重启后照常恢复;崩溃时正在进行的那一轮不续跑。写到一半被打断的
最后一行在读时略过(记一条 warn),下一次追加前截掉——不会让整份会话读不回来。

一份会话日志同一时刻只有一个写者:写日志的进程对 `sessions/<id>/writer.lock` 持
`flock`,会话在某个 daemon 里活着就一直持有,进程无论怎么退出内核都会释放。另一个
共用同一状态目录的 daemon 要 resume 或给它跑 turn 时直接报错,不换 id 新建。

daemon 不自己重启自己,交给 launchd(`KeepAlive` 设 `SuccessfulExit = false`)或
systemd(`Restart=on-failure`)。systemd 下要设 `KillMode=process`:`daemon.shutdown` 与
`SIGTERM` 走正常关闭,由 daemon 按会话设置决定哪些后台命令停、哪些留,整组杀掉会抹掉
这个区别。

---

## 2. 一轮是什么

`crates/runtime/src/turn/` 是引擎的主循环。**步骤顺序是骨架,不开放**;
每一步上"选哪条路"的判断是决策,**开放**。

```
收用户消息
  └─ UserPromptSubmit hook · 斜杠命令 · 记忆召回 · 先放进空闲期间到的通知(§3.8)
循环:
  ├─ 0. 落盘        上一步写进内存的消息先落盘
  ├─ 1. 停止判断    TurnPolicy::before_model_call   ← 这一轮还该不该继续
  ├─ 2. 压缩        Compactor::should_compact       ← 该不该压、压什么
  ├─ 3. 硬顶        ContextBudget::hard_cap
  ├─ 4. 装配提示词  PromptAssembler / 注册的块 / 装配钩子
  ├─ 5. 调模型      Model + ModelInterceptor + BackoffPolicy
  │     ├─ 边流边跑工具 Permission → ToolMiddleware → Tool → ToolResultTransformer
  │     └─ 出错 → RecoveryPolicy:换模型 / 压缩重试 / 失败
  ├─ 6. 复查        TurnPolicy::after_model_call    ← 这一轮还走不走下一步
  ├─ 7. 记账        BudgetPolicy::on_usage          ← 继续 / 警告 / 耗尽
  ├─ 8. 有工具调用:送达这一轮运行期间收到的用户消息与通知,再来一轮
  └─ 9. 没有:RecoveryPolicy::on_early_stop       ← 抬 max_tokens 重发 / pause_turn 发回去续上
         Stop hook(结束这一轮 / 拦下:把理由交给模型再来一轮);
         BudgetPolicy::on_output_target 决定是否续写
         (收尾时还有没送达的用户消息,再调一次模型回答它们)
出口:finish_turn —— 先落盘;团队成员再发 TaskCompleted / TeammateIdle;最后发 TurnComplete
```

**回复停在 `max_tokens` 时,三个决策按步骤顺序轮流说话,先说的算。** 第 6 步
`TurnPolicy::after_model_call` 说停就停;回复要了工具就照常下一步,另外两个都不问;
否则第 9 步先问 `RecoveryPolicy::on_early_stop`,它决定抬高上限重发就重发,
Stop hook 和 `BudgetPolicy::on_output_target` 这一步都不再问;它放弃了才轮到 Stop hook,
最后才是 `on_output_target`。

**工具在第 5 步里跑,不等回复写完。** 并发安全的 `tool_use` 一到齐就开始,模型接着往下写;
不并发安全的是屏障,等前面那批跑完再单独跑;它跑的时候照样读流,读到的等它跑完再处理。所以第 6、7 步拦不住这一条回复要的工具——
它们决定的是还走不走下一步。要在工具跑之前拦,用权限、`PreToolUse` hook 或
`ToolMiddleware`。

**出错时什么都没留下,才交给 RecoveryPolicy。** 请求被拒(状态码、连不上),或者流在回复
要工具之前断了(流里的 `error` 事件、没等到消息结尾就结束),这次调用的半截回复整个丢掉,
transcript 不变,恢复策略重发的就是失败的那个请求;每次调用至多恢复一次。工具已经跑过的,
回复连同结果照写,这一轮以错误结束——再发一遍会让工具再跑一遍。

**压缩只有一条路径。** 阈值逼出来的(第 2 步)、`/compact` 要的、请求因体积被拒后
恢复策略要的(第 5 步),都走 `run_compaction`:`PreCompact` hook、需要时先让模型写摘要、
否则走 compactor 的级联、把丢掉的读文件结果与 skill / 计划模式 / 后台任务的提醒补回前面、
写 transcript 标记、发 `compact` 帧、`PostCompact`,并让下一轮重新注入被压掉的开场上下文
(AGENTS.md 等)和召回过的记忆。阈值量的是整个请求,所以自动压缩把对话压到阈值减去系统
提示词与工具定义、再减去必然补回的提醒之后剩下的大小;压完比压之前还大的不生效。

**一轮只有一个出口。** 循环里有十几处提前结束(hook 截停、预算耗尽、取消、出错),
落盘和宣布结束都只在 `finish_turn` 做一次,所以哪条路都跳不过。顺序本身是契约:宿主
把 `TurnComplete` 读作"会话空闲、它的 transcript 已经在盘上"——daemon 收到它就把会话
交给下一个调用方,并可能立刻从 transcript 重建它。每一步开始前也落一次盘,进程在
一轮中途死掉最多丢正在进行的那一步;落盘点都在 `tool_use` 与它的 `tool_result` 已经
配对的地方。

**运行中的用户消息在步骤边界送达。** 一轮在跑时收到的用户消息先排队,在一批工具结果
写进对话之后、下一次调模型之前加进去,所以永远不会落在一对 `tool_use` /
`tool_result` 中间。这一轮在送达之前就结束了(取消、截停、出错)的,消息不进
transcript,以 `UserMessagesReturned` 交还宿主。通知(`SessionState::post_notice`,
如后台子 Agent 结束)走同一组边界;一轮已经结束的,留给下一轮开头。


**骨架与决策的分界是这套设计的核心。** 步骤顺序写死,是因为顺序本身就是"一轮"的定义;
而每一步的判断——跑几步停、错了怎么办、什么时候压缩、花多少 token——是意见,
不同部署持不同的意见。所以它们在契约后面。

---

## 3. 会话与场景

### 3.1 一个进程可以跑多个场景

场景(`Scene`)是无状态的:`HarnixScene` / `MiniScene` 是零字段 unit struct,
`CodeScene` 只带一个创建时定下的 `Interaction`,`Arc<dyn Scene>` 只是一组函数。所以同一
进程并存多个场景没有障碍。

内建三个:

| id | 工具 | 用途 |
|---|---|---|
| `code` | 注册的全部工具;`RESIDENT_TOOLS` 之外只报名字,schema 经 `ToolSearch` 取 | 编程,需要项目;daemon 的默认场景。注册的是交互版(`Interaction::Interactive`),嵌入方可换成 `CodeScene::new(Interaction::Unattended)` |
| `harnix` | 常用集:文件读写与搜索、`Bash`、`Skill`、`TodoWrite`、`Agent` 与任务控制、`Team*`、目标、网页检索与抓取、`ToolSearch`、`mcp__*`;宿主经 `extra_tools` 加自己的 | Harnix 宿主会话的默认场景,无人值守 |
| `mini` | `Agent` `Bash` `Edit` `Glob` `Grep` `Read` `Skill` `ToolSearch` `Write`,全部常驻 | 最小可用的编码 agent,无人值守 |

- `--scene` 定 daemon 的默认场景,`--scenes harnix,mini` 追加激活
- `scene.activate` / `scene.deactivate` 运行期增删
- `session.create {"scene": "harnix"}` 指定该会话用哪个;未激活的场景返回 `SCENE_NOT_FOUND`
- **每个会话按自己的场景解析设置层**,包括 `~/.agix/scenes/<scene>/` 下的 settings、
  skills、agents、plugins。设置缓存的键是 `(project, scene)`——只按项目缓存会让先建好
  条目的那个场景的设置泄漏给所有场景。

唯一保留的进程级绑定是 `DaemonPaths::config_root()`,启动时按首个场景算定;
其它场景的目录由 `SessionPool::scene_root` 从 `global_root()/scenes/<id>` 推导。

**`SCENE_MISMATCH`** 守的是"这个会话属于哪个场景",不是"这个 daemon 只能跑一个场景"。
会话的 `Meta.scene` 记录它创建时的场景;操作属于别的场景的会话时拒绝,不跨边界执行。
旧 transcript 没有 `Meta.scene`(v2 之前没这个字段),此时判定为`SceneCheck::Inferred`——按 pool 的默认场景接上,而不是让整份文件解析失败。

### 3.2 `project_root` 的三态

RPC 里 `project_root` 有三种含义,靠 JSON 层面的"键缺失 / 键为 null / 键为字符串"区分:

| 线格式 | 语义 |
|---|---|
| 键省略 | 该 pool 的默认项目 |
| `"project_root": null` | 真正的无项目会话(全局层) |
| `"project_root": "<path>"` | 指定项目 |

serde 的 `Option<Option<T>>` 免费区分前两者,但 `null` 与字符串的区分必须在 wire 层
显式检查,**在到达 `SessionPool` 之前**。无项目会话的 `local_data_dir` 取
`global_data_dir` 本身,而不是 `<project>/.agix`。

`config.*` / `project.*` / `mcp.addServer` / `mcp.updateServer` / `import.*` 这几个**作用在项目层**的方法用的是另一套
两态:省略 = 这个 daemon 自己的项目,给路径 = 它服务的另一个项目,**`null` 拒绝** ——
无项目会话没有项目层可写。

### 3.2.1 一条不变量

> **会话读到的任何按项目索引的东西,都来自它自己的 `project_root`。进程级的只剩
> 不按项目索引的东西。**

按项目索引、因而按 `(project, scene)` 绑定的(`SessionPool::bindings` 里的
`ProjectBinding`):合并设置、skills、agent types、commands、provider 路由与主模型、
MCP 连接、脚本绑定的 root、记忆目录、transcript 目录。

进程级的,因为它们**不按项目索引**:

| 进程级 | 为什么 |
|---|---|
| `DaemonPaths::config_root()` | 启动时按首个场景算定,见 §3.1 |
| 工具注册表 | 内置工具是编译期事实 |
| 场景注册表 | 内建、宿主登记(`Assembly.scenes` / `SessionPool::register_scene`)、插件贡献三类,宿主登记的可以是声明式场景;激活集合是运行期状态,但不按项目 |
| 插件集合 | 插件只有全局层与场景层,**没有项目层** |
| `WatchHub` | 一个 `notify` 线程,按 tier 分键去重,watched 目录是 `1 + M + P` |

**加一样按项目索引的东西,就把它放进 `ProjectBinding`**,不要在池上再挂一个
`HashMap<ProjectSceneKey, _>`。

有两张表是有意的例外,同样按 `(project, scene)` 取键,所以不会读到别的项目,只是寿命
比绑定长、放不进去:

| 池上的表 | 为什么不在绑定里 |
|---|---|
| `mcp_by_project` | 默认项目的连接在启动时就开始,那时还没有任何绑定;`config.reload` 在原处重连、只换掉这一项,不用等绑定重建 |
| `last_good_routers` | 新的 provider 配置解析不出或建不出 client 时退回它;绑定一失效就跟着没了,就退不回去 |

**绑定按层失效。** 每个绑定带一个失效标记,用它建出的会话持有同一个标记。设置文件变化
只作废从那一层合出来的绑定:项目层作废同一项目根的,scene 层作废那个 scene 的,全局层
作废全部;显式的 `config.reload` 不知道动的是哪一层,作废全部。缓存的 MCP 连接跟着同
一层作废。skills、agents 目录的变化不作废任何东西,新内容就地读进绑定共享的目录对象,
会话立刻看到。池常驻监视它自己那一对 `(项目, 场景)` 的各层。设置文件只有内容真的变了才算
变化:macOS 的 FSEvents 会把监视开始前不久的写入补报过来,照原样再写一遍也会报,这些都不作废
绑定。

### 3.2.2 会话规格:重建时照什么重来

一个会话的 `Agent` 会被重建:它的绑定失效、janitor 回收后再被用到、daemon 重启后恢复。
重建照的是**会话规格**,不是触发重建的那次调用碰巧带了什么:创建时的 `options`、
`session.setModel` 选过的模型与厂商、`session.setPrompt` 设的会话提示词。规格以 `ns = "agix.session"`、`event = "spec"`
的扩展条目写进会话日志,每次变化追加一条,最后一条为准,所以跨 daemon 重启也成立。
recorder 不落盘:同一进程里的重建共用同一个 recorder,回放接着原来的进度。

重建之前先停掉旧 `Agent` 并等它收尾(有时限),然后才从盘上读 transcript——否则新
`Agent` 读到的是旧的那个还没写完的历史。关停时同样等所有 `Agent` 收尾。

会话的后台任务(子 Agent、团队成员、后台程序、Monitor)跑在它的运行体之下,停掉旧 `Agent`
就停掉了它们,所以**还有后台任务在跑的会话不重建**:绑定失效的那种推迟,这一轮照旧用现在的
引擎,标记留着下次再看;`session.setPrompt`、换厂商的 `session.setModel` 返回 `SESSION_BUSY`。
真重建时,旧 `SessionState` 记着的任务(结束了的那些,`session.tasks` / `TaskOutput` 还查得到)
和模型还没读到的通知(`<task-notification>` 等)带到新的上面。

会话可以指定自己的厂商(`options.provider`):对话走那个厂商的 client,子 Agent、压缩
等其余路由任务仍按绑定的路由走。首轮结束后的自动命名也是这个会话的调用:按会话自己绑定的
`title` 路由,没有就用会话的对话模型,而且用的是套过录制器的那份 client,所以录得进录像、
回放时也从录像里取。宿主可以经 `Assembly.customize_session` 在每个会话的
`Builder` 建成之前改它,新建、恢复、重建都经过这一处。

### 3.2.3 有人值守与无人值守

`Scene::interactive()` 回答"会话进行中有没有人回答问题、批准计划、应答权限提示"。缺省
`false`;只有交互版 `code` 是 `true`(声明式场景在 `scene.json` 里写 `"interactive": true`)。
这一个答案同时决定四件事:

| | 无人值守 | 有人值守 |
|---|---|---|
| 工具 | 没有 `AskUserQuestion`、`EnterPlanMode` / `ExitPlanMode` | 有;`AskUserQuestion` 另要宿主能回答提问(`Builder::question_prompts`、自带的 `Builder::elicitation`,daemon 上是 `options.prompt_types` 列了 `question`) |
| 提示词 | 不写"问用户""先确认";写"任务没明说就不做,在报告里说明" | 写 |
| 内置技能 | 只有能独立做完的 | 再加需要用户在场的 |
| 权限提示 | 不送给任何人,直接拒绝,拒绝消息里写清怎么授权(§6.3) | 送给宿主等回答 |

内置技能由 `Scene::bundled_skills()` 点名,缺省只有 `skillify`;依赖的工具(`skills::bundled::required_tools`,
如 `loop` 要 `CronCreate`)在场景里不被允许的,也不列。磁盘上和宿主 provider 给的技能不受
影响,同名的照样覆盖内置。`code` 无人值守带 `skillify` `simplify` `verify` `debug`
`security-review`,有人值守再加 `batch` `stuck` `remember` `init` `import` `loop` `rename`
`updateConfig`。

运行时另有一个与提示词无关的兜底:同一 agent 在一个 turn 里用同样的输入调同一个工具到第 3、
5、8 次,在结果后附一条提醒,让模型看上次的结果、换方法或停下汇报(`runtime::repeat_guard`)。
只提醒,不拦调用;不触发就不花 token。

文件工具给模型看的路径由 `Scene::path_style()` 定缺省:`harnix` 用相对工作目录的路径
(参数说明、Glob / Grep / Write / Edit 的输出都按相对写),其余场景用绝对路径;设置里写了
`path_style` 就以设置为准。相对路径省掉的是每次调用都重复出现的那段项目前缀。

### 3.3 Sidechain 会话

子代理(`Agent` 工具、Skill `context: fork`、team 成员)的 transcript 独立成一个会话,
通过 `Meta` 里的 `parent_session_id` + `session_kind: Sidechain` 与父会话关联。
可经 `HistoryStore::child_sessions` 找到,默认不出现在 `session.list`。

跑完写 `LogEntry::SessionEnd { state }`。**被取消的轮次刻意不写这个标记**——取消是被打断,
不是到达终点,所以会话必须保持可 resume。对已写终止标记的 sidechain 发起 resume 会被拒。

关闭父会话会**级联删除**其全部 sidechain:父会话一旦关闭,就没有任何路径能再触达它们。

### 3.4 委派深度

委派链由 `execution.max_agent_depth`(默认 3,即 `EngineConfig::max_agent_depth`)**计数**约束,
不靠从工具集里摘掉 `Agent`。
`Inner::spawn_guard` 在每一条 spawn 路径上检查;到顶时返回普通工具错误,模型看得见,
可以改为自己做。同时在跑的个数另有上限,见 §3.8。

**过滤工具名不构成约束**:`Builder::build()` 会为子会话新建注册表并把 `Agent` 注册进去,
与父级传入的工具集无关;`RuntimeAgentSpawner`(Skill fork、team 成员)直接透传
`sub_tools()`,根本不经过 `AgentTool::resolve_tools`。

### 3.4.1 会话目标

目标(`base::context::goal`)让一个会话一轮轮自己推进:一轮正常结束(`end_turn`、
`stop_sequence`、`max_tokens`)时,目标若仍是 `active` 且已武装,就开下一轮,注入
`<goal_round>` 提醒目标与轮次,直到模型标完成或受阻、或轮数用完(转 `blocked`)。

- **谁开下一轮**:daemon 下是会话池,和定时任务同一条路径(`run_turn`,`initiator:"goal"`),
  会话用 `Builder::goal_rounds_from_owner()` 建;直接嵌入时 `Agent::run` 自己开。
- **谁能做什么**由这一轮是谁发起的决定(`TurnOrigin`,从 `turn_id` 读出,模型改不了):人发起的
  一轮什么都能做;自动轮只能标完成或受阻,受阻要从自武装起的第 3 个自动轮起;子 Agent 碰不到目标。
- **一次只等一轮**:`next_round` 交出下一轮时把它记成待开始,不计数;这一轮的 turn 开始时
  `claim_round` 认领它才计数。等着的期间别的 turn 结束不会再交出一轮,目标被暂停、清除、
  修改或解除武装则作废这一轮,认领失败,它就不跑。
- **武装不落盘**:每次变化以扩展条目(`ns:"agix.goal"`)记进会话日志,从日志恢复的目标一律不
  武装,要人说继续;daemon 在内存里原地重建会话时把武装和待开始的那一轮带过去。一轮以取消、
  hook 中止、预算耗尽或出错结束,也解除武装。

### 3.5 场景的工具面

场景对工具有三种方向不同的控制:

| 方法 | 作用 |
|---|---|
| `tools()` | 白名单,与注册表取交集——**只能减**;以 `*` 结尾的项按前缀匹配(`mcp__*`) |
| `extra_tools()` | 贡献注册表里没有的工具——**只能加** |
| `deferred_tools()` | 哪些工具只报名字不带 schema |

白名单(连同白名单为空时的 `disallowed_tools()`)经 `Scene::permits_tool` 作用在会话
注册表本身:`Builder::build()` 把不许的工具从注册表里移走,之后运行期才出现的 MCP 适配器
也按它过滤。宣告、`ToolSearch`、分派读的是同一张表,不存在"不宣告但能调用"的工具。

这份判定在 `Builder::build()` 里算一次,成为会话的 `ToolPolicy`(`runtime/src/tool_policy.rs`):
场景允许的,加上插件经 `Registrar::admit_tools` 放行的自己登记的工具(场景 `disallowed_tools()`
里的除外)。注册表过滤、每次请求的工具宣告、MCP 适配器同步都读这一份;`runtime` 里别处不直接问
`Scene::permits_tool`,`daemon/tests/wiring_coverage.rs` 守着这一条。

三条硬约束:

1. **`extra_tools()` 重名直接失败,不覆盖。** 注册表按名字返回首个同名项,静默覆盖会让
   调用点拿到的和模型看到的不是同一个工具。检查跑在引擎态工具注册**之后**,
   否则场景能占用 `Agent` / `Skill` 这些名字并赢得查找。
2. **`ToolSearch` 永不 defer。** 它是取回 deferred 工具 schema 的唯一途径。
3. **`Tool::is_deferred()` 不是策略输入**,是 `DeferredTool` 包装器设置的输出。
   工具自己声明它没有裁剪 schema 的效果——只会让它对 `ToolSearch` 撒谎。

### 3.6 场景收窄模块

`Scene::modules()` / `Scene::disabled_modules()` 对模块做的事和 `tools()` 对工具做的一样:
**只能减**。`Builder::build()` 在任何模块运行之前按 `Scene::permits_module` 过滤装配
列表,`SessionContext::modules` 和子 agent 拿到的都是过滤后的那份。留下的模块 `requires`
一个被排除的模块时构建失败——依赖关系由模块声明,场景不能悄悄拆掉它。

### 3.7 声明式场景

`scene::DeclarativeScene` 是内核自己的一个 `Scene` 实现,数据来自 `scene.json` +
`prompt.md`。它和 Rust 写的场景一样是加载后不再变的值:两个文件只读一次。注册走
`SceneRegistry::register_declarative*`,不替换已登记的 id;插件包里的那份 id 固定为
`plugin:<name>`。运行中的 daemon 经 `scene.reload` 重读 `~/.agix/scenes/`,经
`scene.register` / `scene.unregister` 登记、撤销一个不落盘的场景。格式与占位符见
`extension_points.md`。

### 3.8 并发:每个 agent 是一个任务

几个词在本文与代码里的含义:

| 词 | 含义 |
|---|---|
| **Agent** | 执行实体:自己的上下文与步进循环(一轮之内「调模型 → 调工具」直到结束);会话的主 Agent 与常驻团队成员另有输入循环(`Agent::run`),一次性子 Agent 跑完一轮即止。都是线程池上的一个 tokio 任务 |
| **Task**(运行任务) | 在跑的工作的句柄:状态、输出、取消、收件箱。类型有 agent / shell / monitor / teammate / cron / code(`RunCode` 的程序);`TaskOutput` / `TaskWait` / `TaskStop` / `TaskMessage`、`session.tasks`、`task_*` 事件说的都是它 |
| **Todo** | 记账清单(`TodoWrite`),不执行任何东西 |
| **Event / Message** | Agent 之间、Agent 与宿主之间的全部交互:输入通道与收件箱进,`AgentEvent` 与 `session.event` 出 |

会话的主 Agent、前台与后台子 Agent、`TeamCreate` 成员、常驻团队成员,各自是多线程
tokio 运行时上的**一个任务**,带自己的循环与上下文。一个 Agent 一轮里并发的工具调用
是这个任务里交错执行的 future(`runtime::streaming`),不另起任务——它们共享这个
Agent 的上下文,本来就该在一处。

前台子 Agent 也是独立任务:父 Agent 的工具调用 spawn 它并等结果
(`run_one_turn_detached`),所以同一轮的几个前台 `Agent` 调用、`TeamCreate` 同一阶段
的成员是真并行,一个子 Agent 的同步重活不会卡住兄弟和父 Agent。父 Agent 的调用被丢弃
时子任务被中止,不留无人观察的子 Agent;子任务 panic 变成父 Agent 的工具错误;取消照旧
走 `CancellationToken`。

**Agent 之间只经通道说话,不共享可变状态:**

| 方向 | 通道 | 入口 |
|---|---|---|
| 父 → 子 | 收件箱:`RunningTask::inbox`,即子 Agent 的 steering 队列,步骤边界读到 | `TaskMessage`、`session.messageTask` |
| 子 → 父 | 结束时 `post_notice` 一条 `<task-notification>`,父 Agent 下一个步骤边界或下一轮开头读到 | 自动;宿主另有 `task_started` / `task_finished` 事件 |
| 父等子 | `RunningTask::wait_ended` | `TaskWait`(any / all + 超时)、`TaskOutput` |
| 成员之间 | 团队 mailbox 与团队共享记忆(§8) | `SendMessage` / `ReadMail`、`TeamMemorySave` / `TeamMemoryRead` |

通知在任务结束**之前**投递,父 Agent 已经用 `TaskWait` / `TaskOutput` 读到结果的,
通知随之撤回,同一件事不说两遍;被 `TaskStop` 停掉的不发通知。

**写新的 Agent 级代码守四条:**

1. 长期运行的 Agent 级工作用 `tokio::spawn` 起自己的任务,不要在另一个 Agent 的任务里
   poll 它。持有 `JoinHandle` 的一方负责它的生命周期——丢弃即中止,或交给取消令牌。
2. 一个 Agent 的状态只由它自己的任务改。别人经收件箱、通知、事件跟它说话;锁只用于
   短暂的快照读写,不跨 `.await` 持有。
3. 超过约 1 ms 的同步工作(扫目录、解析大文件、全量正则)放 `spawn_blocking`。独占线程
   只留给需要线程亲和的东西(`notify` 的 watcher 那类)。
4. 资源上限都在一处可配:委派深度 `execution.max_agent_depth`(§3.4);同时在跑的子 Agent
   `execution.max_concurrent_subagents`(默认 6,整棵会话树共用一份);`TeamCreate`
   每阶段 `team_stage_concurrency`;常驻成员 `team_max_persistent_members`(满了先回收最久
   空闲的,全在忙就拒绝新成员)。常驻成员的取消令牌是父会话结束令牌的子令牌:会话结束时,
   空闲的和正在跑一轮的成员都停下。

**并发上限超出时拒绝,不排队。** 持有名额的子 Agent 再为自己的子 Agent 等名额,整棵树
的名额可能全被这样的等待者占着。拒绝是一个模型看得见的工具错误,它可以等(`TaskWait`)
或自己做。`TeamCreate` 成员与常驻成员各有自己的上限,不计入。例外是 `RunCode` 程序里的
`agent()`:程序只在根会话里跑,根会话不占名额,所以它扇出超过上限时排队等空位(`docs/ptc.md` §5)。

**为什么不是一个 Agent 一个 OS 线程。** 多线程运行时已经把任务分到所有核上,一个 Agent
绝大部分时间在等模型 API;专用线程意味着每个 Agent 一个运行时或一套同步桥,换来的只是
同步重活不互相阻塞——这一点 `spawn_blocking` 已经给了。

---

## 4. 历史:状态即日志

会话状态存在一份 append-only 的 JSONL 里,`history::transcript` 把它**投影**成模型看到的消息。

- **投影可换**(`TranscriptProjection`),挂在 store 上——日志和读它的规则必须一起走,
  否则 resume / fork / 搜索会看见三个不同的过去
- `LogEntry::Extension { ns, event, payload }` 让内核之外的东西(插件、脚本、宿主)
  把状态放进同一条时间线。**未知 `ns` 必须可跳过**:卸载插件后旧日志仍能加载。
  daemon 自己也用这条通道:会话规格记在 `agix.session` 下(§3.2.2)。
  `ExtensionsAreVisible::all()` 不分 `ns` 地把扩展条目渲染给模型,这些也在其内——
  要让模型读到某个扩展的条目,按 `ns` 列名单
- **模型可见即可从日志重建**。一个靠询问活着的插件来渲染条目的投影,会产出一个
  重新打开就不一样的会话。`model_visible_content_is_reconstructible` 把这条变成可检查的
- 大内容可以外置到 `BlobStore`,日志里只留引用;**引用解析不了时条目保持惰性,不是报错**。
  store 要由嵌入方自己装(`JsonlHistoryStore::with_blob_store` / `with_paste_store`);
  daemon 默认不装,大内容照样写在日志行里

**一个会话连同它的侧链可以在同一时刻截下来**(`HistoryStore::snapshot_lineage`)。
`JsonlHistoryStore` 不持任何锁:先量一遍父会话项目目录里每份日志的长度,再各自读到量下的
位置,读的时候截到最后一个整行——append 一次写完整的一行,量到的位置若落在写了一半的行
里,那一行不算。一个 append 慢的会话不会挡住另一个会话的快照。走的是 `Meta` 里的父子关系,只沿 `session_kind: sidechain`
往下走,分叉只记 id。默认实现一个接一个地 `load`,不是同一时刻,自己实现 store 的要覆盖它。
`session.export` 就站在这上面:快照加上图片本体、两种 RPC 视图和一份带版本的清单写成
一个归档,这份布局也是将来 `session.import` 要读的格式(`daemon_rpc_protocol.md` 的
`session.export`)。

**留存的时间戳与标识符走 `Environment` 契约**,所以在 `FixedEnvironment` 下同一段对话
能重放出逐位相同的日志。daemon 不用它:会话的时钟是 `ShiftedEnvironment`(从
`clock.start` 或录像开录的时刻起按墙上时钟走),标识符每次新生成,所以 daemon 的回放
得到的是内容一致、时间戳与 id 不同的日志(`daemon_rpc_protocol.md` 的 `options.recorder`)。
纯测量用的 `Instant` 不走契约——那正是它该做的事。

---

## 5. 执行层:工具怎么碰到机器

四个契约一组设计,因为它们互相咬合:沙箱约束进程,进程要文件与网络,
网络策略又要能作用于沙箱里的进程。

| 契约 | 管什么 |
|---|---|
| `Process` | 程序在哪里跑。**输出是流式的**——长命令的输出要边跑边到用户眼前 |
| `FileSystem` | 工具的文件在哪里。整值读写,因为所有调用点都是整值的 |
| `Network` | 每一次出网请求,以及它能不能出去 |
| `Sandbox` | 进程**怎么**被约束——永远不决定**要不要**约束 |

两个 provider:`platform::local_providers()`(本机,生产里用的)与 `ExecProviders::in_process()`
(内存文件树、预置的命令应答、离线网络、诚实地报告自己什么都不约束的沙箱)。

**一个会话一套。** `Builder::build()` 造一次,工具调用、引擎自己的 git 都用它,并经
`SessionState::bind_exec` 交给权限判断:权限层判路径(`canonicalize`)、判沙箱是否完整兜得住
命令时,问的是这次调用真正要跑在上面的那套,子 Agent 的调用按子 Agent 会话的那套判。

五条容易搞错的:

1. **路径安全在契约之上,符号链接解析在契约之内。** 一个路径能不能写是策略,
   provider 不该能取消它;但远端的符号链接图是远端的。顺序是:
   `provider.canonicalize` → `check_write(已解析的路径)` → `provider.write`。
2. **出网策略问的是"模型能够到哪里"**,不是"这个进程能连出去哪里"。请求带着
   `Origin`:模型选的目标受 `allowed_domains` 约束,运维配的端点(模型 API、MCP、遥测)不受。
   把它作用到模型 API 上会让 agent 连自己的大脑都够不着。模型选的目标还不许落到本机或
   内网地址(`is_internal_address`:回环、私网、链路本地、CGNAT、`0.0.0.0`、带着这些 IPv4
   的 IPv6……),除非 `allowed_domains` 点名了那个主机;HTTP hook 的 url 带
   `HttpRequest::public_only`,同样不许。重定向的每一跳都查;名字在发出前解析查一次,
   连接时用的解析器再查一次,中间换了答案的名字也进不去。
3. **受约束的策略绝不静默降级为无约束执行。** 后端报告 `Full` / `Partial` / `None`
   并列出兑现不了的部分;`sandbox.require_enforcement` 决定不足 `Full` 时是否拒绝。
4. **一个命令是它起的整个进程组。** 本机 provider 让每个子进程领一个会话(也就领了一个
   进程组,且没有控制终端——命令没法经 `/dev/tty` 用 `TIOCSTI` 往 daemon 的终端里塞输入,
   等沙箱外的 shell 去执行),停的时候整组停;前台进程退出后输出最多再等 `OUTPUT_GRACE_AFTER_EXIT`,还被后台进程占着就
   连它们一起停。Bash、Monitor、command hook、MCP stdio 都是这条规矩。
5. **子进程的环境只在一处决定。** `base::interface::exec::env::SubprocessEnv`:缺省
   继承宿主环境但去掉引擎自己的凭据(`ANTHROPIC_*`、`AGIX_DAEMON_TOKEN`,以及任何一层
   设置里 provider 的 `api_key_env` 指向的变量——加载设置时、读取 key 时都记下,进程内
   一直有效),`allowlist` 模式只留 `PATH` / `HOME` / 语言环境等几项加上 `allow` 列出的。Bash、
   Monitor、command hook、MCP stdio 服务与引擎自己跑的 git 都按它起——git 用缺省策略,其余
   按 `sandbox.subprocess_env` 取策略——MCP
   服务取的是声明它的那份设置(daemon 按项目、scene 建连),再叠各服务自己的 `env`。这和沙箱无关——沙箱关着,模型一句 `printenv` 读到什么也由它决定。

沙箱是**轻量写限制,不是安全隔离**。已知逃逸路径写在
`crates/platform/src/sandbox.rs` 的模块头里,四条;第 3 条对引擎自己的
git 已经收口(见 §6.3),剩下的是用户事后在项目里自己跑的 git;第 4 条(让别的进程替命令
在沙箱外做事)在无人值守会话里收口(见 §6.3)。它限住的写入范围见 §6.3。

---

## 6. 权限

### 6.1 规则怎么裁决

内建的 `PermissionGate` 按固定顺序走:凭据(任何模式都拒)→ 工具自己的判定(拒绝即拒;
放行只先记下,等后面都没否掉才兑现)→ 规则 → 免于 bypass 的路径 → 模式级硬拒绝 → 兑现 →
模式分派。完整顺序在 `crates/permissions/src/gate.rs` 的模块头。

**两份敏感路径名单。** gate、文件工具(`tools::security::read_allowed`、`Grep` / `Glob`
的遍历、只读 `Bash` 的路径判断)和 OS 沙箱读的是 `base::paths` 里同两份名单,不各抄一份:

- **凭据**(`base::paths::Credentials`):状态目录与各场景目录下的设置文件(`settings*.json`、
  `mcp.local*.json`,明文的 `api_key` 与 MCP headers 在里面)和 `mcp/oauth/`,状态目录下
  daemon 的 `daemon/auth/`,项目 `.agix` 下的 `settings.local.json` / `mcp.local.json`,以及
  任何位置的 `.ssh` / `.aws` / `.gnupg`。调用里有一个字符串(路径工具的路径及其解析后的各种
  写法,shell 命令的每个词与赋值,`--flag=value` 的值)落在这里,gate 就拒绝,不问,
  `bypassPermissions` 下也一样;`Grep` / `Glob` 搜它上层的目录时跳过它;沙箱里的命令读不到,
  `sandbox.allow_read` 放不开。没有一个回答能让"把 token 读进模型"成为好主意,所以不询问。
- **引擎配置**(`base::paths::ENGINE_CONFIG_{ROOTS,FILES,DIRS}` 与 `PROJECT_ENGINE_CONFIG_DIRS`):
  `.agix` / `.claude` 下除工作区之外的部分,和项目的 `.agents/skills/`(skill 的
  `allowed-tools` 会给自己授权)。写工具写这里要确认(工具自判的放行不算数),沙箱里只读
  (§6.2、§6.3)。读不受限。

项目根的 `AGENTS.md`(以及 `CLAUDE.md`)不在引擎配置里:它每会话注入提示词,但只是文字——
不授予权限、不跑命令、不接 hook 或 MCP,模型写进去的东西和写进 README、代码注释的一样,只能
影响模型自己读到什么;它在仓库里,改动看得见;让 agent 维护它是常见的请求(`init`、`import`
两个内建 skill 就写它)。保护它会给日常编辑加询问,挡不住同一类"持久的提示"(仓库里任何会被
读到的文件都是)。

规则这一步有四条约束:

1. **deny 永远优先。** 一次调用命中的规则里有一条 deny 就拒绝,否则有 ask 就问,否则
   才 allow(`bypassPermissions` 只看部署方 managed 层的规则,见本节末)。哪一层写的、写得多具体,都不参与裁决——项目仓库里提交的一条 allow 推不翻
   用户自己设置里的 deny,插件贡献的、客户端经 RPC 传进来的 allow 也一样。要"大范围
   禁止、小范围例外",写法是 `dontAsk` 模式加只列允许范围的 allow,而不是一条宽 deny
   加一条窄 allow。
2. **各层的规则并存。** 全局、scene、项目三层(以及各自的 `settings.local.json`)与部署方
   的 managed 层的 `permission_rules` 拼在一起生效,不像普通设置项那样后层盖前层。每条
   规则记着自己来自哪一层——全局与 scene 层是 `UserSettings`,项目层是 `ProjectSettings`,
   local 是 `LocalSettings`,managed 层是 `PolicySettings`——只用来报出命中的是哪一条
   (几条同样具体时先报 managed 层的)。
3. **路径先规整再匹配。** 读写路径的工具,路径按会话 cwd 解析成绝对路径并去掉 `.` /
   `..`,`~` 在规则与路径两边都展开;符号链接解析后的路径算作别名——按原样写法解析的
   也算,符号链接后面的 `..` 是从链接目标往上走的,`link/../x` 落在哪里,deny 就在哪里
   命中。别名上命中 deny / ask 照样生效,allow 只认工具真正打开的那个路径。glob 按 gitignore 约定:`*` 不跨
   `/`,`**` 跨。`Read(...)` 规则同样作用于 `Grep` / `Glob`,`Edit(...)` 规则同样作用
   于 `Write` / `NotebookEdit`。
4. **shell 命令逐段匹配。** `Bash(...)` 规则对命令行里的每一条简单命令分别匹配:认
   引号,切 `&&` `||` `;` `|` `&` 与换行,`$(...)`、`<(...)`、反引号里的命令单独算一段。
   任一段命中 deny 即拒;allow 要每一段都被放行,或者整行原样等于一条不带通配的 allow
   ("永久允许"记下的就是那一整行)。deny / ask 另按每段**实际启动的程序**再比一遍
   (`permissions::invocation`):去掉引号与转义,展开花括号,丢掉重定向,程序路径取
   名字(`/bin/rm` 是 `rm`),剥掉 `env` / `command` / `exec` / `nice` / `time` /
   `sudo` / `timeout` / `xargs` / `busybox` / `flock` / `ionice` / `pkexec` 等 wrapper 连同
   它们的选项,`sh -c`、`eval`、`find -exec`、`su -c`、`runuser -- `、`watch`、`script -c`、
   `parallel` 里的命令也算。程序名从
   文本里读不出来的段(`$CMD x`、`r?`、看不懂的 wrapper 选项、`case`/`function` 块、读 stdin 的
   裸 `sh`)在该工具有带内容的 deny / ask 规则时按 ask 处理(`dontAsk` 下拒绝),永远不会因此
   被放行。allow 只认原文——规整只让裁决更严。这些 `Bash(...)` 的 deny / ask 同样管住别的
   跑 shell 的工具(`Monitor`),allow 不跨工具。但 shell 上的 deny 规则不是沙箱
   (`python3 -c 'os.remove(...)'` 里没有 `rm`),边界是 OS 沙箱。
   只读命令(`Bash` 免询问的那一类)用同一套调用解析判:行里启动的每个程序都要按名字(不带
   路径——`./ls` 跑的是那个文件)在只读名单里,读不出程序的段不算只读;包在外面的 wrapper
   只能是不改变跑什么、在哪跑的那几个(`nice`、`timeout`、`nohup`、`stdbuf`、`command`、
   `builtin`、`exec`、不写报告文件的 `time`、只清除或传递变量的 `env`),`xargs`、`sudo`、
   `flock`、`env -C` / `-P` 都不算。只读名单只收"不管带什么参数都不会写文件、不会执行别的
   程序"的命令:`rg --pre`、`fd -x`、`git grep -O`、`man -P`、`sort --compress-program`、
   `cargo build/check`(跑 build.rs)这类会拉起程序的,`sort -o`、`uniq IN OUT`、`yq -i`、
   `git config k v`、`git branch -D`、`printf -v` 这类会写文件 / 改仓库 / 设变量的,写文件的
   重定向(含 `>&file`),以及设 `PATH` / `LD_PRELOAD` / `CDPATH`、任何 `*PAGER` /
   `*BROWSER` / `*EDITOR`、`GIT_*`、`SSH_*`(`SSH_ASKPASS`)、`HOMEBREW_*`、`XDG_*`、
   `*_CONFIG_PATH` 等变量的(`permissions::shell::is_dangerous_variable`),都不免询问;长选项按 GNU 与 git 的缩写规则认
   (`sort --outp=f` 就是 `--output`)。项目自己的文件能把程序换掉的也不在名单里:
   `cargo metadata` / `cargo tree` 要问 rustc(`.cargo/config.toml` 的
   `build.rustc-wrapper`),`cargo` / `rustc --version` 与 `rustup show` 跑
   `rust-toolchain.toml` 指定的工具链(可以是一个路径);`node` / `npm` / `pip` 连
   `--version`、`list` 也不收——装了 asdf / nvm / volta / pyenv 时它们是按项目里的
   `.tool-versions` / `.nvmrc` / `.python-version` 选二进制的 shim。`brew` 整个不收(按
   `HOMEBREW_*` 拉起浏览器、编辑器)。`git` 只以 `git <子命令>` 的形式、
   只在会话自己所在的仓库里只读:`cd` 进项目带来的裸仓库(`fixtures/x.git`)或嵌套的 `.git`
   再跑,那个仓库的配置(`core.fsmonitor`、`diff.external`)由项目作者写,git 会照着拉起
   程序。会向参数里的主机发请求的 `dig`、`ping`、`nslookup`、`traceroute`、`npm view`
   (`--registry`)、`git remote show <URL>` 也不在名单里(主机名本身就能经 DNS 带出数据);
   `git remote` 只认不联网的光秃秃、`-v`、`get-url`。
   只读命令的自动放行还只限路径参数都落在 cwd 与附加目录之内的;指向外面的,和 `Read`
   读项目外的文件一样交给模式分派。参数里有 shell 还要展开的东西(`$VAR`、`~`、替换)
   的,`cd` / `pushd` 到不是项目内字面路径的地方(或 `cd -` / `popd` / `pushd +N` 落到
   命令没写出的目录)之后再按相对路径读的,都不算在内;glob 按它此刻在磁盘上匹配到的
   路径查(`.*/x` 在 macOS 的 bash 3.2 下能匹配到 `../x`)。

场景的工具白名单不在这里:它在会话注册表上就把工具拿掉了(§3.5),被拿掉的工具根本
到不了权限这一步。

`bypassPermissions` 只看部署方 managed 层的 deny 与 ask(见下面"用户规则与部署方规则"),
用户自己各层的规则一条都不看:deny、ask、免于 bypass 的路径(`/etc/passwd`、
`rm -rf /` 一类)、配置目录的确认、工具自己的判定(含 §6.2 第 2 道的拒写名单)
全部跳过,输入合法就放行——凭据除外,它在任何模式下都拒绝(本节开头)。仍然生效的是输入校验——读后再写的过期检查、大小上限、§6.2
第 1 道的误读路径、`Bash` 的超时上限与长 `sleep` 拦截——和 OS 沙箱:`Bash` 照样被关在
可写根之内(`dangerously_disable_sandbox` 除外),无人值守会话也一样。`agixd` 的
`bypassPermissions` 会话同样走这道 gate,运行中进了计划模式也照样只读;只有宿主自己给了
`Assembly.permission`、且没有 managed 层时,才换成宿主的那一个。要 deny 规则仍然算数,用 `acceptEdits`,
或 `dontAsk` 加 allow 规则。

这样设计是因为 bypass 是给"边界在引擎之外"的部署用的——一次性容器、VM、CI 机器,
真正挡东西的是那一层与 OS 沙箱。一个"跳过大部分规则"的模式会让人以为规则还守着,
其实 deny 本来就不是沙箱(§6.1 第 4 条),它在 bypass 下留下的只是错觉;所以 bypass
要么全跳,要么不用,中间态交给 `dontAsk`。

**用户规则与部署方规则。** 上面的规则都是用户自己写的(或项目、插件、客户端替用户写的),
bypass 是用户对自己规则的放弃。部署方——企业 IT、无人值守的托管方——要的是另一回事:
一层用户改不了、用户选的模式也绕不过的规则。这就是 managed 层(`base::managed_settings`):

- **来源。** 系统路径上的一个文件:macOS `/Library/Application Support/Agix/managed-settings.json`,
  Linux 等 `/etc/agix/managed-settings.json`,Windows `%ProgramData%\Agix\managed-settings.json`。
  `agixd --managed-settings <路径>`(嵌入方:`DaemonConfig::managed_settings_path`)换成别的
  文件,这个文件**必须存在**。用户数据目录、项目、模型都改不了这条路径:它只来自系统路径
  和启动参数,不从任何设置文件读。嵌入方也可以在 `ConfigSource` 里给一层
  `Tier::Managed`;装配时有文件就以文件为准。读一次,改了要重启 daemon。
- **能写什么。** 只认 `permission_rules`(与其他层同一格式)和
  `disable_bypass_permissions_mode`(`true` / `false`),外加编辑器的 `$schema`。
- **压过下层。** managed 的 deny / ask 与其他层的规则一起按第 1 条裁决,下层任何 allow
  都推不翻;在 `bypassPermissions`(含 `auto` / `yolo`)下,gate 仍然只拿 managed 层的规则
  判一遍:命中 deny 拒绝,命中 ask(或程序名读不出来的 shell 段)询问,其余照 bypass 放行。
  询问在无人值守会话里就是拒绝。
- **managed 的 allow 不比别层的 allow 多什么。** 它和用户层的 allow 一样参与裁决:推不翻
  下层的 deny,也不把 bypass 之外的模式变宽。理由是第 1 条:deny 是硬边界,用户给自己写的
  deny 也应当守住;部署方要放宽的只是"少问",那用普通 allow 就够,而要"禁止"就写 deny。
  让 managed allow 盖过用户 deny,等于让部署方替用户撤销用户自己的边界,这不是 managed 层
  要解决的问题。
- **关掉 bypass。** `disable_bypass_permissions_mode: true` 时,`bypassPermissions` 不管从
  哪来(设置文件、`options.permission_mode`、agent 类型 frontmatter、团队)都按 `default`
  判;会话创建时报出的模式也是 `default`。
- **读不了就全拒。** 文件读不了、不是合法 JSON、有上面以外的键(`permision_rules` 这样的
  拼写错误不会被当成"没写")、值的形状不对、规则写不成规则——一律当作 managed 层在、但
  拒绝所有工具调用,任何模式都一样,`daemon.doctor` 的 `settings.managed` 报 `failing`。
  文件不存在(且没用 `--managed-settings` 点名)= 没有 managed 层,一切照旧。
- **看得见。** `daemon.doctor` 的 `settings.managed` 检查报出查找的路径、是否存在、来源与
  内容;`config.get` 的 `effective` 回答多一个 `managed` 字段。

managed 层管的是权限,不是沙箱:它挡得住模型发起的调用,挡不住能改 daemon 启动参数或
换掉二进制的人。要防机器的使用者本人,启动方式与二进制也得由部署方控制。

`acceptEdits` 只多放行只读调用:项目内的 `Edit` / `Write` 本来就由工具自己放行,项目外
的、`.git` / `.agix` / `.agents/skills` 下要确认的,在 `acceptEdits` 下照样询问——无人值守的会话里就是拒绝。
只读调用里够得比"读"更远的也不放行(`Tool::accept_edits_allows`):`WebFetch` / `Ping`
只放行 `sandbox.allowed_domains` 列出的主机(含子域),URL 本身就能把数据带到主机那里;
进了会话所在仓库之外的仓库的只读 `git`(`Bash` / `Monitor`)照样询问。读项目外的文件也与
`default` 一样询问:能免询问读的范围是 `security::readable_roots`——会话的工作目录、它能写的
附加目录(`sandbox.additional_directories` 与实例状态目录)和 `sandbox.read_directories`,
`sandbox.read_anywhere` 时不限。`Read` / `Glob` / `Grep` 按目标路径判,只读的 `Bash` /
`Monitor` 命令按 `bash::paths` 对路径参数的判断(含 `cd` 之后的相对路径、glob 在磁盘上的
匹配;`~`、`$VAR`、命令替换展开不了,一律当作项目外)。这些判断与工具自己在 `default` 下
放行用的是同一个函数,所以这几个工具的读在 `acceptEdits` 下不比 `default` 多放行。

`plan` 拒绝一切非只读工具,唯独离开计划模式的那一个(`ExitPlanMode`,
`Tool::leaves_plan_mode`)照常走规则与询问:宿主批准才退出,回到进入前的模式。除此之外
与 `default` 相同:只读工具自己没放行的调用(读项目外的文件、`WebFetch`、只读的 MCP 工具)
照样询问。

`Monitor` 算不算只读看它要跑的命令,与 `Bash` 用同一套分类:`tail -f log` 是只读,
`npm run dev`、`python3 watch.py` 不是——`plan` 下拒绝,`acceptEdits` 下询问。

子 Agent 与团队成员的每次调用都交给父会话的 `Permission` 判
(`Permission::check_for_delegate`),父会话的规则与宿主的回答都作用于它们,输入校验看的是
子 Agent 自己的会话状态(它读过哪些文件)。判的模式是子 Agent 自己的
(`PermissionMode::for_delegate`):父会话在 `plan` 或 `dontAsk` 时取两者中更严的那个,父会话是
`bypassPermissions` 时随父会话。agent 类型 frontmatter 的 `permission_mode` 可以比父会话
宽也可以严,插件带来的、未信任项目里的类型只能收紧(§6.6);模型在 `Agent` 调用、`TeamCreate` /
`Agent(team_name)` 里自己要的 `permission_mode` 只能收紧。子 Agent 的询问经父会话的通道到宿主,等待上限就是会话的
`permission_prompt_timeout`;在那里答"永久允许",记下的是这一条命令或路径,不是整个工具。

skill 正文里的 `` !`cmd` `` 在送给模型之前执行,同样先过会话的 `Permission`(按一次
`Bash` 调用判):deny 规则拒掉的不跑,要询问的也不跑——预处理没有人可问——只有规则
放行的、或 `Bash` 自己静默放行的(项目内的只读命令)才执行,替换成一行写明原因的文字。
skill 自己的 `allowed-tools` 这时还没生效,不能给自己的命令开门。未信任项目自己的 skill
(`.agents/skills/`)里的命令一条都不跑(§6.6)。

模型调 `Skill`、`context: fork` 的 skill 交给子 Agent、用户打 `/name`,三条路径展开 skill
用的是同一个函数(`skills::expand_for_invocation`,内核经 `SkillCatalog::expand` 调到):
去掉 frontmatter 的正文 → `` !`cmd` `` → 参数替换 → 正文截到 8000 字符 → 接上 `files:`
声明的参考文件。`allowed-tools` 授权到这一轮结束(fork 的子 Agent 有自己的 `Permission`,
不受它影响);只写工具名的整类授权只认用户层 skill,项目层与插件包带来的 skill 必须写成
`Tool(内容)`;未信任项目自己的 skill 什么都不授予(§6.6)。

### 6.2 写路径的管控

写文件走两道:

1. **路径请求本身**——未展开的 `~`、UNC 路径、shell 替换、归一化与显示不一致的名字。
   这些是关于"模型递过来的那个字符串"的。前三样会让路径被读错,算输入不合法,在工具的
   `validate_input` 里就拒掉,任何模式都生效。
2. **它解析成的目标**——三份名单加上项目边界与系统目录前缀。目标按会话 cwd 解析;
   路径里有 `..` 而且跟在符号链接后面时,取文件系统解析出的真实位置,检查、读后再写的
   比对和真正的写入用的是同一个路径:
   - 凭据(`.env*` / `id_rsa` / `.ssh` / `.aws` / `.netrc` …)
   - 不该被随手改的生成物与会被别的工具照着执行的文件(`.gitignore` / `Cargo.lock` /
     `package-lock.json` / `.envrc`)
   - 引擎自己的配置(`settings.json` / `settings.local.json` / `mcp.local.json`,且在 `.agix` /
     `.claude` 下)

   最后一条**按"文件名 + 所在目录"匹配,不按目录**。`.agix` 同时是子代理 git worktree
   的落脚点,把整个目录设为拒写会否掉子代理被创建出来要做的每一次写入。`.agix` / `.claude`
   下的配置目录(`hooks`、`agents`、`skills` …)和项目的 `.agents/skills/` 对写工具是"要确认",
   不是"拒写";沙箱里的命令没法逐次确认,所以对它们是拒写(§6.3)。两边用同一份名单
   (`base::paths::ENGINE_CONFIG_{ROOTS,FILES,DIRS}`、`PROJECT_ENGINE_CONFIG_DIRS`,§6.1)。
   另外,凭据名单(§6.1)上的路径在 gate 那一步就拒绝了,任何模式都到不了这里。

   **这个不对称是有意的。** 写工具改引擎配置目录是确认,沙箱里的 Bash 写同一处是硬拒,
   理由有两条:无人值守时确认本来就会变成拒绝,所以硬拒只是把这个结果提前;交互时用户
   看得见 Write / Edit 具体要改哪个文件、改成什么,而 Bash 的写入藏在一条命令里,
   用户批准的是命令,看不见它最后写了什么。

`sandbox.allow_write` 给拒绝规则开例外,`sandbox.deny_write` 加自己的名字,
与读侧的 `allow_read` / `deny_read` 对称。**例外只解除拒绝规则,不解除项目边界**——
放宽可写范围是 `sandbox.additional_directories` 的事,两者分开,不然一个配置会悄悄干另一个的活。

### 6.3 项目之外

可读写的根:项目目录、实例状态目录(`~/.agix`)下的 `scratchpad` 与 `memory`、
`sandbox.additional_directories`。只读的根再加整个状态目录(凭据除外,§6.1)与
`sandbox.read_directories`。
`Read` / `Glob` / `Grep` 与只读 shell 命令在这些根之内直接放行,之外要人批准;
`sandbox.read_anywhere: true` 把读放开到任意位置。这些都是实例的设置——嵌入方创建实例时定。

无人值守的会话没人批准,所以根之外的读写一律拒绝。授权只走设置(上面几项,或
`permission_rules` 的 allow),不从任务文本里读:任务文本常常带着外部内容,让模型照着
一句话给自己开权限,等于让注入的内容给自己开权限。

`Bash` 不能事先知道一条命令会写哪里,所以无人值守时它靠 OS 沙箱:沙箱完整生效
(`Enforcement::Full`)就放行,写入被限制在可写根之内;沙箱不可用(关掉了,或 Linux 上没有
bubblewrap)就和别的未授权调用一样被拒。沙箱只限写不限网,网络缺省是 `Unrestricted`,一条被关在
项目目录里的命令照样能往外连;`sandbox.unattended_requires_network_policy: true` 时自动放行另外
要求 `sandbox.network_mode` 是 `deny_all` / `allowlist`,否则回到 allow 规则。这个开关 0.3.x 缺省
关(保持原来的行为),计划在后续 0.3.x 改为缺省开。沙箱在 `assembly` / `daemon` 缺省编译进来。沙箱只限写,不限读——读的限制落在文件工具上,
`Bash` 读项目外的东西只受凭据黑名单约束:`sandbox.deny_read`(缺省是家目录下的凭据,
`base::interface::exec::default_deny_read`)减去 `sandbox.allow_read`;另外不论怎么配,
§6.1 凭据名单上引擎自己的那部分——状态目录与各场景目录下的设置文件(`settings*.json`、
`mcp.local.json`)和 `mcp/oauth/`,状态目录下的 `daemon/auth/`,以及项目自己的
`.agix/settings.local.json`、`.agix/mcp.local.json`(项目这一层的 key 写在这里)——都读不到,
`allow_read` 也放不开。状态目录里别的东西(skills、memory、scratchpad、会话记录、`scene.json`、
`daemon/instances.d/`)照常可读。`Monitor` 的脚本进同一个沙箱,
`sandbox.require_enforcement` 对它同样生效,无人值守下的自动放行也走上面这同一条件。

沙箱里的命令能写的:工作目录、`sandbox.additional_directories` 与会话授予的目录、
`sandbox.cache_directories`(见下)、状态目录下的 `scratchpad` 与 `memory`、`/dev`,以及
会话自己的临时目录——它作为 `TMPDIR` 交给命令(`<状态目录>/tmp/<会话 id>`),别的会话的
写不到;7 天没动过的,进程里第一次准备临时目录时清掉。
`/tmp`、`/var/tmp` 这类共享的临时目录写不到;macOS 上另外放开当前用户的
`DARWIN_USER_TEMP_DIR`,因为系统的 `mktemp` 不看 `TMPDIR`、只往那里建文件——同一用户的
会话在这里彼此可写,所以会话的 `TMPDIR` 不放在这里。Linux 上
`/tmp` 是每条命令私有、用完即弃的 tmpfs。写规则之前每个路径都先解析符号链接(macOS 的
`/tmp` 实为 `/private/tmp`),否则规则落不到内核检查的那个路径上。

可写范围之内,引擎或 git 之后会在沙箱外照着执行的东西不可写:项目的 `.git`、`.agix`、
`.claude` 这几个目录项本身(不能改名挪走或换成符号链接,不存在时也建不出来)、`.git/config`、
`.git/config.worktree`、`.git/commondir`、`.git/hooks/`,每个子模块的 git 目录(`.git/modules/`
下,嵌套的子模块在上一层的 `modules/` 下)里的同样几项以及通向它的各级目录(不能挪走换一个;
查找不跟符号链接,深度和个数有上限)、`.agix` 与 `.claude` 下的配置
(`settings.json`、`settings.local.json`、`mcp.local.json`,以及 `mcp`、`hooks`、`agents`、
`skills`、`plugins`、`rules`、`scenes`、`policy`、`scripts`、`output-styles` 目录)、项目的
`.agents/skills/` 连同它上面的 `.agents` 目录项(macOS 上不存在时也建不出来;`.agents` 下别的
东西照常可写)、引擎 worktree 的 `.git` 链接;状态目录除
`scratchpad`、`memory` 与本会话的 `tmp/<会话 id>` 外整个不可写。`git add` / `git commit`、在 `.agix` 下写草稿照常可用;
`git config`、`git remote add` 这类改仓库配置的命令在沙箱里会失败。Linux 上 `.agix`(以及已经
存在的 `.claude`)整个只读、已有的工作目录逐个重新打开,所以其下直接新建条目也会失败;Linux
上不存在的 `.claude` 不加挡(挡它会在每个项目里留下一个空挂载点),`.agents/skills/` 同理只在
`.agents` 已经存在时挡(`.agents` 绑定到自身,挪不走;`skills` 只读,不存在时留一个只读的空挂载点),
不存在的 `.agents` 在 Linux 上建得出来;没有 `.git` 的项目、没有
`commondir` 的仓库在 Linux 上补不上这一道(见 `sandbox.rs` 模块头第 3 条),所以有下一段。

引擎自己跑的 git(每轮的 `git status`、worktree 的创建与清理)经 `base::frozen::git::EngineGit` 起:会话的命令进沙箱(没有
`dangerously_disable_sandbox`)时,它也进同一个沙箱,不论有人值守与否——仓库配置让 git 去
跑的 filter driver 之类因此同样受约束。这时它只能写会话的 `TMPDIR`,创建 worktree 时再加
git 目录与 `.agix/worktrees/`,工作目录本身不可写,网络不通。沙箱兑现不了时照
`sandbox.require_enforcement` 办:要求就不跑,当作没有 git 信息(状态不显示,worktree 创建报错),
不要求就和 `Bash` 一样在沙箱外跑。删除 worktree 时目录与 `.git/worktrees/` 下的那一项由
引擎直接删,不交给 git——沙箱不许写已有 worktree 的 `.git` 链接与 `commondir`。沙箱的 git
只给 git 目录就在项目根下 `.git` 的仓库建 worktree,别处的 git 目录沙箱护不住它的配置。
不论进不进沙箱,这些 git 都带 `-c core.fsmonitor=false -c core.hooksPath=/dev/null`。
`/resume @repo` 找仓库根不跑 git,只向上找 `.git`。

沙箱只管命令自己的进程树,管不到它请系统服务代办的事。macOS 上实测:`open` 让
LaunchServices 在沙箱外起一个应用,`defaults write` 让 `cfprefsd` 写任意偏好域,Apple event
能让沙箱外的应用替它做事(Finder 本身拒绝来自沙箱进程的事件,别的应用未必);`launchctl
submit` / `bootstrap` 则被 launchd 拒绝。无人值守会话的 `Bash` / `Monitor` 另外拒绝这几条:
macOS profile 加 `(deny lsopen)`、`(deny user-preference-write)`、`(deny appleevent-send)`,
拒绝 `com.apple.SecurityServer` 的 mach-lookup(`security find-generic-password` 经它读钥匙串),
并拒绝连 tmux(`/private/tmp/tmux-<uid>`、`$TMUX_TMPDIR`、`$TMUX` 指的 socket)与 screen
(`/private/tmp/screens`、`$SCREENDIR`)的 socket——往一个 tmux 会话里敲命令就是在沙箱外跑;
Linux 上 bwrap 加 `--unshare-ipc` 并遮住 `$XDG_RUNTIME_DIR/bus`、`$XDG_RUNTIME_DIR/systemd`、
`DBUS_SESSION_BUS_ADDRESS` 指向的路径与 screen 的 socket 目录(`/run/screen`;tmux 的在 `/tmp`,
本来就是命令私有的;抽象 socket 的总线只有断网才断得掉;Linux 这一半已在 Debian 12 aarch64 的真机
bubblewrap 上跑过,CI 的 `sandbox-linux` job 也在真 bubblewrap 上跑)。`tmux -S` 自己选的 socket 找不到。有人值守的会话不加:命令经人确认,`open` 打开浏览器
之类照常可用;钥匙串也照常可用——`git push` 走 osxkeychain 凭据助手、`codesign`、
`xcrun notarytool` 都要它,而钥匙串对不在 ACL 里的程序还会弹窗再问一次。
网络敞开时,命令能连到的本机服务(sshd、容器守护进程)在任何策略下都是出口。

daemon 自己是最近的那个本机服务:连上它的 socket 就是宿主,能 `mcp.addServer` 让 daemon 在沙箱
外起程序、`project.trust` 自己的项目、`session.respondToPrompt` 自己批准自己的询问。所以不论
哪种网络策略、有没有人值守,沙箱里的命令都连不上它:macOS profile 拒绝连状态目录下的任何 unix
socket(项目就在状态目录里时除外)和这个进程监听的 socket(`--socket` 放在别处的也算),按解析后
的路径比,符号链接、相对路径绕不过;这个进程的 TCP / WebSocket 端口同样拒绝。Linux 上 bwrap
拒绝不了 connect,只能把 `/dev/null` 挂到状态目录与各场景目录下的每个 socket 文件、以及这个进程
监听的 socket 上(目录本身照常可读);TCP 端口在 Linux 上只有断网才断得掉(Linux 这一半同样已在真机 bubblewrap
上跑过,CI 的 `sandbox-linux` job 覆盖)。daemon 这一侧另有一道:unix socket 上对端进程是 daemon 自己起的
(它的子孙,或者还在 daemon 起的某个进程组里——父进程退出后被 init 收养的后台进程也算),连接
直接以 `UNAUTHORIZED` 拒绝,不论命令进没进沙箱;`setsid` 离开进程组的孤儿认不出来,这时靠沙箱。
TCP / WebSocket 靠 token:子进程环境里没有 `AGIX_DAEMON_TOKEN`,但 `--token` 写在命令行上时
`ps` 里谁都看得见,Linux 上这就是一条出口,所以用环境变量传 token。

这个进程监听的 socket、端口和下面的 token 文件登记在 `platform::sandbox::fence_socket` /
`fence_port` / `fence_secret` 返回的 `Fence` 上,跟它们护着的东西同生共死:监听器关了、token
文件删了就注销,之后别的服务用到同一个端口或路径不再被挡。token 文件先删、后注销,存在期间
始终挡着。

**Linux 围不了单个端口。** bwrap 只有 `--unshare-net` 一个网络开关,没有按地址、端口过滤的
原语,所以 `fence_port` 在 Linux 上什么也不做:网络开着(`network_mode: unrestricted`,缺省)时,
沙箱里的命令连得上 daemon 的 TCP / WebSocket 端口,也连得上本机任何监听回环的服务。挡住它的只有
token——子进程环境里没有 `AGIX_DAEMON_TOKEN`。要在 Linux 上把这条路也断掉,用
`network_mode: deny_all` 或 `allowlist`(两者在 bwrap 上都是整个断网),或者不开 TCP /
WebSocket、只用 unix socket。

**沙箱不按域名放行。** `network_mode: allowlist` 对 `WebFetch` / `Ping` 是"只去
`sandbox.allowed_domains` 列出的主机",对沙箱里的 `Bash` / `Monitor` 命令则两个后端都做不到:
sandbox-exec 的 profile 里远端主机只能写 `*` 或 `localhost`,写了别的域名 profile 编译不过,
每条命令都起不来;bwrap 只有 `--unshare-net`。所以 macOS 上 allowlist 是 `(deny network*)`,
名单里有 `localhost` 或回环地址时再放开回环(连得到本机所有监听回环的服务,daemon 自己的端口
除外);Linux 上整个断网,命令只有自己网络命名空间里的回环。名单里的域名两边都连不上,后端报
`Partial`,`unmet` 里列出这些域名:`sandbox.require_enforcement` 打开时每条沙箱命令都被拒,
无人值守的自动放行要求 `Full`,也不放行。`daemon.doctor` 的 `sandbox.network_allowlist` 在名单里
有回环以外的条目时报 degraded。按域名放行得另起一个本机代理,现在没有。

命令没有控制终端:本机 provider 让它领一个新会话(§5 第 4 条),所以它打不开 `/dev/tty`,
也就没法用 `TIOCSTI` 往 daemon 所在的终端里塞按键(CVE-2017-5226 那一类)。bwrap 不加
`--new-session`:它会把 bwrap 的 PID 1 连同整个 PID 命名空间带出命令的进程组,停组就停不到
命令留下的后台进程了。

**已知的缺口。** 上面两道合起来,只有"命令在沙箱外、又 `setsid` 离开了 daemon 起的进程组"这一种
情况挡不住:lineage 按进程组认,`setsid` 之后认不出来;沙箱本来能拦 connect,可这时根本没有沙箱。
没有沙箱的情况有三种:Linux 上没装 bwrap、Windows、设置里开了 `dangerously_disable_sandbox`。
这时一条用户批准过的命令(比如跑仓库脚本的 `npm test`)起的后台进程能连上 unix socket,当宿主用。
这是接受下来的限制,不是疏漏:按对端 pid 往上找祖先在孤儿身上同样断链,文件权限分不出同一个用户
的两个进程。

缓解办法是 `--unix-socket-auth`(缺省关):unix socket 也要先 `daemon.auth`。token 是
`--token` / `AGIX_DAEMON_TOKEN` 给的那个;没给就每次启动新生成一个,写进
`~/.agix/daemon/auth/<instance>.token`(`0600`,目录 `0700`),实例文件的 `token_file` 指向它,
退出时删掉。这个文件登记进沙箱的读禁止名单(`sandbox::fence_secret`):macOS profile 末尾
`(deny file-read* (literal …))`,`sandbox.allow_read` 放不开;Linux 上 `/dev/null` 挂在它上面。
要说清这个开关挡住的是什么:上面三种没有沙箱的情况里,命令同样能读这个文件,所以 token 文件
在那里挡不住;真正起作用的写法是宿主自己起 daemon、经 `AGIX_DAEMON_TOKEN` 传 token、不落盘
——这样连 socket 还得先拿到另一个进程的环境(Linux 上同一用户可以读 `/proc/<pid>/environ`,
门槛高了,但不是没有)。打开它之后每个宿主都要在 unix socket 上握手,所以先做成可选,
之后再考虑缺省开。

包管理与构建工具要写自己的缓存(`pip`、`npm`、`cargo`、`go` …),所以沙箱里的 `Bash`
另外可写 `sandbox.cache_directories`:按工具链分组的缓存目录,缺省列好了 python / node /
rust / go / java / ruby / dotnet / swift 的常见位置,开箱即用。设置文件里写一个组名就整组替换
(`"python": []` 去掉这一组),写新组名就是加一组,没提到的组保持缺省。这些目录只对 `Bash`
放开,文件工具写不到。

`.agix` 下的路径缺省要人确认(它是 agent 的配置),只有 `scratchpad`、`memory` 与 `worktrees`
三处除外:前两处是提示词让模型去写的工作区,`worktrees` 下是 `EnterWorktree` 建的工作树,
里面是项目本身的工作(工作树里自己的 `.agix`、`.claude`、`.git` 照旧要确认)。项目的
`.agents/skills/` 下的写入同样要确认,`.agents` 下别的路径不要。

### 6.4 MCP 策略

`mcp_servers` 是用户和项目自己写的,插件包也会带(`<插件>-mcp-<名字>`)。企业部署要的是
另一回事:不管这些配置写了什么,只有管理员认可的 MCP server 才连得上。这由 MCP 策略决定,
它只看部署方控制的来源,**从不读用户数据目录或项目里的文件**。

**来源**,只取一个,先到先得:

1. 宿主传入 `Assembly { mcp_policy: Some(policy), .. }`(`mcp::policy::McpPolicy`)。
   这时不读任何文件。
2. `agixd --mcp-policy <路径>`(嵌入方:`DaemonConfig::mcp_policy_path`)指定的文件。
   这个文件**必须存在**:写了路径而文件不在,按读不了处理。
3. 系统管理路径:macOS `/Library/Application Support/Agix/mcp-policy.json`,Linux 等
   `/etc/agix/mcp-policy.json`,Windows `%ProgramData%\Agix\mcp-policy.json`。不存在 =
   没有策略,一切照旧。

策略在装配时读一次,改了要重启 daemon。

**读不了就全拒。** 文件读不了、不是合法 JSON、有不认识的键(`allow_servers` 这样的拼写
错误不会被当成"没写"而放行一切)、一条规则写了两个键、glob 写错——一律当作策略在、但
拒绝所有 MCP server,并在 `daemon.doctor` 的 `mcp.policy` 检查里报 `failing`。

**格式**:

```jsonc
{
  "allowed_servers": [              // 可省;省略 = 没被拒的都放行;[] = 全部拒绝
    { "command": "/opt/mcp/bin/github-mcp" },
    { "command": "/opt/mcp/bin/*" },
    { "url": "https://*.corp.example/**" }
  ],
  "denied_servers": [               // 可省
    { "command": "/tmp/**" },
    { "name": "shell" }
  ],
  "allowed_authorization_servers": [ // 可省;省略 = 任何授权服务器;[] = 全部拒绝
    "https://login.corp.example",
    "https://corp.okta.com/oauth2/default"  // 写具体租户;*.okta.com 放进任何人的租户
  ],
  "enterprise_idp": {               // 可省;企业托管授权换身份的 IdP(mcp_auth.md §4)
    "issuer": "https://login.corp.example",
    "token_endpoint": "https://login.corp.example/oauth2/token",
    "client_id": "agix"
  }
}
```

`allowed_authorization_servers` 管 OAuth(§6.5):远端 server 的授权服务器 issuer 规整成
`scheme://host[:port][/path]`(末尾不带 `/`)后按 glob 匹配,不匹配的在任何跳转、注册、令牌
请求之前被拒绝,`mcp.auth.start` 报 `MCP_SERVER_DENIED`。`enterprise_idp` 给出
`"grant": "enterprise"` 的 server 用来交换用户身份的 IdP;宿主经 `Assembly::mcp_enterprise_idp`
给了的话以宿主的为准。策略读不了时两者都不生效:没有授权服务器被允许,也没有 IdP。
issuer 的 glob 不要通配托管 IdP 的域名:`https://*.okta.com/**`、`https://login.microsoftonline.com/**`
放进的是这家服务上**任何**租户,攻击者自己开一个就能当上"允许的"授权服务器;写到具体租户
(`https://corp.okta.com/oauth2/default`、带租户 ID 的路径)为止。

每条规则恰好一个键,按它匹配:

| 键 | 匹配对象 | 规则 |
|---|---|---|
| `name` | server 的名字(`mcp_servers` 的键;插件的是 `<插件>-mcp-<名字>`) | 完全相等;**只在 `denied_servers` 里起作用** |
| `command` | stdio server 的 `command`,`$VAR` / `${VAR}` / `${VAR:-默认}` 展开之后 | glob;不带通配就是完全相等 |
| `url` | 远端 server 的 `url`,同样先展开,再规整成 `scheme://host[:port]/path` | glob |

**`name` 不放行。** 名字是写 `mcp_servers` 的人自己起的键,谁都能把任意命令或 URL 起名叫
`github`,所以 `allowed_servers` 里的 `name` 规则不放行任何 server——放行要落在实际跑的
`command` 或实际连的 `url` 上。这样的规则不让策略失效(其余规则照常),`daemon.doctor` 的
`mcp.policy` 报 `degraded` 并在 `inert_allowed_rules` 里列出它们;被它"命中"却因此被拒的
server,拒绝原因会说明。`denied_servers` 里的 `name` 照常生效:按名字拒绝不会放进不该放的。

glob 按 §6.1 的约定:`*` 不跨 `/`,`**` 跨。所以 `{"command": "npx"}` 只放行写成 `npx`
的命令,不放行 `/tmp/evil-npx`,也不放行 `/usr/local/bin/npx`;`/opt/mcp/bin/*` 不放行
`/opt/mcp/bin/sub/x`。匹配前 `command` 先按字面规整(去掉 `.` 段与重复的 `/`,`..` 按上一级
折算),含 `..` 的命令不命中任何 `allowed_servers` 规则——`/opt/mcp/**` 不放行
`/opt/mcp/../../tmp/evil`——但照样按折算后的路径去比 `denied_servers`。macOS 的文件系统不分
大小写,`command` 规则在那里也不分(`/tmp/**` 拦得住 `/TMP/evil`)。`command` 规则不匹配远端 server,`url` 规则不匹配 stdio server。
规则不看 `args`:放行一个解释器(`/usr/bin/python3`、`node`)就放行了它能跑的任何脚本。
不带 `/` 的命令名在起进程时按 `PATH` 找,而 `PATH` 用户改得了(daemon 的环境、server 配置
的 `env`),所以要钉死一个二进制,放行它的绝对路径。

URL 规整去掉了 userinfo、query、fragment,主机名小写、去掉末尾的 `.`、默认端口省略,
`ws://` / `wss://` 当作 `http://` / `https://`(WebSocket server 连不上时会回退到同一主机的
HTTP;规则里写的 `ws://` / `wss://` 同样换过来):`https://*.corp.example/**`
不会被 `https://evil.example?.corp.example/` 或 `https://evil.example/.corp.example/` 骗过,
`denied_servers` 里的 `https://evil.example/**` 也拦得住 `wss://evil.example/` 和
`https://evil.example./`;它不匹配显式写了非默认端口的 `https://mcp.corp.example:8443/`
(要放行就写进模式里)。路径要用 `/**`:`https://h/*` 只匹配一层路径。

**deny 是尽力而为,allow 才可靠。** 匹配只看规整后的字面:同一台主机换个 IP 字面量
(`https://203.0.113.7/`)、换个 DNS 别名、换个装着同一个二进制的路径,`denied_servers` 都拦
不住。要可靠地限制连什么,写 `allowed_servers`。

**裁决**:命中任一条 `denied_servers` 就拒;否则有 `allowed_servers` 时必须命中其中一条;
都没写就放行。deny 永远优先,和 §6.1 一样。

**作用在哪里**:会话池连 MCP 的四个入口都先过同一道 `SessionPool::admit_mcp`——启动时的
连接、`config.reload`(以及设置文件变化触发的重载)后的重连、另一个项目第一次起会话时的
连接、`mcp.addServer` / `mcp.updateServer`。插件声明的 server 在这四处都和 `mcp_servers`
合在一起、一视同仁。被拒的 server **不起进程、不发请求**;启动与重连时发一条
`mcp_connect_failed`(`error` 写明是哪份策略、哪条规则),在 `mcp.status` 里以
`"state": "denied"` 列出;`mcp.addServer` / `mcp.updateServer` 在写 settings.json **之前**
就以 `MCP_SERVER_DENIED` 拒绝。见 `daemon_rpc_protocol.md` §6.5、§8、§9。

**不受管的**:宿主经 `customize_session` 或自己的 `Builder` 装进会话的 `McpModule`。它们是
宿主的代码,不是用户能改指向的配置;要管,宿主自己管。

**项目层的 `$VAR` 只从 server 自己的 `env` 块取。** 项目 `.agix/settings.json`(以及
`settings.local.json`)里声明或改写过的 server——`mcp.addServer` / `mcp.updateServer` 写的
就是这一层,其中 `headers` 与 `env` 落在 `settings.local.json`——`command` / `args` / `url` / `headers` 里的 `$VAR` 只用它自己 `env` 块里的值
展开,`env` 块的值本身不展开,一律不读 daemon 的进程环境;它的 `oauth` 也不能用
`client_secret_env` / `private_key_jwt.key_env`(读的是 daemon 的环境)、
`private_key_jwt.key_file`(读本机的私钥文件)和 `"grant": "enterprise"`(花宿主交来的企业身份),
写了就以 `mcp_connect_failed` 拒绝连接。全局层、场景层和插件声明的 server 照旧用 daemon 的环境展开。
这条对已经信任的项目同样成立:克隆来的仓库不应该能把 `$GITHUB_TOKEN` 发到它指定的 URL。
项目层只改了全局 server 的一部分(比如只换了 `url`)也算项目层的 server。未信任的项目,
它自己的 server 根本不启动(§6.6)。

### 6.5 MCP 授权

远端 MCP server 要求 OAuth 时(MCP Authorization 规范 2025-11-25),登录由宿主驱动,令牌由
daemon 管。设计与用法在 [`mcp_auth.md`](mcp_auth.md);这里只记它在整体里的位置:

- **每个会话池一个 `mcp::oauth::McpAuth`**:各 server 的授权状态、进行中的登录、凭据存储
  (默认 `scenes/<id>/mcp/oauth/`,一个 server 一个 0600 文件;宿主可经
  `Assembly::mcp_credential_store` 换成自己的)。同一进程里的两个池互不相见。
- **连接时**,能用 OAuth 的 server(`streamable_http` / `sse`,没配固定 `Authorization` 头)
  经 rmcp 的 `AuthClient` 发请求:有令牌就带上,快到期先刷新,被 401 拒了先刷新再重试一次;
  没有令牌就裸发——不要求授权的 server 照旧能用。
- **要求登录的 server 不算连接失败**:它不重试,以 `auth_required` 留在这个项目的 MCP 管理器
  里,不提供工具;模型在系统提示里看到一句"这个 server 要用户先登录"。
- **状态变化回到会话池**:`McpAuth` 的监听器发 `mcp_auth_*` 通知;令牌失效时会话池把这个
  server 从所有缓存的连接里摘下,登录完成后只重连它,受影响的会话在下一轮之前重建。
- **MCP 策略在授权之前**:被 §6.4 拒绝的 server 不连接,也就走不到 OAuth;授权服务器在任何
  跳转之前对照 `allowed_authorization_servers`。
- **无人值守的 server**(`client_credentials`、企业托管授权)不经过登录:令牌在每次请求之前
  检查,快到期或被拒时重新取。企业身份由宿主经 `mcp.auth.setIdentity` 交来,只在内存里。
- **地址过滤与代理**:OAuth 请求与非回环 server 的 MCP 连接都过 `mcp::oauth::http` 的地址
  过滤,判定与模型发起的请求共用 `is_internal_address`。配了 `HTTP(S)_PROXY` 时代理替我们解析
  DNS,过滤只看得到主机名与 IP 字面量(`mcp_auth.md` §7)。
- **工具名的 server 段在一个管理器里唯一**:`mcp__<段>__<工具>` 的段由 server 名确定地造出
  (`mcp::naming`),本身合规的名字原样用,别的清洗后接哈希,两个不同的名字可能落到同一段
  (`my_srv_<哈希>` 与 `my.srv`)。连接前查重:同时配置的几个撞了,一个都不连,`mcp.status`
  里都是 `failed`,`error` 点名撞上的那个;后来经 `mcp.addServer` 加进来的撞上已有的,只拒
  新来的。不给撞的加哈希,是因为那会改掉一个合规名字的工具名,写好的权限规则跟着失效;都拒
  而不是挑一个留下,是因为两边谁是"本来的"无从判断,留错一个就让权限规则落到别人的工具上。

### 6.6 项目信任

项目层——`<项目>/.agix/settings.json` 与 `settings.local.json`、`.agix/agents/`、
`.agix/hooks/`、`.agents/skills/`——是仓库内容,克隆来的仓库里就是作者写的东西。§6.1 到
§6.4 的那些防线都假设项目可能是敌意的;没有信任这一层,同一个仓库只要在自己的设置里写
`permission_mode: "bypassPermissions"`、一条 hook 或一个 MCP server,就绕过了它们。

**未信任的项目层按插件级信任处理**——复用插件包已有的那条"网上来的东西只能新增、只能收紧"
的规则(`plugin_packages.md` §信任),不另造一套权限体系:

| 项目层内容 | 未信任时 |
|---|---|
| `permission_mode` | 只能比它下面几层合出来的更严 |
| `permission_rules` | `deny` / `ask` 照常,`allow` 丢弃(含 `settings.local.json` 里"永久允许,本项目"记下的) |
| `hooks_config`、`scripts` | 整段忽略;`.agix/hooks/` 也不放进 hook 命令的 `PATH`,继承的 `PATH` 里的相对条目去掉,用户层 hook 写的裸脚本名不会落到项目里的同名文件;用户层 command hook 以相对路径起的程序(`./scripts/check.sh`)、或命令里哪个相对路径的词正是项目里的文件或目录(`python3 scripts/check.py`、`node .`、`bash x.sh`、`< x.sh`)的,不跑,算 hook 失败——工作目录仍是项目,那个路径指的是仓库里的文件(`hooks.md`) |
| `mcp_servers` | 不启动;`mcp.status` 里是 `"state": "untrusted"`;`mcp.addServer` / `mcp.updateServer` 以 `PROJECT_UNTRUSTED` 拒绝,什么都不写 |
| `allow_client_permission_override: true`、`disable_skill_shell_execution: false` | 丢弃 |
| `sandbox`:`dangerously_disable_sandbox` / `read_anywhere` 设为真,`require_enforcement` / `unattended_requires_network_policy` 设为假,`network_mode` 更松 | 丢弃 |
| `sandbox`:`additional_directories` / `read_directories` / `allow_read` / `allow_write` / `allowed_domains`,`cache_directories` 的某一组,`subprocess_env.allow` | 只要多出下面几层没有的条目就整项丢弃;只删不加的保留 |
| `sandbox`:`deny_read` / `deny_write`、`subprocess_env.mode` | 少了下面几层有的条目(或从 `allowlist` 改回 `inherit`)就丢弃;`deny_read` 非空会替换内建的凭据清单,所以下面几层没写时项目也只能不写 |
| `providers` | 整段不要:改一个已有 provider 的 `base_url`,用户全局层的 `api_key` 就会跟着发过去;新 provider 的 `api_key_env` 能把 daemon 的任意环境变量当 key 发出去 |
| `default_provider`、`task_models.<任务>` | 指向项目自己的 provider(下面几层没有的)时丢弃;选下面几层已有的照常 |
| `model.base_url`、`model.auth_token` | 丢弃 |
| `telemetry_url`、`recorder` | 丢弃:一个把遥测送到项目指定的地址,一个把宿主请求的录像写到项目指定的目录 |
| `instruction_file` | 丢弃:它按路径直接读进系统提示词,不过沙箱也不过 Read 的 `deny_read` |
| `plugins` | `enabled: true`(下面几层是 `false`)丢弃;`allow` 放进下面几层没有的名字、或清空下面几层的清单,丢弃;只收窄的保留 |
| `.agix/agents/*.md` | 来源是 `AgentTypeSource::Project { trusted: false }`:`permission_mode` / `max_turns` 只能收紧,与内建、插件、用户层同名的不替换它们 |
| `.agents/skills/` 里的 `` !`cmd` `` | 不执行,换成一行说明(与 `disable_skill_shell_execution` 同一条路) |
| `.agents/skills/` 里的 `allowed-tools` | 什么都不授予:模型调用这个 skill 之后,它列的工具照常询问 |
| 其余(提示词、`rules`、记忆、在下面几层已有的 provider 与模型之间选择……) | 照常 |

文本到达模型本来就要靠人读(插件同一判断),所以提示词一类照常;决定对话、凭据、遥测送到哪里,
或者把用户的哪个文件读进提示词的键,按上表扣下。全局层、场景层里写的脚本绑定若是相对路径,
在未信任的项目里不绑定——它会按项目根解析,读到仓库放在那里的文件。stdio MCP server 不论项目
信任与否,都以声明它的那一层的目录为工作目录启动:全局层的在全局配置目录、场景层的在场景配置
目录、插件的在插件包根目录、项目自己的在项目根;相对的 `command` 与参数里的相对路径都在那里解析,
从不落到 daemon 的工作目录(它可能是任何一个项目)。哪一层算"声明它的",看哪一层给了它的
`command`:项目层只补了 `env` 的全局 server 仍在全局目录启动。信任之后项目层恢复全部行为。

一个值类型不对(`"language": 5`、`"network_mode": 42`)时,`Settings::load` 只丢掉这个值并
warn,`daemon.doctor` 的 `settings.tiers` 报 `degraded`、在那一层的 `ignored_values` 里列出;
同一个文件里其余的值和别的层照常。整个对象的值都不对时报这个对象本身。这对信任与否都一样:
一处拼错不会让全局层的 hook、沙箱设置跟着失效。`permission_rules` 逐条读:一条写错的规则只丢它
自己(`ignored_values` 里的键是 `permission_rules[0]` 这样带下标),同一层的 deny 照常生效。

**信任是项目根的属性,存在全局层,项目自己改不了**:

```jsonc
// ~/.agix/settings.json 或 settings.local.json
{ "project_trust": {
    "default": "trusted",                       // trusted | untrusted;0.3.x 缺省 trusted
    "trusted": ["/Users/x/work/**", "/srv/ipd/lines/*"] } }
```

判定:没有自己的项目层(无项目会话,或项目的 `.agix` 就是全局 / 场景目录)→ 不涉及信任;
`trusted` 里有一条命中项目根(路径相等,或 glob:`*` 不跨 `/`,`**` 跨,`~/` 是家目录;
符号链接两种写法都认)→ 信任;否则取 `default`。比的是项目根解析之后的真实目录,原样给出的
写法只在不含 `.` / `..` 时也拿来比,所以 `/home/u/work/**` 不会命中
`/home/u/work/../../tmp/x`。`project_trust` 只从全局层读,场景层、项目层里写的忽略并记一条
warn。全局层 `settings.json` 与 `settings.local.json` 的
`trusted` 清单相加,不是后者盖前者。合并时的判定结果在 `Settings::project_layer`:
信任状态、命中的那条、以及项目层里哪些东西要信任才生效(`requires_trust`)。

宿主经 `project.status` 读状态与 `requires_trust`(用来画"信任这个项目吗"的对话框),经
`project.trust` 把项目根加进或移出全局 `settings.local.json` 的 `trusted`,之后这个项目的
绑定与 MCP 连接重建,发 `project_trust_changed` 通知。`project.trust` 只收绝对路径,记下的是
解析之后的真实目录,其中的 glob 元字符(`*`、`?`、`[` 等)转义掉——`Project [v2]` 这一条只
命中它自己。往未信任项目的项目层里写配置的方法(`mcp.addServer` / `mcp.updateServer`、
`config.setProvider`)以 `PROJECT_UNTRUSTED` 拒绝,什么都不写:写进去也不会生效。
`session.create` / `session.resume` 的结果带 `project_trust`,宿主不必另查。
`daemon.doctor` 的 `project.trust` 检查报 daemon 自己的项目:未信任且有东西被扣下,或只靠
`default: trusted` 才信任而有东西要信任才生效,都是 `degraded`。确认对话框是宿主的事;无人值守部署用 `trusted` glob 预先信任
它们自己创建的项目目录。

**0.3.x 缺省 `trusted`**,行为与之前完全一样,`daemon.doctor` 提示哪些内容在缺省翻转后会停用;
**计划在后续 0.3.x 把缺省改成 `untrusted`(具体版本发布说明另行通知)**。

引擎自己改设置文件的地方——`project.trust`、`config.setProvider`、`mcp.addServer` /
`updateServer`、权限询问里的"永久允许"——都走 `base::settings_file`:读改写期间锁住那一层
的目录(进程内一把锁,unix 上跨进程靠目录上的 `flock`),写进同目录的临时文件、落盘之后改名替换,
读的一方只会看到旧文件或新文件。`settings.local.json` 每次写都是 0600,别的文件保留原来的
权限。设置文件是符号链接的,读写都拒绝:项目层里的链接是仓库内容,顺着它写等于让仓库挑一个
用户的文件来改写,把链接换成普通文件又会悄悄拆掉用户自己的 dotfile 链接。

---

## 7. 锁

两把锁,都靠 `flock`,进程无论怎么退出内核都会释放:

- **daemon 实例锁** `daemon.lock`:拿到 `flock` 之后,再看文件里记的上一个持有者——不持
  `flock` 的旧版本 daemon 也挡得住。这一步由 `platform::process_identity::decide_stale_lock` 判。
- **会话日志的单写者锁** `sessions/<id>/writer.lock`(§1.3):只有 `flock`,不记持有者。

`decide_stale_lock` 的判定依据是 **pid + 进程启动时间**,不是文件 mtime,也不是超时:

| 存活性 | 决策 | 理由 |
|---|---|---|
| `Alive` | 保留 | 持有者还在 |
| `Unknown` | 保留 | 无法确认时保守——宁可拒绝启动,不可双开 |
| `Reused` | 回收 | pid 被复用,原持有者已死 |
| `Dead` | 回收 | 进程不存在 |

**记启动时间是为了防 pid 复用。** 只比 pid 会把碰巧复用了旧 pid 的无关进程误判为持有者,
从而永久拒绝启动。

---

## 8. 磁盘布局

`~/.agix/` 下按**索引维度**分目录,不按产品名或场景分:

| 目录 | 索引键 | 内容 |
|---|---|---|
| `projects/<sanitized-cwd>/` | 项目 | `<session-id>.jsonl` transcript |
| `projects/_no-project/` | ——(保留) | 无项目会话的 transcript |
| `sessions/<session-id>/` | 会话 | 会话记忆、metadata、输入历史、`writer.lock`(§1.3) |
| `memory/` | 全局 | 跨会话记忆,模型照提示词写在这里;项目级的(抽取出的 `type: project`)在 `<项目>/.agix/memory/`。两处只由 `MemoryStore::for_paths` 推出,提示词里的索引是两处合并的;写入按目录加锁(`base::dir_lock`:进程内一把锁,unix 上再对目录 `flock`,别的 daemon / CLI 进程也排队)、先写临时文件再改名;存到 200 条(`MAX_MEMORIES`)就不再抽取新的,不自动删,`daemon.doctor` 的 `memory` 检查报 `degraded`(报的是磁盘上的实数);`Write` 工具直接写进目录的不经这道检查,所以读取时也只读最近的 200 条、索引只列 200 条,多出的打一条 warn |
| `scenes/<id>/` | 场景 | 该场景的设置层(`settings.json`、skills、agents);声明式场景在这里放 `scene.json` + `prompt.md` |
| `scenes/<id>/mcp/oauth/` | MCP server URL | 远端 MCP server 的 OAuth 凭据,一个 server 一个 0600 文件(§6.5) |
| `daemon/groups/<instance>.json` | 实例 | 在跑命令的进程组,供崩溃后下次启动清理 |
| `daemon/crashes/` | 进程 | 崩溃记录 |

项目里的 `<项目>/.agix/teams/<team_id>/` 是一个 `TeamCreate` 团队的全部状态:`team.json`、
`state.json`、`events.jsonl`、`SCRATCHPAD.md`、`mailbox/`、`memory/`(团队共享记忆,
格式同个人记忆)。每个阶段开始时成员读到 `memory/` 的索引;`TeamDelete` 删掉整个目录。

项目的 `.agix/` 在用户的仓库里。引擎第一次往一个设置目录写 `settings.local.json` /
`mcp.local.json`(里面是 key 与 MCP headers),或第一次往项目 `.agix/telemetry/` 录遥测时,
那个目录还没有 `.gitignore` 就生成一份(`base::paths::LOCAL_DATA_GITIGNORE`:
`settings.local.json`、`mcp.local.json`、`telemetry/`)。已有的 `.gitignore` 是用户的,不覆盖,
缺的行也不补。状态目录与场景目录写 `settings.local.json` 时同样会生成,无害。`scratchpad/`
自己另有一份 `*`。

**会话进哪个项目目录由它自己的 `Meta.project_root` 决定**,不是由 daemon 的 cwd
决定 —— 一个 daemon 同时服务多个项目时,后者会把所有项目的 transcript 堆进一个
目录。侧链没有自己的 `project_root`,跟父会话进同一个目录(级联删除按那里找)。

sidecar 不能塞进 `projects/<cwd>/`,因为按 session id 查找时没有项目信息可用。
两者是并列的两个维度,不是两层嵌套。**反过来,"这个会话属于哪个项目"也只有
sidecar 答得出**:`Meta` 在 transcript 里,而要读 transcript 先得知道它在哪个项目
目录下。所以 `Meta` 落盘前先写 sidecar,顺序与计数器那类"从已有日志算出来的"
附带信息相反。

区分两类目录靠**目录名能否解析成 `SessionId`**:sanitize 后的 cwd 必然以 `-` 开头
(绝对路径首字符是分隔符),`_no-project` 以 `_` 开头,都解析不成 base58 id。

---

## 9. 扩展面

引擎有 45 个扩展点,分三类:**契约** 30 个(换掉一个子系统)、**拦截** 9 个
(坐在某件事的路径上)、**注册** 6 个(往一个集合里贡献)。

扩展只有两种:**Rust**——契约直接 `impl` 那个 trait 交给 `runtime::agent::Builder`,
注册与拦截打包成一个 `runtime::plugin::Plugin`,全部扩展点可用;**QuickJS 脚本**——给不
重新编译引擎的人,挂到其中九个点上,本身也是一个 `Plugin`(`script_host::ScriptPlugin`,
`Layer::QuickJs`)。两者落在同一个点上时按层排:Rust 先(包裹时在内),
QuickJS 后(包裹时在外),与交给 `Builder` 的先后无关(`extension_points.md`「叠加顺序」)。脚本和 `RunCode`
跑在同一套内嵌 QuickJS 里,限额限的是开销,不是对恶意代码的隔离,所以脚本能做什么按它
从哪来定。引擎不带 WebAssembly 运行时;需要的上层在 Rust 接口上自己做。

完整清单、各自的触发时机与信任级别在 **`extension_points.md`**——那张表是从
`base::interface::catalog` 生成的,有测试保证它和代码一致。

```
cargo build -p daemon                                          # QuickJS + 插件包(默认)
cargo build -p daemon --no-default-features --features full    # 两者皆无
```

怎么写脚本见 `extending_quickjs.md`;插件包的格式见 `plugin_packages.md`;两种插件的开发
指南与样例见 `plugin_development.md`。

hook 的协议与三十个事件见 [`hooks.md`](hooks.md);遥测的文件、信封与事件清单见
[`telemetry.md`](telemetry.md)。
