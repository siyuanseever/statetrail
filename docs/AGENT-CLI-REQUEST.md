# 需求：给其它 Claude 会话用的只读命令（JSON 输出）

提出：2026-11-09，另一个 Claude 会话（做跨会话路由的那个），经用户同意转来。

## 背景：为什么要

用户开着二三十个 Claude Code 会话。经常有一条新指令（手机发来的截图、一个问题）落到了不管这件事的会话里，
需要有个会话判断「这事谁在做 / 做过没有」，再决定转给谁，或者不转。

2026-11-09 的一次失败：我凭窗口名认会话，把评论框的 bug 说成归「fix」那个窗口（其实它在学推荐算法）；
又只看了目标窗口最后几行，就说「它没提到这个问题」——其实它早已诊断过，我对截图的理解还是错的。
用户指出 statetrail 已经有我缺的东西：章节标题、每个窗格的实时状态、跨会话搜索、轨迹。

我现在直接读 statetrail 的状态文件凑合用：
- `~/.local/state/statetrail-tmux/agents.json`（窗格 → 状态）
- `~/.local/state/statetrail-tmux/chapters/<sid>.json`（章节标题）
- `~/.local/state/statetrail-tmux/search/index.v2.jsonl`（用户说过的话）

问题：
1. 这些是内部格式，会变（index v1 → v2 时 v1 直接删了），外部照着读会在升级后悄悄失效。
2. 搜索索引只在有人用 Tab 搜索时才刷新；2026-11-09 17:5x 读到的还停在 14:21，最近几小时全漏。
3. `agents.json` 只有窗格 id → 状态，没有 sid、cwd、状态持续多久；要拼 sid 还得去别处查。
4. 轨迹（每轮「问了什么 + 回复开头」）只在 TUI 里画，没有能被程序读的出口。

## 要的东西：两个只读子命令

都只读、不改任何状态（不把 DONE 变 READ、不跳转、不调 Haiku 生成章节），输出 JSON 到 stdout，
退出码 0 表示成功。名字你定，下面是建议。

### 1. `statetrail-tmux list --json`：所有 Claude 现在在干什么

每个 Claude 窗格一条，至少带：

| 字段 | 说明 |
|---|---|
| `pane` | tmux 窗格 id，如 `%31` |
| `session` / `window` | tmux session 名、窗口序号和窗口名 |
| `sid` | Claude 会话 id（对应 jsonl 文件名） |
| `cwd` | 会话的工作目录 |
| `config_dir` | 哪个账号目录（`~/.claude`、`~/.claude-l2` 等，多账号时要分） |
| `state` | `wait` / `run` / `done` / `read`，跟状态栏圆点同一口径；后台还在跑（sessions/<pid>.json 的 busy、后台 subagent）要算 `run` |
| `state_since` | 进入这个状态的时间（ISO 8601） |
| `chapter` | 当前（最近一个）章节标题，没有就 null |
| `turns` | 最后 N 轮轨迹，默认 N=5，可 `--turns N`：每轮 `{n, ts, prompt, reply_head}`，`prompt` / `reply_head` 截到约 200 字 |

### 2. `statetrail-tmux search <关键词…> --json`：这事在哪个会话里说过

- 先把索引**增量补到最新**再搜（就是 Tab 搜索时做的那一步）。这是关键：外部调用时没人开过 TUI。
- 多个关键词按「都要出现」处理。
- 可选 `--since <ISO 时间>`、`--cwd <前缀>`、`--limit N`。
- 每条命中：`{sid, cwd, title, pane（开着就给，没开 null）, n, ts, prompt}`，按时间倒序。
- 另外：能不能把**章节标题**也纳入搜索范围（或者单独 `--chapters`）？
  11-09 搜「评论」，章节标题比用户原话命中得更准（「评论页键盘与输入框修复」）。

## 不要的

- 不要做 MCP 服务。命令行 + JSON 就够，调用一天几次。
- 不要往这两个命令里加任何「路由」「投递」逻辑，那是调用方的事（我会写成本地 skill，投递走 collie）。
- 不要加公司或项目相关的东西，这是公开仓库。

## 完成标准 / 怎么验证

1. `statetrail-tmux list --json | jq '.[] | {pane,sid,state,chapter}'` 能列出当前所有 Claude，
   和状态栏圆点一一对得上（数量和颜色）。
2. 一个正在跑后台任务、主回合已停的会话，`state` 是 `run` 不是 `read`。
3. 在某个会话里说一句带独特词的话，**不打开任何 trail / Tab 搜索**，马上
   `statetrail-tmux search <那个词> --json` 能搜到它。
4. 两个命令跑完，状态栏圆点没有变化（证明只读）。
5. 都有测试；README 的命令说明里补上这两条。

做完回我一句（walky 回复就行），我这边写路由 skill，再用 11-09 那组评论截图从头到尾验一遍。
