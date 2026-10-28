# ipdrun-03：进程内嵌入没有「消息进一轮之前」的原生拦截

类型：新能力｜优先级：中

## 需求

嵌入方想在用户消息进入一轮之前看一眼原文，按自己的规则放行、改写或拒收（例如：某类会话不许带进某些内容；
某些消息要先登记再放行）。今天能做的只有两条路，都不合适：

- 设置层 `hooks_config.UserPromptSubmit` 的 command / http / prompt / wasm hook（触发点 `crates/runtime/src/turn.rs:433-487`）：
  每条消息起一次子进程或一次 HTTP；规则写在嵌入方自己的 Rust 代码里时，还得再造一个可执行文件让 hook 去调。
- `Builder::hooks(Arc<HookRunner>)`（`crates/runtime/src/agent.rs:1692`）：**整个替换** HookRunner，设置层配的 hook 就丢了。

工具调用早有原生的中间件（`Builder::tool_middleware`，`agent.rs:1437`；trait 在 `crates/core/src/interface/tool_middleware.rs:169`），
用户消息这一侧没有对应物。

## 建议

照 `ToolMiddleware` 的样子加一个 trait 和一个 Builder 方法：

```rust
// crates/core/src/interface/prompt_middleware.rs（新文件）
pub enum PromptDelivery { Turn, Steer }

pub struct PromptCall<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub content: &'a str,
    pub delivery: PromptDelivery,
    pub permission_mode: PermissionMode,
}

pub enum PromptDecision {
    Continue,
    Rewrite(String),
    Block { message: Option<String> },
}

#[async_trait]
pub trait PromptMiddleware: Send + Sync {
    async fn inspect(&self, call: &PromptCall<'_>) -> PromptDecision;
}

// runtime::agent::Builder
pub fn prompt_middleware(mut self, m: Arc<dyn PromptMiddleware>) -> Self;  // 可多次调用，按加入顺序
```

- **次序**：在设置层的 UserPromptSubmit hook 之后跑，按加入顺序；前一个的改写是后一个看到的原文。放在 hook 之后，
  嵌入方的规则才是最后一道，配置里的 hook 改写不回来（与 `tool_result_transformer` 放在最后是同一个理由）。
- **Block**：与 hook block 走同一条收尾——不调模型、`stop_reason = "stopped_by_hook"`、照样发 `turn_complete`
  （`turn.rs:471-481`；2ede9b3 之后 hook block 已经会正常结束一轮），`message` 进 `turn_complete.message`。
  不新造 stop_reason，宿主对 hook block 的处理原样适用。
- **Steer**：步骤边界送进去的消息（`turn.rs:344-372`）也过同一条链，`delivery = Steer`；被拒的那条不进转录，
  怎么告诉发送方见 `ipdrun-04`。
- 链为空时零开销（同 `dispatch_through` 的做法）。
- `extension_points.md` 的「拦截」一节加一行。

## 兼容性

- 纯新增：不调 `prompt_middleware` 的嵌入方行为不变；设置层 hook 的语义与次序不变。
- `Builder::hooks` 不动。
- 协议不变（daemon 不对外暴露这个点；它是嵌入方的 Rust 接缝）。

## 验收

`crates/runtime/src/agent.rs` 的测试模块，仿 `user_prompt_submit_hook_block_ends_the_turn_before_any_model_call`：

- 一个返回 `Block` 的中间件：模型 0 次调用，`turn_complete` 发出且 `stop_reason == "stopped_by_hook"`，会话随后能正常再跑一轮。
- `Rewrite`：模型收到的是改写后的文本，转录里记的也是。
- 同时配了设置层 hook（改写 A→B）和中间件：中间件看到的是 B。
- 两个中间件按加入顺序跑。
- 一轮在跑时 steer 一条消息：中间件收到 `delivery == Steer`。

## 依赖

被拒的 steer 消息怎么回告依赖 `ipdrun-04`；其余可以先做。Harnix `ipdrun-13` 经 `EngineContrib` 把它交给模块，依赖本条。

## 来源

IPDrun 要在人直接对某类会话说话时先过一遍自己的规则；进程内只能走 command hook，每条消息起一次 `ipdrun user-prompt`
子进程，cargo 测试里没有这个二进制时这一层就不在。

## 实现记录

- `base::interface::prompt_middleware`：`PromptDelivery { Turn, Steer }`（另有 `as_str`）、`PromptCall`、
  `PromptDecision { Continue, Rewrite(String), Block { message } }`、`PromptMiddleware::inspect`；与草图一致。
- `Builder::prompt_middleware(Arc<dyn PromptMiddleware>)`，可多次调用；在设置层 UserPromptSubmit hook 之后按加入
  顺序跑，与 ipdrun-04 共用 `screen_user_prompt`；steer 的拒收走 `UserMessageRefused`。链为空时只是一次空迭代。
- catalog 加了 `prompt.submit`（拦截、仅宿主），`extension_points.md` 的表随之重新生成并加了一节。
- 测试：`crates/runtime/src/agent.rs` 的 `a_prompt_middleware_block_ends_the_turn_before_any_model_call`、
  `a_prompt_middleware_rewrite_is_what_the_model_and_the_transcript_get`、
  `prompt_middlewares_run_after_the_configured_hook_in_the_order_they_were_added`、
  `a_steered_message_passes_the_prompt_middleware_too`。
