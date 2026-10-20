# AttaCore Daemon RPC 接口

daemon(`attacored`)把一个多会话 Agent 引擎池暴露成 JSON-RPC 2.0 服务。
本文回答"怎么用它":怎么连上、有哪些方法、事件怎么读、错误怎么分辨。

**协议版本:2**(`daemon.ping` 的 `protocol_version`,以及发现文件里的同名字段)。

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
daemon 的真实响应逐字段比对;`daemon.doctor` 的例子由 `daemon/src/doctor.rs` 的
`documented_shape_tests` 比对。文档里写了的方法必须可调用、可调用的方法必须写进
§6,由 `daemon/tests/protocol_doc_matches_dispatch.rs` 保证。

---

## 1. 传输与编码

**帧格式**:每行一个 JSON 对象,`\n` 分隔(NDJSON)。不使用 Content-Length 头,
`tail -f` / `nc` 可以直接调试。WebSocket 例外:它本身就是消息分帧的,
**一条 text 消息 = 一帧**,不加 `\n`。

| 传输 | 怎么开 | 认证 |
|---|---|---|
| Unix socket | 默认开(`--socket <path>` 可改) | 文件权限 |
| TCP | `--listen <addr>` | `daemon.auth` 握手,见 §5.1 |
| WebSocket | `--listen-ws <addr>` | `daemon.auth` 握手 + Origin 校验,见 §5.1 |

TCP 与 WebSocket 都要求 `--token` 或 `ATTACORE_DAEMON_TOKEN`;没有 token 时监听器
**启动即失败**,不会先起来再拒绝所有人。

**分帧之上的一切都与传输无关** —— 方法、参数、事件帧、权限提问三条路完全一致,
换传输不需要改协议层代码。

编码 UTF-8。**单帧上限 16 MiB**,三条传输都强制。超限的连接被**关闭**而不是跳过
该帧:超限之后服务端已经不知道下一个帧边界在哪。

**请求**:

```jsonc
{ "jsonrpc": "2.0", "method": "session.create", "params": { }, "id": 1 }
```

`id` 省略 = 通知,服务端不回响应。`params` 省略等价于 `{}`。

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

daemon 启动时在 `~/.atta/daemon/instances.d/` 下写自己那一份实例文件,正常退出时
删除。**每个实例一个文件,没有共享写点**,多个 daemon 同时启动不会因为并发
read-modify-write 同一个索引文件而丢更新。读取方 `read_dir` 汇总。

```
~/.atta/daemon/instances.d/desktop.json
~/.atta/daemon/instances.d/ci-runner.json
```

```jsonc
{ "instance": "desktop",
  "pid": 4210,
  "pid_start_time": 1754899200,
  "socket": "/Users/x/.atta/daemon/desktop.sock",
  "tcp": "127.0.0.1:7878",              // 仅在 --listen 时出现
  "ws": "127.0.0.1:7879",               // 仅在 --listen-ws 时出现
  "scenes": ["coding", "chat", "research"],
  "protocol_version": 2,
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

daemon 另外还写一份 `~/.atta/scenes/<scene>/daemon.lock`(`{pid, pid_start_time,
socket_path, version, started_at, protocol_version}`)。它早于多场景支持,只描述
一个场景,新客户端用 `instances.d/`。

**socket 路径**(daemon 自己启动时的推导规则,列出仅供排查):

```
--socket <path> 指定        → 用它
否则                        → $HOME/.atta/scenes/<scene>/daemon.sock
```

(Windows 上没有 Unix socket,daemon 改用命名管道 `\\.\pipe\attacore-daemon`。)

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

| 能力 | 含义 | coding | research | chat | demo |
|---|---|---|---|---|---|
| `requires_project` | 会话必须绑定项目根 | ✅ | ✅ | ❌ | ❌ |
| `supports_team` | 注册 Team 工具 | ✅ | ✅ | ❌ | ❌ |

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

  级联删除认的是 `Meta` 里的 `parent_session_id`,**不看 `session_kind`** ——
  `session.fork` 出来的会话也记着源会话作父,所以它同样会被上表的规则删掉
  (见 `session.fork`)。

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
| `acceptEdits` | 自动放行文件写入,其余询问 |
| `auto` | 依赖会话分类器 —— 程序专用,同上 |
| `yolo` | 激进自动放行 |
| `bypassPermissions` | 跳过全部权限检查 |

表格是按**宽松程度从严到宽**排的,这个顺序就是会话级取值被夹紧的依据:会话只能
比 settings 更严,不能更宽,除非 settings 里开了 `allow_client_permission_override`。

**越界不报错。** 经 RPC 传进来的 `permission_mode` 无论是什么,都不会被拒绝:比
settings 宽的被**静默夹到 settings 允许的最宽处**并记一条 warn 日志,程序专用的两个
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

`rule_content` 的写法按工具而定:`Bash` 用命令前缀模式(`"git push:*"`),
文件类工具用 glob(`"/etc/**"`)。

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

### 4.7 `PermissionDecision` — 回答权限提问

`session.respondToPrompt` 的 `decision`,四种形态:

```jsonc
{ "type": "permit" }
{ "type": "deny", "reason": "…" }
{ "type": "permit_always", "scope": "session" }   // 本会话内一直允许
{ "type": "permit_always", "scope": "local" }     // 另外写进 .atta/settings.local.json
```

### 4.8 `stop_reason`

出现在 `turn_complete` 事件和 transcript 的 assistant 消息上:

| 值 | 含义 |
|---|---|
| `end_turn` | 模型正常结束 |
| `max_tokens` | 触到单次输出上限 |
| `tool_use` | 模型请求工具(轮内中间态) |
| `stop_sequence` | 命中停止序列 |
| `pause_turn` | 模型请求暂停 |
| `max_turns_reached` | 触到本轮 API 调用次数上限(引擎自己发的,不来自 API) |
| `unknown` | 反序列化兜底,遇到未知取值时的落点 |

---

## 5. 连接、认证、并发与顺序

### 5.1 认证

TCP 与 WebSocket 需要,两者用**同一个 token、同一段校验代码**。连接建立后的
**第一帧**必须是:

```jsonc
{ "jsonrpc":"2.0", "method":"daemon.auth", "params":{"token":"…"}, "id":0 }
← { "jsonrpc":"2.0", "id":0, "result":{"authenticated":true} }
```

第一帧不是 `daemon.auth`、token 不对、或者根本不是合法 JSON → `UNAUTHORIZED
(-32003)`,然后关连接(WebSocket 会先发 close 帧再断,浏览器端拿到的是"被拒绝"
而不是"连接出错")。token 比对是定长时间比较。

认证粒度是 **daemon 级**,不按场景或会话签发 —— 一条通过认证的连接可以访问这个
daemon 的全部场景与会话,而且**没有任何逐方法的授权**。特别地,
`config.setProvider` 能把后续所有模型流量指向调用方给的 `base_url`/`api_key`;
把 socket 或 token 暴露出去,等同于暴露模型凭据本身。

Unix socket 靠文件系统权限,不握手,一样是"能连上就全权"。

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

**一个会话同一时刻只能有一个 turn。** 第二个 `run_turn` 立刻失败:

```jsonc
{ "error": { "code": -32015, "message": "session is busy",
             "data": { "session_id": "S1", "current_turn_id": "t1" } } }
```

**不排队、不抢占**。客户端要发新消息必须先 `session.interrupt`。UI 的"发送"按钮
应该在 `session.get` 的 `turn_state != "idle"` 时置灰,让这个错误根本不出现。

注意错误发生**在用户消息被投递之前** —— 消息不会被前一个 turn 吃掉。

### 5.4 事件帧推给谁

`session.event` 帧推给**所有订阅了该会话的连接**。

**连接不是会话的所有者。** 一个会话可以被任意多条连接同时观看,它们收到同一份帧流。
发起 `run_turn` 的那条连接没有特殊地位,它只是被自动加进订阅者集合而已。

| 订阅来路 | 说明 |
|---|---|
| `session.subscribe` | 显式订阅 |
| `session.run_turn` | 发起方自动订阅 |

退订用 `session.unsubscribe`;连接断开时它的全部订阅一起消失,**别的什么都不动**。

**关于慢客户端**:每条连接前面有一个 1024 帧的出站队列。填满之后,广播给它的帧被
丢弃(连接本身不动)—— 一个卡住的 tab 不能拖住 turn,也不能拖住其他正在看的 tab。
被丢掉的一方重新 `session.subscribe` 并靠 `last_seq` 追平(§5.6)。

`daemon.event`(§8)是另一条通道:与会话订阅无关,推给所有
`daemon.subscribeEvents` 的订阅者。

### 5.5 事件顺序保证

| 保证 | 说明 |
|---|---|
| 同一 turn 内 `text_delta` 严格有序 | 按到达顺序拼接即得完整文本 |
| 每个 `tool_result` 必有先行的同 `id` `tool_use` | 反之不然:被取消的工具可能没有 result |
| `turn_complete` 是本 turn 最后一个事件 | 其后只有 `run_turn` 的最终响应 |
| 每个开始了的 turn 都有且只有一个 `turn_complete`(出错时是 `run_turn` 的错误响应) | 被 hook 拦下、被中断、被预算或策略截停的 turn 同样以它结束 |
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
可闭合。`last_seq` 是 transcript 的条目数;日志 append-only,条目位置稳定,所以
位置本身就是序号。

**没有"续接流"**:重连不会补发上一个 turn 已经推过的帧。已产生的内容都在 transcript
里,用 `session.history` 取 —— 追赶靠历史,不靠重放。

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
{ "pong": true, "protocol_version": 2 }
```

#### `daemon.status`

无参。

```jsonc
// ← result
{ "version": "…",         // daemon 二进制版本
  "uptime_secs": "…",     // 数字,秒
  "sessions": 0 }         // 当前内存里的会话数(含侧链)
```

#### `daemon.doctor`

只读自检:三份 settings.json 各自的状态、provider 路由的解析结果、hooks 配置,
外加这个进程的几项装配事实。不做任何修复,也不发起任何网络探测 —— `HealthCheck`
契约要求每项检查从已经握着的状态立刻回答。

下面是一个全新装配、三份 settings.json 都还不存在时的完整响应,逐字段为真
(`path` 因机器而异,其余是字面值):

```jsonc
{ "scene": "coding",
  "health": {
    "status": "ok",                   // ok | degraded | failing,取所有检查里最差的一项
    "checks": [                       // 每项的 details 与下面同名的顶层字段是同一份数据
      { "name":"settings.tiers",     "status":"ok", "summary":"every settings.json present parses" },
      { "name":"settings.providers", "status":"ok", "summary":"provider routing resolves" },
      { "name":"settings.hooks",     "status":"ok", "summary":"hooks configuration parses" } ] },
  "settings_tiers": [
    { "tier":"global",  "path":"…/settings.json",       "exists":false, "parses":true, "error":null },
    { "tier":"scene",   "path":"…/settings.json",       "exists":false, "parses":true, "error":null },
    { "tier":"project", "path":"…/.atta/settings.json", "exists":false, "parses":true, "error":null } ],
  "providers": { "ok":true, "default_provider":null, "configured":[], "task_models":[],
                 "warnings":[], "error":null },
  "hooks": { "configured":false, "ok":true, "error":null },
  "session_persistence": { "history_store_wired":true },
  "plugins": { "status":"enabled" },
  "permission_rules_count": 0,
  "model": { "model_name":"claude-sonnet-4-6", "api_type":"Anthropic" } }
```

`health` 是 `HealthCheck` 契约(`docs/extension_points.md`)的报告:宿主注册的检查
与引擎自带的三项并排出现在同一个 `checks` 里。状态是三值而不是布尔 ——
`degraded`("还能用,但有人该知道")与 `failing`("这块不能指望了")是两件事。

`settings_tiers` / `providers` / `hooks` 是 `health.checks` 里同名检查 `details`
的副本,放在顶层是因为现有客户端读的是它们。

`providers.ok` 只说明配置**形状**解析得通(每个 `task_models` 条目都指向一个存在
的 provider),不说明真能建出客户端,更不说明那个端点能连上 —— 缺凭据、`base_url`
指向一个没人监听的地址,这里都仍然是 `ok: true`。这项检查从已经握着的设置立刻回
答,不发起任何网络探测。

`plugins.status` 是这个二进制**能不能加载插件**:`enabled`、`disabled-by-policy`
(编进来了但配置关掉了)、`compiled-out`(根本没编进来)。

#### `daemon.subscribeEvents`

无参。把当前连接标记为异步通知订阅者,此后推送 §8 的 `daemon.event` 帧,直到连接
关闭。**不补发过去的事件** —— 关心启动期通知的客户端要在连上后立刻订阅。

```jsonc
// ← result
{ "subscribed": true }
```

#### `daemon.shutdown`

无参(传什么都一样)。关掉所有会话的运行体并让进程退出。

```jsonc
// ← result
{ "shutting_down": true }
```

**不删除任何 transcript,包括侧链的**(§3.3)。也没有"优雅等待"选项:正在跑的 turn
被直接取消。

### 6.2 `scene.*`

#### `scene.list`

无参。列出这个二进制注册的全部场景,包括未激活的。

```jsonc
// ← result
{ "scenes": [
    { "scene":"coding", "name":"AttaCode Coding", "description":"…",
      "active":true, "sessions":0,
      "requires_project":true, "supports_team":true } ] }
```

`sessions` 是当前内存里挂在这个场景下的会话数。**数组顺序不稳定**(底层是
HashMap),客户端要按 `scene` 找条目,不要按下标。

#### `scene.activate`

```jsonc
// → params
{ "scene": "chat" }
// ← result
{ "scene": "chat", "active": true }
```

幂等。场景 id 不是这个二进制注册的 → `SCENE_NOT_FOUND`。

#### `scene.deactivate`

```jsonc
// → params
{ "scene": "chat" }
// ← result
{ "scene": "chat" }
```

该场景下还有**内存中的**会话时失败,`SCENE_HAS_ACTIVE_SESSIONS`,`message` 里列出
挡路的会话 id(没有结构化 `data`)。调用方先 `session.close` 再重试,不提供 `force`。

停用只是把场景移出激活集合,**不删除磁盘上任何内容**;它的历史会话仍然列得出来,
重新激活后仍可 resume。

#### `scene.describe`

```jsonc
// → params
{ "scene": "coding", "project_root": null, "include_secrets": false }
// ← result
{ "scene": "coding", "name": "AttaCode Coding", "description": "…",
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
`password` 这些键名的值一律脱敏为末 4 位(任意嵌套深度)。

### 6.3 `session.*`

#### `session.create`

```jsonc
// → params
{ "scene": "coding",                      // 可选,默认 daemon 的 --scene
  "project_root": "/Users/x/repo-a",      // 见 §3.1 的三态
  "options": {                            // 可选,只在创建时生效
    "permission_mode": "default",
    "permission_rules": [ ],
    "recorder": { "mode":"replay", "name":"…", "dir":"…", "strict":true },
    "telemetry": { "output":"/abs/path.jsonl" } } }
// ← result
{ "session_id": "…", "scene": "coding", "session_kind": "primary",
  "project_root": "…", "created_at": "…" }
```

`scene` 必须是**当前激活**的场景,否则 `SCENE_NOT_FOUND`。场景的 `requires_project`
为真而 `project_root` 是 `null` → `PROJECT_REQUIRED`;给了路径但目录不存在 →
`PROJECT_NOT_FOUND`。

`options` 只在真正新建时生效,之后改不了。**形状不认识的 `options` 会被整体当成
"没传"**,不报错 —— 拼错字段名不会有任何提示。

`recorder.mode` 取 `record` / `replay` / `rerun`;`dir` 省略时用 daemon 自己的
录制根目录。

#### `session.run_turn`

**流式方法**:最终响应之前会在同一连接推送 0..N 个 `session.event` 帧(§7)。
并发约束见 §5.3,顺序保证见 §5.5。

```jsonc
// → params
{ "session_id": "…",              // 可选!省略则自动新建一个会话
  "message": "把权限层重构一下",     // 必填
  "turn_id": "t-1",               // 可选,省略则服务端生成
  "attachments": [ ],             // 可选,见 §4.4
  "options": { } }                // 可选,同 session.create,仅新建/重建时生效
// ← result
{ "session_id": "…", "turn_id": "…",
  "name": null,                   // 首轮自动命名的结果,场景不开启时为 null
  "api_calls": "…" }              // 数字:本轮打了几次模型 API
```

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
  "scene": "coding", "project_root": "…",   // null = 无项目会话,或老日志没记
  "scene_active": true,
  "turn_state": "idle",            // idle | running
  "current_turn_id": null }
```

比 `session.list` 的条目多 `scene_active` / `turn_state` / `current_turn_id` 三项 ——
第一个要问 daemon 当前激活了哪些场景,后两个只有内存里的池子知道。`turn_state` 就是
"发送按钮该不该置灰"的依据。

`scene` / `project_root` 与 `session.list` 的条目**同源**(都是那一条 `SessionInfo`),
所以两个方法对同一个会话不会给出不同的答案。

**`scripts`——这个会话的脚本都干了什么。** 只有绑了脚本的会话才有这个键;没绑的会话
**没有这个键**,而不是一个空对象——「这儿没有脚本」和「脚本一次都没跑」是两个不同的
答案。

```jsonc
"scripts": {
  "calls": 7, "applied": 5, "no_change": 1, "failed": 1, "refused": 0,
  "dropped": 0,                    // 非 0 表示 recent 只是尾巴
  "recent": [                      // 最近 20 条,新的在前
    { "point": "prompt.assemble", "script": "/proj/.atta/scripts/x.js",
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
      "scene": "coding", "project_root": "…", "compact_count": 0 } ] }
```

返回内存中的会话 + 磁盘上的历史会话,合并去重,**不分页、不过滤场景**。
没有 `scene` / `project_root` / `status` / `limit` / `cursor` 这些**参数** —— 传了
会被忽略;但每个**条目**都带 `scene` 与 `project_root`,按项目或场景分组在客户端一次
遍历就能做完,不用逐个 `session.get`。

冷会话的范围是**这个 daemon 服务过的项目**:它自己的项目,加上每个被
`session.create {project_root}` 绑定过的项目。不是磁盘上的全部项目 —— 那会报出
别的 daemon、别的工作目录的会话。daemon 重启后绑定表是空的,一个项目在有会话落到
它上面之前不出现在列表里;活跃会话不受影响,它们本来就在内存里。

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

`resume: true` 不接 `options`:要指定 `permission_mode` 之类的恢复,显式调
`session.resume`。恢复失败的错误跟 `session.resume` 完全一致 —— 跨场景仍然
`SCENE_MISMATCH`(§3.4),终态侧链仍然 `SIDECHAIN_TERMINAL`,池子满了仍然按恢复那边
的规矩来。

`pending_prompts` 里的帧带**原来的 `prompt_id`**,所以中途打开的 tab 可以直接答那个
还悬着的权限提问。这些帧在 turn 结束时清空。

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
错误 —— 调用方要的是会话空闲,而它确实空闲。

#### `session.rename`

```jsonc
// → params
{ "session_id": "…", "name": "重构 daemon 装配" }   // name 为 null = 清除
// ← result
{ "session_id": "…", "name": "重构 daemon 装配" }
```

会话标题是**使用者的东西**,所以它写在会话自己的事实里(`~/.atta` 下的会话
sidecar),不是某个宿主自己的一层覆盖 —— 各宿主各存一份的结果是同一个会话在这边
叫一个名字、在那边叫另一个,共用 `~/.atta` 的意义就打了折。`session.list` /
`session.get` 的 `name` 就是这里写进去的值。

**冷会话也能改名**:命名是关于会话的事实,不是对运行中引擎的操作,所以它不会为了
改个名字把会话恢复起来。

`name` 传 `null`(或者不传这个键)= 清除,会话回到"自动命名"的状态。非空、最长
200 字符、不接受控制字符 —— 这是要显示在别人列表里的一行字。

**用户改过的名字不会被自动命名覆盖**:开了自动命名的场景在首轮结束后会生成一个
名字,但它跳过已经被 `session.rename` 命名过的会话。

#### `session.setModel`

```jsonc
// → params
{ "session_id": "…", "model": "claude-opus-4-6" }
// ← result
{ "session_id": "…", "model": "claude-opus-4-6", "applies": "next_turn" }
```

换掉这个会话**下一次模型调用**用的模型。transcript 不动 —— 这是换"谁来回答下一
条",不是改写已经说过的话。所以"这一段用便宜的,接下来换个强的"不再需要新建会话,
也就不再需要丢掉上下文。

**换的是模型名,不是 provider。** 走的还是这个 daemon 当初建起来的那个 client;
跨 provider 的切换要配 `providers` / `task_models`(§`config.setProvider`),不是
这个方法能做的事。

**有 turn 在跑时返回 `SESSION_BUSY`**,不排队。引擎跑 turn 的时候不读输入通道,
排进去的命令会在某个没人宣布的时刻生效 —— 调用方被告知模型换了,却看着旧模型答完
这一轮。要换就先 `session.interrupt`,或者等这一轮结束。

**只对内存里的会话有效**,冷会话返回 `SESSION_NOT_FOUND`(先 `session.resume`)。
选择跟着这个 daemon 活着:会话被驱逐后重建、`config.reload` 之后重建,选的还是这个
模型;**daemon 重启则回到配置里的模型** —— 重启是看得见的事件,驱逐不是。

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
  "options": { } }              // 可选,同 session.create
// ← result
{ "session_id": "…",
  "status": "already_active",   // already_active | resumed | created
  "active": true,
  "message_count": "…", "entry_count": "…",
  "name": null,
  "created_at": "…", "last_active": "…",
  "scene": "coding",
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

> ⚠️ **分叉出来的会话会被算成源会话的子会话。** 级联删除只看 `Meta` 里的
> `parent_session_id`,不看 `session_kind`,所以对**源会话**调 `session.close` 或
> `session.delete` 会**连带删掉这个分叉的 transcript**;`session.list
> {parent_session_id}` 也会把它列出来,而且把它标成 `"session_kind":"sidechain"`。
> 想留住分叉,就别关它的源会话。

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
(§3.3)。仍在跑的先取消再删;删除失败只写日志,不会让 `close` 失败。

#### `session.delete`

```jsonc
// → params
{ "session_id": "…", "dry_run": false }
// ← result
{ "session_id": "…", "deleted": true,
  "sidechains_deleted": 0, "sidechain_ids": [ ] }
```

**真正删除磁盘 transcript,并级联删除记着它作父会话的全部会话(侧链,以及它的
分叉)。不可逆。**

`dry_run: true` 只返回将要删除的清单(`deleted` 为 `false`),不实际删除 —— 供宿主
做二次确认。

#### `session.respondToPrompt`

```jsonc
// → params
{ "session_id": "…", "prompt_id": "…",
  "prompt_type": "permission",          // 可选,默认也是唯一取值
  "decision": { "type": "permit" } }    // 见 §4.7
// ← result
{ "prompt_id": "…" }
```

**谁都能答,先答者生效。** 提问广播给该会话的全部订阅者,任何一条连接都可以回答,
不必是发起 turn 的那条。`prompt_id` 未知(已超时、已被别人答掉)时**静默成功** ——
调用方要的是"这个提问被答掉",而它确实被答掉了。

会话不在内存里 → `SESSION_NOT_FOUND`(这个和 `session.close` 不一样:答一个没在跑
的会话的提问不可能是"已经完成了")。

未在 `--permission-prompt-timeout`(默认 300 秒)内回答的提问**失败关闭**:按拒绝
处理,turn 带着一条 error `tool_result` 继续,不会挂死。

### 6.4 `config.*`

#### `config.get`

```jsonc
// → params
{ "scene": "coding", "project_root": null,
  "tier": "effective",            // global | scene | project | effective,默认 effective
  "include_secrets": false }
// ← result
{ "scene": "coding", "project_root": null, "tier": "effective",
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
    "mcp_connected": [ ], "mcp_failed": [ ] } }
```

`ok` 与 `router_rebuilt` 是**两件事**:配置可以解析得通(`ok: true`)却建不出客户端
(`router_rebuilt: false` + `router_error`),`api_type: openai_compatible` 就是这种。

```jsonc
// → params(都可选)
{ "scene": "coding",            // 省略 = 这个 daemon 的默认场景
  "project_root": "/Users/x/repo-b" }   // 省略 = 这个 daemon 自己的项目
```

**没有按层失效的参数**:`tier` 不存在,重读永远是"这一对 `(项目, 场景)` 当前磁盘上
的三层整个重读一遍"。缓存则是整个丢掉重建——全局层与场景层是每个项目共同的下层,
只失效一对是不够的。

`project_root` **不接受 `null`**:这个方法作用在**项目层**,而无项目会话没有项目层
(`config.get {tier: "project"}` 早就是这个答案)。传 `null` → `INVALID_PARAMS`。

失效**惰性生效**:已有会话在下一次 `run_turn` 之前若空闲则原地重建,忙碌的跳过本轮
下次再检查。**正在进行的 turn 不会被中断。**

**多数情况下不需要显式调用**:daemon 监视每个项目的 `settings.json` /
`settings.local.json`,改动落盘后自己重读(去抖 300ms)。这个方法留给两种情况——
监视起不来(权限、内核 inotify 上限;`daemon.doctor` 会报),以及**改动前那个文件
还不存在**:不存在的文件监视不了,新建出来的那一份要等到下一次绑定重建或者这里
被显式调用。

#### `config.getProvider` / `config.setProvider`

两者都接受 `scene` / `project_root` 定位参数,语义与 `config.reload` 一致:省略 = 这个
daemon 自己那一对,给路径 = 它服务的另一个项目,`null` → `INVALID_PARAMS`。
`setProvider` 写的是**被指定项目**的 `{project_root}/.atta/settings.json`,
`written_to` 回的就是那个路径。`mcp.addServer` 同样接受这两个参数。

**provider 路由是按 `(项目, 场景)` 解析的**:一个项目在自己的 settings 里配了
`providers` / `task_models`,它的会话就走它自己的路由,不是 daemon 那个项目的。

```jsonc
// → params (getProvider)
{ "scene": "coding",                      // 都可选,见上
  "project_root": "/Users/x/repo-b",
  "include_secrets": false }
// ← result (getProvider)
{ "providers": { },           // { "<id>": {api_type, base_url, api_key, default_model, models} }
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

响应是 `config.reload` 的结果再加一个 `written_to`(实际写入的文件路径)。

**总是写项目层的 settings.json**(daemon 默认项目下的 `.atta/settings.json`),
没有选层的参数。`config` / `task_models` 是部分补丁:只提到的键被覆盖,没提到的
保持原样。

### 6.5 `mcp.*` / `plugin.*` / `import.*` / `commands.*`

这一组方法**都只作用于 daemon 自己的默认项目和默认场景**,不接受定位参数。

#### `mcp.status`

无参。

```jsonc
// ← result
{ "servers": [ ] }    // 元素:{ "name":"…", "transport":"…", "tool_count": 0 }
```

#### `mcp.addServer`

```jsonc
// → params
{ "name": "fs",
  "config": { "type": "stdio", "command": "…", "args": [], "env": {} } }
```

`config` 是一个 `McpServerConfig`,按 `type` 标签联合:`stdio` / `streamable_http` /
`sse` / `websocket` / `in_process`。写进项目层 settings.json 并**立刻尝试连接**,
成功与否都会发一条 `mcp_connected` / `mcp_connect_failed` 的 `daemon.event`。

响应:`{ "written_to": "<路径>", "servers": [ <同 mcp.status 的元素> ] }`。
配置解析不了、名字为空、已有的 settings.json 不是合法 JSON → `INVALID_PARAMS`。

#### `plugin.list`

无参。

```jsonc
// ← result
{ "plugins": [ ] }
// 元素:{ "name":"…", "version":"…", "description":"…", "enabled": true,
//        "root": "/…/plugins/cache/<name>/<version>",
//        "script_faults": [], "component_faults": [] }
```

列出磁盘上的全部插件及启用状态,包括已停用的。

`root` 是解包后的目录。有它,宿主要读包里自己关心的东西(界面产物、图标)就不必自己
拼 `<plugins-dir>/cache/{name}/{version}/`——那样会把 daemon 的磁盘布局变成它 API 的
一部分。

`script_faults` 是这个包的 `[[script]]` 绑定里没能兑现的那些,每条带原因。来自包的绑定
是逐条降级的,所以这里非空不代表包没装上,只代表少了那几条贡献。

`component_faults` 是同一回事,换成 `[[wasm]]` 组件:哪个组件没能加载,以及为什么,
每条以组件文件名开头(一个包可以声明不止一个)。**这一条比上一条更要紧**——一个包
如果只有组件、而组件加载不了,它照样列在这里、照样 `enabled: true`、什么也不贡献,
而其他每个能问的问题都答"没事":装上了、启用着、也没被熔断——熔断数的是调用产生的
故障,而一个从没加载成功的组件从来不会被调用。

不带载体的构建这一项恒为空:它不加载组件,所以没有可报的。那种构建里"组件不会跑"
这件事由 `plugin.install` 的披露(`wasm.runnable`)说,不由这里说。

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

**`checksum` 对 `http(s)://` 源是必填的**,缺了会在发起请求之前就拒绝;`file://`
源可以省略。流程:取包 → 校验 → 解包 → **在安装时就把组件编译成 AOT 产物**;编译
失败会把这次安装**回滚**并报错,而不是留一个每次加载都要重编的插件。

**组件数为 0 的包不走编译这一步**,也就不需要任何编译器。纯脚本、纯 MCP 的包在
默认构建(`plugin-packages`,不带 WASM 载体)上装得上;带 `[[wasm]]` 的包也装得上,
disclosure 的 `wasm.runnable` 会如实报成 `false`。

响应:`{ "success": true, "message": "…", "disclosure": … }`。`disclosure` 是这个
插件会往模型上下文里塞的文本,让调用方在依赖它之前先看见。

#### `plugin.uninstall`

```jsonc
// → params
{ "name": "…", "version": "1.0.0", "scope": "global" }   // version 省略 = 全部版本
```

响应 `{ "success": true, "message": "…" }`。AOT 产物在版本目录下,一起带走。

#### `plugin.enable` / `plugin.disable`

```jsonc
// → params
{ "name": "…", "scope": "global" }
```

响应 `{ "name": "…", "enabled": true, "scope": "global" }`。启用状态按层记录,
不改动已安装的文件。

#### `plugin.reload`

无参。重扫插件目录并重新加载,响应与 `plugin.list` 同形。

上面四个方法各自会自动刷新,**这个是给它们盖不到的情况用的**:有人手工把插件放进
目录、或者开发时原地替换了组件。

**关于 `scope`**:只有 `global`(全局根下的 `plugins/`,跨场景共享)和 `scene`
(场景根下的,只对这个 daemon 的场景生效)两个值,默认 `global`,别的值返回
`INVALID_PARAMS`。

**什么时候生效**:装/卸/启用/停用/重扫**只影响之后新建的会话**。插件贡献的工具是在
会话创建时注入工具表的,已经在跑的会话保持原样。受影响的场景与斜杠命令会立即刷新,
所以 `scene.list` / `commands.list` 在调用返回之后就是新的。

> 插件子系统可以在编译期整个裁掉(daemon 的 `plugins` feature,**默认不开**)。
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

`agent.*`(列举/停止运行中的子 Agent)和 `team.*`(团队的列举/事件流)在早期设计里
定过接口,但**代码里没有对应的 dispatch 分支**,调用返回 `METHOD_NOT_FOUND`。

引擎里子 Agent 和团队都是有的,模型能通过 `Agent` / `TeamCreate` 等工具用;只是没有
RPC 出口。要看子 Agent 干了什么,走它的侧链会话:`session.list
{parent_session_id}` 拿 id,再 `session.history`。

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
| `text_delta` | `{text}` | 模型输出增量 |
| `tool_use` | `{id,name,input}` | 模型发起工具调用 |
| `tool_result` | `{id,name,content,is_error}` | 工具返回 |
| `prompt` | `{prompt_type,prompt_id,tool_name,message,paths[]}` | 需宿主决策,用 `session.respondToPrompt` 回答 |
| `subagent_progress` | `{agent_label,agent_session_id,agent_type,parent_turn,event}` | 子 Agent 事件镜像,`event` 是嵌套的本表结构 |
| `team_progress` | `{team,team_id,stage,stage_index,stage_count,status,members,failed}` | 团队阶段生命周期 |
| `skills_changed` | `{added,removed}` | 会话下的 skill 文件变了,缓存了 `commands.list` 的客户端该重取 |
| `compact` | `{strategy,messages_before,messages_after,dropped_rounds?,dropped_messages?,estimated_tokens_saved?}` | 上下文被压缩了(§7.1) |
| `turn_complete` | `{stop_reason,api_calls,usage,message?}` | 本轮结束,transcript 已落盘;最终响应随后到达 |

**这张表是白名单。** 引擎内部还有其它事件,但只有上面这些会被转成帧发出来。特别地:
没有 `turn_state` / `agent_state` 事件 —— 判断 turn 是不是在跑要问 `session.get` 的
`turn_state`。

**但客户端必须忽略不认识的 `kind`。** 这张表只会往下长,而且加一种事件不该是一次
破坏性变更:碰到没见过的 `kind` 就跳过它,不要报错、不要断开、更不要把整条连接当成
协议错误。

`prompt_type` 目前只有 `"permission"`,但字段刻意保持通用 —— 未来的"停下来问一句"
可以复用同一帧与同一回答通道。

### 7.1 `compact`

```jsonc
{ "kind":"compact", "strategy":"Snip",
  "messages_before":84, "messages_after":22,
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

后三项是策略相关的:给不出的策略就不带这三个键,不是带 `null`。

`turn_complete` 的 `usage` 是完整的四字段用量(§3.5),
**是这一轮所有模型调用的合计**,不是最后一趟。一轮可能来回好几趟
(`api_calls` 就是趟数),按最后一趟计费会少算——用它做预算的宿主尤其要看清这一点。

`message` 只在截停这一轮的一方给了理由时出现 —— 比如 hook 返回的 `message`;`stop_reason` 此时是 `stopped_by_hook`。

**`stop_reason` 和 `usage` 只在这里出现**,`run_turn` 的最终响应里没有。

---

## 8. 异步通知:`daemon.event`

推给所有调用过 `daemon.subscribeEvents` 的连接,不属于任何会话或 turn。

```jsonc
{ "jsonrpc":"2.0", "method":"daemon.event", "params": { "kind":"…" } }
```

| `kind` | 载荷 | 什么时候 |
|---|---|---|
| `mcp_connected` | `{server, transport, tool_count}` | 启动连接、`config.reload` 重连、`mcp.addServer` 成功 |
| `mcp_connect_failed` | `{server, error}` | 同上,失败时。具体原因在 daemon 日志里 |
| `import_detected` | `{sources:[{source,description}]}` | 启动时探测到还没处理过的可导入配置 |

**只有这三种。** 没有 `config_reloaded` / `scene_activated` / `session_evicted` 之类
的通知 —— 场景激活、配置重载、会话回收都不会广播。

---

## 9. 错误码

| 码 | 名称 | 含义 |
|---|---|---|
| -32601 | METHOD_NOT_FOUND | 未知方法 |
| -32602 | INVALID_PARAMS | 参数缺失/类型错/取值非法 |
| -32603 | INTERNAL_ERROR | 服务端内部错误(写盘失败、会话创建失败等) |
| -32000 | SESSION_NOT_FOUND | 会话不存在,或该方法要求它在内存里而它不在 |
| -32002 | ENGINE_ERROR | turn 执行期错误,`message` 带引擎错误码 |
| -32003 | UNAUTHORIZED | TCP / WebSocket 握手缺失或失败 |
| -32004 | SCENE_NOT_FOUND | 场景未注册,或(`session.create`)未激活 |
| -32006 | SCENE_MISMATCH | 会话记录的场景与这个 daemon 的不符 |
| -32009 | SCENE_HAS_ACTIVE_SESSIONS | `scene.deactivate` 被内存中的会话挡住 |
| -32010 | PROJECT_NOT_FOUND | `project_root` 不存在或不是目录 |
| -32012 | PROJECT_REQUIRED | 场景 `requires_project` 为真,但传了 `project_root: null` |
| -32014 | SIDECHAIN_TERMINAL | 侧链会话已终态收尾,不可 resume |
| -32015 | SESSION_BUSY | 该会话已有 turn 在跑(§5.3) |
| -32016 | PLUGINS_DISABLED | 插件子系统不可用 —— 被编译裁掉或被策略关闭 |
| -32017 | TOO_MANY_IN_FLIGHT | 这条连接同时在途的请求已达上限(§5.2) |

还有三个常量在代码里定义了但**没有任何路径会发出**,客户端不必为它们写分支:
`PARSE_ERROR (-32700)` / `INVALID_REQUEST (-32600)`(解析不了的帧被静默丢弃,见
§1)、`SESSION_CAP_REACHED (-32001)`(会话数到顶时驱逐最久未用的那个,不拒绝新建)。

### 9.1 `error.data` 的形状

**只有三个错误码带结构化 `data`**,客户端可以依赖:

```jsonc
// SESSION_BUSY
{ "session_id": "S1", "current_turn_id": "t1" }

// SCENE_MISMATCH
{ "session_id": "S1", "recorded_scene": "chat", "requested_scene": "coding" }

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
// 1. 发现:读 ~/.atta/daemon/instances.d/*.json,联合判活后连它的 socket

// 2. 看有哪些场景,决定要不要弹"选择项目"
→ {"jsonrpc":"2.0","method":"scene.list","id":1}
← {"jsonrpc":"2.0","id":1,"result":{"scenes":[
     {"scene":"coding","name":"AttaCode Coding","description":"…","active":true,
      "sessions":0,"requires_project":true,"supports_team":true}]}}

// 3. 订阅 daemon 级通知(可选)
→ {"jsonrpc":"2.0","method":"daemon.subscribeEvents","id":2}
← {"jsonrpc":"2.0","id":2,"result":{"subscribed":true}}

// 4. 建会话
→ {"jsonrpc":"2.0","method":"session.create","id":3,
   "params":{"scene":"coding","project_root":"/Users/x/repo-a"}}
← {"jsonrpc":"2.0","id":3,"result":{"session_id":"S1","scene":"coding",
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
   "params":{"scene":"coding","project_root":null}}
← {"jsonrpc":"2.0","id":11,"error":{"code":-32012,
   "message":"scene `coding` requires a project; pass project_root"}}
// 处理:先看 scene.list 的 requires_project,该弹项目选择框就弹。

// C. 恢复到了错的场景 —— 最该被拦住的一类
← {"jsonrpc":"2.0","id":12,"error":{"code":-32006,"message":"scene mismatch",
   "data":{"session_id":"S9","recorded_scene":"chat","requested_scene":"coding"}}}
// 处理:用 data.recorded_scene 找到对的 daemon(或先 scene.activate)重试。
//       不要当普通失败吞掉 —— 它意味着 UI 里那条会话挂在了错误的场景下。

// D. 停用场景被会话挡住
← {"jsonrpc":"2.0","id":13,"error":{"code":-32009,
   "message":"scene chat has active sessions: s1, s2"}}
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
