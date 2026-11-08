//! 常驻进程（`statetrail-tmux daemon`，launchd 拉起）：每秒一次 `tmux list-panes -a`，别的都不做。
//! 和 Python 版 trail_daemon.py 的 tick 一样的规矩，只是不再替轨迹拼列表、推 fzf（轨迹自己画）：
//! - 默认打开：Claude 独占一个窗口、够宽（≥120 列）、你正看着、没被你关过、知道是哪个会话，就在右边开轨迹
//! - Claude 窗格没了，或者连续 5 秒不是 Claude（正在换账号的不算）：收掉轨迹；轨迹没了，它上面那条命令行也收掉
//! - 队友（agent teams）在同一个窗口开了窗格：平铺的轨迹先收起来（不记 @trail_off），队友走了自动回来；浮着的不动
//! - 轨迹和命令行都补上 @resurrect_skip（tmux-resurrect 恢复后不留空壳）
//! - 告诉每个轨迹它的窗口有没有人在看、焦点在不在这一对上（只有这时它才去读 Claude 的屏幕）、Claude 在哪个目录：
//!   变了才发，发到它的命令口
//! 只有一次 list-panes 成功了才动手：一次调用失败（机器很忙时会超时）不会把轨迹关掉。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

use crate::{Config, open, pane, tmux};

const TICK: Duration = Duration::from_secs(1);

// 用户选项在格式里会一路往上找到全局的（@trail_auto），省一次 tmux show。目录放最后：按制表符切，目录名里就算有制表符也不错位
const FMT: &str = "#{pane_id}\t#{@trail_for}\t#{window_id}\t#{pane_width}\t#{pane_current_command}\t#{@trail_off}\t\
#{window_active}\t#{session_attached}\t#{@trail_auto}\t#{pane_floating_flag}\t#{@picker_float}\t#{@trail_shell}\t\
#{@resurrect_skip}\t#{pane_active}\t#{pane_current_path}";

struct P {
    win: String,
    width: i64,
    claude: bool,
    off: bool,
    float: bool,
    seen: bool,
    active: bool,
    cwd: String,
}

#[derive(Default)]
struct Daemon {
    not_claude: HashMap<String, u32>, // Claude 窗格 → 连续几轮不是 Claude 了
    crowded: HashMap<String, u32>,    // Claude 窗格 → 连续几轮挤着队友
    sent: HashMap<String, String>,    // 轨迹窗格 → 上次发给它的状态
    st: tmux::Status,
}

/// 正在换账号：Claude 要退出、在 shell 里重启，中间那几秒不是 Claude，别把轨迹收了。
/// 两处都看：轨迹自己换的（本程序的状态目录），和 Python 侧栏 / 网页换的（claude-trail 的）。
fn switching(caller: &str) -> bool {
    [crate::run_dir(), crate::home().join(".local/state/claude-trail/run")].iter().any(|d| {
        std::fs::read(d.join(format!("{caller}.switch")))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v["state"] == "running")
    })
}

impl Daemon {
    fn tick(&mut self, cfg: &Config) {
        let Some(out) = tmux::run(&["list-panes", "-a", "-F", FMT]) else {
            return; // tmux 没在跑或一时没回：什么都不动
        };
        self.st.reload();
        let mut panes: HashMap<String, P> = HashMap::new();
        let mut sidebars: Vec<(String, String)> = Vec::new();
        let mut shells: Vec<(String, String)> = Vec::new();
        let mut tiled: HashMap<String, u32> = HashMap::new();
        let mut mates: HashMap<String, u32> = HashMap::new();
        let mut auto = true;
        for line in out.lines() {
            let f: Vec<&str> = line.splitn(15, '\t').collect();
            let [pid, trail_for, win, width, cmd, off, wactive, attached, tauto, floating, pfloat, trail_shell, rskip, pactive, cwd] =
                f.as_slice()
            else {
                continue;
            };
            if *tauto == "0" {
                auto = false;
            }
            if (!trail_for.is_empty() || !trail_shell.is_empty()) && *rskip != "1" {
                tmux::set_opt(pid, "@resurrect_skip", "1");
            }
            let claude = tmux::is_version(cmd);
            // 窗口里平铺着几个「正经」窗格：轨迹、它上面的命令行、浮窗都不算；其中几个是 Claude（队友开出来的每一格都是）
            if trail_for.is_empty() && trail_shell.is_empty() && *floating != "1" && pfloat.is_empty() {
                *tiled.entry(win.to_string()).or_default() += 1;
                if claude {
                    *mates.entry(win.to_string()).or_default() += 1;
                }
            }
            panes.insert(
                pid.to_string(),
                P {
                    win: win.to_string(),
                    width: width.parse().unwrap_or(0),
                    claude,
                    off: *off == "1",
                    float: *floating == "1",
                    seen: *wactive == "1" && !matches!(*attached, "" | "0"),
                    active: *pactive == "1",
                    cwd: cwd.to_string(),
                },
            );
            if !trail_for.is_empty() {
                sidebars.push((pid.to_string(), trail_for.to_string()));
            }
            if !trail_shell.is_empty() {
                shells.push((pid.to_string(), trail_shell.to_string()));
            }
        }
        let mut kept: HashSet<String> = HashSet::new();
        for (sb, caller) in &sidebars {
            let c = panes.get(caller);
            let mut gone = c.is_none();
            match c {
                Some(c) if !c.claude && !switching(caller) => {
                    let n = self.not_claude.entry(caller.clone()).or_default();
                    *n += 1;
                    gone = *n >= 5;
                }
                Some(_) => {
                    self.not_claude.remove(caller);
                }
                None => {}
            }
            let floating = panes.get(sb).is_some_and(|p| p.float);
            if let Some(c) = c
                && mates.get(&c.win).copied().unwrap_or(0) > 1
                && !floating
            {
                let n = self.crowded.entry(caller.clone()).or_default();
                *n += 1;
                gone |= *n >= 2;
            } else {
                self.crowded.remove(caller);
            }
            if gone {
                let _ = tmux::run(&["kill-pane", "-t", sb]);
                continue;
            }
            kept.insert(caller.clone());
            let Some(c) = c else { continue };
            // 焦点在这一对上：窗口看得见，活动窗格是 Claude、轨迹或它上面那条命令行
            let shell_active = shells.iter().any(|(sh, o)| o == caller && panes.get(sh).is_some_and(|p| p.active));
            let mine_active = c.active || panes.get(sb).is_some_and(|p| p.active) || shell_active;
            let self_active = c.seen && panes.get(sb).is_some_and(|p| p.active);
            let msg = format!("state {}{}{} {}", u8::from(c.seen), u8::from(c.seen && mine_active), u8::from(self_active), c.cwd);
            if self.sent.get(sb) != Some(&msg) && pane::send_command(sb, &msg) {
                self.sent.insert(sb.clone(), msg);
            }
        }
        for (sh, caller) in &shells {
            if !kept.contains(caller) {
                let _ = tmux::run(&["kill-pane", "-t", sh]);
            }
        }
        self.sent.retain(|sb, _| panes.contains_key(sb));
        // 默认打开：Claude 独占窗口、够宽、正被看着、没被你关过、知道是哪个会话
        if cfg.auto_open && auto {
            let have: HashSet<&String> = sidebars.iter().map(|(_, c)| c).collect();
            for (pid, p) in &panes {
                if p.claude
                    && !have.contains(pid)
                    && tiled.get(&p.win) == Some(&1)
                    && p.width >= cfg.min_width
                    && p.seen
                    && !p.off
                    && self.st.session(pid).is_some()
                {
                    let _ = open::open(pid, cfg);
                }
            }
        }
        // 轨迹被强杀、没来得及收拾的命令口文件：窗格已经不在、放了 10 秒以上的删掉
        for e in std::fs::read_dir(crate::run_dir()).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.split('.').next().unwrap_or("");
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(10));
            if old && stem.starts_with('%') && !panes.contains_key(stem) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn log(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::state_dir().join("daemon.log"))
    {
        let _ = writeln!(f, "{} {msg}", chrono::Local::now().format("%F %T"));
    }
}

pub fn run(cfg: Config) {
    let _ = std::fs::create_dir_all(crate::run_dir());
    // 只跑一个：launchd 拉起的和手动起的撞上时，后来的那个直接退
    let lock = File::create(crate::state_dir().join("daemon.lock"));
    if let Ok(l) = &lock
        && l.try_lock().is_err()
    {
        eprintln!("statetrail-tmux daemon: already running");
        return;
    }
    log(&format!("start {}", env!("CARGO_PKG_VERSION")));
    let mut d = Daemon::default();
    let (code, mut code_new) = (crate::exe_mtime(), None);
    loop {
        let t0 = Instant::now();
        d.tick(&cfg);
        if crate::code_changed(code, &mut code_new) {
            log("new version: restarting");
            drop(lock); // 放开锁，新程序要拿
            crate::restart();
            return;
        }
        std::thread::sleep(TICK.saturating_sub(t0.elapsed()));
    }
}
