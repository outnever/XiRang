#!/usr/bin/env bash
# 打一个 macOS 的 XiRang.app —— 不用额外打包工具，cargo + 手写 Info.plist 就够。
#
# 用法：scripts/make_macos_app.sh
# 产出：dist/XiRang.app（双击里面的可执行文件即可运行；带 .xirang 文件关联）
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
app_crate="$root/app"
out="$root/dist/XiRang.app"

# 仓库自带的 Rust 工具链（.cargo / .rustup）；没有就用系统的
if [ -x "$root/.cargo/bin/cargo" ]; then
  export PATH="$root/.cargo/bin:$PATH"
  export RUSTUP_HOME="$root/.rustup"
  export CARGO_HOME="$root/.cargo"
fi

echo "→ 编译 release（app/Cargo.toml）"
cargo build --release --manifest-path "$app_crate/Cargo.toml"

bin="$app_crate/target/release/xirang-app"
if [ ! -x "$bin" ]; then
  echo "找不到编译产物：$bin" >&2
  exit 1
fi

rm -rf "$out"
mkdir -p "$out/Contents/MacOS" "$out/Contents/Resources"
cp "$bin" "$out/Contents/MacOS/XiRang"
chmod +x "$out/Contents/MacOS/XiRang"

cat > "$out/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>XiRang</string>
  <key>CFBundleDisplayName</key><string>息壤 XiRang</string>
  <key>CFBundleIdentifier</key><string>com.xirang.app</string>
  <key>CFBundleExecutable</key><string>XiRang</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>息壤文件</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSHandlerRank</key><string>Owner</string>
      <key>LSItemContentTypes</key>
      <array><string>com.xirang.nodefile</string></array>
      <key>CFBundleTypeExtensions</key>
      <array><string>xirang</string></array>
    </dict>
  </array>
  <key>UTExportedTypeDeclarations</key>
  <array>
    <dict>
      <key>UTTypeIdentifier</key><string>com.xirang.nodefile</string>
      <key>UTTypeDescription</key><string>息壤节点文件</string>
      <key>UTTypeConformsTo</key>
      <array><string>public.data</string></array>
      <key>UTTypeTagSpecification</key>
      <dict>
        <key>public.filename-extension</key>
        <array><string>xirang</string></array>
      </dict>
    </dict>
  </array>
</dict>
</plist>
PLIST

echo "→ 完成：$out"
echo "   运行：open \"$out\"       （或 open \"$out\" --args 某个文件.xirang）"
echo "   双击 .xirang 关联：把 $out 拖进「应用程序」，或在 Finder 里选它作为打开方式"
