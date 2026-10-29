# statetrail

**Every turn of your Claude Code session, docked next to the running Claude — click one and Claude scrolls to it.**

A [herdr](https://herdr.dev) plugin. It sits on the right of a Claude Code pane and lists the session turn by turn:
what you asked, and how Claude's final reply starts. Click a turn and Claude's own screen scrolls to that reply.
On top: the model, the account, and context / 5-hour / 7-day quota bars.

[中文说明](README.zh-CN.md)

![statetrail in herdr: open, click a turn, narrow, wide, Git button](docs/demo.gif)

Tools like claude-code-history-viewer or claude-code-transcripts let you browse sessions after the fact.
statetrail is for the session you are in: it stays beside the live Claude and follows it as it works.

## What you get

- **The trail.** One row per turn. Click a row and Claude jumps there, usually in well under a second.
  Turns before a `/compact` fold into one row. The list refreshes as soon as Claude starts or stops working.
  When Claude exits, the trail closes within a few seconds (with its pane, at once); start Claude again and it reopens.
  When the list is longer than the pane, `▲ 12` at the start of the line above it and `▼ 5` at the start of the
  line below it say how many turns are out of view (in the narrow view `▲12` gets the last header row to itself and
  `▼5` sits on the bottom row); click one to page by a screenful. They sit on the left, in line with the bars, in a
  slot that stays when a side runs out, so nothing next to them moves while you page.
- **Follows where you are in Claude.** Scroll Claude's own screen up and the trail marks the turn you are reading
  (and pages to it); back at the bottom it marks the latest turn. Only the trail next to the Claude you are in does
  this, reading Claude's screen twice a second (about 0.1% of one core); every other trail stays asleep.
- **Chapters.** Turns are grouped by task: each new request or problem starts a chapter with a short title, and its
  turns sit indented under it. All chapters start open; click a title to close or reopen one. Claude Haiku
  decides where chapters start by reading what you typed (only for the Claude you are looking at, once per message,
  about $0.002 for 80 messages). Right-click a turn for *New chapter* or a title for *Join above* when it guessed
  wrong; your marks win. `chapters = "manual"` keeps only your marks, `"off"` keeps the flat list.
- **Model, account, quota.** `ctx`, `5h` and `7d` bars, grey while things are fine and coloured only when they
  are not: a quota bar turns orange or red when you are using it faster than the window passes, not just when it is
  high. Each quota also says how long until its window resets (`2h41m`, `2d12h`). Two Claude accounts at once
  (separate `CLAUDE_CONFIG_DIR`s) are fine; each pane shows its own. Model and effort are shown, not switched:
  change them with `/model` in Claude as usual.
- **Switch account in place** (optional). If you list your accounts in `~/.config/claude-tmux/accounts.json`
  (`{"accounts": {"work": {"config_dir": "~/.claude-work"}, ...}}`), click the account name (`⇄`) and pick another:
  when that Claude is idle, statetrail exits it and starts it again in the same pane with `--resume`, same session,
  other account. It refuses while Claude is working, waiting for you, or has unsent text in the prompt.
- **Narrow and wide.** It opens narrow (17 columns): model, effort, account, percentages with reset times, one short
  row per turn. Drag the divider to 30 columns or more for the full view with bars. `»` / `«` in the top-left corner
  jumps between the two widths. There is no close button: the narrow view is how you put it away.
- **Follows herdr, adds only what herdr lacks.** Colours come from your herdr theme, and a tab you never named gets
  the Claude session's name. Claude's running / waiting state is not repeated: herdr's agent list already shows it.
- **Your buttons.** A row at the bottom for your own tools — by default Git (lazygit), Files (yazi) and a shell. A tool
  opens below Claude, in Claude's working directory (Git and Files take 90% of its height, the shell half): Claude
  keeps its width and only gets shorter, and the trail stays
  usable beside it. The bottom row stays as it is. To close a tool: click its button again, quit the tool (`q` in
  lazygit and yazi, `Esc` in lazygit too, `exit` in the shell), or, for Git and Files, click anywhere outside it. Claude gets its
  height back and the focus. Clicking another button swaps the tool, so only one is open at a time. The third default
  button, `›_`, is a shell for the commands Claude can't run for you (`sudo` asking for a password,
  `gcloud auth login` in a browser, an interactive `ssh`): it starts in Claude's directory and stays open when you
  click elsewhere. If Claude's directory isn't inside a git repository (say `~/projects`), Git first lists the
  repositories one or two levels down, one per row with its branch and a `●` if it has uncommitted changes; click one
  to open lazygit there, `Esc` to leave. With a single repository below it opens that one directly; with none it just
  says so instead of letting lazygit offer to create one.
- **Your other herdr plugins, one click away.** After your own buttons and a `│`, the bottom row lists the other
  herdr plugins you have installed and enabled, one icon each, no text (on a second row in the narrow view). herdr
  plugins don't declare icons, so statetrail ships them for the ones it knows (herdr-sidebar `▥`, herdr-file-viewer
  `▭`, zoetrope `⋔`, or their Nerd Font icons); a plugin without one is listed by name under `⋯` instead, until you
  give it an `icon` in the config. A button runs the plugin's first action that works on
  this machine, in its manifest's order (herdr-sidebar: `open-sidebar`). Exception: zoetrope's first action opens an
  overlay, and herdr's overlay covers the whole tab, trail included, so its button runs `open-tab` instead
  (`open-split` would squeeze Claude from 83 to 42 columns; a new tab leaves this one untouched). A plugin that opens
  a new tab: the button switches to it, remembers it for this Claude, and on the next click just switches back there
  instead of opening another one (a new one again only after you close it). What doesn't fit
  goes into `⋯`, a small drop-down right above it with one name per plugin: click one to run it, click anywhere else
  in the trail to close it. The list is read
  when the trail opens, when you click `⟳`, and when herdr's `plugins.json` changes (install / uninstall), so an
  uninstalled plugin's button goes away by itself within a few seconds.
- **Updates itself in place.** After `herdr plugin update`, each open trail notices within about ten seconds and
  restarts in the same pane with the new code, keeping its place and width. No need to close and reopen it.

## Install

Requirements: herdr 0.9.5+ on macOS or Linux. Nothing else: installing downloads a prebuilt binary for your
platform (Apple Silicon / Intel Mac, x86_64 / arm64 Linux) from this repository's GitHub release and checks its
SHA-256. On any other platform, or offline, it builds from source if `cargo` is installed. Optional: lazygit and yazi
for the default buttons.

```bash
herdr plugin install siyuanseever/statetrail/herdr
brew install lazygit yazi        # if you want the default buttons
```

The Python version (0.1.x) is still there: `herdr plugin install siyuanseever/statetrail/herdr --ref v0.1.4`.

If something is missing, statetrail says so (a herdr notification, or a message in its pane) instead of failing silently.

Then open a tab with Claude Code in it: the trail opens on the right by itself. To toggle it from the keyboard,
bind the action in `~/.config/herdr/config.toml` and run `herdr server reload-config`:

```toml
[[keys.command]]
key = "prefix+t"
type = "plugin_action"
command = "statetrail.toggle"
description = "statetrail: open, or switch wide / narrow"
```

Claude Code should use fullscreen rendering (`/tui fullscreen`), which is what the jump scrolls.

## Configuration

`config.toml` in the directory `herdr plugin config-dir statetrail` prints. Everything is optional; a missing or
broken file means defaults.

```toml
auto_open = true          # open by itself in a tab that has exactly one Claude
default_mode = "narrow"   # width when it opens: "narrow" or "wide"
width = 0.25              # wide view, as a share of the Claude pane (Claude always keeps 110 columns)
icons = "auto"            # "auto": Nerd Font icons if one is installed, else plain symbols;
                          # or "nerd" / "unicode" / "text". Installed is not in use: if you see boxes, set "unicode"
live_usage = true         # also ask for live 5h / 7d quota with your Claude login (see below); false: never
lang = ""                 # "en" / "zh"; empty follows your locale
tab_names = true          # name an unnamed tab after the Claude session
chapters = "auto"         # group turns into chapters: "auto" (Claude Haiku, right-click to fix), "manual"
                          # (only where you right-click), "off" (flat list, nothing sent)
tool_height = 0.5         # a bottom-button tool takes this share of the Claude pane's height (0.1–0.9),
                          # unless the button sets its own height

# Bottom-row buttons. Writing any [[buttons]] replaces the default three.
# A button shows its icon, or its label when it has no icon (or icons = "text").
[[buttons]]
label = "Git"
command = "lazygit"       # opens below Claude, in Claude's working directory (through sh -c)
icon = "git"              # an icon name (git, files) or any single glyph
height = 0.9              # share of the Claude pane's height; 0.9 is the most herdr allows
pick_repo = true          # not in a repository: pick one of the repositories below first

[[buttons]]
label = "Files"
command = "yazi"
icon = "files"
height = 0.9

[[buttons]]
label = "Shell"
command = "$SHELL"        # your login shell
icon = "shell"
close_on_blur = false     # stays open when you click elsewhere; exit, or click the button again, to close

# A button can also run another herdr plugin's action instead of a command:
# [[buttons]]
# label = "Sidebar"
# action = "herdr-sidebar.open-sidebar"

# Buttons for your other herdr plugins (listed automatically). false: none.
plugin_buttons = true
# Per plugin, by its id (herdr plugin list): hide it, bind another of its actions, change its icon or name.
# [plugins."herdr-sidebar"]
# action = "show-explorer"
# icon = "◧"
# label = "side"
# [plugins."herdr-file-viewer"]
# hidden = true
```

## What it reads, what it touches

- **Reads** Claude Code's transcripts (`<config dir>/projects/*/<session>.jsonl`) and `.claude.json` (for the
  account name). It never writes them.
- **Quota: several sources, the newest wins.** Each one is used only if it is there:
  1. **Live** (on by default; `live_usage = false` turns it off): asks the endpoint Claude Code's `/usage` uses
     (`api.anthropic.com/api/oauth/usage`), at most every 30 minutes, with the OAuth token Claude Code keeps in the
     macOS Keychain (or `<config dir>/.credentials.json`). The token stays in memory for that one request; it is never
     written, logged or refreshed. Not a documented public API: if it changes, this source just goes quiet.
  2. **Status line bridge** (only if you wire it up, see below): Claude Code hands its status line the 5-hour and
     7-day percentages after every reply (Pro / Max plans). No credentials involved.
  3. **Claude Code's own cache** in `.claude.json` (`cachedUsageUtilization`, one per config dir). Always there, but
     Claude Code only refreshes it when it asks for usage (for example when you run `/usage`), so it can be days old.

  The header says how old the numbers are, and a window whose reset time has passed shows `?` rather than a stale number.
- **Chapters send what you typed to Claude Haiku** (`chapters = "auto"`, the default): `claude -p --model haiku`
  under the account of that Claude session, with no tools, no settings or hooks, and no session saved. It sends only
  your messages (each cut to 300 characters, plus the last six already grouped), never Claude's replies or tool
  output, and only for the Claude you are looking at. Results go to `chapters/<session>.json` in the state directory,
  one line per call to `chapters/chapters.log`. `"manual"` or `"off"` sends nothing.
- **Sends keys** to the Claude pane to scroll it (PageUp / PageDown / Ctrl+Home / Ctrl+End / mouse wheel). It types
  into the prompt only when you pick another account from the header: `/exit`, then the restart command.
- **Checks for updates** by reading herdr's own `plugins.json` and the modification time of its own binary.
  Nothing goes over the network.
- Its own state lives in the plugin's state directory (`~/.local/state/herdr/plugins/statetrail/`).

## Fresh quota without credentials: the status line bridge (optional)

Claude Code passes the current 5-hour / 7-day usage to your status line command after every reply. `statusline.py`
records it for statetrail and then runs your own status line command with the same input, so your status line
looks exactly as before. statetrail never edits `settings.json`; you add this yourself, in each config dir you use
(`~/.claude/settings.json`, and for example `~/.claude-work/settings.json`):

```json
{"statusLine": {"type": "command",
                "command": "python3 <plugin dir>/statusline.py -- <your current status line command>"}}
```

`herdr plugin list` shows the plugin dir. If you have no status line yet, leave out everything from `--` on — but
note that once a status line is configured, Claude Code keeps a line for it under the prompt even when it prints
nothing. If you would rather not have that line, skip the bridge and use `live_usage`, or rely on the cache.


## Known limits

- **The narrow view is about 15 columns at least.** herdr keeps split ratios between 0.1 and 0.9, so the narrowest
  a pane can be is 10% of its split.
- **No right-click menu entry.** herdr 0.9.5's pane menu is fixed; plugins cannot add to it
  ([herdrdev/herdr#5550](https://github.com/herdrdev/herdr/issues/5550)). That is why the narrow view, not a close
  button, is the way to put the trail away.
- **One Claude per tab.** With agent-team teammates in the same tab it does not open by itself.
- **Turns that are no longer on Claude's screen** (after `/compact`, or older parts of a resumed session) are marked;
  clicking them tells you instead of scrolling forever.
- **Full height** means the full height of the Claude pane; if you split the Claude pane horizontally, the trail
  is as tall as the Claude part.
- Only Claude Code for now. The transcript reader is a separate layer (`src/transcript.rs`) so other agents can follow.

## Roadmap

- Tokens per turn; expand a turn's tool calls; mark where `/compact` happened
- Search across sessions
- Other agents (Codex, OpenCode)

## How it works

Each trail is one small Rust program (about 1 MB on disk, about 7 MB of memory; 26 open trails measured 0% CPU
together while idle). It subscribes to herdr's events for its Claude pane (status changes, pane closed) and sleeps
otherwise. Reading and scrolling Claude's screen go through herdr's socket. `src/transcript.rs`, `claude.rs`,
`render.rs` and `jump.rs` are not herdr-specific (transcripts, quota, rendering, the jump algorithm); `pane.rs` and
`herdr.rs` are the herdr side. Version 0.1.x was Python + fzf; 0.2 is the same plugin rewritten in Rust.

## License

MIT. The theme palettes in `src/themes.json` are extracted from herdr (Apache-2.0).
