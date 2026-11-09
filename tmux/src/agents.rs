//! `statetrail-tmux agents`：常驻，看着 tmux 里所有 Claude，认出它们的状态，画到状态栏上。
//!
//! - 状态看屏幕认（core::detect，规则照搬 herdr），不靠钩子：钩子漏一次就一直错，屏幕不会。
//!   主回合停了、后台还有活的，屏幕上和闲着一样，另看两处：Claude 自己写的 <配置目录>/sessions/<进程号>.json 里的
//!   status（busy 在干活、shell 主回合停了但后台命令还在跑，2026-11-09 实测），和钩子记下的后台 subagent。
//! - 四种：WAIT 等你确认、RUN 在跑、DONE 做完了你还没看、READ 看过了 / 一直闲着。
//!   跑完时你正看着它就直接是 READ；DONE 等你把焦点放到它上面（或在 picker 里跳过去）才变 READ。
//! - 画两处：全局选项 @claude_dots（状态栏一排圆点，一个 Claude 一个，按 session 分组）、每个窗口的 @claude_win（窗口列表里的徽标）。
//!   变了才写。跑起来后在全局选项 @claude_agents_pid 写自己的进程号，Python 钩子看到它就不再写这两处。
//! - 省：每秒一次 tmux 调用（list-panes + list-clients 一起）；只有窗口有了新输出、或标题变了的 Claude 才去读屏，
//!   要读的几个也并成一次调用。闲着的 Claude 一次都不读。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use statetrail_core::detect::{self, State};

use crate::tmux;

const TICK: Duration = Duration::from_secs(1);
const RECHECK: Duration = Duration::from_secs(30); // 屏幕没动也隔这么久重读一次，免得错过什么
const PID_OPT: &str = "@claude_agents_pid";
const DOTS_OPT: &str = "@claude_dots";
const WIN_OPT: &str = "@claude_win";

const FMT: &str = "P\t#{pane_id}\t#{session_name}\t#{window_id}\t#{pane_current_command}\t#{window_activity}\t\
#{?#{m:*--agent-id*,#{pane_start_command}},1,0}\t#{pane_title}";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Show {
    Wait,
    Run,
    Done,
    Read,
}

struct Pane {
    id: String,
    sess: String,
    win: String,
    title: String,
    activity: String,
    mate: bool,
}

struct Agent {
    det: State,
    rule: &'static str,
    key: (String, String), // 上次读屏时的（窗口输出时间, 标题）
    read_at: Instant,
    show: Show,
}

#[derive(Default)]
struct Agents {
    agents: HashMap<String, Agent>,
    st: tmux::Status,
    own: Own,
    dots: Option<String>, // 上次写进 @claude_dots 的
    announced: bool,      // 这个 tmux server 上已经写过 @claude_agents_pid
}

/// Claude 自己报的状态：窗格 → status（"busy" / "shell" / "idle"）。每个会话文件变了才重读。
#[derive(Default)]
struct Own {
    files: HashMap<std::path::PathBuf, (SystemTime, Option<(String, String)>)>,
}

impl Own {
    fn reload(&mut self) -> HashMap<String, String> {
        let mut seen = HashSet::new();
        for dir in statetrail_core::claude::config_dirs() {
            for e in std::fs::read_dir(dir.join("sessions")).into_iter().flatten().flatten() {
                let path = e.path();
                if path.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                let Ok(mt) = e.metadata().and_then(|m| m.modified()) else { continue };
                seen.insert(path.clone());
                if self.files.get(&path).is_some_and(|(t, _)| *t == mt) {
                    continue;
                }
                let v: Option<serde_json::Value> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
                // "tmux": "session:@窗口.%窗格"
                let got = v.and_then(|v| {
                    let pane = v["tmux"].as_str()?.rsplit('.').next()?.to_owned();
                    Some((pane, v["status"].as_str()?.to_owned()))
                });
                self.files.insert(path, (mt, got));
            }
        }
        self.files.retain(|p, _| seen.contains(p));
        self.files.values().filter_map(|(_, g)| g.clone()).collect()
    }
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn is_claude(cmd: &str) -> bool {
    cmd == "claude" || tmux::is_version(cmd)
}

impl Agents {
    /// 一轮。返回 false = tmux 没在跑。
    fn tick(&mut self) -> bool {
        let fmt_c = "C\t#{client_flags}\t#{pane_id}";
        let fmt_w = "W\t#{window_id}\t#{@claude_win}";
        let Some(out) = tmux::run(&["list-panes", "-a", "-F", FMT, ";", "list-clients", "-F", fmt_c, ";", "list-windows", "-a", "-F", fmt_w])
        else {
            self.announced = false;
            return false;
        };
        let mut panes = Vec::new();
        let mut watched = HashSet::new();
        let mut wins: Vec<(String, String)> = Vec::new();
        for line in out.lines() {
            let f: Vec<&str> = line.splitn(8, '\t').collect();
            match f.as_slice() {
                ["P", id, sess, win, cmd, activity, mate, title] if is_claude(cmd) => panes.push(Pane {
                    id: id.to_string(),
                    sess: sess.to_string(),
                    win: win.to_string(),
                    title: title.to_string(),
                    activity: activity.to_string(),
                    mate: *mate == "1",
                }),
                // 控制模式的客户端（Collie 这类远程查看器挂上来的）总是带 focused，不算你在看
                ["C", flags, pane] if flags.split(',').any(|x| x == "focused") && !flags.split(',').any(|x| x == "control-mode") => {
                    watched.insert(pane.to_string());
                }
                ["W", win, badge] => wins.push((win.to_string(), badge.to_string())),
                ["W", win] => wins.push((win.to_string(), String::new())),
                _ => {}
            }
        }
        if !self.announced {
            let _ = tmux::run(&["set", "-g", PID_OPT, &std::process::id().to_string()]);
            self.dots = None;
            self.announced = true;
        }

        // 读屏：新来的、窗口有了新输出或标题变了的、太久没读的
        let need: Vec<&Pane> = panes
            .iter()
            .filter(|p| {
                self.agents
                    .get(&p.id)
                    .is_none_or(|a| a.key != (p.activity.clone(), p.title.clone()) || a.read_at.elapsed() > RECHECK)
            })
            .collect();
        let screens = capture_all(&need);
        self.st.reload();
        let own = self.own.reload();
        let now = now_secs();
        for p in &panes {
            let seen = watched.contains(&p.id);
            let a = self.agents.entry(p.id.clone()).or_insert_with(|| Agent {
                det: State::Idle,
                rule: "",
                key: (String::new(), String::new()),
                read_at: Instant::now(),
                // 刚起来不知道之前的事：钩子说做完了还没看的，先当 DONE
                show: if self.st.unread_done(&p.id) { Show::Done } else { Show::Read },
            });
            if let Some(screen) = screens.get(&p.id) {
                let d = detect::detect(screen, &p.title);
                if !d.skip {
                    a.det = d.state;
                    a.rule = d.rule;
                }
                a.key = (p.activity.clone(), p.title.clone());
                a.read_at = Instant::now();
            }
            let busy_sub = self.st.busy_subagents(&p.id, now) > 0
                || matches!(own.get(&p.id).map(String::as_str), Some("busy" | "shell"));
            a.show = match a.det {
                State::Blocked => Show::Wait,
                State::Working => Show::Run,
                _ if busy_sub => Show::Run,
                _ => match a.show {
                    Show::Run | Show::Wait | Show::Done if !seen && !(a.show == Show::Done && self.st.read(&p.id)) => {
                        // 停下了没人看着：做完了；已经是 DONE 的，picker / 跳转标过已读就算看过
                        if a.show == Show::Wait { Show::Read } else { Show::Done }
                    }
                    _ => Show::Read,
                },
            };
        }
        let live: HashSet<&str> = panes.iter().map(|p| p.id.as_str()).collect();
        self.agents.retain(|k, _| live.contains(k.as_str()));
        self.draw(&panes, &wins);
        true
    }

    fn draw(&mut self, panes: &[Pane], wins: &[(String, String)]) {
        let mut cmd: Vec<String> = Vec::new();
        let mut push = |args: &[&str]| {
            if !cmd.is_empty() {
                cmd.push(";".into());
            }
            cmd.extend(args.iter().map(|s| s.to_string()));
        };
        let shown: Vec<(&Pane, Show)> =
            panes.iter().filter(|p| !p.mate).filter_map(|p| self.agents.get(&p.id).map(|a| (p, a.show))).collect();
        let dots = render_dots(&shown);
        if self.dots.as_deref() != Some(dots.as_str()) {
            push(&["set", "-g", DOTS_OPT, &dots]);
            self.dots = Some(dots);
        }
        for (win, cur) in wins {
            let mine: Vec<(&Pane, Show)> = shown.iter().filter(|(p, _)| &p.win == win).copied().collect();
            let want = render_badge(&mine);
            if &want == cur {
                continue;
            }
            if want.is_empty() {
                push(&["set", "-wqu", "-t", win, WIN_OPT]);
            } else {
                push(&["set", "-w", "-t", win, WIN_OPT, &want]);
            }
        }
        if !cmd.is_empty() {
            let args: Vec<&str> = cmd.iter().map(String::as_str).collect();
            let _ = tmux::run(&args);
        }
    }
}

/// 几个窗格的屏幕一次调用读完：每个前面先打一行 \x01窗格号 当分隔。
fn capture_all(panes: &[&Pane]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if panes.is_empty() {
        return out;
    }
    let mut args: Vec<&str> = Vec::new();
    for p in panes {
        if !args.is_empty() {
            args.push(";");
        }
        args.extend(["display", "-p", "-t", &p.id, "\x01#{pane_id}", ";", "capture-pane", "-p", "-t", &p.id]);
    }
    let Some(text) = tmux::run(&args) else { return out };
    let mut cur: Option<String> = None;
    let mut buf = String::new();
    for line in text.split_inclusive('\n') {
        if let Some(id) = line.strip_prefix('\x01') {
            if let Some(c) = cur.take() {
                out.insert(c, std::mem::take(&mut buf));
            }
            cur = Some(id.trim_end().to_owned());
        } else {
            buf.push_str(line);
        }
    }
    if let Some(c) = cur {
        out.insert(c, buf);
    }
    out
}

// ---- 画 ----------------------------------------------------------------------------------------------
// 颜色、图标和 claude-tmux-sessions 的窗口徽标同一套（hooks/tmux_status_update.py 的 BADGE_STYLES）：
// WAIT 红、DONE 亮绿、RUN 暗金（会动、又帮不上忙，不该抢眼）、READ 灰。

const DIM: &str = "#585858";

fn colour(s: Show) -> &'static str {
    match s {
        Show::Wait => "#ff5f5f",
        Show::Done => "#5fff00",
        Show::Run => "#af8700",
        Show::Read => DIM,
    }
}

/// 状态栏那一排：`OS ●●●  news ●●●●`，当前 session 的名字亮，点一个点跳到那个 Claude（status-click.sh 的 p<n>）。
/// range 名用 p<数字> 不用 %<数字>：选项内容会被当格式再展开一遍，% 会被 strftime 吃掉。
fn render_dots(shown: &[(&Pane, Show)]) -> String {
    let mut groups: Vec<(&str, String)> = Vec::new();
    for (p, s) in shown {
        if groups.last().is_none_or(|g| g.0 != p.sess) {
            groups.push((&p.sess, String::new()));
        }
        let g = groups.last_mut().unwrap();
        g.1 += &format!("#[range=user|p{}]#[fg={}]●#[norange]", p.id.trim_start_matches('%'), colour(*s));
    }
    let out: Vec<String> = groups
        .iter()
        .map(|(sess, dots)| {
            let name = sess.replace('#', "##");
            let cond = name.replace(',', "#,").replace('}', "#}");
            // 条件分支里不能有逗号，所以亮的样式拆成两个 #[]
            format!("#{{?#{{==:#{{session_name}},{cond}}},#[fg=#e4e4e4]#[bold],#[fg=#8a8a8a]}}{name}#[nobold] {dots}")
        })
        .collect();
    if out.is_empty() { String::new() } else { out.join("  ") + "#[default]" }
}

/// 一个窗口的徽标：有哪几种状态就画哪几种，最要紧的在前，同一种多于一个写个数。
/// 只有一个在跑时借 Claude 标题里那个会转的字（◐ ◑ …）；READ 只是灰，整个窗口都没事时连窗口名一起变灰（当前窗口除外）。
fn render_badge(mine: &[(&Pane, Show)]) -> String {
    const ORDER: [(Show, &str, &str); 4] = [
        (Show::Wait, "⏸\u{fe0e}", "#[fg=#ff5f5f,bold]"),
        (Show::Done, "✔\u{fe0e}", "#[fg=#5fff00,bold]"),
        (Show::Run, "▶\u{fe0e}", "#[fg=#af8700]"),
        (Show::Read, "✓\u{fe0e}", "#[fg=#585858]"),
    ];
    let mut parts = String::new();
    for (s, icon, style) in ORDER {
        let of: Vec<&Pane> = mine.iter().filter(|(_, x)| *x == s).map(|(p, _)| *p).collect();
        if of.is_empty() {
            continue;
        }
        let icon = if s == Show::Run && of.len() == 1 { run_spinner(&of[0].id) } else { icon.to_owned() };
        let style = if s == Show::Read { format!("#{{?window_active,,{style}}}") } else { style.to_owned() };
        let n = if of.len() > 1 { of.len().to_string() } else { String::new() };
        parts += &format!("{style}{icon}{n}#[default]");
    }
    if parts.is_empty() {
        return parts;
    }
    if mine.iter().all(|(_, s)| *s == Show::Read) {
        return format!(" {parts}#{{?window_active,#[default],#[fg={DIM}]}}");
    }
    format!(" {parts}")
}

/// 在跑的那一格借 Claude 标题开头那个会转的字；标题开头是 ASCII 或 ✳（闲着的字）就用 ▶。和 Python 版 run_spinner 一样。
fn run_spinner(pane: &str) -> String {
    let num = pane.trim_start_matches('%');
    let g = "#{=1:pane_title}";
    format!("#{{P:#{{?#{{==:#{{s|^.||:pane_id}},{num}}},#{{?#{{||:#{{m:[ -~],{g}}},#{{==:{g},✳}}}},▶\u{fe0e},{g}}},}}}}")
}

// ---- 入口 --------------------------------------------------------------------------------------------

/// `agents`：常驻。`agents --once`：读一遍，打印每个 Claude 的状态和命中的规则（不写 tmux）。
pub fn run(once: bool) {
    // 别继承「当前客户端」：Collie 这类只读控制客户端挂着时，tmux 会挑它当当前客户端，写选项会被拒（client is read-only）
    unsafe { std::env::remove_var("TMUX") };
    let mut s = Agents::default();
    if once {
        let Some(out) = tmux::run(&["list-panes", "-a", "-F", FMT]) else { return };
        let panes: Vec<Pane> = out
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.splitn(8, '\t').collect();
                let ["P", id, sess, win, cmd, act, mate, title] = f.as_slice() else { return None };
                is_claude(cmd).then(|| Pane {
                    id: id.to_string(),
                    sess: sess.to_string(),
                    win: win.to_string(),
                    title: title.to_string(),
                    activity: act.to_string(),
                    mate: *mate == "1",
                })
            })
            .collect();
        let refs: Vec<&Pane> = panes.iter().collect();
        let screens = capture_all(&refs);
        s.st.reload();
        let own = s.own.reload();
        for p in &panes {
            let d = detect::detect(screens.get(&p.id).map(String::as_str).unwrap_or(""), &p.title);
            let subs = s.st.busy_subagents(&p.id, now_secs());
            let o = own.get(&p.id).map(String::as_str).unwrap_or("-");
            println!("{}\t{}\t{:?}\t{}\tclaude={o}\tsubagents={subs}{}", p.id, p.sess, d.state, d.rule, if p.mate { "\tmate" } else { "" });
        }
        return;
    }
    let code = crate::exe_mtime();
    let mut code_new = None;
    loop {
        let up = s.tick();
        if crate::code_changed(code, &mut code_new) {
            crate::restart();
        }
        std::thread::sleep(if up { TICK } else { TICK * 5 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, sess: &str, win: &str) -> Pane {
        Pane {
            id: id.into(),
            sess: sess.into(),
            win: win.into(),
            title: String::new(),
            activity: String::new(),
            mate: false,
        }
    }

    #[test]
    fn dots_group_by_session() {
        let (a, b, c) = (pane("%1", "OS", "@1"), pane("%2", "OS", "@1"), pane("%7", "news", "@3"));
        let d = render_dots(&[(&a, Show::Run), (&b, Show::Read), (&c, Show::Wait)]);
        let plain: String = {
            let mut s = d.clone();
            while let (Some(i), Some(j)) = (s.find("#["), s.find(']')) {
                if j < i {
                    break;
                }
                s.replace_range(i..=j, "");
            }
            s
        };
        assert!(plain.contains("OS ●●") && plain.contains("news ●"), "{plain}");
        assert!(d.contains("range=user|p7]#[fg=#ff5f5f]●"));
        assert!(!d.contains('%'));
    }

    #[test]
    fn quiet_window_fades() {
        let a = pane("%1", "OS", "@1");
        assert!(render_badge(&[(&a, Show::Read)]).ends_with("#{?window_active,#[default],#[fg=#585858]}"));
        assert!(render_badge(&[(&a, Show::Done)]).starts_with(" #[fg=#5fff00,bold]✔"));
        assert_eq!(render_badge(&[]), "");
    }
}
