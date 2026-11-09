# statetrail（tmux 版）

**Claude Code 会话的每一轮，贴在 tmux 里正在运行的 Claude 旁边：点一轮，Claude 就滚到那一轮。**

[English](README.md)

Rust 写的，替掉 [claude-tmux-sessions](https://github.com/siyuanseever/claude-tmux-sessions) 里 Python + fzf 的侧栏。
读对话记录、额度、画面、跳转、分章、搜索和 herdr 版共用一份代码（仓库根目录的 `core/`），这里只有和 tmux 打交道的那一层。

## 有什么

- **轨迹。** 每轮一项：你说了什么、Claude 最终回复的开头。点一轮，Claude 自己的屏幕滚到那一轮的最终回复。
  `/compact` 之前的轮次收成一行；主界面里已经翻不到的轮次灰显、写「主界面已无」，再点直接提示，不白翻。
- **跟着你在 Claude 里看到哪儿。** 在 Claude 里往上翻，轨迹的选中跟到你正在看的那一轮；回到最底就是最新一轮。
  只有你正在用的那个 Claude 旁边的轨迹这样做，而且只在 Claude 的窗口有了新输出时才去读它的屏幕。
- **分章。** 轮次按任务分章，同时只展开选中那一轮所在的章，收着的章写几轮。哪一轮开新章由 Claude Haiku 判断
  （只判断你正在看的那个 Claude，用它自己的账号）；分错了右键一轮「这里开新章」、右键章名「并回上一章」。
- **搜索。** 焦点在轨迹上时按 `/`，打字只留下含这些字的轮次，`Enter` 跳过去。
  搜索框里按 `Tab` 搜本机所有会话里你说过的话（轨迹临时放大到整个窗口）：开着的会话就切过去，没开就新开一个窗口
  `claude --resume`。
- **键盘。** 焦点在轨迹上时：`↑` `↓`（`k` `j`）挪，PageUp / PageDown，Home / End（`g` `G`），`←` `→`（`h` `l`）在章和轮之间，
  `Enter` 跳，`Esc` 把焦点还给 Claude。滚轮挪选中，不跳。鼠标点轨迹焦点就到轨迹上（和 herdr 插件一样）。
- **上下还有几轮。** 列表比窗格长时，上面写「▲ 12」、下面写「▼ 5」，点一下翻一屏。
- **窄版 / 宽版。** 默认开窄版（17 列，和 herdr 插件一样）；表头第一个按钮 `«` 放宽，`»` 收回窄版。
- **表头。** 模型、effort、账号，`ctx` `5h` `7d` 三条柱子（平时灰，超出匀速线才上色）和重置倒计时；点柱子马上查一次额度。
  按钮布局和 herdr 插件一样：最上面一行 宽窄、到最底、刷新额度（宽版后面写读数多新）；
  最底下一行 Git（宽版后面是没提交的文件数）、文件。
  点账号名选另一个账号，就地换（交给 claude-tmux-sessions 的 `switch_account.py`，它有安全闸）。
- **Git、文件用浮窗**，浮在右边、点外面就关（`float-tool.sh`）。目录不在仓库里时，Git 先列出下面两层的仓库和分支，
  有改动的标 `●`，点一个再开 lazygit。
- **和 Python 侧栏一样的 tmux 行为**：窗口够宽（≥120 列）自动开，从状态栏按钮关掉的那个不再自动开；上面切一条 8 行的命令行；
  窗口里有 agent teams 的队友时平铺的轨迹让开，从状态栏按钮打开时浮在右边；tmux-resurrect 恢复后不留空壳（`@resurrect_skip`）。
- **换了新版本自己重启。** `install.sh` 换上新程序后，开着的轨迹和常驻进程几秒内原地换成新的，位置不变。

## 安装

需要：tmux 3.7+（浮动窗格）、Rust（`cargo`）、claude-tmux-sessions 装好（它的 hook 写
`~/.claude/tmux-claude-status.json`，告诉轨迹哪个窗格是哪个会话；还有 `float-tool.sh`、`usage-refresh.py`、`switch_account.py`）。
Claude Code 要用全屏界面（`/tui fullscreen`），跳转滚的就是它。可选：lazygit、yazi、Nerd Font。

```bash
cd tmux
./install.sh             # 编好装到 tmux/bin/statetrail-tmux
./install.sh --launchd   # 再装常驻服务 com.statetrail.tmux（开机自启、崩了自动拉起）
```

状态栏的轨迹按钮改成调它：claude-tmux-sessions 的 `bin/float-tool.sh` 里 `tool:trail` 那一支换成

```bash
exec /path/to/statetrail/tmux/bin/statetrail-tmux toggle "$caller"
```

从 Python 侧栏切过来时，先停掉它的常驻服务（`launchctl bootout gui/$(id -u)/com.claude-tmux.sidebar`），
关掉开着的 fzf 侧栏，Rust 版的常驻进程会在一秒内重新开出来。窗格上的标记两版一样，tmux 配置、resurrect 钩子不用改。

## 配置

`~/.config/statetrail-tmux/config.toml`，每一项都可以不写：

```toml
auto_open = true          # 窗口够宽时自动开（tmux 选项 @trail_auto 0 也能关）
default_mode = "narrow"   # 开出来多宽："narrow"（17 列，默认，和 herdr 插件一样）或 "wide"
width = 0.34              # 宽版占 Claude 那一格的比例
min_width = 120           # Claude 那一格至少这么宽才自动开
shell_height = 8          # 上面那条命令行的行数，0 不开（tmux 选项 @trail_shell_height 优先）
chapters = "auto"         # "auto"（Haiku 分章）/ "manual"（只按你右键标的）/ "off"
live_usage = true         # 额度旧了（30 分钟）经 usage-refresh.py 去查；false 只读现有读数
icons = "auto"            # "auto" / "nerd" / "unicode" / "text"
lang = ""                 # "zh" / "en"；不写就和 claude-tmux-sessions 一样：CLAUDE_TMUX_LANG > ~/.config/claude-tmux/lang > 中文
scripts = "~/projects/claude-tmux-sessions/bin"   # 不写就顺着 ~/.claude/hooks/float-tool.sh 那条软链找

[theme]
name = "catppuccin"       # 配色，名字和 herdr 的主题一样
```

## 读什么、动什么

- **读** 对话记录（`<配置目录>/projects/*/<会话>.jsonl`）、hook 写的状态文件、`.claude.json`（账号名）、
  usage-refresh.py 写的 `<配置目录>/tmux-usage-live.json`（额度，和 tmux 底栏同一份）。从不写它们。
- **发键** 给 Claude 窗格只为滚屏（PageUp / PageDown / Ctrl+Home / Ctrl+End / 滚轮）。换账号由 switch_account.py 做。
- **分章** 会把你说的话发给 Claude Haiku（`claude -p --model haiku`，用那个会话自己的账号，不带工具、不留会话记录），
  只发你正在看的那个 Claude 的；`chapters = "manual"` / `"off"` 什么都不发。
- 自己的状态在 `~/.local/state/statetrail-tmux/`：命令口 `run/<轨迹窗格>.sock`、分章 `chapters/`、跨会话搜索的索引、
  `jump.log`、`daemon.log`。

## 怎么做的

- **常驻进程**（`statetrail-tmux daemon`）每秒一次 `tmux list-panes -a`，别的都不做：默认打开、Claude 没了就收、
  队友来了就让、补 `@resurrect_skip`，再把「窗口有没有人看、焦点在不在这一对上、窗口最后一次有输出的时间、Claude 的目录」
  变了才发给对应的轨迹。
- **每个轨迹**是一个小程序，画面全用 core。对话记录和状态文件每秒 stat 一次（没人看时 5 秒），变了才读新追加的部分；
  不轮询 tmux。读 Claude 的屏幕只在焦点在这一对上、窗口有了新输出时做。
- 点一轮的跳转和 herdr 版是同一份算法（core 的 `jump`）：从近的一头翻页、每翻一下读一次屏，看到那一轮回复的开头就停。

### 实测内存和 CPU

2026-11-08，macOS，tmux 3.7c。Rust 版在单独的 tmux 服务器里开 13 个窗口，每个一个假 Claude（画真实对话记录，
最近改过的 13 份共 303MB）加一个轨迹，空闲 60 秒；footprint 是 macOS `footprint` 的物理内存占用，CPU 是 `ps` 的 CPU 时间之差。
Python 版是同一天这台机器上正在跑的那套（当时开着 3 个侧栏）。

| | Python + fzf | Rust |
|---|---|---|
| 常驻进程 | `trail_daemon.py` 47MB，自己 0.85% 单核（3 个侧栏；13 个时约 49MB、1–2%） | 2.1MB，自己 0.10% |
| 每个轨迹 | 一个 fzf，约 10MB，0.08–0.10% | 3.0–5.5MB，空闲 0–0.05% |
| 13 个轨迹 + 常驻进程 | 约 49 + 13 × 10 ≈ 180MB | 52MB，合计 0.22% |

两版的常驻进程都每秒起一次 tmux 客户端（`list-panes`）。隔一秒冷启动一次 tmux 客户端约 10 毫秒 CPU（连着调只要 3 毫秒，
`statetrail-tmux bench-tmux` 可以量），所以算上它起的 tmux 客户端和 tmux 服务器那边，Rust 常驻进程合计约 1.3% 单核
（`/usr/bin/time -l` 61 秒 0.62 秒，tmux 服务器多 0.14 秒）。Python 版这一笔一样要付，外加它自己的 0.85–2%，
以及每个看得见的侧栏每秒一次 `capture-pane`；Rust 版的轨迹只在 Claude 的窗口有新输出时才读屏。

## 已知限制

- 跳转靠 Claude Code 全屏界面的滚动快捷键；`/compact` 之前、续接会话里较早的部分，主界面已经没有了，会标出来。
- 最后一两轮下面内容不够一屏时，主界面没法把它推到顶部。
- 按文件顺序切轮：用过 `/rewind` 的会话，被放弃的分支也会列出来。
- 窄版最窄 17 列。
