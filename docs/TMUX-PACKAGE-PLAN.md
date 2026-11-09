# tmux 版打包方案（2026-11-09 定）

## 结论

把 tmux 上的三样东西打成一个产品 **statetrail for tmux**：底部状态栏一排圆点（所有 Claude 谁在等你）、
跳转浮窗（挑一个 Claude 跳过去）、侧边轨迹（这个 Claude 干到哪一轮）。

做成后，用户只需要两步就能用上：

1. 装一个程序（下载现成的，或 `cargo install`）。
2. 往 `tmux.conf` 里加一行。

不再需要 Python、Claude 钩子和 launchd。代码放在 statetrail 仓库，仓库首页换成以 tmux 为主，herdr 插件降为一节。

先做到这一步再推广。现在的 tmux 工具装起来要两个仓库、十几个脚本、改 Claude 设置、装两个常驻服务，别人照着装大概率放弃。
10-25 发过 dev.to 文章，claude-tmux-sessions 现在 1 star；11-02 在 herdr 讨论区发帖，statetrail 现在 0 star。

## 为什么是一个产品

三样东西回答的是同一件事的两层：

- **圆点和跳转浮窗**：所有 Claude 里，现在该去哪个。
- **侧边轨迹**：进了这个 Claude，它干到哪了，点一轮跳过去。

用的时候是连着的：看圆点 → 进浮窗挑一个 → 侧边看轨迹。分开装、分开介绍，用户就得自己拼。

## 现在的样子（盘点）

| 东西 | 在哪 | 语言 | 打包后 |
|---|---|---|---|
| 状态栏圆点、窗口徽标 | statetrail `tmux/src/agents.rs` | Rust | 留，已经是读屏 + Claude 自己的会话文件 |
| 侧边轨迹、常驻进程 | statetrail `tmux/` | Rust | 留 |
| 跳转浮窗（`prefix g`） | claude-tmux-sessions `bin/claude-tmux-picker.sh`、`list-rows.sh` 等 | bash + fzf + Python，约 1500 行 | **改 Rust**：照 herdr 版 `herdr/src/goto.rs`（已经是照它做的），共用的部分抽进 `core/` |
| 状态钩子（谁在跑、会话 id、subagent） | claude-tmux-sessions `hooks/tmux_status_update.py` | Python，1400 行 | **不再需要**：会话 id 和在不在干活从 Claude 自己写的 `sessions/<进程号>.json` 读，状态读屏认 |
| 点状态栏的分发 | claude-tmux-sessions `bin/status-click.sh` | bash | 并进 Rust 程序 |
| Git / 文件浮窗 | claude-tmux-sessions `bin/float-tool.sh` | bash | 并进 Rust 程序（lazygit、yazi 没装就提示怎么装） |
| 换账号 | claude-tmux-sessions `bin/switch_account.py` | Python | 不进第一版（多账号的人少）；轨迹里的按钮在装了它时才出现 |
| 系统监视、token 报表、总览网页、恢复会话 | claude-tmux-sessions | Python / bash | 不进，留在 claude-tmux-sessions 当我自己的工具 |

## 装起来是什么样

```bash
# 1. 程序：GitHub Release 下现成的（macOS / Linux，arm / x86），或者
cargo install --git https://github.com/siyuanseever/statetrail statetrail-tmux

# 2. tmux.conf 加一行
run-shell 'statetrail-tmux init'
```

`statetrail-tmux init` 做这几件事，都可以用 tmux 选项关掉或改：

- 起常驻进程（tmux 拉起，tmux 关了它就退出），Linux 也能用。
- 状态栏加上圆点：默认接在 `status-right` 最前面；想放别处就设 `@statetrail_status off`，自己写 `#{E:@claude_dots}`。
- 绑键：`prefix g` 开跳转浮窗；点状态栏的圆点和 session 名直接跳。
- 窗口够宽时自动在 Claude 旁边开轨迹（`@trail_auto 0` 关掉）。

不改 `~/.claude/settings.json`，不装钩子。

## 分几步做

| 步 | 做什么 | 怎么算做完 | 估计 |
|---|---|---|---|
| 1 | 跳转浮窗改 Rust：`statetrail-tmux goto`，在 `display-popup` 里跑；herdr 版 goto 的列表、预览、按键搬过来 | 我自己的 tmux 里 `prefix g` 换成它用一天，没有退回旧版 | 1–2 天 |
| 2 | 去掉对 Python 钩子的依赖：会话 id、在不在干活全从 Claude 的会话文件读；钩子写的状态文件有就用、没有也行 | 在一个干净的用户目录（没装钩子）里，圆点、轨迹、浮窗都正常 | 半天 |
| 3 | `init` 子命令 + 常驻进程改由 tmux 拉起；CI 加上 tmux 版的四个平台，打标签就发 | 干净的 macOS 用户和一台 Linux 上，照 README 两步装好、零报错 | 1 天 |
| 4 | README（英文为主，附中文）+ 15 秒动图：圆点变红 → 点它跳过去 → 侧边点一轮 → Claude 滚到那里 | 动图用演示装置录（不露真实会话），README 第一屏就是动图和两行安装 | 半天到 1 天 |
| 5 | 我自己换成打包版：dotfiles 里的 tmux 配置改成 `init` 那一行，claude-tmux-sessions 只留自用的东西 | 用一周，没有退回 | 跟着平时用 |
| 6 | 推广（见下） | | |

第 1–4 步做完再推广。第 5 步和推广可以同时走。

**进度（2026-11-09）**
- 第 1 步做完，等你实际用一天：浮窗共用代码 `354c140`，tmux 版 `dd3a406`，`prefix g` 已换。
- 第 2 步做完：`1784e0b`。会话 id 和状态不再靠钩子，钩子的状态文件只当退路。
- 第 3 步基本做完，只剩发版那一下：
  - 不装 claude-tmux-sessions 也能用：`0ae3993`。
  - `init` / `serve` / `click`：`98fa00a`，状态栏圆点那排 tmux 刚起时也马上出来是 `2bb6391`。
  - CI 编 4 个平台：`7bf1e92`、`d40e1b5`。macos-14 已不支持，换成 macos-15。
  - `get.sh` 和安装说明：`0ade3b2`。
  - 验过的：独立的 tmux server（干净配置、单独状态目录、当成没装钩子和脚本）里 init 跑通；Ubuntu 24.04 容器（tmux 3.4）里装 CI 编的
    Linux 包，两步装好，3 秒内出圆点，浮窗列表对，tmux 关了常驻进程自己退。
  - 没验过：真的另一个 macOS 用户；get.sh 真下载，要等第一个 `tmux-v*` 标签发出去、合进 main 才有地址。
- **2026-11-09 发了 tmux-v0.1.0**：Release 8 个文件（4 个平台各一个包 + sha256），main `98dfc11..1e8ad8a`。
  用登录过的 gh 下回来核过 sha256、能跑。没登录的 curl 当天下 Release 文件全是 504：GitHub 当天有事故
  「Increased timeouts due to surge in anonymous users」，get.sh 加了重试（`32cb9b8`），main 已快进到 `9f8533f`。随后 `curl … get.sh | sh` 真下载装到临时目录，`statetrail-tmux 0.1.0` 能跑。
- 原来第 3 步要处理的三处 claude-tmux-sessions 脚本（已处理）：
  - `float-tool.sh`：Git、文件浮窗，并进 Rust。
  - `usage-refresh.py`：查实时额度，改用 core 里 herdr 版那套 Rust 查法。
  - `switch_account.py`：换账号，不进第一版。脚本在就显示，不在就不显示。

## 推广

发之前，每条发出去的文字都先给你看原文。

- **英文：**
  - Reddit 的 r/ClaudeAI 和 r/tmux：动图加两行安装加一句话。
  - Hacker News 的 Show HN。
  - 给 awesome-claude-code 这类合集列表提 PR，把它加进去。
  - herdr 的 Discord 和讨论区：说明 tmux 版也有了。
- **中文：** V2EX、知乎。旧稿在 claude-tmux-sessions 的 `docs/promo/`，改成新产品的说法。
- **一句话卖点（草稿）：** *See every Claude Code session in tmux at a glance — who's waiting, who's done — and jump to any turn of any session.*

## 定下来的（2026-11-09 用户：「按推荐来」）

1. **放哪个仓库、叫什么。** 定：放 statetrail 仓库、叫 statetrail，herdr 插件市场上已经是这个名字，"state + trail"正好是圆点加轨迹。
   另一个办法是放 claude-tmux-sessions：名字已经有 1 star 和一篇文章，但仓库里一大半是 Python 自用工具，要大清理。
2. **第一版要不要换账号。** 定：不要：多账号的人少，它又依赖 Python 脚本。
3. **rust 分支什么时候合进 main。** 定：第 3 步做完、打第一个 tmux 版标签时，一起合进 main（main 只放发布）。
