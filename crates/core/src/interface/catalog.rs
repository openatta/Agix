//! The extension points, as data.
//!
//! They accumulated one at a time, each documented where it lives, which is
//! the right place for the detail and the wrong place for the overview. There
//! was no answer to "what can I hook, when does it fire, how often, and am I
//! allowed to" that did not involve reading nine modules — and no way to check
//! that the answer written in a document still matched the code.
//!
//! So the overview is a value. [`all`] is the list; the reference table in the
//! extension-point index is [rendered from it](render_markdown) rather than
//! typed out, which is what keeps the two from drifting.
//!
//! # Frequency is a design constraint, not a footnote
//!
//! A point that fires once per session can afford a subprocess. One that fires
//! per streamed chunk cannot afford anything — at ten thousand calls a turn,
//! even a cheap in-process call is the difference between streaming and
//! stuttering. That is why [`Frequency::PerStreamChunk`] points are closed to
//! scripts by default: not because a script would do something dangerous
//! there, but because the cost is not one an author can see when they write
//! it.

/// What kind of seam this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Replace a whole subsystem by implementing a trait.
    Contract,
    /// Contribute something to a collection the engine assembles.
    Registration,
    /// Sit in the path of something the engine is doing, with the option to
    /// change it.
    Interception,
}

/// How often it fires, to an order of magnitude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    /// Once per process.
    PerProcess,
    /// Once per session.
    PerSession,
    /// Once or twice a turn.
    PerTurn,
    /// Once per tool call — tens per turn.
    PerToolCall,
    /// Once per model request — a few per turn.
    PerModelCall,
    /// Per streamed chunk — thousands per turn.
    PerStreamChunk,
}

impl Frequency {
    pub fn label(self) -> &'static str {
        match self {
            Self::PerProcess => "每进程",
            Self::PerSession => "每会话",
            Self::PerTurn => "每轮",
            Self::PerToolCall => "每次工具调用",
            Self::PerModelCall => "每次模型调用",
            Self::PerStreamChunk => "每个流式分片",
        }
    }

    /// Roughly how many times a busy turn hits it.
    pub fn magnitude(self) -> &'static str {
        match self {
            Self::PerProcess | Self::PerSession => "10⁰ 量级",
            Self::PerTurn | Self::PerModelCall => "10⁰–10¹ 量级",
            Self::PerToolCall => "10¹ 量级",
            Self::PerStreamChunk => "10³–10⁴ 量级",
        }
    }
}

/// What one kind of author may do at a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Everything the point offers.
    Full,
    /// May contribute, may not change or remove what is already there.
    AddOnly,
    /// The full point, but only with a capability declared at install and
    /// shown to whoever installed it.
    Declared,
    /// Not available.
    Denied,
}

impl Access {
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "完全",
            Self::AddOnly => "只能新增",
            Self::Declared => "需声明能力",
            Self::Denied => "不开放",
        }
    }
}

/// Who may do what, by where the extension came from.
///
/// The axis is *provenance*, not privilege level: the question a host is
/// really answering is "did the operator write this, or download it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trust {
    /// The engine's own registrations.
    pub kernel: Access,
    /// Written in settings by whoever runs this deployment.
    pub config: Access,
    /// A script in the operator's own project.
    pub script: Access,
    /// An installed plugin.
    pub plugin: Access,
}

impl Trust {
    /// Everything the operator wrote is unrestricted; a plugin may add.
    pub const fn operator_full_plugin_adds() -> Self {
        Self {
            kernel: Access::Full,
            config: Access::Full,
            script: Access::Full,
            plugin: Access::AddOnly,
        }
    }

    /// Everything the operator wrote is unrestricted; a plugin needs to have
    /// declared the capability.
    pub const fn operator_full_plugin_declares() -> Self {
        Self {
            kernel: Access::Full,
            config: Access::Full,
            script: Access::Full,
            plugin: Access::Declared,
        }
    }

    /// A seam only the embedding program can reach — it is a Rust trait
    /// wired at build time, so there is nothing for a script or a plugin to
    /// register through.
    pub const fn host_only() -> Self {
        Self {
            kernel: Access::Full,
            config: Access::Denied,
            script: Access::Denied,
            plugin: Access::Denied,
        }
    }
}

/// One place something can be plugged in.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionPoint {
    /// Stable id. What documentation, configuration and error messages call
    /// this point.
    pub id: &'static str,
    pub kind: Kind,
    /// One line: what it is for.
    pub summary: &'static str,
    /// When in a turn it happens.
    pub timing: &'static str,
    /// What an implementation may change. "nothing" for a pure observer.
    pub rewrites: &'static str,
    pub frequency: Frequency,
    pub trust: Trust,
    /// Where the contract is defined, as a Rust path.
    pub defined_in: &'static str,
}

/// Every extension point the engine has.
pub fn all() -> &'static [ExtensionPoint] {
    &POINTS
}

static POINTS: [ExtensionPoint; 45] = [
    ExtensionPoint {
        id: "tool.registry",
        kind: Kind::Contract,
        summary: "整套工具,或者其中一个工具",
        timing: "会话构建时,以及之后任何时候",
        rewrites: "有哪些工具、它们各是什么",
        frequency: Frequency::PerSession,
        trust: Trust::host_only(),
        defined_in: "base::interface::tool::ToolRegistry",
    },
    ExtensionPoint {
        id: "tool.around",
        kind: Kind::Interception,
        summary: "套在每次工具调用外面的一圈:超时、重试、缓存、埋点",
        timing: "分发前后,在权限和 hooks 的外侧",
        rewrites: "取消信号和结果;改不了输入",
        frequency: Frequency::PerToolCall,
        trust: Trust::operator_full_plugin_declares(),
        defined_in: "base::interface::tool_middleware::ToolMiddleware",
    },
    ExtensionPoint {
        id: "tool.result",
        kind: Kind::Interception,
        summary: "工具结果长什么样:截断、脱敏、大块内容",
        timing: "所有 hook 之后,模型看到它之前的最后一步",
        rewrites: "结果文本和其中的图片",
        frequency: Frequency::PerToolCall,
        trust: Trust::operator_full_plugin_declares(),
        defined_in: "base::interface::tool_result::ToolResultTransformer",
    },
    ExtensionPoint {
        id: "prompt.block",
        kind: Kind::Registration,
        summary: "系统提示里一个具名、有序、可撤销的块",
        timing: "提示组装时,每轮一次",
        rewrites: "只能新增",
        frequency: Frequency::PerTurn,
        trust: Trust::operator_full_plugin_adds(),
        defined_in: "base::interface::prompt_registry::PromptRegistry::register_block",
    },
    ExtensionPoint {
        id: "prompt.context",
        kind: Kind::Registration,
        summary: "文本在提示组装时才算出来的块",
        timing: "提示组装时,每轮一次",
        rewrites: "只能新增",
        frequency: Frequency::PerTurn,
        trust: Trust::operator_full_plugin_adds(),
        defined_in: "base::interface::prompt_registry::PromptRegistry::register_context",
    },
    ExtensionPoint {
        id: "prompt.variable",
        kind: Kind::Registration,
        summary: "`{{name}}` 在每个块里展开成什么",
        timing: "提示组装时,块合并之后",
        rewrites: "只有自己那个占位符,别的都不行",
        frequency: Frequency::PerTurn,
        trust: Trust::operator_full_plugin_adds(),
        defined_in: "base::interface::prompt_registry::PromptRegistry::register_variable",
    },
    ExtensionPoint {
        id: "prompt.assembler",
        kind: Kind::Contract,
        summary: "所有贡献怎么变成请求携带的那些块",
        timing: "提示组装时,取代引擎自己那套",
        rewrites: "顺序、缓存边界、合并策略——整个结果",
        frequency: Frequency::PerModelCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::prompt_assembler::PromptAssembler",
    },
    ExtensionPoint {
        id: "prompt.assemble",
        kind: Kind::Interception,
        summary: "在组装好的整份提示上再过一遍",
        timing: "提示组装的最后一步",
        rewrites: "块的内容、顺序和取舍",
        frequency: Frequency::PerTurn,
        trust: Trust::operator_full_plugin_declares(),
        defined_in: "base::interface::prompt_assembly::AssemblyHook",
    },
    ExtensionPoint {
        id: "prompt.submit",
        kind: Kind::Interception,
        summary: "用户消息进对话之前的最后一道:放行、改写或拒收",
        timing: "每条用户消息进转录之前,开一轮的与 steer 进来的都算;在 UserPromptSubmit hook 之后",
        rewrites: "消息文本;或者拒收这条",
        frequency: Frequency::PerTurn,
        trust: Trust::host_only(),
        defined_in: "base::interface::prompt_middleware::PromptMiddleware",
    },
    ExtensionPoint {
        id: "event.sink",
        kind: Kind::Contract,
        summary: "除了引擎返回的那个 channel,事件还往哪里去",
        timing: "每次发射,在 sink 自己的 task 上",
        rewrites: "什么都改不了——只能观察",
        frequency: Frequency::PerStreamChunk,
        trust: Trust::host_only(),
        defined_in: "base::interface::event_sink::EventSink",
    },
    ExtensionPoint {
        id: "health.check",
        kind: Kind::Registration,
        summary: "某个子系统是否正常,和引擎自己的答案并列",
        timing: "每当有人要一份健康报告",
        rewrites: "什么都改不了——检查只汇报,不修复",
        frequency: Frequency::PerProcess,
        trust: Trust::host_only(),
        defined_in: "base::interface::health::HealthCheck",
    },
    ExtensionPoint {
        id: "elicitation.ask",
        kind: Kind::Contract,
        summary: "引擎怎么问人:授权、澄清、导入",
        timing: "每当一个决定需要人来做",
        rewrites: "答案",
        frequency: Frequency::PerTurn,
        trust: Trust::host_only(),
        defined_in: "base::interface::elicitation::Elicitation",
    },
    ExtensionPoint {
        id: "permission.check",
        kind: Kind::Contract,
        summary: "一次工具调用允不允许",
        timing: "每次工具调用之前",
        rewrites: "放行、拒绝,或者问人",
        frequency: Frequency::PerToolCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::permission::Permission",
    },
    ExtensionPoint {
        id: "scene",
        kind: Kind::Contract,
        summary: "系统提示骨架、工具面和预算",
        timing: "会话构建时",
        rewrites: "一个 agent 怎么呈现自己,全部",
        frequency: Frequency::PerSession,
        trust: Trust::host_only(),
        defined_in: "base::interface::scene::AgentScene",
    },
    ExtensionPoint {
        id: "model",
        kind: Kind::Contract,
        summary: "LLM 后端与它的传输协议",
        timing: "每次模型请求",
        rewrites: "整场交互",
        frequency: Frequency::PerModelCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::model::Model",
    },
    ExtensionPoint {
        id: "model.factory",
        kind: Kind::Registration,
        summary: "settings 里的 `api_type` 怎么变成一个跑得起来的模型",
        timing: "启动时,读 provider 配置的那一刻",
        rewrites: "哪些协议可以被配置出来",
        frequency: Frequency::PerProcess,
        trust: Trust::host_only(),
        defined_in: "base::interface::model_factory::ModelFactory",
    },
    ExtensionPoint {
        id: "model.request",
        kind: Kind::Interception,
        summary: "组装好、还没发出去的请求:消息、工具、参数",
        timing: "每次模型调用之前的最后一刻",
        rewrites: "请求里的一切",
        frequency: Frequency::PerModelCall,
        trust: Trust::operator_full_plugin_declares(),
        defined_in: "base::interface::model_interceptor::ModelInterceptor::on_request",
    },
    ExtensionPoint {
        id: "model.message",
        kind: Kind::Interception,
        summary: "模型产出的一条完整消息,在它被记下来之前",
        timing: "承载它的那段流结束之后",
        rewrites: "消息内容",
        frequency: Frequency::PerModelCall,
        trust: Trust::operator_full_plugin_declares(),
        defined_in: "base::interface::model_interceptor::ModelInterceptor::on_message",
    },
    ExtensionPoint {
        id: "credentials",
        kind: Kind::Contract,
        summary: "provider 的 API key 从哪来",
        timing: "启动时,读 provider 配置的那一刻",
        rewrites: "凭据本身",
        frequency: Frequency::PerProcess,
        trust: Trust::host_only(),
        defined_in: "base::interface::credentials::CredentialSource",
    },
    ExtensionPoint {
        id: "config.source",
        kind: Kind::Contract,
        summary: "配置层在合并之前从哪来",
        timing: "进程启动,任何东西被构建之前",
        rewrites: "有哪些层、每层里是什么 JSON;合并本身动不了",
        frequency: Frequency::PerProcess,
        trust: Trust::host_only(),
        defined_in: "base::interface::config_source::ConfigSource",
    },
    ExtensionPoint {
        id: "token.count",
        kind: Kind::Contract,
        summary: "上下文被判定为多大",
        timing: "每次预算检查",
        rewrites: "压缩据以触发的那个数字",
        frequency: Frequency::PerTurn,
        trust: Trust::host_only(),
        defined_in: "base::interface::token_counter::TokenCounter",
    },
    ExtensionPoint {
        id: "history.store",
        kind: Kind::Contract,
        summary: "会话日志在两次运行之间存在哪",
        timing: "每次追加,以及恢复时",
        rewrites: "日志怎么持久化、存到哪",
        frequency: Frequency::PerTurn,
        trust: Trust::host_only(),
        defined_in: "history::store::HistoryStore",
    },
    ExtensionPoint {
        id: "history.query",
        kind: Kind::Contract,
        summary: "怎么找会话:按时间列表和按文本搜索",
        timing: "每当有人要的是若干会话而不是某一个会话",
        rewrites: "返回哪些会话、按什么顺序、怎么算匹配",
        frequency: Frequency::PerSession,
        trust: Trust::host_only(),
        defined_in: "history::store::HistoryStore::find_sessions",
    },
    ExtensionPoint {
        id: "history.blob",
        kind: Kind::Contract,
        summary: "大到不适合留在日志里的内容存在哪",
        timing: "每次追加带图片或大负载时,以及加载时",
        rewrites: "大块内容存在哪、怎么寻址",
        frequency: Frequency::PerTurn,
        trust: Trust::host_only(),
        defined_in: "history::blob::BlobStore",
    },
    ExtensionPoint {
        id: "history.projection",
        kind: Kind::Contract,
        summary: "一份日志对模型意味着什么,包括扩展自己写的条目",
        timing: "每次读取转录:恢复、分叉、搜索、翻页",
        rewrites: "哪些条目变成消息、它们说了什么",
        frequency: Frequency::PerSession,
        trust: Trust::host_only(),
        defined_in: "history::transcript::TranscriptProjection",
    },
    ExtensionPoint {
        id: "history.extension_entry",
        kind: Kind::Registration,
        summary: "在会话日志里写自己的状态,挂在自己的命名空间下",
        timing: "任何时候;和其余条目同序",
        rewrites: "只能新增;内核从不解析 payload",
        frequency: Frequency::PerTurn,
        trust: Trust::operator_full_plugin_adds(),
        defined_in: "history::entry::LogEntry::Extension",
    },
    ExtensionPoint {
        id: "memory.storage",
        kind: Kind::Contract,
        summary: "长期记忆存在哪",
        timing: "召回时,以及每次写入记忆时",
        rewrites: "记忆怎么持久化、存到哪",
        frequency: Frequency::PerTurn,
        trust: Trust::host_only(),
        defined_in: "base::interface::memory_contracts::MemoryStorage",
    },
    ExtensionPoint {
        id: "memory.retriever",
        kind: Kind::Contract,
        summary: "一轮能召回哪些记忆",
        timing: "每条用户消息一次,在后台",
        rewrites: "召回的集合",
        frequency: Frequency::PerTurn,
        trust: Trust::host_only(),
        defined_in: "base::interface::memory_contracts::MemoryRetriever",
    },
    ExtensionPoint {
        id: "memory.retrieval_hook",
        kind: Kind::Interception,
        summary: "召回的问题在提出之前,召回的答案在被用之前",
        timing: "召回前后",
        rewrites: "查询,以及召回的记忆名",
        frequency: Frequency::PerTurn,
        trust: Trust::operator_full_plugin_declares(),
        defined_in: "base::interface::memory_contracts::RetrievalHook",
    },
    ExtensionPoint {
        id: "history.append_observer",
        kind: Kind::Interception,
        summary: "看着条目进入会话日志",
        timing: "每次追加成功之后",
        rewrites: "什么都改不了——只读是类型定死的,不是约定的",
        frequency: Frequency::PerTurn,
        trust: Trust {
            kernel: Access::Full,
            config: Access::Full,
            script: Access::Full,
            plugin: Access::AddOnly,
        },
        defined_in: "history::store::AppendObserver",
    },
    ExtensionPoint {
        id: "skill.source",
        kind: Kind::Contract,
        summary: "一次会话的 skill 从哪来",
        timing: "会话构建时,以及每当有 MCP server 接入",
        rewrites: "有哪些 skill、每个展开成什么文本",
        frequency: Frequency::PerSession,
        trust: Trust::host_only(),
        defined_in: "base::interface::skill_provider::SkillProvider",
    },
    ExtensionPoint {
        id: "instruction.source",
        kind: Kind::Contract,
        summary: "常驻指令从哪来",
        timing: "会话构建时",
        rewrites: "每会话注入一次的 AGENTS.md 文本",
        frequency: Frequency::PerSession,
        trust: Trust::host_only(),
        defined_in: "base::interface::instruction_provider::InstructionProvider",
    },
    ExtensionPoint {
        id: "rules.source",
        kind: Kind::Contract,
        summary: "规则索引从哪来",
        timing: "系统提示组装期间",
        rewrites: "告诉模型存在哪些规则文档",
        frequency: Frequency::PerModelCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::instruction_provider::RuleProvider",
    },
    ExtensionPoint {
        id: "turn.policy",
        kind: Kind::Contract,
        summary: "一轮什么时候算走得够久了",
        timing: "每次模型调用之前,以及每次返回之后",
        rewrites: "循环还走不走下一步,以及上报的停止原因",
        frequency: Frequency::PerModelCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::turn_policy::TurnPolicy",
    },
    ExtensionPoint {
        id: "model.recovery",
        kind: Kind::Contract,
        summary: "模型调用失败或者半途截断时怎么办",
        timing: "出错时,以及响应撞上输出上限被截断时",
        rewrites: "换模型、压缩后重试、抬高上限,还是直接失败",
        frequency: Frequency::PerModelCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::recovery_policy::RecoveryPolicy",
    },
    ExtensionPoint {
        id: "model.backoff",
        kind: Kind::Contract,
        summary: "失败的请求还发不发,发之前先等多久",
        timing: "在 client 内部、模型契约之下,每次失败的尝试",
        rewrites: "重试前等多久,以及到底有没有重试",
        frequency: Frequency::PerModelCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::backoff::BackoffPolicy",
    },
    ExtensionPoint {
        id: "budget",
        kind: Kind::Contract,
        summary: "一轮能花多少,一个请求能长到多大",
        timing: "每次模型调用之后,以及每个请求组装之前",
        rewrites: "这轮还继不继续、被告知了什么、压缩的上限",
        frequency: Frequency::PerModelCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::budget_policy::BudgetPolicy",
    },
    ExtensionPoint {
        id: "environment",
        kind: Kind::Contract,
        summary: "被记下来的时间和标识符——让重放对不上的那些输入",
        timing: "每当一个答案是被写下来而不是被测量出来的",
        rewrites: "日志时间戳、条目 id、提示里带的日期",
        frequency: Frequency::PerTurn,
        trust: Trust::host_only(),
        defined_in: "base::interface::environment::Environment",
    },
    ExtensionPoint {
        id: "exec.process",
        kind: Kind::Contract,
        summary: "程序在哪跑",
        timing: "工具启动的每一条命令",
        rewrites: "活儿在哪台机器上干",
        frequency: Frequency::PerToolCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::exec::process::Process",
    },
    ExtensionPoint {
        id: "exec.filesystem",
        kind: Kind::Contract,
        summary: "工具的文件在哪",
        timing: "工具的每一次读、写、stat",
        rewrites: "工具看到的是哪个文件系统",
        frequency: Frequency::PerToolCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::exec::filesystem::FileSystem",
    },
    ExtensionPoint {
        id: "exec.network",
        kind: Kind::Contract,
        summary: "工具发出的请求,以及它们能不能出去",
        timing: "每个出站请求;出网策略约束的是模型选的那些",
        rewrites: "请求去哪、去不去、谁来应答",
        frequency: Frequency::PerToolCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::exec::network::Network",
    },
    ExtensionPoint {
        id: "exec.sandbox",
        kind: Kind::Contract,
        summary: "一个进程怎么被约束——而不是要不要约束",
        timing: "命令启动之前",
        rewrites: "真正跑起来的那条命令,以及它能碰什么",
        frequency: Frequency::PerToolCall,
        trust: Trust::host_only(),
        defined_in: "base::interface::exec::sandbox::Sandbox",
    },
    ExtensionPoint {
        id: "compaction",
        kind: Kind::Contract,
        summary: "对话什么时候被缩短、怎么缩",
        timing: "每轮一次:两趟老化、一趟预测,最后是阈值",
        rewrites: "消息历史,以及到底要不要重写",
        frequency: Frequency::PerTurn,
        trust: Trust::host_only(),
        defined_in: "compaction::compact::Compactor",
    },
    ExtensionPoint {
        id: "script.carrier",
        kind: Kind::Contract,
        summary: "在九个挂载点上跑运维者自己的代码,就在本进程里(QuickJS)",
        timing: "载体被绑到哪就在哪;受每轮配额约束",
        rewrites: "所绑那个点允许的一切,按脚本自己的来源定级",
        frequency: Frequency::PerTurn,
        trust: Trust::operator_full_plugin_declares(),
        defined_in: "base::interface::script::ScriptEngine",
    },
    ExtensionPoint {
        id: "hooks",
        kind: Kind::Interception,
        summary: "生命周期回调——command、prompt、HTTP、agent 或 wasm",
        timing: "三十个具名时刻;见 hook 事件清单",
        rewrites: "按事件而异:拦截、改写输入、结束这一轮",
        frequency: Frequency::PerToolCall,
        trust: Trust {
            kernel: Access::Full,
            config: Access::Full,
            script: Access::Full,
            plugin: Access::Declared,
        },
        defined_in: "hooks::runner::HookRunner",
    },
];

/// Look one up by id.
pub fn find(id: &str) -> Option<&'static ExtensionPoint> {
    all().iter().find(|p| p.id == id)
}

/// The reference table, generated.
///
/// The extension-point index embeds this rather than restating it, so a point
/// added to [`all`] appears in the documentation without anybody remembering
/// to write it down, and a point whose trust rules change cannot leave a
/// stale row behind.
pub fn render_markdown() -> String {
    let mut out = String::new();
    out.push_str("| 扩展点 | 类型 | 时机 | 可改什么 | 频率 | 配置 | 脚本 | 插件 |\n");
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for p in all() {
        let kind = match p.kind {
            Kind::Contract => "契约",
            Kind::Registration => "注册",
            Kind::Interception => "拦截",
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {}({}) | {} | {} | {} |\n",
            p.id,
            kind,
            p.timing,
            p.rewrites,
            p.frequency.label(),
            p.frequency.magnitude(),
            p.trust.config.label(),
            p.trust.script.label(),
            p.trust.plugin.label(),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_point_says_all_the_things_a_reader_needs() {
        for p in all() {
            for (field, value) in [
                ("id", p.id),
                ("summary", p.summary),
                ("timing", p.timing),
                ("rewrites", p.rewrites),
                ("defined_in", p.defined_in),
            ] {
                assert!(
                    !value.trim().is_empty(),
                    "extension point '{}' has an empty {field}",
                    p.id
                );
            }
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut seen = HashSet::new();
        for p in all() {
            assert!(seen.insert(p.id), "duplicate extension point id '{}'", p.id);
        }
    }

    /// The rule the frequency column exists to enforce: a point that fires
    /// thousands of times a turn is not open to a script, because the cost is
    /// invisible to whoever writes one.
    #[test]
    fn the_highest_frequency_points_are_closed_to_scripts() {
        for p in all() {
            if p.frequency == Frequency::PerStreamChunk {
                assert_eq!(
                    p.trust.script,
                    Access::Denied,
                    "'{}' fires {} and must not be open to scripts",
                    p.id,
                    p.frequency.magnitude()
                );
                assert_eq!(p.trust.plugin, Access::Denied, "same for '{}'", p.id);
            }
        }
    }

    /// A plugin may only ever *add* without declaring something. Anything a
    /// point lets it rewrite has to be behind a declaration, or the
    /// install-time disclosure is not telling the truth.
    #[test]
    fn a_plugin_never_gets_more_than_add_only_by_default() {
        for p in all() {
            assert!(
                matches!(
                    p.trust.plugin,
                    Access::AddOnly | Access::Declared | Access::Denied
                ),
                "'{}' would give an installed plugin unrestricted access",
                p.id
            );
        }
    }

    #[test]
    fn the_table_has_a_row_for_every_point() {
        let table = render_markdown();
        for p in all() {
            assert!(
                table.contains(&format!("`{}`", p.id)),
                "'{}' is missing from the generated table",
                p.id
            );
        }
        assert_eq!(
            table.lines().count(),
            all().len() + 2,
            "header, separator, and one row each"
        );
    }

    #[test]
    fn find_is_the_id_lookup_it_claims_to_be() {
        assert_eq!(
            find("tool.around").map(|p| p.kind),
            Some(Kind::Interception)
        );
        assert!(find("nothing.like.this").is_none());
    }
}
