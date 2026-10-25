# Releasing statetrail / 发版清单

Every release goes through these steps in order. A step that fails stops the release: fix it first.
每次发版按顺序走完这几步。有一步不过就不发，先修。

## 1. Resource check / 资源检查

Resource use is the first thing to protect: CPU, memory and network, on the user's own machine.
资源占用是第一位的：CPU、内存、网络，在用户自己的机器上量。

With the new version running in the trails you actually use (the plugin restarts them in place after an update),
let them settle for a minute, then:
新版本在你平时用的轨迹里跑起来以后（插件更新后它们会原地重启），等一分钟让它们稳定下来，然后：

```sh
python3 herdr/tools/resource-check.py 60
```

It measures every open trail (the `trail.py pane` process and its fzf) for 60 seconds and exits non-zero when
something is over budget. Paste its output into the release commit message.
它把每个开着的轨迹（`trail.py pane` 进程和它的 fzf）量 60 秒，超出预算就以非 0 退出。把输出贴进发版的提交说明。

| What / 项 | Budget / 预算 | How it is measured / 怎么量 |
|---|---|---|
| CPU per trail, Claude idle / 每个轨迹，Claude 空闲 | ≤ 0.5 % of one core | CPU time delta ÷ duration (`ps -o time`) |
| CPU per trail, Claude working / Claude 干活时 | ≤ 2 % | same |
| Memory per trail, python + fzf / 每个轨迹的内存 | ≤ 60 MB RSS, must not keep growing with the session | peak of 5 s samples (`ps -o rss`) |
| Network while idle / 空闲时网络 | 0 bytes | per-process byte counters (`nettop -P`) |
| Live quota queries / 额度实时查询 | ≤ 1 per account per 30 min, for all trails together | `fetch.log`, one line per request actually sent |

Notes / 说明:
- The quota throttle is shared: one state file and one lock per account (`core/claude.py` `refresh_usage`), so N
  trails on the same account still send one request per interval. Clicking ⟳ forces one (at most every 20 s), and
  the check counts those too.
  额度查询的节流是所有轨迹共用的：每个账号一份状态文件、一把锁，开 N 个轨迹也只发一次。点 ⟳ 会强制查一次（最多每 20 秒一次），检查也会数进去。
- Known edge: a transcript of several hundred MB settles at about 55-62 MB for the python process alone (CPython
  keeps the memory pool it used while parsing; the live summaries are under 1 MB). Ordinary sessions are well under
  budget. Parsing the first read in a short-lived child process would remove it.
  已知的边界情况：几百 MB 的对话记录，python 进程本身会停在 55～62 MB（CPython 留着解析时用过的内存池，真正还活着的摘要不到 1 MB）。普通会话远在预算内。把首次读取放到一个临时子进程里解析可以去掉这部分。

## 2. Behaviour / 行为不变

- Render baselines: the 32 header / row renderings must be byte-identical unless the change is meant to alter them
  (then the diff is part of the review).
  渲染基准：32 份表头和列表的渲染逐字节相同，除非这次就是要改它们（那样差异要一起审）。
- In the isolated demo herdr (`python3 herdr/demo/gif.py --stage`), click through what changed, and at least: a
  jump, the narrow / wide switch, each bottom button open and close.
  在隔离的演示 herdr 里把这次改动的地方真点一遍，至少再点：一次跳转、宽窄切换、每个底部按钮的开和关。

## 3. Version, push, tag / 版本号、推送、标签

1. Set `version` in `herdr/herdr-plugin.toml`, in a commit of its own. / 改 `herdr/herdr-plugin.toml` 的 `version`，单独一个提交。
2. `git fetch`; push `main` only if it is not behind; never force. / 先 fetch，不落后才推 `main`，不 force。
3. `git tag -a vX.Y.Z -m ...` and push the tag. / 打标签并推送。
4. Re-record `herdr/docs/demo.gif` when the look changed (`python3 herdr/demo/gif.py`). / 界面变了就重录 GIF。

## 4. Fresh install / 全新安装

In an empty `$HOME` with `env -i` (carry `https_proxy` / `http_proxy` / `all_proxy` over if the machine uses a
proxy), run `herdr plugin install --yes siyuanseever/statetrail/herdr`; confirm `herdr plugin list` shows the new
commit and the manifest says the new version. Delete the temporary `$HOME` afterwards.
在一个空的 `$HOME` 里用 `env -i`（机器走代理的话把 `https_proxy` / `http_proxy` / `all_proxy` 带进去）跑
`herdr plugin install --yes siyuanseever/statetrail/herdr`，确认 `herdr plugin list` 里是新的提交、manifest 里是新版本号。测完删掉临时 `$HOME`。
