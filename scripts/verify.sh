#!/usr/bin/env bash
# Run all real-corpus import-resolution acceptance tests and print a CI summary.
#
# 分母口径（与 crates/astrolabe-core/tests/import_resolution.rs 一致，独立于 ResolverSet）：
#   * 提取：剔除注释与字符串字面量中的伪 import，支持多行语句；
#     Python `import a, b` 计两条；Rust `use a::{b, c}` 按一条 use 语句计
#     （任一展开路径指向工作区 crate 即计入）。
#   * 仓库内：Python 以 pyproject 推导的 source root + 命名空间包存在性；
#     Go 以 go.mod 模块前缀；Java 以 Maven 惯例根下的 FQN 文件；
#     Rust 以 Cargo.toml crate 名 / crate|self|super；TS 以相对路径落点存在。
# 任一语言不达标（测试失败或结果行不是 PASS）则退出码非零。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

tests=(
  python_import_resolution
  go_import_resolution
  java_import_resolution
  rust_import_resolution
  typescript_import_resolution
)

results=()
failed=0

for test_name in "${tests[@]}"; do
  output="$(cargo test -p astrolabe-core --test import_resolution "$test_name" -- --ignored --exact --nocapture 2>&1)"
  status=$?
  printf '%s\n' "$output"

  result="$(printf '%s\n' "$output" | awk -F'|' '/^ASTROLABE_RESULT\|/ { line=$0 } END { print line }')"
  if [[ -z "$result" ]]; then
    case "$test_name" in
      python_*) language="Python"; corpus="serena" ;;
      go_*) language="Go"; corpus="gin" ;;
      java_*) language="Java"; corpus="gson" ;;
      rust_*) language="Rust"; corpus="ripgrep" ;;
      typescript_*) language="TypeScript"; corpus="openvisio-oss" ;;
    esac
    result="ASTROLABE_RESULT|$language|$corpus|0|0|0.00|ERROR"
  fi
  results+=("$result")
  if [[ $status -ne 0 || "$result" != *"|PASS" ]]; then
    failed=1
  fi
done

printf '\n口径: 注释/字符串中的伪 import 不计; 仓库内判定独立于 ResolverSet。\n'
printf '\n%-12s | %-14s | %8s | %8s | %8s | %s\n' \
  "语言" "语料" "总数" "解析数" "解析率" "是否达标"
printf '%s\n' '-------------+----------------+----------+----------+----------+---------'
for result in "${results[@]}"; do
  IFS='|' read -r _ language corpus total resolved rate verdict <<<"$result"
  printf '%-12s | %-14s | %8s | %8s | %7s%% | %s\n' \
    "$language" "$corpus" "$total" "$resolved" "$rate" "$verdict"
done

exit "$failed"
