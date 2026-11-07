# PTC：让模型写一段程序来调用工具

`RunCode` 工具让模型写一段 JavaScript，在里面调用本会话的工具与子 Agent——并行、循环、
按结果分支——由引擎执行，**只把程序的返回值交回对话**。批量的、中间结果不必逐条进上下文
的活（在几十个文件里找东西再汇总、抓一批页面挑字段、给一批条目各派一个子 Agent 再合并），
一次工具调用做完。

它是 Agix 的批量编排手段：不另设 workflow 工具，脚本里调 `agent()` 就是编排。

## 1. 形态

| | |
|---|---|
| 工具 | `RunCode`，一个 |
| 语言 | JavaScript（ES2023），在内嵌的 QuickJS 里**作为脚本**（不是模块）执行，可以顶层 `await` |
| 生命周期 | 跟着一次工具调用：开始、跑完、结束，不留任何状态。长期存在的扩展是用户侧脚本（`docs/extending_quickjs.md`），与此无关 |
| 编译 | `ptc` 特性，`coding` 及以上档位默认带。与扩展载体（`scripts` / `plugins`）**不互斥**：这里跑的是模型写的一次性代码，和用户装的扩展是两条信任边界 |
| 场景 | `code`、`harnix` 常驻；`mini` 没有。部署方也可以用一条 deny 权限规则关掉 |

### 参数

```jsonc
{ "code": "…",            // 脚本正文，以 `return <JSON 值>` 结束
  "args": { … },          // 可选，脚本里读 `args`
  "description": "…",     // 一句话，任务列表里显示
  "background": false }   // true：立刻返回任务 id，跑完以通知回到会话，结果用 TaskOutput 取
```

## 2. 脚本里有什么

| 名字 | 说明 |
|---|---|
| `tools.<Name>(input)` | 调本会话一个可从代码调用的工具，返回 Promise。结果有结构化内容时是那个对象，否则是和直接调用时模型看到的同一段文本。失败（工具报错、参数不对、超出授权）就 reject，抛 `Error`，`error.tool` 是工具名 |
| `callTool(name, input)` | 同上，名字是字符串——给运行时才知道名字的 MCP 工具用 |
| `agent(prompt, opts?)` | 起一个子 Agent 并等它结束。`opts`：`type`（子 Agent 类型）、`schema`（要结构化结果时的 JSON Schema，结果按它校验后以对象返回）、`label`、`model`。失败 reject |
| `args` | 调用时传入的 `args` |
| `log(text)` | 一行进度：宿主在任务进度里看到，并随结果交回模型（最后 4,000 字符），用来看清中间值的格式。`console.log` 同它 |
| `notify(text)` | 告诉发起它的 Agent 一件事；只在后台运行时送达（前台运行时 Agent 正在等结果，退化为 `log`），一次运行至多 10 条 |
| `stopRequested()` | 是否有人要求它停（`TaskStop`）。长循环里查一下，交回已有的部分结果 |
| `messages()` | 取出别人经 `TaskMessage` 发给它的消息（字符串数组），取过即清 |

标准库都在：`JSON`、`Math`、`Date`、`RegExp`、`Map` / `Set`、`Promise`（含 `all` /
`allSettled`）、数组与字符串方法。

**没有**：`require` / `import`、npm 包、Node 或浏览器 API（`fs`、`process`、`Buffer`、
`fetch`、`setTimeout`……）。这些名字被预先定义成一调用就抛错的桩，错误信息直接说「这里没有
它，用 `tools.*`」，模型不会在一个 `undefined` 上反复猜。

### 例子

```js
// 找出所有直接 unwrap() 的地方，按文件汇总
const files = (await tools.Glob({ pattern: "crates/**/*.rs" })).split("\n").filter(Boolean);
const hits = await Promise.all(files.map(f =>
  tools.Grep({ pattern: "\\.unwrap\\(\\)", path: f, output_mode: "count" })
    .then(n => ({ f, n: parseInt(n) || 0 }))));
return hits.filter(h => h.n > 0).sort((a, b) => b.n - a.n).slice(0, 20);
```

```js
// 每个模块派一个只读子 Agent 审一遍，只交回有问题的
const reviews = await Promise.allSettled(args.modules.map(m => agent(
  `Review ${m} for error-handling bugs. List concrete issues with line numbers.`,
  { type: "explore", label: m,
    schema: { type: "object", properties: { issues: { type: "array", items: { type: "string" } } },
              required: ["issues"] } })));
return reviews.map((r, i) => ({ module: args.modules[i], ...(r.status === "fulfilled" ? r.value : { error: String(r.reason) }) }))
              .filter(r => r.error || r.issues.length);
```

## 3. 哪些工具能从代码里调

由工具自己声明（`Tool::callable_from_code`，缺省否）。**干活、返回数据的**声明可以；
**和对话或人打交道的**不声明：

| 可以 | 不可以 |
|---|---|
| `Read` `Glob` `Grep` `WebFetch` `WebSearch` | `AskUserQuestion`（要等人） |
| `Bash` `Edit` `Write` `NotebookEdit`（照常过权限） | `TodoWrite`、`Goal*`、计划模式（改的是会话本身） |
| `Agent`（脚本里经 `agent()` 用） | `Skill`、`ToolSearch`、`RunCode` 自己 |
| MCP 工具（`mcp__*`） | `Team*`、`Task*`、`Monitor`、`Cron*`、`PushNotification`、`StructuredOutput`、worktree |

使用指南里列出本会话可调的那些，一行一个签名，由各工具的 schema 与它声明的返回形状
（`Tool::code_result`）生成；场景过滤掉的不出现（工具描述里只列调用形状，见 §6）：

```
tools.Glob({pattern, path?, …}) → {paths: string[], more: number} — Find files …
tools.Read({file_path, offset?, limit?}) → text, each line prefixed with its number and a tab — …
```

给程序用的结构化结果不改变模型直接调用时看到的文本：

| 工具 | 程序拿到的 |
|---|---|
| `Glob` | `{paths, more}`：路径按修改时间新→旧，`more` 是被 `max_results` 截掉的个数 |
| `Grep` | `files_with_matches`（缺省）：`{files}`；`count`：`{counts: {路径: 次数}}`；`content`：文本，每行一处 `路径:行` |
| `Bash` | `{stdout, stderr, exit_code}`；退出码非零则 reject，错误里带输出 |
| `Read` | 文本，每行前是行号与制表符 |
| `agent()` | 子 Agent 的最终文本；给了 `schema` 时是校验过的对象 |

没声明的工具给文本，与直接调用时相同。脚本里调了不存在的 `tools.Foo`，得到的是
「Foo 在这里不可用，可用的有……」，不是 `undefined is not a function`。

## 4. 权限：沿用会话的判定，询问即失败

每次子调用走和模型直接调用**同一条**派发路径：参数校验、`PreToolUse` / `PostToolUse` hook、
中间件、遥测、权限判定。

权限判定照会话的模式与规则给出允许 / 询问 / 拒绝：

- **允许**：照常执行。项目内的读取总是允许；项目内改文件在 `acceptEdits` 下允许；Bash 在
  沙箱里、项目内可写——和直接调用时一样；
- **询问**：**直接失败**，错误说明「这一步要授权，RunCode 里不能问；直接调用这个工具」。
  脚本运行中途不弹询问；
- **拒绝**：失败。

不为 RunCode 另立放行规则：用户选了「改文件前先问我」，就不能因为换成写脚本而被绕过。

## 5. 执行

- **一件运行任务**，类型 `code`：前台运行也登记，宿主在 `session.tasks` 里看得到它的描述、
  已做的子调用数；`task_progress` 是 `log()` 的行与每次子调用的一行摘要
  （`→ Read(src/a.rs)` / `← ok` / `← error: …`）。`TaskOutput` 给的是这份完整的子调用记录
  加最后的结果，供审计。
- **线程**：脚本在一个专门的线程上、一个单线程的 QuickJS 异步运行时里跑；每次子调用经通道交给
  引擎的多线程运行时执行，结果送回脚本里的 Promise。所以 `Promise.all` 是真并行。
- **并发**：可并行的工具（读取、检索、`agent()`）同时至多 8 个在跑；不可并行的（写文件、Bash）
  按提交顺序一个接一个——与模型一次发多个工具调用时的规则相同。子 Agent 另受引擎的委派并发
  上限约束。
- **停止**：`TaskStop` 先置停止标志（`stopRequested()` 为真），10 秒宽限后仍未结束就强制：
  打断脚本、取消在跑的子调用。用户中断这一轮也是强制停止。
- **结果**：返回值 JSON 序列化，至多 50,000 字符，超出截断并注明。脚本抛错时，工具结果是错误
  信息加已完成子调用的清单——已经发生的副作用不会因为脚本失败而消失，模型得知道它们。

### 限额

| | 缺省 |
|---|---|
| 总时长 | 5 分钟；脚本里第一次调 `agent()` 起放宽到 60 分钟 |
| 子调用总数 | 1,000 |
| `agent()` 次数 | 100 |
| 同时在跑的子调用 | 8 |
| 交回脚本的单个工具结果 | 200,000 字符（超出截断并注明） |
| 返回值 | 50,000 字符 |
| 脚本内存 | 64 MB |
| 代码长度 | 100,000 字符 |

超出任一项，脚本以错误结束，在跑的子调用取消。

## 6. 提示词

`RunCode` 常驻，描述每次请求都带，所以只放够用的部分（约 1.2K 字符）：用途、什么时候用、
脚本环境一句话，加上本会话可调工具的调用形状（不含各工具的一句话说明；`Agent` 不列，
脚本里用 `agent()`）：

```
Run a JavaScript program that calls this session's tools — in parallel, in loops or in
stages — and get back only its return value. For batch work (many files, pages or items)
and for workflows or multi-agent orchestration: agent(prompt, {type, schema}) starts a
sub-agent. For a single task or a few calls, call the tools directly.

A script (QuickJS, ES2023, top-level await; no require/import or Node/browser APIs).
tools.<Name>(input) and callTool(name, input) return Promises that reject on failure;
log(text) reports progress. End with `return <JSON value>`.

Callable here (MCP tools too, with callTool; the guide has the rest):
  tools.Read({file_path, limit?, offset?, pages?}) → text, each line prefixed with …
  …
```

其余——全部全局函数（`args`、`notify`、`stopRequested`、`messages`）、权限与限额、
带一句话说明的完整签名清单——在它的使用指南里，`ToolSearch select:RunCode` 取。脚本抛错时
结果末尾提示去取指南。

Agix 没有单独的 workflow 工具：用户要的 workflow / 多 Agent 编排也写成一个 RunCode 程序，
阶段就是先后 await，扇出就是对 `agent()` 的 `Promise.all`。

场景提示词的「批量工作」一段把它放在首位：中间结果不必逐条看的批量，写一个 RunCode；
要按结果一步步判断的，才用后台 `Agent` + `TaskWait`；事先定好的分阶段委派用 `TeamCreate`。

## 7. 和其他做法的关系

- **直接调用工具**仍是常态。RunCode 是为「多而不必逐条看」准备的。
- **后台 `Agent` + `TaskWait`**：每一步都要模型判断时用。RunCode 里的 `agent()` 是同一种
  子 Agent，只是由程序来起、来收。
- **`TeamCreate`**：成员之间要互相通信、跨阶段共享团队记忆时用。
- **Harnix flow**：持久、可复用、要人审批的流程。RunCode 跑完即止。
