#!/bin/bash
# PreToolUse hook（.agix/settings.json 里 hooks_config.PreToolUse 引用）。
# 把每次匹配到的工具调用 payload 追加写进日志，用来验证 hooks 接线在模板项目里
# 真的生效——不是靠读代码猜的。
set -euo pipefail

log_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/hooks_log"
mkdir -p "$log_dir"

input="$(cat)"
printf '%s\n' "$input" >> "$log_dir/pre_bash_log.jsonl"

echo '{"continue": true}'
