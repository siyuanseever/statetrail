#!/usr/bin/env python3
"""演示用的假 Claude Code（tmux 版）：把一份对话记录画成 Claude 全屏界面的样子，能翻页、到顶、到底、滚轮，
往上翻了就在底部显示「Jump to bottom」——轨迹的跳转靠的就是这些，所以演示里的跳转是真跳。

    2.1.999 python3 fake_claude.py <对话记录.jsonl> <会话 id> [终端标题] [状态]

由 launcher.c 编出来的 2.1.999 拉起（tmux 看到的进程名像 Claude 的版本号）。起来后照 Claude Code 的样子写
$HOME/.claude/sessions/<launcher 进程号>.json（会话 id、在哪个 tmux 窗格），statetrail 就认得这一格是哪个会话。
状态：idle / working / blocked，画在屏幕上（working：标题里转的字 + esc to interrupt；blocked：Bash 权限框），
statetrail 看屏幕认。演示中途改状态：往 $HOME/demo-state/<会话 id> 里写新的状态。
只是个画面：不调任何模型，也不响应输入框里的打字。
"""
import json
import os
import re
import select
import shutil
import signal
import subprocess
import sys
import termios
import textwrap
import time
import tty
import unicodedata

PATH, SID = sys.argv[1], sys.argv[2]
TITLE = sys.argv[3] if len(sys.argv) > 3 else "photo-tidy CLI"
STATE = [sys.argv[4] if len(sys.argv) > 4 else "idle"]
CTL = os.path.join(os.environ["HOME"], "demo-state", SID)
DIM, BOLD, RESET = "\033[2m", "\033[1m", "\033[0m"
ORANGE, GREY, BLUE = "\033[38;2;215;119;87m", "\033[38;2;136;136;136m", "\033[38;2;87;105;247m"
SPIN = "◐◓◑◒"


def body_lines(width):
    """对话记录 → 屏幕上的行：你的话 ❯ 开头，回复 ⏺ 开头，回复后面接一行耗时，和 Claude 的样子一样。"""
    lines = [f" {ORANGE}✻{RESET} {BOLD}Claude Code{RESET}", f"   {GREY}~/work{RESET}", ""]
    for raw in open(PATH):
        e = json.loads(raw)
        msg = e.get("message") or {}
        if e.get("type") == "user":
            for i, ln in enumerate(textwrap.wrap(msg["content"], width - 4) or [""]):
                lines.append((f"{GREY}❯{RESET} " if i == 0 else "  ") + f"{GREY}{ln}{RESET}")
            lines.append("")
        elif e.get("type") == "assistant":
            text = "".join(b.get("text", "") for b in msg.get("content", []))
            first = True
            for para in text.split("\n"):
                for ln in textwrap.wrap(para, width - 4) or [""]:
                    lines.append(("⏺ " if first else "  ") + ln)
                    first = False
            lines += ["", f"{GREY}✻ Worked for 38s{RESET}", ""]
    return lines


class Screen:
    def __init__(self):
        self.offset = 0                 # 从底往上翻了几行
        self.tick = 0

    def draw(self):
        w, h = shutil.get_terminal_size()
        rule = f"{GREY}{'─' * w}{RESET}"
        state = STATE[0]
        if state == "blocked":
            foot = [rule, f" {BOLD}Bash command{RESET}", "", "   git push origin main", "",
                    " Do you want to proceed?", f" {BLUE}❯ 1. Yes{RESET}", "   2. No", "",
                    f" {GREY}Esc to cancel · Tab to amend{RESET}"]
        else:
            tail = " · esc to interrupt" if state == "working" else " (shift+tab to cycle)"
            foot = [rule, f"{GREY}❯{RESET} ", rule, f"  {GREY}⏵⏵ auto mode on{tail}{RESET}"]
            if state == "working":
                foot = [f"{ORANGE}✻{RESET} Thinking… ({12 + self.tick // 2}s · ↓ 1.2k tokens)", ""] + foot
        lines = body_lines(w)
        room = h - len(foot)
        self.offset = max(0, min(self.offset, len(lines) - room))
        end = len(lines) - self.offset
        view = lines[max(0, end - room):end]
        if self.offset:                 # 往上翻了：和 Claude 一样，底部提示怎么回去
            view = view[1:] + [" " * (w - 26) + f"{BLUE}Jump to bottom (End) ↓{RESET}"]
        view = [""] * (room - len(view)) + view
        glyph = SPIN[self.tick % 4] if state == "working" else "✳"
        sys.stdout.write(f"\033]0;{glyph} {TITLE}\007\033[H\033[2J" + "\r\n".join(view + foot))
        sys.stdout.flush()


def session_file():
    """照 Claude Code 写 <配置目录>/sessions/<进程号>.json：进程号是 launcher（窗格里的那个进程）。"""
    pane = os.environ.get("TMUX_PANE", "")
    where = subprocess.run(["tmux", "display", "-p", "-t", pane, "#{session_name}:#{window_id}.#{pane_id}"],
                           capture_output=True, text=True).stdout.strip()
    d = os.path.join(os.environ["HOME"], ".claude", "sessions")
    os.makedirs(d, exist_ok=True)
    pid = os.getppid()
    with open(os.path.join(d, f"{pid}.json"), "w") as f:
        json.dump({"pid": pid, "sessionId": SID, "tmux": where, "kind": "interactive",
                   "status": "busy" if STATE[0] == "working" else "idle"}, f)
    return os.path.join(d, f"{pid}.json")


def main():
    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    tty.setraw(fd)
    sys.stdout.write("\033[?1049h\033[?25l\033[?1000h\033[?1006h")
    scr = Screen()
    signal.signal(signal.SIGWINCH, lambda *_: scr.draw())
    scr.draw()
    own = session_file()
    keys = {b"\x1b[5~": "pgup", b"\x1b[6~": "pgdn", b"\x1b[1;5H": "top", b"\x1b[1;5F": "end"}
    try:
        while True:
            r, _, _ = select.select([fd], [], [], 0.5)
            try:
                want = open(CTL).read().strip()
            except OSError:
                want = STATE[0]
            if want and want != STATE[0]:
                STATE[0] = want
                session_file()
            if STATE[0] == "working":
                scr.tick += 1
            if not r:
                scr.draw()
                continue
            data = os.read(fd, 4096)
            if not data or data in (b"\x03", b"\x04"):
                break
            h = shutil.get_terminal_size().lines - 4
            for seq in re.findall(rb"\x1b\[<\d+;\d+;\d+[Mm]|\x1b\[[\d;]*[~A-Za-z]|.", data, re.S):
                act = keys.get(seq)
                m = re.match(rb"\x1b\[<(\d+);\d+;\d+M", seq)
                if act == "pgup":
                    scr.offset += h // 2
                elif act == "pgdn":
                    scr.offset -= h // 2
                elif act == "top":
                    scr.offset = 10 ** 6
                elif act == "end":
                    scr.offset = 0
                elif m and m.group(1) in (b"64", b"65"):      # 滚轮：上 / 下，一格 3 行
                    scr.offset += 3 if m.group(1) == b"64" else -3
            scr.draw()
    finally:
        try:
            os.remove(own)
        except OSError:
            pass
        sys.stdout.write("\033[?1000l\033[?1006l\033[?25h\033[?1049l")
        termios.tcsetattr(fd, termios.TCSADRAIN, old)


main()
