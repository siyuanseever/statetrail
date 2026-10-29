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
  herdr/        插件目录（2026-10-28 起是 Rust 版）：herdr-plugin.toml、install.sh（构建步骤）、Cargo 工程、
    src/        可选的 statusline.py 桥接
  docs/
  .github/workflows/release-rs.yml
```

**2026-10-28 已替换**（用户「我倾向于用 Rust 版直接替换 herdr/：可以」）：Python 版直接删掉，没有挪进 `legacy/herdr-py/`——
main 上多一份 `herdr-plugin.toml` 市场就会多列一个同名插件。Python 版要用就 `--ref v0.1.4`。`statusline.py` 留在 `herdr/`
（Rust 版照样读它写的额度）；`demo/stage.py` 借用 Python 版的 `demo/gif.py`，一起删了。下面「替换前」的文字保留作记录。

替换前：

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
| `picker.rs` | Git 选仓库：自己画的扁平列表，不再起 fzf（⋯ 后来改成轨迹里就地弹的下拉菜单，见下文） | `cmd_pickrepo`、`cmd_menu` |
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
- 曾经有过第 4 个默认按钮「监视器」，从 Claude 下面打开 btop（高度 0.9，`5fabd10`）。后来还自带了一份精简的 btop 布局（`4cf44c1`）：
  只显示 CPU、内存、进程三个框，磁盘只看系统盘。原因是默认布局把同一块盘的 7 个 APFS 分区都列出来，内存只剩 29 列宽。
- 2026-10-26 两样都去掉了。用户原话：「我们这么耦合 btop 是不是不太好呀？」插件要认得 btop 配置文件里的键，
  btop 改了格式，插件就得跟着改；这是个人对 btop 的偏好，不该写进插件。别的 herdr 插件也没有自带一排第三方工具按钮的。
  想要 btop 按钮的人自己在 `[[buttons]]` 里加，命令写 `btop` 或 `btop -c <自己的配置>`。
  下一步：用户想把监视器做进插件自己的程序里，做成可以关掉的配置项，范围还没定。

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
| 5 | ~~btop 按钮~~（做过，2026-10-26 去掉；系统资源由 herdr 顶部状态区显示，见上文） | — |
| 6 | 分发：install.sh、GitHub Actions、发版顺序；替换 `herdr/` | 全新环境从 GitHub 安装，不装 Rust 也能用 |

每一步都在单独的 worktree 里做，做完先 link 到你本机试，资源检查达标才算这一步完成。第 6 步之前，不动大家正在装的 Python 版。

**第 6 步进展（2026-10-27，用户「要不我们尝试着发一下？」）**

- 市场规则（herdr v0.9.5 文档 `marketplace.mdx`）：只收**默认分支（main）**上能解析的 `herdr-plugin.toml`，仓库要带 `herdr-plugin` 标签（已带），每 30 分钟刷新。所以 Rust 版上市场 = 合进 main；之前都可以在 `rust` 分支上做。`herdr plugin install` 支持 `--ref`，预发布可以 `herdr plugin install siyuanseever/statetrail/herdr-rs --ref v0.2.0-rc.1` 装。
- 已做（`835ec3a`，本地已提交）：`herdr-rs/install.sh`（清单 `[[build]]` 调它，按 version 从 Release 下包、校验 sha256，下不到就用 cargo 现编）；`.github/workflows/release-rs.yml`（mac arm64 / x86_64、linux musl x86_64 / arm64；推 `v0.2*` 起的标签发 Release，带「-」的是预发布；推 rust 分支只编不发）。现编那条路实测过，下载那条路要等第一个预发布。
- 接下来：推 rust 看 4 个平台编得过 → 清单 version 改 `0.2.0-rc.1`、打同名标签（发布动作，等用户点头）→ 本机 unlink 本地版、从 GitHub 装一遍试 → 合 main（等用户点头）。
- 合 main 时要定：main 上 `herdr/`（Python 版，同一个 id）和 `herdr-rs/` 两份清单市场会各列一行，都叫 statetrail。（已定：Rust 版替换 `herdr/`，`508acc7`）
- **v0.2.0-rc.1 已发（2026-10-28 11:44，用户「发」）**：标签指向 `aa6fbbc`，CI run 40628400331 编 4 个平台、建了预发布 Release（8 个文件：4 个 tar.gz + 4 个 sha256）。
  本机试装：unlink 本地版 → `herdr plugin install siyuanseever/statetrail/herdr --ref v0.2.0-rc.1 --yes` → 装上的 `bin/statetrail` 的 sha256
  （`c3babc4d…`）和 Release 里 aarch64-apple-darwin 包解出来的一样，插件目录里没有 `target/`（是下载的、不是现编的）；26 个轨迹都换成从
  安装目录起，表头正常。试完 `herdr plugin link` 回本地 `herdr/`。main 可以从 rust 快进（main 上没有 rust 没有的提交）。

## 本机从 tmux 迁到 herdr 之后补的（2026-10-26）

- 默认打开窄版（细条，19 列）：用户原话「我们用细条版本作为默认版本吧」。配置 `default_mode = "wide"` 才开宽版；Python 版不动，仍默认宽版。
- ⋯ 改成轨迹里就地弹的下拉菜单（用户原话「不要居中，就跟我们的切换账号和 model 差不多的弹窗」）：以前用 herdr 的 popup，只能开在屏幕正中，
  还把每个插件的所有动作都列出来。现在贴着底部按钮、靠右弹出，和模型 / 账号菜单同一套（`ui::draw_menu`，悬停垫底色）；
  只列底部没放下的插件，一个插件一行、只写名字，点了调它的默认动作（用户：「每一个插件只显示一个标题就可以了，其它功能不要展示了」）。
  manifest 里的 `menu` 窗格、`statetrail menu` 子命令、`plugrun` 回传都删了。只列有 `pane` 上下文动作的插件（和以前一样）。
- 窄版的 ▲（上面还有几轮）单独占表头最后一行，不再和 7d 额度挤一行（用户 2026-10-26 要的）。一屏放得下时没有这一行。
- 窄版表头 effort 单独一行（用户原话「effort 单独一行」）：以前是「Opus5.5 hi」挤一行、effort 只留两个字；现在模型名一行、下一行是 effort 全名，和模型名对齐，两段都还能点开下拉菜单。
- 窄版 effort 那一行自带图标（仪表盘，nf-fa-tachometer；unicode 档「◔」），不再用空格对齐模型名（用户原话「model 和 effort 能不能分别给一个 icon 呀，现在 effort 前面是空的，感觉不协调」）。
- 窄版账号写完整用户名（siyuan.l / siyuan.l2），不再只留和别的账号不一样的那截（用户原话「账号不应该显示用户名吗？l 是简称吧」）；放不下 15 列才退回那截。
- ~~effort 菜单里 ultracode 只放一行~~（`c027c8f`），当天就随下一条一起撤了。
- **2026-10-27 起模型、effort、ultracode 只显示，不能点**，表头里能点的只剩账号（连 ⇄）。用户原话「我切换模型咋没反应呢」「有反应之后他一直在滚动 conversation 历史」「如果我们做的体验不好就不要做可交互了」。原因：只改这个会话的模型，只有 Claude 的 /model 选择框能做到（带参数的 `/model sonnet` 会存成新会话默认，两个账号共用），轨迹只能替用户打 /model、按 ↑↓←→、按 s，屏幕跟着动；Claude 在跑时还按不了，窄版当时只画一个「!」，看着像没反应。`statetrail set` 命令行还留着（`switch::set_model` / `set_ultracode`），轨迹不再调。
- 窄版的提示（换账号的进展、失败原因）按宽度折到按钮下面、最多三行，不再只画一个「!」。
- 窄版 5h / 7d 百分比后面跟着还有多久重置（「 9%  2h41m」「36%  2d12h」，格式和宽版一样，用户原话「额度还剩多久更新」）；读不到重置时间就不写。
- **轨迹跟着 Claude 的屏幕选中**（2026-10-28，用户「根据我在 Claude Code 里查看的历史消息或者最新底部，来更新右侧轨迹的选中区域」「我们找一个不太消耗的方式，如果特别消耗我们就不做」）：
  焦点在这一对 Claude / 轨迹上（订阅的 `pane.focused` 事件，启动时看 `pane.get` 的 focused）时，每 0.5 秒 `pane.read` 一次 Claude 的屏幕；屏幕哈希没变就不认、不重画。
  没有「Jump to bottom」= 在最底 → 选中最新一轮；往上翻着 → `jump::located_rows` 找屏幕上最上面露出开头的那一轮，开头在下半屏就算上一轮；都没露出开头就不动。
  选中的那一轮不在列表可见范围就滚过去。点一轮替 Claude 翻屏的那 7 秒内不跟。实测：用户在 w2:p5 往上翻时 w2:pB 的选中从 173 跟到 174；
  26 个轨迹 20 秒共用 0.19 CPU 秒，跟着的那个 0.02（单核约 0.1%），其余每个 ≤0.01（和没这功能时一样，在处理焦点事件）。
- **轨迹分章**（2026-10-29，用户「不同的任务需求，起一个一级标题？对话轮次是二级的？」，选了推荐方案「Haiku 自动分 + 右键手动改」）：`src/chapters.rs`。
  - 为什么要模型：现成的信号都对不上。Claude 的 `ai-title` 一个会话只有一个；压缩边界只跟上下文满没满有关；按两句话的间隔切也不行，
    本会话实测「嗯，你继续吧」前面隔了 1036 分钟却是同一件事，「我有一些新需求」离上一句 3 分钟却是新任务。
  - 怎么调：`claude -p --model haiku --no-session-persistence --restricted --tools "" --strict-mcp-config --system-prompt … --json-schema …`，
    cwd 是状态目录下的 `chapters/`（不读项目 CLAUDE.md），非默认账号设 `CLAUDE_CONFIG_DIR`。`--bare` 不能用：它只认 API key，不读订阅登录。
    试过本会话 25 句：分出 13 章、标题读得懂，0.002 美元、16 秒，`~/.claude/projects` 下没多出会话文件。
  - 什么时候调：只有焦点在这一对上的轨迹调，没看的会话不花额度；一次最多 80 句，带前面 6 句和上一章章名当上下文；全机一把锁（`chapters/.lock`，
    120 秒算过期），20 多个轨迹不会同时起 claude；失败 10 分钟后再试。结果按轮次 id 存 `chapters/<会话 id>.json`（auto / upto / manual），手动标的盖过 auto。
  - 界面：章名一行（▾ 展开 / ▸ 收着，窄版收着时后面写轮数，宽版写「第 a–b 轮 · 时间」），轮次缩进。没点过的章只展开选中的那一轮所在的章，
    所以跟着 Claude 屏幕换选中时会自动换展开的章；收着的章里有选中的轮次时章名画成选中。分章时不再把压缩掉的轮次收成「1–N +」一行（那样一个章名都看不到），改成画暗。
    右键一轮「这里开新章」、右键章名「并回上一章」；菜单字不超过 11 列（细条 17 列，框占 6 列）。
  - 实测：本机 4 个会话自动分了（日志 `chapters.log` 每批 20～40 秒、找到 22～45 章）；在 w1:pA 用 SGR 鼠标序列测了右键开新章、点章名展开、并回上一章；
    27 个轨迹 20 秒共 0.12 CPU 秒、每个最多 7 MB，和之前一样。

来自迁移会话留下的交接（`docs/HANDOFF-2026-10-26-herdr.md`，已并进这里、删掉）。本机现在装的就是这个目录：
`herdr plugin link ~/projects/statetrail/herdr`（10-28 之前是 `herdr-rs`）。link 的目录 `herdr plugin update` 管不到，有新提交要本地
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
- 轨迹表头的下拉菜单（`1676fae`，用户 2026-10-26 要的）：点模型名、effort（表头里是「Opus 5.5 high」的 high，2026-10-26 去掉了斜线、用一格空分开）、账号名（后面跟着 ⇄），在那段字正下方弹出模型 / effort（带 ultracode
  开关）/ 账号的列表。菜单由轨迹自己画：herdr 的 popup 没有位置参数，只能开在**当前看着的那个标签页**正中，不管是谁叫开的。
  模型和 effort 走 `/model` 选择器按 `s`（只改这个会话）：带参数的 `/model sonnet`、`/effort low` 会顺手写进 `settings.json` 当新会话默认，
  数字键也会；对话已缓存时还会问「Switch model?」，自动回车确认。换账号会按原启动参数重起，只改这个会话的模型 / effort 随之失效。
  能点的那几段字平时垫一层主题选中色作底（像按钮；用户不要下划线），鼠标停上去换强调色底；菜单项停上去整行垫选中色（crossterm 的鼠标捕获本来就开了移动事件，按「停在哪一段」去重，不是每动一下都重画）。
  用 SGR 移动序列模拟测过；herdr 会不会把真鼠标的移动转给窗格要真机看，不转也还有底色。
- 压缩后表头没有模型（2026-10-26 修）：读用量时碰到压缩分界线就返回了，分界线之后 Claude 还没回过话时模型 / effort 是空的。
  现在用量取分界线上的 `postTokens`，模型 / effort 继续往前找。

### 关「命令行」时 Claude 闪一下（2026-10-26 修）

用户的观察：点轨迹的「命令行」打开时 Claude 那一格显示的是**上半截**，herdr 自带的 `prefix + -` 显示的是最下面（输入框）；
关的时候才一下子整屏重画。实测（每毫秒读一次 Claude 窗格）：`plugin.pane.open` 切出来以后布局马上是 28 行，
但 Claude 的终端还是 55 行，过 1.3 秒、甚至 3 秒都不变；`pane.split` 切出来约 80 毫秒就变成 27 行。
lazygit / yazi（当时还有 btop）的高度是 0.9，开完要挪线，挪线会让 herdr 重算尺寸，所以没事；「命令行」没写高度，
默认 0.5 正好不挪。现在 0.5 时也把线往上挪 0.01 再挪回来：点完约 0.1 秒 Claude 就是 27 行、露出输入框，关的时候和自带分屏一样。

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
