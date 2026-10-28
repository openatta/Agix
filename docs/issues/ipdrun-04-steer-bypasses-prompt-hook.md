# ipdrun-04：步骤边界送进去的消息（steer）不经 UserPromptSubmit

类型：缺陷｜优先级：中

## 现象

用户消息有两条路进会话：

- 空闲时开一轮：`process_turn` 先跑 UserPromptSubmit hook，可以拒收或改写（`crates/runtime/src/turn.rs:433-487`）；
- 一轮在跑时 `on_busy: "steer"`：消息进 `SteerQueue`（`crates/runtime/src/agent.rs:536-557`），在下一个步骤边界由
  `deliver_queued_user_messages` 直接写进转录、发 `UserMessageDelivered`（`turn.rs:344-372`）。

第二条路不经过任何 hook。配了 UserPromptSubmit 来挡某类内容的嵌入方，只要用户在模型干活时发消息，就绕过去了；
`turn.rs:433` 的注释说它是「the one point where a hook sees the user's message」，对 steer 的消息并不成立。

## 建议

- steer 的消息在送进转录之前过同一个 UserPromptSubmit（`HookInput` 加可选字段 `delivery: "turn" | "steer"`，
  旧 hook 不读它照样工作），以及 `ipdrun-03` 的原生中间件（若已有）。
- 改写：送进转录的是改写后的文本，`UserMessageDelivered.content` 同样是改写后的。
- 拒收：这条不进转录，这一轮照常继续；发一个新事件告诉发送方：
  ```rust
  AgentEvent::UserMessageRefused { message_id: String, content: String, message: Option<String>, turn_id: String }
  ```
  daemon 照 `user_message_delivered` 的样子转成 `{"kind":"user_message_refused", …}`（`daemon/src/session_pool.rs:3464` 旁边）。
  消息原文还给发送方，由它决定要不要提示用户。
  不复用 `UserMessagesReturned`：它的意思是「没来得及读，要不要再发由宿主定」，宿主照这个意思会把它们重新排队
  （Harnix 的中枢就把退回的消息排到队首），被拒的消息再发一遍只会再被拒一次。
- hook 除了拒收、改写之外的副作用（记日志、计数之类）对 steer 的消息也会发生；在意的 hook 读 `delivery` 区分。
- `deliver_queued_user_messages` 现在是同步函数；hook 是异步的，实现上要么把它改成 async，要么在 `offer` 之前（入队时）跑。
  入队时跑更早告诉发送方，但那时还不知道属于哪一步；交给维护者定。

## 兼容性

- 没配 UserPromptSubmit 的会话：行为不变（链为空时直接送达）。
- 配了的：steer 的消息从「不检查」变成「检查」。这就是要修的漏洞；hook 能从 `delivery` 区分两种来路，想对 steer 放行的可以直接放行。
- 新事件 `user_message_refused` 是新增的 `kind`；不认识它的宿主照现在忽略未知事件的做法即可。`AgentEvent` 加一个变体，
  对在 Rust 里穷举匹配它的嵌入方是一次编译期提醒（以前加 `UserMessagesReturned` 时也是这样）。

## 验收

`crates/runtime/src/agent.rs` 的测试模块（仿已有的 steer 用例，`agent.rs:6457` 一带）：

- hook 对含 `secret` 的消息 block：一轮在跑时 steer「secret …」→ 收到 `UserMessageRefused`，转录里没有这条，这一轮正常结束；
  steer 一条普通消息照常 `UserMessageDelivered`。
- hook 改写：转录与 `UserMessageDelivered.content` 都是改写后的。
- 没配 hook：与现在逐字节相同的事件序列（回归）。

## 依赖

无。`ipdrun-03` 的中间件复用这里的拒收通路。

## 来源

IPDrun 准备在界面上给人「立即送达」（steer）的选项，发现这样发出去的话会绕过它用 UserPromptSubmit 做的检查。

## 实现记录

- 在送达时跑（`deliver_queued_user_messages` 改成 async）：知道是哪一步、哪一轮，与开一轮的消息共用同一个
  `screen_user_prompt`。`HookInput.delivery`：开一轮的为 `"turn"`，steer 的为 `"steer"`。
- 拒收发 `AgentEvent::UserMessageRefused { message_id, content, message, turn_id }`，daemon 转成
  `{"kind":"user_message_refused","message_id","content","message"}`；协议文档 §7 与 steer 一节已补。
  最后一个步骤边界上取到的全被拒时再看一次队列，把它关上（或送达新到的）。
- 测试：`crates/runtime/src/agent.rs` 的 `a_steered_message_the_prompt_hook_blocks_is_refused_and_the_turn_goes_on`、
  `a_steered_message_reaches_the_transcript_as_the_prompt_hook_rewrote_it`、
  `without_a_prompt_hook_steered_messages_are_delivered_as_sent`。
- 提交：118e39b。
