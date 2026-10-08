#!/usr/bin/env bash
# macOS 发布构建：UI + core 测试 + Tauri universal2（arm64 + x86_64）.app 打包 + tar.gz 发布包。
# 用法：scripts/build-macos.sh [输出目录]（默认 dist/）
#
# 产物结构：Halcyon.app/ + config.example.json。
# 未做 Apple Developer ID 签名（内部使用）：用户首次打开需右键 → 打开，
# 或 xattr -dr com.apple.quarantine Halcyon.app。签名与 notarization 见
# docs/release-protocol.md 的 macOS 章节。
set -euo pipefail
cd "$(dirname "$0")/.."

# 更新授权所需的编译期注入变量（缺失时 fail fast，避免产出无 Client ID 的包）
: "${HALCYON_UPDATE_PUBLIC_KEY:?missing HALCYON_UPDATE_PUBLIC_KEY}"

npm ci --prefix ui
npm run build --prefix ui
cargo test -p halcyon-core

version=$(python3 -c 'import json; print(json.load(open("src-tauri/tauri.conf.json"))["version"])')
rustup target add aarch64-apple-darwin x86_64-apple-darwin
ui/node_modules/.bin/tauri build --bundles app,dmg --target universal-apple-darwin

bundle="target/universal-apple-darwin/release/bundle/macos/Halcyon.app"
[ -d "$bundle" ] || { echo "missing .app bundle: $bundle" >&2; exit 1; }
[ -f "$bundle/Contents/Info.plist" ] || { echo "missing Info.plist" >&2; exit 1; }
[ -x "$bundle/Contents/MacOS/halcyon" ] || { echo "missing executable" >&2; exit 1; }
file "$bundle/Contents/MacOS/halcyon"
lipo -info "$bundle/Contents/MacOS/halcyon"
codesign -dv --verbose=2 "$bundle" 2>&1 | head -5 || true

dmg="$(find target/universal-apple-darwin/release/bundle/dmg -name '*.dmg' | head -1)"
[ -n "$dmg" ] || { echo "missing dmg bundle" >&2; exit 1; }

out="${1:-dist}"
mkdir -p "$out"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cp -R "$bundle" "$tmp/"
cp config.example.json "$tmp/config.example.json"
tar -czf "$out/halcyon-v${version}-macos-universal.tar.gz" -C "$tmp" .
cp "$dmg" "$out/halcyon-v${version}-macos-universal.dmg"
echo "macOS package: $out/halcyon-v${version}-macos-universal.tar.gz"
echo "macOS dmg: $out/halcyon-v${version}-macos-universal.dmg"
