#!/usr/bin/env python3
"""README 里的三张截图：宽版轨迹、窄版轨迹、跳转浮窗。和 gif.py 同一套隔离装置（假 $HOME、私有 tmux、假 Claude），
另外在几个工作区里各开几个假 Claude、报不同的状态，浮窗里才有东西可列。

    cargo build --release && cp target/release/statetrail bin/.new && mv bin/.new bin/statetrail
    python3 demo/shots.py          → docs/wide.png、docs/narrow.png、docs/goto.png

诚实边界同 gif.py：Claude 和数据是假的，轨迹、章节显示、宽窄切换、浮窗都是真插件画的。章名是 fixture 预先写好的
（隔离环境里没有登录，调不了 Haiku），用的是轨迹自己的章节文件格式。
"""
import json
import subprocess
import sys
import time
from pathlib import Path

DEMO = Path(__file__).resolve().parent
src = (DEMO / "gif.py").read_text()
assert src.rstrip().endswith("\nmain()")
g = {"__file__": str(DEMO / "gif.py"), "__name__": "gif"}
exec(compile(src.rstrip()[: -len("main()")], str(DEMO / "gif.py"), "exec"), g)
sh, herdr, screen, find, click, trail_x = (g[k] for k in ("sh", "herdr", "screen", "find", "click", "trail_x"))
TMUX, ENV, HOME, WORK, W, H, HERDR, PLUGIN = (g[k] for k in ("TMUX", "ENV", "HOME", "WORK", "W", "H", "HERDR", "PLUGIN"))
OUT = PLUGIN / "docs"


def shot(name):
    ansi = WORK / f"{name}.ansi"
    ansi.write_text(sh(*TMUX, "capture-pane", "-e", "-p", "-t", "demo"))
    png = OUT / f"{name}.png"
    subprocess.run(["bash", str(DEMO / "render.sh"), str(ansi), str(png), "14"], check=True, stdout=subprocess.DEVNULL)
    print(png)


def result(out):
    return json.loads(out)["result"]


def others():
    """别的 Claude：同一个工作区的第一个用工作区自带的窗格，后面的各开一个标签页。都不抢焦点。"""
    ws = {}
    for line in (HOME / "others.tsv").read_text().splitlines():
        sid, path, label, cwd, title, state = line.split("\t")
        if label not in ws:
            r = result(herdr("workspace", "create", "--label", label, "--cwd", cwd, "--no-focus"))
            ws[label] = r["workspace"]["workspace_id"]
            pane = r["root_pane"]["pane_id"]
        else:
            r = result(herdr("tab", "create", "--workspace", ws[label], "--label", title, "--cwd", cwd, "--no-focus"))
            pane = r["root_pane"]["pane_id"]
        herdr("pane", "run", pane, f"clear; exec python3 {DEMO / 'fake_claude.py'} {path} {sid} '{title}' {state}")


def main():
    if sh("defaults", "read", "com.google.Chrome", "GenAILocalFoundationalModelSettings").strip() != "1":
        raise SystemExit("先设 Chrome 策略：defaults write com.google.Chrome GenAILocalFoundationalModelSettings -int 1")
    import shutil
    sh(*TMUX, "kill-server")
    if (HOME / ".config" / "herdr" / "herdr.sock").exists():
        herdr("server", "stop")
        time.sleep(1)
    shutil.rmtree(HOME, ignore_errors=True)
    shutil.rmtree(WORK, ignore_errors=True)
    HOME.mkdir(parents=True)
    WORK.mkdir(parents=True)
    sid = sh("env", "-i", *(f"{k}={v}" for k, v in ENV.items()), sys.executable, "-I",
             str(DEMO / "fixture.py"), str(HOME), str(PLUGIN)).strip()
    transcript = next((HOME / ".claude" / "projects").glob(f"*/{sid}.jsonl"))
    work = HOME / "work" / "photo-tidy"
    herdr("plugin", "link", str(PLUGIN))
    try:
        sh(*TMUX, "new-session", "-d", "-s", "demo", "-x", str(W), "-y", str(H), "-c", str(work),
           "env -i " + " ".join(f"{k}={v}" for k, v in ENV.items()) + f" {HERDR}")
        time.sleep(4)
        for key in ("Enter", "Escape"):       # 欢迎框、integrations 设置（见 gif.py）
            sh(*TMUX, "send-keys", "-t", "demo", key)
            time.sleep(1)
        others()
        herdr("pane", "run", "w1:p1", f"clear; exec python3 {DEMO / 'fake_claude.py'} {transcript} {sid}")
        for _ in range(40):                     # 等轨迹自己打开
            time.sleep(0.5)
            if any(p.get("label") == "statetrail" and p["workspace_id"] == "w1"
                   for p in result(herdr("pane", "list"))["panes"]):
                break
        time.sleep(3)
        tx = trail_x()

        # 1. 窄版：Rust 版默认就以窄版打开
        shot("narrow")
        # 2. 宽版：点 « 拉宽，再点第 3 轮，Claude 滚到那一轮（它在第一章里，第一章收着就先点章名展开）
        click(*find("«", right_of=tx))
        time.sleep(2.5)
        tx = trail_x()
        try:
            x, y = find("Can it skip", right_of=tx)
        except SystemExit:
            click(*find("Build the CLI", right_of=tx))
            time.sleep(1.5)
            x, y = find("Can it skip", right_of=tx)
        click(x, y)
        time.sleep(2)
        shot("wide")
        # 3. 跳转浮窗
        herdr("plugin", "action", "invoke", "statetrail.goto")
        time.sleep(3)
        shot("goto")
    finally:
        if "--keep" not in sys.argv:
            sh(*TMUX, "kill-server")
            herdr("server", "stop")
            time.sleep(1)
            shutil.rmtree(HOME, ignore_errors=True)


main()
