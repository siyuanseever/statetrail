#!/usr/bin/env bash
# herdr 装插件时跑的 [[build]]：把这个平台编好的 statetrail 下到 bin/。`herdr plugin link`（本地开发）不跑它。
# 发版标签和清单里的 version 对上（v<version>），所以哪个版本的代码就下哪个版本的程序。
# 下不到（没有这个平台的包、断网）而本机有 cargo，就地编一个。
set -euo pipefail

NAME="statetrail"
REPO="siyuanseever/statetrail"
# build 命令不一定拿得到 herdr 的环境变量，从脚本自己的位置找插件根目录
ROOT="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$ROOT/bin"

VERSION="$(grep -m1 '^version' "$ROOT/herdr-plugin.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
TAG="v${VERSION}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  Linux-aarch64 | Linux-arm64) target="aarch64-unknown-linux-musl" ;;
  Linux-x86_64) target="x86_64-unknown-linux-musl" ;;
  *) target="" ;;
esac

build_from_source() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "$NAME: $1, and cargo is not installed; install Rust (https://rustup.rs) and reinstall the plugin" >&2
    exit 1
  fi
  echo "$NAME: $1, building from source with cargo"
  (cd "$ROOT" && cargo build --release --locked)
  mkdir -p "$BIN_DIR"
  # 先写到旁边再改名：macOS 上原地改写正在用的程序，一跑就被系统杀掉
  cp "$ROOT/target/release/$NAME" "$BIN_DIR/.$NAME.new"
  mv "$BIN_DIR/.$NAME.new" "$BIN_DIR/$NAME"
  echo "$NAME: installed $BIN_DIR/$NAME"
  exit 0
}

[ -n "$target" ] || build_from_source "no prebuilt binary for $(uname -s)-$(uname -m)"

archive="${NAME}-${target}.tar.gz"
base="https://github.com/${REPO}/releases/download/${TAG}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# 刚发的版本 GitHub 的 CDN 可能几分钟内还是 404：每次失败都重试
dl() {
  for _ in 1 2 3 4 5 6; do
    curl -fsSL "$1" -o "$2" && return 0
    sleep 3
  done
  return 1
}

echo "$NAME: downloading $archive ($TAG)"
if ! dl "$base/$archive" "$tmp/$archive" || ! dl "$base/$archive.sha256" "$tmp/$archive.sha256"; then
  build_from_source "could not download $base/$archive"
fi

expected="$(awk '{print $1}' "$tmp/$archive.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$archive" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')"
fi
if [ "$expected" != "$actual" ]; then
  echo "$NAME: checksum mismatch (expected $expected, got $actual)" >&2
  exit 1
fi

mkdir -p "$BIN_DIR"
tar -xzf "$tmp/$archive" -C "$tmp"
install -m 0755 "$tmp/$NAME" "$BIN_DIR/.$NAME.new"
mv "$BIN_DIR/.$NAME.new" "$BIN_DIR/$NAME"
echo "$NAME: installed $BIN_DIR/$NAME"
