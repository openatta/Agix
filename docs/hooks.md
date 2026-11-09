# Hooks

hook 是写在设置里的外部回调:引擎走到一个具名时刻(调工具之前、一轮要结束时、压缩之前……)
就把这一刻的情况作为一份 JSON 交给它,它可以只是看看,也可以在允许的地方拦下、改写或者
把模型送回去接着干。一个会话里一共有三十个这样的时刻,`HookEvent` 的每个变体是一个;
后端有四种:跑一条 shell 命令、问一次模型、POST 一个 URL、起一个子 Agent。

在扩展面里的位置见 [`extension_points.md`](extension_points.md) 的 `hooks` 一节;
嵌入方用 Rust 拦同样的东西,用的是那里的 `tool.around`、`prompt.submit` 等扩展点。

---

## 配置在哪

`settings.json` 的 `hooks_config`:事件名到一组 hook 的映射。

```json
{
  "hooks_config": {
    "PreToolUse": [
      { "type": "command", "command": "check-push.sh", "if": "Bash(git push:*)", "timeout": 5000 }
    ],
    "Stop": [
      { "type": "prompt", "prompt": "If the user's request is not fully done, answer {\"decision\":\"block\",\"message\":\"<what is left>\"}; otherwise answer {}." }
    ],
    "SessionEnd": [
      { "type": "http", "url": "https://hooks.example.com/agix", "headers": { "authorization": "Bearer …" } }
    ]
  }
}
```

- **按层合并。** 设置文件各层(全局 `~/.agix/settings.json` → 场景 → 项目
  `.agix/settings.json`,每层后面跟它自己的 `settings.local.json`)按 JSON 递归合并:
  不同事件各自保留,**同一个事件在高层出现时,高层的数组整个替换低层的**。
- **坏一条只丢一条。** 不认识的事件名(拼错、写给更新版本引擎的)、格式不对的条目,丢掉那一条、
  记一条 warn,其余照常生效。事件名区分大小写,不做"最接近的"猜测。
- **从不触发的事件会说出来。** 有两个事件能配、能解析,但引擎里没有地方触发它们(表里标着
  **从不触发**,清单是 `hooks::UNWIRED_EVENTS`);给它们配了 hook,会话建起来时记一条 warn。
- **整棵会话树一个 runner。** 子 Agent 用父会话的 hooks:子 Agent 里的工具调用、轮次照样
  触发配置的 hook,payload 带的是子 Agent 自己的 `session_id`、`cwd`、`permission_mode`。

---

## 四种后端

`type` 字段选后端。四种都认 `timeout`(毫秒,不写就是 **60 秒**)和 `if`。

### `command`

```json
{ "type": "command", "command": "./scripts/check.sh", "shell": "bash",
  "timeout": 5000, "if": "Bash", "only_on_error": false, "once": false, "async_rewake": false }
```

- 以 `<shell> -c <command>` 起进程,`shell` 缺省 `bash`;工作目录是 payload 的 `cwd`。
- `PATH` 前面依次加上项目的 `.agix/hooks/`、`~/.agix/scenes/<scene>/hooks/`、
  `~/.agix/hooks/`,所以 `command` 可以直接写放在那里的脚本名。环境与 Bash 工具同一套
  (`sandbox.subprocess_env`)。command hook **不进 OS 沙箱**,以 daemon 的身份运行。
- stdin 收到一行 JSON payload(见下)。
- **退出码只认 0 和 2。**
  - `0`:stdout 是 JSON 回答;stdout 为空等于什么都不说;不是 JSON 的记 warn,当作没说。
  - `2`:拦下(等同 `{"decision":"block"}`),理由取 stderr,stderr 为空取 stdout;
    stdout 不当 JSON 读。
  - 其他(含 1、被信号杀掉):这个 hook 坏了,记 warn,**放行**,什么都不改。
- 只有 command 有的三个开关:
  - `only_on_error`:只在 payload 的 `is_error` 为 `true` 时跑(配在 `PostToolUse` 上用)。
  - `once`:在这个会话(连同它的子 Agent)里**真正跑过一次**后不再跑;`if`、`only_on_error`
    没放行的那几次不算。
  - `async_rewake`:回答里带 `"rewake": true` 时记下这个 hook,等后台工作结束后以
    `hook_event_name: "WakeEvent"` 再跑它一次(重跑的回答被丢弃)。"后台工作结束"指会话的一个
    运行中任务(子 Agent、后台命令、Monitor)结束;嵌入方给了 `Builder::wake_receiver` 的,
    改为在那条通道上收到信号。会话在两条输入之间查一次:处理完一条输入、等下一条之前,有
    信号就重跑。hook 要求唤醒之前就结束的工作不算。

### `prompt`

```json
{ "type": "prompt", "prompt": "…", "model": "claude-haiku-4-5", "timeout": 30000, "if": "" }
```

一次不带工具的模型调用:`prompt` 作为系统提示词,payload 的 JSON 作为用户消息,模型回的
文本当作 JSON 回答解析(包在 ```` ```json ```` 里也认);解析不出来等于什么都不说。走会话
自己的 provider,`model` 换的是模型名。这次调用在遥测里记为 `api_request`,`purpose` 为
`hook`。

### `http`

```json
{ "type": "http", "url": "https://hooks.example.com/agix",
  "headers": { "authorization": "Bearer …" }, "timeout": 10000, "if": "*" }
```

- `POST` payload 的 JSON(`content-type: application/json`),加上配置的 `headers`。
- 2xx:响应体当 JSON 回答解析,解析不出来等于什么都不说。其他状态码、连不上、超时:失败,
  记 warn,放行。
- **只能去公网。** url 写在一份仓库也能带进来的设置文件里,所以每一跳(最多 10 次重定向,
  每一跳都查)都不许落到本机或内网:回环、私有网段、链路本地、`0.0.0.0/8`、
  `100.64.0.0/10`、`198.18.0.0/15`、组播、保留段,IPv6 的 ULA、链路本地、站点本地,以及
  映射或内嵌这些 IPv4 的 IPv6 地址。主机名在发出前查一次、建连时由连接自己的解析器再查
  一次,第二次解析到内网同样拒绝。`sandbox.allowed_domains` 管不到它(它管的是模型选的
  目标),也放不开这条。重定向换了主机或协议时,`authorization`、`cookie`、
  `proxy-authorization` 不跟过去。

### `agent`

```json
{ "type": "agent", "prompt": "Review this change for secrets…", "timeout": 120000, "if": "Write|Edit" }
```

用会话的 agent spawner 起一个子 Agent,任务是 `prompt` 后接 `Context:` 和 payload 的
JSON。子 Agent 的最终回答应当是 `{"ok": true|false, "reason": "…"}`:`ok: true` 记为
`approve`(配在 `PreToolUse` 上就是替人答应权限询问,见"回答"),`ok: false` 或缺 `ok` 记为
`block`,`reason` 作 `message`;不是 JSON 的回答只当 `message`,不拦不改。写了 `model` 的,
子 Agent 跑在这个模型上,provider 按会话的路由选;不写用会话的模型。

---

## `if`

决定这个 hook 对这一次事件跑不跑,写法同权限规则:

| 写法 | 命中 |
|---|---|
| `""`、`*`(或不写) | 一切,包括不带工具的事件 |
| `Bash` | `tool_name` 等于 `Bash` |
| `Bash(git push:*)` | 工具名相同,且内容是 `git push` 或以 `git push ` 开头 |
| `Write(*.rs)` | 内容匹配这个 glob(`*` 跨目录;`file_path` 是绝对路径) |
| `mcp__github` | 这个 MCP server 的所有工具(`mcp__github__…`) |
| `Edit\|Write`、`Bash(git push:*)\|Write` | 任一个;括号里的 `\|` 属于内容 |

"内容"依次取 `tool_input` 的 `command`、`file_path`、`path`、`url`。除 `""` / `*` 之外的
写法都要求 payload 有 `tool_name`——表里哪些事件带它,写在 payload 一列。

---

## payload

每份 payload 都有四个字段:

| 字段 | 内容 |
|---|---|
| `hook_event_name` | 事件名,同配置里的键(`async_rewake` 的重跑是 `WakeEvent`) |
| `session_id` | 触发这一事件的会话;子 Agent 里触发的是子 Agent 的 id |
| `cwd` | 会话的工作目录 |
| `permission_mode` | 触发时会话**当下**的权限模式:`default`、`plan`、`acceptEdits`、`bypassPermissions`、`dontAsk`、`bubble` |

其余字段按事件出现,没有的不写(不是 `null`):

| 字段 | 类型 | 说明 |
|---|---|---|
| `tool_name` | string | 工具名;生命周期事件借它放来源(`PushNotification`、`FileWatcher`、`mcp_elicitation_<server>`) |
| `tool_input` | object | 工具入参;`PreCompact` / `PostCompact` / `FileChanged` / `Elicitation*` 用它放各自的数据 |
| `tool_use_id` | string | 这次工具调用的 id |
| `tool_result` | string | 工具结果的文本(`PostToolUse`) |
| `is_error` | bool | 结果是不是失败,工具自己报告的失败(如 Bash 非零退出)也算 |
| `user_prompt` | string | 用户消息的文本 |
| `delivery` | string | `turn`(开一轮)或 `steer`(在运行中的一轮里插进来) |
| `turn_id` | string | 所在的轮 |
| `stop_reason` | string | 一轮或一个子 Agent 怎么结束的 |
| `agent_type` | string | 子 Agent 的类型,起它时选了才有 |
| `reason` | string | 人读的说明:询问的文字、拒绝的理由、错误文本、通知正文…… |
| `stop_hook_active` | bool | `Stop`:这一轮已经被 Stop hook 送回去过一次 |
| `team_name` / `teammate_name` | string | 团队成员的事件:团队名与成员名 |

---

## 回答

回答是一个 JSON 对象,所有字段可选,`{}` 或空输出等于什么都不说:

| 字段 | 作用 |
|---|---|
| `decision` | `"block"`:在能拦的事件上拦下(各事件含义见下表);`"approve"`:`PreToolUse` 上替人答应——权限检查要询问的这次调用不再问人、直接放行(无人值守会话同样),权限规则拒绝的照样拒绝;别的事件上不起作用 |
| `message` | 拦下的理由——交给模型或写进结果的那句话 |
| `continue` | `false`:在能结束一轮的事件上结束它 |
| `updated_input` | `PreToolUse`:新的入参(整个替换);`UserPromptSubmit`:新的消息文本(JSON 字符串) |
| `rewake` | 见 `async_rewake` |

hook 的 stdout 只用来读回答,引擎不把它交给模型或宿主;要让模型看到的话写进 `message`。
所以没有 `suppress_output` 这样的字段,写了也不报错。

**一个事件配了几个 hook,它们并发跑,结果按配置顺序合并:** 有一个说 `block` 就拦(理由取
第一个拦的,`block` 压过别的 hook 的 `approve`);有一个说 `continue: false` 就结束;
`updated_input` 取配置顺序里第一个给出的,后面的不看。一个 hook 慢或者坏了,不耽误同一事件的其他 hook。

**失败就是放行。** 超时(到时 command 的进程被杀掉)、起不来、command 退出码不是 0 或 2、
HTTP 非 2xx、回答解析不出来——这个 hook 不拦、不改、不结束任何东西,只记一条 warn。
hook 是策略的补充,不是唯一的门:要硬拦的东西写成权限规则。

---

## 事件

下表由 `hooks::catalog` 生成,`daemon/tests/hook_events_doc.rs` 保证它和代码一致——改
`HookEvent::spec`,别改表(`AGIX_UPDATE_DOCS=1` 重写这张表)。"只通知"的事件,hook 的回答
在触发处被丢掉,拦不了也改不了。

<!-- BEGIN GENERATED TABLE — see hooks::catalog::render_markdown -->
| 事件 | 何时触发 | payload(三个公共字段之外) | 回答能做什么 |
|---|---|---|---|
| `PreToolUse` | 每次工具调用分发之前,在输入校验与权限检查之前;子 Agent 的调用也算 | `tool_name`、`tool_input`、`tool_use_id` | `decision: "block"`(或 exit 2):这次调用不执行,模型收到 `Blocked by PreToolUse hook: <message>`;`updated_input`:换掉入参,换过的照常校验、过权限;`decision: "approve"`:权限检查要询问时不再问人,直接放行(规则拒绝的照样拒绝) |
| `PostToolUse` | 工具真正跑完之后(成功或失败);被 PreToolUse、权限拦下的调用不发 | `tool_name`、`tool_input`、`tool_use_id`、`tool_result`(结果文本)、`is_error` | `decision: "block"`:结果换成 `Denied by PostToolUse hook: <message>`,同一批并发的其他调用被取消;`continue: false`:这一批工具跑完后结束这一轮(`stopped_by_hook`) |
| `PostToolUseFailure` | 工具调用以错误结束(工具返回错误或 panic)时,在 PostToolUse 之前;工具自己报告失败的结果(如 Bash 非零退出)不算 | `tool_name`、`tool_input`、`reason`(错误文本)、`turn_id` | 只通知,回答被丢弃 |
| `TurnStart` | 一轮开始,UserPromptSubmit 放行之后、第一次调模型之前 | `turn_id` | 只通知,回答被丢弃 |
| `TurnComplete` | 一轮正常走完(模型不再要工具,Stop hook 也放行了) | `turn_id`、`stop_reason` | 只通知,回答被丢弃 |
| `SessionStart` | 会话开始运行,紧接在 `Setup` 之后 | — | 只通知,回答被丢弃 |
| `SessionEnd` | 会话的主循环退出:收到关闭、输入通道关闭、被取消,三种都发 | — | 只通知,回答被丢弃 |
| `Stop` | 模型这一步不再要工具、这一轮要结束时;每次要结束都问一遍 | `stop_hook_active`(这一轮已经被 Stop hook 送回去过) | `continue: false`:结束这一轮(`stopped_by_hook`,随后发 `StopFailure`);`decision: "block"`(或 exit 2):不让结束,`message` 作为提醒交给模型接着做 |
| `StopFailure` | 一轮被 Stop hook 的 `continue: false` 截停时 | `reason` | 只通知,回答被丢弃 |
| `UserPromptSubmit` | 用户消息进转录之前:开一轮的那条,以及运行中 steer 进来的每一条 | `user_prompt`、`delivery`(`turn` / `steer`) | `decision: "block"`:这条消息不处理——开一轮的直接以 `stopped_by_hook` 结束、不调模型,steer 的不进转录;`updated_input`(JSON 字符串):换掉消息文本 |
| `PreCompact` | 每次压缩之前(按阈值自动压、`/compact`、恢复策略要求的压缩) | `tool_input`:`messages_before`、`token_count`、`threshold`、`trigger`(`auto` / `manual`) | `decision: "block"` 或 `continue: false`:这次不压缩 |
| `PostCompact` | 压缩完成之后 | `tool_input`:`strategy`、`messages_before`、`messages_after`、`tokens_before`、`tokens_after` | 只通知,回答被丢弃 |
| `PermissionRequested` | 权限检查判为"询问"、开始等宿主回答之前(无人值守会话不问,直接按拒绝处理) | `tool_name`、`tool_input`、`reason`(询问的文字)、`turn_id` | 只通知,回答被丢弃 |
| `PermissionDenied` | 一次工具调用被拒:规则拒绝、无人值守时的询问、宿主答拒绝或没给出决定、询问超时 | `tool_name`、`tool_input`、`reason`、`turn_id` | 只通知,回答被丢弃 |
| `Setup` | 会话开始运行,早于 `SessionStart` | `reason`(`scene=<id> model=<模型>`) | 只通知,回答被丢弃 |
| `Notification` | 需要人注意时:一个权限询问在等回答;模型调了 `PushNotification` | `tool_name`、`tool_input`(被询问的调用,或 `PushNotification` 的入参)、`reason`、`turn_id` | 只通知,回答被丢弃 |
| `SubagentStop` | 子 Agent 跑完(成功、出错、被取消);常驻团队成员在被停下时 | `session_id` 是子 Agent 的;`agent_type`、`stop_reason`(`end_turn` / `error` / `cancelled` / `shutdown`) | 只通知,回答被丢弃 |
| `SubagentStart` | 子 Agent 开始跑之前,每一种起法都发:前台、后台、宿主 `session.startAgent`、`TeamCreate` 成员、常驻成员 | `session_id` 是子 Agent 的;`agent_type` | 只通知,回答被丢弃 |
| `TeammateIdle` | 团队成员的一轮结束(不论怎么结束),在 `TaskCompleted` 之后 | `team_name`、`teammate_name`、`turn_id`、`stop_reason` | 只通知,回答被丢弃 |
| `TaskCreated` | 一个运行中的任务开始:子 Agent、后台 shell 命令、Monitor、团队成员 | `reason`(这个任务在 `session.tasks` 里的 JSON) | 只通知,回答被丢弃 |
| `TaskCompleted` | 团队成员的一轮正常结束、交给它的活干完了 | `team_name`、`teammate_name`、`turn_id`、`stop_reason` | 只通知,回答被丢弃 |
| `Elicitation` | MCP 工具的结果里带了 elicitation 链接(`mcp://`、`elicitation://`) | `tool_name`(`mcp_elicitation_<server>`)、`tool_input`:`server_name`、`url` | 只通知,回答被丢弃 |
| `ElicitationResult` | **从不触发**:宿主把用户对 elicitation 的回答交回来时;还没有宿主侧的回传通路 | `tool_name`、`tool_input`:`server_name`、`url`、`response`;`tool_result` | 只通知,回答被丢弃 |
| `ConfigChange` | **从不触发**:配置在运行中变了;改配置的 RPC 在池一级,高于任何一个会话的 hooks | — | 只通知,回答被丢弃 |
| `WorktreeCreate` | `EnterWorktree` 成功之后 | `tool_name`、`tool_input`、`turn_id` | 只通知,回答被丢弃 |
| `WorktreeRemove` | `ExitWorktree` 成功之后 | `tool_name`、`tool_input`、`turn_id` | 只通知,回答被丢弃 |
| `InstructionsLoaded` | 会话上下文(项目说明)注入转录时,说明非空才发;一个会话一次 | `reason`(注入了多少字节) | 只通知,回答被丢弃 |
| `CwdChanged` | 会话开始运行时报一次工作目录;活着的会话不会换目录 | `reason`(`session working directory: <路径>`) | 只通知,回答被丢弃 |
| `FileChanged` | 根会话的 `instruction_file`,或它载入的某份 `AGENTS.md`,被改、建、删;300 ms 去抖,一次变化一个事件 | `tool_name`(`FileWatcher`)、`tool_input`:`file_path`、`change_type`(`created` / `modified` / `deleted`) | 只通知,回答被丢弃 |
| `PostSampling` | 一次模型调用的回复完整收完、写进转录之后(被取消的不发) | `reason`(`stop_reason` 与输出 token 数) | 只通知,回答被丢弃 |
<!-- END GENERATED TABLE -->

几处补充:

- **`Stop` 的送回。** 被 `block` 时,`message`(为空时用一句默认的话)以
  `<system-reminder>` 交给模型,这一轮接着跑;下一次要结束时 `Stop` 再问一遍,payload 的
  `stop_hook_active` 为 `true`。一个无条件拦的 hook 应当看这个字段放行;不看的,由每轮的
  调用上限截住。
- **团队成员。** `TaskCompleted` / `TeammateIdle` 只在团队成员(`TeamCreate` 的成员、`Agent`
  带 `team_name` 的常驻成员)的会话里发,先 `TaskCompleted`(只在正常结束时)后
  `TeammateIdle`,都在这一轮落盘之后,比这一轮的 `TurnComplete` hook 晚。
- **`FileChanged`。** 只有根会话起监视,子 Agent 不重复发;监视的是 `instruction_file`
  和根会话载入过的每一份 `AGENTS.md`,不是任意文件。不存在的路径不监视。
- **`UserPromptSubmit` 之后还有一道。** 嵌入方的 `prompt.submit` 扩展点跑在 hook 之后,
  hook 的改写是它看到的原文,它的改写 hook 改不回来。
