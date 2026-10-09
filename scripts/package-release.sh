#!/usr/bin/env bash
# 把一个平台构建出来的 burrow 打成发布产物。
#
# 用法：scripts/package-release.sh <target-triple>
# 产物（都在 dist/）：
#   peon-burrow-<target>.tar.gz     归档（自更新下载它）
#   burrow-<target>                 裸二进制（桌面端 sidecar 直接用）
#   peon-burrow-<target>.tar.gz.sha256
set -euo pipefail

target="${1:?用法: package-release.sh <target-triple>}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dist="$root/dist"
mkdir -p "$dist"

binary="$root/target/$target/release/burrow"
if [ ! -f "$binary" ]; then
  echo "找不到构建产物：$binary" >&2
  exit 1
fi

archive="peon-burrow-$target.tar.gz"
# 只有归档里放裸二进制；桌面端要的裸二进制单独再放一份
tar -czf "$dist/$archive" -C "$(dirname "$binary")" "$(basename "$binary")"
cp "$binary" "$dist/burrow-$target"

# 自更新取的是**裸二进制**（update crate 明确不解压），所以它也要有校验和；
# 归档只是给人手动下载用的
if command -v sha256sum > /dev/null 2>&1; then
  (cd "$dist" && sha256sum "$archive" > "$archive.sha256" && sha256sum "burrow-$target" > "burrow-$target.sha256")
else
  (cd "$dist" && shasum -a 256 "$archive" > "$archive.sha256" && shasum -a 256 "burrow-$target" > "burrow-$target.sha256")
fi

echo "已生成："
ls -l "$dist"