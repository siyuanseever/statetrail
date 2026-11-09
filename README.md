# statetrail

**See every Claude Code session in tmux at a glance — who is waiting for you, who is done — and jump to any turn of any
session.**

<p align="center">
  <img src="tmux/docs/demo.gif" alt="A Claude in another session asks for permission: its dot in the status bar turns red; one click jumps there; prefix g lists every Claude by session; clicking a turn in the trail scrolls Claude to it" width="960">
</p>

<p align="center"><sub>Real tmux and the real program; the Claude sessions and their data are a demo rig
(<a href="tmux/demo/">tmux/demo</a>).</sub></p>

| A dot per Claude; the red one waits for you | `prefix g`: every Claude by session | Click a turn: Claude scrolls there |
|---|---|---|
| ![Status bar dots and the trail beside Claude](tmux/docs/statusbar-trail.png) | ![Popup listing every Claude by session with state, age, task and live preview](tmux/docs/goto.png) | ![After clicking turn 3 in the trail, Claude shows that turn's reply](tmux/docs/jump.png) |

[简体中文](README.zh-CN.md)

## What you get

- **A dot per Claude in the status bar**, grouped by tmux session: red waits for you (a permission prompt, a question),
  green finished while you were elsewhere, gold is running, grey you have seen. Click a dot to go there, click a
  session name to switch to it.
- **`prefix g`**: a popup listing every Claude by session, with how long it has been in that state, what it was asked,
  and a live preview. `h` `l` move between sessions, `j` `k` between Claudes, `Enter` jumps.
- **The trail**, docked to the right of the Claude you are looking at: one row per turn (what you asked, how the reply
  starts), grouped into chapters; click a turn and Claude's own screen scrolls there. `/` searches this session,
  `Tab` every session on the machine. The header shows model, account, context and 5h / 7d quota.

State comes from reading each pane's screen (with [herdr](https://herdr.dev)'s Claude detection rules) and from the
session files Claude Code writes itself. No hooks, nothing added to `~/.claude/settings.json`. One small Rust binary:
the background part takes about 15 MB of memory and each open trail about 10 MB; a Claude's screen is only read when
its window has new output (and every 30 seconds as a safety net).

## Install

macOS or Linux, tmux 3.2+ (clicking the dots needs 3.4+, the Git and file floats 3.7+).

```bash
curl -fsSL https://raw.githubusercontent.com/siyuanseever/statetrail/main/tmux/get.sh | sh
```

Then add this line at the end of your tmux config and reload tmux:

```tmux
run-shell '~/.local/bin/statetrail-tmux init'
```

Jumping to a turn needs Claude Code's fullscreen UI (`/tui fullscreen`). Options, keys and how it works:
[tmux/README.md](tmux/README.md).

## Also for herdr

The same trail and popup as a [herdr](https://herdr.dev) plugin:
`herdr plugin install siyuanseever/statetrail/herdr` — see [herdr/](herdr/).

## Layout

| | |
|---|---|
| [`tmux/`](tmux/) | statetrail for tmux |
| [`herdr/`](herdr/) | the herdr plugin |
| [`core/`](core/) | shared by both: transcripts, quota, drawing, jumping, chapters, search, the popup, screen-state detection |

## Credits

The screen-state rules (`core/src/detect/claude.toml`) and the way they are matched come from
[herdr](https://github.com/herdrdev/herdr), Apache License 2.0; its license is next to them.

## License

MIT
