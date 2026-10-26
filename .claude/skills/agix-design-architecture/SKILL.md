---
name: agix-design-architecture
description: 架构设计 — crate 划分、trait 边界、数据流、状态管理、技术决策。产出设计文档，为实现提供蓝图。
---

# 架构设计

> **本阶段目标：** 基于已确认的需求规格，设计软件的骨架结构。只描述"crate 做什么、数据怎么流"，不写实现代码。

## 前置条件

- 需求规格文档已就绪（由 `/agix-analyze-requirements` 或 `/agix-feature-dev` 产出）

## 流程

1. **确认输入** — 读取需求规格，理解范围与场景
2. **探索现有模式**（只读） — 查看受影响的现有 crate、可复用的 trait 和类型、项目约定（见 CLAUDE.md 和 docs/）
3. **crate/模块划分** — 每个模块单一职责，标注新建/修改/删除。注意 Agix 有两个 Cargo workspace（`AttaMeta/` 和 `ClawPod/`），新增 crate 需明确放入哪个 workspace
4. **设计数据流** — 状态存在哪（Engine 状态 / 独立 store / 文件持久化）、如何传递（trait 调用 / channel / 事件流）
5. **定义 trait 契约**（如涉及跨 crate 边界） — trait 方法签名、关联类型、错误类型、`Box<dyn Trait>` 还是泛型
6. **技术决策记录** — 每个选择给理由和替代方案

## 铁律

- **不写实现代码** — trait 签名可以写，不给函数体（`fn foo(&self) -> Bar` 可以，`{ ... }` 不行）。
- **不写计划** — "先做 A 再做 B"是实现计划的事。架构设计只定义结构，不定义顺序。
- **遵循现有模式** — 不要为了"更好"而偏离项目已有的分层和命名习惯。Agix 的错误模式是 `thiserror`（库）+ `anyhow`（应用），异步统一 `tokio`。
- **每个决策有理由** — 不做"显然应该这样"的假设。
- **注意 workspace 边界** — Agix 涉及 `AttaMeta/` 和 `ClawPod/` 两个独立 Cargo workspace。跨 workspace 的依赖只能通过 path 引用（如 `ClawPod/crates/bridge` 引用 `AttaMeta/Proto`），不能混入对方的 workspace member。

## 产出

保存为 `docs/design/YYYY-MM-DD-[slug].md`：

```markdown
# [功能名] 架构设计

**日期：** YYYY-MM-DD
**基于需求：** [需求文档路径]

## Crate/模块结构
| 模块 | 所在 Crate | 操作 | 职责 |
|------|-----------|------|------|
| ...  | `agix-*` | 新建/修改/删除 | 一句话 |

## 数据流
- 状态位置: [Engine 状态 / 独立 store / 文件持久化 / 环境变量]
- 传递路径: [触发方] → [trait 调用 / channel / 事件流] → [接收方]
- 错误路径: ...

## Trait 契约（如涉及跨 crate 边界）
| Trait | 所在 Crate | 关键方法 | 错误类型 | 使用者 |
|-------|-----------|---------|---------|--------|
| ...   | ...       | ...     | ...     | ...    |

## 状态管理
| 状态 | 类型 | 作用范围 | 持久化 |
|------|------|---------|--------|
| ...  | ...  | Engine 内 / 全局 | 是/否（持久化方式） |

## 技术决策
| 决策 | 选择 | 理由 | 替代方案 |
|------|------|------|---------|
| ...  | ...  | ...  | ...     |
```

## 简报

文档输出后，在对话中输出以下简报：

```markdown
## [功能名] 架构设计完成

**文档：** `docs/design/YYYY-MM-DD-[slug].md`

### Crate/模块结构
- [模块名] — 新建/修改/删除 — 职责一句话
- ...（列出全部，不超过 8 个）

### 数据流关键路径
- [触发方] → [trait / channel] → [接收方]（一句话描述主路径）

### Trait 契约（如涉及）
- `TraitName` — `crate_a` 定义，`crate_b` 实现（列出全部新增/修改的 trait）

### 关键决策
- ...（只列结论，每条一句话，不超过 5 条）

### 下一步
→ `/agix-plan-and-execute`（标准：分步实施 + 独立检视）或 `/agix-implement`（快捷：合并最后两步）
```
