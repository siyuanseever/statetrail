#!/usr/bin/env python3
"""resource-check.py — 发版前的资源检查：开着的每个轨迹占多少 CPU、内存、网络，额度查询实际发了几次。

    python3 herdr/tools/resource-check.py [秒数，默认 60]

量的是本机正在跑的 statetrail 轨迹（`trail.py pane` 进程，连同它的 fzf 子进程），不起新的东西：
- CPU：这段时间里累计 CPU 时间的增量 ÷ 时长，即平均占单核的百分比
- 内存：每 5 秒看一次 RSS，报最大值
- 网络：macOS 的 nettop 按进程给累计收发字节，取开始、结束两次的差
- 额度查询：core/claude.py 每真发出一次就往 fetch.log 记一行，数这段时间和最近 30 分钟里的行数

预算（docs/RELEASING.md 第一节）：每个轨迹（python + fzf）空闲时 CPU ≤ 0.5%、Claude 干活时 ≤ 2%，RSS ≤ 60 MB，
空闲时网络 0 字节，额度查询每个账号每 30 分钟最多 1 次。有一项超了就以非 0 退出。
"""
import json
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(HERE))
import trail                                    # noqa: E402  只用它的 herdr 接口和路径
from core import claude                         # noqa: E402

CPU_IDLE, CPU_BUSY, RSS_MB, FETCH_PER_30MIN = 0.5, 2.0, 60, 1


def ps(pid, field):
    r = subprocess.run(["ps", "-o", f"{field}=", "-p", str(pid)], capture_output=True, text=True)
    return r.stdout.strip()


def cpu_secs(pid):
    """ps 的 TIME（[[d-]hh:]mm:ss.cc）→ 秒；进程没了就 None。"""
    t = ps(pid, "time")
    if not t:
        return None
    days, _, t = t.rpartition("-")
    secs = 0.0
    for part in t.split(":"):
        secs = secs * 60 + float(part)
    return secs + (int(days) * 86400 if days else 0)


def rss_mb(pid):
    v = ps(pid, "rss")
    return int(v) / 1024 if v else 0.0


def net_bytes():
    """{pid: (收, 发)}：nettop 一次给全部进程的累计字节。没有网络连接的进程不出现，当 0。"""
    try:
        out = subprocess.run(["nettop", "-P", "-L", "1", "-n", "-x", "-J", "bytes_in,bytes_out"],
                             capture_output=True, text=True, timeout=20).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    got = {}
    for line in out.splitlines()[1:]:
        name, _, rest = line.partition(",")
        pid = name.rpartition(".")[2]
        cols = rest.split(",")
        if pid.isdigit() and len(cols) >= 2 and cols[0].isdigit() and cols[1].isdigit():
            got[int(pid)] = (int(cols[0]), int(cols[1]))
    return got


def trails():
    """[(轨迹窗格, Claude 窗格, python pid, [fzf pid])]，按登记文件找，只认还活着的。"""
    out = []
    for f in sorted(trail.RUN.glob("*.json")):
        try:
            r = json.loads(f.read_text())
            pid = int(r["pid"])
        except (OSError, ValueError, KeyError):
            continue
        if not ps(pid, "pid"):
            continue
        kids = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout.split()
        fzf = [int(k) for k in kids if ps(k, "comm").endswith("fzf")]
        out.append((r["sidebar"], r["target"], pid, fzf))
    return out


def fetches(since):
    log = claude.USAGE_DIR / claude.FETCH_LOG
    rows = []
    try:
        for line in log.read_text().splitlines():
            ts, _, rest = line.partition("\t")
            if ts.isdigit() and int(ts) >= since:
                rows.append((int(ts), rest.split("\t")[0]))
    except OSError:
        pass
    return rows


def main():
    secs = int(sys.argv[1]) if len(sys.argv) > 1 else 60
    ts = trails()
    if not ts:
        sys.exit("没有开着的轨迹（statetrail 的 run/ 里没有活着的登记）")
    pids = [p for _, _, py, fz in ts for p in [py, *fz]]
    t0, cpu0, net0 = time.monotonic(), {p: cpu_secs(p) for p in pids}, net_bytes()
    peak = {p: rss_mb(p) for p in pids}
    status = {}
    print(f"量 {len(ts)} 个轨迹，{secs} 秒……", flush=True)
    while time.monotonic() - t0 < secs:
        time.sleep(min(5, max(0.1, secs - (time.monotonic() - t0))))
        for p in pids:
            peak[p] = max(peak[p], rss_mb(p))
        for sb, tg, *_ in ts:                  # 这段时间里 Claude 干过活没有：干过就按「干活时」的预算
            st = (trail.pane_info(tg) or {}).get("agent_status")
            status[sb] = status.get(sb) or ("busy" if st in ("working", "blocked") else None)
    took = time.monotonic() - t0
    cpu1, net1 = {p: cpu_secs(p) for p in pids}, net_bytes()

    def cpu(p):
        return 0.0 if cpu0[p] is None or cpu1[p] is None else (cpu1[p] - cpu0[p]) / took * 100

    def net(p):
        if net0 is None or net1 is None:
            return None
        a, b = net0.get(p, (0, 0)), net1.get(p, (0, 0))
        return (b[0] - a[0]) + (b[1] - a[1])
    bad, tot_cpu, tot_rss, tot_net = [], 0.0, 0.0, 0
    print(f"\n{'轨迹':8} {'Claude':8} {'状态':5} {'CPU%':>6} {'RSS MB':>7} {'网络 B':>7}   （python + fzf）")
    for sb, tg, py, fz in ts:
        c = sum(cpu(p) for p in [py, *fz])
        m = sum(peak[p] for p in [py, *fz])
        n = [net(p) for p in [py, *fz]]
        n = None if None in n else sum(n)
        busy = status.get(sb) == "busy"
        lim = CPU_BUSY if busy else CPU_IDLE
        over = [f"CPU {c:.2f}% > {lim}%" for _ in [0] if c > lim] + [f"RSS {m:.0f} MB > {RSS_MB} MB" for _ in [0] if m > RSS_MB]
        if n and not busy:
            over.append(f"空闲时有网络流量 {n} B")
        bad += [f"{sb}: {o}" for o in over]
        tot_cpu, tot_rss, tot_net = tot_cpu + c, tot_rss + m, tot_net + (n or 0)
        print(f"{sb:8} {tg:8} {'干活' if busy else '空闲':4} {c:6.2f} {m:7.1f} {'?' if n is None else n:>7}"
              + ("   ← " + "；".join(over) if over else ""))
    print(f"{'合计':8} {'':8} {'':5} {tot_cpu:6.2f} {tot_rss:7.1f} {tot_net:>7}")
    now = int(time.time())
    win, last30 = fetches(now - int(took) - 1), fetches(now - 1800)
    per = {}
    for _, acct in last30:
        per[acct] = per.get(acct, 0) + 1
    print(f"\n额度查询：这 {took:.0f} 秒里 {len(win)} 次；最近 30 分钟 {len(last30)} 次，按账号 {per or '{}'}"
          f"（实时查询{'开着' if trail.CFG['live_usage'] else '关着'}）")
    bad += [f"账号 {a} 最近 30 分钟查了 {n} 次 > {FETCH_PER_30MIN}" for a, n in per.items() if n > FETCH_PER_30MIN]
    if net0 is None:
        print("（nettop 不可用，网络没量）")
    print("\n" + ("超出预算：\n  " + "\n  ".join(bad) if bad else "全部在预算内"))
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
