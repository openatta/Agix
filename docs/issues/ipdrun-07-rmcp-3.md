# ipdrun-07：rmcp 还在 1.x，嵌入方的锁里 rmcp 与 base64 各多一份

类型：维护（依赖升级）｜优先级：中

## 需求

Agix 的 MCP 客户端钉在 rmcp 1.x：

- `Cargo.toml` 的 `[workspace.dependencies]`：`rmcp = { version = "1.6", features = [...] }`（客户端、子进程、
  streamable-http + reqwest 的那组特性，默认特性也开着），`process-wrap = "9"`（rmcp 1.x 用的那一版）；
- `tests/fixtures/mcp_toy_server/Cargo.toml`：`rmcp = "1.7"`（服务端、宏、stdio）。

rmcp 现在是 3.x。把 Agix 嵌进进程、自己也用 rmcp 3.x 的项目，锁里就有两份 rmcp，各自带一套依赖：
rmcp 3.x 用 base64 0.23、process-wrap 10，1.x 用 process-wrap 9；再加上 `tiktoken-rs 0.6` 带的 base64 0.21，
锁里 base64 有三份。

rmcp 3.x 换了内容模型，`crates/mcp/src/connect.rs` 里用到旧类型的地方要跟着改：

- `list_resources`：`Annotated<RawResource>` 没了，`list_all_resources` 直接给 `Resource`；
- `read_resource`：`ResourceContents` 标了 `#[non_exhaustive]`，要有兜底分支；
- `render_prompt_result`：`PromptMessageContent` 并进 `ContentBlock`（多了 `Audio`，也是 `#[non_exhaustive]`），
  `PromptMessageRole` 并进 `Role`；
- `rmcp_content_to_ours`：`Annotated<RawContent>` 换成 `ContentBlock`；
- toy server：`ServerInfo` 改名 `ServerConfig`（旧名留作弃用别名）。

## 建议

- 工作区依赖改成 `rmcp = { version = "3.5", default-features = false, features = [原来那组] }`：MCP 客户端
  用不到默认特性里的服务端与宏；`process-wrap` 升到 10，与 rmcp 3.x 用的同一版（stdio 服务器经它起成进程组组长）。
- toy server 改 `rmcp = "3.5"`，`ServerInfo` 换成 `ServerConfig`。
- `connect.rs` 按上面各处换类型；新出现的非穷尽变体：资源内容落到 `McpContent::Other`（与工具结果的未知内容一样，
  原样序列化），prompt 里的音频与未知内容给占位文字（与图片、内嵌资源同一种处理）。
- 顺带：`tiktoken-rs` 0.6 → 0.12（用到的 `cl100k_base()`、`CoreBPE::encode_with_special_tokens` 不变），
  去掉 base64 0.21，连带去掉 fancy-regex 0.13、rustc-hash 1、bit-set/bit-vec 旧版。

## 兼容性

- `mcp` crate 的公开接口不变（`McpClient`、`McpContent`、`McpServerConfig`、`NotifyingHandler` 等），宿主不用改。
- 行为不变：stdio / streamable-http / WebSocket 三种连法、自定义头与 `Authorization` 的处理、重连、通知转发照旧。
  rmcp 3.x 保留给传输自己的头与 1.x 相同。
- 小差别：工具结果里非文本、非图片的内容块序列化进 `McpContent::Other` 时，rmcp 3.x 把注解放在块内，
  有注解的块 JSON 里会多出 `annotations` 字段。
- reqwest：Agix 自己用 reqwest 0.12；rmcp 1.x 与 3.x 的 streamable-http 客户端都用 reqwest 0.13，升级前后都是
  这两份，不新增。Agix 整体换到 0.13 要改 TLS 特性名并波及各 crate，不在本条。
- rmcp 3.8 声明的最低 Rust 是 1.88，高于工作区写的 `rust-version = "1.80"`；但 wasmtime 47 已要 1.94，
  实际门槛不因本条变化。

## 验收

- `cargo test -p mcp` 全过（含 `render_prompt_result_*` 用新类型构造的用例）。
- `tests/runner` 的 `mcp_toy_server_smoke`（`--ignored`）：真起 toy server，`ping` 往返；`fixture_smoke` 过。
- `daemon/tests/daemon_e2e.rs` 的 MCP 用例（含 `--ignored` 的
  `mcp_add_server_connects_a_real_server_and_lists_its_tools`）过。
- `cargo test -p model -p compaction -p session`（token 估算换了 tiktoken-rs 版本）过。
- 嵌入方用这版 Agix 解析时，锁里只有一份 rmcp。

## 依赖

无。

## 来源

IPDrun 的锁里 rmcp 1.8 与 3.8 两份、base64 三份（ROADMAP T12 / R4）：它自己的 MCP 服务端用 rmcp 3.x，
嵌进来的 Agix 还在 1.x。

## 实现记录

- `Cargo.toml`：`rmcp` 3.5（锁定 3.8.0，`default-features = false`，特性不变）、`process-wrap` 10；
  `tiktoken-rs` 0.12。toy server 的 `rmcp` 3.5。
- `crates/mcp/src/connect.rs`：按「需求」所列各处换成 `Resource` / `ContentBlock` / `Role`；
  `read_resource` 的兜底落 `McpContent::Other`，`render_prompt_result` 加音频与未知内容的占位。
- toy server：`ServerInfo` → `ServerConfig`。
- 锁：rmcp 1.7 → 3.8，process-wrap 9 → 10，新增 base64 0.23（rmcp 3.x 用）与 syn 3（只经 toy server 的
  rmcp-macros 3.x）；去掉 base64 0.21、fancy-regex 0.13、rustc-hash 1、bit-set 0.5、bit-vec 0.6。
- 测试：`cargo test -p mcp` 174 过；`mcp_toy_server_smoke` 与 `fixture_smoke` 7 过；`daemon_e2e` 的 MCP 用例 6 过；
  `model` / `compaction` / `session` 全过；`cargo clippy -p mcp -p mcp-toy-server -p model --all-targets -D warnings`
  与 `cargo fmt --all --check` 干净。
- 提交：89edaeb（rmcp 3.x）、8bc3519（tiktoken-rs 0.12）。
