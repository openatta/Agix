# 插件开发

Agix 的插件只有两种:**编译进宿主的 Rust 插件**,和**跑在内嵌 QuickJS 里的脚本**。两种走同一个
接口 `runtime::plugin::Plugin`,可以在同一个会话里叠加。本文讲怎么写这两种插件。每个接口
在 `examples/plugins/` 下都有一个能跑的用法:

| 目录 | 是什么 |
|---|---|
| `examples/plugins/rust/` | `demo-rust`,一个 Rust 插件。`Registrar` 的 16 个注册方法全部用到,每个做一件小事 |
| `examples/plugins/quickjs/` | `demo-quickjs`,一个插件包。九个可绑定的点各绑一个脚本,另带一个 agent 类型和一个场景 |

这两个样例有测试跑着:`tests/live/tests/demo_plugins.rs` 用模型桩逐项断言,每次 push 都跑;
`tests/live/tests/live_plugins.rs` 让真实模型把它们用一遍(LIVE 层,见 `tests/README.md`)。

## 选哪一种

| | Rust 插件 | QuickJS 脚本 |
|---|---|---|
| 能做什么 | `Registrar` 的全部注册方法:工具、agent 类型、所有拦截点、提示词贡献、健康检查、每轮回调 | 九个点上的变换:改写、拒绝、过滤、加块。不能做任何 I/O |
| 信任 | 就是宿主自己的代码,不设边界 | 内存、时间、每轮调用次数都有限额;项目里的脚本能改写,包里的只能新增 |
| 怎么上线 | 宿主重新编译 | 改 `settings.json`,或者装一个插件包,不用编译 |
| 状态 | 随便持有 | 每次调用都是全新运行时,什么都不留 |

需要 I/O、需要状态、需要新工具,写 Rust 插件,或者把工具放进 MCP 服务器(插件包也能带 MCP,
见 `plugin_packages.md`)。只想在某个点上改一改看到的东西,写脚本。要跑不可信的代码,这两种都
不合适:引擎里没有隔离边界,需要的宿主在 Rust 接口上自己做(比如用 WASM)。

要替换一整个子系统(会话存储、模型后端、权限……)不走插件,走 `Builder` 上对应的 setter,
见 `extension_points.md`「契约」。

## Rust 插件

### 接口

```rust
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;                    // 一个会话里唯一
    fn layer(&self) -> Layer { Layer::Rust }   // 自己写的插件不用改
    fn register(&self, session: &PluginSession<'_>, r: &mut Registrar);
}

pub struct PluginSession<'a> {
    pub session_id: &'a str,
    pub project_root: &'a Path,
    pub scene: &'a str,
    pub depth: u32,      // 宿主建的会话是 0,子 Agent 是父会话 + 1
}
```

`register` 在**每个会话**构建时调一次,子 Agent 也算,会话重建时还会再调。所以按会话的
状态在 `register` 里创建:`demo-rust` 每次 `register` 都新建一个 `Activity`,交给要计数的
那几样东西,两个会话的计数互不干扰。

交给宿主:

```rust
// 直接嵌引擎
let builder = Builder::new().plugin(Arc::new(DemoPlugin::new()));

// 嵌 daemon
let pool = daemon::assemble::pool(&config, scene, daemon::Assembly {
    plugins: vec![Arc::new(DemoPlugin::new())],
    ..Default::default()
}).await?;
```

### `Registrar` 的 16 个方法

| 方法 | 扩展点 | `demo-rust` 里拿它做什么 | 文件 |
|---|---|---|---|
| `tool` | 工具 | `DemoStamp`:给一段文本算指纹 | `tool.rs` |
| `admit_tools` | 场景的工具白名单 | 让 `DemoStamp` 在 `mini`、`harnix` 这类有白名单的场景里也留下 | `lib.rs` |
| `agent_type` | agent 类型 | `demo-auditor`:只读的审计子 Agent | `lib.rs` |
| `tool_middleware` | `tool.around` | 涉及 `.secret` 文件的调用一律拒绝 | `policy.rs` |
| `tool_result` | `tool.result` | 把工具结果里的 `SECRET-…` 换成 `[redacted]` | `policy.rs` |
| `model_interceptor` | `model.request` / `model.message` | 给 `max_tokens` 封顶;数模型回了几次 | `policy.rs` |
| `retrieval_hook` | `memory.retrieval_hook` | 召回最多 5 条,`draft-` 开头的不召回 | `policy.rs` |
| `prompt_middleware` | `prompt.submit` | 把 `!stamp <文本>` 展开成一句完整请求 | `policy.rs` |
| `prompt_block` | `prompt.block` | 固定的说明块 `demo-rust.guide` | `prompt.rs` |
| `prompt_context` | `prompt.context` | 每轮重算的 `demo-rust.activity` | `prompt.rs` |
| `prompt_variable` | `prompt.variable` | `{{demo_rust_project}}`:项目目录名 | `prompt.rs` |
| `prompt_variable_from` | `prompt.variable` | `{{demo_rust_turn}}`:第几轮,来源记成插件 | `prompt.rs` |
| `assembly_hook` | `prompt.assemble`(同步) | 在末尾加一份各段的目录 | `prompt.rs` |
| `async_assembly_hook` | `prompt.assemble`(异步) | 每轮读 `.demo/notice.md`,以插件权限只追加 | `prompt.rs` |
| `health_check` | `health.check` | 报告这个会话的计数;通知文件读不出来时报 `degraded` | `activity.rs` |
| `on_turn_start` | 每轮开始 | 轮次计数 | `activity.rs` |

这些 trait 各自的契约(什么时候调、能改什么、失败了怎样)写在 `extension_points.md` 对应的
小节里,本文不重复。几条写插件时容易踩的:

- **工具结果里的失败**:`tool_middleware` 拒绝一次调用,返回 `Ok(ToolAnswer::failure(..))`,
  模型看到的是一个报错的工具结果。`Err` 的意思是"引擎没能执行这个工具",会取消同一批里的
  其余调用,不是拒绝该用的。
- **拦截范围要窄**:`demo-rust` 只看 `file_path`、`notebook_path`、`path`、`command` 这几个参数。
  最早的版本扫描全部参数,结果一个"去审计 keys.secret"的 `Agent` 调用被当成读密钥拦了下来。
- **两种组装钩子的权限不一样**:`assembly_hook` 带引擎自己的权限,可以改、删、挪任何块;
  `async_assembly_hook` 的权限由你传入。`demo-rust` 传的是 `Authority::plugin(..)` 加默认能力,
  只能追加。代码的权限给到够用就停。
- **变量在钩子之前展开**:所有块先展开 `{{name}}`,组装钩子才运行,所以钩子读到的就是模型会读到的
  文本。

### 子 Agent

会话派生的子 Agent(前台、后台、团队成员)会带着同一批插件重新构建,`register` 再调一次,
`session.depth` 加 1。哪些东西该跟过去,由插件自己按 `depth` 决定。`demo-rust` 的做法:

- 策略类的(工具拦截、结果改写、模型拦截、召回)**跟过去**。跟不过去的规则,模型一次 `Agent`
  调用就能绕开。
- 提示词类的和 `prompt_middleware` **不跟**(`depth > 0` 时直接返回)。子 Agent 有自己的场景
  和提示词,也没有用户往里输入。
- 工具**跟过去,但重新注册**:子 Agent 拿到的是它自己那次 `register` 给的新实例,不是父会话
  那份的拷贝。agent 类型的工具白名单对插件工具一样生效,`explore` 这类只读类型拿不到插件的
  写工具。

`agent_type` 的 `source`:编进宿主的插件用 `AgentTypeSource::Builtin`,它覆盖的
`max_turns` 照原样生效;`Plugin` 是给从网上装来的包用的,`max_turns` 会被钳到父会话的上限
以内,`permission_mode` 也只能比父会话严;`Builtin` 与磁盘上的 `agents/*.md` 可以放宽。优先级是:内建和包 <
Rust 插件 < 磁盘上的 `agents/*.md`。

### 场景的工具白名单

场景的 `tools()` 非空(`mini`、`harnix`,以及写了 `"tools"` 的声明式场景)时,会话里只留白名单
里的工具,插件用 `tool` 登记的也一样会被拿掉。要让自己的工具在这样的场景里也留下,在同一次
`register` 里调 `admit_tools`:

```rust
r.tool(Arc::new(tool::StampTool::new(activity.clone())))
    .admit_tools(tool::NAME);          // 精确名
// 一组按前缀起名的工具:
r.admit_tools("app__*");               // 以 * 结尾是前缀
```

- pattern 只和**这个插件自己** `tool` 登记的工具比,碰不到内建工具,也碰不到别的插件的工具。
  写 `admit_tools("Bash")` 什么也不放行。
- 场景的 `disallowed_tools` 仍然优先:场景作者写了 `"disallowed_tools": ["app__*"]`,放行的也留不下。
- 子 Agent 照旧按 agent 类型的 `allowed_tools` 收窄;子会话自己那次 `register` 里的 `admit_tools`
  对子会话的场景生效。
- 白名单为空的场景(如 `code`)本来就留着插件工具,调不调都一样。

被场景拿掉的工具记一条 `debug` 日志(`tool removed from the session`,带工具名、场景和插件名),
看到"登记了但模型说没有这个工具"时先打开 `RUST_LOG=runtime=debug` 看这一条。

### 名字

插件名、工具名、agent 类型名在一个会话里只能用一次。提示词块名和变量名在同一层里只能用一次,
宿主自己的 `prompt_registry` 算 Rust 层。撞了名,会话构建失败,错误信息会点出是哪两个插件。
Rust 层和 QuickJS 层之间同名不算冲突,按「叠加」那节的规则处理。

### 宿主看得见什么

- `health_check` 注册的检查出现在 daemon 的 `session.get` 结果的 `health` 里(只有注册了检查的
  会话才有这个键),直接嵌引擎时从 `Agent::health()` 取。
- 工具来源:`Tool::spec()` 里的 `source` 写 `plugin:<name>`(`examples/plugins/rust/src/tool.rs`
  就是这么写的;怎么实现 `spec()` 见 `extension_points.md`「实现一个工具」),
  `session.inspectContext` 的 `tools[].source` 里看得到。

### 测试

- 纯逻辑写单元测试,`examples/plugins/rust/src/policy.rs` 里那几个就是。
- 要跑真实的轮次:`tests/runner` 的 `script_session::Session` 驱动一个真 `Agent`,模型那端按
  脚本回答,`Session::rust(..)` 把插件交给 `Builder`。`tests/runner/tests/rust_and_quickjs_layering.rs`
  是例子。
- 要连 daemon 一起测:`tests/live` 的 `DaemonOptions::host(Host { plugins, .. })` 在本进程起一个
  带插件的 daemon,见 `tests/live/tests/demo_plugins.rs`。

## QuickJS 插件

脚本的写法、九个点各自收什么返回什么,在 `extending_quickjs.md`。这里讲怎么把它们组织成插件。

### 两种来源

| | 写在 `settings.json` 的 `scripts` 里 | 放进插件包的 `[[script]]` |
|---|---|---|
| 谁写的 | 运维者自己 | 装来的包 |
| 在 `prompt.assemble` 上 | 能改、删、挪 | 只能新增 |
| 一条绑定有问题 | 整段都不绑 | 只有这一条不生效 |
| 和 Rust 插件撞块名或变量名 | 覆盖 Rust 的 | 被丢掉 |

两种在引擎里都包成一个 `script_host::ScriptPlugin`,层是 `Layer::QuickJs`。

### `demo-quickjs`

```
examples/plugins/quickjs/
├── plugin.toml
├── scripts/            # 九个点各一个文件
├── agents/reviewer.md  # [[agent]] 的提示词
└── scene/              # [scene] dir:scene.json + prompt.md
```

| 点 | 脚本 | 做什么 |
|---|---|---|
| `prompt.block` | `prompt_block.js` | 固定块 `demo-js.house-rules`,里面用到 `{{demo_js_os}}` |
| `prompt.variable` | `prompt_variable.js` | `{{demo_js_os}}`:操作系统 |
| `prompt.context` | `prompt_context.js` | 每轮重算的 `demo-js.today`:日期与可用工具数 |
| `prompt.assemble` | `prompt_assemble.js` | 在末尾追加一份检查清单(包里的脚本只能追加) |
| `tool.around` | `tool_around.js` | 拒绝手写 `generated/` 下的文件;`Bash` 限时两分钟 |
| `tool.result` | `tool_result.js` | 超过 6000 字符的结果截断,并告诉模型截掉了多少 |
| `memory.retrieval_hook` | `memory_retrieval.js` | 召回最多 3 条,`scratch-` 开头的不召回 |
| `model.request` | `model_request.js` | 这个项目离线工作,`WebSearch`、`WebFetch` 不给模型 |
| `model.message` | `model_message.js` | 把连续的空行折成一行 |

`plugin.toml` 里每条绑定都把 `timeout_ms` 调到 1000。脚本本身远不到 1 毫秒,但默认的 100 毫秒是
墙钟时间,机器忙的时候可能超时。

### 打包与安装

```sh
cd examples/plugins/quickjs && zip -r ../demo-quickjs.zip . && shasum -a 256 ../demo-quickjs.zip
```

```json
{"jsonrpc":"2.0","id":1,"method":"plugin.install",
 "params":{"name":"demo-quickjs","version":"1.0.0",
           "download_url":"file:///abs/path/demo-quickjs.zip","checksum":"<sha256>"}}
```

返回的 `disclosure` 列出九条脚本能力,以及每一段会到达模型的文本(agent 提示词、场景提示词、
描述)。之后新建的会话就带上这些脚本。场景要显式进入:
`session.create {"scene": "plugin:demo-quickjs"}`。manifest 的完整格式、校验规则和信任模型在
`plugin_packages.md`。

### 包里带 skill

教模型何时、怎样用包里工具的那几段说明,写成 skill 跟包一起发,不要让宿主拷进实例的 skill
目录——那样停用、卸载时它们不会跟着走,也不在披露里。在 `plugin.toml` 里写

```toml
[skills]
dir = "skills"
```

再把 skill 放成 `skills/<名字>/SKILL.md`,写法与磁盘上的 skill 相同,但校验严得多:
frontmatter 拼错一个键、`name` 与目录名不一致、少了 `description`,整个包都装不上,
错误里写明是哪个 skill、哪一项。动手前先在本地装一遍,看 `plugin.install` 的报错和 `disclosure`
里列出的正文。

裸名,不加命名空间:用户磁盘上有同名 skill 时用的是用户那份,包照常装上,`plugin.list` 的
`skill_conflicts` 会报出来。所以给 skill 取一个不容易与别人撞的名字。`allowed-tools` 只写
`Tool(内容)` 形式、且内容真正收窄的才生效——路径在项目之内、命令以一个完整的程序名开头、URL
钉住一个可注册的域名(`Bash(cargo test:*)`、`Write(src/**)` 生效,`Bash(*)`、`Bash(b*)`、
`Write(../**)`、`WebFetch(https://*.github.io/**)` 不生效)。完整规则见 `plugin_packages.md`「包里的 skill」。

### 调试

脚本失败了什么都不改,所以"没生效"在外面看起来和"没绑上"一样。查 `session.get` 的 `scripts`:
每个点调了几次、`applied` / `no_change` / `failed` / `refused` 各多少,最近 20 条带原因。
`demo_plugins.rs` 和 `live_plugins.rs` 都断言 `failed == 0`。

## 叠在一起

同一个点上两层都有贡献时,顺序由层决定,跟交给 `Builder` 的先后无关:

- 串行的点(`tool.result`、`model.*`、召回、`prompt.submit`……):Rust 先,QuickJS 后,
  脚本读到的是 Rust 处理过的结果。
- `tool.around`:QuickJS 在最外面,Rust 在里面。`demo-quickjs` 拒掉的写 `generated/` 的调用,
  根本到不了 `demo-rust` 的那一环。
- 同名的块、变量:项目里的脚本覆盖 Rust;包里的脚本那份被丢掉,打一条警告。

完整规则在 `extension_points.md`「插件」与「叠加顺序」。

## 稳定性

`Plugin`、`PluginSession`、`Registrar` 的方法,九个脚本点的 JSON 形状,脚本绑定的字段,以及 `plugin.toml` 的写法(`api_version = "0.1"`),原则上只做增量,不改已有的含义;但 `api_version` 还是 `0.1`,这条原则不是保证。破坏兼容的改动不会静默发生,每次都列在发布对应的迁移文档(`migrating_to_0.3.md`)里,升级前先读那一节。

0.3.10 就有这样的改动:`[[mcp]] kind = "dsh"` 被拒(引擎不再带 DSH 桥);披露里超限的文本从警告变成拒装;`[[agent]] prompt` 与 `[[mcp]] config` 必须在包内,与 `[[script]]` 一样;`[[mcp]] config` 里写 `oauth_provider` 的被拒,改用 `oauth`;`plugin.install` 的 `name`、`version` 只收 1–128 个 ASCII 字母、数字与 `.`、`_`、`-`、`+`。0.3.11 新增 `[skills]` 段,只增不改:没写它的包不受影响。

各个点背后的 trait(`ToolMiddleware`、`ModelInterceptor`……)按 `extension_points.md` 里各自的说明。

## 自己跑一遍

```sh
cargo test -p demo-rust-plugin                                    # 样例自己的单元测试
cargo test -p live --test integration demo_plugins::              # 模型桩,两个样例一起
cargo test -p live --features real-model --test real_model \
  live_plugins:: -- --nocapture                                    # 真实模型,要 .env
```
