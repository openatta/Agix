---
name: agix-implement
description: 实施（快捷） — 合并计划与实施 + 检视与修复最后两步。适用于已有清晰设计/方案、改动范围可控的场景。
---

# 实施（快捷路径）

> **本阶段目标：** 合并 `/agix-plan-and-execute` 和 `/agix-review-and-fix` 两个阶段。输入清晰的设计/方案，一步完成从规划到收尾。不替代分析/设计阶段。

## 何时用

| 场景 | 用哪个 |
|------|--------|
| 单文件小改动 | 直接干，无需 skill |
| 已有设计/方案，2–5 task，不需要分步规划 | `/agix-implement` ← 这个 |
| 多文件特性，需要分步规划和正式审查 | `/agix-plan-and-execute` → `/agix-review-and-fix` |

## 前置条件

- 设计/方案已就绪（由 `/agix-design-architecture` / `/agix-design-fix` / `/agix-feature-dev` 产出）
- 改动范围可控，2–5 个轻量 task

## 流程

### Step 1: 规划

输出轻量任务列表，等待用户确认（不写文件）：
```
## Task List (~N tasks)
- T1: [标题] — `crates/agix-*/src/file.rs`
- T2: [标题] — `crates/agix-*/src/file.rs`
```

### Step 2: 执行

每个 task：
```
1. 编码 → 优先 TDD（RED→GREEN→REFACTOR）
2. 自检 → 审视改动是否有缺陷
3. 验证 → clippy + test + build 必须通过
4. 下一个 →
```

### Step 3: 检视与收尾

- 全量验证：`cargo nextest run --workspace` + `cargo clippy --workspace -- -D warnings` + `cargo fmt --all -- --check` + `cargo build --workspace`
- 自审查：按五维（正确性/可读性/架构/安全/性能）快速检查变更
- 修复发现的问题
- 输出变更总结

> 如改动跨 `AttaMeta/` 和 `ClawPod/` 两个 workspace，需在两个 workspace 各跑一次验证。

## 铁律

- **范围纪律** — 只改规划范围内的内容。发现无关问题记录下来，不做。
- **每次增量可构建** — clippy + build 不等到最后。
- **内嵌检视** — 每个 task 的自检 + 收尾的自审查替代独立的 review-and-fix。发现 Critical 问题 >3 个时停止，退回完整流程。
- **阻塞即停** — 不确定的事向用户求助，不硬猜。

## 简报

全部完成后输出收尾简报：

```markdown
## 实施完成

### 变更清单
- `crates/agix-*/src/file.rs` — ...（每个文件一行说明）

### 验证结果
| 检查项 | 结果 |
|--------|------|
| `cargo nextest run --workspace` | ✅/❌ |
| `cargo clippy --workspace -- -D warnings` | ✅/❌ |
| `cargo fmt --all -- --check` | ✅/❌ |
| `cargo build --workspace` | ✅/❌ |

### 自审查
- 问题: N 个 (Critical: X, Important: Y)，已修复: N 个
- 状态: 就绪

### 后续
- ...（如有遗留项）
```
