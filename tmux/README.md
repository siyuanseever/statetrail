# statetrail for tmux

**Every turn of your Claude Code session, docked next to the running Claude in tmux: click one and Claude scrolls to it.**

[中文](README.zh-CN.md)

Written in Rust. It replaces the Python + fzf sidebar of
[claude-tmux-sessions](https://github.com/siyuanseever/claude-tmux-sessions). Reading transcripts, quota, drawing,
jumping, chapters and search are shared with the herdr plugin (`core/` at the repository root); this directory is only
the tmux layer.

## What it does

- **The trail.** One item per turn: what you asked and how Claude's final reply starts. Click one and Claude's own
  screen scrolls to that reply. Turns before `/compact` fold into one row; turns Claude no longer shows are greyed out.
- **Follows where you are in Claude.** Scroll up in Claude and the selection follows the turn you are reading. Only the
  trail beside the Claude you are using does this, and only when Claude's window has new output.
- **Chapters.** Turns are grouped by task; only the selected chapter is expanded. Claude Haiku decides where chapters
  start (only for the Claude you are looking at, with that session's own account); right-click to fix.
- **Search.** The search button (or `/` when the trail has focus) filters this session; `Tab` searches everything you
  said in every session on this machine. Open sessions are switched to; closed ones resume in a new window.
- **Keyboard.** `↑` `↓` / `k` `j`, PageUp / PageDown, Home / End, `←` `→` between chapters and turns, `Enter` jumps,
  `Esc` gives focus back to Claude. The wheel moves the selection. Clicking the trail does not steal focus.
- **▲ 12 / ▼ 5** above and below the list when it is longer than the pane; click to page.
- **Narrow / wide**: `»` shrinks to 17 columns, `«` brings it back.
- **Header**: model, effort, account, `ctx` `5h` `7d` bars (grey until something needs attention) and reset countdowns.
  Buttons: bottom, search, Git (with the number of uncommitted files), files, refresh quota, close. Click the account
  name to switch this Claude to another account in place (via claude-tmux-sessions' `switch_account.py`).
- **Git and files open as floating panes** on the right. Outside a repository, Git first lists the repositories below
  with their branch and a `●` for uncommitted changes.
- **Same tmux behaviour as the Python sidebar**: auto-opens in wide windows, remembers when you closed it, a shell above
  it, makes way for agent-teams teammates, nothing left behind after tmux-resurrect.
- **Restarts itself on upgrade**: after `install.sh`, open trails and the daemon switch to the new binary in place.

## Install

Needs tmux 3.7+, Rust, and claude-tmux-sessions (its hook writes `~/.claude/tmux-claude-status.json`; it also provides
`float-tool.sh`, `usage-refresh.py` and `switch_account.py`). Claude Code must use the fullscreen UI (`/tui fullscreen`).

```bash
cd tmux
./install.sh             # builds and installs tmux/bin/statetrail-tmux
./install.sh --launchd   # also installs the daemon as launchd service com.statetrail.tmux
```

Point the status-bar button at it: in claude-tmux-sessions' `bin/float-tool.sh`, make the `tool:trail` branch
`exec /path/to/statetrail/tmux/bin/statetrail-tmux toggle "$caller"`. Pane options are the same as the Python sidebar's,
so the tmux config and the resurrect hooks need no change.

## Configuration

`~/.config/statetrail-tmux/config.toml`; see [README.zh-CN.md](README.zh-CN.md#配置) for every key
(`auto_open`, `default_mode`, `width`, `min_width`, `shell_height`, `chapters`, `live_usage`, `icons`, `lang`, `scripts`, `[theme]`).

## Memory and CPU

Measured 2026-11-08 on macOS with tmux 3.7c: 13 windows, each with a fake Claude drawing a real transcript (303 MB in
total) and a trail, idle for 60 s.

| | Python + fzf | Rust |
|---|---|---|
| Daemon | 47 MB, 0.85 % of a core (3 sidebars; ~49 MB, 1–2 % with 13) | 2.1 MB, 0.10 % |
| Each trail | one fzf, ~10 MB, ~0.1 % | 3.0–5.5 MB, 0–0.05 % idle |
| 13 trails + daemon | ~180 MB | 52 MB, 0.22 % in total |

Both daemons start a tmux client once a second (`list-panes`); a cold tmux client costs about 10 ms of CPU. Counting
those clients and the tmux server, the Rust daemon is about 1.3 % of a core. Trails only read Claude's screen when its
window has new output.
