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
  见 `deployment.md`。
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
