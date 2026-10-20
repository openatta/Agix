# AttaCore 架构

AttaCore 是一个**库**,不是一个应用。它提供的是"跑一个 agent 会话"这件事的引擎:
装配提示词、调模型、执行工具、管理上下文与历史。**交互界面不在这里**——那是使用方
应用的事。`daemon` 是仓库里附带的一个参考宿主,
把引擎包成 JSON-RPC 服务,同时也是集成测试的载体。

这份文档讲**组件、概念、和它们之间的约束**。想知道怎么用请看:

- `daemon_rpc_protocol.md` —— 通过 RPC 使用它
- `extension_points.md` —— 能扩展什么(总揽)
- `extending_quickjs.md` / `extending_wasm.md` —— 怎么扩展

---

## 1. 分层

```
                    daemon  (参考宿主:JSON-RPC / 会话池 / 插件安装)
                      │
   runtime ── 轮次循环、流式、工具调度、子代理生命周期
      │
   ┌──┴──────────────┬───────────┬──────────┬──────────┐
 tools            model      history    compaction   skills / hooks / mcp / team / task
 (39 个内建工具)   (协议适配)  (日志)     (上下文治理)
      └────────────────┴───────────┴──────────┴──────────┘
                      │
              core (包名 base) —— 所有契约、所有共享类型
```

**`core` 里没有任何第一方依赖。** 它定义 trait 与数据类型,别的 crate 实现它们。
这条约束是整个扩展面成立的前提:一个宿主要替换某个子系统时,面对的是 `core` 里的
一个 trait,而不是某个具体 crate 的内部结构。

| crate | 职责 |
|---|---|
| `core`(包名 `base`) | 契约与共享类型。所有 `trait`、`Settings`、消息与提示词类型、扩展点清单 |
| `runtime` | 轮次循环、流式解析、工具调度、子代理与团队的生命周期 |
| `model` | 模型协议适配(Anthropic Messages / OpenAI Chat Completions),重试与退避 |
| `tools` | 内建工具的具体实现 |
| `history` | 会话日志:`HistoryStore` 契约、JSONL 实现、投影、查询、大对象外置 |
| `session` | 内存中的会话状态(消息列表、会话记忆、摘要) |
| `compaction` | 上下文压缩:何时压、怎么压 |
| `permissions` | 权限规则匹配、模式分发、路径安全 |
| `skills` / `hooks` / `mcp` / `team` / `task` / `telemetry` / `auth` | 各自领域 |
| `script-host` | QuickJS 脚本载体 |
| `plugin` / `plugin-host` / `wasm-host` / `plugin-compiler` | WASM 插件载体 |
| `daemon` | 参考宿主 |

---

## 2. 一轮是什么

`crates/runtime/src/turn.rs` 是引擎的主循环。**步骤顺序是骨架,不开放**;
每一步上"选哪条路"的判断是决策,**开放**。

```
收用户消息
  └─ UserPromptSubmit hook · 斜杠命令 · 记忆召回
循环:
  ├─ 1. 停止判断    TurnPolicy::before_model_call   ← 这一轮还该不该继续
  ├─ 2. 压缩        Compactor::should_compact       ← 该不该压、压什么
  ├─ 3. 硬顶        ContextBudget::hard_cap
  ├─ 4. 装配提示词  PromptAssembler / 注册的块 / 装配钩子
  ├─ 5. 调模型      Model + ModelInterceptor + BackoffPolicy
  │     └─ 出错 → RecoveryPolicy:换模型 / 压缩重试 / 抬 max_tokens / 失败
  ├─ 6. 复查        TurnPolicy::after_model_call
  ├─ 7. 记账        BudgetPolicy::on_usage          ← 继续 / 警告 / 耗尽
  ├─ 8. 执行工具    Permission → ToolMiddleware → Tool → ToolResultTransformer
  └─ 9. 有工具调用则再来一轮;否则 BudgetPolicy::on_output_target 决定是否续写
```


**骨架与决策的分界是这套设计的核心。** 步骤顺序写死,是因为顺序本身就是"一轮"的定义;
而每一步的判断——跑几步停、错了怎么办、什么时候压缩、花多少 token——是意见,
不同部署持不同的意见。所以它们在契约后面。

---

## 3. 会话与场景

### 3.1 一个进程可以跑多个场景

场景(`AgentScene`)是无状态的:`CodingScene` / `ChatScene` / `ResearchScene` 都是零字段
unit struct,`Arc<dyn AgentScene>` 只是一组函数。所以同一进程并存多个场景没有障碍。

- `--scene` 定 daemon 的默认场景,`--scenes chat,research` 追加激活
- `scene.activate` / `scene.deactivate` 运行期增删
- `session.create {"scene": "chat"}` 指定该会话用哪个;未激活的场景返回 `SCENE_NOT_FOUND`
- **每个会话按自己的场景解析设置层**,包括 `~/.atta/scenes/<scene>/` 下的 settings、
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
`global_data_dir` 本身,而不是 `<project>/.atta`。

`config.*` / `mcp.addServer` / `import.*` 这几个**作用在项目层**的方法用的是另一套
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
| 场景注册表 | 同上;激活集合是运行期状态,但不按项目 |
| plugin host / 插件集合 | 插件只有全局层与场景层,**没有项目层** |
| WASM 插件的 `${workspace}` | 组件是池级加载的,跟着 daemon 的工作目录 |
| `WatchHub` | 一个 `notify` 线程,按 tier 分键去重,watched 目录是 `1 + M + P` |

**加一样按项目索引的东西,就把它放进 `ProjectBinding`**,不要在池上再挂一个
`HashMap<ProjectSceneKey, _>` —— 那正是这条不变量曾经在四个地方同时不成立的原因。

### 3.3 Sidechain 会话

子代理(`Agent` 工具、Skill `context: fork`、team 成员)的 transcript 独立成一个会话,
通过 `Meta` 里的 `parent_session_id` + `session_kind: Sidechain` 与父会话关联。
可经 `HistoryStore::child_sessions` 找到,默认不出现在 `session.list`。

跑完写 `LogEntry::SessionEnd { state }`。**被取消的轮次刻意不写这个标记**——取消是被打断,
不是到达终点,所以会话必须保持可 resume。对已写终止标记的 sidechain 发起 resume 会被拒。

关闭父会话会**级联删除**其全部 sidechain:父会话一旦关闭,就没有任何路径能再触达它们。

### 3.4 委派深度

委派链由 `EngineConfig::max_agent_depth` **计数**约束,不靠从工具集里摘掉 `Agent`。
`Inner::spawn_guard` 在每一条 spawn 路径上检查;到顶时返回普通工具错误,模型看得见,
可以改为自己做。

**过滤工具名不构成约束**:`Builder::build()` 会为子会话新建注册表并把 `Agent` 注册进去,
与父级传入的工具集无关;`RuntimeAgentSpawner`(Skill fork、team 成员)直接透传
`sub_tools()`,根本不经过 `AgentTool::resolve_tools`。

### 3.5 场景的工具面

场景对工具有三种方向不同的控制:

| 方法 | 作用 |
|---|---|
| `tools()` | 白名单,与注册表取交集——**只能减**;以 `*` 结尾的项按前缀匹配(`mcp__*`) |
| `extra_tools()` | 贡献注册表里没有的工具——**只能加** |
| `deferred_tools()` | 哪些工具只报名字不带 schema |

白名单(连同白名单为空时的 `disallowed_tools()`)经 `AgentScene::permits_tool` 作用在会话
注册表本身:`Builder::build()` 把不许的工具从注册表里移走,之后运行期才出现的 MCP 适配器
也按它过滤。宣告、`ToolSearch`、分派读的是同一张表,不存在"不宣告但能调用"的工具。

三条硬约束:

1. **`extra_tools()` 重名直接失败,不覆盖。** 注册表按名字返回首个同名项,静默覆盖会让
   调用点拿到的和模型看到的不是同一个工具。检查跑在引擎态工具注册**之后**,
   否则场景能占用 `Agent` / `Skill` 这些名字并赢得查找。
2. **`ToolSearch` 永不 defer。** 它是取回 deferred 工具 schema 的唯一途径。
3. **`Tool::is_deferred()` 不是策略输入**,是 `DeferredTool` 包装器设置的输出。
   工具自己声明它没有裁剪 schema 的效果——只会让它对 `ToolSearch` 撒谎。

---

## 4. 历史:状态即日志

会话状态存在一份 append-only 的 JSONL 里,`history::transcript` 把它**投影**成模型看到的消息。

- **投影可换**(`TranscriptProjection`),挂在 store 上——日志和读它的规则必须一起走,
  否则 resume / fork / 搜索会看见三个不同的过去
- `LogEntry::Extension { ns, event, payload }` 让内核之外的东西(插件、脚本、宿主)
  把状态放进同一条时间线。**未知 `ns` 必须可跳过**:卸载插件后旧日志仍能加载
- **模型可见即可从日志重建**。一个靠询问活着的插件来渲染条目的投影,会产出一个
  重新打开就不一样的会话。`model_visible_content_is_reconstructible` 把这条变成可检查的
- 大内容外置到 `BlobStore`,日志里只留引用;**引用解析不了时条目保持惰性,不是报错**

**留存的时间戳与标识符走 `Environment` 契约**,所以同一段录制能重放出逐位相同的日志。
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

两个 provider:`ExecProviders::local()`(本机,默认)与 `in_process()`
(内存文件树、预置的命令应答、离线网络、诚实地报告自己什么都不约束的沙箱)。

五条容易搞错的:

1. **路径安全在契约之上,符号链接解析在契约之内。** 一个路径能不能写是策略,
   provider 不该能取消它;但远端的符号链接图是远端的。顺序是:
   `provider.canonicalize` → `check_write(已解析的路径)` → `provider.write`。
2. **出网策略问的是"模型能够到哪里"**,不是"这个进程能连出去哪里"。请求带着
   `Origin`:模型选的目标受 `allowed_domains` 约束,运维配的端点(模型 API、MCP、遥测)不受。
   把它作用到模型 API 上会让 agent 连自己的大脑都够不着。
3. **受约束的策略绝不静默降级为无约束执行。** 后端报告 `Full` / `Partial` / `None`
   并列出兑现不了的部分;`sandbox.require_enforcement` 决定不足 `Full` 时是否拒绝。
4. **一个命令是它起的整个进程组。** 本机 provider 让每个子进程领一个进程组,停的时候
   整组停;前台进程退出后输出最多再等 `OUTPUT_GRACE_AFTER_EXIT`,还被后台进程占着就
   连它们一起停。Bash、Monitor、command hook、MCP stdio 都是这条规矩。
5. **子进程的环境只在一处决定。** `base::interface::exec::local::SubprocessEnv`:缺省
   继承宿主环境但去掉引擎自己的凭据(`ANTHROPIC_*`、`ATTACORE_DAEMON_TOKEN`),
   `allowlist` 模式只留 `PATH` / `HOME` / 语言环境等几项加上 `allow` 列出的。Bash、
   Monitor、command hook 与 MCP stdio 服务都按 `sandbox.subprocess_env` 取策略——MCP
   服务取的是声明它的那份设置(daemon 按项目、scene 建连),再叠各服务自己的 `env`。这和沙箱无关——沙箱关着,模型一句 `printenv` 读到什么也由它决定。

沙箱是**轻量写限制,不是安全隔离**。已知逃逸路径写在
`crates/core/src/interface/exec/local/sandbox.rs` 的模块头里,四条,都还没收口。

---

## 6. 写路径的管控

写文件走两道:

1. **路径请求本身**——未展开的 `~`、UNC 路径、shell 替换、归一化与显示不一致的名字。
   这些是关于"模型递过来的那个字符串"的。
2. **它解析成的目标**——三份名单加上项目边界与系统目录前缀:
   - 凭据(`.env*` / `id_rsa` / `.ssh` / `.aws` / `.netrc` …)
   - 不该被随手改的生成物(`.gitignore` / `Cargo.lock` / `package-lock.json`)
   - 引擎自己的配置(`settings.json` / `settings.local.json`,且在 `.atta` / `.claude` 下)

   最后一条**按"文件名 + 所在目录"匹配,不按目录**。`.atta` 同时是子代理 git worktree
   的落脚点,把整个目录设为拒写会否掉子代理被创建出来要做的每一次写入。

`sandbox.allow_write` 给拒绝规则开例外,`sandbox.deny_write` 加自己的名字,
与读侧的 `allow_read` / `deny_read` 对称。**例外只解除拒绝规则,不解除项目边界**——
放宽可写范围是 `additional_directories` 的事,两者分开,不然一个配置会悄悄干另一个的活。

---

## 7. 锁

daemon 实例锁与 team 目录锁共用 `base::process_identity::decide_stale_lock`。
判定依据是 **pid + 进程启动时间**,不是文件 mtime,也不是超时:

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

`~/.atta/` 下按**索引维度**分目录,不按产品名或场景分:

| 目录 | 索引键 | 内容 |
|---|---|---|
| `projects/<sanitized-cwd>/` | 项目 | `<session-id>.jsonl` transcript |
| `projects/_no-project/` | ——(保留) | 无项目会话的 transcript |
| `sessions/<session-id>/` | 会话 | 会话记忆、metadata、输入历史 |
| `memory/` | 全局 | 跨会话记忆(项目级的在 `<项目>/.atta/memory/`) |

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

引擎有 44 个扩展点,分三类:**契约** 30 个(换掉一个子系统)、**拦截** 8 个
(坐在某件事的路径上)、**注册** 6 个(往一个集合里贡献)。

用 Rust 实现它们不需要载体——直接 `impl` 那个 trait 交给 `runtime::agent::Builder`。
**载体**是给不重新编译引擎的人用的:把脚本或 WASM 组件挂到其中一部分点上。

完整清单、各自的触发时机与信任级别在 **`extension_points.md`**——那张表是从
`base::interface::catalog` 生成的,有测试保证它和代码一致。

**两个载体互斥,一个构建只带一个:**

```
cargo build -p daemon                                                    # QuickJS(默认)
cargo build -p daemon --no-default-features --features plugin-compile    # WASM
cargo build -p daemon --no-default-features                              # 两者皆无
```

同时带两个会被 `compile_error!` 拒绝。这不是洁癖:cargo 的 feature 并集让"两个都带"
是踩出来的而不是选出来的,而代价是双倍的扩展攻击面和二十兆字节。

怎么写脚本 / 插件见 `extending_quickjs.md` 与 `extending_wasm.md`。
