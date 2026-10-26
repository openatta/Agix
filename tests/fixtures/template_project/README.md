# template_project

Agix 测试系统用的模板项目 fixture。目录结构就是一个真实项目的项目级布局
（`docs/ARCHITECTURE.md` 的「磁盘布局」一节），AGENT/SKILLS/HOOK/RULES/MCP 全部配好，
用来验证"配置真的生效"，不是只测裸 Agent。

| 配置面 | 位置 | 验证点 |
|---|---|---|
| 指令入口 | `AGENTS.md` | 引用 `.agix/rules/testing.md` |
| Skill | `.agents/skills/code-review/SKILL.md` | 项目级 skill 能被发现 |
| 自定义 subagent | `.agix/agents/reviewer.md` | `AgentTool` 能列出/派生这个自定义类型 |
| Rule | `.agix/rules/testing.md` | 惰性发现（只读首行），被 `AGENTS.md` 显式引用 |
| Hook | `.agix/hooks/pre_bash_log.sh` + `.agix/settings.json` 的 `hooks_config.PreToolUse` | 每次 Bash 调用会在 `.agix/hooks_log/pre_bash_log.jsonl` 留一行记录 |
| MCP | `.agix/settings.json` 的 `mcp_servers.demo` | 真的能连上，`ping` 工具真的能调 |

## 关于 `mcp_servers.demo`

指向 `tests/fixtures/mcp_toy_server`——一个真实的、用 rmcp 写的最小 stdio MCP server
（一个 `ping` 工具，回显 `toy: <message>`）。**不是**占位符或预期失败的桩：
`tests/runner/tests/mcp_toy_server_smoke.rs` 端到端验证过真的能连上、`mcp__demo__ping`
真的能调、返回值真的对。

`command` 字段的值是字面量占位符 `{{MCP_TOY_SERVER_BIN}}`——因为这份 fixture 会被拷贝到
随机的临时目录运行，没法在提交进仓库的文件里写死一个绝对路径。`tests/runner/src/fixture.rs`
的 `resolve_mcp_toy_server_placeholder()` 会在拷贝之后、启动 Agent/daemon 之前，把它替换成
`cargo build -p mcp-toy-server` 编译出来的真实二进制绝对路径（没编译过会先编译一次）。
如果某个用例需要另一个 MCP 工具而不是 `ping`，扩展 `mcp_toy_server` 本身（加一个新
`#[tool]` 方法），不要为了单个用例另起一个 server 二进制。

之前（2026-08-05 早些时候）这里用的是 `in_process` transport 指向一个没注册过的服务名，
写着"预期连接失败,用来验证配置链路"——那是因为 `McpServerConfig::InProcess` 依赖的
`register_in_process_service()` 在当时的代码库里根本不存在（只有类型定义,没有实现）。
现在换成了真正能跑的 stdio server,不再需要"预期失败"这个说法。

## 用法

用例运行时把整个目录**拷贝**到临时目录再操作（不直接改这份 fixture 本身），保证可重复运行、
互不干扰。见 `tests/README.md`。
