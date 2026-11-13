#!/usr/bin/env bash
# 从 CHANGELOG.md 截取指定版本的段落，供 Release 说明使用。
#
# 用法：extract_changelog.sh v0.1.2 [CHANGELOG.md]
# 退出码：找不到该版本或段落为空时非 0，避免发出空说明的 Release。

set -euo pipefail

version="${1:-}"
file="${2:-CHANGELOG.md}"

if [[ -z "$version" ]]; then
  echo "用法：$0 <版本号，如 v0.1.2> [CHANGELOG.md]" >&2
  exit 2
fi

if [[ ! -f "$file" ]]; then
  echo "找不到 $file" >&2
  exit 2
fi

# 标题必须从行首精确匹配 "## [v0.1.2]"，避免 v0.1.2 命中 v0.1.20。
# 段落结束于下一个二级标题；首尾空行在 END 里裁掉。
body="$(
  awk -v target="## [${version}]" '
    index($0, target) == 1 { found = 1; next }
    found && /^## / { exit }
    found { lines[++n] = $0 }
    END {
      start = 1
      while (start <= n && lines[start] ~ /^[[:space:]]*$/) start++
      end = n
      while (end >= start && lines[end] ~ /^[[:space:]]*$/) end--
      for (i = start; i <= end; i++) print lines[i]
    }
  ' "$file"
)"

if [[ -z "${body//[[:space:]]/}" ]]; then
  echo "CHANGELOG.md 里没有 ${version} 的说明；先补好该版本段落再打 tag。" >&2
  exit 3
fi

printf '%s\n' "$body"
