# 用 QuickJS 脚本扩展 Agix

一个脚本就是一个 `.js` 文件加一行配置。会话构建时引擎读这个文件,在你指定的
扩展点上调用你指定的函数,再把结果交回给发问的那部分引擎。不用构建,不起子进程,
不走网络。

这一层就是这么点东西:在"重编译引擎"和"起一个进程"之间,给那些小活留的位置
——改提示词里的一行、给工具结果打个标记、拿缓存答掉一次工具调用、把召回的查询
收窄一点。解释器是 QuickJS,嵌在 daemon 里,一次调用的开销是微秒级。

`docs/extension_points.md` 是引擎里所有可扩展之处的总目录,不限使用者、不限方式。
本文只讲今天脚本能绑上去的那九个点。

## 这个构建带载体吗

脚本载体是 `daemon` crate 的 `scripts` feature,**默认开启**:

```sh
cargo build -p daemon                                             # QuickJS
cargo build -p daemon --no-default-features --features plugin-compile
cargo build -p daemon --no-default-features                       # neither
```

一个构建只带**一个**扩展载体,或者一个都不带。`scripts` 和 `plugins` 互斥,同时
开启编译不过——插件构建必须带 `--no-default-features`,否则 cargo 会把默认的
`scripts` 并回来,`daemon/src/lib.rs` 里的守卫会拒绝。

不开这个 feature,就没有任何 JavaScript 引擎被链接进来,`settings.json` 里出现
`scripts` 段会被明确拒绝:

```
this build carries no script engine, so a `scripts` section cannot be honored;
rebuild with the `scripts` feature or remove the section
```

## 从零开始

**1. 写脚本。** 放项目里哪儿都行,`.agix/scripts/` 是惯例:

```js
function onAssemble(blocks) {
  return blocks.concat([
    {
      name: "team.house_style",
      content:
        "House rule, absolute: end every reply with the line " +
        "'-- checked by the house style script'. No exceptions.",
    },
  ]);
}
```

**2. 绑定它**,在项目的 `.agix/settings.json` 里:

```json
{
  "scripts": [
    {
      "path": ".agix/scripts/house_style.js",
      "point": "prompt.assemble",
      "entry": "onAssemble"
    }
  ]
}
```

**3. 启动 daemon**,把那个项目当成它的工作目录——相对的脚本路径是按它解析的。
会话创建时会打出

```
bound scripts to extension points  scripts=1
```

从此这个会话里每一次回复都以那行 house-style 结尾。

第 3 步有两件事要知道。文件是**在会话构建时**读的,所以改了脚本,不新起一个会话
就什么都不会变。还有,一条兑现不了的绑定——文件读不出来、点不存在、点不给脚本用
——会让**整组**绑定作废:daemon 打出

```
a script binding is invalid; no scripts were bound
```

然后这个会话一个脚本都不带地跑,而不是带着其中一半跑。

绑定检查的是点和文件,**不求值**。所以语法错的文件绑得干干净净,会话照常起来,每个
点在第一次被调用时各自失败——和路径打错是两种不同的坏日子:那一种什么都不带地跑,
这一种带着一整套永远失败的绑定跑。

## 绑定的字段

| 字段 | 含义 |
|---|---|
| `path` | 脚本文件。绝对路径,或相对于 daemon 的工作目录——决定脚本权限的也是这个根。 |
| `point` | 绑到哪个扩展点,用它在目录里的 id。下面九个之一。 |
| `entry` | 调这个文件里的哪个函数。 |
| `timeout_ms` | 一次调用的墙钟时间。默认 100。 |
| `calls_per_turn` | 这条绑定在一轮里能调多少次。默认 1000。 |

一个文件可以绑不止一次——绑到几个点,或者同一个点上换不同的入口函数。每一行都是
自己独立的绑定,各有各的预算。两个脚本共用一个点时,按绑定出现的顺序安装;在
`tool.around` 上这意味着第一条是最外面那一环。

`docs/extension_points.md` 目录里的「脚本」那一列,说的是每个点的*契约*允许脚本
做什么。下面这九个是载体真的写了适配器的。两份名单不是一回事,绑到一个原则上开放
但没有适配器的点,会被指着名字拒绝:

```
`history.append_observer` exists but scripts cannot be bound to it; today that
is prompt.assemble, prompt.block, prompt.context, prompt.variable, tool.around,
tool.result, memory.retrieval_hook, model.request, model.message
```

## 脚本是怎么被调用的

你的文件先被求值,然后 `entry` 函数被调用,只带一个参数:这个点的输入,已经从
JSON 解析好了。你返回的东西会被序列化回 JSON。什么都不返回等于返回 `null`。

入口要声明成顶层的普通函数——`function onAssemble(input)`。没有模块,也没有
`export`。

每次调用都拿到一个**全新的运行时**。调用之间什么都不留:你设的全局变量不留,你建
的缓存不留,别的会话也看不见任何东西。要状态,就得从输入里带进来。

脚本够不到自己以外的**任何**东西。没有 `require`,没有 `process`,没有 `fetch`,
没有 `XMLHttpRequest`,没有文件系统,没有网络。`Date` 能用。输入进去,输出出来。

## 九个点

每一节讲这个点什么时候触发、你的函数收到什么、能返回什么。这里引用的每一个 fixture
都在 `tests/fixtures/scripts/` 下,由 `tests/runner/tests/script_carrier.rs` 端到端
跑过一遍。

### `prompt.assemble` — 整个系统提示词,最后一道

一轮一次,在所有块都装配完之后。收到的是按顺序排好的块,返回它想要的那份清单:

```json
[{ "name": "scene.skeleton", "content": "…" }, { "name": "rules", "content": "…" }]
```

返回的清单会拿去跟给你的那份**逐个名字比对**:

- 原来没有的名字是**新增**,作为新块放在末尾;
- 名字在、内容变了的是**修改**;
- 给了你、你没还回来的名字是**删除**;
- 原样交回来的块根本不算一次编辑,所以为了改一行而把整份清单返回,不花你任何代价。

没有名字的块两个方向上都被忽略。重排在这里表达不出来——位置来自块注册时的顺序,
不是来自它在你数组里的位置。

场景里没有自己起名字的那些段落,第一段叫 `scene.skeleton`,后面的依次是
`scene.skeleton.2`、`scene.skeleton.3`——每一段都能被单独指到。

**万一还是有两个块同名**(比如你自己新增了一个已有的名字),你拿到的清单和你还回来的
清单是**按出现次序逐个对上的**,第 k 个对第 k 个,所以原样交回来仍然不算编辑;而改动
其中某一个会被拒绝并报出来,不会落到第一个身上——块按名字寻址,这样的编辑没法说清指
的是哪一个。

这几件事里你能做哪些,取决于你的文件在哪儿;见下面的[权限](#权限)。

```js
// tests/fixtures/scripts/prompt_assemble.js
function onAssemble(blocks) {
  blocks.push({ name: "script.fixture.assemble", content: "SCRIPT-TRACE-ASSEMBLE" });
  return blocks;
}
```

抛异常、超时、配额耗尽,或者返回一个根本不是块清单的东西,提示词就原封不动
——一份改了一半的提示词比一份没改的更糟,因为下游没人分得清自己看的是哪一种。

### `prompt.block` — 一个固定的块

绑定的时候调用**一次**,参数是 `null`。它把块连同正文一起答回来:

```js
// tests/fixtures/scripts/prompt_block.js
function onBlock() {
  return {
    name: "team.conventions",
    order: 250,
    content: "SCRIPT-TRACE-BLOCK: keep answers short.",
  };
}
```

`name` 必填。`order` 可选,默认 500,也就是排在引擎自己贡献的所有东西之后;内核
自身的几级分别在 0(`scene.*`)、100(`skills.catalog`)、200(`memory.session`)、
300(`rules`)、400(`mcp.instructions`)和 500(`config.prompt_append`)。`content`
必须是字符串——正文要随会话变的,该去 `prompt.context`。

已经属于内核块的名字——`scene.` 开头的任何一个,以及 `skills.catalog`、
`memory.session`、`rules`、`mcp.instructions`、`config.prompt_append`、
`config.prompt_override`——会被拒绝。块是按名字寻址的,先匹配到的那个赢,所以一个
占了内核名字的贡献会悄悄把本该落在真块上的编辑吸走。

这次调用要是失败了,或者答出来的东西没有能用的 `name`,或者 `order` 不是数字,
那就什么都不注册,并且给出一条警告说明。

### `prompt.context` — 每次装配都重算的块

两种调用。第一种在绑定时发生,传 `null`,问的是这个块的身份——跟上面一样的
`{ name, order }`。此后每一次调用都传会话上下文,要的是块的正文:

```js
// tests/fixtures/scripts/prompt_context.js
function onContext(ctx) {
  if (ctx === null) {
    return { name: "project.status", order: 260 };
  }
  return "SCRIPT-TRACE-CONTEXT: working in " + ctx.cwd;
}
```

上下文讲的是这个会话跑在哪儿,以及所有还不是提示词块的东西:

```json
{
  "cwd": "/home/u/proj", "os": "linux", "shell": "bash",
  "homeDir": "/home/u", "date": "2026-06-10", "modelName": "claude-opus-5",
  "isGit": true, "gitBranch": "main", "isWorktree": false,
  "language": null, "scratchpadDir": null, "availableTools": "Read,Bash"
}
```

技能清单、MCP 指令、会话记忆和输出风格是刻意不给的:它们每一个都已经是一个块了,
想读或者想改写其中一个的脚本,该去绑 `prompt.assemble`。它们同时也是这附近最大的
四个字符串,而一个只想要 `cwd` 的脚本,会让它们每轮往一个全新的解释器里序列化一遍。
`gitStatus` 不给,一是因为它的体积,二是因为一份未提交的 diff 恰恰是这里唯一一样
从项目外面来的脚本没有理由读的东西。

除字符串以外的任何东西——`null` 也算——这一轮都不贡献块,调用失败或超时的结果
也是这个。

### `prompt.variable` — `{{name}}` 展开成什么

同样是两段式。身份那次调用给出变量的名字;`order` 和 `content` 在这里没有意义。
之后的调用拿到跟 `prompt.context` 一样的上下文对象,答出值:

```js
// tests/fixtures/scripts/prompt_variable.js
function onVariable(ctx) {
  if (ctx === null) {
    return { name: "script_trace_var" };
  }
  return "SCRIPT-TRACE-VARIABLE(" + ctx.os + ")";
}
```

**只有字符串才是值。** `null`、数字、对象、抛异常、超时、配额耗尽,全都让
`{{script_trace_var}}` 原样留在提示词里:一个没解析的变量是个该被看见的 bug,
不是个该被藏起来的洞。真想让占位符消失的脚本,返回 `""`。

没人提过的变量哪儿也不展开——得先有谁把 `{{script_trace_var}}` 放进某个块里。

### `tool.around` — 一次工具调用前的一个决定

一次工具调用一次,在派发之前、权限闸门之外。

```json
{ "tool": "Read", "input": { "file_path": "/etc/hosts" } }
```

返回下面之一,返回别的都表示"照常来":

```json
{ "action": "deny", "reason": "reads outside the project are not allowed" }
{ "action": "respond", "text": "(cached)" }
{ "action": "proceed", "timeoutMs": 2000 }
```

- **deny** —— 调用不派发,工具把 `reason` 当成自己的错误报出去。一个没有 `reason`
  的 `deny` 没什么可告诉模型的,所以那次调用改为照常进行。
- **respond** —— 调用不派发,`text` 就是模型看到的结果。没有 `text` 字符串就照常
  进行,因为拿一个空结果去答,等于给模型一个悄没声儿什么也没产出的工具。
- **proceed** —— 正常派发。`timeoutMs` 放在这个决定上、或者放在任何一个会派发的
  决定上,都会让调用在那么长时间之后停下。

```js
// tests/fixtures/scripts/tool_around.js
function onAround(call) {
  if (call.tool === "ScriptEcho") {
    return { action: "respond", text: "SCRIPT-TRACE-AROUND: answered without dispatch" };
  }
  return null;
}
```

`deny` 和 `respond` 都不经过 `tool.result`:那个点看的是一次派发的结果,而这两个
决定都没有派发。所以一个想给每条工具结果打标记的脚本,标不到另一个脚本从缓存里答
掉的那些——要连这些一起管,得绑在这里。

你**不能**改写参数:`Read(a.txt)` 在这里变不成 `Read(~/.ssh/id_rsa)`。你也没法把
期限往后拉——`timeoutMs` 装的是这一轮那个信号的子信号,所以它可以比会话本来放弃的
时刻更早触发,但绝不会更晚。而且你看不到结果;脚本只在派发前跑那一次。结果长什么样
是 `tool.result` 的活。

因为这一环坐在权限闸门外面,一个 `deny` 是在问用户任何事之前就把调用拦了,一个
`respond` 是在闸门根本没被问过的情况下答的。两者都没执行任何东西,但两者都决定了
模型被告知这个工具干了什么。

### `tool.result` — 一个结果长什么样

一个工具结果一次,在所有 hook 之后,紧挨着模型看到它之前。**派发过的才有结果**:
`tool.around` 用 `deny` 或 `respond` 答掉的那些调用不会走到这里。

```json
{ "tool": "ScriptEcho", "input": { "say": "anything" }, "text": "…", "isError": false }
```

返回一个字符串,它就成了结果的正文。返回别的——数字、对象、什么都不返回——结果就
不动,一个带 bug 的脚本得到的也是这个结果。

```js
// tests/fixtures/scripts/tool_result.js
function onResult(result) {
  return "SCRIPT-TRACE-RESULT(" + result.tool + ") " + result.text;
}
```

正文就是这里的全部词汇。脚本没法丢掉或替换随结果一起走的图片,也没法改变这个结果
是不是一个错误。

### `memory.retrieval_hook` — 召回的两头

这个点有两半,而一条绑定只指一个函数,所以阶段是跟着输入走的。调用两次:一次带
`"phase": "before"`,一次带 `"phase": "after"`:

```json
{
  "phase": "before",
  "query": "what did we decide about deploys",
  "limit": 5,
  "alreadySurfaced": ["deploy-window"],
  "recentTools": ["Bash", "Read"],
  "modelName": "claude-opus-5",
  "sessionId": "01J…"
}
```

`"after"` 那次带的字段一样,外加 `names`——检索器产出的那份清单,最好的在前。

`"before"` 要返回一个对象:`query`(字符串)成为新的问题,`limit`(正整数)成为
新的上限,两个都可选。`"after"` 要返回一个字符串数组——要留下的那些名字,按模型
该看到的顺序排。

```js
// tests/fixtures/scripts/memory_retrieval.js
function onRetrieval(recall) {
  if (recall.phase === "before") {
    return { query: "SCRIPT-TRACE-RECALL" };
  }
  return recall.names.filter(function (name) {
    return name.indexOf("secret-") !== 0;
  });
}
```

两个阶段返回别的东西,召回就保持原样,而且一次请求绝不会只挪了一半:一个 `query`
没问题、`limit` 荒唐的答复,两样都不改。你自己编出来的名字会在下游被丢掉,而不是
报错。

### `model.request` — 那几个旋钮,就在发送之前

一次模型调用一次。你拿到的是这个请求里小的那部分:

```json
{
  "model": "claude-opus-5",
  "maxTokens": 8192,
  "thinkingMode": "auto",
  "fallbackModel": null,
  "tools": ["Read", "Write", "Bash"],
  "promptBlocks": ["scene.skeleton", "rules"],
  "messageCount": 24
}
```

**不含对话,也不含工具的 schema。** 平常一轮里这两样是几十 KB,每次模型调用都要
序列化一遍、解析一遍、再扔掉,而消息正文自己有两个更便宜的点——出去的路上是
`prompt.assemble`,回来的路上是 `model.message`。

返回一个对象,键取哪几个都行;没出现的键保持不动。

```json
{ "maxTokens": 2048, "model": "claude-haiku-5", "thinkingMode": "off", "tools": ["Read"] }
```

- `model`、`maxTokens`、`thinkingMode`、`fallbackModel` 直接替换请求里的那些。
  `fallbackModel: null` 是清空;把这个键留在外面是保持原样。`thinkingMode` 取
  `"auto"`、`"off"`、`"on"` 或者 `{ "on_budget": 4096 }`。`maxTokens: 0` 和空的
  `model` 会被拒绝,因为照这个建出来的请求发不出去,脚本的 bug 会以一个 provider
  错误的面目冒出来。
- `tools` 保留它点到名的工具,其余丢掉,顺序还是它们原来的顺序。请求里没提供的
  名字会被忽略——脚本变不出一个工具定义,所以这里只会收窄,永远不会放宽。`[]`
  是个正当的答案。

```js
// tests/fixtures/scripts/model_request.js
function onRequest(req) {
  return { tools: req.tools.filter(function (t) { return t !== "WebSearch"; }) };
}
```

整个对象是先读完再动手的,所以某个字段类型不对,请求是完全不变,而不是变了一部分。

### `model.message` — 一条写完的消息,在它被记下来之前

作用在整条消息上,绝不作用在流式增量上:助手这一轮的正文一次,它那批工具调用一次。
一次模型调用几次,不是几千次。

```json
{ "role": "assistant", "text": ["Here is what I found."], "toolUses": ["Read"] }
```

`text` 装的是这条消息的文本块,按顺序。`toolUses` 点出这条消息要用的工具名字,
不带它们的参数——单单一次 `Write` 调用就能带上一整个文件。一条没有正文的消息压根
不会调用脚本,因为正文是它唯一能改的东西。

返回一个字符串数组,长度跟给你的 `text` **完全一样**;每一项替换同一位置上的那个
块,`""` 把一块清空。

```js
// tests/fixtures/scripts/model_message.js
function onMessage(msg) {
  return msg.text.map(function (t) {
    return "SCRIPT-TRACE-MESSAGE " + t;
  });
}
```

长度不一样,就说不清多出来或少掉的那一项属于哪个块,所以跟其他形状不对的返回值
一样被丢掉。然后这条消息就照模型产出的原样记下来。

这里够不到的东西:**thinking 块和它们的签名**,那些必须一字不差地回传给 provider,
否则下一次调用会被拒;**图片**;还有**工具调用块**,它们的参数正是引擎马上要派发
的东西。

注意,改写一条消息影响的是模型在*下一次*请求里看到什么,不是正在飞的这一次。

## 预算

**时间。** 一次调用有 `timeout_ms`,默认 100。这一条强制了两遍:QuickJS 把这个期限
带在一个中断处理器里,它在字节码指令之间触发,所以连 `while (true) {}` 都能停下来;
载体又在异步那条路外面套了自己的超时,作为第二道。

**次数。** `calls_per_turn`,默认 1000,按绑定分别计,每个用户轮次开始时重置。一个
按工具调用触发的点,没法把一个病态的轮次变成一张病态的账单。

**内存。** 每个运行时的上限是 16 MB。

任何一项超了,失败的只是*那一次调用*,别的都不受影响。这一轮继续跑,只是脚本本该
贡献的那点东西没了。

## 失败

所有地方的规矩都是:**失败的脚本什么都改不了。** 抛异常、超时、配额耗尽、返回一个
这个点没法拿来做事的值——全都让这个点保持适配器发现它时的原样,并打一条点了脚本
名字的警告。每个适配器都是先把自己的改动完整算出来再动手施加,所以不存在什么施加了
一半的状态需要你去琢磨。

有两种失败发生得比这更早:

- **绑定**时,文件读不出来或者点写错了,那就一个脚本都不绑,daemon 记一条错误。
- **注册**时,`prompt.block`、`prompt.context` 和 `prompt.variable` 会在绑定过程中
  做那次身份调用。在那儿抛异常的脚本,或者根本没有那个名字的函数的脚本,什么都
  注册不上——一个没有名字的块寻址不到,一个没有确定 order 的块也放不下去。

## 派生出去的 agent

一个会话可以派生子 agent、团队成员和后台任务,每一个都跑自己的会话。你的脚本
跟不跟过去,按**这条线**分:

- **围在工具调用和模型调用外面的那四个点** —— `tool.around`、`tool.result`、
  `model.request`、`model.message` —— 跟过去。一条到第一次派生就失效的策略不算
  策略:一个"本项目禁用 Bash"的脚本,否则离被绕开只差模型的一句 `Agent` 调用,
  而做这个决定的不是任何人。`memory.retrieval_hook` 也跟过去,尽管派生出来的
  agent 通常没有自己的记忆库,所以那边多半不会触发。
- **四个提示词点** —— `prompt.assemble`、`prompt.block`、`prompt.context`、
  `prompt.variable` —— **不跟过去**。它们是对着绑定它们的那个会话的提示词写的,
  而派生出来的 agent 有自己的场景、自己的提示词。冲着一份提示词写的编辑,不是
  冲着另一份写的。

**载体不跟过去,配额跟着轮次。** 派生出来的 agent 拿到的是适配器,不是载体,所以
它的调用记在**父会话这一轮**的配额上,它自己的轮次也不会把父会话正在花的预算重置
掉。`calls_per_turn` 管的是整整一轮,包括这一轮派出去的活。

## 权限

一个脚本能做什么,是由**它的文件在哪儿**决定的,不由它自己或者它的绑定声明什么
决定——因为一个来自外面的脚本要撒谎,撒的正是这种声明。检查的是解析并规范化之后的
路径:

- **在项目根之内** —— 那是操作者自己写的,所以他手动改提示词能做的事,它都能做。
- **在别的任何地方**,包括随下载来的插件一起到的脚本 —— 它只能**新增**,别的都
  不行。修改、删除和重排,都需要一项它没有声明过的能力。

这一条只在 `prompt.assemble` 上真正咬人,那是唯一一个有东西可限的点。一次被拒的
编辑不会把获准的那些一起取消:这一遍的其余部分照常施加,而且这次拒绝是被报出来、
被计数的,不是被悄悄丢掉的,所以"这个脚本什么也没做"和"这个脚本正在被拦着不让做"
分得开。

```
plugin '/opt/pkg/x.js' may not modify the prompt block 'rules': it did not
declare that capability at install
```

一个来自项目外面的脚本会被记成 plugin 来源,消息里那么叫它就是这个缘故:这条轴讲
的是出身,不是打包方式。

另外八个点不按出身分岔,因为它们没有一个有可以分岔进去的"缩水模式"——召回过滤器
不存在"可以收窄但不许放宽"的版本,不像提示词装配那样有一个只许新增的编辑版本。

## 脚本不许绑到哪儿

目录里有四个点,契约上对脚本是开放的,而故意不给适配器:

- **`history.append_observer`** 每写一条日志就触发一次。那个频段是刻意对脚本关闭
  的:在那儿放一个回调要花多少代价,写这个回调的人看不见。
- **`history.extension_entry`** 是一项写能力,不是一个 hook。脚本需要的是一个能
  发出条目的 API,不是一个接收条目的回调。
- **`hooks`** 是它自己的子系统,有自己的进程模型。
- **`script.carrier`** 就是载体本身。

目录里其余的东西对脚本都标成 `closed` —— 那些是构建期就接好的 Rust trait,没有
留给脚本注册的口子。

## 接下来看哪儿

- `docs/testing_scripts.md` —— 这九个点怎么被测,以及一条用例要满足什么才算数。
- `tests/fixtures/scripts/` —— 每个点一个能跑的 fixture,每一个都留下一处引擎里
  别的地方绝不会产生的痕迹;`broken/` 下面是各种坏法。
- `tests/runner/tests/script_carrier.rs` —— 每个点都驱动一次真实会话,跑三遍:
  绑上、不绑、绑一个抛异常的。后面还有八条交叉用例,问的是两个点放在一起会怎样。
- `tests/runner/tests/script_boundaries.rs` —— 上面「预算」「失败」「权限」三节写的
  承诺,逐条对着跑起来的会话验一遍。
- `tests/runner/tests/script_task_profiles.rs` —— 九个点一起绑上,做一件真活儿,
  看工作目录还是不是原来那个。
- `crates/core/src/interface/script_adapters/` —— 每个点一个适配器。每个适配器实现
  的 JSON 契约就写在适配器自己身上,那是不会漂移的那一版。
- `docs/extension_points.md` —— 引擎里的每一个点,以及谁可以用它。
