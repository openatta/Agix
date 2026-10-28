# ipdrun-05：prompt 中间件判断一条消息时看不到它带的附件

类型：新能力｜优先级：中

## 需求

`PromptMiddleware::inspect`（`crates/core/src/interface/prompt_middleware.rs`，ipdrun-03）拿到的 `PromptCall` 只有消息文本。
开一轮的 `InputMessage::User` 还带着 `attachments`（`crates/runtime/src/agent.rs:99` 的 `Attachment::{File, Text, Image}`），
它们在中间件之后才由 `resolve_attachments`（`crates/runtime/src/turn.rs`）变成内容块进转录。

做内容审查的嵌入方因此只看得到半条消息：文本放行了，附件里的东西照样进了模型。它想做的判断（「这类会话不收附件」
「文本附件里不许有某些内容」「图片超过多大不收」）都无从下手。

## 建议

只加不改。**不给 `PromptCall` 加字段**：它的字段全是 pub，嵌入方会用结构体字面量构造它（测试里尤其常见），加字段就编不过。

```rust
// crates/core/src/interface/prompt_middleware.rs
pub enum PromptAttachmentKind { File, Text, Image }

#[non_exhaustive]
pub struct PromptAttachment<'a> {
    pub kind: PromptAttachmentKind,
    pub name: Option<&'a str>,        // File、Text 的 path；Image 没有
    pub media_type: Option<&'a str>,  // Image；按扩展名认得出的图片 File
    pub size: Option<u64>,            // 字节数；File 取不到元数据时为 None
    pub text: Option<&'a str>,        // 只有 Text 有
}
impl<'a> PromptAttachment<'a> {
    pub fn file(path: &'a str, media_type: Option<&'a str>, size: Option<u64>) -> Self;
    pub fn text(name: &'a str, content: &'a str) -> Self;
    pub fn image(media_type: &'a str, size: u64) -> Self;
}

#[async_trait]
pub trait PromptMiddleware: Send + Sync {
    async fn inspect(&self, call: &PromptCall<'_>) -> PromptDecision;
    // 新增，有缺省实现：忽略附件，交给 inspect
    async fn inspect_with_attachments(
        &self,
        call: &PromptCall<'_>,
        attachments: &[PromptAttachment<'_>],
    ) -> PromptDecision {
        self.inspect(call).await
    }
}
```

- 运行时改为调 `inspect_with_attachments`；只实现了 `inspect` 的中间件经缺省实现走原路，行为不变。
- 只读视图：中间件看得到类型、名字、大小、`Text` 附件的内容，改不了附件。`Rewrite` 只改文本，附件原样进转录；
  `Block` 拒收整条消息（连同附件）。
- `File` 附件不替中间件读文件内容（运行时随后还要读一遍），中间件要看内容就按 `name` 自己读。
- `PromptAttachment` 标 `#[non_exhaustive]`，以后加字段不破坏谁；嵌入方在测试里用构造函数造它。
- 链为空时不构造视图、不取文件元数据。
- steer 进来的消息（`QueuedUserMessage`）今天不带附件，中间件收到空切片。

## 兼容性

- 纯新增：已有的 `PromptMiddleware` 实现不改照样编过、行为不变；`PromptCall` 不动。
- 设置层 UserPromptSubmit hook 不变（`HookInput` 不加附件）。
- 协议不变。

## 验收

`crates/runtime/src/agent.rs` 的测试模块：一个实现了 `inspect_with_attachments` 的中间件，与一个只实现 `inspect`
的旧式中间件同时装上：

- 开一轮带 `Text`、`Image`、`File` 三种附件：新中间件看到三项，类型、名字、媒体类型、大小、文本内容都对；
  它据此 `Block`，模型 0 次调用。旧式中间件照常被调用。
- 一轮在跑时 steer 一条消息：新中间件收到 `delivery == Steer` 和空的附件列表。

## 依赖

ipdrun-03（`PromptMiddleware`）、ipdrun-04（steer 过同一条链）。Harnix 重导出 `PromptAttachment`、`PromptAttachmentKind`
后模块才能不点名 Agix 的 crate 用上它。

## 来源

IPDrun 的某类会话不许带附件进来；它在 prompt 中间件里拦，但中间件看不到附件，只能在界面上提示。

## 实现记录

- `base::interface::prompt_middleware`：`PromptAttachmentKind { File, Text, Image }`（另有 `as_str`）、
  `PromptAttachment`（`#[non_exhaustive]`，构造函数 `file` / `text` / `image`）、
  `PromptMiddleware::inspect_with_attachments`（缺省交给 `inspect`）；与草图一致。
- 运行时（`turn.rs` 的 `screen_user_prompt`）改调 `inspect_with_attachments`；链为空时不构造视图。
  `Image` 的大小按 base64 解码后的长度算。steer 的消息给空切片。
- 测试：`crates/runtime/src/agent.rs` 的 `a_prompt_middleware_sees_the_attachments_of_a_turn_and_none_on_a_steer`
  （同时装一个只实现 `inspect` 的旧式中间件，证明它照常运行）。
- 提交：ce121c6。
