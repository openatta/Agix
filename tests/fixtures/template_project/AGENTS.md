# 项目指令

这是 Agix 测试系统的模板项目——一个最小但配置完整（agent/skills/hooks/rules/mcp 全配好）的
fixture，供 `.test` 用例在其**拷贝**上运行，验证配置真的生效，而不是只测裸 Agent。

## 项目结构

```
src/main.py   — 一个最小的 Python 脚本，供 Agent 实际读/改/跑
```

## 构建与测试命令

```sh
python3 src/main.py
```

## 编码约束

- 改动 `src/` 下的代码后，运行一次 `python3 src/main.py` 确认没有语法错误。
- 保持改动最小，不要引入本用例说明之外的额外文件。

## 详细规则

完成编码任务前，遵循：
- `.agix/rules/testing.md`

## 可用 Skills

见 `.agents/skills/`，Agent 会按需自动发现并调用。
