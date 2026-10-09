#!/usr/bin/env bash
# 从 dist/ 里的**裸二进制**生成 latest.json（自更新读的清单）。
#
# 用法：scripts/make-manifest.sh <version> <stable|beta> <dist-dir>
#
# ⚠️ 两条契约，改错任何一条都会让线上老版本「更新检查失败」：
#   1. 字段名是 camelCase（`releasedAt` / `notesUrl`），对应 peon-burrow-update 的 serde 重命名；
#   2. `assets[].name` 必须指向**裸二进制**（`burrow-<target>[.exe]`）——
#      update crate **不解压**，指向 .zip/.tar.gz 会被它以「不支持的资产格式」拒掉。
#      归档仍然发布，只是给人手动下载用。
set -euo pipefail

version="${1:?用法: make-manifest.sh <version> <channel> <dist-dir>}"
channel="${2:?缺少 channel}"
dist="${3:?缺少 dist 目录}"
repo="${GITHUB_REPOSITORY:-mail-peon/peon-burrow}"

released_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
assets=""
first=1

for file in "$dist"/burrow-*; do
  case "$file" in
    *.sha256) continue ;;
  esac
  [ -f "$file" ] || continue
  name="$(basename "$file")"
  target="${name#burrow-}"
  target="${target%.exe}"
  size="$(wc -c < "$file" | tr -d ' ')"
  sha="$(cut -d' ' -f1 < "$file.sha256")"

  [ $first -eq 1 ] || assets="$assets,"
  first=0
  assets="$assets
    {\"target\":\"$target\",\"name\":\"$name\",\"size\":$size,\"sha256\":\"$sha\"}"
done

if [ $first -eq 1 ]; then
  echo "dist 里没有裸二进制产物，无法生成清单" >&2
  exit 1
fi

cat <<JSON
{
  "schema": 1,
  "version": "$version",
  "channel": "$channel",
  "releasedAt": "$released_at",
  "notesUrl": "https://github.com/$repo/releases/tag/v$version",
  "assets": [$assets
  ]
}
JSON