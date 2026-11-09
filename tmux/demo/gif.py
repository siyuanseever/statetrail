#!/usr/bin/env python3
"""tmux/docs/demo.gif — statetrail for tmux 的样子，十几秒：真程序、真 tmux、假 Claude、假数据。

    python3 tmux/demo/gif.py            → tmux/docs/demo.gif（各帧在 $TMPDIR/statetrail-tmux-gif/）
    python3 tmux/demo/gif.py --stage    只搭好不录，留着手动看：tmux -L sttouter attach

怎么做的：两个私有 tmux server。里面那个（-L sttdemo）用假 $HOME（/private/tmp/sttdemo）、空配置加一行
`run-shell 'statetrail-tmux init'`，和用户装好的样子一样；四个 session 里跑 fake_claude.py。外面那个（-L sttouter）
的一格 attach 到里面，抓这一格就是整屏（状态栏、浮窗都在）。点击是往外面那一格发真实的鼠标序列（SGR），
里面的 tmux 当成真点击处理；prefix g 也是真按键。ANSI → PNG（herdr/demo/render.sh：headless Chrome）→ GIF（ffmpeg）。

分镜：
  1. photo-tidy 里的 Claude，右边是轨迹（章名、轮次），底部状态栏每个 Claude 一个圆点
  2. api 里一个 Claude 弹出权限框：它的圆点变红
  3. 点那个红点：跳到它
  4. prefix g：跳转浮窗，所有 Claude 按 session 分组
  5. 点状态栏的 photo-tidy：回来
  6. 点轨迹里的第 3 轮：Claude 滚到那一轮的回复

诚实边界：Claude 是假的（不调模型）；对话、账号、额度数字是编的（herdr/demo/fixture.py）；图标用 unicode 档
（渲染时不依赖 Nerd Font）。状态栏、圆点认状态、跳转、浮窗、轨迹、点一轮让 Claude 滚过去都是真代码。
"""
import os
import re
import shutil
import subprocess
import sys
import time
import unicodedata
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
HERDR_DEMO = ROOT / "herdr" / "demo"
OUT = ROOT / "tmux" / "docs" / "demo.gif"
HOME = Path("/private/tmp/sttdemo")
WORK = Path(os.environ.get("TMPDIR", "/tmp")) / "statetrail-tmux-gif"
BIN = ROOT / "tmux" / "target" / "release" / "statetrail-tmux"
W, H = 170, 44
STAGE = "--stage" in sys.argv
ENV = {"HOME": str(HOME), "PATH": f"{WORK}/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
       "TERM": "xterm-256color", "LANG": "en_US.UTF-8", "STATETRAIL_STATE_DIR": str(HOME / ".local/state/statetrail-tmux")}
INNER = ["tmux", "-L", "sttdemo"]
OUTER = ["tmux", "-L", "sttouter", "-f", "/dev/null"]


def sh(*cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw).stdout


def env_cmd(*cmd):
    return ["env", "-i", *(f"{k}={v}" for k, v in ENV.items()), *cmd]


def inner(*args):
    return sh(*env_cmd(*INNER, *args))


def screen():
    return sh(*OUTER, "capture-pane", "-p", "-t", "outer")


def cols(s):
    return sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in s)


def find(text, right_of=0, row=None):
    """屏幕上 text 第一次出现的位置 (列, 行)，从 1 数；row：只看这一行（-1 = 最后一行，状态栏）。"""
    lines = screen().rstrip("\n").split("\n")
    rows = [(len(lines), lines[-1])] if row == -1 else list(enumerate(lines, 1))
    for y, line in rows:
        i = line.find(text)
        while i >= 0:
            x = cols(line[:i]) + 1
            if x > right_of:
                return x, y
            i = line.find(text, i + 1)
    raise SystemExit(f"屏幕上找不到 {text!r}:\n{screen()}")


def find_dot(rgb):
    """状态栏（最后一行）上这个颜色的那个圆点 (列, 行)：按 ANSI 颜色找，不按位置猜是第几个。"""
    raw = sh(*OUTER, "capture-pane", "-e", "-p", "-t", "outer").rstrip("\n").split("\n")
    line, y = raw[-1], len(raw)
    x, fg = 0, ""
    for tok in re.findall(r"\x1b\[[0-9;]*m|.", line):
        if tok.startswith("\x1b["):
            fg = tok if "38;" in tok else ("" if tok in ("\x1b[0m", "\x1b[m", "\x1b[39m") else fg)
            continue
        x += cols(tok)
        if tok == "●" and rgb in fg:
            return x, y
    raise SystemExit(f"状态栏上没有 {rgb} 的圆点:\n{line!r}")


def click(x, y):
    sh(*OUTER, "send-keys", "-t", "outer", "-l", f"\x1b[<0;{x};{y}M")
    time.sleep(0.12)
    sh(*OUTER, "send-keys", "-t", "outer", "-l", f"\x1b[<0;{x};{y}m")


def keys(*k):
    for key in k:
        sh(*OUTER, "send-keys", "-t", "outer", key)
        time.sleep(0.25)


frames = []


def shot(secs):
    n = len(frames) + 1
    ansi = WORK / f"f{n}.ansi"
    ansi.write_text(sh(*OUTER, "capture-pane", "-e", "-p", "-t", "outer"))
    png = WORK / f"f{n}.png"
    subprocess.run(["bash", str(HERDR_DEMO / "render.sh"), str(ansi), str(png), "14"], check=True, stdout=subprocess.DEVNULL)
    frames.append((png, secs))


def stage():
    sh(*OUTER, "kill-server")
    sh(*INNER, "kill-server")
    time.sleep(0.5)
    shutil.rmtree(HOME, ignore_errors=True)
    shutil.rmtree(WORK, ignore_errors=True)
    HOME.mkdir(parents=True)
    (WORK / "bin").mkdir(parents=True)
    subprocess.run(["cc", "-o", str(WORK / "bin" / "2.1.999"), str(HERE / "launcher.c")], check=True)
    sid = sh(*env_cmd(sys.executable, "-I", str(HERDR_DEMO / "fixture.py"), str(HOME), str(ROOT / "herdr"))).strip()
    transcript = next((HOME / ".claude" / "projects").glob(f"*/{sid}.jsonl"))
    # 章名：fixture 写在 herdr 版的状态目录里，tmux 版的在 STATETRAIL_STATE_DIR
    st = Path(ENV["STATETRAIL_STATE_DIR"])
    shutil.copytree(HOME / ".local/state/herdr/plugins/statetrail/chapters", st / "chapters")
    cfg = HOME / ".config" / "statetrail-tmux"
    cfg.mkdir(parents=True)
    # live_usage = false：不然会用钥匙串里的真令牌去查（钥匙串不跟着假 $HOME 走），录进真实用量
    (cfg / "config.toml").write_text('icons = "unicode"\nlang = "en"\nlive_usage = false\ndefault_mode = "wide"\nshell_height = 0\n')
    (HOME / "demo-state").mkdir()
    conf = HOME / ".tmux.conf"
    conf.write_text("set -g mouse on\nset -g default-terminal tmux-256color\nset -g status-style bg=#1e1e2e,fg=#a6adc8\n"
                    "set -g status-left-length 30\nset -g status-left '#[fg=#89b4fa,bold] #S #[default]'\nset -g status-right ''\n"
                    "set -g window-status-current-style fg=#cdd6f4,bold\nset -g pane-border-style fg=#45475a\n"
                    "set -g pane-active-border-style fg=#89b4fa\nset -g popup-style bg=default\nset -g popup-border-style fg=#89b4fa\n"
                    f"run-shell '{BIN} init'\n")
    fake = lambda path, s, title, state: f"2.1.999 python3 {HERE / 'fake_claude.py'} {path} {s} '{title}' {state}"
    work = HOME / "work" / "photo-tidy"
    sh(*env_cmd(*INNER, "-f", str(conf), "new-session", "-d", "-s", "photo-tidy", "-n", "photo-tidy", "-x", str(W), "-y", str(H - 1),
                "-c", str(work), fake(transcript, sid, "photo-tidy CLI", "idle")))
    made = set()
    for line in (HOME / "others.tsv").read_text().splitlines():
        osid, path, ws, cwd, title, state = line.split("\t")
        state = {"done": "idle", "blocked": "idle"}.get(state, state)   # 权限框留到第 2 幕再弹
        cmd = fake(path, osid, title, state)
        if ws not in made:
            inner("new-session", "-d", "-s", ws, "-n", title, "-c", cwd, "-x", str(W), "-y", str(H - 1), cmd)
            made.add(ws)
        else:
            inner("new-window", "-d", "-t", f"{ws}:", "-n", title, "-c", cwd, cmd)
    inner("source-file", str(conf))                # 第二遍：init 时所有 session 都在了
    sh(*OUTER, "new-session", "-d", "-s", "outer", "-x", str(W), "-y", str(H),
       " ".join(env_cmd(*INNER, "attach", "-t", "photo-tidy")))
    for _ in range(40):                            # 等轨迹自己打开（常驻进程：窗口够宽、有人在看、认得会话）
        time.sleep(0.5)
        if "Build the CLI" in screen():
            break
    time.sleep(2)
    return {l.split("\t")[4]: l.split("\t")[0] for l in (HOME / "others.tsv").read_text().splitlines()}


def main():
    if sh("defaults", "read", "com.google.Chrome", "GenAILocalFoundationalModelSettings").strip() != "1":
        raise SystemExit("先设 Chrome 策略，免得它下载本地 AI 模型：defaults write com.google.Chrome GenAILocalFoundationalModelSettings -int 1")
    sids = stage()
    if STAGE:
        print("搭好了：tmux -L sttouter attach；收掉：tmux -L sttouter kill-server; tmux -L sttdemo kill-server")
        return
    try:
        shot(2.6)                                                    # 1. Claude + 轨迹 + 状态栏圆点
        (HOME / "demo-state" / sids["migrate to v2"]).write_text("blocked")
        time.sleep(3)
        shot(2.0)                                                    # 2. api 那个圆点变红
        click(*find_dot("255;95;95"))                                # 红的那个（WAIT）
        time.sleep(2)
        shot(2.6)                                                    # 3. 跳过去：权限框
        keys("C-b", "g")
        time.sleep(2)
        shot(3.0)                                                    # 4. 跳转浮窗
        keys("q")
        time.sleep(0.8)
        click(*find("photo-tidy", row=-1))
        time.sleep(2)
        tx = find("Build the CLI")[0] - 4
        click(*find("Build the CLI", right_of=tx))                   # 第 3 轮在第一章里：先点开这一章
        time.sleep(1.5)
        click(*find("Can it skip", right_of=tx))
        time.sleep(2.5)
        shot(3.0)                                                    # 6. 点一轮：Claude 滚过去
    finally:
        sh(*OUTER, "kill-server")
        sh(*INNER, "kill-server")
    lst = WORK / "frames.txt"
    lst.write_text("".join(f"file '{p}'\nduration {d}\n" for p, d in frames) + f"file '{frames[-1][0]}'\n")
    OUT.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i", str(lst),
                    "-vf", "scale=1200:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse",
                    "-loop", "0", str(OUT)], check=True)
    print(OUT, f"{sum(d for _, d in frames):.1f}s", f"{OUT.stat().st_size // 1024} KB")
    shutil.rmtree(HOME, ignore_errors=True)


main()
