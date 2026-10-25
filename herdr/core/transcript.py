"""transcript.py — 读对话记录、切轮次。

每种 agent 一个读取器，接口都一样（以后加 Codex / OpenCode 就是再写一个类，登记进 READERS）：

    tr = reader_for("claude")(path)
    tr.update()      # 读新追加的部分；返回这次有没有读到新东西
    tr.visible()     # [{id, kind, prompt, reply, ts, compacted, has_text}, ...]，按时间正序

一轮 = 一次让 agent 开始干活的输入（你说的话，或者后台任务完成、别的会话发来的消息），加上它之后
agent 写的文字。reply 只留最后一段文字的第一行，够侧栏显示、够跳转时认位置。

切轮规则从 tmux 版搬来（sidebar/claude_trail.py 的 _trigger、parse_session，sidebar/trail_daemon.py 的
Transcript），两边对同一份对话记录切出来的轮次一致。
"""
import json
import os
import re
import subprocess
import sys
from pathlib import Path

from . import i18n
from .i18n import t as _t

# Plain-string user entries that are harness noise, not something that makes Claude reply.
NOISE_PREFIXES = ("<local-command", "<command-name>", "<command-message>",
                  "This session is being continued from a previous conversation")
SLASH_CMD = re.compile(r"^/[\w:-]+(\s|$)")
PROMPT_MAX = 4000        # 展开时最多显示你那句话的多少个字
CHILD_OVER = 8 * 1024 * 1024   # 一次要读这么多以上（打开轨迹、原地重启时的第一次）就交给子进程解析


def text_of(content):
    if isinstance(content, str):
        return content
    parts = []
    for block in content or []:
        if isinstance(block, dict) and block.get("type") == "text":
            parts.append(block.get("text", ""))
    return "\n".join(parts)


def first_line(s):
    for line in s.splitlines():
        line = re.sub(r"[#*`>|]", "", line)
        # 下划线只剥 markdown 的强调（_重点_、__粗体__），不剥标识符里的：created_at、my_var 原样显示
        line = re.sub(r"(?<![A-Za-z0-9])_+|_+(?![A-Za-z0-9])", "", line).strip(" -")
        if line:
            return line
    return ""


def _trigger(entry):
    """If this entry starts a new stretch of Claude work, return (kind, text)."""
    if entry.get("type") != "user" or entry.get("isSidechain"):
        return None
    content = (entry.get("message") or {}).get("content")
    if isinstance(content, list) and any(
            isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
        return None
    kind = (entry.get("origin") or {}).get("kind")
    text = text_of(content).strip()
    if kind == "task-notification":
        return ("notify", _t("后台任务完成"))
    if kind == "peer":
        return ("peer", _t("收到其他会话的消息"))
    if entry.get("isMeta") or not text:
        return None
    if kind == "human":
        return ("human", text)
    if text.startswith(NOISE_PREFIXES) or text.startswith("<") or SLASH_CMD.match(text):
        return None
    return ("human", text)


class ClaudeTranscript:
    """Claude Code 的对话记录（<配置目录>/projects/*/<会话 id>.jsonl）的增量解析：只读新追加的行，
    每轮只留侧栏要显示的东西（不留全文），十几份大对话记录常驻也占不了多少内存。"""

    agent = "claude"

    def __init__(self, path):
        self.path, self.pos, self.size = path, 0, -1
        self.stops, self.title, self.cur = [], None, None
        self.compacts = 0                         # 见过几个压缩边界（子进程把它交回来）

    def update(self):
        try:
            size = self.path.stat().st_size
        except OSError:
            return False
        if size == self.size:
            return False
        if size < self.pos:                       # 被截断/重写了：从头来
            self.__init__(self.path)
        if size - self.pos > CHILD_OVER and not os.environ.get("STATETRAIL_IN_CHILD") and self.in_child(size):
            return True
        # 一行一行地读、读一行解析一行，不把整个文件读进内存：几百 MB 的对话记录一次读进来再解码、切行，
        # 峰值要好几 GB，Python 用过的内存又很少还给系统（实测 737 MB 的记录峰值 2.8 GB、读完还占 770 MB）
        start = self.pos
        with open(self.path, "rb") as fh:
            fh.seek(self.pos)
            for raw in fh:
                if not raw.endswith(b"\n"):      # 最后一行可能还没写完，留到下次
                    break
                self.pos += len(raw)
                try:
                    self.feed(json.loads(raw.decode("utf-8", "replace")))
                except ValueError:
                    continue
        self.size = size
        return self.pos > start

    def in_child(self, size):
        """大块的第一次读放进一个临时子进程：逐行解析时 CPython 用过的内存池还不回系统（几百 MB 的对话记录，真正
        留下的摘要不到 1 MB，进程却停在 55～62 MB），子进程退出就全还了。子进程只把每轮的摘要交回来。失败就返回
        False，照常在本进程里读。"""
        env = dict(os.environ, STATETRAIL_IN_CHILD="1", STATETRAIL_LANG=i18n.LANG)   # 「后台任务完成」这类字要同一种语言
        code = ("import sys, json; sys.path.insert(0, sys.argv[1]); from pathlib import Path\n"
                "from core.transcript import ClaudeTranscript\n"
                "t = ClaudeTranscript(Path(sys.argv[2])); t.pos = int(sys.argv[3])\n"
                "cur = json.load(sys.stdin)\n"
                "if cur: t.stops, t.cur = [cur], cur\n"     # 接着填前面那一轮：它的回复可能写在这一段开头
                "t.update()\n"
                "json.dump({'pos': t.pos, 'size': t.size, 'title': t.title, 'stops': t.stops,"
                " 'compacts': t.compacts}, sys.stdout)")
        try:
            r = subprocess.run([sys.executable, "-c", code, str(Path(__file__).resolve().parent.parent),
                                str(self.path), str(self.pos)], env=env, capture_output=True, text=True, timeout=600,
                               input=json.dumps(self.cur))
            got = json.loads(r.stdout) if r.returncode == 0 else None
        except (OSError, ValueError, subprocess.TimeoutExpired):
            got = None
        if not got:
            return False
        if self.cur is not None:                  # 子进程交回来的第一轮就是接着填完的那一轮，换掉这边的
            self.stops.pop()
        if got["compacts"]:                       # 子进程那一段里有压缩边界：这边已有的轮次也都在它之前
            for st in self.stops:
                st["compacted"] = True
        self.stops += got["stops"]
        self.title = got["title"] or self.title
        self.compacts += got["compacts"]
        self.cur = self.stops[-1] if self.stops else None    # cur 总是最后追加的那一轮
        self.pos, self.size = got["pos"], got["size"]
        return True

    def feed(self, e):
        t = e.get("type")
        if t == "ai-title":
            self.title = e.get("aiTitle") or self.title
            return
        if t == "system" and e.get("subtype") == "compact_boundary":
            # Claude's screen restarts here: earlier turns are no longer shown in it.
            for st in self.stops:
                st["compacted"] = True
            self.compacts += 1
            return
        trig = _trigger(e)
        if trig:
            self.cur = {"kind": trig[0], "prompt": trig[1][:PROMPT_MAX], "ts": e.get("timestamp"),
                        "id": e.get("uuid"), "reply": "", "has_text": False, "compacted": False}
            self.stops.append(self.cur)
            return
        if t == "assistant" and not e.get("isSidechain") and self.cur is not None:
            txt = text_of((e.get("message") or {}).get("content")).strip()
            if txt:
                self.cur["has_text"] = True
                self.cur["reply"] = first_line(txt)[:200]

    def visible(self):
        # A notification/peer stretch where Claude said nothing is noise; a human prompt
        # with no reply yet (still working, or interrupted) stays so you can see it.
        return [s for s in self.stops if s["has_text"] or s["kind"] == "human"]


READERS = {"claude": ClaudeTranscript}


def reader_for(agent):
    return READERS.get(agent)
