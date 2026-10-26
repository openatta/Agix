---
name: agix-review-and-fix
description: 检视与修复 — 审查变更的正确性/可读性/架构/安全/性能，修复发现的问题，产出变更总结。流程终点。
---

# 检视与修复

> **本阶段目标：** 对已完成的变更集做最终审查，修复发现的问题，产出变更总结。这是实施流程的终点。

## 前置条件

- 代码变更已完成（由 `/agix-plan-and-execute` 或 `/agix-implement` 产出）
- 全量测试、clippy、build 已通过

## 流程

### Phase 1: 审查

按五维逐文件检查变更：

**正确性：** `Option`/`Result` 处理是否完整？是否存在 `unwrap()`/`expect()` 可能 panic？错误路径是否覆盖？竞态条件（`tokio::spawn` 的取消安全）？边界值处理？

**可读性：** 命名是否清晰（Rust 惯例：`snake_case` 函数、`CamelCase` 类型）？嵌套是否过深？不必要的 `clone()`？是否引入了不必要的抽象层？

**架构：** 是否符合项目 crate 分层？trait 边界是否清晰？依赖方向是否正确（`agix-core` 不依赖 `agix-engine`）？是否引入循环依赖？

**安全：** 是否有 `unsafe` 块？沙盒是否正确启用？secret/token 是否可能出现在日志或错误信息中？文件路径是否校验（`sanitize_cwd_for_filesystem`）？

**性能：** 是否有不必要的 `.clone()` / `.to_owned()`？同步阻塞在 async context？大 allocation 在热路径？不必要的 `Box<dyn Trait>` 间接调用？

问题分级：
- **Critical** — `unsafe` 使用不当、secret 泄露、沙盒绕过（阻塞合并）
- **Important** — 缺测试、`unwrap()` 可能 panic、错误被吞（必须修）
- **Nit** — 命名不惯用、多余的 `.clone()`、注释过时
- **Suggestion** — 建议

### Phase 2: 修复

- Critical 和 Important 问题必须修复
- 修复后重新验证（clippy + test + build）
- 修复本身也要自检，不引入新问题

### Phase 3: 总结

```markdown
## 变更总结

### 改动清单
- `crates/agix-*/src/file.rs` — 改了什么（一行说明）

### 未触碰
- `crates/agix-*/src/other.rs` — 发现 X 问题但不在本任务范围

### 潜在关注点
- 风险 1
- 风险 2

### 验证结果
- `cargo nextest run --workspace` — ...
- `cargo clippy --workspace -- -D warnings` — ...
- `cargo build --workspace` — ...

### 后续建议
- ...
```

## 铁律

- **不新增功能** — 审查发现"应该加个 X"记为 Suggestion，不做。修 bug 不顺手加 feature。
- **不重构超出变更范围的代码** — 发现"这个模块整体写得不好"记录为后续建议，不在本次顺手改。
- **不死磕** — 发现 Critical 问题超过 3 个或架构级问题，退回到 `/agix-design-fix` 重新设计。

## 简报

审查与修复完成后，输出以下简报：

```markdown
## 检视与修复完成

### 审查结论
- 审查: ✅ Approved / ⚠️ Changes requested
- 问题: N 个 (Critical: X, Important: Y, Nit: Z)
- 已修复: N 个
- 状态: 就绪 / 需回退修改

### 变更总结
- ...（Phase 3 的完整总结）

### 如需回退

审查发现需要重大修改时：
→ `/agix-design-fix` → `/agix-plan-and-execute` → `/agix-review-and-fix`
或切到简化全流程：`/agix-bug-fix`

这是流程的**终点**。
```
