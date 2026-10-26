---
name: agix-plan-and-execute
description: 计划与实施 — 将需求/设计/方案分解为可执行任务，逐任务实施验证。每个增量后项目保持可构建。
---

# 计划与实施

> **本阶段目标：** 把上阶段的产出（需求/设计/方案）分解为有序任务列表，逐任务实施。每个 task 完成后项目必须可构建且测试通过。

## 前置条件

- 以下之一已就绪：需求规格 / 架构设计 / 修复方案（由前述 skill 产出）
- 如输入不足（缺设计、需求模糊），退回上一阶段

## 流程

### Phase 1: 规划

1. **读取输入** — 理解需求/设计/修复方案
2. **分解任务** — 按依赖排序，每个 task 粒度 10–20 分钟，只改一个逻辑点
3. **展示确认** — 在对话中输出轻量任务列表，等待用户确认

任务列表格式（不写文件）：
```
## Task List (~N tasks)
- T1: [标题] — 涉及文件: `crates/agix-*/src/file.rs` (新建/修改)
- T2: [标题] — 涉及文件: `crates/agix-*/src/file.rs`
- ...
```

### Phase 2: 执行

对每个 task 执行循环：
```
1. 理解 → 读相关代码
2. 编码 → 仅改当前 task 要求的文件，优先 TDD（RED→GREEN→REFACTOR）
3. 自检 → 审视刚才的改动有没有缺陷
4. 验证 → clippy + test + build 必须通过
5. 下一个 →
```

### Phase 3: 收尾

全量验证：
- `cargo nextest run --workspace` — 全量测试
- `cargo clippy --workspace -- -D warnings` — lint
- `cargo fmt --all -- --check` — 格式检查
- `cargo build --workspace` — 构建

> 如改动跨 `AttaMeta/` 和 `ClawPod/` 两个 workspace，需在两个 workspace 各跑一次。

## 铁律

- **范围纪律** — 只改当前 task 的内容。发现无关问题不顺手改，记录下来最后统一询问用户。
- **每次增量可构建** — 一个 task 完成后 clippy + build 必须通过，不攒到最后一起来。
- **简单优先** — 写最简可工作版本，不为假想需求设计。
- **阻塞即停** — 遇到不确定的事不要硬猜，向用户求助。
- **不产出独立计划文档** — 任务列表在对话中输出即可。此 skill 替代旧的 `write-plan` + `execute-plan`。

## 简报

全部 task 完成后，输出收尾简报：

```markdown
## 实施完成

### 变更清单
- `crates/agix-*/src/file.rs` — ...（每个文件一行）

### 验证结果
| 检查项 | 结果 |
|--------|------|
| `cargo nextest run --workspace` | ✅/❌ |
| `cargo clippy --workspace -- -D warnings` | ✅/❌ |
| `cargo fmt --all -- --check` | ✅/❌ |
| `cargo build --workspace` | ✅/❌ |

### 下一步
→ `/agix-review-and-fix`（检视与修复）
```
