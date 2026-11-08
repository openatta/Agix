# 插件包

插件包是一个带 `plugin.toml` 的目录,打成 zip 安装。它本身不带要跑的代码运行时:包里
声明的东西——MCP 服务器、绑到扩展点上的 QuickJS 脚本、一个声明式场景、agent 类型——都由
引擎里已有的部分兑现。安装时,一切会到达模型的文本都摆到安装者面前。

包方案是 `daemon` 的 `plugin-packages` feature,默认开启。不开时 `plugin.*` 这些 RPC 返回
`PLUGINS_DISABLED`(`-32016`)。脚本怎么写见 `extending_quickjs.md`;引擎的全部扩展点见
`extension_points.md`。一个完整的包在 `examples/plugins/quickjs/`,开发流程见
`plugin_development.md`。

## `plugin.toml`

```toml
[plugin]
name = "github-tools"          # 必填
version = "1.2.0"              # 必填
api_version = "0.1"            # 必填;必须是这个构建实现了的版本
description = "GitHub tools"   # 会到达模型——安装时披露
author = ""
homepage = ""

# ── 脚本 ──
# 跑它的是进程里已有的 QuickJS 载体;包只发 JavaScript 并指名一个扩展点。
[[script]]
point = "tool.result"                   # 扩展点 id
entry = "scripts/annotate.js:onResult"  # <文件>:<函数>,文件相对插件根目录
timeout_ms = 100                        # 可选;默认取载体自己的预算
calls_per_turn = 1000                   # 可选;同上

# ── MCP 服务器 ──
[[mcp]]
name = "github"
kind = "native"                # native 必须给 `config`
config = "mcp/github.json"

[[mcp]]
name = "pr-helper"
kind = "dsh"                   # dsh 必须给 `entry`,由 agix-dsh-bridge 进程代为说 MCP
entry = "dist/index.js"
env = ["GITHUB_TOKEN"]         # 只透传这里点名的环境变量

# ── 插件自己拥有的场景 ──
# 一个声明式场景目录(scene.json + prompt.md),格式见 extension_points.md「声明式场景」;
# scene.json 的 id 必须是 "plugin:<name>"。
[scene]
dir = "scene"

# ── agent 类型 ──
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

# ── 写给宿主的段 ──
# `x-` 开头的顶级段:引擎不读、不执行、不警告;宿主自己约定它的形状。
[[x-ui]]
panel = "ui/panel.js"
```

`api_version` 没有默认值。漏掉它解析不过;写了这个构建没实现的版本会被拒,消息里给出
支持的版本集合。

manifest 里出现引擎不认识的顶级段,不拒,但打一条 WARN 说它"什么也不贡献"——`[[scripts]]`
这种拼错,那是唯一的信号。有两类段是**拒装**的,因为它们是写给已经移除的东西的:
`[[wasm]]`(引擎不跑 WebAssembly 组件)和 `[scene.own]`(改用 `[scene] dir`)。

校验规则:

- `[[script]]` 的 `entry` 必须是 `<文件>:<函数>`,文件必须在包里面。绝对路径、`..`、
  解析后指到包外的符号链接,都会被拒。
- `[scene] dir` 同样必须在包里面。目录里的 `scene.json` 按声明式场景的规则从严校验,
  校验不过的包**装不上**,而不是装上之后少一个场景。
- **哪些点能绑脚本由载体说了算**——`script_host::bindings::BINDABLE_POINTS`,
  `extending_quickjs.md` 里有。
- **来自包的脚本绑定逐条降级。** 一条兑现不了的绑定只让它自己消失,原因进 `plugin.list`
  的 `script_faults`。运维自己写在 `settings.json` 里的 `scripts` 仍是全有或全无。

## 打包与安装

压缩包就是插件目录的一个普通 zip,`plugin.toml` 在根上;`tests/runner/src/plugin_fixture.rs`
是完整的打包步骤,测试就用它打包 `examples/plugins/quickjs/`。

```bash
cd my-plugin && zip -r ../my-plugin.zip . && shasum -a 256 ../my-plugin.zip
```

```json
{"jsonrpc":"2.0","id":1,"method":"plugin.install",
 "params":{"name":"github-tools","version":"1.2.0",
           "download_url":"file:///abs/path/my-plugin.zip",
           "checksum":"<sha256 hex>",
           "scope":"global"}}
```

- `download_url` 接受 `https://`/`http://` 和 `file://`。**网络来源必须带 checksum**,没有就
  在下载前拒掉;`file://` 可以不带。
- `scope` 是 `"global"`(默认)或 `"scene"`,决定落进哪一层的 cache。
- 解压后立刻按发现时的方式把 manifest 读回来;读不回来(包括上面那两类拒装的段)就整个
  删掉并报错。
- zip 里带 `..` 或绝对路径的条目跳过,不解压。

其余的生命周期:`plugin.list`(每个已安装的插件连同启用状态与 `root`,包括被禁用的)、
`plugin.enable` / `plugin.disable`(按层持久化在 `enabled.json`;scene 层压过 global,没设过
的算启用)、`plugin.uninstall`(`version` 可选,不给就全删)、`plugin.reload`。

插件的场景装上就是 active,不需要 `scene.activate`;进入它仍然要显式的
`session.create {"scene": "plugin:<name>"}`。

## 它落在哪

```
~/.agix/plugins/cache/<name>/<version>/                   # global 层
~/.agix/scenes/<scope>/plugins/cache/<name>/<version>/    # scene 层,同名时压过 global
```

同一个名字装了多个版本时,semver 最高的赢。加载不了的 manifest 带一条警告被跳过,其他
插件照常加载。

## 安装期披露

`plugin.install` 返回一个 `disclosure`,安装器应当把它摆到安装者面前:

```json
{"plugin":"github-tools","version":"1.2.0",
 "capabilities":["run its own JavaScript at the `tool.result` extension point"],
 "scene":"plugin:github-tools",
 "mcp_servers":["github (native)","pr-helper (dsh)"],
 "model_visible":[{"origin":"plugin description","text":"GitHub tools"},
                  {"origin":"agent `pr-reviewer` system prompt","text":"…"}],
 "inert":false}
```

`[[script]]` 列在 `capabilities` 里:它是宿主进程里的代码,跑在一个能改写模型读到什么的
点上,没有东西把它和宿主隔开。

`model_visible` 列出每一段会一字不差到达模型的文本——插件描述、agent 描述与提示词、场景
描述与提示词——每段标明来处。"文本到达模型"是隔离解决不了的,唯一的防线是有人去读。

两个上限是拒绝,不是警告:单行描述超过 **500 字符**;提示词超过 **40 000 字符**。

`inert: true` 表示这个包不声明能力、没有场景、没有 MCP 服务器、没有任何对模型可见的文本。

## 信任

- 包里的脚本以 `BlockOrigin::Plugin` 的身份运行,在提示词那几个点上"只能新增";它们的
  每一次工具调用照常过权限规则。QuickJS 的限额限的是开销,不是恶意代码,所以包里的脚本
  能做什么由这份身份决定,而不是由虚拟机决定。
- 包声明的 agent 类型带 `AgentTypeSource::Plugin`,它对 `permission_mode` / `max_turns` 的
  覆盖被钳制——网上来的定义不能给子代理比会话本身更多。
- `settings.json` 的 `plugins` 段是部署级开关:`{"plugins": {"enabled": false}}` 一个都不
  加载;非空的 `{"plugins": {"allow": ["a", "b"]}}` 是白名单。完全不要插件的部署优先用
  构建 flag,那样没有东西可以再打开。
- 插件名安装时对照黑名单检查,比较前先把形似 ASCII 的西里尔、希腊字母折回原字母。
