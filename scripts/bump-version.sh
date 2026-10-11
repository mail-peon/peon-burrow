#!/usr/bin/env bash
# 统一改版本号：`[workspace.package] version` + 6 个内部依赖的版本。
#
# 用法：scripts/bump-version.sh 0.1.0 [--dry-run]
#
# 与 `bump-version.ps1` 等价（CI 在 Linux 上跑用这个）。为什么要脚本：
# 发布时 path 会被剥掉、只留版本，只改一处的话后面几个包会去找一个不存在的版本，
# 而报错（`no matching package named …`）看起来像网络问题、不像漏改一行。
set -euo pipefail

version="${1:?用法: bump-version.sh <x.y.z> [--dry-run]}"
dry_run="${2:-}"

case "$version" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "不是合法的 semver：$version（形如 0.1.0 / 0.1.0-beta.1）" >&2; exit 1 ;;
esac

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$root/Cargo.toml"
tmp="$(mktemp)"

# 只动 [workspace.package] 里的 version，以及以 peon-burrow- 开头的依赖行
awk -v v="$version" '
  /^\[workspace\.package\][[:space:]]*$/ { in_pkg = 1; print; next }
  /^\[/ { in_pkg = 0 }
  in_pkg && /^version = "/ { print "version = \"" v "\""; changed++; next }
  /^peon-burrow-[A-Za-z0-9_-]+ = \{ version = "/ {
    sub(/version = "[^"]+"/, "version = \"" v "\"")
    print; changed++; next
  }
  { print }
  END { if (changed != 7) { print "预期改 7 处，实际 " changed " 处 —— 根 Cargo.toml 结构变了" > "/dev/stderr"; exit 1 } }
' "$manifest" > "$tmp"

if [ "$dry_run" = "--dry-run" ]; then
  diff -u "$manifest" "$tmp" || true
  rm -f "$tmp"
  echo "（--dry-run：没有写盘）"
  exit 0
fi

# 保留文件原有的末尾状态：awk 的 print 总会补换行，而本仓库的 Cargo.toml 末尾没有换行，
# 白补一个会让「改版本」变成多一行的假 diff。
# （CRLF 工作区只出现在 Windows + autocrlf 的情况，那条路径走 bump-version.ps1）
if [ -z "$(tail -c1 "$manifest")" ]; then
  printf '%s' "$(cat "$tmp")" > "$tmp.trimmed"
  mv "$tmp.trimmed" "$tmp"
fi

mv "$tmp" "$manifest"
echo "已写入 $manifest"
echo "验证（cargo metadata）："
cargo metadata --manifest-path "$manifest" --no-deps --format-version 1 |
  tr ',' '\n' | grep -E '"name"|"version"' | paste - - | sed 's/^/  /'
echo "下一步：cargo build --workspace；然后 git add Cargo.toml Cargo.lock 并提交"
