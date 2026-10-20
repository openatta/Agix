# 用 WebAssembly 插件扩展 AttaCore

一个插件就是一个目录,里面有一份 `plugin.toml`,通常还有一个或多个 WebAssembly
组件(component)。它从压缩包安装,跑在一个沙箱里——这个沙箱能够到哪里,它在任何人
安装之前就得先写下来——卸载时也不用碰二进制。

这是贵的那一档。如果你要的只是在轮次的某一个点上插几行运维方自己的代码,那是 QuickJS
载体(`docs/extending_quickjs.md`)的活:它大约花一兆,而这里要花二十兆,而且它不需要
打包。当你要加的东西是一个**包**的时候才用插件——别人的代码,分发出去,带版本,按名字
安装、按名字卸载。

所有扩展点的总揽,以及其中哪些是插件够得着的,在 `docs/extension_points.md`。

---

## 在别的之前:包方案和载体是两件事

**读一个包**——解析 manifest、取包、校验 checksum、解压、版本化缓存、安装期披露、
启停——不需要任何运行时。**跑一个包的 WebAssembly 组件**需要 wasmtime,二十兆。

这两件事是两个 feature:

- **`plugin-packages`**(默认开启):包方案。`plugin.*` 全套 RPC 在这个 feature 上,
  包声明的 `[[mcp]]` 服务器和 `[[script]]` 脚本也在这里被兑现。它和谁都不互斥。
- **`plugins`**:WASM 载体,把组件跑起来的那部分。它和 `scripts` 互斥——
  `daemon/src/lib.rs` 碰上同时带两个的构建会 `compile_error!` 直接拒掉,而不是
  默不作声地接受,因为 cargo 的 feature 合并会让"两个都带"成为一个人误打误撞就
  走到的地方。

```bash
cargo build -p daemon                                                   # QuickJS + 包方案(默认)
cargo build -p daemon --no-default-features --features plugin-compile   # 包 + WASM,编译器在进程内
cargo build -p daemon --no-default-features --features plugins          # 包 + WASM,不链接编译器
cargo build -p daemon --no-default-features --features plugin-packages  # 只有包方案,没有任何载体
cargo build -p daemon --no-default-features                             # 什么都不带
```

`--no-default-features` 对带 `plugins` 的构建是必须的:单独给 `--features plugins`,
默认的 `scripts` 会被并回来,守卫会拒掉这个构建。

**默认构建装得上带 `[[wasm]]` 的包**,只是不跑它的组件——安装响应里的 disclosure
会把 `wasm.components` 报成实际数量、`wasm.runnable` 报成 `false`。这比"整个包装不上"
诚实:一个包的组件跑不了,不该连它的 MCP 服务器和脚本一起没了。

第二个 flag 决定的是**组件在哪里被编译**:

- **`plugin-compile`** 把 Cranelift 链进 daemon。安装插件时,它的组件在进程内编译。
- 单独的 **`plugins`** 不链接任何 WebAssembly 编译器。那个构建里根本没有
  `Component::new`,所以 daemon 只能加载别人产出的产物——安装时它 shell 出去调
  `atta-plugin-compile` 这个二进制,从正在运行的可执行文件往上找;加载时缓存没命中
  就是拒绝,而不是悄悄重编一遍。载体三分之二的体积是编译器,所以这是给那种"要跑插件、
  但不想让服务进程里带一个代码生成器"的部署用的构建。

  **组件数为 0 的包不走编译。** 一个纯脚本 / 纯 MCP 的包在这个构建上照样装得上,
  不必再拖一个编译器进来。

`tests/scripts/locked_build.sh` 拿真实的依赖图把这些逐条验过:两个载体一起上会失败;
什么都不带的构建 `cargo tree` 里没有任何插件机器;默认构建里有 `plugin` 而没有
wasmtime;带 `plugins` 而不带 `plugin-compile` 的构建不链接 Cranelift。

一个跑着的 daemon 会自己回答关于自己的这个问题——`daemon.doctor` 把 `plugins.status`
报成 `compiled-out`、`packages-only`、`enabled` 或 `disabled-by-policy`。包方案都不在时,
`plugin.*` 这些 RPC 返回 `PLUGINS_DISABLED`(`-32016`),而不是一个空列表。

---

## 从零开始:一个能跑的插件

仓库里带了这么一个插件的组件那一半,在 `tests/fixtures/wasm_echo_plugin/`——故意做得很小,
`crates/wasm-host/tests/component_roundtrip.rs` 构建并折腾的就是它。下面这些就是那个
fixture 加一份 manifest。

### 1. 你的组件要实现的 world

契约就是一个 WIT world,`crates/wasm-host/wit/plugin.wit`,发布为 `atta:plugin@0.1.0`:

```wit
world plugin {
  import host;                                    // log, progress, now-ms, http-request, secret, kv-get, kv-set
  export tools;                                   // list-tools, call-tool
  export events;                                  // on-event
  export init: func(config-json: string) -> result<_, string>;
}
```

`tools` 和 `events` 都是这个 world 必须导出的。不参与事件的组件照样要导出 `on-event`,
答 `proceed`。

### 2. 组件

一个 Rust guest,放在 workspace 之外(它编译到 `wasm32-wasip2`,而一个只能编到别的
target 的成员会让 `cargo build --workspace` 挂掉):

```toml
# Cargo.toml
[package]
name = "wasm-echo-plugin"
version = "0.1.0"
edition = "2021"

[workspace]            # 故意让它自成一个 workspace

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.48"
```

```rust
// src/lib.rs
wit_bindgen::generate!({ path: "…/crates/wasm-host/wit", world: "plugin" });

use exports::atta::plugin::tools::{Guest as ToolsGuest, ToolDef, ToolOutput};
use exports::atta::plugin::events::{Guest as EventsGuest, HookDecision};

struct Component;

impl ToolsGuest for Component {
    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "echo".into(),
            description: "Echo the `text` argument back".into(),   // 每轮都发出去
            doc: Some("Returns `text` verbatim, plus a structured copy.".into()), // 按需取
            input_schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#.into(),
            read_only: true,
            concurrency_safe: true,
        }]
    }

    fn call_tool(_name: String, input_json: String, call_id: String) -> ToolOutput {
        atta::plugin::host::progress(&call_id, "echoing");   // 流式推给用户
        ToolOutput { content: input_json, structured: None, is_error: false }
    }
}

impl EventsGuest for Component {
    fn on_event(_event: String, _payload_json: String) -> HookDecision {
        HookDecision::Proceed
    }
}

impl Guest for Component {
    fn init(_config_json: String) -> Result<(), String> { Ok(()) }
}

export!(Component);
```

构建它:

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/wasm_echo_plugin.wasm
```

### 3. manifest

```toml
# plugin.toml,放在插件目录的根上
[plugin]
name = "echo-plugin"
version = "1.0.0"
api_version = "0.1"
description = "Echoes things back"

[[wasm]]
component = "echo.wasm"
```

`api_version` 没有默认值。漏掉它的 manifest 解析不过;写了一个这个构建没实现的版本,
会被拒掉,并在消息里给出支持的版本集合——WIT world、能力语义和事件白名单是一起动的,
没有"退一步用半个契约"这种事。

manifest 里出现引擎不认识的顶级段,不会被拒,但会打一条 WARN 说它"什么也不贡献"——
`[[scripts]]` 这种拼错,那是唯一会收到的信号。**`x-` 开头的顶级段是例外**:它们是写给
宿主看的,不是写给引擎的。

```toml
# 引擎不读、不执行、不警告;宿主自己约定它的形状
[[x-ui]]
panel = "ui/panel.js"
```

一个包可能被两个宿主装,所以这里定的是前缀而不是某一个段名(比如 `[[host]]`),否则
两个宿主要抢同一个名字。引擎对这些段的承诺只有一句:**不碰,也不误报**——没有读取
API,没有校验,没有转发。

### 4. 打包与安装

插件压缩包就是插件目录的一个普通 zip,`plugin.toml` 在压缩包根上。布局上没有任何特别
之处;`tests/runner/src/plugin_fixture.rs` 就是完整的打包步骤。

```bash
cd my-plugin && zip -r ../my-plugin.zip . && shasum -a 256 ../my-plugin.zip
```

通过 daemon 的 JSON-RPC socket 安装:

```json
{"jsonrpc":"2.0","id":1,"method":"plugin.install",
 "params":{"name":"echo-plugin","version":"1.0.0",
           "download_url":"file:///abs/path/my-plugin.zip",
           "checksum":"<sha256 hex>",
           "scope":"global"}}
```

- `download_url` 接受 `https://`/`http://` 和 `file://`。**网络来源必须带 checksum**,
  没有就在下载之前先拒掉;`file://` 是本地旁路安装,可以不带。
- `scope` 是 `"global"`(默认)或 `"scene"`,决定插件落进哪一层的 cache。
- 安装会先解压,然后**立刻编译每一个声明的组件**。有组件编不过就整个安装失败,插件被
  删掉——趁用户还站在旁边的时候发现,比会话跑到一半才发现强。
- zip 里带 `..` 或绝对路径的条目会被跳过,不解压。

响应里带 `success`、`message`,还有一个 `disclosure` 对象——见[安装期披露](#安装期披露)。

生命周期的其余部分:`plugin.list`(每一个已安装插件连同它的启用状态,包括被禁用的),
`plugin.enable` / `plugin.disable`(按层持久化在 `enabled.json` 里;scene 层的设置压过
global,没设过的插件算启用),`plugin.uninstall`(`version` 可选,不给就全删),
`plugin.reload`。

### 5. 看它工作

插件的工具会进到每个会话的注册表里,不需要场景。但**提示词**只有插件自己的场景能塑造,
所以顺手也给它一个,后面看它工作时用得上:

```toml
# plugin.toml,接着上面
[scene.own]
name = "Echo"
prompt = "scene/prompt.md"
```

重装,然后在这个场景里建一个会话:

```json
{"jsonrpc":"2.0","id":2,"method":"session.create","params":{"scene":"plugin:echo-plugin"}}
```

`scene.list` 里 `plugin:echo-plugin` 显示为 active,会话的工具注册表里现在有
`plugin__echo-plugin__echo`——它是延后暴露的(deferred),所以模型是靠 `ToolSearch`
找到它,而不是在每次请求的工具数组里看到它。

### 6. 它落在哪

```
~/.atta/plugins/cache/<name>/<version>/                   # global 层
~/.atta/scenes/<scope>/plugins/cache/<name>/<version>/    # scene 层,同名时压过 global
    plugin.toml
    echo.wasm
    config.json          # 可选,用户提供(见 [plugin.config])
    .aot/<hash>.cwasm    # 编译产物,按组件字节内容寻址
```

同一个插件名下,scene 层压过 global 层;同一个名字装了好几个版本时,semver 最高的赢。
加载不了的 manifest 会带一条警告被跳过,其他插件照常加载。

---

## `plugin.toml`

下面每一节都是加载器真的会读的。别的顶层小节不会让加载失败——一个针对更新版本 Core
写的包不该在旧 Core 上装不上——但会带着小节名打一条 warning。`[[scripts]]` 拼成复数
的那种笔误,只有这条日志会告诉你。

```toml
[plugin]
name = "github-tools"          # 必填
version = "1.2.0"              # 必填
api_version = "0.1"            # 必填;必须是这个构建实现了的版本
description = "GitHub tools"   # 会到达模型——安装时披露
author = ""
homepage = ""

[plugin.config]
schema = "config.schema.json"  # JSON Schema,init 之前拿它校验 config.json

# ── WebAssembly 载荷 ──
[[wasm]]
component = "gh.wasm"                     # 相对插件根目录
tools = ["diff"]                          # 只在安装时展示
events = ["PreToolUse", "PostToolUse"]    # 必须在可订阅白名单里

[wasm.capabilities]
fs_read  = ["${workspace}/src"]
fs_write = ["${plugin}/scratch"]
net      = ["api.github.com"]
env      = ["GITHUB_TOKEN"]
max_memory_mb = 128            # 默认 64
timeout_ms    = 5000           # 默认 30000

# ── 脚本载荷 ──
#
# 载体是宿主的,不是包的:包发一份 JavaScript 并指名一个扩展点,跑它的是进程里
# 已经有的那个引擎。所以这一节在默认构建上就能兑现,不需要 WASM。
[[script]]
point = "tool.result"                 # 扩展点 id
entry = "scripts/annotate.js:onResult"  # <文件>:<导出函数>,文件相对插件根目录
timeout_ms = 100               # 可选;默认取载体自己的预算
calls_per_turn = 1000          # 可选;同上

# ── MCP 载荷 ──
[[mcp]]
name = "github"
kind = "native"                # native 必须给 `config`
config = "mcp/github.json"

[[mcp]]
name = "pr-helper"
kind = "dsh"                   # dsh 必须给 `entry`
entry = "dist/index.js"
env = ["GITHUB_TOKEN"]

# ── 插件自己拥有的场景 ──
[scene.own]
name = "GitHub workflow"
description = "PR review and issue triage"
prompt = "scene/prompt.md"     # 必填;markdown 系统提示词
reminder = "scene/reminder.md" # 可选;每轮的提醒
tools = ["Read", "Grep"]
disallowed_tools = ["Bash"]
deferred_tools = []

[scene.own.budget]
compact_threshold = 120000     # 默认 150000
compact_keep_recent = 20       # 默认 20
max_api_calls_per_turn = 40    # 默认不设上限

# ── 插件声明的 agent 类型 ──
[[agent]]
name = "pr-reviewer"
description = "Reviews PRs"    # 会到达模型——披露
prompt = "agents/reviewer.md"  # 会到达模型——披露
allowed_tools = ["Read"]
disallowed_tools = []
model = "claude-opus-5"
permission_mode = "plan"
effort = "high"
max_turns = 30
scene = "plugin:github-tools"  # 让这个 agent 的子代理跑在插件的场景里
```

有两条校验规则值得在撞上之前就知道:

- **最多只有一个 `[[wasm]]` 组件可以声明 `events`。** 宿主是**按名字**把事件解析到插件
  的,所以第二个订阅的组件会让一个名字有两个可能的答案,其中一个订阅就会被悄悄忽略。
  多个组件没问题,只要只有一个订阅。
- `[[wasm]]` 下的 `tools = [...]` 是写给安装器看的文档。运行时以组件自己的 `list-tools`
  为准——真正被注册的是那个。
- `[[script]]` 的 `entry` 必须是 `<文件>:<函数>`,文件必须落在包里面。绝对路径、
  `..`、以及解析后指到包外面的符号链接,都会被拒。

`[[script]]` 还有两条运行期的规矩:

- **哪些点能绑脚本由载体说了算**,不由 manifest 说了算——那份清单是
  `script_host::bindings::BINDABLE_POINTS`,`docs/extending_quickjs.md` 里有。
  manifest 层不复述它,复述出来的第二份答案只会过期。
- **来自包的绑定逐条降级。** 一条兑现不了的绑定只让它自己消失,原因记进那个包的
  状态,`plugin.list` 的 `script_faults` 里能看到。运维自己写在 `settings.json` 里的
  `scripts` 仍然是全有或全无——那是一份文档、一个作者,半份是没人写过的;而由若干
  已安装的包拼出来的集合没有这么一个作者,一个坏包不该悄悄拿掉其它所有包的贡献。

### 可订阅的事件

```
PreToolUse   PostToolUse   PostToolUseFailure
PermissionRequested   SessionStart   SessionEnd
```

这是引擎那三十个 hook 事件里刻意挑出的一个子集。两条规则决定一个事件能不能进来:它得是
低频的(每次调用都要新建一个 WASM store),而且它的载荷得小到能按值穿过沙箱边界。写别的
名字是 manifest 错误,消息里会列出允许的那些。

订阅了的组件会答三件事之一,而宿主把它收窄到"一个下载来的包可以说的话":

| 答案 | 会发生什么 |
|---|---|
| `proceed` | 什么也不发生 |
| `block(reason)` | 事件被拒,理由展示给用户 |
| `add-context(text)` | 一条可归属的备注加进对话 |

每个订阅都拿组件自己的 `timeout_ms` 当期限来注册——hook 跑在一个正等着它的轮次里,所以
它拿不到比自己要的更多的时间。组件没加载成功的 hook 根本不会被注册,所以它认领过的事件
不会白白付一次分发。

这里刻意没有"改写输入"这一档。引擎自己的 `HookResponse` 可以带 `updated_input`,插件这条
路够不着它:拒掉一次工具调用,和悄悄改掉它要做的事,是两种不同的权力,而一个下载来的包
只拿得到第一种。

### 用户配置

如果 `[plugin.config]` 的 `schema` 设了,插件安装目录里的 `config.json` 会在组件加载之前
拿这份 JSON Schema 校验,而且**所有**违规一次性全报出来。没有 `config.json` 的插件拿到
`{}`——没有不等于写错了。校验过的 JSON 接着交给组件的 `init`,最后一句话归插件说:`init`
返回 `Err` 就意味着这个插件不加载。schema 本身写坏了是错误,不是悄悄变成"不校验"。

---

## 能力模型

**什么都不会因为没写而被授予。** 一个不声明任何能力(capability)的组件只能算,别的都
做不了:没有文件,没有网络,没有环境变量。插件想要什么就得写下来,而写下来的东西就是
安装器给用户看的东西。

这张表以及所有基于它的判定都住在 `base::interface::capabilities` 里,在载体之上,与其他
每一个载体共享。载体把自己的 manifest 转成内核的 `CapabilityDeclaration` 然后去问;它不
负责回答。`daemon/tests/carrier_invariants.rs` 会在某个载体长出自己的 `allows_url` /
`allows_env` / `allows_read` / `allows_write` 时失败。

### `fs_read`、`fs_write`

路径**必须**锚在 `${workspace}`(daemon 的工作目录)或 `${plugin}`(插件的安装目录)上。
没锚的、或者绝对路径的,加载时就拒——`fs_read` 里一个光秃秃的 `/` 读起来就是把整台机器
交出去,而这恰恰是审阅的人一眼扫过去会漏掉的那一行。展开后的路径里任何位置出现 `..`,
同样拒。

展开只在加载时做一次,所以没有任何调用时的判断依赖于插件能在"检查"和"使用"之间改动的
状态。活下来的那些变成 WASI 的预打开目录(preopen),仅此而已——宿主接口里没有文件 API,
因为把路径检查用宿主函数再实现一遍,正是那些检查出微妙错误的方式。

在组件内部,一个预打开目录以它**路径的最后一段**出现:`/Users/me/secret/project` 在 guest
眼里是 `/project`。插件没有必要知道自己的 workspace 在这台机器的哪个位置。

一个能力指向的目录不存在,会让加载失败并把那个路径写进消息里,而不是等第一次碰文件时
莫名其妙地挂掉。

### `net`

精确的 host 匹配,在 `host.http-request` 里检查:

- host 大小写不敏感,端口不参与匹配
- **不是**后缀规则——`api.github.com` 不放行 `evil.api.github.com.attacker.test`
- userinfo 伪装不了 host:`https://allowed.example@evil.example/` 指向的是
  `evil.example`,拒
- 非 `http(s)` 的 URL 直接拒,不解析——`file:///etc/passwd` 根本到不了解析器

拒绝时报的是那个 **host**,绝不是整个 URL:这条消息会返回给 guest,而 guest 通常原样把它
当工具结果交出去,于是进了模型的上下文和会话记录。

### `env`

精确、区分大小写的名字,只能通过 `host.secret` 读。`GITHUB_TOKEN` 不放行 `github_token`,
也不放行 `GITHUB_TOKEN_2`。没声明的 key 返回 `none`,并记一条"插件问过"的日志。

### `max_memory_mb`、`timeout_ms`

由 store 的资源限制器和调用期限来执行。默认是 64 MB 和 30 秒。

### 宿主借回去什么

`log`、`progress`(为正在进行的这次调用流式推给用户)、`now-ms`、`http-request`、
`secret`、`kv-get`、`kv-set`。键值命名空间是每插件一份,在宿主的内存里;它之所以存在,
是因为组件**每次调用都拿到一个全新的 store**,因此自己没有任何记忆。宿主看得见它、能清空
它、能随插件一起把它丢掉——这正是重点。它不跨 daemon 重启,也不跨插件 reload。

---

## 安装期披露

`plugin.install` 返回一个 `disclosure`(披露)对象,安装器应该把它摆到正在安装的那个人
面前:

```json
{"plugin":"github-tools","version":"1.2.0",
 "capabilities":["read files under ${workspace}/src","make network requests to api.github.com",
                 "run its own JavaScript at the `tool.result` extension point"],
 "events":["PreToolUse"],
 "scene":"plugin:github-tools",
 "mcp_servers":["github (native)"],
 "model_visible":[{"origin":"plugin description","text":"GitHub tools"},
                  {"origin":"tool `diff` description","text":"…"},
                  {"origin":"tool `diff` guide","text":"…"},
                  {"origin":"agent `pr-reviewer` system prompt","text":"…"}],
 "wasm":{"components":1,"runnable":true},
 "inert":false}
```

`[[script]]` 出现在 `capabilities` 里,和文件、网络那些并排:它是宿主进程里的代码,
跑在一个能改写模型读到什么的点上,沙箱对它无话可说。

`wasm` 讲的是这个**构建**会不会跑这个包的组件。`components` 是包声明的组件数,
`runnable` 在没有 WASM 载体的构建上是 `false` —— 包照装,组件不跑,而不是整个包
装不上。

`model_visible` 之所以存在:沙箱管的是插件**执行**什么,对它**说**什么毫无办法。工具描述、
agent 描述、场景的系统提示词,都会一字不差地到达模型,而"文本到达模型"是这套隔离模型
唯一解决不了的攻击。唯一的防线是有人去读它,所以安装响应把这些全带上,每一段都标明它从
哪来。能力那几行按 manifest 写的原样展示,`${workspace}` 也照原样留着。

有两个上限是拒绝,不是警告——只会警告的上限,每一个自动安装器都会径直走过去:

- 单行描述超过 **500 字符**(这么长的标签要么是写错了,要么是想把指令塞进一个审阅者只会
  扫一眼的字段里)
- 提示词或长工具指南超过 **40 000 字符**

注意工具那部分文本只可能来自一个**已加载的组件**——manifest 里没有它——所以在没有 WASM
宿主的情况下产出的披露就是不列任何工具。`inert: true` 意味着这个插件不声明能力、不声明
事件、没有场景、没有 MCP server、没有组件,也没有任何对模型可见的文本:除了名字之外
没什么可审的。

披露刻意与载体无关。它讲的是一个扩展**说**了什么,所以 `crates/plugin/src/disclosure.rs`
里不提任何运行时,一旦它开始提,不变量测试就会失败。

---

## 插件能贡献什么

已安装插件呈给引擎的一切都走同一个 trait,`runtime::plugin_host::PluginHost`,宿主以
`Option` 持有它——正是这一点让整个子系统能被编译掉,而不必在引擎里到处撒 `#[cfg]`。

| 贡献 | 怎么到达一个会话 |
|---|---|
| **工具** | 从组件的 `list-tools` 适配成普通的引擎工具,名字是 `plugin__<plugin>__<tool>` |
| **MCP server** | 并进 daemon 已配置的 server 集合,key 为 `<plugin>-mcp-<name>` |
| **Hook 订阅** | 注册进会话的 `HookRunner`,背后是组件的 `on-event` |
| **一个场景** | 以 `plugin:<name>` 注册并激活;用 `session.create {"scene": "plugin:<name>"}` 进入 |
| **Agent 类型** | 通过 `Agent` 的 `subagent_type` 被发现 |

这张表里有两件事值得挑明。

**工具进的是每个会话,不只是插件自己那个场景。** `Builder::build` 把
`PluginHost::tools()` 里的每一个都注册进它建的会话——**包括跑在内建场景上的会话**。
一个装上的插件的工具,这个进程里的每个会话都看得见、都调得动。

这条路是后加的,而且是有意加的:插件可以只声明 `[[wasm]]` 而不声明 `[scene.own]`,
manifest 允许,而那样的插件在只走场景那条路的时候,工具哪儿都没注册。拥有场景的插件把
同一批 `Arc` 交两次——一次作为场景的 `extra_tools`,一次在这里——靠指针相等去重,同名
但不同实例才算冲突。

**这意味着装一个插件就是把它的工具交给整个进程,不是交给某一个场景。** 装之前那份
披露正是为此存在的。要把它挡在某个会话之外,靠的是权限规则(下面那条:插件工具永远
答 `Ask`),不是靠换一个场景。

**插件只能在自己拥有的场景里塑造行为。** 改写用户为别的原因选中的场景的提示词,是劫持;
拥有一个用户明确进入的场景,是插件在干自己的活。所以插件拿到自己的场景,永远拿不到别人
的。插件场景的 `max_api_calls_per_turn` 也**抬不高**真正的上限——轮次循环把预算解析成
`min(settings, scene)`,所以场景只可能收紧。

工具适配器有几个后果常常出乎人意料:

- 插件工具默认是**延后暴露**的。一个第三方 schema 出现在每次请求的工具数组里,是在为模型
  通常不看的东西付固定的 token;模型想用这个工具的时候,会用 `ToolSearch` 去取那份长的
  `doc`。`description` 才是每轮都发出去的那一行。
- `check_permissions` 永远答 **Ask**。插件自己断言这次调用没问题,不构成任何证据;组件
  给的 `read_only` / `concurrency_safe` 只用于调度。
- `plugin__<name>__<tool>` 这个形状照着 `mcp__<server>__<tool>` 来,权限规则也一样:
  写完整的工具名(`plugin__github-tools__diff`)只管这一个工具,写到插件为止
  (`plugin__github-tools`)覆盖它的全部工具——规则名后面紧跟着 `__` 才算前缀命中,
  所以 `plugin__github` 碰不到 `plugin__github-tools__diff`。
- 组件给的输入 schema 解析不了,照样注册;它对外宣称一个开放对象,因为拒绝等于让用户为
  作者的笔误买单。

插件**不**贡献 skill 和斜杠命令,也不能绑到脚本够得着的那些拦截点上(`prompt.assemble`、
`model.request` 以及其余)。`docs/extension_points.md` 里的「插件」列讲的是每个**契约**
开放了什么;WebAssembly 载体实际的表面就是上面那五行。

---

## 边界

### 沙箱

每进程一个 wasmtime `Engine`——它持有编译器和代码缓存,所以每插件一个只会把这份开销重复
付一遍,换不来任何隔离上的好处。**隔离来自 store,不来自 engine**:组件每一次调用都拿到
一个全新的 `Store`,而一个 store 拥有自己的线性内存、自己的表、自己的资源上限。一次 trap、
一个跑飞的循环、一次分配爆炸,收场是同一个——store 被丢掉,调用返回错误,engine 接着跑。

期限用 epoch 中断来执行,不用 fuel:"这东西是不是已经超出预算了"和"用户是不是按了
Ctrl-C",都是挂钟时间的问题。一个专用的 OS 线程每 10 ms 推进一次 epoch——定时任务不行,
因为"每个异步 worker 都忙着"恰恰就是一个跑飞的 guest 造出来的局面。epoch 让 guest 交出
执行权,于是一个不可中断的循环变成一个宿主可以丢掉的 future,超时和取消因此是同一个操作,
而不是两套机制配两种失败模式。

这换来什么,round-trip 测试拿一个真组件断言过:一个工具死循环的插件会在它自己声明的超时
上被停住,而对同一个插件的下一次调用成功;一个 trap 的工具是一个错误结果,下一次调用成功;
一个没声明的 host 从 guest 内部够不着,而且错误里会说清哪个能力本来可以放行它。

失败怎么呈现给调用方:

| 发生了什么 | 模型看到什么 |
|---|---|
| 超时 | 一个错误**结果**——是这个工具失败了,不是这一轮失败了 |
| trap | 一个错误结果,点名插件和工具 |
| 取消 | 引擎自己的 `Cancelled` 错误,绝不是一个会被模型读成答案的结果 |

### 健康熔断

每次调用都隔离,让"继续调一个坏掉的插件"变得**安全**,但没变得有用。一个每次调用都 trap
的组件,会把模型的每一次尝试都变成一个错误结果,而模型会一直试下去。所以连续 **3 次故障**
(`wasm_host::health::FAULT_LIMIT`)之后,这个插件被搁置:它导出的每一个工具都拒绝执行,
消息里点的是插件的名字,而不只是那个失败的工具。

是 3 次而不是 1 次,因为一次故障可能来自插件没处理好的某个输入,而一个坏参数不该让用户
赔上整个工具。一次成功就把连续计数清零,所以一个只在边角上失败的插件不会被罚。

**只有故障计数。** 超时或取消说的是这份工作或这个用户的事,不是插件本身健不健全——否则
一个确实做着慢活的插件,会因为被按预期使用而把自己禁掉。

故障记录住在一个以插件名为 key 的注册表里,所以它们活得比组件实例长——而组件实例在每一次
安装、卸载、启用、禁用时都会重建。实际上一个被搁置的插件在下一次 reload 时就回来了:
reload 会再问每个组件要一遍工具列表,而一次答得上来的 `list-tools` 就是一次成功,连续计数
被清零。所以在插件作者发布修复之后,`plugin.reload` 就是给它再来一次机会的方式。

这些记录以 `plugins.breakers` 这个健康检查上报(`degraded`,每插件的故障数在 `details`
里),这样一个本来会被载体无声做掉的决定——工具不再被提供,模型不再问,而装了它的人得不到
任何信号——就变成运维方可以处置的东西。

### 加载

插件最多 4 个并发加载,整个加载阶段有 30 秒预算。加载会跑插件的 `init`,所以它不只是 I/O。
daemon 在开始服务之前会等它;预算到期时还没加载完的插件会被丢掉并留一条警告,因为少了它
还能起来是可以恢复的,而起不来不是。一个插件在任何一步失败都会被丢掉,其他插件照常加载。

### 信任模型

- 能力是**声明 ∩ 用户规则**,永远不是并集。声明的能力是插件**可以去要**的东西;每一次调用
  仍然由用户的权限规则决定,而插件工具默认是 `Ask`。
- 插件声明的 agent 类型带 `AgentTypeSource::Plugin`,正是这个东西在钳制它对
  `permission_mode` / `max_turns` 的覆盖——一个从网上来的定义,不能给它的子代理比会话本身
  已经拥有的更多。
- `settings.json` 的 `plugins` 段是部署级的开关:`{"plugins": {"enabled": false}}` 一个都
  不加载;非空的 `{"plugins": {"allow": ["a", "b"]}}` 是一份白名单——不在上面的名字是被
  拒绝,而不只是没被提到。一个完全不想要插件的部署应该优先用构建 flag,那样根本没有东西
  可以再打开。
- 插件名在安装时会对照一份黑名单检查;配置了市场索引时,还会检查与官方名字之间的同形混淆。
- AOT 产物只以组件的内容哈希为 key,所以重新构建过的插件永远不会读回上一次构建的机器码。
  兼容性不是这个 key 的活:`Component::deserialize` 会校验产物自己的头,拒掉来自异种工具链
  的那一个,所以拿错产物得到的是 wasmtime 自己的消息,而不是一次无声的错过。在带编译器的
  构建里,一次拒绝会降级成重编;在只有运行时的构建里,这里就是加载停下的地方——而这正是
  拿掉编译器的全部意义。
