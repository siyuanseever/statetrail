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
| `tools.rs` | 底部按钮开工具窗格（Claude 下面，高度按按钮）、Git 选仓库（自己画的列表，不再起 fzf）、命令行 | `run_tool`、`cmd_pickrepo` |
| `plugins.rs` | 列出其他 herdr 插件、绑 action、开新标签页的插件跳回原页、⋯ 弹窗 | `load_plugins`、`run_plugin`、`cmd_menu` |
| `sysres.rs` | **新**：底部的系统资源一行（CPU、GPU、内存、磁盘、网络），点开看明细 | 取代被取消的 v0.1.5 |
| `theme.rs`、`i18n.rs` | herdr 的 18 套调色板、中英文（`themes.json`、`en.tsv` 编进程序里） | `theme.py`、`core/i18n.py` |
| `update.rs` | 发现插件被更新（看 `plugins.json`）就在原地换成新程序 | `code_sig`、`code_changed` |

和 Python 版相比，有三处做法会变：

1. **不再读自己的屏幕。** 列表滚到哪、露出来几轮，都是程序自己记着的，▲ / ▼ 的数字不再要「等 fzf 画完再读屏」。
2. **没有 fzf 子进程**，每个轨迹只剩一个进程。Git 选仓库和 ⋯ 弹窗也改成程序自己画。
3. **对话记录只解析需要的字段**：不再把每一行整个变成对象，内存天然就小，不用 v0.1.4 那个子进程的办法。

## 系统资源那一行（新功能）

- 显示：CPU、GPU、内存、磁盘、网络（上下行速率），平时灰色，高了才变色，规则和额度柱子一样；窄版放不下时按 网络 → 磁盘 → GPU 的顺序丢。
- 采样只由一个进程做：所有轨迹抢同一把文件锁，抢到的那个每 5 秒采一次，写进一份缓存文件，其他轨迹只读缓存。采样本身也算进资源预算。
- 数据来源（macOS，不要 sudo）：CPU 和内存用系统调用（内存口径和「活动监视器」的「已使用」一致，照搬 tmux 版
  `bin/tmux-sysstat.sh` 的算法）；GPU 用 `ioreg -r -d 1 -c IOAccelerator` 的 "Device Utilization %"；磁盘、网络用系统调用。
  每 5 秒起一次 `ioreg` 会不会太贵，要实测，贵就改成拉长间隔。
- 点某一项，就像 Git 一样从 Claude 下面开一个窗格看明细；明细程序从 tmux 版的 `bin/sys-monitor.py` 移植过来，复制进本仓库，不依赖 claude-tmux-sessions。
- 可以在配置里关掉这一行。

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
| 5 | 系统资源一行 | 开 5 个轨迹时，资源检查仍然达标 |
| 6 | 分发：install.sh、GitHub Actions、发版顺序；替换 `herdr/` | 全新环境从 GitHub 安装，不装 Rust 也能用 |

每一步都在单独的 worktree 里做，做完先 link 到你本机试，资源检查达标才算这一步完成。第 6 步之前，不动大家正在装的 Python 版。

## 待官方，这次不做

- 在 Claude 输出里 Ctrl+点击文件路径直接打开。herdr 的 `[[link_handlers]]` 只接 OSC 8 链接，Claude Code 在 herdr 里不输出 OSC 8；作者给的临时办法
  `FORCE_HYPERLINK=1` 你不接受（herdr#4748），herdr 也不认纯文本路径（#5009，已关）。等 Claude Code 把 herdr 加进它的终端名单，或者 herdr#5457 合并以后再做。

## 风险

- herdr 的 socket 协议还在变（0.9.x）：程序启动时检查 herdr 版本，不认识就在窗格里明说，不要悄悄出错。
- 鼠标：Python 版的点击由 fzf 处理，Rust 版要自己解析 herdr 转来的 SGR 鼠标序列；演示环境偶尔整段收不到鼠标的那个现象，到时候顺便再查一次。
- 预编译程序要不要签名：macOS 从网上下载的程序会被加上隔离属性（quarantine）；我们是用 curl 下载、在终端里运行，一般不会被拦，第 6 步实测确认。
