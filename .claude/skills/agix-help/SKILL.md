---
name: agix-help
description: Agix 开发工作流帮助 — 展示 skill 体系全景、选径指南、各 skill 速查。同时作为方法论基准，供其他项目评估 skill 覆盖度。
---

# Agix 开发工作流帮助

> **本 skill：** 输出 Agix 的 AI 辅助开发工作流全貌。不执行任何开发动作，只做信息展示。
> **双重角色：** 既是本项目的开发帮助，也是 Atta 方法论在各项目的基准指标 —— 其他项目可对照本 skill 评估自己的 skill 覆盖度。

## 触发时机

- 用户输入 `/agix-help`
- 用户问"有哪些 skill" / "怎么开发" / "用什么流程"
- 用户不确定该走哪条路径

## 产出

根据用户问题的具体程度，选择以下三种输出之一：

### 1. 快速选径（用户知道要干什么但不确定用哪个 skill）

```markdown
## 选径指南

| 我要... | 用这个 |
|---------|--------|
| 从零做新功能，需独立设计文档 | `/agix-analyze-requirements` → `/agix-design-architecture` → `/agix-plan-and-execute` → `/agix-review-and-fix` |
| 从零做新功能，已有设计，不用独立检视 | `/agix-analyze-requirements` → `/agix-design-architecture` → `/agix-implement` |
| 中等功能，端到端一步搞定 | `/agix-feature-dev`（实施前会让你确认） |
| 修不确定根因的 bug | `/agix-describe-problem` → `/agix-design-fix` → `/agix-plan-and-execute` → `/agix-review-and-fix` |
| 修不确定根因的 bug，快捷版 | `/agix-describe-problem` → `/agix-design-fix` → `/agix-implement` |
| 修能快速定位的 bug | `/agix-bug-fix`（修复前会让你确认） |
| 了解项目整体状态 | `/agix-status` |
| 优化代码质量、消除技术债 | `/agix-refactor`（分析后让你确认） |
```

### 2. 全景图（用户想了解 skill 体系架构）

```markdown
## Agix Skill 工作流全景

### 完整流程（6 skills，分步执行）

```
特性开发 track:
  /agix-analyze-requirements → /agix-design-architecture ↘
                                                            /agix-plan-and-execute → /agix-review-and-fix
问题修复 track:                                                ↗
  /agix-describe-problem     → /agix-design-fix
```

### 快捷路径（1 skill，合并最后两步）

`/agix-implement` 合并 plan-and-execute + review-and-fix。当前面设计/方案已就绪时使用：

```
  ...-design-architecture ↘
                            → /agix-implement（一步收尾）
  ...-design-fix          ↗
```

### 简化全流程（2 skills，端到端）

`/agix-feature-dev` 和 `/agix-bug-fix` 端到端完成全部工作。内部有**决策门**：分析/诊断完成后输出简报，用户确认后才动手改代码。

```
  /agix-feature-dev  =  需求分析 + 架构设计 + [决策门] + 实施 + 检视
  /agix-bug-fix      =  问题诊断 + 修改方案 + [决策门] + 实施 + 检视
```

### 三种路径对比

| 维度 | 完整流程 | 快捷路径 | 简化全流程 |
|------|---------|---------|-----------|
| 步数 | 4 步 | 3 步 | 1 步（内部 2 阶段） |
| 独立设计文档 | ✅ 有 | ✅ 有 | ❌ 无（对话内） |
| 独立审查 | ✅ 有 | ❌ 内嵌 | ❌ 内嵌 |
| 用户决策门 | 每步后 | 每步后 | 实施前 1 次 |
| 适用 | 大特性、需审阅 | 中等、设计已清 | 小到中等、快速迭代 |

### 重构优化（1 skill，不增功能不修 bug）

`/agix-refactor` 分析代码质量并实施重构。内部有**决策门**：分析完成后输出简报（7 维度评分 + 重构清单），用户确认后才动手。

```
  /agix-refactor  =  七维分析 + [决策门] + 逐项重构 + 回归测试
```

### 辅助

  /agix-status   —  项目状态评估（只读审计）
  /agix-help     —  本帮助（你正在看）

### 阶段隔离原则

- **需求/问题阶段** → 不读代码（项目文档除外）
- **设计/方案阶段** → 只读代码，不改代码
- **实施阶段** → 严格按 task 范围改，不顺手重构
- **检视阶段** → 只检视本次变更，不扩展范围

### 错误恢复

- 需求不清 → 退回 `/agix-analyze-requirements`
- 设计有缺陷 → 退回 `/agix-design-architecture` 或 `/agix-design-fix`
- 实施发现方案不可行 → 退回 `/agix-design-fix`
- 审查发现 >3 个 Critical → 退回 `/agix-design-fix`

### 项目技术上下文

本项目的验证命令（各 skill 的简报模板中使用）：

| 检查项 | 命令 |
|--------|------|
| 全量测试 | `cd AttaMeta && cargo nextest run --workspace` |
| Lint | `cargo clippy --workspace -- -D warnings` |
| 格式化 | `cargo fmt --all -- --check` |
| 构建 | `cargo build --workspace` |
| 文档 | `cargo doc --workspace --no-deps` |

> 注意：Agix 涉及两个 Cargo workspace（`AttaMeta/` 和 `ClawPod/`），全量验证需在两个 workspace 各跑一次。
```

### 3. 单 Skill 详情（用户问某个具体 skill 怎么用）

对请求的 skill 输出：

```markdown
## /agix-[name] — [一句话定位]

### 目标
[本阶段要达成什么]

### 前置
[需要什么输入]

### 产出
[输出什么、输出到哪]

### 铁律
- [关键约束 1]
- [关键约束 2]

### 简报
[用户看到的总结内容]

### 下一步
[完成后进入哪个阶段，或 END]
```

## 方法论基准（供其他项目对照）

Atta 方法论定义了 12 个 skills，覆盖软件开发的完整生命周期。其他项目可对照此清单评估自己的 skill 覆盖度：

| # | Skill | 阶段 | 类型 | 覆盖 |
|---|-------|------|------|------|
| 1 | `agix-analyze-requirements` | 分析 | 需求 | ✅ |
| 2 | `agix-describe-problem` | 分析 | 问题 | ✅ |
| 3 | `agix-design-architecture` | 设计 | 架构 | ✅ |
| 4 | `agix-design-fix` | 设计 | 修复方案 | ✅ |
| 5 | `agix-plan-and-execute` | 实施 | 计划+编码 | ✅ |
| 6 | `agix-implement` | 实施 | 快捷编码 | ✅ |
| 7 | `agix-feature-dev` | 实施 | 端到端特性 | ✅ |
| 8 | `agix-bug-fix` | 实施 | 端到端修复 | ✅ |
| 9 | `agix-review-and-fix` | 检视 | 审查+修复 | ✅ |
| 10 | `agix-refactor` | 优化 | 重构 | ✅ |
| 11 | `agix-status` | 辅助 | 审计 | ✅ |
| 12 | `agix-help` | 辅助 | 帮助+基准 | ✅ |

**基准指标：**
- **覆盖率：** 12/12（100%）
- **阶段分布：** 分析 2 + 设计 2 + 实施 4 + 检视 1 + 优化 1 + 辅助 2
- **路径数：** 3（完整 / 快捷 / 简化全流程）
- **决策门：** 4（feature-dev, bug-fix, refactor 各 1 次实施前确认；implement 确认 task list）

## 铁律

- **只展示信息，不执行开发动作** — 帮助 skill 是地图，不是交通工具。
- **根据用户问题粒度选输出** — 用户问"怎么修 bug"就用选径指南，用户说"介绍一下体系"就用全景图。
- **帮助内容与 CLAUDE.md 保持一致** — 如发现差异，以本 skill 为准并同步更新 CLAUDE.md。
- **基准数据保真** — 技能清单和覆盖率数字必须与实际 `.claude/skills/skills/` 目录一致。
