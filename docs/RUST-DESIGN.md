# statetrail Rust 版设计（v0.2）

写于 2026-10-25。Python 版 v0.1.4（`6d2dabe`）发布后冻结，以后只修 bug。Rust 版功能和它对齐、资源检查达标之后，再替换它。

## 结论

- 一个 Rust 程序（暂名 `statetrail`），自己画界面（ratatui + crossterm），不再用 fzf 和 Python。
- 子命令和现在的 `trail.py` 一一对应：`pane` / `toggle` / `auto` / `menu` / `pickrepo`。manifest 的写法基本不变，只是把 `python3 trail.py` 换成这个程序。
- 用户安装时不需要装 Rust：安装时的构建步骤会下载 GitHub Actions 预先编译好的程序；下载不到、而本机又有 cargo 时，才在本机编译。
- 资源目标：每个轨迹 RSS ≤ 10 MB，空闲时 CPU 约等于 0，空闲时网络 0。仍用 `docs/RELEASING.md` 和 `herdr/tools/resource-check.py` 检查。
- 分 6 步做，每一步都能在你本机单独试用（link 一个开发目录），不影响现在装着的 Python 版。

## 目录

```
statetrail/
  herdr/        Python 版（冻结）。替换那一步之前，安装命令一直指向这里
  herdr-rs/     Rust 版的插件目录：herdr-plugin.toml、install.sh（构建步骤）、Cargo 工程
    src/
  docs/
```

开发期间你可以 `herdr plugin link ~/projects/statetrail/herdr-rs` 来试，想退回就再 link 回 `herdr/`。
功能对齐、资源达标以后：Python 版挪到 `legacy/herdr-py/`（v0.1.4 标签里也留着），`herdr-rs/` 改名成 `herdr/`。这样大家的安装命令
`herdr plugin install siyuanseever/statetrail/herdr` 不用改。

## 模块划分，以及和 Python 版的对应

| Rust 模块 | 做什么 | 对应 Python 版 |
|---|---|---|
| `main.rs` | 子命令分发；`pane` 的主循环（事件、按键、鼠标、定时） | `trail.py` 的 `COMMANDS`、`Sidebar.run` |
| `herdr.rs` | herdr socket 客户端（一请求一连接的 JSON）、事件订阅 | `call`、`subscribe` |
| `transcript.rs` | 对话记录增量逐行读，每行只取需要的几个字段（serde 跳过其余部分），每轮只留摘要 | `core/transcript.py` |
| `claude.rs` | 配置目录、账号、上下文和模型、额度三层来源（实时 / statusLine 桥接 / `.claude.json` 缓存）、所有轨迹共用的查询节流（文件锁 + 时间戳）、`fetch.log` | `core/claude.py` |
| `jump.rs` | 让 Claude 滚到某一轮的那套算法（读屏、PageUp / PageDown、找回复开头） | `core/jump.py` |
| `ui/` | 表头（按钮、柱子、模型账号）、列表（自己记滚动位置，▲ / ▼ 翻页、点击、悬停）、底部（自己的按钮、插件图标、⋯）；宽版 / 窄版 | `core/render.py`、`Sidebar.header/rows/footer`、fzf |
| `tools.rs` | 找目录、找 git 仓库、找程序，列出其他 herdr 插件、绑 action | `live_dir`、`git_root`、`find_repos`、`load_plugins` |
| `picker.rs` | Git 选仓库和 ⋯ 弹窗：自己画的扁平列表，不再起 fzf | `cmd_pickrepo`、`cmd_menu` |
| `pane.rs` 里的底部 | 底部按钮开工具窗格（Claude 下面，高度按按钮）、点外面就关、开新标签页的插件跳回原页 | `run_tool`、`run_plugin`、`footer` |
| `theme.rs`、`i18n.rs` | herdr 的 18 套调色板、中英文（`themes.json`、`en.tsv` 编进程序里） | `theme.py`、`core/i18n.py` |
| `update.rs` | 发现插件被更新（看 `plugins.json`）就在原地换成新程序 | `code_sig`、`code_changed` |

和 Python 版相比，有三处做法会变：

1. **不再读自己的屏幕。** 列表滚到哪、露出来几轮，都是程序自己记着的，▲ / ▼ 的数字不再要「等 fzf 画完再读屏」。
2. **没有 fzf 子进程**，每个轨迹只剩一个进程。Git 选仓库和 ⋯ 弹窗也改成程序自己画。
3. **对话记录只解析需要的字段**：不再把每一行整个变成对象，内存天然就小，不用 v0.1.4 那个子进程的办法。

## 系统资源：不放进轨迹（2026-10-25 改）

原计划在每个轨迹底部加一行 CPU、GPU、内存、磁盘、网络。用户改了方案：

- 不放进轨迹：开 5 个轨迹，同样的数字会重复显示 5 遍。
- 不做「每个 Claude 占多少内存」。用户原话：「这个就别写了」。
- 实时总览放在 herdr 顶部标签栏右侧的状态区（herdr 配置 `[ui] tab_bar_right`，每 5 秒跑一次命令）。
  用户原话：「它就跟我们的 statetrail 没有关系了，对吗？它是一个 herdr 的配置。」
  所以由 dotfiles 的 `herdr/status.sh` 来做，不在本仓库里。
- 本仓库只做一件事：底部第 4 个默认按钮「监视器」，从 Claude 下面打开 btop（高度 0.9），再点一次关掉，在 btop 里按 q 也能退出。
  btop 1.4.6 的默认配置就能只用鼠标结束进程：点进程那一行 → 点 terminate → 点 Yes。所以不另带 btop 配置。
  这个按钮跟第 4 步一起做了（`5fabd10`）。

## 分发：用户不用装 Rust

```
herdr plugin install siyuanseever/statetrail/herdr
  └─ [[build]] bash install.sh   （工作目录 = 这次的安装目录）
       1. 从 herdr-plugin.toml 读版本号，按 uname 选平台
       2. 下载 github.com/siyuanseever/statetrail/releases/download/vX.Y.Z/statetrail-<平台>.tar.gz 和它的 .sha256
       3. 校验通过，解压到 bin/statetrail
       4. 下载不了：本机有 cargo 就 cargo build --release；都不行就报错退出，并写明怎么装
```

- 平台：`aarch64-apple-darwin`、`x86_64-apple-darwin`、`x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`。
- GitHub Actions：推一个 `v*` 标签就触发。四个平台各编一份（macOS 用 macos-14 和 macos-13 两种 runner，Linux 用 ubuntu-24.04 和 ubuntu-24.04-arm），
  打包，算 sha256，上传到这个标签的 Release 里。
- **发版顺序要改**：`plugin install` 装的是默认分支 main 的最新提交，不是标签。所以先在发版分支上打标签，等 Actions 把四个平台的程序传上
  Release，再把 main 快进过去。不然在这几分钟里安装的人，会下载不到程序。这一条会写进 `docs/RELEASING.md`。
- 下载要走用户的代理（`https_proxy` 这些环境变量）：herdr 执行构建步骤时会不会带上这些变量，要实测。
- 运行时联网的只有额度实时查询（Rust 里用 `ureq` 发 HTTPS 请求，同样要遵守代理变量），和 Python 版一样每个账号最多 30 分钟一次，所有轨迹共用。

## 分 6 步做

| 步 | 做完能看到什么 | 怎么验 |
|---|---|---|
| 1 | 骨架：`herdr-rs` 能 link、能自动打开，列表显示每一轮，点一下能跳转 | 和 Python 版对同一个会话切出的轮次逐条相同；内存、CPU 先量一次 |
| 2 | 表头：按钮、ctx / 5h / 7d 柱子、模型账号、额度三层来源；宽版 / 窄版；主题、中英文 | 32 份基准改成比较去掉颜色后的文字，和 Python 版一致 |
| 3 | 交互：▲ / ▼ 翻页、宽窄切换、tab 命名、Claude 退出就关、更新后原地重启 | 演示 herdr 里逐项真点 |
| 4 | 底部：Git / 文件 / 命令行（下面打开，按钮自己的高度）、Git 选仓库、插件图标、⋯、跳回已开的标签页 | 照 v0.1.3、v0.1.4 的验证清单再跑一遍 |
| 5 | btop 按钮（系统资源改由 herdr 顶部状态区显示，见上文） | 只用鼠标在 btop 里结束一个进程；资源检查仍然达标 |
| 6 | 分发：install.sh、GitHub Actions、发版顺序；替换 `herdr/` | 全新环境从 GitHub 安装，不装 Rust 也能用 |

每一步都在单独的 worktree 里做，做完先 link 到你本机试，资源检查达标才算这一步完成。第 6 步之前，不动大家正在装的 Python 版。

## 本机从 tmux 迁到 herdr 之后补的（2026-10-26）

来自迁移会话留下的交接（`docs/HANDOFF-2026-10-26-herdr.md`，已并进这里、删掉）。本机现在装的就是这个目录：
`herdr plugin link ~/projects/statetrail/herdr-rs`。link 的目录 `herdr plugin update` 管不到，有新提交要本地
`cargo build --release`，再按 manifest 注释用 `bin/.new` + `mv` 换掉程序；开着的轨迹会自己原地重启。

| 问题 | 怎么处理的 | 提交 |
|---|---|---|
| herdr 的 Claude 集成只在 SessionStart 报会话 id，之前就开着的会话轨迹显示「找不到会话」 | `herdr::session_id` 没有 `agent_session` 就读 `<配置目录>/sessions/<pid>.json` 的 `sessionId` | `bf4f9e2` |
| 会话 id 后报、对话记录后出现时没有事件，轨迹不刷新 | 没有对话记录时每 5 秒（`AGENT_CHECK`）重查一次 | `bf4f9e2` |
| Claude 干活时前台进程可能是它的子进程，`claude_pid` 返回空，l2 的会话被当成 l | 前台里没有就从 shell 的直接子进程里找（`ps` 的 ucomm 是版本号） | `bf4f9e2` |
| herdr 服务的 PATH 里 python3 是 3.9.6 | Rust 版不调 python3，不受影响 | — |
| herdr 里没有 tmux 版写的 effort 窗格选项 | 轨迹从对话记录读 effort，不受影响；usage-monitor 会话页的 effort 仍是空的 | — |
| 换账号后会话 id 不变，表头还显示旧账号 | Claude 在窗格里退出又起来、或收到 `rescan` 命令时，按新进程重认配置目录 | `c7c5ddf` |

### 换账号：`statetrail switch`（`src/switch.rs`）

用户要求把换账号搬进 Rust 版。从 `claude-tmux-sessions/bin/switch_account.py` 搬过来，账号配置（`~/.config/claude-tmux/accounts.json`）、
安全闸和提示文字都一样，只是读屏、发键改用 herdr：

```
statetrail switch --to <账号> [--dry-run] [--stop-bg] [--json] <窗格>...
  /exit → 等前台回到 shell → env -u ... CLAUDE_CONFIG_DIR=<目录> claude <保留的启动选项> --resume <同一个会话 id>
  → 等新进程的 sessions/<pid>.json 出现在目标账号目录、会话 id 没变 → 叫旁边的轨迹 rescan
```

- 安全闸：只动空闲的（herdr 状态和屏幕都要是空闲）；输入框有草稿不动（herdr 读出来真敲的字不带颜色，Claude 的灰色建议是 SGR 2，不算草稿）；
  挂着后台 / 定时任务不动，`--stop-bg` 时在退出确认框里选「Exit and stop tasks」；队友、后台会话不动；已是目标账号的跳过；当前窗格不动。
- 带队友的领队：先往它输入框里打一句收队，等同一标签页里带 `--agent-id` 的窗格都退出再切。这条路照搬了，本机 herdr 里还没有 agent team，没实测过。
- usage-monitor 会话页的「切到 l2」按钮：`usage-monitor/switch_account.py` 对 herdr 窗格调 `statetrail switch --json`（从 herdr 的 `plugins.json`
  找程序），tmux 窗格照旧走 claude-tmux-sessions（usage-monitor `cabfd50`）。
- 轨迹表头的下拉菜单（`1676fae`，用户 2026-10-26 要的）：点模型名、「/high」、账号名（后面跟着 ⇄），在那段字正下方弹出模型 / effort（带 ultracode
  开关）/ 账号的列表。菜单由轨迹自己画：herdr 的 popup 没有位置参数，只能开在**当前看着的那个标签页**正中，不管是谁叫开的。
  模型和 effort 走 `/model` 选择器按 `s`（只改这个会话）：带参数的 `/model sonnet`、`/effort low` 会顺手写进 `settings.json` 当新会话默认，
  数字键也会；对话已缓存时还会问「Switch model?」，自动回车确认。换账号会按原启动参数重起，只改这个会话的模型 / effort 随之失效。
  能点的那几段字平时带下划线，鼠标停上去反色；菜单项停上去整行反色（crossterm 的鼠标捕获本来就开了移动事件，按「停在哪一段」去重，不是每动一下都重画）。
  用 SGR 移动序列模拟测过；herdr 会不会把真鼠标的移动转给窗格要真机看，不转也还有下划线。
- 压缩后表头没有模型（2026-10-26 修）：读用量时碰到压缩分界线就返回了，分界线之后 Claude 还没回过话时模型 / effort 是空的。
  现在用量取分界线上的 `postTokens`，模型 / effort 继续往前找。

### 底部按钮的工具没装（`41f5dd4`）

以前点了只在表头闪 8 秒提示，用户以为没反应。现在照常在 Claude 下面开工具窗格，里面写「找不到 btop　$ brew install btop　按回车安装」，
装完直接打开工具；没有 brew 或者是自定义命令就只说找不到。实测时 `brew uninstall btop` 顺带 autoremove 了无关的 p7zip（已装回）：
以后要卸东西做测试，先 `brew autoremove --dry-run` 看一眼，或者设 `HOMEBREW_NO_AUTOREMOVE=1`。

迁移时踩过的坑：`claude --resume <id>` 打开的如果是后台会话（`--bg`，或退出时选了留在后台），只是挂上去看，杀掉后台进程这边也跟着退出；
同一个会话不要在两处同时开。

## 待官方，这次不做

- 在 Claude 输出里 Ctrl+点击文件路径直接打开。herdr 的 `[[link_handlers]]` 只接 OSC 8 链接，Claude Code 在 herdr 里不输出 OSC 8；作者给的临时办法
  `FORCE_HYPERLINK=1` 你不接受（herdr#4748），herdr 也不认纯文本路径（#5009，已关）。等 Claude Code 把 herdr 加进它的终端名单，或者 herdr#5457 合并以后再做。

## 风险

- herdr 的 socket 协议还在变（0.9.x）：程序启动时检查 herdr 版本，不认识就在窗格里明说，不要悄悄出错。
- 鼠标：Python 版的点击由 fzf 处理，Rust 版要自己解析 herdr 转来的 SGR 鼠标序列；演示环境偶尔整段收不到鼠标的那个现象，到时候顺便再查一次。
- 预编译程序要不要签名：macOS 从网上下载的程序会被加上隔离属性（quarantine）；我们是用 curl 下载、在终端里运行，一般不会被拦，第 6 步实测确认。
