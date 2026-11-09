#!/bin/sh
# statetrail for tmux：下最新发布的 tmux 版到 ~/.local/bin（STATETRAIL_BIN_DIR 可改），再告诉你 tmux.conf 里加哪一行。
#   curl -fsSL https://raw.githubusercontent.com/siyuanseever/statetrail/main/tmux/get.sh | sh
# Installs the latest statetrail for tmux into ~/.local/bin and prints the one line to add to tmux.conf.
set -eu
repo=siyuanseever/statetrail
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) t=aarch64-apple-darwin ;;
  Darwin-x86_64) t=x86_64-apple-darwin ;;
  Linux-x86_64) t=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) t=aarch64-unknown-linux-musl ;;
  *)
    echo "No prebuilt binary for $(uname -s) $(uname -m). Build from source:"
    echo "  cargo install --git https://github.com/$repo statetrail-tmux"
    exit 1
    ;;
esac
tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=50" |
  grep -o '"tag_name": *"tmux-v[^"]*"' | head -n 1 | sed 's/.*"\(tmux-v[^"]*\)"/\1/')
[ -n "$tag" ] || { echo "No tmux release found on github.com/$repo"; exit 1; }
dir=${STATETRAIL_BIN_DIR:-$HOME/.local/bin}
mkdir -p "$dir"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
url="https://github.com/$repo/releases/download/$tag/statetrail-tmux-$t.tar.gz"
curl -fsSL "$url" -o "$tmp/a.tar.gz"
curl -fsSL "$url.sha256" -o "$tmp/a.sha256"
want=$(cut -d' ' -f1 "$tmp/a.sha256")
got=$( (shasum -a 256 "$tmp/a.tar.gz" 2>/dev/null || sha256sum "$tmp/a.tar.gz") | cut -d' ' -f1)
[ "$want" = "$got" ] || { echo "Checksum mismatch for $url"; exit 1; }
tar -xzf "$tmp/a.tar.gz" -C "$tmp"
# 先放旁边再改名：正在跑的那个不会被原地改写（macOS 上原地改写会被系统杀掉），它几秒内发现换了新的、自己重启
mv "$tmp/statetrail-tmux" "$dir/.statetrail-tmux.new"
mv "$dir/.statetrail-tmux.new" "$dir/statetrail-tmux"
echo "Installed statetrail-tmux $tag to $dir"
if ! grep -qs "statetrail-tmux init" "$HOME/.tmux.conf" "$HOME/.config/tmux/tmux.conf" "$HOME/.tmux.conf.local"; then
  echo
  echo "Add this line at the end of your tmux.conf (oh-my-tmux: at the end of .tmux.conf.local), then reload tmux:"
  echo "  run-shell '$dir/statetrail-tmux init'"
fi
