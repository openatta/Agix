# Agix Daemon RPC 接口

daemon(`agixd`)把一个多会话 Agent 引擎池暴露成 JSON-RPC 2.0 服务。
本文回答"怎么用它":怎么连上、有哪些方法、事件怎么读、错误怎么分辨。

**协议版本:3**(`daemon.auth`、`daemon.ping`、`daemon.status` 的 `protocol_version`,以及发现文件里的同名字段)。

场景、会话、侧链这些概念本身是什么,以及它们在磁盘上怎么落,见
[`ARCHITECTURE.md`](ARCHITECTURE.md);本文只讲它们在 RPC 上长什么样。

## 怎么读本文里的例子

方法一节里的代码块有两种标记:

```jsonc
// → params        请求的 params 对象
// ← result        成功响应的 result 对象
```

示例中的 `"…"` 表示**该字段一定存在,但值随机器或时刻而变**(路径、id、时间戳、
计数)。`{}` / `[]` 表示该字段是对象/数组,内容随情况而定。

带 `// ← result` 的块由 `daemon/tests/protocol_doc_examples.rs` 读回来,跟一个真实
daemon 的真实响应逐字段比对;`daemon.doctor` 的例子由 `crates/assembly/src/doctor.rs` 的
`documented_shape_tests` 比对。文档里写了的方法必须可调用、可调用的方法必须写进
§6,由 `daemon/tests/protocol_doc_matches_dispatch.rs` 保证。

---

## 1. 传输与编码

**帧格式**:每行一个 JSON 对象,`\n` 分隔(NDJSON)。不使用 Content-Length 头,
`tail -f` / `nc` 可以直接调试。WebSocket 例外:它本身就是消息分帧的,
**一条 text 消息 = 一帧**,不加 `\n`。

| 传输 | 怎么开 | 认证 |
|---|---|---|
| Unix socket | 默认开(`--socket <path>` 可改) | 文件权限:socket 为 `0600`,daemon 为它新建的目录为 `0700`;daemon 自己起的进程连不上,见 §5.1 |
| TCP | `--listen <addr>` | `daemon.auth` 握手,见 §5.1 |
| WebSocket | `--listen-ws <addr>` | `daemon.auth` 握手 + Origin 校验,见 §5.1 |

TCP 与 WebSocket 都要求 `--token` 或 `AGIX_DAEMON_TOKEN`;没有 token 时监听器
**启动即失败**,不会先起来再拒绝所有人。**推荐用环境变量**:命令行参数在 `ps` 里
别的用户也看得见,daemon 自己起的进程(模型调用的 Bash)也看得见。daemon 起子进程
(Bash、hook、stdio MCP 服务器)时会把这个变量剥掉,不会传下去。

**TCP 与 WebSocket 都是明文,daemon 不做 TLS。** token 和之后的每一帧(提示、工具结果、
`config.setProvider` 的 `api_key`)都按原样过网,所以只把它们开在回环地址、可信网络上,
或者经 SSH 隧道 / 自己的 TLS 终结代理转出去。

所有监听器都 bind 好之后,daemon 才写 `instances.d` 里自己的那一项(§2):宿主看到这个
文件出现,就可以马上连上。

**分帧之上的一切都与传输无关** —— 方法、参数、事件帧、权限提问三条路完全一致,
换传输不需要改协议层代码。

编码 UTF-8。**单帧上限 16 MiB**,三条传输都强制。超限的连接被**关闭**而不是跳过
该帧:超限之后服务端已经不知道下一个帧边界在哪。

**请求**:

```jsonc
{ "jsonrpc": "2.0", "method": "session.create", "params": { }, "id": 1 }
```

`id` 省略 = 通知,服务端不回响应,出错(包括 `TOO_MANY_IN_FLIGHT`)也不回;流式帧照发。
`params` 省略等价于 `{}`。

**响应**:

```jsonc
{ "jsonrpc": "2.0", "id": 1, "result": { } }
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32004, "message": "…" } }
```

`result` 与 `error` 互斥,恰有其一。

**解析不了的帧被静默丢弃**:一行不是合法 JSON、或者缺 `method`,服务端跳过这一行
继续读下一行,不回任何响应。所以客户端不会看到 `PARSE_ERROR` / `INVALID_REQUEST` ——
这两个常量在代码里定义了,但没有任何路径发出它们(§9)。

---

## 2. 发现:找到 daemon

daemon 启动时在 `~/.agix/daemon/instances.d/` 下写自己那一份实例文件,正常退出时
删除。**每个实例一个文件,没有共享写点**,多个 daemon 同时启动不会因为并发
read-modify-write 同一个索引文件而丢更新。读取方 `read_dir` 汇总。

```
~/.agix/daemon/instances.d/desktop.json
~/.agix/daemon/instances.d/ci-runner.json
```

```jsonc
{ "instance": "desktop",
  "pid": 4210,
  "pid_start_time": 1754899200,
  "socket": "/Users/x/.agix/daemon/desktop.sock",
  "tcp": "127.0.0.1:7878",              // 仅在 --listen 时出现
  "ws": "127.0.0.1:7879",               // 仅在 --listen-ws 时出现
  "scenes": ["code", "harnix", "mini"],
  "protocol_version": 3,
  "started_at": "2026-08-11T10:00:00Z" }
```

写入是原子的(临时文件 + rename,`0600`),读取方不会读到半截 JSON。
`tcp` / `ws` 缺省表示那条传输没有监听。**token 不在这里**,它是凭据,不是地址。

**陈旧条目**:daemon 崩溃时文件会残留。读取方必须按 `(pid, pid_start_time)`
**两项一起**判定进程是否仍存活 —— 只比 `pid` 会在 pid 被系统复用时把已死的 daemon
误判为存活。判定为陈旧的条目应忽略。

集成的第一步永远是:`read_dir` → 联合判活 → 按 `scenes` / `instance` 挑一个 →
连它的 `socket`。**不要自己拼 socket 路径去猜**;daemon 换启动参数重启时,读实例
文件的客户端不用改代码。

daemon 另外还写一份 `~/.agix/scenes/<scene>/daemon.lock`(`{pid, pid_start_time,
socket_path, version, started_at, protocol_version}`)。它早于多场景支持,只描述
一个场景,新客户端用 `instances.d/`。daemon 运行期间对它持有 `flock`,同一场景的第二个
daemon 拿不到就拒绝启动;退出时删掉它。

**socket 路径**(daemon 自己启动时的推导规则,列出仅供排查):

```
--socket <path> 指定        → 用它
否则                        → $HOME/.agix/scenes/<scene>/daemon.sock
```

(Windows 上没有 Unix socket,daemon 改用命名管道 `\\.\pipe\agix-daemon`。)

`--instance <name>` 决定 `instances.d/<name>.json` 的文件名,默认是把所有激活场景
(`--scene` 加 `--scenes`)去重排序后用 `,` 连接。多 daemon 并存时 instance 名必须
不同 —— 重名不会被拦下,两个进程会互相覆盖对方的发现条目。

---

## 3. 通用约定

### 3.1 定位参数

| 参数 | 类型 | 说明 |
|---|---|---|
| `scene` | string | 场景 id。`session.create` 省略 = daemon 的默认场景(`--scene`) |
| `project_root` | string \| null | 绝对路径。`null` = 无项目会话;**省略** = daemon 的默认项目(它的 cwd) |
| `session_id` | string | 会话 id,跨重启稳定 |
| `turn_id` | string | 一轮对话的 id,客户端可自带,不传则服务端生成 |
| `prompt_id` | string | 权限提问的 id,进程内有效 |

`session.create` 的 `project_root` **三态**是有意的:省略、显式 `null`、给路径,
分别是"用 daemon 的默认项目"、"这是一个无项目会话"、"绑到这个项目"。给路径时目录
必须存在,否则 `PROJECT_NOT_FOUND`。

桌面端同时开多个项目时应始终显式传值(含显式 `null`)—— 依赖 daemon 的 cwd 会让
所有会话共享同一份项目层配置。

### 3.2 场景能力

每个场景声明两个能力位,`scene.list` / `scene.describe` 暴露:

| 能力 | 含义 | code | harnix | mini |
|---|---|---|---|---|
| `requires_project` | 会话必须绑定项目根 | ✅ | ❌ | ❌ |
| `supports_team` | 注册 Team 工具 | ✅ | ✅ | ❌ |

宿主据此决定新建会话时要不要先弹"选择项目"。`requires_project` 的场景传
`project_root: null` 会在 `session.create` 就被拒(`PROJECT_REQUIRED`),而不是等到
第一次工具调用才失败。

### 3.3 会话类型

| `session_kind` | 谁发起 | 默认是否出现在 `session.list` |
|---|---|---|
| `primary` | 客户端(`session.create` / `session.run_turn`) | 是 |
| `sidechain` | 模型(`Agent` 工具 / 团队成员) | 否 |

两者存储与查询机制完全一致,差异有三:

- **可见性** —— 侧链默认不进 `session.list`,要传 `include_children: true`,或者用
  `parent_session_id` 只查某个父会话的侧链。
- **可恢复性** —— 侧链一旦写下终止标记就**不可 resume**,列表里 `resumable` 为
  `false`,强行 `session.resume` 返回 `SIDECHAIN_TERMINAL`。一个跑完的一次性任务
  没有续跑语义;被中断的侧链没有这个标记,仍可恢复。
- **寿命** —— 侧链的 transcript 锚定在父会话上:

  | 触发 | 侧链 |
  |---|---|
  | `session.close` | **删除** |
  | `session.delete` | 删除(连同父 transcript) |
  | LRU 驱逐 / 空闲超时 / `daemon.shutdown` | **保留**(只回收运行体) |
  | daemon 崩溃 | 遗留在磁盘上 |

  遗留的侧链**没有任何自动回收**:仓库里没有清理它们的 GC,也没有对应的保留期
  配置。要清理只能手工删 transcript 文件。

  级联删除只删 `session_kind` 为 `sidechain` 的子会话。`session.fork` 出来的会话也记着
  源会话作父,但它是 `primary`,不受上表影响(见 `session.fork`)。

  实际含义:**"打开子 Agent 的会话查 history"只在父会话还没 close 时可用。**
  父会话 close 之后,它自己的 transcript 里仍留有 `Agent` 工具的 `tool_use` 与
  `tool_result`(含子 Agent 的最终输出),丢掉的只是中间步骤。

### 3.4 场景绑定与恢复

每个会话在 transcript 的 `Meta` 行里记着自己属于哪个场景。**恢复时不允许跨场景**:

```
Meta.scene
  ├─ 缺失(老文件)  → 按当前 daemon 的场景恢复,响应带 "scene_inferred": true
  ├─ 与 daemon 的场景不符 → SCENE_MISMATCH,拒绝
  └─ 相符                 → 恢复
```

这条检查对 `session.run_turn` / `.resume` / `.fork` / `.close` / `.delete` /
`.subscribe` / `.interrupt` 都生效,错误的 `data` 带 `recorded_scene` 与
`requested_scene`,客户端可以据此改用正确的 daemon(或者先 `scene.activate`)重试。
`run_turn` 传的 `session_id` 在磁盘上没有历史时不受此约束——那是新建,不是恢复。

**恢复出来的会话跑在它自己记录的场景上**,不是这个 daemon 的默认场景:系统提示词、
工具白名单、deferred 策略都跟着 `Meta.scene` 走,与设置层的解析口径一致。

`Meta.project_root` **不参与校验**:项目目录被移动过之后仍然能恢复。

### 3.5 时间、id 与用量

时间戳一律 RFC 3339 UTC(`2026-08-11T10:00:00Z`)。会话未激活时,列表里的
`created_at` / `last_active` 是**空字符串**(见 `session.list`)。

`usage` 四个字段,缓存读写各自成项:

```jsonc
{ "input_tokens": 12345, "output_tokens": 678,
  "cache_creation_input_tokens": 2048, "cache_read_input_tokens": 30720 }
```

四项**互不重叠**,不能只读 `input_tokens` 当作这一轮读了多少:开了提示缓存的
provider 上,命中之后绝大部分输入记在 `cache_read_input_tokens`,`input_tokens`
只剩增量。三项各自的价钱也不同(缓存读是普通输入的一个零头,缓存写是溢价),
所以要算钱就得四项分开算。

缓存两项是后加的。0.2.5 之前的客户端只读前两项,不受影响;读旧录像时这两项缺省为 0。

daemon 按**单轮**如实上报,不做跨轮/跨会话聚合。

**没有游标分页。** `session.history` 用 `offset` / `limit`;`session.list` 不分页,
一次返回全部。

### 3.6 项目信任

一个项目自己的配置(`<项目>/.agix/` 下的设置、agent 类型、hook 脚本,`.agents/skills/`)
是仓库内容。**没被信任的项目按插件级信任处理**:它的设置只能收紧,不能放宽;它的
`hooks_config`、`mcp_servers`、`scripts` 不生效,`.agix/hooks/` 不进 hook 命令的 `PATH`;
决定对话、凭据、遥测送到哪里的键(`providers`、指向项目自己 provider 的 `default_provider` /
`task_models`、`model.base_url`、`telemetry_url`、`recorder`)和 `instruction_file` 不生效;
它的 agent 类型只能收紧、不替换同名类型;它的 skill 里的 `` !`cmd` `` 不执行,`allowed-tools`
不授予任何东西。完整的表在 `ARCHITECTURE.md` §6.6。

信任是**项目根**的属性,只能写在全局层的 `project_trust` 里(`default` 加 `trusted` 路径 /
glob 清单),项目自己写的忽略。三态:

| `trust` | 意思 |
|---|---|
| `trusted` | 项目层全部生效 |
| `untrusted` | 项目层按上面收紧 |
| `none` | 没有自己的项目层(无项目会话,或项目的 `.agix` 就是全局目录) |

**0.3.x 缺省 `default: "trusted"`**,行为与之前完全一样;**0.4 缺省改为 `untrusted`**。
宿主用 `project.status`(§6.8)拿到状态和"信任之后才生效的东西"来画确认对话框,用
`project.trust` 记下用户的选择;无人值守的部署在全局设置里写 `trusted` glob 预先信任。
`session.create` / `session.resume` 的结果里有 `project_trust`(同上表的三个值)。

---

## 4. 数据类型

### 4.1 `ContentBlock` — 消息体

出现在 `session.history` 的 `messages[].content`,以及 `tool_result.content` 的
数组形态里。字段名与取值和 Anthropic Messages API 一致,便于直传。

```jsonc
{ "type": "text", "text": "……" }
{ "type": "image", "source": <ImageSource> }
{ "type": "tool_use", "id": "toolu_…", "name": "Bash", "input": { } }
{ "type": "tool_result", "tool_use_id": "toolu_…",
  "content": <ToolResultContent>, "is_error": false }   // is_error 为 false 时不出现
{ "type": "thinking", "thinking": "……", "signature": "…" }
{ "type": "redacted_thinking", "data": "…" }
```

> **`thinking` / `redacted_thinking` 必须原样保留。** `signature` / `data` 是载荷
> 不是装饰:开了思考且该轮有工具调用时,下一次请求要把上一轮的思考块连同签名回传。
> 纯展示的客户端可以不渲染,但不要丢弃。

还有一个只出现在请求方向的块(列出以免解析器遇到时报错):

```jsonc
{ "type": "cache_edits", "cache_edits": [ { "type": "delete_tool_result", "tool_use_id": "…" } ] }
```

消息本身按 `role` 判别:

```jsonc
{ "role": "user",      "content": [ <ContentBlock> ] }
{ "role": "assistant", "content": [ <ContentBlock> ], "stop_reason": "end_turn", "model": "…" }
{ "role": "system",    "content": "……", "kind": "local_command" }  // local_command|reminder|notice
```

`system` 只进 transcript、不进模型请求 —— 本地斜杠命令的输出、提醒、通知。

### 4.2 `ImageSource`

```jsonc
{ "type": "base64", "media_type": "image/png", "data": "<base64,无 data-URI 前缀>" }
{ "type": "url",    "url": "https://…" }
```

### 4.3 `ToolResultContent`

**无标签联合** —— 按 JSON 类型判别,不是靠 `type` 字段:

```jsonc
"纯文本结果"                          // string 形态
[ <ContentBlock>, <ContentBlock> ]    // 数组形态(工具返回了图片或多段内容)
```

客户端必须同时处理两种。

### 4.4 `Attachment` — `session.run_turn` 的附件

```jsonc
{ "kind": "file",  "path": "/abs/path.png" }
// daemon 读盘。按扩展名判定:图片 → image 块;其余内联为文本

{ "kind": "text",  "path": "selection", "content": "……" }
// 宿主已读好的文本。path 只作展示标签,可以是合成名(如编辑器选区)

{ "kind": "image", "media_type": "image/png", "data": "<base64>" }
// 宿主已解码的图片,磁盘上没有文件(剪贴板粘贴)
```

**形状不对的附件会被丢掉并写一条 warn 日志,turn 照常跑** —— 不会让整次调用失败。
所以客户端别指望用错误码发现自己拼错了字段名。

大小受 §1 的 16 MiB 单帧上限约束;base64 放大约 4/3,单张内联图片实际上限约
12 MiB。超大文件用 `kind: "file"` 让 daemon 自己读盘,不占帧。

### 4.5 `PermissionMode`

`session.create` / `session.run_turn` / `session.resume` 的 `options.permission_mode`,
和 settings.json 的 `permission_mode` 共用这一组取值(camelCase):

| 值 | 含义 |
|---|---|
| `dontAsk` | 不询问,未显式放行的一律拒绝 |
| `plan` | 只允许只读工具 |
| `default` | 未显式放行的工具调用都发 `prompt` 询问(**默认**) |
| `bubble` | 把询问上抛给父 Agent —— 程序专用,写进 settings.json 会被退回 `default` |
| `acceptEdits` | 自动放行项目内的文件写入与只读调用,其余询问(项目外的写入照样询问;`WebFetch` / `Ping` 只放行 `sandbox.allowed_domains` 列出的主机) |
| `bypassPermissions` | 不询问、不看任何规则 —— 规则里的 `deny`、`.ssh` / `.aws` / `.gnupg` 与 `/etc/passwd` 一类路径、`rm -rf /` 一类命令都放行;只有不合法的输入(没读过就改、超出上限、会被读错的路径)被拒,`Bash` 仍在 OS 沙箱里 |

`auto`、`yolo` 是 `bypassPermissions` 的另外两种写法:在 settings.json、`options.permission_mode`、
agent 类型的 frontmatter 里都收,解析出来就是 `bypassPermissions`,夹紧、存进会话日志、
`session.resume` 报出来的也都是 `bypassPermissions`。

表格是按**宽松程度从严到宽**排的,这个顺序就是会话级取值被夹紧的依据:会话只能
比 settings 更严,不能更宽,除非 settings 里开了 `allow_client_permission_override`。

`plan` 模式里调 `ExitPlanMode` 照常询问(`prompt_type: "permission"`、`tool_name:
"ExitPlanMode"`,有命中的规则时按规则),是 `plan` 不拒的唯一一个非只读工具:`permit` 后
退出,`deny` 留在 `plan`。会话中途 `EnterPlanMode` 进的 `plan`,退出后回到进入前的那个
模式;会话一开始就是 `plan` 的,退出后是 `default`。活着的会话,`session.resume` 的
`permission.mode` 报的是它此刻所在的模式。

**越界不报错。** 经 RPC 传进来的 `permission_mode` 无论是什么,都不会被拒绝:比
settings 宽的被**静默夹到 settings 允许的最宽处**并记一条 warn 日志,程序专用的 `bubble`
也只是这样被夹掉,不会有错误响应。所以客户端拿不到"你要的模式没生效"的信号 ——
要确认实际生效的模式,读 `session.resume` 响应里的 `permission.mode`。

### 4.6 `PermissionRule`

`options.permission_rules[]`:

```jsonc
{ "source": "session",              // 见下表
  "behavior": "allow",              // allow | deny | ask
  "tool_name": "Bash",
  "rule_content": "git push:*" }    // 可选;省略 = 匹配该工具的全部调用
```

`rule_content` 的写法按工具而定:

- `Bash`(以及 `Monitor`)用命令前缀模式(`"git push:*"`),对命令行里的**每一条简单
  命令**分别匹配:`&&` `||` `;` `|` `&`、换行、`$(...)`、反引号里的命令各算一段,引号
  里的不切。任一段命中 `deny` 就拒;`allow` 要每一段都被放行,或者整行原样等于一条
  不带通配的 `allow`。`deny` / `ask` 还按每段实际启动的程序再比一次:引号与转义去掉、
  花括号展开、程序路径取名字(`/bin/rm` 即 `rm`)、`env` / `command` / `exec` / `sudo` /
  `nice` / `time` / `timeout` / `xargs` 这类 wrapper 连同选项剥掉、`sh -c` / `eval` /
  `find -exec` 里的命令也算;程序名读不出来的(`$CMD x`)在有带内容的 `deny` / `ask`
  时按 `ask` 处理。`allow` 只认原文。
- `Bash` 的只读命令在路径参数都落在项目内时不询问;参数里有 `$VAR`、`~` 或替换的,
  `cd` 到不是项目内字面路径之后再读的,照常询问。
- 文件类工具用 glob(`"/etc/**"`)。匹配前两边都规整成绝对路径:相对路径按会话的
  工作目录解析,`.` / `..` 去掉,`~` 展开;符号链接解析后的路径上命中 `deny` / `ask`
  也算。`*` 不跨 `/`,`**` 跨。`Read(...)` 同样约束 `Grep` / `Glob`,`Edit(...)` 同样
  约束 `Write` / `NotebookEdit`。

裁决只看 `behavior`:一次调用命中的规则里只要有一条 `deny` 就拒绝,否则有 `ask`
就提问,否则才是 `allow`。`source` 和规则写得多具体都不参与裁决——客户端传入的
`allow` 推不翻任何一层设置里的 `deny`。同一行为有多条命中时,按特异性、再按下表
的数值挑一条作为报出的那条规则:

| `source` | 数值 |
|---|---|
| `cliArg` | 60 |
| `session` | 50 —— **客户端经 RPC 传入的规则用这个** |
| `command` | 45 |
| `localSettings` | 40 |
| `projectSettings` | 30 |
| `userSettings` | 20 |
| `policySettings` | 10 |
| `plugin` | 5 |

`source` 与 `behavior` 都是 camelCase/小写的既有序列化格式,不是笔误。

### 4.7 `PermissionDecision` / `QuestionAnswer` — 回答提问

`prompt_type` 为 `permission` 时,`session.respondToPrompt` 的 `decision`,四种形态:

```jsonc
{ "type": "permit" }
{ "type": "deny", "reason": "…" }
{ "type": "permit_always", "scope": "session" }   // 本会话内一直允许
{ "type": "permit_always", "scope": "local" }     // 另外写进 .agix/settings.local.json,之后新建的会话即可见
```

未信任的项目(§3.6)不读它 `settings.local.json` 里的 allow 规则,`local` 写下的规则因此
不会被之后的会话读回;要它持久,先信任这个项目。

`prompt_type` 为 `question` 时,`session.respondToPrompt` 的 `answer`,两种形态:

```jsonc
{ "type": "answer", "text": "a" }        // 选项的 key,或者自由文本
{ "type": "decline", "reason": "…" }     // 不回答
```

`text` 等于某个选项的 `key` 时,模型读到的是 `key: label`(如 `a: main`);否则原样交给模型。

### 4.8 `stop_reason`

出现在 `turn_complete` 事件和 transcript 的 assistant 消息上:

| 值 | 含义 |
|---|---|
| `end_turn` | 模型正常结束 |
| `max_tokens` | 触到单次输出上限 |
| `tool_use` | 模型请求工具(轮内中间态) |
| `stop_sequence` | 命中停止序列 |
| `pause_turn` | 模型请求暂停。引擎把这条回复原样发回去让它接着写,所以一轮通常不以它结束;只有恢复策略不续(如 `NeverRecover`)时才会 |
| `cancelled` | 这一轮被 `session.interrupt` 取消;已经推过的那段回复留在 transcript 里 |
| `max_turns_reached` | 触到本轮 API 调用次数上限(引擎自己发的,不来自 API) |
| `unknown` | 反序列化兜底,遇到未知取值时的落点 |

---

## 5. 连接、认证、并发与顺序

### 5.1 认证

TCP 与 WebSocket 需要,两者用**同一个 token、同一段校验代码**。连接建立后的
**第一帧**必须是:

```jsonc
{ "jsonrpc":"2.0", "method":"daemon.auth", "params":{"token":"…"}, "id":0 }
← { "jsonrpc":"2.0", "id":0, "result":{"authenticated":true,
      "version":"…", "protocol_version":3, "capabilities":["…"]} }
```

`version` 是 daemon 二进制的版本,`protocol_version` 是本文描述的协议版本,
`capabilities` 是能力位(§6.1 `daemon.ping`)。宿主应当在握手这一步就拒绝它不认识的
协议版本,而不是用到一半才发现。

第一帧不是 `daemon.auth`、token 不对、或者根本不是合法 JSON → `UNAUTHORIZED
(-32003)`,然后关连接(WebSocket 会先发 close 帧再断,浏览器端拿到的是"被拒绝"
而不是"连接出错")。token 比对是定长时间比较。

认证粒度是 **daemon 级**,不按场景或会话签发 —— 一条通过认证的连接可以访问这个
daemon 的全部场景与会话,而且**没有任何逐方法的授权**。特别地,
`config.setProvider` 能把后续所有模型流量指向调用方给的 `base_url`/`api_key`;
把 socket 或 token 暴露出去,等同于暴露模型凭据本身。连接不加密(§1),token 在不可信的
网络上一过就算泄露了。

Unix socket 靠文件系统权限(只有 daemon 的用户能连),不握手,一样是"能连上就全权"。
唯一的例外是 daemon 自己起的进程:对端是 daemon 的子孙(模型调用的 `Bash`、hook、stdio MCP
服务器,以及它们留在后台的进程)时,daemon 先发一帧
`{"jsonrpc":"2.0","id":null,"error":{"code":-32003,…}}` 再关连接,一个请求都不读。它们和宿主
是同一个用户,文件权限分不出来,而模型的命令要是能当宿主,就能自己信任自己的项目、自己批准
自己的询问。沙箱里的命令另外连不上 socket(macOS 上也连不上 TCP / WebSocket 端口),见
`ARCHITECTURE.md` §6.3。宿主要嵌 daemon 又要让自己起的子进程连它的,得另开一个不由 daemon
进程起的客户端。

#### WebSocket 的 Origin 校验

**绑定回环挡不住网页**:WebSocket 不受同源策略约束,用户浏览器打开的任何站点都能
向 `ws://127.0.0.1:<port>` 发起连接。所以升级握手阶段还要看 `Origin`:

| `Origin` | 结果 |
|---|---|
| 缺失(CLI、编辑器等非浏览器客户端) | 放行,进入 token 握手 |
| `http(s)://localhost` / `127.0.0.1` / `[::1]`,任意端口 | 放行,进入 token 握手 |
| 其它 | 升级失败,`403 Forbidden` |

端口不校验 —— 前端跑在哪个端口是部署细节。浏览器不允许页面伪造 `Origin`,这个判断
才成立;非浏览器客户端不发这个头,而缺失不构成任何证据,所以照常走 token 握手。

### 5.2 一条连接上的并发

**允许多个请求同时在途。** 每个请求在自己的任务里处理,读循环从不等待任何一个,
所以响应**可以乱序返回** —— 客户端必须按 `id` 匹配,不能假设先发先回。

**这条对长 turn 同样成立,而且正是它最关键的地方。** `session.run_turn` 一跑就是
几分钟,服务端这期间照常读取并处理同一条连接上的其它请求 —— 所以在跑 turn 的那条
连接上可以回答它自己的权限提问、可以 `session.interrupt` 把它停掉。浏览器一个 tab
只有一条 WS,全部交互都在上面,靠的就是这一条。

流式帧不会占住连接:每个 `session.event` 是独立的一帧,两帧之间随时可以插别的东西。

单条连接同时在途的请求数上限 **64**,超过返回 `TOO_MANY_IN_FLIGHT (-32017)`。
**是拒绝而不是排队**:排队就得让读循环停下来等,而"读循环不停"正是上面那条保证的
全部来源。

`id` 由客户端生成,在同一连接内必须唯一,数字或字符串均可。

**通知(无 `id`)只对无返回值的方法有意义**。对 `session.run_turn`、`session.create`
这类需要返回值的方法发通知是未定义行为:服务端会执行但不回响应。

### 5.3 同一会话的并发 `run_turn`

**一个会话同一时刻只能有一个 turn。** 第二个 `run_turn` 怎么办由它的 `on_busy`
决定:

| `on_busy` | 行为 |
|---|---|
| `"reject"`(缺省) | 立刻失败,消息不投递 |
| `"queue"` | 等正在跑的 turn 结束,再作为新的一轮执行;调用一直挂着,和普通 `run_turn` 一样流式返回 |
| `"steer"` | 把消息交给正在跑的 turn,它在**下一个步骤边界**(一批工具结果之后、下一次调模型之前)加进对话;调用立刻返回 |

`reject` 的错误:

```jsonc
{ "error": { "code": -32015, "message": "session is busy",
             "data": { "session_id": "S1", "current_turn_id": "t1" } } }
```

错误发生**在用户消息被投递之前** —— 消息不会被前一个 turn 吃掉。

`steer` 被接下时立刻返回 `{"session_id","turn_id","delivery":"steer","into_turn_id"}`,
`turn_id` 是这条消息自己的 id,`into_turn_id` 是接下它的那一轮。之后:

- 消息真正加进对话时,那一轮推一帧 `user_message_delivered`(§7)。模型已经答完、只差
  收尾时到达的消息也会被读到 —— 那一轮会再调一次模型,不会留到下一轮。
- 那一轮在读到它之前就结束了(被 `session.interrupt`、预算、策略截停或出错),消息
  **不进 transcript**,以一帧 `user_messages_returned` 交还,要不要重发由客户端决定。
- 配了 `UserPromptSubmit` hook 时,消息在加进对话前同样过它(hook 输入里 `delivery` 为 `"steer"`,
  开一轮的消息为 `"turn"`)。改写了的,`user_message_delivered.content` 是改写后的文本;被拒的
  **不进 transcript**,那一轮照常继续,推一帧 `user_message_refused` 带回原文与 hook 的 `message`——
  再发一遍还会被拒,所以不走 `user_messages_returned`。

正在跑的 turn 已经过了最后一个步骤边界时,`steer` 退化成 `queue`。带 `attachments`
的消息不走 `steer`,直接按 `queue` 处理。

### 5.4 事件帧推给谁

`session.event` 帧推给**所有订阅了该会话的连接**。

**连接不是会话的所有者。** 一个会话可以被任意多条连接同时观看,它们收到同一份帧流。
发起 `run_turn` 的那条连接没有特殊地位,它只是被自动加进订阅者集合而已。

| 订阅来路 | 说明 |
|---|---|
| `session.subscribe` | 显式订阅 |
| `session.run_turn` | 发起方自动订阅 |

退订用 `session.unsubscribe`;连接断开时它的全部订阅一起消失,**别的什么都不动**。

**关于慢客户端**:每条连接前面有一个 1024 帧的出站队列。某个会话的帧广播过来时队列
已满,这条连接**对这个会话的订阅被取消**(连接本身和它的其它订阅不动)—— 一个卡住的
tab 不能拖住 turn,也不能拖住其他正在看的 tab。被取消的一方会收到一帧
`subscription_dropped`(§7),它排在取消之前已经入队的全部帧之后;收到之后重新
`session.subscribe` 并靠 `last_seq` 追平(§5.6)。

`daemon.event`(§8)是另一条通道:与会话订阅无关,推给所有
`daemon.subscribeEvents` 的订阅者。

### 5.5 事件顺序保证

| 保证 | 说明 |
|---|---|
| 同一 turn 内 `text_delta` 严格有序 | 按到达顺序拼接即得完整文本 |
| 每个 `tool_result` 必有先行的同 `id` `tool_use` | 反之不然:被取消的工具可能没有 result |
| `turn_complete` 是本 turn 最后一个事件 | 其后只有 `run_turn` 的最终响应 |
| 每个开始了的 turn 都有且只有一个 `turn_complete`(出错时发起者收到 `run_turn` 的错误响应,其他订阅者收到 `error` 帧) | 被 hook 拦下、被中断、被预算或策略截停的 turn 同样以它结束 |
| 收到 `turn_complete` 时本轮已写进 transcript | 此刻读 `session.history` 能看到这一轮 |

> **⚠️ 按 `id` 配对,不要按位置配对。** 并发安全的工具是并行执行的,谁先跑完谁先发
> `tool_result`,所以事件流里 `tool_result` 的顺序与 `tool_use` 的顺序**可以不同**。
> 而 `session.history` 里的块是按 transcript 顺序还原的。按 `id` 配对的客户端在
> 两边都对,按数组下标配对的客户端在事件流里会错。

**子 Agent 事件**以 `subagent_progress` 包裹,其 `event` 字段是嵌套的同构事件。
父与子的事件在同一条流上交错出现,靠 `agent_label` / `agent_session_id` 区分归属。

### 5.6 断线与重连

**turn 进行中断线 → 什么都不会被取消。** turn 跑完、落盘,会话按正常的空闲超时
回收。断开只意味着这条连接的订阅没了。

**重连/新开 tab 的追赶顺序(不能颠倒)**:

```
1. session.subscribe {session_id}       → 拿到 last_seq,此刻起的实时帧进本连接
2. session.history  {session_id, ...}   → 读到 last_seq 为止的内容
3. 把 1 之后缓冲的实时帧接在后面
```

**先读历史再订阅会永久丢掉中间那段帧** —— `last_seq` 存在的唯一理由就是让这个接缝
可闭合。`last_seq` 是 transcript 里的**消息数**,和 `session.history` 的
`offset` / `total`、每条消息的 `seq`、`step_end` / `turn_complete` 帧的 `seq` 是同
一套坐标:读到 `offset + n == last_seq` 就追平了,之后的实时帧接着往下。压缩会让消息
数变小(§7.1),所以它是水位,不是只增不减的计数。

**没有"续接流"**:重连不会补发上一个 turn 已经推过的帧。已产生的内容都在 transcript
里,用 `session.history` 取 —— 追赶靠历史,不靠重放。

任务的输出不进 transcript,按字节偏移追赶:`task_progress` 帧带 `from_byte` / `next_byte`,
断线或订阅被取消期间漏掉的,从手上的 `next_byte` 起用 `session.taskOutput {from_byte}` 补齐
(§6.3 `session.taskOutput` 的跟随做法)。

**心跳**:daemon 不主动发心跳。空闲连接靠 TCP keepalive 或客户端自己的
`daemon.ping` 维持。WebSocket 传输会自动回应 ping 帧,但浏览器的 WebSocket API
发不了 ping —— 网页端要保活就用 `daemon.ping`。

**会话回收**:超过 `--session-idle-timeout`(默认 3600 秒)没活动的会话,以及池子
满(`--session-cap`,默认 32)时最久未活动的那个,运行体会被回收。transcript 不
受影响,下次 `run_turn` / `session.resume` 会自动从磁盘恢复。

---

## 6. 方法

### 6.1 `daemon.*`

#### `daemon.ping`

无参。

```jsonc
// ← result
{ "pong": true, "version": "…", "protocol_version": 3,
  "capabilities": ["on_busy_steer", "subscription_dropped_notice", "…"],
  "modules": ["mcp", "agents", "skills", "scripts", "plugin-packages"] }
```

`modules` 列出这个构建编入的模块,按装配顺序:`mcp`、`agents`、`skills`,以及载体与包
方案(`scripts`、`plugin-packages`)。上面是默认构建的;少编一个模块的构建就不列它,只含
内核的构建(`minimal` 档)为空。
属于某个模块的方法在没编入它的构建里返回 `METHOD_NOT_FOUND`,`error.data` 为
`{"module": "<id>"}`;目前这样的方法是 `mcp.*`(模块 `mcp`)与 `session.startAgent`(模块
`agents`)。`plugin.*` 在没有包方案
时仍返回 `PLUGINS_DISABLED`。

`capabilities` 列出协议版本之外、可以单独检查的能力。只会增加;客户端忽略不认识的项。

| 能力 | 意思 |
|---|---|
| `on_busy_steer` | `session.run_turn` 接受 `on_busy:"steer"`(§5.3) |
| `subscription_dropped_notice` | 订阅因为跟不上被取消时会收到 `subscription_dropped`(§5.4) |
| `session_lifecycle_events` | `daemon.event` 报会话进出内存与每一轮的开始结束(§8) |
| `events_lagged_notice` | `daemon.event` 送不到时会收到 `events_lagged`(§8) |
| `schedule` | 有 `schedule.*` 方法与 daemon 内的调度器(§6.7) |
| `session_tasks` | 有 `session.tasks` / `session.stopTask`,任务事件进 `session.event`(§6.3、§7) |
| `context_edits` | 有 `session.inspectContext` / `session.dropMessages` / `session.editMessage`(§6.3) |
| `extension_entries` | 有 `session.appendEntry` / `session.entries`,写入推 `extension_entry`(§6.3、§7) |
| `task_output` | 有 `session.taskOutput`,Shell 命令与监视器的输出边跑边记(§6.3) |
| `task_progress` | 任务边跑边推:`task_progress` 推 Shell 命令、监视器与 `RunCode` 程序新增的输出(程序的是 `log()` 行与每次子调用的一行),`task_updated` 推子 Agent 的用量与等待;`subagent_progress` 带 `task_id`(§7) |
| `task_output_offsets` | 任务输出按字节偏移:`session.taskOutput` 接受 `from_byte`,应答带 `from_byte` / `next_byte` / `dropped` / `final`;`task_progress` 帧带 `from_byte` / `next_byte`;被停掉的任务等工作收尾才发 `task_finished`(§6.3、§7) |
| `task_messages` | 有 `session.messageTask`,消息进运行中的后台子 Agent(§6.3) |
| `host_agents` | 有 `session.startAgent`,宿主不经模型在会话下起子 Agent;子 Agent 任务带 `usage`、`structured_output`、`detail.tag`(§6.3) |
| `goals` | 有 `session.goal` / `session.setGoal` / `session.updateGoal`,目标变化推 `goal_changed`(§6.3、§7) |
| `question_prompts` | `session.create` 接受 `options.prompt_types`;列了 `question` 的会话把模型的 `AskUserQuestion` 推成 `prompt_type:"question"` 的 `prompt` 帧,用 `session.respondToPrompt` 回答(§6.3、§7) |
| `session_export` | 有 `session.export`,把会话连同侧链、附件打成一个归档写到宿主给的路径(§6.3) |
| `ptc` | 这个构建带 `RunCode`,模型能写程序调本会话的工具(`docs/ptc.md`) |

#### `daemon.status`

无参。

```jsonc
// ← result
{ "version": "…",         // daemon 二进制版本
  "protocol_version": 3,
  "capabilities": ["…"],  // 同 daemon.ping
  "modules": ["…"],       // 同 daemon.ping
  "uptime_secs": "…",     // 数字,秒
  "sessions": 0 }         // 当前内存里的会话数(含侧链)
```

#### `daemon.doctor`

只读自检:三份 settings.json 各自的状态、provider 路由的解析结果、hooks 配置、
记忆存了多少,外加这个进程的几项装配事实。不做任何修复,也不发起任何网络探测 —— `HealthCheck`
契约要求每项检查从已经握着的状态立刻回答。

下面是一个全新装配、三份 settings.json 都还不存在时的完整响应,逐字段为真
(`path` 因机器而异,其余是字面值):

```jsonc
{ "scene": "code",
  "health": {
    "status": "ok",                   // ok | degraded | failing,取所有检查里最差的一项
    "checks": [                       // 每项的 details 与下面同名的顶层字段是同一份数据
      { "name":"settings.tiers",     "status":"ok", "summary":"every settings.json present parses" },
      { "name":"settings.providers", "status":"ok", "summary":"provider routing resolves" },
      { "name":"settings.hooks",     "status":"ok", "summary":"hooks configuration parses" },
      { "name":"memory",             "status":"ok", "summary":"0/200 memories",
        "details": { "enabled":true, "count":0, "cap":200 } } ] },
  "settings_tiers": [
    { "tier":"global",  "path":"…/settings.json",       "exists":false, "parses":true, "error":null, "unknown_keys":[], "ignored_values":[] },
    { "tier":"scene",   "path":"…/settings.json",       "exists":false, "parses":true, "error":null, "unknown_keys":[], "ignored_values":[] },
    { "tier":"project", "path":"…/.agix/settings.json", "exists":false, "parses":true, "error":null, "unknown_keys":[], "ignored_values":[] } ],
  "providers": { "ok":true, "default_provider":null, "configured":[], "task_models":[],
                 "warnings":[], "error":null },
  "hooks": { "configured":false, "ok":true, "error":null },
  "session_persistence": { "history_store_wired":true },
  "plugins": { "status":"enabled" },
  "permission_rules_count": 0,
  "model": { "model_name":"claude-sonnet-4-6", "api_type":"Anthropic" } }
```

`health` 是 `HealthCheck` 契约(`docs/extension_points.md`)的报告:宿主注册的检查
与引擎自带的四项并排出现在同一个 `checks` 里。状态是三值而不是布尔 ——
`degraded`("还能用,但有人该知道")与 `failing`("这块不能指望了")是两件事。

`settings_tiers` / `providers` / `hooks` 是 `health.checks` 里同名检查 `details`
的副本,放在顶层是因为现有客户端读的是它们。

`memory` 数的是会话用的记忆(全局与项目两个目录合起来)。到了上限(200 条)是 `degraded`:
轮末的记忆抽取不再添新的,已有的一条不删,要腾地方得有人删掉一些。`memory_enabled` 为
`false` 时是 `ok`,`details.enabled` 为 `false`。

`settings_tiers[].unknown_keys` 是那份文件里没有任何设置读的顶层键(拼错的、别的工具的),
加载时被忽略,daemon 日志里各有一条 warn。有这样的键时 `settings.tiers` 仍是 `ok`,
`summary` 里写明是哪几层。

`settings_tiers[].ignored_values` 是这一层(`settings.json` 与 `settings.local.json`)里类型
不对、因而被丢掉的值:`{ "tier", "origin", "key", "error" }`,`key` 是点分的
(`sandbox.network_mode`),整个对象都不对时是这个对象本身。被丢的只是这个值,同一个文件里
其余的值和别的层照常生效。有这样的值时 `settings.tiers` 是 `degraded`,`summary` 列出
`<层> (<键>)`。

`providers.ok` 只说明配置**形状**解析得通(每个 `task_models` 条目都指向一个存在
的 provider),不说明真能建出客户端,更不说明那个端点能连上 —— 缺凭据、`base_url`
指向一个没人监听的地址,这里都仍然是 `ok: true`。这项检查从已经握着的设置立刻回
答,不发起任何网络探测。

`plugins.status` 是这个二进制**能不能加载插件**:`enabled`、`disabled-by-policy`
(编进来了但配置关掉了)、`compiled-out`(根本没编进来)。

daemon 自己的会话池在 `health.checks` 里还有一项 `project.trust`,`details` 就是
daemon 自己的项目的 `project.status` 结果(§6.8):

```jsonc
{ "name":"project.trust", "status":"degraded",
  "summary":"project trusted only by project_trust.default, which 0.4 turns to untrusted; these would then stop taking effect: hooks_config, mcp_servers.github",
  "details": { "project_root":"/Users/x/repo", "trust":"trusted", "matched":null,
               "default":"trusted", "requires_trust":["hooks_config", "mcp_servers.github"] } }
```

没有东西要信任才生效、或项目由 `trusted` 清单明确信任时是 `ok`;项目未被信任而有东西被扣下
(`summary` 列出它们),或只靠 `default: "trusted"` 信任而有这样的东西(0.4 起它们会停用)时
是 `degraded`。

编入了 mcp 模块的构建,`health.checks` 里还有一项 `mcp.policy`(排在最前),说明在执行
哪一份 MCP 策略(`ARCHITECTURE.md` §6.4):

```jsonc
{ "name":"mcp.policy", "status":"ok", "summary":"no MCP policy",
  "details": { "source":"none",         // none | host | file
               "path":null,             // source 是 file 时为那个文件
               "in_force":false,        // 有策略在执行(读不了也算:那时全部拒绝)
               "allowed_servers":null,  // 规则条数;null = 没写 allowed_servers
               "denied_servers":null,
               "allowed_authorization_servers":null, // 条数;null = 没写,任何授权服务器都行
               "enterprise_idp":null,   // 策略指定的企业 IdP 的 issuer
               "error":null } }         // 读不了时的原因
```

`source` 为 `file` 而文件不存在时(系统管理路径上没放策略)是 `ok`、`in_force: false`。
策略文件读不了或解析不了时这项是 `failing`,`health.status` 随之是 `failing`:这时所有
MCP server 都被拒绝。

插件包开着(`plugins.status` 是 `enabled`)时还有一项 `plugins.enable_state`:两层
`enabled.json` 都读得了(或不存在)是 `ok`;有一层读不了是 `degraded`,`details` 是
`{"tiers": [{"tier": "global", "fault": "…"}]}`——那一层的包这时全部按禁用算。

#### `daemon.subscribeEvents`

无参。把当前连接标记为异步通知订阅者,此后推送 §8 的 `daemon.event` 帧,直到连接
关闭。**不补发过去的事件** —— 关心启动期通知的客户端要在连上后立刻订阅。同一条连接
再调一次照样回 `subscribed: true`,但每条通知仍只送一份。

```jsonc
// ← result
{ "subscribed": true }
```

#### `daemon.shutdown`

无参(传什么都一样)。先停止接受新连接,再关掉所有会话的运行体,等它们停下(最多几秒)
后回响应,进程随后退出。

```jsonc
// ← result
{ "shutting_down": true }
```

**不删除任何 transcript,包括侧链的**(§3.3)。也没有"优雅等待"选项:正在跑的 turn
被直接取消。关停开始后,已经连着的连接上要新建或带回内存的会话(`session.create`、
`session.run_turn`、`session.resume` 等)一律报错 `the daemon is shutting down`。
SIGINT / SIGTERM 走同一条路。

### 6.2 `scene.*`

#### `scene.list`

无参。列出这个二进制注册的全部场景,包括未激活的。

```jsonc
// ← result
{ "scenes": [
    { "scene":"code", "name":"Agix Code", "description":"…",
      "active":true, "sessions":0,
      "requires_project":true, "supports_team":true } ] }
```

`sessions` 是当前内存里挂在这个场景下的会话数。**数组顺序不稳定**(底层是
HashMap),客户端要按 `scene` 找条目,不要按下标。

#### `scene.activate`

```jsonc
// → params
{ "scene": "harnix" }
// ← result
{ "scene": "harnix", "active": true }
```

幂等。场景 id 不是这个二进制注册的 → `SCENE_NOT_FOUND`。

#### `scene.deactivate`

```jsonc
// → params
{ "scene": "harnix" }
// ← result
{ "scene": "harnix" }
```

该场景下还有**内存中的**会话时失败,`SCENE_HAS_ACTIVE_SESSIONS`,`message` 里列出
挡路的会话 id(没有结构化 `data`)。调用方先 `session.close` 再重试,不提供 `force`。

停用只是把场景移出激活集合,**不删除磁盘上任何内容**;它的历史会话仍然列得出来,
重新激活后仍可 resume。

#### `scene.reload`

无参。重新读 `<状态根>/scenes/<id>/` 下的声明式场景,重建场景表。

```jsonc
// ← result
{ "scenes": [ "code", "harnix", "triage", "…" ],  // 重建后全部可用的场景 id
  "added": [ "triage" ], "removed": [ ],
  "errors": [ "…" ] }                              // 没加载上的,逐条说明原因
```

已激活、重建后仍在的场景指向新的定义:改过 `prompt.md` 的场景,此后新建的会话用改过的版本;
已经在跑的会话保持建会话时的那份。新增的场景照常需要 `scene.activate`。消失的场景从场景表
移除,已激活的仍需 `scene.deactivate`。

#### `scene.register` / `scene.unregister`

不落盘地登记一个声明式场景,只在这个实例里有效:

```jsonc
// → scene.register params
{ "scene": { "id": "triage", "name": "Triage", "tools": ["Read", "Grep"] },  // scene.json 的内容;对象或 JSON 文本
  "prompt": "把进来的问题分类。" }                                          // prompt.md 的内容
// ← result
{ "scene": "triage", "registered": true }

// → scene.unregister params
{ "scene": "triage" }
// ← result
{ "scene": "triage", "registered": false }
```

`scene` 按声明式场景的规则从严校验(格式见 `extension_points.md`「声明式场景」),不合格或 id
已被占用(内建、宿主、磁盘或已登记的场景)→ `INVALID_PARAMS`。登记不等于激活,要用就
`scene.activate`。`scene.unregister` 只撤 `scene.register` 登记的场景,别的 →
`SCENE_NOT_FOUND`;有内存中的会话在用时与 `scene.deactivate` 一样返回
`SCENE_HAS_ACTIVE_SESSIONS`。daemon 重启后登记的场景不在了。

#### `scene.describe`

```jsonc
// → params
{ "scene": "code", "project_root": null, "include_secrets": false }
// ← result
{ "scene": "code", "name": "Agix Code", "description": "…",
  "project_root": null,
  "active": true,
  "capabilities": { "requires_project": true, "supports_team": true },
  "tools": { "allowed": null, "disallowed": [], "deferred": [ "…" ] },
  "settings": { } }
```

`project_root` 省略与显式 `null` 在这里是一个意思(没有会话要绑,不需要区分)。

`tools.allowed` 为 `null` 而不是 `[]`,是因为空白名单的含义是"不限制";`[]` 会被
读成"一个工具都不给",正好相反。`deferred` 是"先只报名字,`ToolSearch` 取到 schema
才展开"的那批工具。

`settings` 是三层合并后的完整结果,**不带溯源信息** —— merge 之后不保留每个字段来自
哪层。要看某一层的原样内容用 `config.get` 的 `tier`。

`include_secrets` 默认 `false`:`api_key` / `auth_token` / `token` / `secret` /
`password` / `authorization`,以及以 `_token` / `_secret` / `_password` 结尾的键(键名不分
大小写),值一律脱敏为末 4 位(任意嵌套深度);`headers` 与 `env` 对象(MCP server 的请求头
与 stdio 环境变量)里的值不论键名全部脱敏。

### 6.3 `session.*`

#### `session.create`

```jsonc
// → params
{ "scene": "code",                      // 可选,默认 daemon 的 --scene
  "project_root": "/Users/x/repo-a",      // 见 §3.1 的三态
  "options": {                            // 可选,只在创建时生效
    "permission_mode": "default",
    "permission_rules": [ ],
    "recorder": { "mode":"replay", "name":"…", "dir":"…", "strict":true },
    "clock": { "start": "2026-10-28T09:00:00Z" },  // 可选,记下来的时间从这里起
    "telemetry": { "output":"/abs/path.jsonl" },
    "output_schema": { "type":"object", … },  // 结构化输出必须满足的 JSON Schema
    "provider": "deepseek",               // 可选,providers 里的 id
    "model": "deepseek-chat",             // 可选
    "prompt": { "append": "…", "override": null },  // 可选,见 session.setPrompt
    "prompt_types": ["permission", "question"] } }  // 可选,宿主能回答的提问,默认只有 permission
// ← result
{ "session_id": "…", "scene": "code", "session_kind": "primary",
  "project_root": "…",
  "project_trust": "trusted",           // trusted | untrusted | none,§3.6
  "created_at": "…" }
```

`scene` 必须是**当前激活**的场景,否则 `SCENE_NOT_FOUND`。场景的 `requires_project`
为真而 `project_root` 是 `null` → `PROJECT_REQUIRED`;给了路径但目录不存在 →
`PROJECT_NOT_FOUND`。

`options` 在新建时生效,并随会话保存:之后每次重建引擎(设置变化、空闲回收后再用、
daemon 重启后恢复)都按同一份 `options` 重建,之后的调用里再传的 `options` 被忽略。
会话日志里没有记录过 `options` 的旧会话例外:恢复时采用这次调用带的那份。
`options` 记在会话日志里一条 `ns` 为 `agix.session`、`event` 为 `spec` 的扩展条目上,
`session.setModel` 选的模型也记在那里,所以同样跨重启保留。

`options` 逐字段读:不认识的字段、值读不出来的字段(如 `"permission_mode": "plann"`)
被**忽略**,其余字段照常生效;`options` 本身不是对象时整个忽略。被忽略的列在响应
`result.ignored_options` 里(没有被忽略的就没有这个字段),daemon 日志里也各记一条 warn:

```jsonc
"ignored_options": [
  { "field": "permission_mode", "error": "unknown variant `plann`, expected one of …" },
  { "field": "permision_rules", "error": "unknown field" } ]
```

`field` 为 `null` 表示整个 `options` 读不出来。`session.create`、`session.run_turn`、
`session.resume` 都这样。**以后的版本会改为拒绝**(`INVALID_PARAMS`),宿主现在就应当把
`ignored_options` 当作错误处理。

`recorder` 不落盘:同一进程里的重建接着原来的回放进度,不从头重放;daemon 重启之后,
把会话带回内存的那次调用(`session.resume`、带 `resume:true` 的 `session.subscribe`、
自动恢复的 `session.run_turn`)在 `options` 里再给一次 `recorder` 才接着录制或回放 ——
这是 `options` 里唯一一项以这次调用为准的,其余仍按建会话时记下的。

`recorder.mode` 取 `record` / `replay` / `rerun`;`dir` 省略时用 daemon 自己的
录制根目录。

**录像不脱敏。** 系统提示、整段对话、工具参数与结果按发出去的原样记下,不经过遥测的脱敏
策略——录像要的就是"实际发了什么"。所以提示里带着的密钥录像里也有;录像目录与文件只对
属主可读,保管标准与它所录的 transcript 相同。

回放在每个会话里分两路对录像:对话调用按顺序对录像里的对话调用,跳过夹在中间的辅助调用,
严格回放查的顺序就是这一路;带 `purpose` 的辅助调用(压缩、记忆提取、起名……)取录像里同一
`purpose`、还没回放过的最早一条,不管它录在哪两次对话调用之间 —— 后台的辅助调用什么时候
完成每次都不一样。对上的那条照样比对请求内容。某个 `purpose` 的辅助调用录像里已经没有了,
回放报错,不会转去调真的模型;录像里有、这次没要的,结束时算回放没跑完。

首轮结束后的自动命名也走这个会话的录制器:录制时它是一条 `purpose` 为 `title` 的辅助调用,
回放时从录像里取。起名是上面两条的例外:录像里没有这条调用(录的时候场景不起名、或者是更早
的录像),回放的会话就不起名,`name` 留空,不会转去调真的模型,严格回放也不把这当成分歧;
录像里有、这次没要的起名调用留着不用,不算回放没跑完。

`clock.start`(RFC 3339)是这个会话"记下来的时间"的起点:提示里的日期、会话日志的时间戳
从它起、按墙上时钟往前走(`ShiftedEnvironment`)。不给时用机器的时钟 —— 回放(`recorder.mode`
为 `replay`)例外:从录像开录的时刻(录像头的 `created_at`)起,于是提示里的日期与录下来的
那天相同,严格回放(`strict:true`)不会因为换了一天而对不上。`start` 写错只记警告、照旧用
机器的时钟。

`output_schema` 给了,`StructuredOutput` 工具就以这份 schema 作为自己的参数格式直接
宣告给模型(不再需要先 `ToolSearch`),不符合 schema 的调用以错误结果退回、模型可以
改了再交;被接受的那一份出现在 `turn_complete` 的 `structured_output` 里(§7.2)。

`prompt_types` 说宿主能回答哪几种 `prompt` 帧(§7)。`permission` 总是会问,写不写都一样。
列了 `question`,有人值守场景(`ARCHITECTURE.md` §3.2.3,如 `code`)的会话才有
`AskUserQuestion`,模型的提问推成 `prompt_type:"question"` 的帧等宿主回答;没列时这个
会话根本没有 `AskUserQuestion` —— 一个只会被拒绝的工具不该给模型看。不认识的取值被忽略,
不会让整份 `options` 作废。

`provider` 让这个会话的对话走 `providers` 里那个厂商的 client,而不是设置里
`task_models.main` 路由到的那个;子 Agent、压缩等其他路由任务仍按设置走。`model`
省略时用该厂商的 `default_model`。厂商没配置、或 `model` 不在该厂商的 `models`
白名单里 → `INVALID_PARAMS`。

#### `session.run_turn`

**流式方法**:最终响应之前会在同一连接推送 0..N 个 `session.event` 帧(§7)。
并发约束见 §5.3,顺序保证见 §5.5。

```jsonc
// → params
{ "session_id": "…",              // 可选!省略则自动新建一个会话
  "message": "把权限层重构一下",     // 必填;只有空白且没有附件 → INVALID_PARAMS
  "turn_id": "t-1",               // 可选,省略则服务端生成;不能是目标轮(goal-<修订>-<轮次>-…)或定时任务(schedule- / cron- 开头)的形式
  "attachments": [ ],             // 可选,见 §4.4
  "options": { },                 // 可选,同 session.create,仅新建时生效
  "on_busy": "reject" }           // 可选,reject | queue | steer,见 §5.3
// ← result
{ "session_id": "…", "turn_id": "…",
  "delivery": "turn",             // turn = 这条消息跑了一整轮;steer 见 §5.3
  "name": null,                   // 首轮自动命名的结果,场景不开启时为 null
  "api_calls": "…" }              // 数字:本轮打了几次模型 API
```

`message` 为空或只有空白、又没有一个形状对的附件(§4.4),这次调用直接以 `INVALID_PARAMS`
失败:不建会话、不调模型。只带附件、`message` 为空是可以的。

**最终响应里没有 `stop_reason` / `usage`** —— 那两个在 `turn_complete` 事件里
(§7)。想拿到它们就必须读事件流。

`session_id` 给了但不在内存里:磁盘上有历史就自动 resume,没有就新建一个用这个 id
的会话。跨场景的 id 不会被静默恢复(§3.4)。

turn 期间的引擎错误不是事件,是**这次调用的错误响应**:`ENGINE_ERROR (-32002)`,
`message` 形如 `"<引擎错误码>: <描述>"`。

#### `session.get`

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "session_id": "…", "name": null, "preview": null,
  "message_count": 0, "created_at": "…", "last_active": "…",
  "status": "active",              // active | inactive
  "session_kind": "primary",
  "resumable": true,
  "scene": "code", "project_root": "…",   // null = 无项目会话,或老日志没记
  "scene_active": true,
  "turn_state": "idle",            // idle | running
  "current_turn_id": null }
```

比 `session.list` 的条目多 `scene_active` / `turn_state` / `current_turn_id` 三项 ——
第一个要问 daemon 当前激活了哪些场景,后两个只有内存里的池子知道。`turn_state` 就是
"发送按钮该不该置灰"的依据。

`scene` / `project_root` 与 `session.list` 的条目**同源**(都是那一条 `SessionInfo`),
所以两个方法对同一个会话不会给出不同的答案。

**`context`——上下文用了多少。** 形状与 `turn_complete` 的 `context` 相同(§7.2),取最近
一次调用过模型的那一轮。会话在内存里、且至少有一轮调过模型时才有这个键。

**`health`——这个会话的插件报的健康检查。** 形状与 `daemon.doctor` 的 `health` 相同
(`status` 取最差的一项,`checks` 每项带 `name`/`status`/`summary`/`details`)。只有插件注册了
检查(`Registrar::health_check`)的会话才有这个键。

**`scripts`——这个会话的脚本都干了什么。** 只有绑了脚本的会话才有这个键;没绑的会话
**没有这个键**,而不是一个空对象——「这儿没有脚本」和「脚本一次都没跑」是两个不同的
答案。

```jsonc
"scripts": {
  "calls": 7, "applied": 5, "no_change": 1, "failed": 1, "refused": 0,
  "dropped": 0,                    // 非 0 表示 recent 只是尾巴
  "recent": [                      // 最近 20 条,新的在前
    { "point": "prompt.assemble", "script": "/proj/.agix/scripts/x.js",
      "entry": "onAssemble", "turn": 2, "outcome": "failed",
      "detail": "script exceeded its 100ms budget" }
  ]
}
```

`outcome` 取 `applied`(点采纳了返回值)、`no_change`(跑了但没有可施加的东西)、
`failed`(调用没完成:抛异常/超时/配额耗尽/没有引擎)、`refused`(要求的事它的出身
不允许)。**这四个在引擎里的效果完全一样——什么都没变**,所以一个脚本"没生效"到底
属于哪一种,只有这里说得清。`detail` 在后三种里带着原因。

侧链会话另有 `parent_session_id`;主会话没有这个字段(不是 `null`,是不出现)。

**`preview` 恒为 `null`,`message_count` 恒为 `0`** —— 两个字段在协议里占了位置,
但没有任何代码路径会填它们。要消息条数用 `session.history` 的 `total`。

#### `session.list`

```jsonc
// → params
{ "include_children": false,     // 可选,默认 false = 不含侧链
  "parent_session_id": null }    // 可选,给了就只返回这个会话的侧链
// ← result
{ "sessions": [
    { "session_id": "…", "name": null, "preview": null,
      "message_count": 0, "created_at": "…", "last_active": "…",
      "status": "active", "session_kind": "primary", "resumable": true,
      "scene": "code", "project_root": "…", "compact_count": 0 } ] }
```

返回内存中的会话 + 磁盘上的历史会话,合并去重,**不分页、不过滤场景**。
没有 `scene` / `project_root` / `status` / `limit` / `cursor` 这些**参数** —— 传了
会被忽略;但每个**条目**都带 `scene` 与 `project_root`,按项目或场景分组在客户端一次
遍历就能做完,不用逐个 `session.get`。

冷会话的范围是**这个 daemon 服务过的项目**:它自己的项目,加上这个进程里有会话
建在或恢复在其上的每个项目。不是磁盘上的全部项目 —— 那会报出别的 daemon、别的工作
目录的会话。设置变化不会让 daemon 忘掉服务过的项目;重启会,一个项目在重启后有会话
落到它上面之前不出现在列表里。活跃会话不受影响,它们本来就在内存里;`session.get`
按 id 查,不在列表范围里的冷会话也查得到。

`scene` / `project_root` 来自会话自己的 `Meta` 行(§3.4 校验的是同一处),所以冷会话
也答得出来。**两个字段恒出现**,老会话(`Meta` 里没记场景的 pre-v2 文件)是 `null` —— 
`null` 是"这份日志没写",不是"没有项目"。

`compact_count` 是这个会话被压缩过几次 —— 引擎写 transcript 的时候顺手记的,冷会话
也答得出来。它回答的是"这个会话被压过没有",**不回答"哪一次压掉了什么"**:那件事只有
压缩发生的当时说得清,看 §7.1 的 `compact` 帧。

**未激活会话的摘要字段几乎全是空的**:`name` 为 `null`,`created_at` / `last_active`
是**空字符串**,`message_count` 为 `0`。这是设计如此 —— 列表不为了填几个摘要字段去
逐个读 transcript。活跃会话的 `created_at` / `last_active` 是真的,`name` 在场景开启
自动命名且已经跑过一轮时才有值。

`parent_session_id` 是**事后**查子 Agent 的路径:它扫的是磁盘上的 `Meta`,daemon
重启后仍然可用。拿到子会话 id 后用普通 `session.history` 读内容 —— 侧链与主线共用
同一套查询 API,不提供父子合并视图。

这个过滤只比对 `Meta.parent_session_id`,所以 `session.fork` 出来的会话也会出现在
结果里,而且**一律被标成 `"session_kind":"sidechain"`、`"resumable"` 按侧链规则算**
—— 这一路径的 `session_kind` 是写死的,不是从磁盘读的。要判断一条记录到底是不是
侧链,用 `session.get` 问它自己。

#### `session.subscribe`

```jsonc
// → params
{ "session_id": "…",
  "resume": false }            // 可选,默认 false
// ← result
{ "session_id": "…",
  "last_seq": "…",             // 数字:transcript 水位,用于追赶(§5.6)
  "pending_prompts": [ ],      // 还没被回答的 kind:"prompt" 帧,原样重放
  "resumed": false }           // 这次调用有没有替你恢复它;true 时另有 scene_inferred
```

订阅之后该会话的 `session.event` 帧都推到这条连接上,直到 `session.unsubscribe`
或连接断开。重复订阅同一会话是幂等的。

**默认只能订阅内存里的会话** —— 会话不在内存里返回 `SESSION_NOT_FOUND`,而不是给一
个永远不会有数据的订阅。

**`resume: true` 表示"打开它,不管它在不在内存里"**:冷会话先恢复再订阅,响应带
`"resumed": true` 与 `session.resume` 那边同样的 `scene_inferred`。这不是默认行为,
因为恢复要读一遍 transcript 并占掉会话池的一个名额 —— 一个画会话列表的宿主给每一行
都订阅一次,默认恢复就等于替它把整页会话全恢复了,而它以为自己只是在看。

`resume: true` 不接 `options`:要给一个没有记录过 `options` 的旧会话指定
`permission_mode` 之类再恢复,显式调 `session.resume`。恢复失败的错误跟 `session.resume` 完全一致 —— 跨场景仍然
`SCENE_MISMATCH`(§3.4),终态侧链仍然 `SIDECHAIN_TERMINAL`,池子满了仍然按恢复那边
的规矩来。

`pending_prompts` 里的帧带**原来的 `prompt_id`**,所以中途打开的 tab 可以直接答那个
还悬着的提问(权限提问与模型的提问都在里面)。一轮自己的提问在这一轮结束时清掉;后台子 Agent 的提问不属于哪一轮,
留到被回答或它的任务结束。

#### `session.unsubscribe`

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "session_id": "…", "subscribed": false }
```

**没订阅过也返回成功**:调用方要的是"别再推给我",而现状已经如此。

#### `session.interrupt`

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "session_id": "…", "interrupted": false }
```

取消当前 turn,**保留会话**。没有进行中的 turn 时 `interrupted` 是 `false`,不是
错误 —— 调用方要的是会话空闲,而它确实空闲。模型还没开始回答(请求还在等响应、在退避
重试之间)、正在写自动压缩的摘要,也立即生效,不等它答;压缩作废,会话历史保持原样。这一轮以
`stop_reason: "cancelled"` 结束。

#### `session.compact`

**流式方法**,与 `session.run_turn` 发 `"/compact"` 相同。

```jsonc
// → params
{ "session_id": "…" }
// ← result:同 session.run_turn
```

**不看阈值,现在就压。** 配了摘要模型时由它写全文摘要(`FullCompact`),否则按引擎的
级联压到历史当前大小的一半左右。压了会推一帧 `compact`(§7.1),没什么可压时没有这
一帧;两种情况都以 `turn_complete`(`stop_reason: "command_executed"`)结束,正文
里有一句结果说明。会话正在跑 turn 时返回 `SESSION_BUSY`。

#### `session.clear`

**流式方法**,与 `session.run_turn` 发 `"/clear"` 相同。

```jsonc
// → params
{ "session_id": "…" }
// ← result:同 session.run_turn
```

清空对话上下文。清空本身写进 transcript,之后从磁盘恢复的会话也是空的。会话正在跑
turn 时返回 `SESSION_BUSY`。

#### `session.rename`

```jsonc
// → params
{ "session_id": "…", "name": "重构 daemon 装配" }   // name 为 null = 清除
// ← result
{ "session_id": "…", "name": "重构 daemon 装配" }
```

会话标题是**使用者的东西**,所以它写在会话自己的事实里(`~/.agix` 下的会话
sidecar),不是某个宿主自己的一层覆盖 —— 各宿主各存一份的结果是同一个会话在这边
叫一个名字、在那边叫另一个,共用 `~/.agix` 的意义就打了折。`session.list` /
`session.get` 的 `name` 就是这里写进去的值。

**冷会话也能改名**:命名是关于会话的事实,不是对运行中引擎的操作,所以它不会为了
改个名字把会话恢复起来。

`name` 传 `null`(或者不传这个键)= 清除,会话回到"自动命名"的状态。非空、最长
200 字符、不接受控制字符 —— 这是要显示在别人列表里的一行字。

**用户改过的名字不会被自动命名覆盖**:开了自动命名的场景在首轮结束后会生成一个
名字,但它跳过已经被 `session.rename` 命名过的会话。生成名字用的是会话自己项目的
`task_models.title` 路由,没配就用会话自己的对话模型。

#### `session.goal` / `session.setGoal` / `session.updateGoal`

会话目标:一个一轮做不完的目标。会话空闲时引擎自动开下一轮(`initiator:"goal"`),注入
`<goal_round>` 提醒模型目标和轮次,直到模型标记完成或受阻,或者轮数用完(用完即转为
`blocked`)。模型经 `GoalCreate` / `GoalGet` / `GoalUpdate` 工具操作同一个目标。

```jsonc
// session.goal → params
{ "session_id": "…" }
// ← result;没有目标时 goal 为 null
{ "goal": { "id": "…", "revision": 3, "objective": "迁移到新接口,测试全绿",
            "phase": "active", "armed": true, "max_rounds": 256,
            "rounds_started": 2, "rounds_since_armed": 2, "blocked_reason": null } }

// session.setGoal → params(会话已有未完成的目标时失败)
{ "session_id": "…", "objective": "…", "max_rounds": 256 }   // max_rounds 可选,默认 256
// session.updateGoal → params
{ "session_id": "…", "action": "pause" }   // pause | resume | edit | clear
{ "session_id": "…", "action": "edit", "objective": "…", "max_rounds": 64 }
// ← 二者的 result 同 session.goal
```

- `phase`:`active` / `paused` / `blocked` / `complete`。只有 `active` 且 `armed` 的目标
  会自动开轮。`armed` 不落盘:会话从日志恢复(驱逐后、daemon 重启后)目标一律不武装,要人
  `resume` 才继续。会话在内存里原地重建(`session.setPrompt`、`session.setModel` 换厂商、
  配置变化)保留武装;重建之后推一次 `goal_changed`。
- 宿主经这两个方法的操作都按「用户亲自发起」对待。模型那一侧:设定、修改、暂停、恢复
  只能在用户发起的那一轮做;自动轮里只能标完成或受阻,受阻要从自武装起的第 3 个自动轮起。
- 一轮以取消、hook 中止、预算耗尽、出错结束,目标就解除武装,等人决定。以 `max_tokens`
  结束的一轮和正常结束一样,接着开下一轮。
- 在空闲会话上设定或恢复目标,第一轮马上开始;`pause` 碰上正在跑的自动轮会中断它。
- 同一时刻至多有一个自动轮在等着开始:别的轮(客户端的、定时任务的)在它之前结束,不会
  再开一轮。等着的这一轮开始前目标被暂停、清除、修改或解除武装,它就不跑了。
- 清除之后再设的目标,`revision` 接着清除前的往上数,不会和旧目标的重复。
- 会话不在内存里时,`session.goal` 从日志读(`armed` 为 `false`),`setGoal` /
  `updateGoal` 先把它恢复起来。会话不存在 → `SESSION_NOT_FOUND`;状态不允许的操作 →
  `INVALID_PARAMS`,`message` 说明原因。
- 每次变化推 `goal_changed`(§7),并以扩展条目(`ns:"agix.goal"`)记进会话日志。

#### `session.appendEntry`

往会话日志里加一条扩展条目(`history.extension_entry`):宿主自己的、给人看的东西,
比如一张任务卡、一次批准的结论。和其余条目同序,恢复、分叉、`session.entries` 都看得到;
模型看不到它,除非宿主给这个命名空间配了 `ExtensionsAreVisible`。

```jsonc
// → params
{ "session_id": "…",
  "ns": "com.example.cards",   // 必填,1–128 字符;`agix.` 开头的是引擎自己的,拒绝
  "event": "job_done",         // 必填
  "payload": { } }             // 可选,任意 JSON,引擎从不解析
// ← result
{ "id": "…", "ts": "…", "ns": "com.example.cards", "event": "job_done",
  "payload": { }, "at": "…" }  // at:它前面已有的消息数,坐标同 session.history 的 seq
```

会话冷着也能写,不会把它拉进内存。会话在内存里时给它的订阅者推一帧
`extension_entry`(§7),`turn_id` 为 `null`。会话不存在 → `SESSION_NOT_FOUND`;`ns`
或 `event` 不合规 → `INVALID_PARAMS`。

#### `session.entries`

按日志顺序列出会话的扩展条目,不含 `agix.*`(引擎的记账)与侧链里的。

```jsonc
// → params
{ "session_id": "…", "namespaces": ["com.example.cards"] }   // namespaces 可选,省略 = 全部
// ← result
{ "entries": [ { "id": "…", "ts": "…", "ns": "com.example.cards", "event": "job_done",
                 "payload": { }, "at": "…" } ] }
```

`at` 让宿主把条目插回对话里:它排在第 `at` 条消息(`seq == at`)之前,`at` 等于消息
总数就是在最后。会话不存在 → `SESSION_NOT_FOUND`。

#### `session.setModel`

```jsonc
// → params
{ "session_id": "…",
  "model": "claude-opus-4-6",     // model 与 provider 至少给一个
  "provider": "anthropic" }       // 可选,providers 里的 id
// ← result
{ "session_id": "…", "model": "claude-opus-4-6", "provider": "anthropic",
  "applies": "next_turn" }
```

换掉这个会话**下一次模型调用**用的模型。transcript 不动 —— 这是换"谁来回答下一
条",不是改写已经说过的话。所以"这一段用便宜的,接下来换个强的"不再需要新建会话,
也就不再需要丢掉上下文。

只给 `model`:换模型名,仍走当前厂商的 client。给 `provider`:对话改走那个厂商的
client,会话在原地按新的规格重建(它是空闲的,不会腰斩什么),没同时给 `model` 就用
该厂商的 `default_model`。`model` 按它将被发往的厂商的 `models` 白名单检查,不在
白名单里 → `INVALID_PARAMS`,而不是把一个对端不认识的模型名静默发过去。子 Agent、
压缩等其他路由任务仍按设置走。

**有 turn 在跑时返回 `SESSION_BUSY`**,不排队。引擎跑 turn 的时候不读输入通道,
排进去的命令会在某个没人宣布的时刻生效 —— 调用方被告知模型换了,却看着旧模型答完
这一轮。要换就先 `session.interrupt`,或者等这一轮结束。换厂商要重建会话,会话还有后台
任务在跑(子 Agent、团队成员、后台程序、Monitor)时同样 `SESSION_BUSY`,`data` 多一个
`running_task_ids` 列出它们 —— 重建会停掉它们;等它们结束,或先 `session.stopTask`。

**只对内存里的会话有效**,冷会话返回 `SESSION_NOT_FOUND`(先 `session.resume`)。
选择记在会话日志里(见 `session.create` 的 `options`):会话被驱逐后重建、
`config.reload` 之后重建、daemon 重启后恢复,选的都还是这个模型和厂商。

#### `session.setPrompt`

这个会话自己的系统提示词:一段追加文字与一份整体替换,优先于 settings.json 的
`prompt_append` / `prompt_override`。`session.create` 的 `options.prompt` 给初值,这个方法
在会话进行中改。

```jsonc
// → params
{ "session_id": "…",
  "append": "回答一律用中文。",   // 字符串 = 设置
  "override": null }               // null = 清除,回到 settings 文件的值;省略 = 不变
// ← result
{ "session_id": "…", "append": "回答一律用中文。", "override": null, "applies": "next_turn" }
```

`override` 替换整份系统提示(场景骨架、规则、MCP 说明都不再出现),`append` 接在最后。
从下一轮生效:空闲的会话立即按新提示重建,历史不变。和 `options` 一样记在会话日志里,
重建、空闲回收后再用、daemon 重启后恢复都保留;settings 文件不被改动。只作用于这个会话
自己的 agent,它派生的子 agent 用各自类型与场景的提示。

有一轮正在跑、或有后台任务在跑时拒绝,`SESSION_BUSY`,`data` 同 `session.setModel`。`append` 与 `override`
都没给,或类型不对 → `INVALID_PARAMS`;会话不存在 → `SESSION_NOT_FOUND`。

#### `session.history`

```jsonc
// → params
{ "session_id": "…", "offset": 0, "limit": 100 }
// ← result
{ "session_id": "…",
  "messages": [ ],          // 投影后的消息数组,元素形状见 §4.1
  "total": "…",             // 投影后的消息总数
  "offset": 0,
  "limit": 100,
  "has_more": false,
  "entry_count": "…",       // 原始日志条目数(≥ total)
  "active": true }
```

返回的是**投影后的消息视图**,不是原始 JSONL 行:压缩边界被合并、侧链条目被过滤。

每条消息在 §4.1 的形状(`role`、`content` …)之外还带:

```jsonc
{ "role": "user", "content": [ ],
  "seq": 0,                 // 在投影视图里的位置,即 offset 的单位
  "id": "…",                // 写下它的那一行的 id;压缩后的替换消息是 "<压缩行 id>#<序号>"
  "ts": "2026-10-20T08:00:00Z",
  "turn_id": "…",           // 哪一轮写的;旧日志没有,为 null
  "origin": "user",         // 见下
  "usage": { } }            // 产生这条回复的那次调用的用量;只有 assistant 有,旧日志为 null
```

`origin` 区分模型眼里同样是 user 的几种消息:

| 值 | 含义 |
|---|---|
| `user` | 人打的字 |
| `steer` | 人在一轮运行中送来的(`on_busy:"steer"`) |
| `command` | 人跑的斜杠命令展开后的内容 |
| `injected` | 引擎或工具加的:提醒、召回的记忆、skill 的指令 |
| `tool_result` | 对上一条回复里工具调用的应答 |
| `assistant` | 模型的回复 |
| `compact` | 压缩后替换进来的消息,没有自己的一行 |
| `extension` | 插件写进 transcript、按项目配置对模型可见的条目 |
| `null` | 旧日志,不知道 |

界面只把 `user` / `steer` / `command` 画成人说的话。`turn_id` 与实时帧的 `turn_id`
相同,`seq` 与 `step_end` / `turn_complete` 帧的 `seq`、`session.subscribe` 的
`last_seq` 同一套坐标(§5.6),所以实时看到的一轮和事后读到的一轮可以逐条对上。

`limit` 默认 100,上限 500,超了**夹紧不报错** —— 响应回显实际生效的 `limit`,配合
`total` / `has_more` 就能知道自己被夹了。`offset` / `limit` 传成负数或非整数一律
退回默认值,同样不报错。

侧链会话有自己完整的 transcript,用它自己的 `session_id` 查即可。

#### `session.resume`

把磁盘 transcript 投影回内存并建立运行实体。

```jsonc
// → params
{ "session_id": "…",
  "create_if_missing": false,   // 可选,默认 false
  "options": { } }              // 可选,同 session.create;会话已记录 options 时忽略
// ← result
{ "session_id": "…",
  "status": "already_active",   // already_active | resumed | created
  "active": true,
  "message_count": "…", "entry_count": "…",
  "name": null,
  "created_at": "…", "last_active": "…",
  "scene": "code",
  "project_trust": "trusted",   // trusted | untrusted | none,§3.6
  "permission": { "mode": "default", "prompts": true, "prompt_timeout_secs": "…" },
  "scene_inferred": false }
```

- 会话已经在内存里 → `already_active`,什么都不动,只报状态。
- 磁盘上有历史 → `resumed`。
- 磁盘上没有历史:`create_if_missing: false`(默认)时 `SESSION_NOT_FOUND`;
  为 `true` 时新建,`status` 是 `created`。
- 场景不符 → `SCENE_MISMATCH`;老文件没记场景 → 按当前场景恢复,`scene_inferred`
  为 `true`(§3.4)。
- 目标是已终态的侧链 → `SIDECHAIN_TERMINAL`(§3.3)。
- 会话正在共用同一状态目录的另一个 daemon 里 → `INTERNAL_ERROR`,消息含
  "being written by another process";不会换 id 新建。`session.run_turn` 指名这样的会话
  同样如此。
- 会话因别的原因建不回来(例如它记录的 `provider` 已经不在配置里)→ `INTERNAL_ERROR`,
  消息写明原因。同样不会换一个新 id 建会话顶替,`session.run_turn` 也一样。

`permission.prompts` 为 `false` 意味着这个会话跑在 `bypassPermissions` 下,不会有
任何 `kind:"prompt"` 事件。

#### `session.fork`

```jsonc
// → params
{ "session_id": "…", "at_message": 12 }   // at_message 可选,省略 = 整段复制
// ← result
{ "session_id": "…",            // 新会话 id
  "parent_session_id": "…",     // 源会话
  "forked_at_message": "…",
  "message_count": "…",
  "entry_count": "…",           // 实际复制的日志条目数
  "source_message_count": "…",
  "active": false }
```

`at_message` 数的是**投影后的消息**(和 `session.history` 同一套坐标)。超过总数会被
夹到总数;`0` 是合法的("只要这个会话的开场,不要任何一轮")。不是非负整数则
`INVALID_PARAMS` —— 这里**不**像 `limit` 那样宽容,分叉到错的地方会产出一个看着
像样的错误分支。

分叉的结果**写到磁盘就完了,不会被拉进内存**:源会话完全不受影响,新 id 表现得像
任何一个有历史的 id,下一次 `session.resume` / `session.run_turn` 带上它就是。

两处有意的丢失:**侧链条目不复制**;源的 `Meta` 行被换成一条新的,记下
`parent_session_id`。

整段复制(省略 `at_message`,或它不小于消息总数)时复制到日志末尾,最后一条消息之后
写下的扩展条目(`session.appendEntry`)也在;从中间分叉时截在第 `at_message` 条消息
所在的那一行。

分叉记着源会话作 `parent_session_id`,但它是 `"session_kind":"primary"`,不是侧链:对源会话
调 `session.close` 或 `session.delete` 不会删它。`session.list {parent_session_id}` 会把它和
源会话的侧链一起列出,`session_kind` 照实标成 `primary`。

#### `session.export`

把一个会话在引擎里能迁移的全部状态——它和它的侧链各自的转录、消息里的图片、扩展条目——
在同一时刻打成一个归档,写到宿主给的路径。

```jsonc
// → params
{ "session_id": "…",
  "path": "/abs/dir/S1.agix.zip",            // daemon 所在机器上的绝对路径
  "format": "zip",                            // 可选:zip(默认)| dir
  "include": ["sidechains", "attachments"] }  // 可选,省略即这两项;录像只在列了 "recording" 时带
// ← result
{ "session_id": "…",
  "path": "…",
  "format": "zip",
  "format_version": 1,      // 归档格式的版本,与 manifest.json 的相同
  "bytes": "…",             // 归档在磁盘上的大小
  "cut_at": "…",            // 快照时刻
  "sessions": [ ],          // 每个会话一项:session_id、kind、parent_session_id、entry_count
  "attachments": 0,         // attachments/ 下的文件数
  "recording": false,       // 带没带录像
  "warnings": [ ] }
```

**一致的快照。** 这个会话和它下面任意深度的侧链,在同一时刻截断:之后才写下的条目、之后才
起的子会话都不在归档里。导出不占用会话——正在跑的一轮照常跑完,归档里有的是它到**最近一个
跑完的步骤**为止写下的内容(一轮的每一步结束都会落盘),清单里这个会话的 `active` 为
`true`、`turn_state` 为 `running`。导出不恢复、不创建会话,也不检查场景。

**路径。** 必须是绝对路径;它所在的目录必须已经存在;目标本身必须不存在(那里有一个符号链接
也算存在)——导出从不覆盖。归档先写到同目录下的一个临时名,写完才换到目标名上,失败时临时
文件会被删掉,目标名上不会留下半个归档。目标不能落在会话历史目录(`projects/`、`sessions/`)
之下。路径在 **daemon 所在的机器上**:经 TCP / WebSocket 连上来的客户端拿到的是那台机器上的
文件。

**归档的布局**(`format: "dir"` 时是同样结构的一个目录):

```
manifest.json
sessions/<session_id>/meta.json          Meta 行,加上名字、标签、用量计数
sessions/<session_id>/transcript.jsonl   原始日志,逐行照存(含 agix.* 条目与侧链条目)
sessions/<session_id>/history.json       整段 session.history 的消息(同样的 seq / id / origin)
sessions/<session_id>/entries.json       session.entries 的那些行
sessions/<session_id>/session_memory.md  有才有
sessions/<session_id>/runtime.json       会话在内存里时才有:权限模式、todo、目标;"durable": false
attachments/<sha256>.<扩展名>             消息里的 base64 图片解码后的本体,同样内容只存一份
recordings/<name>/calls.jsonl, blobs/    只在 include 列了 "recording" 时
```

`history.json` 里的图片块,`source` 换成 `{"attachment": "attachments/<sha256>.png",
"media_type": "image/png"}`;`transcript.jsonl` 保持原样(图片仍是内联 base64),所以同一张
图在归档里有两份。`include` 不含 `"attachments"` 时没有 `attachments/`,转录和 `history.json`
里的图片都换成一个写着媒体类型、大小与 sha256 的文本块,清单的 `importable` 为 `false`。

**清单** `manifest.json`:

```jsonc
{ "format": "agix.session-export", "format_version": 1,
  "engine_version": "…", "protocol_version": 3,
  "cut_at": "…", "root_session_id": "…",
  "include": ["sidechains", "attachments"],
  "importable": true,
  "sessions": [ { "session_id": "…", "kind": "primary", "parent_session_id": null, "depth": 0,
                  "scene": "code", "project_root": "…", "title": null, "dir": "sessions/…",
                  "entry_count": 12, "message_count": 4, "active": false, "turn_state": "idle",
                  "terminal": null,               // 侧链跑完时是 "completed" / "failed"
                  "transcript_sha256": "…" } ],
  "id_map": { "<session_id>": "sessions/<session_id>" },
  "attachments": [ { "file": "attachments/….png", "sha256": "…", "media_type": "image/png",
                     "bytes": 1234, "refs": [ { "session_id": "…", "entry_id": "…", "seq": 0 } ] } ],
  "url_images": [ ],        // 只有 URL 的图片:引擎手里没有本体
  "unresolved_blobs": [ ],  // 外置内容的引用,而这个 daemon 没挂那个存储
  "forks": [ ],             // 从这些会话分叉出去的会话 id,不进归档
  "recordings": [ { "dir": "recordings/…", "sessions": ["…"], "calls": 3, "covers": "latest_run" } ],
  "warnings": [ ] }
```

`refs` 的 `entry_id` 是转录行的 id,或 `history.json` 里那条消息的 `id`;`seq` 是那条消息在
`history.json` 里的位置,只在转录里而不在投影视图里的为 `null`。`format_version` 只在布局或
清单的含义变了时才加。

**带什么、不带什么。** 带:`Meta`、整份原始转录(含引擎自己的 `agix.*` 条目和宿主的扩展条目)、
任意深度的侧链、图片本体、会话记忆,以及会话在内存里时的那部分运行状态。不带:分叉出去的会话和
它分叉自的会话(只记 id)、"本会话内允许"的授权、设置 / provider / API key / OAuth、输入历史、
`prompt_state.json`、遥测、团队目录、调度、运行中的任务。录像只覆盖**最近一次运行**(每次
录制会重写 `calls.jsonl`);会话的 `recorder` 不记进日志,daemon 重启后只找得到 daemon 自己的
录像根下以会话 id 命名的那份。

归档与转录一样敏感:它就是这些会话说过的全部内容。

`include` 里有不认识的项、`format` 不是 `zip` / `dir`、路径不合上面的规则 → `INVALID_PARAMS`;
会话不存在 → `SESSION_NOT_FOUND`;写盘失败 → `INTERNAL_ERROR`。

#### `session.close`

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "closed": "…",              // 注意:是 session_id 字符串,不是布尔
  "sidechains_deleted": 0 }
```

销毁运行实体并释放资源。**这个会话自己的 transcript 保留**,之后仍可 resume ——
但**磁盘上记着它作父会话的那些会话全部被删除**:侧链,以及从它分叉出去的会话
(§3.3)。仍在跑的先取消再删;删除失败只写日志,不会让 `close` 失败。返回时这个会话
目标的每次变化都已记进日志,紧接着的 `session.goal` 从日志读到的就是关闭前的目标。

#### `session.delete`

```jsonc
// → params
{ "session_id": "…", "dry_run": false }
// ← result
{ "session_id": "…", "deleted": true,
  "sidechains_deleted": 0, "sidechain_ids": [ ] }
```

**真正删除磁盘 transcript 与 `sessions/<id>/` 整个目录(会话记忆、metadata、输入历史),
并级联删除记着它作父会话的全部会话(侧链,以及它的分叉)。不可逆。**

`dry_run: true` 只返回将要删除的清单(`deleted` 为 `false`),不实际删除 —— 供宿主
做二次确认。

#### `session.inspectContext`

这个会话**下一次**调模型会带什么,但不真的调。会话不在内存里就先恢复它;正在跑一轮时
返回 `SESSION_BUSY`。

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "basis": "next_step_without_new_input",
  "model": "…", "max_tokens": 0,
  "system": [ { "name": "skills.catalog",   // 块名,见引擎的扩展点文档
                "role": "user",             // system:进系统提示;user:作为对话开头的一条消息
                "origin": "kernel",         // kernel | config | {"plugin":"…"} | {"script":"…"}
                "tokens": 0, "text": "…" } ],
  "tools": [ { "name": "Bash", "source": "builtin", "schema_bytes": 0, "tokens": 0 } ],
  "messages": { "count": 0, "tokens": 0 },  // 坐标同 session.history
  "total_tokens": 0,                        // 本地估算(与 BPE 分词相差约 ±12%)
  "compact_threshold": 0, "hard_cap": null }
```

`basis` 说明这是什么:**现在开始一步、不带新输入**时的请求。真正的下一轮还会多出
用户这句话、为它召回的记忆、提交时 hook 注入的内容,所以总量会更大。装配时脚本扩展照常
运行(它们决定系统块),所以脚本自己的统计会把这一次也算进去。

#### `session.dropMessages`

把用户消息或工具结果从**此后的**上下文里去掉。消息本身留着,内容换成
`[removed by the host]`(图片去掉):后面的位置不变,工具结果仍然回答它的那次调用。
模型的消息不能去掉 —— 带签名的思考与工具调用去掉一半会被模型服务拒绝。

```jsonc
// → params
{ "session_id": "…",
  "at": [2, 5],     // 位置,坐标同 session.history
  "total": 8 }      // 可选:你看到的消息数;对不上(期间新增或压缩过)就拒绝
// ← result
{ "dropped": [2, 5] }
```

改动先作用到引擎内存里的上下文,再以 `agix.context` 扩展条目写进 transcript,所以恢复
之后照样生效;`session.history` 看到的是改过之后的内容。会改动前缀,模型服务的提示缓存
从这一点往后失效。位置不对、是模型的消息、`total` 对不上 → `INVALID_PARAMS`;正在跑一轮
→ `SESSION_BUSY`。

#### `session.editMessage`

把一条用户自己写的消息(只有文字,没有工具结果)改成 `text`。规则同上。

```jsonc
// → params
{ "session_id": "…", "at": 0, "text": "…", "total": 8 }
// ← result
{ "edited": 0 }
```

#### `session.tasks`

这个会话里在会话之外继续跑的工作:子 Agent(前台的和后台的)、团队成员、执行中的
Shell 命令与监视器,以及安排给它的定时任务(§6.7)。和模型用 `TaskOutput` / `TaskStop`
看到的是同一份。

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "active": true,           // 引擎是否在内存里;不在时只有定时任务
  "tasks": [
    { "task_id": "…",
      "kind": "agent",      // agent | teammate | shell | monitor | cron | code
      "status": "running",  // running | waiting | done | failed | stopped;定时任务总是 waiting
      "started_ms": 0, "ended_ms": null,
      "detail": { },        // agent:{agent_type,description,background,tool_use_id,agent_session_id,tag?}
                            // teammate:{team,member};shell:{command,tool_use_id}
                            // monitor:{command,description,persistent,tool_use_id}
                            // code:{description,background,tool_use_id}(RunCode 的程序,docs/ptc.md)
                            // cron:{cron,prompt,recurring,durable,expires_ms}
      "usage": { },         // agent / teammate:它自己的模型调用到此刻的合计(§3.5 四字段)
      "structured_output": { },  // 带 output_schema 起的子 Agent 交了才有
      "waiting_for": [ "…" ],    // 仅 waiting(定时任务除外):在等宿主回答的 prompt_id
      "error": "…" }        // 仅 failed
  ] }
```

`agent_session_id` 是子 Agent 的侧链会话,任务登记时就定了,`task_started` 里已经有;
侧链在子 Agent 建成后才落盘,之前 `session.history` 读它是 `SESSION_NOT_FOUND`。`tag`
只有 `session.startAgent` 给了才有。

`usage` 只算这个子 Agent 自己的调用,与它的侧链会话各次调用之和相等;它再起的子 Agent
另算在它们各自的任务上。父会话 `turn_complete` 的 `usage` 也不含子 Agent(§7.2)。

子 Agent 有询问在等宿主回答(§10)时是 `waiting`,`waiting_for` 列出这些询问的
`prompt_id`;答完(或超时)回到 `running`。它仍在跑:`stopTask`、`messageTask` 照常。

正在跑的全列出,已经结束的保留最近 20 个。会话不存在 → `SESSION_NOT_FOUND`。

#### `session.taskOutput`

`session.tasks` 里一件任务到目前为止产出了什么 —— 和模型的 `TaskOutput` 读的是同一份,只读、
不等它结束。

```jsonc
// → params
{ "session_id": "…", "task_id": "…",
  "tail_bytes": 8192,       // 可选:要最后多少字节,默认 8 KiB,最多 256 KiB
  "from_byte": 0 }          // 可选,与 tail_bytes 二选一:从这个偏移读到末尾
// ← result
{ "output": "…",            // tail_bytes:最后这么多字节(在字符边界上截);from_byte:从那里到末尾
  "truncated": false,       // tail_bytes:前面还有没给的,或者早已丢掉的;from_byte:dropped > 0
  "status": "running",      // 同 session.tasks 的 status
  "from_byte": 0,           // output 第一个字节的偏移
  "next_byte": 0,           // output 之后的偏移,下一次 from_byte 从这里接
  "dropped": 0,             // 要的起点到 from_byte 之间已经不在了的字节数;tail_bytes 时为 0
  "final": false }          // 输出已经结束,next_byte 不会再长
```

偏移是这件任务有史以来全部输出的字节数,从 0 起,只增不减;每件任务只保留最后 256 KiB,所以
`from_byte` 读到的是从要的起点和仍保留的最早一个字节两者中靠后的那个开始、到末尾的全部(至多
256 KiB)。要的起点落在一个字符中间时往后挪到字符边界,挪过的也算进 `dropped`。恒有
`要的起点 + dropped == from_byte`、`from_byte + output 的字节长度 == next_byte`。
`from_byte` 与 `tail_bytes` 同时给、不是非负整数、或者超过当前末尾 → `INVALID_PARAMS`,超过末尾时
message 里写着此刻的 `next_byte`。

`final` 在这件工作自己收尾之后才是 `true`:之后 `next_byte` 不再长,读它得到空的 `output`。
`status` 已经是 `stopped` 而 `final` 还是 `false`,是工作被停掉、正在收尾 —— 还会写几行。

Shell 命令的输出逐行记下,stdout 与 stderr 按到达的顺序交错;监视器的 stdout 逐行记下,
stderr 按到达的块原样记下,两者交错。`RunCode` 程序记 `log()` / `notify()` 行、每次子调用的
一行和最后的结果。子 Agent 与团队成员的 `output` 是它们每一轮结束时的回答文字,
不是过程 —— 过程走它们的侧链会话。会话不存在 → `SESSION_NOT_FOUND`;引擎不知道这个任务
→ `TASK_NOT_FOUND`;定时任务没有输出,也是 `TASK_NOT_FOUND`。引擎被回收出内存、或者任务结束得早
已被忘掉(只保留最近 20 个结束的),之前读到一半的也会变成 `TASK_NOT_FOUND`。

**跟随一件任务的输出**(宿主的"跟随"视图):

```
1. session.subscribe {session_id}
2. session.taskOutput {task_id}(要从头就 from_byte: 0)  → 记下 next_byte,显示 output
3. 每来一帧 task_progress(按 task_id):
   - 帧的 next_byte <= 手上的        → 已经有了,丢掉
   - 帧的 from_byte <= 手上的        → 接上 appended 里从 (手上的 - from_byte) 字节起的部分
   - 帧的 from_byte >  手上的        → 中间漏了(skipped_bytes 或丢帧):
                                      session.taskOutput {from_byte: 手上的} 补齐
   手上的 = 处理完的 next_byte
4. task_finished 之后读一次 from_byte: 手上的,final 为 true 就停
```

补读的 `dropped > 0` 表示那段已经被挤掉,明示"中间丢了 N 字节"即可。断线、订阅被取消
(`subscription_dropped`)之后照 1、2 重来,第 2 步用手上的 `next_byte`。

#### `session.stopTask`

```jsonc
// → params
{ "session_id": "…", "task_id": "…" }
// ← result
{ "stopped": true }         // 没有这个任务,或者它已经结束 → false
```

和模型的 `TaskStop` 是同一个入口:取消这件工作所在的令牌 —— Shell 命令连同它的整个
进程组一起结束,子 Agent 在下一个检查点停下;定时任务则是取消安排。被停掉的工具调用
给模型回的是"已取消",这一轮照常继续。

`session.tasks` 里它立刻是 `stopped`、带 `ended_ms`;`task_finished` 要等工作收完尾 —— 收尾时
写的输出(进程退出前的几行、程序交回的结果)先随最后一帧 `task_progress` 出去。工作 15 秒内
没收完尾的,到时照样发 `task_finished`,输出就此截止。

#### `session.messageTask`

```jsonc
// → params
{ "session_id": "…", "task_id": "…", "message": "…" }
// ← result
{ "delivered": true }
{ "delivered": false, "reason": "…" }   // 任务不存在、已结束、不是子 Agent,或者它此刻不在一轮里
```

和模型的 `TaskMessage` 是同一个入口:消息进后台子 Agent(含团队常驻成员正在处理的那条)
正在跑的这一轮,在它下一个步骤边界读到,和用户在一轮中途发给主 Agent 的消息一样。
Shell 命令、监视器没有收件箱。会话不在内存里 → `SESSION_NOT_FOUND`。

#### `session.startAgent`

宿主不经模型,在一个会话下起一个子 Agent。和模型调 `Agent` 工具、`background: true`
起的是同一种东西:登记成这个会话的任务(`kind: "agent"`),发 `task_started` /
`task_finished`,`session.tasks` / `taskOutput` / `messageTask` / `stopTask` 照常可用,
侧链会话带 `parent_session_id`,`session.list {parent_session_id}` 列得出。

```jsonc
// → params
{ "session_id": "…",                 // 父会话
  "prompt": "…",
  "description": "…",                // 可选,进 task.detail
  "subagent_type": "reviewer",       // 可选,按 agent 目录取模型、工具、skills、scene、permission_mode(可放宽,插件带来的类型只能更严)
  "model": "…",                      // 可选,压过 agent 类型的 model;不接受 provider,子 Agent 用父会话的 provider
  "allowed_tools": [ ], "disallowed_tools": [ ],  // 可选,在 agent 类型之上再收窄,只会更少
  "permission_rules": [ ],           // 可选,同 options.permission_rules 的写法;只有 deny 生效
  "output_schema": { },              // 可选,同 session.create;交上来的进任务的 structured_output
  "notify_parent": true,             // 可选,默认 true:结束时给父会话投 <task-notification>
  "tag": "…" }                       // 可选,宿主的关联串,原样出现在 task.detail.tag
// ← result
{ "task_id": "…", "agent_session_id": "…" }
```

- 父会话空闲、正在一轮里都能起,互不阻塞;不在内存里时先恢复,同 `session.run_turn`。
- 子 Agent 随父会话的运行体结束:父会话被 `session.close` 或 `daemon.shutdown` 时,它被停下
  (`stopped`)。还有子 Agent 在跑的会话不会被驱逐。
- 计入 `execution.max_concurrent_subagents`,也受委派深度限制;超了 → `TASK_LIMIT`,不排队。
- 权限:子 Agent 的工具调用照父会话的权限判;`permission_rules` 里的 deny 规则先判,命中
  就拒。allow / ask 规则不生效 —— 宿主起的子 Agent 只能比父会话更严。询问怎么答见 §10。
- `output_schema`:子 Agent 交的不满足 schema 时被退回重交;结束时在 `task_finished.task`
  与 `session.tasks` 的 `structured_output` 上。
- `notify_parent: true` 的通知在父会话的下一轮开头读到;父会话空闲时不会因此自己开一轮。

#### `session.respondToPrompt`

```jsonc
// → params
{ "session_id": "…", "prompt_id": "…",
  "prompt_type": "permission",          // 可选,默认 permission;取值同那一帧的 prompt_type
  "decision": { "type": "permit" } }    // 见 §4.7
// ← result
{ "prompt_id": "…" }
```

回答模型的提问(`prompt_type:"question"` 的帧)带 `answer` 而不是 `decision`:

```jsonc
// → params
{ "session_id": "…", "prompt_id": "…",
  "prompt_type": "question",
  "answer": { "type": "answer", "text": "a" } }   // 见 §4.7
```

`prompt_type` 必须与这个 `prompt_id` 那一帧的相同:拿 `decision` 去答一个还悬着的
`question`(或反过来)→ `INVALID_PARAMS`。认不出的 `prompt_type`、缺了或形状不对的
`decision` / `answer` 也是 `INVALID_PARAMS`。

**谁都能答,先答者生效。** 提问广播给该会话的全部订阅者,任何一条连接都可以回答,
不必是发起 turn 的那条。`prompt_id` 未知(已超时、已被别人答掉)时**静默成功** ——
调用方要的是"这个提问被答掉",而它确实被答掉了。

会话不在内存里 → `SESSION_NOT_FOUND`(这个和 `session.close` 不一样:答一个没在跑
的会话的提问不可能是"已经完成了")。

未在 `--permission-prompt-timeout`(默认 300 秒)内回答的提问**失败关闭**:权限提问按拒绝
处理,turn 带着一条 error `tool_result` 继续;模型的提问以"用户没有回答"作为
`AskUserQuestion` 的结果交还模型。都不会挂死。

### 6.4 `config.*`

#### `config.get`

```jsonc
// → params
{ "scene": "code", "project_root": null,
  "tier": "effective",            // global | scene | project | effective,默认 effective
  "include_secrets": false }
// ← result
{ "scene": "code", "project_root": null, "tier": "effective",
  "settings": { } }
```

**该层没有文件时 `settings` 是 `null`,不是 `{}`** —— "这层没配"和"这层配了个空的"
是两种状态,配置界面得能区分。`tier: "project"` 而没给 `project_root` 同理。
`tier` 取值非法 → `INVALID_PARAMS`。脱敏规则同 `scene.describe`。

#### `config.reload`

**无参**。重新读三层 settings.json、重新解析 provider 路由、重建 task router。

```jsonc
// ← result
{ "providers": [ ],           // 重载后配置里的 provider id
  "default_provider": null,
  "task_models": [ ],         // 重载后配置里的 task 名
  "routing": {
    "ok": true,               // 配置形状解析得通
    "warnings": [ ], "error": null,
    "router_rebuilt": false,  // 真的建出了一个可用的 router
    "router_error": null,
    "mcp_connected": [ ], "mcp_failed": [ ], "mcp_connecting": [ ] } }
```

`ok` 与 `router_rebuilt` 是**两件事**:配置可以解析得通(`ok: true`)却建不出客户端
(`router_rebuilt: false` + `router_error`),`api_type: openai_compatible` 就是这种。

重载按新配置重连 MCP server,最多等 10 秒:到点还没连完的列在 `mcp_connecting`,在后台接着连,
各自的结果随后以 `mcp_connected` / `mcp_connect_failed` 通知到达(同会话创建)。命令不存在、
没有执行权限、URL 解析不了的 server 不重试,直接进 `mcp_failed`。

```jsonc
// → params(都可选)
{ "scene": "code",            // 省略 = 这个 daemon 的默认场景
  "project_root": "/Users/x/repo-b" }   // 省略 = 这个 daemon 自己的项目
```

**没有按层失效的参数**:`tier` 不存在,重读永远是"这一对 `(项目, 场景)` 当前磁盘上
的三层整个重读一遍"。这个方法不知道动的是哪一层,所以缓存的绑定整个丢掉重建——全局层
与场景层是每个项目共同的下层,只失效一对是不够的。

`project_root` **不接受 `null`**:这个方法作用在**项目层**,而无项目会话没有项目层
(`config.get {tier: "project"}` 早就是这个答案)。传 `null` → `INVALID_PARAMS`。

失效**惰性生效**:已有会话在下一次 `run_turn` 之前若空闲则原地重建,忙碌的跳过本轮
下次再检查。**正在进行的 turn 不会被中断**;还有后台任务在跑的会话也算忙,这一轮照旧用原来的
配置,任务都结束之后才重建,所以后台任务不会因为配置变化被停掉。重建保留会话结束了的任务
(`session.tasks` / `session.taskOutput` 照查)和模型还没读到的 `<task-notification>`。

**多数情况下不需要显式调用**:daemon 监视每个项目的 `settings.json` /
`settings.local.json`,改动落盘后自己重读(去抖 300ms);文件内容没变的写入不算改动。监视触发的重读知道是哪一层变了,
只作废从那一层合出来的绑定:项目层只作废同一项目的会话,场景层只作废那个场景的,全局层
作废全部。skills、agents 目录的改动不作废任何会话,新内容就地生效。不存在的设置文件与
skills 目录监视不了;它被建出来之后(比如 `permit_always` 的 `local` 第一次写出
`settings.local.json`,项目第一次建出 `.agents/skills/`),下一次用到那一对 scene / 项目的
配置时(在上面新建会话,daemon 自己那一对还有 `commands.list` 等)会发现并重读,已经在跑的
会话要等到那时才作废。这个
方法留给监视起不来的时候(权限、内核 inotify 上限;`daemon.doctor` 会报),以及要已有会话
立刻看到一个新建文件的时候。

#### `config.getProvider` / `config.setProvider`

两者都接受 `scene` / `project_root` 定位参数,语义与 `config.reload` 一致:省略 = 这个
daemon 自己那一对,给路径 = 它服务的另一个项目,`null` → `INVALID_PARAMS`。
`setProvider` 写的是**被指定项目**的 `{project_root}/.agix/settings.json`,
`written_to` 回的就是那个路径;`api_key` 写进同目录的 `settings.local.json`(0600)。`mcp.addServer` 与 `mcp.updateServer` 同样接受这两个参数。

**provider 路由是按 `(项目, 场景)` 解析的**:一个项目在自己的 settings 里配了
`providers` / `task_models`,它的会话就走它自己的路由,不是 daemon 那个项目的。

```jsonc
// → params (getProvider)
{ "scene": "code",                      // 都可选,见上
  "project_root": "/Users/x/repo-b",
  "include_secrets": false }
// ← result (getProvider)
{ "providers": { },           // { "<id>": {api_type, base_url, api_key, api_key_env, default_model, models, reasoning_passback, thinking_toggle} }
  "default_provider": null,
  "task_models": { } }
```

`api_key` 默认脱敏为 `***<末 4 位>`。

```jsonc
// setProvider 的 params
{ "provider_id": "deepseek",
  "config": { "api_type":"openai_compatible", "base_url":"…",
              "api_key":"…", "default_model":"…" },   // 部分补丁,逐键 merge
  "default_provider": "deepseek",                      // 可选
  "task_models": { "subagent": "deepseek" },           // 可选,逐键 merge
  "delete": false }                                    // 可选;true = 删掉这个 provider
```

响应是 `config.reload` 的结果再加 `written_to`(写入的 settings.json)和
`secrets_written_to`(写了密钥时才有,即那个 settings.local.json)。

**总是写项目层**(daemon 默认项目下的 `.agix/`),没有选层的参数。`config` /
`task_models` 是部分补丁:只提到的键被覆盖,没提到的保持原样。

**密钥不进 settings.json。** `config.api_key` 写进项目层的 `settings.local.json` —— 这个
文件按约定不提交,在 unix 上每次写都是 0600;其余字段照旧写 settings.json。settings.json 里
这个 provider 已有的 `api_key`(早先版本写进去的)在这次写入时挪进 settings.local.json。
`delete: true` 从两个文件里都删掉这个 provider。

目标项目未被信任(§3.6)→ `PROJECT_UNTRUSTED`,什么都不写:项目层的 `providers` 在未信任时
不生效。要给它配 provider,先 `project.trust`,或者配在全局层。

两个文件在同一把锁下读改写(与 `mcp.addServer`、`project.trust`、权限询问里的"永久允许"
是同一把,跨进程也互斥),各自写临时文件再改名替换,先写 `settings.local.json`。设置文件是
符号链接的,不顺着写也不替换,以 `INVALID_PARAMS` 拒绝,消息里写明是哪个文件。

### 6.5 `mcp.*` / `plugin.*` / `import.*` / `commands.*`

`mcp.status` / `plugin.*` / `commands.list` **只作用于 daemon 自己的默认项目和默认场景**,
不接受定位参数。`mcp.addServer` / `mcp.updateServer` / `mcp.auth.start` / `mcp.auth.logout` /
`import.*` 接受 §6.4 的 `scene` / `project_root`:省略 = 这个 daemon 自己那一对,给路径 = 它
服务的另一个项目,`null` → `INVALID_PARAMS`。`mcp.auth.complete` / `mcp.auth.cancel` 按
`flow_id` 找到登录,不需要定位;`mcp.auth.setIdentity` 作用于这个 daemon 的全部项目。

#### `mcp.status`

无参。

```jsonc
// ← result
{ "servers": [ ] }
// 元素:{ "name":"…", "transport":"…", "tool_count": 0, "state":"connected",
//         "auth": { "state":"authorized", "issuer":"https://…", "scopes":["…"],
//                   "expires_at":"2026-11-08T10:00:00Z" } }
```

`state`:

| `state` | 意思 |
|---|---|
| `connected` | 连上了 |
| `disconnected` | 连上过,连接后来断了(进程退出、管道关闭);下一个请求先重连。断在途中的工具调用报错,不重发 |
| `connecting` | 还在连。会话创建最多等 10 秒,没连完的在后台接着连,连上后发 `mcp_connected`,这个项目的会话在下一轮之前换上它 |
| `auth_required` | 远端 server 要求 OAuth 登录(回了 401),还没登录或令牌已失效;没有工具,`tool_count` 为 0。用 `mcp.auth.start` 登录 |
| `failed` | 没连上(进程起不来、握手失败或超时),另带 `error` 说明原因 |
| `denied` | MCP 策略(`ARCHITECTURE.md` §6.4)拒绝了它,`tool_count` 为 0,另带 `error` 说明是哪份策略、哪条规则 |
| `untrusted` | 项目自己声明的 server,项目未被信任(§3.6),没有启动;`tool_count` 为 0,另带 `error`。`project.trust` 之后启动 |

`auth` 每个元素都有,是这个 server 的授权状态(`mcp_auth.md` §2):`state` 为
`none` / `required` / `authorizing` / `authorized` / `failed`;按需带 `issuer`、`scopes`
(已授予的;`required` 时是需要的)、`expires_at`(RFC 3339)、`error`(`failed` 时的原因)。
**从不带令牌。** 一个 `connected` 的 server 也可能是 `auth.state: "required"`:它要求更多
scope(403 `insufficient_scope`),现有令牌对别的请求仍然有效。

#### `mcp.addServer`

```jsonc
// → params
{ "name": "fs",
  "config": { "type": "stdio", "command": "…", "args": [], "env": {} } }
```

`config` 是一个 `McpServerConfig`,按 `type` 标签联合:`stdio` / `streamable_http` /
`sse` / `web_socket`(也认 `websocket`);`streamable_http` / `sse` 可以带 `oauth` 块
(`mcp_auth.md` §1),带 `oauth_provider` 的配置被拒绝(`INVALID_PARAMS`,message 给出 `oauth`
的写法)。每种都可以带 `startup_timeout_ms`(起进程加握手,以及工具调用以外的每个请求;默认取
环境变量 `MCP_TIMEOUT`,再缺省 30000)与 `tool_timeout_ms`(一次工具调用;默认取
`MCP_TOOL_TIMEOUT`,再缺省 600000),超时的请求在 server 端被取消。写进项目层 settings.json 并**立刻尝试连接**,
所以它是项目层的 server:配置里的 `$VAR` 只从它自己的 `env` 块展开,不读 daemon 的环境,
`oauth` 里的 `client_secret_env` / `private_key_jwt.key_env` 会让连接被拒(ARCHITECTURE.md
§6.4)——密钥要么写成字面值,要么把 server 声明在全局层。成功与否都会发一条
`mcp_connected` / `mcp_connect_failed` 的 `daemon.event`;要求登录的 server 发
`mcp_auth_required`,在响应的 `servers` 里是 `state: "auth_required"`。

**密钥不进 settings.json。** `config` 里的 `headers` 与 `env` 写进同目录的
`settings.local.json`(按约定不提交,unix 上以 0600 创建),其余字段写 settings.json;两份合并
之后才是这个 server 的配置,`$VAR` 照样从 `env` 块展开。settings.json 里这个 server 已有的
`headers` / `env`(早先版本写进去的)在这次写入时一并替换掉。

响应:`{ "written_to": "<路径>", "secrets_written_to": "<路径>", "servers": [ <同 mcp.status 的元素> ] }`,
`secrets_written_to` 只在改了 settings.local.json 时才有。
配置解析不了、名字为空、已有的 settings.json / settings.local.json 不是 JSON 对象 → `INVALID_PARAMS`。
MCP 策略拒绝这个 server → `MCP_SERVER_DENIED`,**什么都不写、不连接、不发通知**,`message`
写明是哪份策略、哪条规则。目标项目未被信任(§3.6)→ `PROJECT_UNTRUSTED`,同样什么都不做:
写进去的 server 也不会启动。

同名的服务已经存在时是替换,不是再加一份——settings.local.json 里它原有的那部分也一起换掉,
新配置没有 `headers` / `env` 就删掉。这个项目上正在跑的会话在下一轮之前按新连接重建,进行中的
调用不受影响。

#### `mcp.updateServer`

参数、响应与 `mcp.addServer` 相同。区别只在于服务必须已经在那份 settings.json(或同目录的
settings.local.json)里声明过,否则 `INVALID_PARAMS` —— 用来换令牌、换请求头这类"改一个已有的",不会因为名字拼错而
悄悄多出一个服务。连接立刻重建,这个项目上的会话下一轮拿到新连接。

#### `mcp.auth.start`

发起对一个 MCP server 的 OAuth 登录(`mcp_auth.md` §3)。

```jsonc
// → params
{ "server": "docs",
  "redirect": "loopback",          // 可省,默认 loopback;或 "external"
  "redirect_uri": "https://…",     // 只给 external,且必须给:https、回环上的 http、带 "." 的私有 scheme
  "scopes": [ "files:read" ] }     // 可省;省略按 挑战 > 配置的 oauth.scopes > 资源元数据 取
```

```jsonc
// ← result
{ "flow_id": "…", "authorize_url": "https://as.example/authorize?…",
  "redirect_uri": "http://127.0.0.1:53817/callback", "expires_in": 600 }
```

把用户送去 `authorize_url`。`loopback` 时 daemon 在 `redirect_uri` 上等浏览器回来,自己完成
登录并发 `mcp_auth_completed`、`mcp_connected`(失败发 `mcp_auth_failed`);`external` 时宿主
接住回调,调 `mcp.auth.complete`。`expires_in` 秒内没完成,登录过期(`mcp_auth_failed`)。
同一个 server 再发起一次会取消前一次(并先释放它占着的回环端口,固定 `callback_port` 也能重来);
不同 server 的登录可以同时进行。

没有这个 server、它不用 OAuth(stdio、websocket、配了固定 `Authorization` 头的)、它是不需要
登录的 `client_credentials` / `enterprise` server、redirect 不合法 → `INVALID_PARAMS`;MCP 策略
拒绝它,或它的授权服务器不在策略的 `allowed_authorization_servers` 里 → `MCP_SERVER_DENIED`
(这时用户还没被送去任何地方);发现失败、授权服务器不支持 PKCE S256、它或它的端点不是 `https`
(回环地址上的除外)、授权端点不是网页(`file:`、`javascript:`、别的应用协议)、无法注册客户端 →
`MCP_AUTH_FAILED`,`authorize_url` 不会交出来。

#### `mcp.auth.complete`

```jsonc
// → params
{ "flow_id": "…", "callback_url": "https://app.example/oauth/done?code=…&state=…&iss=…" }
```

```jsonc
// ← result
{ "server": "docs", "scopes": [ "files:read" ] }
```

校验 `state`(以及授权服务器给了的 `iss`),用 PKCE 校验码和 `resource` 换令牌,存下,只重连
这一个 server,随后发 `mcp_connected`。`callback_url` 不是回到这次登录的 `redirect_uri` →
`INVALID_PARAMS`;`state` 不符 → `MCP_AUTH_FAILED`,**登录继续等**真正的回调;授权服务器在回调里
报错或换令牌失败 → `MCP_AUTH_FAILED`,登录结束,状态 `failed`。`flow_id` 不存在(已完成、已
取消、已过期)→ `INVALID_PARAMS`。

#### `mcp.auth.cancel`

`{ "flow_id": "…" }` → `{ "server": "docs" }`。放弃这次登录,server 的 `auth.state` 回到发起之前
(还没登录过就是 `required`)。`flow_id` 不存在 → `INVALID_PARAMS`。

#### `mcp.auth.setIdentity`

```jsonc
// → params:二选一
{ "id_token": "eyJ…" }          // 企业 IdP 签给这个客户端的 OIDC ID token
{ "refresh_token": "…" }        // 或企业 IdP 的刷新令牌
```

```jsonc
// ← result
{ "reconnected": [ "docs" ] }
```

交给 daemon 用户的企业身份(`mcp_auth.md` §4),只放在内存里。所有 `"grant": "enterprise"`
且还在等身份的 server 随即用它换令牌并重连(`reconnected` 是重连了的 server,结果看
`mcp_connected` / `mcp_connect_failed` 与 `mcp.status`);之后令牌到期时也用它再换。企业 IdP
拒绝这个身份时 server 留在 `auth_required`,`auth.error` 写明要换一个新的。两个都给或都不给、
没有配置企业 IdP → `INVALID_PARAMS`。

#### `mcp.auth.logout`

`{ "server": "docs" }` → `{ "server": "docs" }`。授权服务器提供吊销端点时吊销令牌
(RFC 7009),删除令牌(保留客户端注册),断开 server;它回到 `state: "auth_required"`、
`auth.state: "required"`。不发通知。

#### `plugin.list`

无参。

```jsonc
// ← result
{ "plugins": [ ] }
// 元素:{ "name":"…", "version":"…", "description":"…", "enabled": true,
//        "root": "/…/plugins/cache/<name>/<version>",
//        "script_faults": [] }
```

列出磁盘上的全部插件及启用状态,包括已停用的。

`root` 是解包后的目录。有它,宿主要读包里自己关心的东西(界面产物、图标)就不必自己
拼 `<plugins-dir>/cache/{name}/{version}/`——那样会把 daemon 的磁盘布局变成它 API 的
一部分。

`script_faults` 是这个包的 `[[script]]` 绑定里没能兑现的那些,每条带原因。来自包的绑定
是逐条降级的,所以这里非空不代表包没装上,只代表少了那几条贡献。

#### `plugin.install`

```jsonc
// → params
{ "name": "code-review-helper",
  "version": "1.0.0",
  "download_url": "file:///abs/path/plugin.zip",   // 或 https://…
  "checksum": "<sha256 hex>",
  "scope": "global" }                              // 可选,global(默认) | scene
```

**daemon 自己去取,没有上传通道。** 没有 marketplace 查询,直接从给的源装。

`name` 与 `version` 各是 cache 里的一级目录名:1–128 个 ASCII 字母、数字、`.`、`_`、
`-`、`+`,不以 `.` 开头;别的(`/`、`..`、绝对路径、非 ASCII)一律 `INVALID_PARAMS`,
什么都不读不删。`plugin.uninstall` 同此。包里 manifest 的 `[plugin] name` 必须等于
`name`,否则按读不回来处理。

**`checksum` 对 `http(s)://` 源是必填的**,缺了会在发起请求之前就拒绝;`file://`
源可以省略。流程:取包(最大 128 MiB)→ 校验 → 解包到临时目录(有总量、条目数、压缩比
上限)→ 改名就位 → 按发现时的方式把 manifest 读回来并生成披露;读不回来、或披露里有超限的
文本,就把这次安装**回滚**并报 `INVALID_PARAMS`。带 `[[wasm]]` 或 `[scene.own]` 段的包在这
一步被拒——引擎不跑 WebAssembly 组件,场景改用 `[scene] dir`。同一版本已经装过时同样读回、
同样披露,不过就报错,但不删它。

响应:`{ "success": true, "message": "…", "disclosure": … }`。`disclosure` 是这个
插件会往模型上下文里塞的文本和它要起的 MCP 服务器(命令行或地址),让调用方在依赖它之前
先看见;字段见 `plugin_packages.md`。

#### `plugin.uninstall`

```jsonc
// → params
{ "name": "…", "version": "1.0.0", "scope": "global" }   // version 省略 = 全部版本
```

响应 `{ "success": true, "message": "…" }`。

#### `plugin.enable` / `plugin.disable`

```jsonc
// → params
{ "name": "…", "scope": "global" }
```

响应 `{ "name": "…", "enabled": true, "scope": "global" }`。启用状态按层记录,
不改动已安装的文件。那一层的 `enabled.json` 读不了或解析不了时报错、不覆盖它——这时这一层
的包全部按禁用算,见 `plugin_packages.md`。

#### `plugin.reload`

无参。重扫插件目录并重新加载,响应与 `plugin.list` 同形。

上面四个方法各自会自动刷新,**这个是给它们盖不到的情况用的**:有人手工把插件放进
目录、或者开发时原地改了包的内容。

**关于 `scope`**:只有 `global`(全局根下的 `plugins/`,跨场景共享)和 `scene`
(场景根下的,只对这个 daemon 的场景生效)两个值,默认 `global`,别的值返回
`INVALID_PARAMS`。

**什么时候生效**:装/卸/启用/停用/重扫**只影响之后新建的会话**。插件贡献的工具是在
会话创建时注入工具表的,已经在跑的会话保持原样。受影响的场景与斜杠命令会立即刷新,
所以 `scene.list` / `commands.list` 在调用返回之后就是新的。

> 插件子系统可以在编译期整个裁掉(daemon 的 `plugin-packages` feature,默认开启)。
> 裁掉或被策略关掉的构建里,上面五个方法一律返回 `PLUGINS_DISABLED (-32016)` ——
> 不是"未知方法",因为方法是存在的,只是这个构建不提供它。用 `daemon.doctor` 的
> `plugins.status` 判断当前是哪种情况。

#### `commands.list`

无参。

```jsonc
// ← result
{ "commands": [ ] }
// 元素:{ "name":"…", "description":"…",
//        "kind":"prompt|local",              prompt = 展开后继续问模型,local = 直接执行
//        "source":"builtin|user|project|plugin" }
```

#### `import.list`

探测某个项目里可导入的外部配置来源。

```jsonc
// → params(可选)
{ "project_root": "/Users/x/repo-b" }   // 省略 = 这个 daemon 自己的项目
// ← result
{ "sources": [ ] }   // 元素:{ "source": "claude_code|codex|cursor", "description": "…" }
```

#### `import.run`

```jsonc
// → params
{ "source": "claude_code",
  "project_root": "/Users/x/repo-b" }   // 可选,同 import.list
```

响应 `{ "source": "claude_code", "actions": [ … ] }`。`source` 必须是**同一个项目**
`import.list` **当前**还探测得到的那些之一,否则 `INVALID_PARAMS`。没有 `dry_run`。

导入读写的是那个项目自己的文件(`.claude/` / `.codex/` / `.cursor/`),所以
`project_root` 与 `config.*` 一样不接受 `null`。

导入完成后会记下这个决定,启动时的自动探测不再重复提示。

### 6.6 还没有的东西

`agent.*` 和 `team.*` 在早期设计里定过接口,**没有实现**,调用返回
`METHOD_NOT_FOUND`。起子 Agent 走 `session.startAgent`;列举、读输出、发消息给、停止运行中的子 Agent、团队成员走 `session.tasks` /
`session.taskOutput` / `session.messageTask` / `session.stopTask`(§6.3);要看一个子 Agent 干了什么,走它的侧链会话:`session.list
{parent_session_id}` 拿 id,再 `session.history`。

`session.import` 还没有:`session.export` 的归档带着版本化的清单,是将来导入要读的格式,但目前
没有方法能把它装回一个 daemon。

### 6.7 `schedule.*`

定时任务:到点时把一段 `prompt` 作为某个会话的一轮发给它。由 daemon 自己的调度器
每分钟检查一次 —— 没有连接也照样跑;会话不在内存里就先恢复。它走的就是
`run_turn` 那条路:这个会话正在跑时**排队**,开始和结束发 `turn_started` /
`turn_completed`(`initiator:"schedule"`,§8),帧推给这个会话的订阅者。daemon 没在跑
的那段时间里错过的触发**不补**。

模型自己也能经 `CronCreate` / `CronList` / `CronDelete` 工具管理它所在会话的任务,和
这里是同一份;模型建的周期任务 7 天后最后触发一次就删除,这里建的不会过期。

#### `schedule.create`

```jsonc
// → params
{ "session_id": "…",        // 必填,必须是这个 daemon 已有的会话
  "cron": "0 9 * * 1-5",    // 必填,5 段:分 时 日 月 周,按 UTC
  "prompt": "…",            // 必填
  "recurring": true,        // 默认 true;false = 下一次匹配时跑一次然后删除
  "durable": true }         // 默认 true;写进磁盘,daemon 重启后还在
// ← result
{ "job": { "id": "…", "cron": "…", "prompt": "…", "recurring": true, "durable": true,
           "session_id": "…", "created_ms": 0 } }
```

`cron` 不合法 → `INVALID_PARAMS`;会话不存在 → `SESSION_NOT_FOUND`。

#### `schedule.list`

```jsonc
// → params(可选)
{ "session_id": "…" }        // 只列这个会话的;不给就列全部
// ← result
{ "jobs": [ { "id": "…", "cron": "…", "prompt": "…" } ] }   // 元素同 schedule.create 的 job
```

#### `schedule.delete`

```jsonc
// → params
{ "id": "…" }
// ← result
{ "deleted": true }          // 不存在时为 false
```

会话被 `session.delete` 删掉时,它的定时任务一起删除。

### 6.8 `project.*`

项目信任(§3.6)。两个方法都接受 `scene` / `project_root` 定位参数,语义同 `config.reload`:
省略 = 这个 daemon 自己那一对,给路径 = 它服务的另一个项目,`null` → `INVALID_PARAMS`。
`scene` 只影响 `requires_trust` 的判断(比如项目的 `permission_mode` 比哪一层的更宽),信任本身
与场景无关。

#### `project.status`

```jsonc
// → params(都可选)
{ "project_root": "/Users/x/repo-b", "scene": "code" }
// ← result
{ "project_root": "…/work",
  "trust": "trusted",          // trusted | untrusted | none
  "matched": null,             // 命中的 project_trust.trusted 条目;null = 由 default 决定
  "default": "trusted",        // project_trust.default
  "requires_trust": [ ] }      // 见下
```

`requires_trust` 是这个项目里**要信任才生效**的东西,不论现在信不信任都列出来——
`trust` 是 `untrusted` 时它们正被扣下,`trusted` 时它们在生效、一旦不信任就会停:

| 条目 | 意思 |
|---|---|
| `hooks_config`、`scripts` | 项目设置里的 hook、脚本绑定 |
| `mcp_servers.<名字>` | 项目设置里声明或改写的 MCP server |
| `permission_mode`、`allow_client_permission_override`、`disable_skill_shell_execution` | 比下面几层更松的值 |
| `permission_rules.allow(<规则>)` | 项目设置里的一条 allow 规则 |
| `sandbox.<键>`(含 `sandbox.cache_directories.<组>`、`sandbox.subprocess_env.allow`) | 放宽沙箱的值 |
| `providers.<id>` | 项目设置里的一个 provider(新加的,或改写已有的) |
| `default_provider`、`task_models.<任务>` | 指向项目自己的 provider(下面几层没有的) |
| `model.base_url`、`model.auth_token` | 项目设置里的模型端点与令牌 |
| `telemetry_url`、`recorder`、`instruction_file` | 遥测送到哪里、录像写到哪里、读进提示词的文件 |
| `plugins.enabled`、`plugins.allow` | 比下面几层放进更多插件的值 |
| `agents/<名字>` | 替换同名类型、或要求比会话更松的模式 / 更高的调用上限的项目 agent 类型 |
| `skills/<名字>` | 正文里有 `` !`cmd` `` / ` ```! ` 的,或 `allowed-tools` 不空的项目 skill |

#### `project.trust`

```jsonc
// → params
{ "project_root": "/Users/x/repo-b",   // 可选,省略 = daemon 自己的项目
  "trusted": true }                     // 必填
// ← result
{ "project_root": "…/work", "trust": "trusted", "matched": "…/work",
  "default": "trusted", "requires_trust": [ ],
  "written_to": "…/settings.local.json" }
```

`trusted: true` 把这个项目根加进全局层 `settings.local.json`(0600)的
`project_trust.trusted`;`false` 把指向同一目录的条目删掉。记下的是**解析之后的真实目录**
(`..`、符号链接都展开),其中的 glob 元字符(`*`、`?`、`[`、`]`、`{`、`}`)用 `[…]` 转义——
`/x/Project [v2]` 记成 `/x/Project [[]v2[]]`,只命中它自己。结果是写完之后的
`project.status`,外加 `written_to`。**`false` 只删这一条**:另一条 glob 或
`default: "trusted"` 仍然信任它时,结果里照样是 `trusted`(`matched` 说明是哪一条),
要改就改全局设置。缺 `trusted`、`project_root` 是相对路径、`trusted: true` 而目录不存在 →
`INVALID_PARAMS`。

写完之后这个项目的会话在下一轮之前按新的信任状态重建,它的 MCP 连接重连,并发一条
`project_trust_changed`(§8)。

---

## 7. 流式帧:`session.event`

推给该会话的全部订阅者(§5.4)。**非终止** —— turn 的终止标志是 `run_turn` 的最终
响应。

```jsonc
{ "jsonrpc":"2.0", "method":"session.event",
  "params": { "session_id":"…", "turn_id":"…", "event": { "kind":"…" } } }
```

| `kind` | 载荷 | 说明 |
|---|---|---|
| `turn_start` | `{}` | 本轮开始,是本轮的第一帧 |
| `step_start` | `{step}` | 本轮要调一次模型;`step` 从 0 计。同一个 `step` 再来一次,说明上一次调用失败、被恢复策略重发(§7 表下):这一步此前推过的 `text_delta` / `thinking_delta` 作废 |
| `step_end` | `{step,stop_reason,tool_calls,usage,seq}` | 这次调用结束,它的回复和它要的工具结果都已落盘;`usage` 只是这一次调用的;`seq` 是此刻 transcript 的消息数(§5.6) |
| `text_delta` | `{text}` | 模型输出增量 |
| `thinking_delta` | `{text}` | 模型思考过程的增量,与正文分开,宿主可以折叠或不显示 |
| `tool_use_start` | `{id,name}` | 模型开始写一次工具调用,参数还在流;同 `id` 的 `tool_use` 随后到达。界面可以先画出工具行 |
| `tool_args_delta` | `{id,partial_json}` | 这次调用参数的原始片段,按到达顺序拼起来就是 `tool_use.input` 的 JSON 文本(高频) |
| `tool_use` | `{id,name,input}` | 模型发起工具调用,参数已完整。参数不是完整 JSON 的(多半被输出上限截断)`input` 为 `{}`,不执行,随后的 `tool_result` 是说明这一点的错误 |
| `tool_result` | `{id,name,content,is_error}` | 工具返回 |
| `prompt` | `permission`:`{prompt_type,prompt_id,tool_name,message,paths[],tool_use_id?}`;`question`:`{prompt_type,prompt_id,tool_use_id,question,header,options:[{key,label}]}` | 需宿主回答,用 `session.respondToPrompt`;`tool_use_id` 是被问的那次 `tool_use`(`question` 的 `prompt_id` 就是它)。`question` 的 `header` 可为 `null`,`options` 为空表示自由回答 |
| `subagent_progress` | `{agent_label,agent_session_id,agent_type,parent_turn,parent_tool_use_id,task_id,event}` | 子 Agent 事件镜像,`event` 是嵌套的本表结构;`parent_tool_use_id` 是父会话里发起它的那次 `Agent` 调用,不知道时为 `null`;`task_id` 是它在 `session.tasks` 里的任务 |
| `team_progress` | `{team,team_id,stage,stage_index,stage_count,status,members,failed}` | 团队阶段生命周期 |
| `skills_changed` | `{added,removed}` | 会话可用的 skill 与它上次看到的不同:skill 文件在两轮之间增删后,下一轮开始时发一次(同一份 skill 目录上的每个会话各发各的)。缓存了 `commands.list` 的客户端该重取 |
| `notification` | `{message,status}` | 模型用 `PushNotification` 要用户回来看:宿主该把 `message` 以通知的形式送到用户面前(桌面通知、消息推送……)。`status` 是 `"proactive"`(模型主动发的)或 `null`。同时触发 `Notification` hook,`reason` 即 `message` |
| `compact` | `{strategy,messages_before,messages_after,tokens_before,tokens_after,dropped_rounds?,dropped_messages?,estimated_tokens_saved?}` | 上下文被压缩了(§7.1) |
| `user_message_delivered` | `{message_id,content}` | 运行中送来的消息(`on_busy:"steer"`)已加进对话,模型下一次调用会读到 |
| `user_message_refused` | `{message_id,content,message}` | 运行中送来的消息被 `UserPromptSubmit` 拒收,不在 transcript 里;`message` 是 hook 给的理由,可为 `null` |
| `user_messages_returned` | `{messages:[{message_id,content}]}` | 运行中送来、但本轮结束前没读到的消息,不在 transcript 里,交还客户端 |
| `turn_complete` | `{stop_reason,api_calls,tool_calls,seq,usage,last_call_usage,context?,structured_output?,message?,truncated?}` | 本轮结束,transcript 已落盘,`seq` 是此刻的消息数;最终响应随后到达 |
| `error` | `{code,message}` | 本轮出错结束。只发给**发起这一轮之外**的订阅者 —— 发起者从 `run_turn` 的错误响应得知 |
| `task_started` | `{task}` | 这个会话的一件任务开始了(`task` 同 `session.tasks` 的条目)。Shell 命令与监视器只在跑过 1 秒后才报,短命令不报;不属于任何一轮,`turn_id` 为 `null` |
| `task_updated` | `{task}` | 一件报过开始、还没结束的任务变了:子 Agent 每调完一次模型(`task.usage`),有询问开始或结束等待(`task.status` 在 `waiting` 与 `running` 之间);`turn_id` 为 `null` |
| `task_progress` | `{task_id,appended,skipped_bytes,from_byte,next_byte}` | 一件报过开始的 Shell 命令、监视器或 `RunCode` 程序新打出来的输出,`appended` 是偏移 `from_byte..next_byte` 那段(偏移同 `session.taskOutput`)。每件任务每秒至多一帧,结束时在 `task_finished` 之前补最后一帧;一帧至多 8 KiB,多出的只发结尾,`skipped_bytes` 是跳过的字节数,恒等于本帧 `from_byte` 减上一帧 `next_byte`;漏帧或跳过的用 `session.taskOutput {from_byte}` 补;`turn_id` 为 `null` |
| `task_finished` | `{task}` | 一件报过开始的任务结束了,`task.status` 是结局;此后没有它的 `task_progress`,输出不再长,`session.taskOutput` 的 `final` 为 `true`。被停掉的任务等工作收尾后才发(§6.3 `session.stopTask`);`turn_id` 为 `null` |
| `goal_changed` | `{goal}` | 会话目标变了(`goal` 同 `session.goal`,清除后为 `null`);`turn_id` 为 `null` |
| `extension_entry` | `{entry}` | 宿主经 `session.appendEntry` 写了一条扩展条目(`entry` 同 `session.entries` 的条目);`turn_id` 为 `null` |
| `subscription_dropped` | `{reason}` | 这条连接对该会话的订阅被取消了(`reason` 目前只有 `"overflow"`,§5.4);不属于任何一轮,`turn_id` 为 `null`。重新 `session.subscribe` 并追平 |

`prompt`、`subagent_progress`、`team_progress`、`skills_changed` 在两轮之间也会发:
后台子 Agent 在父会话空闲时照样会问、会推进度。这时它们不属于任何一轮,`turn_id` 为 `null`。

**这张表是白名单。** 引擎内部还有其它事件,但只有上面这些会被转成帧发出来。特别地:
没有 `turn_state` / `agent_state` 事件 —— 判断 turn 是不是在跑要问 `session.get` 的
`turn_state`。

**但客户端必须忽略不认识的 `kind`。** 这张表只会往下长,而且加一种事件不该是一次
破坏性变更:碰到没见过的 `kind` 就跳过它,不要报错、不要断开、更不要把整条连接当成
协议错误。

一轮帧的次序是:`turn_start`,然后每次模型调用一组 `step_start` … `step_end`,这次调用
的 `text_delta` / `thinking_delta` / `tool_use` / `tool_result` / `prompt` 都落在这一组
里,最后是 `turn_complete`(或 `error`)。本地命令(`/clear` 之类)不调用模型,只有
`turn_start` 和 `turn_complete`。

并发安全的工具(读文件、搜索、抓网页……)在它的 `tool_use` 到齐时就开始跑,模型还在接着
写,所以它的 `tool_result` 可以夹在同一步后面的 `text_delta` / `tool_use` 之间到达。

**一次调用在回复要工具之前失败**(过载、尺寸被拒、流中途的 `error` 事件、连接在消息结尾
之前断开),它这一步推过的文本不会写进 transcript;恢复策略决定重发(换备用模型、压缩后
重试)时,先再发一次同 `step` 的 `step_start`,宿主应丢掉这一步已经画出来的文本。不重发,
这一轮就以 `ENGINE_ERROR` 结束。回复已经要过工具的,工具已经跑了,回复连同结果写进
transcript,这一轮以 `ENGINE_ERROR` 结束,不会重发。

`prompt_type` 有两种:`"permission"` 问一次工具调用能不能跑;`"question"` 是模型用
`AskUserQuestion` 问用户的,只发给在 `session.create` 的 `options.prompt_types` 里列了
`question` 的会话(§6.3)。两种共用同一帧、同一回答方法与同一个超时。

### 7.1 `compact`

```jsonc
{ "kind":"compact", "strategy":"Snip",
  "messages_before":84, "messages_after":22,
  "tokens_before":152000, "tokens_after":41000,   // 引擎对历史大小的估算
  "dropped_rounds":9, "dropped_messages":61,      // 可选,压缩策略给不出就不出现
  "estimated_tokens_saved":31200 }                // 同上
```

`strategy` 是引擎选中的那一档:`Snip`(丢最旧的轮次)、`MicroCompact`(清空旧工具结
果的正文)、`CollapseContext`(把旧轮次折成摘要)、`FullCompact`(LLM 生成全文摘要)。
这份名单会随引擎增删,**按未知值处理它**,别把没见过的策略当成错误。

上下文被压掉是使用者**看得见结果、看不见原因**的一件事 —— "它怎么把前面说过的忘
了"。这条帧是唯一说出原因的时机:事后从 transcript 里推不回来,`session.get` 也不
答它。压缩发生在轮次中间,所以它**不是终止帧**,后面照常有 `text_delta` 与
`turn_complete`。

每一次真正改了对话的压缩都发这一帧:上下文过了阈值、`/compact`、请求因体积被厂商拒绝
后的压缩重发,开了 `compaction.reactive` 时轮中提前清掉旧工具结果的那一趟,以及按时间清掉
旧工具结果的那一趟(会话不少于 100 条消息时,15 分钟以前的;两者都是 `MicroCompact`,
消息条数不变,`tokens_after` 变小)。没有可丢的、或者丢掉的还不如压缩后补回去的提醒多
时,什么都不改,也不发这一帧。`tokens_after` 算的是压缩后的整段对话,包括补回来的
恢复消息(压缩前读过的文件、调用过的 skill、计划模式与后台任务的提醒)。

后三项是策略相关的:给不出的策略就不带这三个键,不是带 `null`。

### 7.2 `turn_complete`

```jsonc
{ "kind":"turn_complete", "stop_reason":"end_turn", "api_calls":2, "tool_calls":1, "seq":14,
  "usage":{…},                 // 本轮所有调用的合计
  "last_call_usage":{…},       // 本轮最后一次调用
  "context":{ "tokens":120000, "compact_threshold":150000, "window":200000 },
  "structured_output":{…},     // 可选
  "message":"…",               // 可选
  "truncated":true }           // 可选,只在为 true 时出现
```

`context.tokens` 是最后一次调用发出去的上下文大小,按厂商的计数(输入加两项缓存),不是
本地估算。`compact_threshold` 是自动压缩开始的位置,`window` 是模型的上下文窗口 ——
做一个"上下文用了多少"的指示条,这三个数就够了。本轮没有调用模型(被 hook 拦下、本地
命令)时没有 `context` 键。

`turn_complete` 的 `usage` 是完整的四字段用量(§3.5),
**是这一轮所有模型调用的合计**,不是最后一趟。一轮可能来回好几趟
(`api_calls` 就是趟数),按最后一趟计费会少算——用它做预算的宿主尤其要看清这一点。

它**只算这个会话自己的调用**,不含这一轮里起的子 Agent。子 Agent 的用量在它的任务上
(`session.tasks`、`task_finished` 的 `task.usage`);要一轮的总花费,把两者加起来。

`structured_output` 是本轮模型最后一次成功交给 `StructuredOutput` 的内容,会话带
`output_schema` 时它一定满足那份 schema;本轮没有调用这个工具就没有这个键。

`message` 只在截停这一轮的一方给了理由时出现 —— 比如 hook 返回的 `message`;`stop_reason` 此时是 `stopped_by_hook`。

`truncated: true` 表示本轮有一次模型调用的流在厂商标明结束之前就关了:OpenAI 兼容的流
既没有 `[DONE]` 也没有 `finish_reason`。不少网关两样都不发,所以这种流照常当作回复结束,
收到的内容就是整个回复,`stop_reason` 按 `end_turn` 报;但连接中途断开看起来一模一样,
回复可能不完整,宿主可以据此提示或重发。其余情况没有这个键。

**`stop_reason` 和 `usage` 只在这里出现**,`run_turn` 的最终响应里没有。

---

## 8. 异步通知:`daemon.event`

推给所有调用过 `daemon.subscribeEvents` 的连接,不属于任何会话或 turn。

```jsonc
{ "jsonrpc":"2.0", "method":"daemon.event", "params": { "kind":"…" } }
```

| `kind` | 载荷 | 什么时候 |
|---|---|---|
| `mcp_connected` | `{server, transport, tool_count}` | 启动连接、`config.reload` 重连、`mcp.addServer` / `mcp.updateServer` 成功 |
| `mcp_connect_failed` | `{server, error}` | 同上,失败时,`error` 是原因(进程起不来、握手失败、超过启动时限……)。另外,启动、重连、另一个项目第一次起会话时要连的 server 里,配置解析不了的、被 MCP 策略拒绝的(`ARCHITECTURE.md` §6.4)各发一条;会话创建时还没连完、之后在后台连上或失败的 server 也各发一条 `mcp_connected` / `mcp_connect_failed` |
| `mcp_auth_required` | `{server, scopes}` | 远端 server 要求登录:没有令牌、令牌被拒且刷新不了,或要求更多 scope(这时 server 保持连接);`enterprise` server 还没有可用的企业身份(宿主该调 `mcp.auth.setIdentity`)。`scopes` 是要请求的(可能为空)。同一个 server 从已登录/未涉及变成需要登录时发一次 |
| `mcp_auth_completed` | `{server}` | 登录完成,令牌已存;随后是这个 server 的 `mcp_connected` / `mcp_connect_failed` |
| `mcp_auth_failed` | `{server, error}` | 登录失败:授权服务器在回调里报错、换令牌失败,或 10 分钟内没完成 |
| `import_detected` | `{sources:[{source,description}]}` | 启动时探测到还没处理过的可导入配置 |
| `session_created` | `{session_id, scene, project_root}` | 一个新会话进了内存 |
| `session_resumed` | `{session_id, scene, project_root}` | 一个已有的会话被恢复进内存 |
| `session_evicted` | `{session_id, reason}` | 会话被移出内存,transcript 还在,可以恢复。`reason`:`idle`(空闲超时)、`capacity`(池子满了,挤掉最久没动的)。正在跑一轮、或还有后台任务在跑的会话不会被驱逐;池子满了而个个都忙时,新会话照样建,池子暂时超出上限。再用被驱逐的会话时,先等它旧的运行体停下(`SessionEnd` hook 跑完,最多 5 秒)再重建,两者不会同时在跑 |
| `session_closed` | `{session_id, reason}` | 会话被关闭。`reason`:`closed`(`session.close` / `session.delete`)、`engine_stopped`(引擎在一轮中途退出) |
| `session_deleted` | `{session_id, parent_session_id?}` | 会话连同 transcript 被删除;级联删除的侧链带 `parent_session_id` |
| `turn_started` | `{session_id, turn_id, initiator}` | 一轮开始。`initiator`:`client`(某条连接的 `run_turn`)、`schedule`(定时任务,§6.7)、`goal`(目标的自动轮) |
| `turn_completed` | `{session_id, turn_id, initiator}` | 这一轮结束,无论成功、出错还是被中断 |
| `events_lagged` | `{missed}` | 这条连接读得太慢,有 `missed` 条通知没送到。需要的话用 `session.list` 等对账 |
| `project_trust_changed` | `{project_root, trust}` | `project.trust` 写完之后;`trust` 是写完后的状态(§3.6) |

会话与轮的事件让宿主**不必订阅每个会话**就知道哪些会话在内存里、哪些在跑 ——
包括不是它发起的轮(定时任务,或者别的客户端)。没有 `config_reloaded` /
`scene_activated` 之类的通知:场景激活、配置重载不会广播。

客户端必须忽略不认识的 `kind`,这张表只会往下长。

---

## 9. 错误码

| 码 | 名称 | 含义 |
|---|---|---|
| -32601 | METHOD_NOT_FOUND | 未知方法;或方法所属的模块没有编入这个构建,此时 `data.module` 指明模块(§6.1 `daemon.ping`) |
| -32602 | INVALID_PARAMS | 参数缺失/类型错/取值非法 |
| -32603 | INTERNAL_ERROR | 服务端内部错误(写盘失败、会话创建失败等) |
| -32000 | SESSION_NOT_FOUND | 会话不存在,或该方法要求它在内存里而它不在 |
| -32002 | ENGINE_ERROR | turn 执行期错误,`message` 带引擎错误码 |
| -32003 | UNAUTHORIZED | TCP / WebSocket 握手缺失或失败;Unix socket 上的对端是 daemon 自己起的进程(§5.1) |
| -32004 | SCENE_NOT_FOUND | 场景未注册,或(`session.create`)未激活 |
| -32006 | SCENE_MISMATCH | 会话记录的场景与这个 daemon 的不符 |
| -32009 | SCENE_HAS_ACTIVE_SESSIONS | `scene.deactivate` 被内存中的会话挡住 |
| -32010 | PROJECT_NOT_FOUND | `project_root` 不存在或不是目录 |
| -32012 | PROJECT_REQUIRED | 场景 `requires_project` 为真,但传了 `project_root: null` |
| -32014 | SIDECHAIN_TERMINAL | 侧链会话已终态收尾,不可 resume |
| -32015 | SESSION_BUSY | 该会话已有 turn 在跑,且 `on_busy` 是 `reject`(§5.3) |
| -32016 | PLUGINS_DISABLED | 插件子系统不可用 —— 被编译裁掉或被策略关闭 |
| -32017 | TOO_MANY_IN_FLIGHT | 这条连接同时在途的请求已达上限(§5.2) |
| -32018 | TASK_NOT_FOUND | `session.taskOutput`:引擎不知道这个任务(没跑过、结束得早已被忘掉,或引擎不在内存里) |
| -32019 | TASK_LIMIT | `session.startAgent`:会话的子 Agent 已到 `execution.max_concurrent_subagents`,或委派已到最深 |
| -32020 | MCP_SERVER_DENIED | `mcp.addServer` / `mcp.updateServer`:MCP 策略拒绝这个 server,没有写入任何东西;`mcp.auth.start` / `mcp.auth.logout`:这个 server 被策略拒绝,或(`start`)它的授权服务器不在 `allowed_authorization_servers` 里 |
| -32021 | MCP_AUTH_FAILED | `mcp.auth.*`:授权服务器或登录本身失败——发现、注册、PKCE、回调 `state` 不符、换令牌;`message` 可以直接给用户看 |
| -32022 | PROJECT_UNTRUSTED | `mcp.addServer` / `mcp.updateServer` / `config.setProvider`:目标项目未被信任(§3.6),写进它项目层的 server / provider 不会生效,所以什么都没写 |

还有两个常量在代码里定义了但**没有任何路径会发出**,客户端不必为它们写分支:
`PARSE_ERROR (-32700)` / `INVALID_REQUEST (-32600)`(解析不了的帧被静默丢弃,见
§1)。`-32001` 保留不用:会话数到顶时驱逐最久未用的空闲会话,不拒绝新建。

### 9.1 `error.data` 的形状

**只有三个错误码带结构化 `data`**,客户端可以依赖:

```jsonc
// SESSION_BUSY
{ "session_id": "S1", "current_turn_id": "t1" }
// 需要重建的调用(setPrompt、换厂商的 setModel)被后台任务挡住时,多一项
{ "session_id": "S1", "current_turn_id": null, "running_task_ids": [ "a1" ] }

// SCENE_MISMATCH
{ "session_id": "S1", "recorded_scene": "harnix", "requested_scene": "code" }

// SIDECHAIN_TERMINAL
{ "session_id": "S1c1", "parent_session_id": "S1", "final_state": "completed" }  // completed | failed
```

其余错误码**没有 `data`**,信息全在 `message` 里 —— 包括
`SCENE_HAS_ACTIVE_SESSIONS`(挡路的会话 id 是拼在 message 文本里的)、
`PROJECT_NOT_FOUND`、`PROJECT_REQUIRED`、`INVALID_PARAMS`。要展示给用户就直接展示
`message`,不要试图解析它。

---

## 10. 权限默认值

daemon 会话的默认权限模式是 **`default`(ask)**,不是放行一切。

工具调用触发询问时,daemon 推送 `session.event {kind:"prompt"}` 并**阻塞该次工具
调用**,直到:收到 `session.respondToPrompt`;或超过 `--permission-prompt-timeout`
(默认 300 秒)→ **按拒绝处理**;或会话被关闭 → 按拒绝处理。

超时**永远失败关闭**,不会自动放行 —— 无人应答不等于同意。

模型的提问(`prompt_type:"question"`,§7)用同一个 `--permission-prompt-timeout` 计时(0 为
不限);超时、会话关闭或宿主 `decline`,模型都从 `AskUserQuestion` 的结果里读到用户没有回答,
不会把沉默当成回答。子 Agent 没有 `AskUserQuestion`。

**子 Agent 的询问在父会话上问。** 子 Agent(模型起的、`session.startAgent` 起的、前台
的、后台的)的工具调用照父会话的权限判;要问时,父会话推一帧 `prompt`,`message` 以
`[sub-agent <侧链会话 id>]` 开头。用 `session.respondToPrompt` 回答,`session_id` 填
**父会话**,不是侧链会话。父会话空闲时这帧照样推(`turn_id` 为 `null`,§7)。等回答
期间子 Agent 的任务是 `waiting`,`waiting_for` 里有这个 `prompt_id`(§6.3 `session.tasks`、
§7 `task_updated`)。超时与关闭的规则同上。

`permission_mode: "bubble"` 不是另一条路:子 Agent 的询问无论它是什么模式,到宿主的都是
上面这条父会话的 `prompt` 帧,团队成员也一样。判的模式是子 Agent 自己的:agent 类型的 `permission_mode` 可以比父会话宽(插件带来的类型
只能更严),团队的 `permission_mode` 只能更严;父会话在 `plan` 时取两者中更严的那个,父会话
是 `bypassPermissions` 时随父会话。它的询问等多久同父会话的(`--permission-prompt-timeout`);答 `permit_always`
记下的是这一条命令或路径。

宿主自己已做沙箱、不需要第二层权限时显式声明放行:任意层 settings.json 的
`"permission_mode": "bypassPermissions"`(daemon 范围),或 `options.permission_mode:
"bypassPermissions"`(单会话范围)。

会话级只能**收紧**、不能放宽,除非 settings 里开了 `allow_client_permission_override`
(§4.5)。

---

## 11. 从连接到跑完一轮

### 11.1 最小客户端骨架

帧格式是 NDJSON,一行一个 JSON 对象。下面覆盖了最容易写错的三件事:发请求、按 `id`
收响应、把其余的行当事件分发出去。

```ts
import * as net from "node:net";
import * as readline from "node:readline";

class DaemonClient {
  private socket: net.Socket;
  private nextId = 1;
  private pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();
  public onEvent: (method: string, params: any) => void = () => {};

  constructor(socketPath: string) {
    this.socket = net.createConnection(socketPath);
    readline.createInterface({ input: this.socket })
      .on("line", (line) => this.handleLine(line));
  }

  private handleLine(line: string) {
    if (!line.trim()) return;
    const msg = JSON.parse(line);
    // 有 id 且带 result/error → 是某个 call() 的响应;否则是推送帧。
    if (msg.id !== undefined && ("result" in msg || "error" in msg)) {
      const p = this.pending.get(msg.id);
      if (!p) return;                       // 未知 id:丢弃,不抛错
      this.pending.delete(msg.id);
      if (msg.error) p.reject(Object.assign(new Error(msg.error.message), msg.error));
      else p.resolve(msg.result);
    } else {
      this.onEvent(msg.method, msg.params); // session.event / daemon.event
    }
  }

  call(method: string, params: unknown = {}): Promise<any> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.socket.write(JSON.stringify({ jsonrpc: "2.0", method, params, id }) + "\n");
    });
  }
}
```

`pending` 必须是按 `id` 索引的 map,不能当队列使 —— 响应会乱序回来(§5.2)。
TCP / WebSocket 上,第一件事是 `call("daemon.auth", {token})`。

### 11.2 走一遍

```jsonc
// 1. 发现:读 ~/.agix/daemon/instances.d/*.json,联合判活后连它的 socket

// 2. 看有哪些场景,决定要不要弹"选择项目"
→ {"jsonrpc":"2.0","method":"scene.list","id":1}
← {"jsonrpc":"2.0","id":1,"result":{"scenes":[
     {"scene":"code","name":"Agix Code","description":"…","active":true,
      "sessions":0,"requires_project":true,"supports_team":true}]}}

// 3. 订阅 daemon 级通知(可选)
→ {"jsonrpc":"2.0","method":"daemon.subscribeEvents","id":2}
← {"jsonrpc":"2.0","id":2,"result":{"subscribed":true}}

// 4. 建会话
→ {"jsonrpc":"2.0","method":"session.create","id":3,
   "params":{"scene":"code","project_root":"/Users/x/repo-a"}}
← {"jsonrpc":"2.0","id":3,"result":{"session_id":"S1","scene":"code",
   "session_kind":"primary","project_root":"/Users/x/repo-a",
   "created_at":"2026-08-11T10:00:00Z"}}

// 5. 跑一轮。事件帧和最终响应走同一条连接。
→ {"jsonrpc":"2.0","method":"session.run_turn","id":4,
   "params":{"session_id":"S1","turn_id":"t1","message":"看看 permissions crate"}}

← {"jsonrpc":"2.0","method":"session.event","params":{"session_id":"S1","turn_id":"t1",
   "event":{"kind":"text_delta","text":"我先看一下"}}}
← {"…":"…","event":{"kind":"tool_use","id":"tu1","name":"Grep","input":{"pattern":"…"}}}
← {"…":"…","event":{"kind":"tool_result","id":"tu1","name":"Grep",
   "content":"…","is_error":false}}

// 6. 需要授权时:提问是事件,回答是一次普通调用,在同一条连接上发就行(§5.2)
← {"…":"…","event":{"kind":"prompt","prompt_type":"permission","prompt_id":"p1",
   "tool_name":"Bash","message":"运行 cargo test?","paths":[]}}
→ {"jsonrpc":"2.0","method":"session.respondToPrompt","id":5,
   "params":{"session_id":"S1","prompt_id":"p1","decision":{"type":"permit"}}}
← {"jsonrpc":"2.0","id":5,"result":{"prompt_id":"p1"}}

// 7. 结束。stop_reason 和 usage 在事件里,不在最终响应里。
← {"…":"…","event":{"kind":"turn_complete","stop_reason":"end_turn","api_calls":3,
   "usage":{"input_tokens":12345,"output_tokens":678,
             "cache_creation_input_tokens":2048,"cache_read_input_tokens":30720}}}
← {"jsonrpc":"2.0","id":4,"result":{"session_id":"S1","turn_id":"t1",
   "name":null,"api_calls":3}}

// 8. 会话还开着时:查它派生过哪些子 Agent(走磁盘,daemon 重启也有效)
→ {"jsonrpc":"2.0","method":"session.list","id":6,"params":{"parent_session_id":"S1"}}
← {"jsonrpc":"2.0","id":6,"result":{"sessions":[
   {"session_id":"S1c1","name":null,"preview":null,"message_count":0,
    "created_at":"","last_active":"","status":"inactive",
    "session_kind":"sidechain","parent_session_id":"S1","resumable":false}]}}

// 9. 读那个子 Agent 的内容 —— 和主线会话同一个 API
→ {"jsonrpc":"2.0","method":"session.history","id":7,"params":{"session_id":"S1c1"}}

// 10. 删除对话(级联删侧链),先预演
→ {"jsonrpc":"2.0","method":"session.delete","id":8,
   "params":{"session_id":"S1","dry_run":true}}
← {"jsonrpc":"2.0","id":8,"result":{"session_id":"S1","deleted":false,
   "sidechains_deleted":1,"sidechain_ids":["S1c1"]}}
```

### 11.3 错误路径

```jsonc
// A. 会话忙 —— 用户在生成过程中又点了发送
← {"jsonrpc":"2.0","id":10,"error":{"code":-32015,"message":"session is busy",
   "data":{"session_id":"S1","current_turn_id":"t1"}}}
// 处理:先 session.interrupt 再重发。UI 应该在 turn_state != "idle" 时置灰发送键,
//       让这个错误根本不会发生。

// B. 场景需要项目但传了 null
→ {"jsonrpc":"2.0","method":"session.create","id":11,
   "params":{"scene":"code","project_root":null}}
← {"jsonrpc":"2.0","id":11,"error":{"code":-32012,
   "message":"scene `code` requires a project; pass project_root"}}
// 处理:先看 scene.list 的 requires_project,该弹项目选择框就弹。

// C. 恢复到了错的场景 —— 最该被拦住的一类
← {"jsonrpc":"2.0","id":12,"error":{"code":-32006,"message":"scene mismatch",
   "data":{"session_id":"S9","recorded_scene":"harnix","requested_scene":"code"}}}
// 处理:用 data.recorded_scene 找到对的 daemon(或先 scene.activate)重试。
//       不要当普通失败吞掉 —— 它意味着 UI 里那条会话挂在了错误的场景下。

// D. 停用场景被会话挡住
← {"jsonrpc":"2.0","id":13,"error":{"code":-32009,
   "message":"scene harnix has active sessions: s1, s2"}}
// 处理:没有结构化 data,会话 id 在 message 文本里。想稳妥就自己用 session.list
//       找出该场景的会话逐个 close 后重试。

// E. 权限提问超时 —— 注意这不是 RPC 错误
← {"…":"…","event":{"kind":"prompt","prompt_type":"permission","prompt_id":"p9",…}}
// (客户端 300 秒内没有调 session.respondToPrompt)
← {"…":"…","event":{"kind":"tool_result","id":"tu9","name":"Bash","is_error":true,…}}
← {"…":"…","event":{"kind":"turn_complete","stop_reason":"end_turn",…}}
// turn 正常走完,只是那次工具调用被拒。失败关闭,永不自动放行。
```

### 11.4 集成自检清单

- [ ] 发现逻辑用 `(pid, pid_start_time)` 联合判活,不是只比 `pid`(§2)
- [ ] 响应按 `id` 匹配,不假设先发先回(§5.2)
- [ ] `tool_result` 按 `id` 配对渲染,不按数组下标(§5.5)
- [ ] `thinking` / `redacted_thinking` 块转发时原样保留(§4.1)
- [ ] 发送按钮看 `session.get` 的 `turn_state`,不靠等 `SESSION_BUSY`(§5.3)
- [ ] `stop_reason` / `usage` 从 `turn_complete` 事件取,不是从 `run_turn` 的响应取
- [ ] 断线重连按 subscribe → history → 补帧的顺序,不颠倒(§5.6)
- [ ] 权限提问超时后 UI 能显示"已拒绝",不会一直转圈(§10)
- [ ] 关闭对话前想清楚要不要保留子 Agent 侧链记录 —— `session.close` 会删掉它们(§3.3)
