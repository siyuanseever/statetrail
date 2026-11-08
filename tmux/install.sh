#!/usr/bin/env bash
# 编 tmux 版轨迹，装到 tmux/bin/statetrail-tmux。开着的轨迹和常驻进程几秒内发现程序换了，原地换上新的。
#
#   ./install.sh            编 release、换上
#   ./install.sh --launchd  另外装 launchd 常驻服务 com.statetrail.tmux（开机自启、崩了自动拉起）
#
# 先写到旁边再改名：macOS 上原地改写正在跑的程序，它一跑就被系统杀掉（退出码 137）。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"
cargo build --release --locked
mkdir -p bin
cp target/release/statetrail-tmux bin/.statetrail-tmux.new
mv bin/.statetrail-tmux.new bin/statetrail-tmux
echo "installed $ROOT/bin/statetrail-tmux ($(bin/statetrail-tmux version))"

if [ "${1:-}" = --launchd ]; then
  label=com.statetrail.tmux
  plist="$HOME/Library/LaunchAgents/$label.plist"
  logdir="$HOME/.local/state/statetrail-tmux"
  mkdir -p "$logdir" "$(dirname "$plist")"
  cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array><string>$ROOT/bin/statetrail-tmux</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardErrorPath</key><string>$logdir/daemon.err</string>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string></dict>
</dict>
</plist>
EOF
  launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$plist"
  echo "launchd: $label loaded ($plist)"
fi
