# 遥测

遥测是引擎对自己做了什么的结构化记账:每件事一行 JSON——一次模型调用花了多少 token、
一次工具调用跑了多久、权限怎么判的、一轮怎么结束的。它为事后排查和算账而存在,所以
**不带内容**:没有提示词、没有消息文本、没有工具的入参与输出,只有名字、大小、计数和
耗时。要逐字重放一次运行,用的是录像(`recorder`),不是遥测。

---

## 记在哪

| 文件 | 内容 |
|---|---|
| `<local_data_dir>/telemetry/<session-id>.jsonl` | 一个会话的事件,一个会话一个文件 |
| `<local_data_dir>/telemetry/pool.jsonl` | 不属于任何会话的事件:daemon 启动、MCP server 连上或没连上、落空的恢复请求 |

- `local_data_dir` 缺省是项目的 `.agix/`(`AGIX_LOCAL_DATA_DIR` 可改);会话文件用会话自己那个
  项目的,`pool.jsonl` 用 daemon 自己那一层的。它不是绝对路径时(测试、没配 `paths` 的
  嵌入方)什么都不记,而不是写进进程的当前目录。
- `session.create` 的 `options.telemetry.output` 把这个会话的文件放到指定路径。
- **开关**:`settings.json` 的 `telemetry_enabled`,缺省 `true`。关掉后会话与池都不记。
- **轮转**:一个文件写到 64 MiB(`telemetry::file_recorder::DEFAULT_MAX_BYTES`)就改名为
  `<文件>.1`(覆盖上一个 `.1`),再开一个新的;一个文件最多占两份的空间。
- **文件权限**:只有属主可读写(0600),已有的文件打开时也收紧;为它们新建的那一层目录是
  0700,再往上缺的目录照常建。录像同样如此。
- **不阻塞**:事件经一个 500 条的队列交给后台写;队列满了就丢,写失败每分钟至多记一条
  warn。遥测从不让一轮等它。

### OpenTelemetry

daemon 用 `--features otel` 编译、配了 `telemetry_url`(或环境变量
`OTEL_EXPORTER_OTLP_ENDPOINT`)时,同样的事件流再送一份到 OTLP 端点。只有三种有对应的
指标:`turn_complete`(一轮的耗时)、`tool_execution`(按工具、按成败计数)、
`api_request`(延迟与四项 token,按模型)。其余只在 JSONL 里。没编 `otel` 却配了端点的,
启动时记一条 warn。

端口是 4317 的走 gRPC,其余走 HTTP/protobuf。HTTP 时端点写 collector 的根地址(如
`http://collector:4318`),trace 发到 `<端点>/v1/traces`、指标发到 `<端点>/v1/metrics`,与
`OTEL_EXPORTER_OTLP_ENDPOINT` 的约定相同;`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` /
`OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` 设了的,按它们原样发。HTTP 导出遵从
`HTTP(S)_PROXY` / `NO_PROXY`,端点在本机(loopback、`localhost`)时直连。

导出器是进程级的:一个 daemon 只起一个,会话结束只 flush;第二个 pool 配了不同端点时以先
起的为准。嵌 daemon 的进程退出前调 `assembly::telemetry_otel::shutdown_process_exporter()`,
否则丢最后一批数据。

### 嵌入方

`telemetry` crate 自己不碰文件也不碰网络:`telemetry::spawn(config, recorders)` 返回一个
handle 和一个后台 future,事件过滤、脱敏之后交给你给的每个 `TelemetryRecorder`。
`TelemetryConfig::event_toggles` / `default_event_enabled` 按 `type` 开关单个事件——daemon
不暴露这一层,它的两条管线都是全开。

---

## 一行长什么样

```json
{"event_id":"6f1c…","session_id":"3kq…","turn_no":2,"turn_id":"t_…","timestamp_ms":1762650000000,
 "schema_version":2,"type":"api_request","model":"claude-sonnet-4-6","input_tokens":1840,
 "output_tokens":212,"cache_creation_tokens":0,"cache_read_tokens":10240,"latency_ms":3105,
 "stop_reason":"tool_use","input_message_count":7,"tool_count":18,"default_model":true}
```

信封字段每行都有:

| 字段 | 内容 |
|---|---|
| `event_id` | UUID v4 |
| `session_id` | 所属会话;`pool.jsonl` 里的事件为空串 |
| `turn_no` | 会话里的第几轮,从 0 计;会话级事件为 0 |
| `turn_id` | 所属那一轮的 id;不属于某一轮的不写 |
| `timestamp_ms` | 本机 Unix 毫秒 |
| `schema_version` | 记录形状的版本,现在是 **2** |
| `type` | 事件种类,即下表第一列 |

信封之外是这一种事件自己的字段,和信封平铺在同一层。

**`schema_version` 2**:事件体不再重复信封的 `turn_no` / `turn_id`;从来没有发出过的种类
删掉了。读日志的程序按 `schema_version` 分支,别按字段有无猜。

**脱敏**:写出前,`api_error`、`tool_execution`、`mcp_connection_error` 的 `error_message`
一律换成 `[REDACTED]`(空串保持为空)——厂商和工具的错误文本里常带路径、主机名、密钥。

---

## 事件清单

下表由 `telemetry::events::inventory` 生成:第二、三列是 `event_kinds!` 里每种事件旁边的
声明,第四列从 payload 类型的 JSON schema 读出(`?` 是可能缺省或为 `null` 的字段,括号里是
枚举字段的取值)。两件事有测试保证:表和代码一致(`crates/telemetry/tests/producers.rs`,
`AGIX_UPDATE_DOCS=1` 重写这张表),以及**表里每一种都有生产者**(`tests/guards/tests/wiring.rs`)
——引擎里确有发出它的地方。所以这里列出的每一种都会出现在日志里,只要它描述的事发生了。

<!-- BEGIN GENERATED TABLE — see telemetry::events::inventory::render_markdown -->
| `type` | 何时发 | 记在哪 | 字段(信封之外) |
|---|---|---|---|
| `session_start` | daemon 建好一个会话之后、它的第一轮之前;新建与按 id 重建都算。 | 会话 | `auth_modes`、`max_tokens`、`mcp_server_count`、`model`、`output_format`、`permission_mode`、`plugin_count`、`resume_from?`、`sandbox_enabled`、`skill_count`、`started_at_ms`、`thinking_mode` |
| `session_end` | 会话的主循环退出(关闭、输入通道关闭、被取消);合计是这次载入以来的。 | 会话 | `duration_ms`、`stop_reason`、`total_api_calls`、`total_cache_creation`、`total_cache_read`、`total_input_tokens`、`total_output_tokens`、`total_permission_denials`、`total_tool_calls`、`total_turns` |
| `startup_timing` | daemon 启动完成、开始接请求时,一次。 | 池 | `config_load_ms`、`pool_build_ms`、`total_startup_ms` |
| `api_request` | 每次模型调用完成。一轮里的调用不带 `purpose`;辅助调用带:`compact`、`memory`、`title`、`hook`、`team_judge`、`web_search`、`web_fetch`。 | 会话 | `cache_creation_tokens`、`cache_read_tokens`、`default_model`、`input_message_count`、`input_tokens`、`latency_ms`、`model`、`output_tokens`、`purpose?`、`stop_reason`、`tool_count`、`truncated?` |
| `api_error` | 一轮里的一次模型调用失败:请求被拒,或流在回复完整之前断了。每次失败的尝试一条,不论这一轮后来恢复没有;取消不算。 | 会话 | `error_kind`、`error_message`、`http_status?`、`latency_ms`、`model` |
| `tool_start` | 工具开始执行:PreToolUse hook、输入校验、权限都放行之后。 | 会话 | `deferred`、`destructive`、`input_json_size`、`tool_name`、`tool_use_id` |
| `tool_execution` | 工具执行结束,成功或失败;工具自己报告的失败(如 Bash 非零退出)算失败。 | 会话 | `error_message?`、`input_json_size`、`is_error`、`latency_ms`、`outcome`(`approved` / `denied` / `succeeded` / `failed` / `timeout` / `cancelled`)、`result_content_size`、`tool_name`、`tool_use_id`、`user_approved` |
| `tool_cancelled` | 一次工具调用在等权限回答时会话被关闭。 | 会话 | `elapsed_ms`、`reason`、`tool_name` |
| `permission_decision` | 会话的权限检查对一次工具调用作出判定,在询问任何人之前。 | 会话 | `destructive`、`latency_ms`、`outcome`(`auto_allow` / `ask` / `deny`)、`permission_mode`、`tool_name` |
| `user_prompt_submit` | 一轮开始,紧接 `turn_start`:开这一轮的用户消息,只记大小。 | 会话 | `attachment_count`、`char_count` |
| `slash_command_used` | 开一轮的消息是斜杠命令。 | 会话 | `command`、`has_arg`、`kind` |
| `turn_start` | 一轮开始,UserPromptSubmit hook 放行之后。 | 会话 | `is_retry`、`resumed` |
| `turn_complete` | 一轮结束,`stop_reason` 说怎么结束的。 | 会话 | `api_calls`、`last_tool_name?`、`last_tool_was_error`、`permission_denials`、`stop_reason`、`tool_calls`、`turn_duration_ms` |
| `budget_enforced` | 会话 token 预算到 90% 注入收尾提醒,或到 100% 中止这一轮。 | 会话 | `action`(`warning_injected` / `turn_stopped`)、`budget`、`total_tokens_used` |
| `compact_action` | 一次压缩完成或失败。 | 会话 | `after_message_count`、`after_tokens`、`before_message_count`、`before_tokens`、`latency_ms`、`strategy`、`success` |
| `resume_action` | 会话从日志重建(会话文件);恢复请求落到已在内存里、找不到、已结束的侧链会话上(池文件);团队从 scratchpad 接上(会话文件)。 | 会话 / 池 | `compact_boundary_count`、`entry_count`、`latency_ms`、`outcome`(`succeeded` / `partial` / `degraded` / `failed`)、`projected_message_count`、`sidechain_entry_count`、`source`、`warning_kind?` |
| `mcp_server_connected` | 一个 MCP server 连上:启动时、重载时、新增时。 | 池 | `server_name`、`tool_count`、`transport` |
| `mcp_connection_error` | 一个要连的 MCP server 没连上:起不来、握手失败或超时、配置被拒。 | 池 | `error_message`、`server_name` |
| `agent_spawned` | 子 Agent 开始:`Agent` 工具起的(前台、后台)、常驻团队成员、`TeamCreate` 流水线的成员。 | 会话 | `agent_id`、`background`、`model?`、`role`、`stage_name?` |
| `agent_completed` | 子 Agent 结束;常驻团队成员在被停下时。 | 会话 | `agent_id`、`duration_ms`、`had_error`、`role` |
| `team_stage_complete` | `TeamCreate` 流水线的一个阶段跑完。 | 会话 | `agent_count`、`agents_with_errors`、`duration_ms`、`stage_name`、`success` |
<!-- END GENERATED TABLE -->
