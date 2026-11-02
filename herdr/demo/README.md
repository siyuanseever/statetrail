# 截图的生成装置

README 里的三张截图（`docs/wide.png`、`docs/narrow.png`、`docs/goto.png`）是这套脚本生成的，任何人都能复现出同样的图，
图不会随着代码演进悄悄失真，也不用把真实会话发出去。

```bash
cargo build --release && cp target/release/statetrail bin/.new && mv bin/.new bin/statetrail
python3 demo/shots.py            # → docs/ 下三张 PNG
python3 demo/shots.py --keep     # 截完不收，tmux -L herdrdemo attach -t demo 进去看
```

`gif.py` 是 Python 版录动图的脚本，分镜照的是旧界面；现在 `shots.py` 借用它的隔离装置（假 `$HOME`、私有 tmux、点击、渲染）。

| 文件 | 做什么 |
|---|---|
| `shots.py` | 搭隔离 herdr，另开三个工作区、六个假 Claude（在跑 / 等确认 / 做完 / 看过），截窄版、宽版（点第 3 轮）、跳转浮窗 |
| `gif.py` | 在私有 tmux server（`-L herdrdemo`）里用假 `$HOME`（`/private/tmp/herdrdemo`）起一个完全隔离的 herdr，按分镜点击、截帧、拼 GIF，最后全收掉 |
| `fixture.py` | 造假 `$HOME`：编出来的对话记录（主会话预先写好章名，隔离环境里调不了 Haiku）、别的 Claude 的清单、假账号 `you@example.com`、写死的额度读数、herdr 和插件的配置、一个小 git 仓库 |
| `fake_claude.py` | 假 Claude：把对话记录画成 Claude 全屏界面的样子，能翻页、到顶、到底、滚轮，向 herdr 报上自己是 claude |
| `render.sh` + `ansi2html.py` | ANSI → HTML → headless Chrome 截图（从 claude-tmux-sessions 的 `docs/demo/` 搬来） |

要的东西：herdr、tmux、python3、fzf、lazygit、ffmpeg、Google Chrome。起 Chrome 之前先确认它不会下载本地 AI 模型
（`defaults read com.google.Chrome GenAILocalFoundationalModelSettings` 要是 `1`）；Chrome 用固定的配置目录，不每次新建。

## 诚实边界

1. **Claude 是假的。** `fake_claude.py` 不调模型，只把一份编好的对话记录画出来；它的界面照 Claude Code 全屏模式的样子画
   （`❯` 你的话、`⏺` 回复、往上翻时底部的「Jump to bottom」），这正是 statetrail 认位置要用的特征，所以图上的跳转是真跳。
2. **数据是编的。** 对话、账号、额度百分比都是 fixture。额度读数的时间是「刚刚」，插件 30 分钟内不会去查，演示不碰任何真实账号。
3. **轨迹是真的。** 插件是 `herdr plugin link` 进隔离 herdr 的真代码：自动打开、列表、跳转、宽窄切换、底部按钮开的 lazygit
   浮窗都是它自己做的。点击是往 herdr 客户端发真实的鼠标序列。
4. **图标用 unicode 档、配色是渲染脚本的调色板加 herdr catppuccin 主题。** 栅格化走浏览器，不依赖 Nerd Font；
   默认配置（`icons = "nerd"`）下真实的图标长得不一样。

## 踩过的坑

- 新装的 herdr 先弹欢迎框，只认回车、Esc 关不掉，开着时**所有鼠标事件都被它吞了**；回车后再弹一页 integrations 设置，Esc 关。
- 假 Claude 报会话 id 要用 `--source herdr:claude`（herdr 自带 Claude 集成的来源），用自定义来源报，`agent_session` 一直是空的。
- 窗口要够宽（200 列）：轨迹要给 Claude 留 110 列，150 列时它会以窄版打开。
- lazygit 第一次运行的欢迎框会盖住浮窗：fixture 里预先写好它的 `state.yml`。
- 录制中途出错时 tmux 关了，但 herdr server 还在；下次开头先 `server stop` 它，再删假 `$HOME`。
- 「做完」状态要先报 working、隔几秒再报 idle（1.5 秒不行、4 秒行）；而且报会话 id 算换对话，herdr 会把状态重置成 idle。
  所以演示里做完的那个 Claude 不报会话 id（浮窗里它那行写目录、不写多久）。
- ⏸ ⏺ 这类符号浏览器默认用彩色 emoji 字体画，成了蓝色方块：`ansi2html.py` 给 U+23xx 加 U+FE0E 要文字样式。
