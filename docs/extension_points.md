# 扩展点

宿主、插件或脚本能接进这个引擎的所有地方,各自要付什么代价,以及谁有资格用。

从你手上真正的那个问题开始:

- **"我想换掉引擎做 X 的方式。"** 你要的是**契约**——实现一个 trait,
  交给 `runtime::agent::Builder`。
- **"我想在引擎已经在做的事情上再添点东西。"** 你要的是**注册**——
  贡献一个具名、有序、可撤销的东西。
- **"我想看见、或者改掉正在进行中的东西。"** 你要的是**拦截**——
  坐进引擎正在做的那件事的路径里。

三者都是 Rust:实现那个 trait,交给 `runtime::agent::Builder`。要扩展一个不由你
编译的构建,就得借助载体——脚本(`extending_quickjs.md`)或者 WebAssembly 插件
(`extending_wasm.md`)。每种载体只够得着这里列出的一部分,各自那份文档会说清楚是
哪一部分。引擎本身怎么搭起来的,见 `ARCHITECTURE.md`。

## 这张表怎么读

`配置` / `脚本` / `插件` 是引擎区分的三种来源,而这条轴线分的不是权限高低,是作者
身份:这东西是运维者自己写的,还是下载来的。运维者自己项目里的脚本可以做任何他本
来就能亲手做的事,因为那就是他本人。装上的插件可以随便加,想做得更多则必须在安装
时声明能力——而装它的人当时看见了那份声明。

`不开放` 的意思是:这个点是构建期接好的 Rust trait,脚本和插件根本没有可以注册
进去的入口——只有嵌入它的那个程序够得着,别人都不行。

**频率是一条约束,不是花边。** 一个会话只触发一次的点养得起一个子进程;每个流式
分片触发一次的点什么都养不起,这就是这类点对脚本和插件干脆全关的原因。

<!-- BEGIN GENERATED TABLE — see base::interface::catalog::render_markdown -->

| 扩展点 | 类型 | 时机 | 可改什么 | 频率 | 配置 | 脚本 | 插件 |
|---|---|---|---|---|---|---|---|
| `tool.registry` | 契约 | 会话构建时,以及之后任何时候 | 有哪些工具、它们各是什么 | 每会话(10⁰ 量级) | 不开放 | 不开放 | 不开放 |
| `tool.around` | 拦截 | 分发前后,在权限和 hooks 的外侧 | 取消信号和结果;改不了输入 | 每次工具调用(10¹ 量级) | 完全 | 完全 | 需声明能力 |
| `tool.result` | 拦截 | 所有 hook 之后,模型看到它之前的最后一步 | 结果文本和其中的图片 | 每次工具调用(10¹ 量级) | 完全 | 完全 | 需声明能力 |
| `prompt.block` | 注册 | 提示组装时,每轮一次 | 只能新增 | 每轮(10⁰–10¹ 量级) | 完全 | 完全 | 只能新增 |
| `prompt.context` | 注册 | 提示组装时,每轮一次 | 只能新增 | 每轮(10⁰–10¹ 量级) | 完全 | 完全 | 只能新增 |
| `prompt.variable` | 注册 | 提示组装时,块合并之后 | 只有自己那个占位符,别的都不行 | 每轮(10⁰–10¹ 量级) | 完全 | 完全 | 只能新增 |
| `prompt.assembler` | 契约 | 提示组装时,取代引擎自己那套 | 顺序、缓存边界、合并策略——整个结果 | 每次模型调用(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `prompt.assemble` | 拦截 | 提示组装的最后一步 | 块的内容、顺序和取舍 | 每轮(10⁰–10¹ 量级) | 完全 | 完全 | 需声明能力 |
| `event.sink` | 契约 | 每次发射,在 sink 自己的 task 上 | 什么都改不了——只能观察 | 每个流式分片(10³–10⁴ 量级) | 不开放 | 不开放 | 不开放 |
| `health.check` | 注册 | 每当有人要一份健康报告 | 什么都改不了——检查只汇报,不修复 | 每进程(10⁰ 量级) | 不开放 | 不开放 | 不开放 |
| `elicitation.ask` | 契约 | 每当一个决定需要人来做 | 答案 | 每轮(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `permission.check` | 契约 | 每次工具调用之前 | 放行、拒绝,或者问人 | 每次工具调用(10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `scene` | 契约 | 会话构建时 | 一个 agent 怎么呈现自己,全部 | 每会话(10⁰ 量级) | 不开放 | 不开放 | 不开放 |
| `model` | 契约 | 每次模型请求 | 整场交互 | 每次模型调用(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `model.factory` | 注册 | 启动时,读 provider 配置的那一刻 | 哪些协议可以被配置出来 | 每进程(10⁰ 量级) | 不开放 | 不开放 | 不开放 |
| `model.request` | 拦截 | 每次模型调用之前的最后一刻 | 请求里的一切 | 每次模型调用(10⁰–10¹ 量级) | 完全 | 完全 | 需声明能力 |
| `model.message` | 拦截 | 承载它的那段流结束之后 | 消息内容 | 每次模型调用(10⁰–10¹ 量级) | 完全 | 完全 | 需声明能力 |
| `credentials` | 契约 | 启动时,读 provider 配置的那一刻 | 凭据本身 | 每进程(10⁰ 量级) | 不开放 | 不开放 | 不开放 |
| `config.source` | 契约 | 进程启动,任何东西被构建之前 | 有哪些层、每层里是什么 JSON;合并本身动不了 | 每进程(10⁰ 量级) | 不开放 | 不开放 | 不开放 |
| `token.count` | 契约 | 每次预算检查 | 压缩据以触发的那个数字 | 每轮(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `history.store` | 契约 | 每次追加,以及恢复时 | 日志怎么持久化、存到哪 | 每轮(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `history.query` | 契约 | 每当有人要的是若干会话而不是某一个会话 | 返回哪些会话、按什么顺序、怎么算匹配 | 每会话(10⁰ 量级) | 不开放 | 不开放 | 不开放 |
| `history.blob` | 契约 | 每次追加带图片或大负载时,以及加载时 | 大块内容存在哪、怎么寻址 | 每轮(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `history.projection` | 契约 | 每次读取转录:恢复、分叉、搜索、翻页 | 哪些条目变成消息、它们说了什么 | 每会话(10⁰ 量级) | 不开放 | 不开放 | 不开放 |
| `history.extension_entry` | 注册 | 任何时候;和其余条目同序 | 只能新增;内核从不解析 payload | 每轮(10⁰–10¹ 量级) | 完全 | 完全 | 只能新增 |
| `memory.storage` | 契约 | 召回时,以及每次写入记忆时 | 记忆怎么持久化、存到哪 | 每轮(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `memory.retriever` | 契约 | 每条用户消息一次,在后台 | 召回的集合 | 每轮(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `memory.retrieval_hook` | 拦截 | 召回前后 | 查询,以及召回的记忆名 | 每轮(10⁰–10¹ 量级) | 完全 | 完全 | 需声明能力 |
| `history.append_observer` | 拦截 | 每次追加成功之后 | 什么都改不了——只读是类型定死的,不是约定的 | 每轮(10⁰–10¹ 量级) | 完全 | 完全 | 只能新增 |
| `skill.source` | 契约 | 会话构建时,以及每当有 MCP server 接入 | 有哪些 skill、每个展开成什么文本 | 每会话(10⁰ 量级) | 不开放 | 不开放 | 不开放 |
| `instruction.source` | 契约 | 会话构建时 | 每会话注入一次的 AGENTS.md 文本 | 每会话(10⁰ 量级) | 不开放 | 不开放 | 不开放 |
| `rules.source` | 契约 | 系统提示组装期间 | 告诉模型存在哪些规则文档 | 每次模型调用(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `turn.policy` | 契约 | 每次模型调用之前,以及每次返回之后 | 循环还走不走下一步,以及上报的停止原因 | 每次模型调用(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `model.recovery` | 契约 | 出错时,以及响应撞上输出上限被截断时 | 换模型、压缩后重试、抬高上限,还是直接失败 | 每次模型调用(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `model.backoff` | 契约 | 在 client 内部、模型契约之下,每次失败的尝试 | 重试前等多久,以及到底有没有重试 | 每次模型调用(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `budget` | 契约 | 每次模型调用之后,以及每个请求组装之前 | 这轮还继不继续、被告知了什么、压缩的上限 | 每次模型调用(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `environment` | 契约 | 每当一个答案是被写下来而不是被测量出来的 | 日志时间戳、条目 id、提示里带的日期 | 每轮(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `exec.process` | 契约 | 工具启动的每一条命令 | 活儿在哪台机器上干 | 每次工具调用(10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `exec.filesystem` | 契约 | 工具的每一次读、写、stat | 工具看到的是哪个文件系统 | 每次工具调用(10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `exec.network` | 契约 | 每个出站请求;出网策略约束的是模型选的那些 | 请求去哪、去不去、谁来应答 | 每次工具调用(10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `exec.sandbox` | 契约 | 命令启动之前 | 真正跑起来的那条命令,以及它能碰什么 | 每次工具调用(10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `compaction` | 契约 | 每轮一次:两趟老化、一趟预测,最后是阈值 | 消息历史,以及到底要不要重写 | 每轮(10⁰–10¹ 量级) | 不开放 | 不开放 | 不开放 |
| `script.carrier` | 契约 | 载体被绑到哪就在哪;受每轮配额约束 | 所绑那个点允许的一切,按脚本自己的来源定级 | 每轮(10⁰–10¹ 量级) | 完全 | 完全 | 需声明能力 |
| `hooks` | 拦截 | 三十个具名时刻;见 hook 事件清单 | 按事件而异:拦截、改写输入、结束这一轮 | 每次工具调用(10¹ 量级) | 完全 | 完全 | 需声明能力 |
<!-- END GENERATED TABLE -->

上面这张表由 `base::interface::catalog` 生成,这份文件一旦和它对不上,
`daemon/tests/extension_points_doc.rs` 就会失败。要改改 catalog,别改表。

---

## 契约——换掉一个子系统

下面每一个都是你实现之后交给 builder 的 trait。它们对脚本和插件一律`不开放`,
因为它们是在构建期用 Rust 接好的。

### 工具集 — `tool.registry`

```rust
let tools: Arc<dyn base::tool::ToolRegistry> = Arc::new(MyRegistry::new());
let (agent, events, input) = Builder::new().tools(tools).model(model).build()?;
```

`register` / `replace` / `remove` 各自返回一个 `Disposer`;把它 dispose 掉,工具
就被取回来,模型也就看不见它了。`base::tool::LayeredToolRegistry` 是一个做好的
范例——它包住另一个 registry,在不改动对方的前提下把工具藏起来。

### LLM 后端 — `model`、`model.factory`、`credentials`

`base::interface::model::Model` 是一个协议。要让一个协议变得*可配置*——也就是在
`settings.json` 里写一句 `api_type` 就能用上——得注册一个 `ModelFactory`:

```rust
let mut factories = model::factory::builtin_registry();
factories.register(Arc::new(MyProtocolFactory));
let router = daemon::model_router::build_task_router_with(
    &providers, "default", resolved, &factories,
)?;
```

注册一个已经存在的 `api_type` 就是替换它。宿主正是这么把自己的 client 塞到
`anthropic` 背后的:不必另发明一个名字,再逼用户去记。

凭据来自 `CredentialSource`,工厂从不直接读 `config.api_key`。缺省的
`ConfigCredentials` 按厂商条目自己写的来:先 `api_key`,没有就读 `api_key_env`
指名的环境变量。宿主要接自己的密钥库,用 `ConfigThen` 把它挂在后面——条目里写了的
照用,没写的问宿主:

```rust
let factories = model::factory::builtin_registry()
    .with_credentials(Arc::new(ConfigThen(Arc::new(MyVaultCredentials))));
```

daemon 的嵌入方不必自己拼:`Assembly { credentials: Some(..), .. }`,每个绑定建
路由时都走这条链;没有任何厂商路由时的缺省 Anthropic 客户端先看
`ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`,再以厂商 id `"default"` 问它。

拿回来的是 `Secret`:没有 `Display`,没有 `Serialize`,`Debug` 打出来是
`<redacted>`。要读它得调 `.expose()`。

### 配置从哪来 — `config.source`

`Settings::load` 读六个文件:全局、scene、项目三个层级,每级一个 `settings.json`
和一个进了 gitignore 的 `settings.local.json` 覆盖层。配置存在配置服务、
ConfigMap 或者数据库里的部署,实现
`base::interface::config_source::ConfigSource`,改调 `Settings::load_from`:

```rust
let source = config_source::Chain(vec![
    Arc::new(config_source::FileTiers::new(global.clone(), scene.clone(), project.clone())),
    Arc::new(control_plane.fetch_layers()?),   // an `InMemoryLayers`
]);
let settings = Settings::load_from(&source, global, scene, project, "code", "opus");
```

一个 source 只决定有哪些层、顺序如何、每层里是什么 JSON。合并两边是同一套——每层
都剥掉 `paths`,每层的 `permission_rules` 单独拎出来按层拼接(不参与覆盖),其余
靠后的层递归盖在靠前的上面——所以把配置从磁盘上搬走,改不了一个配置文件的含义。

那几个目录参数即使 source 一个字节都不从里面读也得留着:它们是 scene 的*数据*待
的地方,而这件事从来轮不到配置文件来决定。

`layers()` 返回的是层而不是 `Result`,因为 `Settings::load` 从不失败。够不着自己
那份存储的 source 记下原因,把手上有的交出来:丢一层是坏事,而因为一个远端服务慢
就拒绝启动,更坏。

### 工具授权 — `permission.check`

`base::interface::permission::Permission` 决定一次工具调用跑不跑。返回 `Prompt`
不是做决定,而是把这个问题经 [`elicitation.ask`](#向人提问--elicitationask)
交给人。

```rust
Builder::new().permission(Arc::new(MyGate));
```

给需要查状态的 handler 准备了两个绑定方法:`bind_tool_registry` 把分发真正在用的
那个 registry 交给 handler(绑到别的 registry 上的 handler 看不见之后注册的工具,
它那条"未知工具"的分支就成了一个洞),`bind_session_state` 让它持续看得见会话中途
的模式切换。两者对不需要它们的 handler 都默认是空实现。

### 一个 agent 怎么呈现自己 — `scene`

`base::interface::scene::AgentScene` 持有系统提示骨架、工具白名单和预算:

```rust
Builder::new().scene(Arc::new(MyScene));
```

引擎自带四个 scene,其中三个写成了照着抄的阶梯:一个完整参考、一个精简的、一个最
小骨架。自己给提示分节命名的 scene 保住那些名字,收在 `scene.` 前缀下——见下面的
块名。

### 会话存储 — `history.store`

`history::store::HistoryStore`。自带两个实现:`JsonlHistoryStore`(项目目录下的
文件)和 `InMemoryHistoryStore`。契约的保证写在它的文档注释里;
`store::contract_tests` 拿同样六条性质同时压这两个实现,第三个后端也该指到这里来。

### 找会话 — `history.query`

`HistoryStore::find_sessions` 收一个 `history::query::SessionQuery`——一根针、一个
范围、一个上限——回一串摘要,最新的在前。按时间列表和按文本搜索是同一个问题的有针
和无针两种情况,所以它们才是一个方法:能便宜地回答其中一个的后端,两个都能便宜地
回答。

默认实现要把范围内每个会话都读一遍才答得出来,`JsonlHistoryStore` 也只是靠文件
mtime 排序、按目录收窄而已。有真索引的后端覆盖这一个方法,就把搜索整个接管过去了;
它必须守住的保证——最新在前且是全序、不超过 limit、匹配数不少于一次大小写无关的
子串扫描——写在这个方法的文档注释里。

### 大块内容 — `history.blob`

`history::blob::BlobStore`。内容超过一千字节的 `User`、`Assistant` 或
`ToolResult` 条目,或者任何尺寸下带图片的条目,都会写进 blob store,在 JSONL 里换
成一条点名 store 和 id 的 `LogEntry::Blob`。带着那个 store 加载会把原条目放回去;
`HistoryStore` 之上的一切只见得到已经补全的条目。

```rust
JsonlHistoryStore::with_roots(cwd, roots).await?.with_blob_store(my_store)
```

自带两个实现:`PasteStore`(`<base>/pastes/` 下按内容寻址的文件,默认那个)和
`InMemoryBlobStore`。一个实现必须按内容寻址,对手上没有的内容必须回 `None` 而不是
报错,而且必须让自己的 `name` 保持稳定——那个名字是写进日志里的。

**解析不了的引用是惰性的,不是错误。** 卸掉后端、拷一份不带 blob 的日志、或者让
清理跑一次,会话照样加载、分叉、恢复,只是内容那块留个空。这和
`history.extension_entry` 是同一条规矩,理由也一样:因为一段对话有一部分够不着就
拒绝打开它,是拿一个降级的会话换了一个没有的会话。

### 记忆 — `memory.storage`、`memory.retriever`

storage 是记忆待的地方;retriever 决定一轮能看见其中哪些。自带的 retriever 去问
模型,代价是一次调用:

```rust
Builder::new().memory_retriever(Arc::new(MyIndexRetriever))
```

`base::interface::memory_contracts::SubstringRetriever` 不要钱,而且是 store 的
纯函数——一个断言召回的测试要的正是这个。

### 向人提问 — `elicitation.ask`

引擎要问人的三件事共用一个 trait:这个工具能不能跑、你刚才是什么意思、这个要不要
导入。

```rust
Builder::new().elicitation(Arc::new(MyDialogs))
```

什么都没注册的时候,每个问题都会*带着理由被拒绝*。沉默从来不等于同意。

### 事件去哪 — `event.sink`

```rust
Builder::new().event_sink(sink)
```

每个 sink 有自己的队列和 task,所以慢的那个只丢自己的事件,不占别人的时间。它拖不
慢一轮。

### 判断上下文有多大 — `token.count`

`base::interface::token_counter::TokenCounter`。默认是本地的 `cl100k_base` 估算,
会偏高 5–15%,因为 Anthropic 没有公开 tokenizer;算得准的宿主就该自己算。

### 够到机器 — `exec.process`、`exec.filesystem`、`exec.network`、`exec.sandbox`

四个契约是当成一个设计的,因为它们缠在一起:沙箱约束进程,进程要文件和网络,而网络
策略又必须伸进沙箱里面去。推理过程在 `ARCHITECTURE.md` §5,这里是摘要。

```rust
let mut ctx = /* … */;
ctx.exec = ExecProviders::in_process();   // switching is this call
```

自带两套 provider。`ExecProviders::local()` 就是本机,到哪儿都是默认。
`ExecProviders::in_process()` 是一棵内存里的树、一批事先定好的命令、一个只回答被
喂过的东西的网络,以及一个如实报告自己什么都没约束的沙箱——配上 `FixedEnvironment`,
整个会话跑得起来也放得回去,不碰任何东西。

自己写一个 provider 之前,有三个形状值得先知道。

**`Process` 是流式的,`FileSystem` 不是。** 长命令的输出必须在它还在跑的时候就到
用户眼前,所以 handle 吐的是带管道标记的分片;一个到最后才一次性返回的 provider 会
把这件事悄悄取消掉。文件是整个值,因为每个调用点要的都是整个文件,而且工具结果的
上限远在一个文件需要分片之前就已经卡住了。

**路径安全在契约之上,规范化在契约之内。** 一个路径能不能写是策略,而一个自己定
越界规则的 provider 可以把这些规则取消掉。但远端的符号链接图是远端的,所以顺序
是:经 provider 规范化,检查解析出来的那个路径,再经 provider 写。

**出网策略问的是*模型*能够到哪。** 每个请求都带着"是谁选的目的地"。
`allowed_domains` 约束 `Origin::Agent`——一个 WebFetch 的 url、一个 Ping 的主机
——而不约束 `Origin::Operator`,也就是模型端点、MCP server、遥测。把它套到所有东西
上,agent 就连自己的模型都够不着了。

运维者自己的流量也是经同一套契约建起来的,但宿主目前还换不掉那些 client 用的
provider——模型、OAuth、遥测和 registry 的 client 各造各的。所以换掉这个点管得住
模型能够到哪,而*整体地*审计或者离线,今天它还给不了。

沙箱有一条不变量是引擎强制而不是请求的:**要求过约束的策略,绝不会悄悄变成一次不
受约束的运行。** 后端报告 `Full`、`Partial` 或 `None`,并说清自己没能兑现什么;
`sandbox.require_enforcement` 决定不到 `Full` 要不要拒。约束什么、要不要约束,始终
是内核的事——provider 只回答*怎么*约束。

### 压缩 — `compaction`

`compaction::compact::Compactor`。这个问题的两半都在:一段对话怎么被缩短,以及什么
时候缩。

一轮里有四个机会,契约对每一个都作答——两趟按年龄清工具结果的老化、一趟在预算撞上
之前就开火的预测,以及那个强制变换的阈值。每一个都有默认实现,就是引擎自己那套算术,
所以只关心变换本身的实现者写一个 `compact` 就完了。`tool_result_budget` 是同一个
想法的纯数字版:改累积的工具结果最多能占一个请求的多少,不必把"占满了怎么办"重新
实现一遍。

```rust
Builder::new().compactor(Arc::new(OnlyAtThreshold(DefaultCompactor)))
```

`OnlyAtThreshold` 包住任何一个 compactor,把不是预算逼出来的都推掉——一个部署在
"除非重写实在避不开,否则别动我的转录"时想要的就是这个形状。

### 一轮什么时候算走得够久了 — `turn.policy`

`base::interface::turn_policy::TurnPolicy`。两个上限——每轮的模型调用数、结构化
输出的重试数——在今天真正检查它们的那两个点上被问到,因为把它们合并会重排整个循环。

```rust
Builder::new().turn_policy(Arc::new(FirstOf(vec![engine_default, mine])))
```

要组合而不是替换:`FirstOf` 保住引擎的限制再加上你的,而且一个"停"永远不会被后面
的策略推翻。

只有关于*进展*的判断住在这里。取消是一条指令,不是一个意见;`PostToolUse` 或
`Stop` hook 结束一轮,本来就已经是某个扩展点的输出,让一个策略去否决它就把信任
次序颠倒了;而"模型要了工具,所以还有事做"是这个循环的定义,不是对它的判断。

### 一次调用出错的时候 — `model.recovery`、`model.backoff`

`base::interface::recovery_policy::RecoveryPolicy` 决定一次失败*对这一轮意味着
什么*:切到备用模型、压缩后重试、抬高输出上限,还是就此失败。分类归引擎——哪些
错误*是*过载、哪些*是*尺寸拒绝,是关于线协议的事实。`NeverRecover` 是一份真配置
而不是占位:按 token 计费的部署宁可返回一个被截断的答案,也不想要一次意外的 64K
调用。

`base::interface::backoff::BackoffPolicy` 坐在模型契约之下、client 之内,回答更窄
的那个问题:失败的请求还发不发,发之前先等多久。两套线协议走的是同一个。
`retry-after` 的解析留在 Anthropic client 里,因为那是协议特有的输入;拿它做什么
则是策略的事。

### 一轮能花多少 — `budget`

`base::interface::budget_policy::BudgetPolicy`。三个本来是常量的判断:累计 token
上限、带边际递减规则的输出量目标,以及 scene 返回的压缩阈值。

原本完全缺席的那一个,是请求*尺寸*的上限。阈值是一个触发器——越过它就开始压缩,而
压缩要是压不到位,请求照样发出去。`Capped` 把上限补上了:压缩之后仍然超标的一轮以
`context_exceeded` 结束,而不是把部署声明过绝不能发的东西发出去。

```rust
Builder::new().budget_policy(Arc::new(Capped {
    inner: EngineBudget::new(settings.execution.max_budget_tokens),
    context_hard_cap: 120_000,
}))
```

**给一次执行卡预算,应该卡在这里,不是在 `TurnOutcome` 上。** `on_usage` 在**每次
模型调用之后**被调,拿到的 `Spend` 是这一轮到此为止的累计——所以超了可以当场把这一轮
停掉,而不是等到结束才知道花了多少。`TurnOutcome.total_usage` 回答的是"花了多少",
这里回答的是"还能不能继续花"。

**`Spend` 把 provider 报的四项分开给,不给一个和。** 哪些算"花掉了"是策略的判断,
不是引擎的:缓存读按普通输入的一个零头计费,全额计入会高估账单,不计入又会让一轮
缓存命中的执行少算掉它读的绝大部分。两种都说得通,所以引擎只负责如实报。
`Spend::total_tokens()` 是 input + output(引擎自带的 `EngineBudget` 用的就是它,
`max_budget_tokens` 的含义因此不变),`Spend::all_tokens()` 是四项全含,想要第三种
口径的自己按需加权:

```rust
fn on_usage(&self, spend: &Spend) -> Spending {
    // 例:按费率折算成"等效输入 token"再卡
    let weighted = spend.input_tokens
        + spend.output_tokens * 5
        + spend.cache_creation_tokens * 5 / 4
        + spend.cache_read_tokens / 10;
    if weighted >= self.cap { Spending::Exhausted { limit: self.cap } }
    else { Spending::WithinBudget }
}
```

### 一份日志对模型意味着什么 — `history.projection`

`history::transcript::TranscriptProjection`。哪些条目变成消息,以及它们说什么。它
挂在 store 上而不是引擎上,因为一份日志和读它的规则是一起走的:恢复、分叉、搜索、
翻页必须看见同一段对话。

```rust
JsonlHistoryStore::with_roots(cwd, roots).await?
    .with_projection(Arc::new(ExtensionsAreVisible { namespaces: vec!["com.acme.deploy".into()] }))
```

`ExtensionsAreVisible` 回答的正是 `history.extension_entry` 提得出来、自己却了结
不了的那个问题:让一个扩展自己的条目变成模型读得到的东西。

答"是"要担一份义务。**模型可见的内容必须能仅凭日志重建出来**——一个靠问活着的扩展
"这条什么意思"来渲染条目的 projection,产出的是一段扩展一没了就再也打不开的对话。
`transcript::model_visible_content_is_reconstructible` 让这件事可查,而不是停在
口号上。

### 被记下来的时间和标识符 — `environment`

`base::interface::environment::Environment`。墙上时钟的此刻,和新鲜的 id。

只有会被*留下来*的答案从这里出:写进日志的时间戳、给条目命名的 id、提示告诉模型的
日期。契约上刻意没有单调时钟——`Instant` 既存不下也传不出,用它的地方全是测量,而
测量正是这个契约不管的那一类。

```rust
let env = Arc::new(FixedEnvironment::epoch());
let store = JsonlHistoryStore::with_roots(cwd, roots).await?.with_environment(env.clone());
let agent = Builder::new()./* … */.history_store(store).environment(env).build()?;
```

要配两处,因为日志那一半是 store 写的,模型可见那一半是 agent 写的。两个都配上,
否则重放会在你漏掉的那一半上对不上。

builder 会把同一个 environment 交给它自己创建的记忆 store,所以长期记忆也是照着它
变旧的——而这决定了它们中哪些会被端到模型面前。用 `Builder::memory_store` 传进来的
store 由造它的人自己配:`MemoryStore::new(user, local).with_environment(env)`。

### skill 从哪来 — `skill.source`

`base::interface::skill_provider::SkillProvider`。自带三个实现,覆盖 skill 一直
以来的三个来源:`skills::sources::SkillDirectory`(一层目录)、`BundledSkills`
(编进二进制的内建)和 `McpSkills`(一台连上的 server 的工具)。第四个是加进去,
不是替掉:

```rust
Builder::new().skill_provider(Arc::new(StaticSkills::new("company", entries)))
```

这样注册的 source 让位给已经加载好的东西。有意要替掉引擎自带某个 skill 的,返回
`SkillPrecedence::Override`。

`SkillProvider::body` 是这里为什么是"源"而不是"列表"的原因:一个正文存在源里、不
在文件里的 skill,被调用时照样展得开。
`base::interface::skill_provider::StaticSkills` 是给两样都已经攥在手里的宿主准备
的内存实现。

### 常驻指令与规则索引 — `instruction.source`、`rules.source`

`base::interface::instruction_provider::InstructionProvider` 决定 `AGENTS.md` /
`CLAUDE.md` 注入的是什么;`RuleProvider` 决定告诉模型存在哪些规则文档。
`InstructionFile` 和 `RuleDirectory` 是文件系统实现,`InlineInstructions` 和
`StaticRules` 是内存实现。

```rust
Builder::new().instruction_provider(Arc::new(InlineInstructions::new(
    "service://conventions",
    conventions_text,
)))
```

全局、scene、项目这三层规则各是一个 `RuleProvider`,由
`base::rules::default_rule_sources` 组合、由 `discover_rules_from` 按后来居上合并,
所以换一套层级列表是换一个组合,而不是改发现函数。

---

## 注册——贡献一份

### 提示块 — `prompt.block`、`prompt.context`、`prompt.variable`

```rust
use base::interface::prompt_registry::{orders, InMemoryPromptRegistry, RegisteredBlock};

let registry = InMemoryPromptRegistry::new();
let handle = registry.register_block(
    RegisteredBlock::system("mine.preamble", orders::SCENE + 50, "read this first"),
);
Builder::new().prompt_registry(registry.clone());
// later
handle.dispose();
```

块按 `order` 升序排,同序的按注册顺序。内核的各个阶段坐在整百上
(`orders::SKILLS_CATALOG` 是 100,`MEMORY_SESSION` 200,`RULES` 300,
`MCP_INSTRUCTIONS` 400,`CONFIG_PROMPT_APPEND` 500),所以任意两个之间都有九十九个
位置可站,而负数 order 排在 scene 前面。

`register_context` 是同一件事,只不过文本在组装时才算;返回 `None` 就一个块都不
贡献——一份贡献想在自己没什么可说的会话里彻底闭嘴,就是这么做的。
`register_variable` 让 `{{name}}` 在所有地方展开;没注册过的占位符,或者提供者
选择不答的,原样留着,一个字不动。

### 内核的块名

这些是**公开的契约**。一个扩展把自己相对某个块定位的时候,靠的就是这个名字继续是
这个意思。

| 名字 | 是什么 |
|---|---|
| `scene.skeleton` | scene 的提示,在这个 scene 不给分节命名的时候;第二段起依次是 `scene.skeleton.2`、`.3` |
| `scene.<section>` | 自己给分节命名的 scene 保住那些名字,加上前缀 |
| `skills.catalog` | 可用 skill 的清单 |
| `memory.session` | 怎么用基于文件的记忆系统 |
| `rules` | `.atta/rules/` 的发现索引 |
| `mcp.instructions` | 来自已连接 MCP server 的指令 |
| `config.prompt_append` | `settings.prompt_append` |
| `config.prompt_override` | `settings.prompt_override`,它把一切都替掉 |

每个块还带一个 `origin`——`Kernel`、`Config`、`Plugin(name)` 或 `Script(path)`
——组装 hook 的权限规则读的就是它。

### 在会话日志里放自己的状态 — `history.extension_entry`

```rust
store.append(session, LogEntry::Extension {
    ns: "com.example.myplugin".into(),
    event: "checkpoint".into(),
    payload: serde_json::json!({ "step": 3 }),
}).await?;
```

内核从不解析 payload。命名空间没人认领的条目会被一路带着走,除此之外不闻不问,所以
卸掉一个插件之后,它留下的老会话照样加载、分叉、恢复,和以前一模一样。

### 东西是不是正常 — `health.check`

```rust
struct QueueDepth(Arc<Metrics>);

impl HealthCheck for QueueDepth {
    fn name(&self) -> &str { "acme.queue" }
    fn check(&self) -> CheckResult {
        match self.0.depth() {
            d if d < 1000 => CheckResult::ok("queue drained"),
            d => CheckResult::degraded(format!("{d} queued"))
                .with_details(serde_json::json!({ "depth": d })),
        }
    }
}

let agent = Builder::new()./* … */.health_check(Arc::new(QueueDepth(metrics))).build()?;
let health = agent.health();          // take this before spawning the engine
let report = health.report();         // fresh answers, every time
```

报告里带着每个已注册检查的判定,以及其中最糟的那个。引擎自己的检查由接线的人注册:
`daemon` 注册了配置层级、provider 路由、hooks 配置和插件故障记录,整套挂在
`daemon.doctor` 的 `health` 键下报出去。

有两条规矩是契约强制的,不是请求的。**检查只汇报,绝不修复**——没有任何一个返回值
能重新合上熔断器、重载配置或者重启什么,因为一个偷偷把事修了的诊断,描述的是它刚
干了什么,而不是它发现了什么。以及**`check()` 是同步的,而且被要求用它手上已有的
状态作答**:一个阻塞在自己正在探测的子系统上的探针,恰恰在答案最要紧的时候挂住。
需要走网络的检查,自己在带外维护一份缓存判定,报那份。

没有 `Err`。判定不了的检查其实已经判定了点什么——它说 `Degraded`,把原因写进摘要,
而不是把"一个失败的检查算不算不健康"甩给每一个调用方各自决定。
### 提示组装本身 — `prompt.assembler`

`base::interface::prompt_assembler::PromptAssembler`。上面那些注册开的是什么进
提示,`prompt.assemble` 开的是成品之后怎么办;这一个开的是中间那段——各阶段按什么
顺序摆、一份贡献的 order 怎么和内核的合到一起、缓存边界落在哪、两个块还算不算两个
块。

```rust
Builder::new().prompt_assembler(Arc::new(MergedSystemPrompt::default()))
```

`DefaultAssembler` 是引擎自己的组装,也是默认。`MergedSystemPrompt` 包住任何一个
assembler,把它的系统块折成一个——提示贡献者比一个请求允许的四个 `cache_control`
断点还多的部署要的正是这个:一个缓存前缀,而不是随便凑出来的四个。

assembler 收到的是一个 `AssemblyRequest`——registry、scene、settings、记忆 store、
scene 上下文,以及已经渲染好的 skill 清单和 MCP 指令。它是个 struct,这样只读其中
两项的实现不必把另外五项重述一遍。

---

## 拦截——坐在路径上

### 围住一次工具调用 — `tool.around`

```rust
#[async_trait]
impl ToolMiddleware for Deadline {
    async fn around(&self, call: &ToolCall, exec: &mut ToolExec, next: NextDispatch<'_>)
        -> ToolOutcome
    {
        if call.name == "Bash" { exec.with_timeout(Duration::from_secs(30)); }
        next.run(call, exec).await
    }
}
Builder::new().tool_middleware(Arc::new(Deadline));
```

把信号收窄(一个超时)、不往下走直接作答(一个缓存)、或者往下走不止一次(一次
重试)。你改不了这次调用的参数——调用是以共享引用送到的,而 `run` 一个参数都不收。
`with_timeout` 派生的是当前 token 的*子* token,所以包装者能让一次调用比整轮更早
结束,而绝不会更晚。

自己造一个结果的时候(缓存那种),`ToolOutcome` 的 `Ok` 装的是 `ToolAnswer`:
`ToolAnswer::text(..)` 是答上了,`ToolAnswer::failure(..)` 是跑完了但这活没成。
后者和 `Err` 不是一回事——`Err` 是"这个工具根本没跑起来",它会连带取消同一批里
并发的其他调用,而 `failure` 只是让模型收到的那个结果带上 `is_error: true`。

包装按注册顺序嵌套,先注册的在最外面。

### 一个工具结果能长什么样 — `tool.result`

```rust
Builder::new().tool_result_transformer(Arc::new(RedactLiterals::new([api_key])));
```

跑在**最后**——所有 hook 之后,模型读到它之前——正是这一点让一个做脱敏的
transformer 成为保证而不是建议。`TruncateText` 和 `RedactLiterals` 自带,但都不
默认注册。

### 组装好的提示 — `prompt.assemble`

```rust
impl AssemblyHook for Mine {
    fn on_assemble(&self, asm: &mut PromptAssembly, ctx: &ScenePromptContext<'_>)
        -> Result<(), String>
    {
        asm.insert_after("scene.skeleton", PromptBlock::system("…").named("mine.note"));
        asm.modify("skills.catalog", curated)?;   // needs authority
        Ok(())
    }
}
```

`push` 和 `insert_after` 永远允许;`modify`、`remove` 和 `move_before` 需要权限。
以 `Authority::local(BlockOrigin::Script(path))` 注册的 hook 权限全有。以
`Authority::plugin(name, caps)` 注册的只有 `caps` 声明过的那些,而一次被拒的编辑
返回 `Denied` 并记在这次组装上,不会无声无息地失败。

### 模型请求与成品消息 — `model.request`、`model.message`

```rust
Builder::new().model_interceptor(Arc::new(Mine));
```

`on_request` 在任何东西离开进程之前看见消息、工具和参数。`on_message` 在承载它的
那段流结束之后看见一条完整的消息。

**没有按分片的 hook,默认情况下也不会有。** 一轮产出上千个分片;放在那里的回调会
被调上千次,而这份代价对写它的人是看不见的。而且一个能改写分片的 hook,也能产出一
条从来没有作为完整体存在过的消息。要在流到达的同时变换它,请提一条引擎能原生执行
的声明式规则。

### 围住一次记忆召回 — `memory.retrieval_hook`

`before_retrieve` 改问题,`after_retrieve` 改答案。一个部署知道一些关于自己词汇的
事,retriever 不知道;也知道一些关于自己策略的事,retriever 本就不该知道。

### 看着日志 — `history.append_observer`

```rust
let store = Arc::new(ObservedHistoryStore::new(inner, vec![my_observer]));
```

只读是类型定死的,不是约定的:条目以共享引用送到,而且什么都不返回。观察者在追加
成功之后才跑,所以一次失败的写入永远不会被观察到。

`history::observers::AppendCounts` 自带一个:每个会话进了多少条、都是什么类型,
一直记着账。它回答的那个问题——这个会话到底长到多大了——否则每问一次就得把整份日志
读一遍、解析一遍。持有 store 持有的那同一个 `Arc`,并在一个会话用完时调 `forget`,
否则那张 map 会一直长到进程结束。

### 生命周期 hook — `hooks`

三十个具名事件,五种后端(command、prompt、HTTP、agent、wasm),在 `settings.json`
里配。有些事件能解析、能接受配置,但还没有任何地方触发它们;`hooks::UNWIRED_EVENTS`
是那份清单,配到这类事件时引擎会告警,而 `daemon/tests/hook_event_wiring.rs` 从两个
方向盯着这份清单不许说谎。

---

## 在一个点上跑自己的代码 — `script.carrier`

`base::interface::script::ScriptEngine` 是便宜的那一档:运维者自己的一小段代码,
就在本进程里,以微秒计——夹在"重新编译引擎"和"起一个子进程"之间。

引擎是 QuickJS,在 `crates/script-host`,藏在 `daemon` 的 `scripts` feature 后面。
`base` 只放契约——它没有任何内部依赖,把一个 JavaScript 运行时放在那儿,会让所有
东西的每一次构建都带上它。

下面这份摘要够你判断要不要用它。**`extending_quickjs.md` 才是指南**——每个可绑定的
点一节,连输入和返回的形状,外加一个能跑的例子。

绑一个脚本:在你的项目里写一个,在 `settings.json` 里点它的名:

```jsonc
// .atta/scripts/prompt.js
function onAssemble(blocks) {
  return blocks.map(b => {
    if (b.name === "skills.catalog") b.content = curate(b.content);
    return b;
  });
}
```

```jsonc
// settings.json
"scripts": [
  { "path": ".atta/scripts/prompt.js", "point": "prompt.assemble", "entry": "onAssemble" }
]
```

可选的 `timeout_ms` 和 `calls_per_turn` 能把预算再收窄。不涉及任何重新编译;一个
文件加一行配置,就是全部。

**权限跟着文件的位置走,不跟着配置走。** 项目根目录里面的脚本是运维者自己的,可以
改写任何东西;在别处的是从外面来的,和任何下载来的扩展一样只能加。绑定里的任何东西
都改不了这一点,因为"声明"恰恰是一个外来脚本会在上面撒谎的东西。

一条绑定点名了一个不存在的点、或者一个脚本不许绑的点,会在启动时被拒绝并点名——而且
一条坏绑定会让整组绑定全部作废,而不是应用一半。一个悄悄不跑的脚本,会让它的作者跑
去自己的 JavaScript 里找一个不存在的 bug。

今天能绑的点是 `prompt.assemble`,再没别的。这份清单短是故意的:每一条都是一个脚本
的代价有界、权限有定义的地方,所以要加一条,得把这两个问题都回答了,而不是往数组里
追加一个字符串。

**一个脚本能够到什么:什么都够不到。** 没有文件系统,没有网络,没有宿主绑定,也没有
任何能活过一次调用的状态——每次调用拿到的是一个全新的运行时,所以一个会话的脚本没法
给另一个会话留下任何东西。

`ScriptCarrier` 从引擎外面强制每轮配额和墙钟预算,因为一个自己管自己限额的引擎可以
选择不管。引擎还额外把 deadline 带进 QuickJS 的中断处理器,真正停下 `while(true){}`
的是它——超时只能丢弃一个 future,停不住一个忙循环。

---

## 一个脚本能绑到哪些点上

上面那张表的`脚本`列说的是*契约*开了什么。QuickJS 载体给其中九个做了适配器,绑到
别的点上会在启动时被拒绝并列出能用的那些——一个悄悄不跑的脚本,比一个加载失败的脚本
更糟。

```jsonc
"scripts": [
  { "path": ".atta/scripts/prompt.js", "point": "prompt.assemble", "entry": "onAssemble" }
]
```

| 点 | 什么时候跑 | 脚本可以做什么 |
|---|---|---|
| `prompt.assemble` | 提示组装完之后 | 改写块,受自己的权限限制 |
| `prompt.block` | 组装时 | 加一个具名的块 |
| `prompt.context` | 组装时,每轮一次 | 加一个正文由它算出来的块 |
| `prompt.variable` | `{{name}}` 出现的任何地方 | 提供那个值 |
| `tool.around` | 分发之前 | 拒绝、代它作答,或者把时限调短 |
| `tool.result` | 模型看见一个结果之前 | 改写它的文本 |
| `memory.retrieval_hook` | 召回的两头 | 改查询,过滤回来的东西 |
| `model.request` | 每次模型调用 | 换模型、改上限、改 thinking 模式;收窄工具列表 |
| `model.message` | 每条完成的消息 | 改写文本块 |

每个适配器自己的文档带着那份 JSON 契约——脚本收到什么、可以返回什么。那是一个脚本
作者唯一能看的文档,所以它待在那份必须让它保持为真的代码旁边。

### 保持关闭的那四个,以及为什么

**`history.append_observer`** 每条日志条目触发一次。那正是 catalog 刻意对脚本关闭
的频率档:放一个回调在那里的代价,对写它的人是看不见的,而且那是按*条目*算,不是按
轮算。

**`history.extension_entry`** 是一项写能力,不是一个 hook。脚本需要的是一个*发出*
条目的 API;一个接收条目的回调是另一回事,只是顶着同一个名字。

**`hooks`** 是自成一体的子系统,有自己的进程模型。把脚本引擎绑到那儿,等于给那个
子系统已经在做的事再开一条路。

**`script.carrier`** 就是那个载体本身。

### 每个适配器保证什么

**一个失败的脚本什么都改不了。** 超时、抛异常、耗光配额、返回错的形状——全是同一个
结果,而那个结果就是"什么也没做"。一个被半途死掉的脚本改了一半的点,比一个没被改过
的点更糟,因为下游没有任何东西分得清自己看的是哪一种。"返回了胡话"和"有 bug"拿到
同一个无害的答复,是故意的。

**一个脚本永远不会把自己的权限撑大。** 来源跟着载体走:运维者自己写的脚本可以改写,
跟着下载来的插件一起到的只能加。适配器读这件事,不决定这件事。

**一个脚本拿到的引擎,永远不超过那个点需要的。** 一次模型请求交给它的是旋钮而不是
消息;一份提示贡献交给它的是环境而不是别的块;一条消息交给它的是文本而不是它的
thinking 签名。每一条排除都在它被做出的地方给出理由,因为这里面每一条的诱人写法都是
"把整个结构体传过去"。

**配额是按轮算的,而且是一轮真的说了算。** 载体在每轮开头被重置;没有这一条,预算
就成了按会话算,而绑在按工具调用触发的点上的脚本,会在一轮长活干到一半时哑掉。

## 载体不变量

无论是什么在加载扩展——今天是 WebAssembly,旁边还会有一个脚本引擎——四件事成立,
`daemon/tests/carrier_invariants.rs` 会在它们不再成立时失败:

1. **一张能力表,一个授权函数。** 两者都在 `base::interface::capabilities`。载体把
   自己的清单转成 `CapabilityDeclaration` 然后去问;它不作答。
2. **载体之间不互相调用。** 它们通过宿主契约互相够到,绝不跨内存模型直接调。
3. **每个载体都是一个编译期 feature,而且一个构建最多带一个。** `scripts` 带来
   QuickJS,是默认;`plugins` 带来 WebAssembly 那一档,要拿它得加
   `--no-default-features`——因为两个都要会被一个 `compile_error!` 拒掉,而不是被
   悄悄接受。`cargo build -p daemon --no-default-features` 一个都不链。

   互斥是因为:两个都带,就为一个没人要过两遍的能力付两倍的攻击面;还因为 cargo 的
   feature 合并让"两个都带"成为一个你踩进去而不是选进去的地方——`--features plugins`
   不加 `--no-default-features` 就足以踩进去。
4. **披露覆盖每一个载体。** 它管的是一个扩展*声明*了什么,所以它压根不点任何载体
   的名。

什么都不会因为没提就被授予:一个不声明任何能力的扩展,能计算,仅此而已。

**`extending_wasm.md`** 是 WebAssembly 载体的指南——一个组件要实现的 world、
`plugin.toml`、能力模型、安装期披露,以及一个插件能贡献什么。

---

## 什么不开放

有十一样东西留在内核里。把其中任何一样变成可替换的,都等于把它存在所要保证的那条
性质交出去。

| # | 只在内核 | 为什么不能开 |
|---|---|---|
| 1 | 能力授权与模块解析 | 决定 `import 'host:fs'` 解不解析得开的那个函数。可替换就等于没授权 |
| 2 | 权限规则的求值顺序 | 那八个阶段的顺序*就是*那条安全性质——一个工具自己的 Allow 必须排在 deny 规则和路径检查之后 |
| 3 | 资源限额与中断 | 停下一个失控扩展的唯一手段 |
| 4 | 调度与配额记账 | 可替换的记账就是可绕开的配额 |
| 5 | 一条沙箱策略要不要施加 | 后端可以换;施不施加这个决定不能换 |
| 6 | 只追加日志的语义,以及它们的不变量检查 | "模型可见即已记录"靠运行期断言撑着;断言可替换,原则就不存在 |
| 7 | 一轮骨架的步骤顺序 | 顺序承载着每一条不变量。每一步上的*决定*是开放的——见 `ARCHITECTURE.md` §2 |
| 8 | 安装期披露 | 一条只会告警的限制,任何自动安装器都会径直跨过去 |
| 9 | 权限规则的来源优先级 | 插件规则必须永远排在用户设置和组织策略之下 |
| 10 | scene 的组合与继承 | 组合面指数级膨胀,而且它和"每会话一个 scene"这条重放不变量相冲 |
| 11 | 插件贡献点的数量 | 插件子系统可以被整个编译掉;这依赖于贡献点的数量始终数得过来 |

前六条不只是安全要求——它们是这个内核区别于一个通用框架的地方。一个需要开放其中
之一的设计,是一个该拿出来争论的设计,不是一个该写的补丁。
