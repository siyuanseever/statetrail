//! 和 tmux 打交道的这一层：调 tmux 命令、读 Claude 的屏幕、给它发键、认窗格里的 Claude 进程，
//! 以及 hook（claude-tmux-sessions 的 hooks/tmux_status_update.py）写的状态文件：哪个窗格跑的是哪个会话、在不在干活。
//! 对应 herdr 版的 herdr.rs。

use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::SystemTime;

/// tmux 程序：launchd 起的常驻进程 PATH 里没有 Homebrew，挨个找。
fn bin() -> &'static str {
    static BIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux", "/usr/bin/tmux"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
            .unwrap_or("tmux")
            .to_owned()
    })
}

/// 跑一条 tmux 命令，成功了返回它的输出。
/// 带 -u：launchd 起的进程没有 LANG，tmux 会把输出里的制表符和非 ASCII 字都换成 _（格式按制表符切就全错了）。
pub fn run(args: &[&str]) -> Option<String> {
    let mut c = Command::new(bin());
    c.arg("-u");
    // 在哪个 tmux 里（TMUX 环境变量的第一段是 socket）就连哪个，但不带「当前客户端」：Collie 这类只读控制客户端挂着时，
    // tmux 会挑它当当前客户端，写选项、切窗格会被拒（client is read-only）。launchd 拉起的没有 TMUX，连默认的。
    if let Some(sock) = std::env::var("TMUX").ok().and_then(|t| t.split(',').next().map(str::to_owned)).filter(|s| !s.is_empty()) {
        c.args(["-S", &sock]).env_remove("TMUX");
    }
    let o = c.args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// 按 tmux 的格式取一个窗格的几样东西（去掉结尾换行）。
pub fn display(pane: &str, fmt: &str) -> Option<String> {
    run(&["display", "-p", "-t", pane, fmt]).map(|s| s.trim_end_matches('\n').to_owned())
}

pub fn set_opt(pane: &str, name: &str, value: &str) {
    let _ = run(&["set", "-p", "-t", pane, name, value]);
}

pub fn unset_opt(pane: &str, name: &str) {
    let _ = run(&["set", "-p", "-u", "-t", pane, name]);
}

pub fn message(pane: &str, text: &str) {
    let _ = run(&["display-message", "-t", pane, text]);
}

/// Claude 屏幕上现在显示的内容（纯文字）。
pub fn capture(pane: &str) -> String {
    run(&["capture-pane", "-p", "-t", pane]).unwrap_or_default()
}

/// 一次 tmux 调用发一串键：起一次 tmux 要几毫秒，跳转一次要发几百次，所以合并成一条（`;` 连起来）。
/// 有名字的键（C-Home、PageDown）照名字发；原样的转义序列（滚轮）用 -l 原样送进去。
pub fn send_keys(pane: &str, keys: &[&str]) {
    let mut args: Vec<&str> = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            args.push(";");
        }
        args.extend(["send-keys", "-t", pane]);
        if k.starts_with('\x1b') {
            args.push("-l");
        }
        args.push(k);
    }
    if !args.is_empty() {
        let _ = run(&args);
    }
}

/// Claude Code 的进程名是它的版本号（2.1.310）。
pub fn is_version(name: &str) -> bool {
    name.split('.').count() == 3 && name.split('.').all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()))
}

/// 窗格里的 Claude 进程号：窗格自己的进程就是 Claude（直接 `claude` 开的窗格），或者是 shell 的直接子进程。
pub fn claude_pid(pane: &str) -> Option<u32> {
    let root: u32 = display(pane, "#{pane_pid}")?.parse().ok()?;
    let out = Command::new("ps").args(["-A", "-o", "pid=,ppid=,ucomm="]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<(u32, u32, &str)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()?))
        })
        .collect();
    let claude = |name: &str| is_version(name) || name == "claude";
    if rows.iter().any(|&(pid, _, name)| pid == root && claude(name)) {
        return Some(root);
    }
    rows.iter().find(|&&(_, ppid, name)| ppid == root && claude(name)).map(|r| r.0)
}

// ---- hook 写的状态文件 -------------------------------------------------------------------

/// `~/.claude/tmux-claude-status.json`：{"%窗格": {"session_id": …, "status": "running" | "blocked" | "input" | …}}。
/// 测试时用 STATETRAIL_STATUS_FILE 换一份，免得和正在用的 tmux 串窗格号。
pub fn status_file() -> PathBuf {
    std::env::var_os("STATETRAIL_STATUS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".claude/tmux-claude-status.json"))
}

/// 会话文件里的窗格号只在一个 tmux server 里有意义（每个 server 都从 %0 数起）：同时开着几个 tmux 时，别的 server 的
/// %1 会冒充这里的 %1。所以只认 Claude 进程真在这个窗格里的：进程就是窗格的进程，或者是它的直接子进程（shell 里起的）。
fn mine(found: Vec<(String, String, u32)>) -> HashMap<String, String> {
    if found.is_empty() {
        return HashMap::new();
    }
    let root: HashMap<String, u32> = run(&["list-panes", "-a", "-F", "#{pane_id} #{pane_pid}"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (p, pid) = l.split_once(' ')?;
            Some((p.to_owned(), pid.trim().parse().ok()?))
        })
        .collect();
    let parent: HashMap<u32, u32> = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .collect();
    found
        .into_iter()
        .filter(|(pane, _, pid)| root.get(pane).is_some_and(|r| r == pid || parent.get(pid) == Some(r)))
        .map(|(pane, sid, _)| (pane, sid))
        .collect()
}

/// agents.json 里一个窗格的状态：现在是 {"state": "run", "since": …}，以前直接是 "run"（换新版本那一下两种都可能读到）
pub fn agent_state(v: &Value) -> Option<&str> {
    v["state"].as_str().or_else(|| v.as_str())
}

/// 哪个窗格是哪个会话、在不在干活。三个来源，前面的说了算，钩子那份只是退路（没装 claude-tmux-sessions 的钩子也能用）：
/// - Claude 自己写的 <配置目录>/sessions/<进程号>.json："tmux": "session:@窗口.%窗格" 和 sessionId（进程退出时删掉，只有活着的）
/// - agents 常驻进程写的 <状态目录>/agents.json：看屏幕认的状态，和状态栏圆点同一份
/// - 钩子写的状态文件（上面 status_file）：会话 id、状态，还有 subagent、已读这些只有它记的
/// 都按修改时间缓存：没变就不重读。
#[derive(Default)]
pub struct Status {
    mtime: Option<SystemTime>,
    data: Value,
    own: HashMap<PathBuf, (SystemTime, Option<(String, String, u32)>)>, // Claude 的会话文件 → (窗格, 会话 id, Claude 进程号)
    own_sid: HashMap<String, String>,
    agents_mtime: Option<SystemTime>,
    agents: Value,
}

impl Status {
    /// 重读（文件变了才读）；返回这次有没有变。
    pub fn reload(&mut self) -> bool {
        let mut changed = false;
        let path = status_file();
        let m = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if m != self.mtime {
            self.mtime = m;
            if let Some(v) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
                self.data = v;
            }
            changed = true;
        }
        let path = crate::agents::state_file();
        let m = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if m != self.agents_mtime {
            self.agents_mtime = m;
            self.agents = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            changed = true;
        }
        changed | self.reload_own()
    }

    fn reload_own(&mut self) -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut changed = false;
        for dir in statetrail_core::claude::config_dirs() {
            for e in std::fs::read_dir(dir.join("sessions")).into_iter().flatten().flatten() {
                let path = e.path();
                if path.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                let Ok(mt) = e.metadata().and_then(|m| m.modified()) else { continue };
                seen.insert(path.clone());
                if self.own.get(&path).is_some_and(|(t, _)| *t == mt) {
                    continue;
                }
                let v: Option<Value> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
                let got = v.and_then(|v| {
                    let pane = v["tmux"].as_str()?.rsplit('.').next()?.to_owned();
                    let pid = v["pid"].as_u64()? as u32;
                    Some((pane, v["sessionId"].as_str()?.to_owned(), pid))
                });
                self.own.insert(path, (mt, got));
                changed = true;
            }
        }
        let before = self.own.len();
        self.own.retain(|p, _| seen.contains(p));
        if changed || self.own.len() != before {
            self.own_sid = mine(self.own.values().filter_map(|(_, g)| g.clone()).collect());
            return true;
        }
        false
    }

    pub fn session(&self, pane: &str) -> Option<String> {
        self.own_sid
            .get(pane)
            .cloned()
            .or_else(|| self.data[pane]["session_id"].as_str().filter(|s| !s.is_empty()).map(str::to_owned))
    }

    /// 这个窗格的 Claude 用的是哪个配置目录（哪个账号）：它的会话文件在哪个目录下就是哪个；认不出就 None
    pub fn config_dir(&self, pane: &str) -> Option<PathBuf> {
        let sid = self.own_sid.get(pane)?;
        self.own
            .iter()
            .find(|(_, (_, g))| g.as_ref().is_some_and(|(p, s, _)| p == pane && s == sid))
            .and_then(|(path, _)| path.parent()?.parent().map(std::path::Path::to_path_buf))
    }

    /// 用钩子的说法（running / blocked / done / input）：agents 常驻进程看屏幕认的优先
    pub fn state(&self, pane: &str) -> Option<String> {
        let mapped = match agent_state(&self.agents[pane]) {
            Some("wait") => Some("blocked"),
            Some("run") => Some("running"),
            Some("done") => Some("done"),
            Some("read") => Some("input"),
            _ => None,
        };
        mapped.map(str::to_owned).or_else(|| self.data[pane]["status"].as_str().map(str::to_owned))
    }

    /// 钩子记下的、还在跑的 subagent 有几个：SubagentStart 之后没等到 SubagentStop、3 小时内还有过动静。
    /// 判据和 claude-tmux-sessions 的 effective_status.live_subagents 一样（CLAUDE_TMUX_SUBAGENT_TTL_SECS 可改）。
    pub fn busy_subagents(&self, pane: &str, now: f64) -> usize {
        let ttl = std::env::var("CLAUDE_TMUX_SUBAGENT_TTL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(3.0 * 3600.0);
        self.data[pane]["subagents"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, r)| {
                let seen = r["seen_at"].as_f64().or_else(|| r["started_at"].as_f64()).unwrap_or(0.0);
                now - seen < ttl
            })
            .count()
    }

    /// 钩子那边这个窗格是「做完了、还没看」（Stop 之后没被标成已读）
    pub fn unread_done(&self, pane: &str) -> bool {
        matches!(self.data[pane]["status"].as_str(), Some("done" | "input")) && self.data[pane]["read"] != true
    }

    /// 钩子那边这个窗格已被标成已读（picker、跳转会标）
    pub fn read(&self, pane: &str) -> bool {
        self.data[pane]["read"] == true
    }

    /// 窗格 → 会话 id，所有知道会话的窗格（跨会话搜索时认哪些会话开着）
    pub fn sessions(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self.own_sid.iter().map(|(p, s)| (p.clone(), s.clone())).collect();
        out.extend(
            self.data
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(p, _)| !self.own_sid.contains_key(*p))
                .filter_map(|(p, e)| Some((p.clone(), e["session_id"].as_str()?.to_owned()))),
        );
        out
    }
}

/// 这一格的会话 id：先看状态文件，没有就按 Claude 进程找 <配置目录>/sessions/<进程号>.json（hook 还没写过的会话）。
pub fn session_of(pane: &str, st: &Status) -> Option<String> {
    st.session(pane).or_else(|| {
        let pid = claude_pid(pane)?;
        let f = statetrail_core::claude::config_dir_of_pid(Some(pid))
            .join("sessions")
            .join(format!("{pid}.json"));
        let v: Value = serde_json::from_slice(&std::fs::read(f).ok()?).ok()?;
        v["sessionId"].as_str().map(str::to_owned)
    })
}
