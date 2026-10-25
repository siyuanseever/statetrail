//! 轨迹窗格本身（`statetrail pane`）。主线程只在一个通道上等：键盘鼠标、herdr 事件、别的进程发来的命令（toggle）、
//! 跳转线程的结果各有一个线程往里送，空闲时不轮询、不醒。对应 Python 版的 Sidebar.run。

use crossterm::event::{self, Event, MouseButton, MouseEventKind};
use crossterm::{execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::i18n::t;
use crate::jump::{self, Outcome, Term};
use crate::transcript::Transcript;
use crate::ui::{self, Item};
use crate::{Config, FRAME, WIDE_MIN, claude, herdr};

const TICK: Duration = Duration::from_secs(60); // 没有事件时多久醒一次
const AGENT_CHECK: Duration = Duration::from_secs(5); // Claude 退出了却没等到事件：这么久补查一次
const GONE_GRACE: Duration = Duration::from_secs(2); // Claude 退出后过这么久就关掉轨迹，不留残骸
const WHEEL: usize = 3; // 滚轮一格翻几项

enum Msg {
    Input(Event),
    Herdr(Value),
    HerdrGone,
    Command(String),
    Flash(String, Duration),
    Jumped(String, String), // (轮次 id, 结果)
}

fn sock_of(pane: &str) -> PathBuf {
    crate::run_dir().join(format!("{}.sock", pane.replace(':', "_")))
}

/// 给开着的轨迹发一条命令（toggle 用）。
pub fn send_command(pane: &str, cmd: &str) {
    if let Ok(mut s) = UnixStream::connect(sock_of(pane)) {
        let _ = writeln!(s, "{cmd}");
    }
}

struct HerdrTerm {
    pane: String,
    height: usize,
}

impl Term for HerdrTerm {
    fn screen(&self) -> String {
        herdr::call("pane.read", json!({"pane_id": self.pane, "source": "visible"}))
            .ok()
            .and_then(|r| r["read"]["text"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }
    fn send(&self, keys: &[&str]) {
        let text: String = keys
            .iter()
            .map(|k| match *k {
                "C-Home" => "\x1b[1;5H",
                "C-End" => "\x1b[1;5F",
                "PageUp" => "\x1b[5~",
                "PageDown" => "\x1b[6~",
                raw => raw,
            })
            .collect();
        let _ = herdr::call("pane.send_text", json!({"pane_id": self.pane, "text": text}));
    }
    fn height(&self) -> usize {
        self.height
    }
}

struct Trail {
    target: String,
    me: String,
    cfg: Config,
    sid: Option<String>,
    cfg_dir: PathBuf,
    tr: Option<Transcript>,
    status: Option<String>,
    opened: Option<String>,
    expanded: bool,
    missing: HashSet<String>,
    flash: Option<(String, Instant)>,
    offset: usize,
    follow: bool, // 停在最底：列表变长时跟着走
    no_claude_since: Option<Instant>,
    jump_token: Arc<AtomicUsize>,
    last_wide: Option<i64>,
    hits: Vec<(u16, Item)>,
}

impl Trail {
    fn busy(&self) -> bool {
        matches!(self.status.as_deref(), Some("working" | "blocked"))
    }

    /// 问 herdr 这个 Claude 现在什么状态、跑的哪个会话，读对话记录新追加的部分。返回 false = Claude 那一格没了。
    fn refresh(&mut self) -> bool {
        let Some(info) = herdr::pane_info(&self.target) else { return false };
        self.status = info["agent_status"].as_str().map(str::to_owned);
        if info["agent"].as_str() == Some("claude") {
            self.no_claude_since = None;
        } else if self.no_claude_since.is_none() {
            self.no_claude_since = Some(Instant::now());
        }
        let sid = herdr::session_id(&info);
        if sid != self.sid {
            // /clear、/resume 换了会话
            self.sid = sid;
            self.tr = None;
            self.opened = None;
            self.expanded = false;
            self.missing.clear();
            self.follow = true;
            if self.sid.is_some() {
                self.cfg_dir = claude::config_dir_of_pid(herdr::claude_pid(&self.target));
            }
        }
        if let (Some(sid), None) = (&self.sid, &self.tr)
            && let Some(p) = claude::transcript_path(sid, &self.cfg_dir)
        {
            self.tr = Some(Transcript::new(&p));
        }
        if let Some(tr) = &mut self.tr {
            tr.update();
        }
        true
    }

    fn widths(&self) -> Option<(i64, i64)> {
        let r = herdr::call("pane.layout", json!({"pane_id": self.me})).ok()?;
        let w = |id: &str| r["layout"]["panes"].as_array()?.iter().find(|p| p["pane_id"] == id)?["rect"]["width"].as_i64();
        Some((w(&self.target)?, w(&self.me)?))
    }

    /// 把轨迹调到 cols 列：挪 Claude 和轨迹之间那条线（缩窄对 Claude 做 right，加宽对轨迹做 left，
    /// 反过来会去挤外侧的窗格）。herdr 把比例夹在 0.1–0.9，再窄它会自己夹住。
    fn set_width(&self, cols: i64) {
        let Some((tw, sw)) = self.widths() else { return };
        let total = (tw + sw) as f64;
        if sw > cols {
            let _ = herdr::call(
                "pane.resize",
                json!({"pane_id": self.target, "direction": "right", "amount": (sw - cols) as f64 / total}),
            );
        } else if sw < cols {
            let _ = herdr::call(
                "pane.resize",
                json!({"pane_id": self.me, "direction": "left", "amount": (cols - sw) as f64 / total}),
            );
        }
    }

    /// 拉到窄版（herdr 允许的最窄）或拉回宽版（上次的宽度，至少 width）。画哪一版按窗格实际宽度定。
    fn toggle(&mut self, width: u16) {
        if width >= WIDE_MIN {
            self.last_wide = Some(width as i64);
            self.set_width(1);
        } else if let Some((tw, sw)) = self.widths() {
            let total = tw + sw;
            let cols = (self.last_wide.unwrap_or(0) + FRAME).max((total as f64 * self.cfg.width).round() as i64);
            let cols = cols.min(total - crate::CLAUDE_MIN - FRAME);
            if cols >= WIDE_MIN as i64 + FRAME {
                self.set_width(cols);
            } else {
                self.say(
                    t("放不下宽版：要给 Claude 留 {n} 列").replace("{n}", &crate::CLAUDE_MIN.to_string()),
                    Duration::from_secs(6),
                );
            }
        }
    }

    fn say(&mut self, msg: String, secs: Duration) {
        self.flash = Some((msg, Instant::now() + secs));
    }

    fn save_reg(&self) {
        let _ = std::fs::create_dir_all(crate::run_dir());
        let reg = json!({"sidebar": self.me, "target": self.target, "pid": std::process::id()});
        let _ = std::fs::write(
            crate::run_dir().join(format!("{}.json", self.me.replace(':', "_"))),
            reg.to_string(),
        );
    }

    /// 点了一轮：跳转放在线程里，主线程照常画；新的一次点击让旧的那次停下。
    fn jump(&mut self, idx: usize, tx: &Sender<Msg>) {
        let Some(tr) = &self.tr else { return };
        let stops = tr.visible();
        let st = stops[idx];
        let id = st.id.clone();
        let token = self.jump_token.fetch_add(1, Ordering::SeqCst) + 1;
        let term = HerdrTerm {
            pane: self.target.clone(),
            height: herdr::pane_info(&self.target)
                .and_then(|i| i["scroll"]["viewport_rows"].as_u64())
                .unwrap_or(40) as usize,
        };
        // agent 还在干的那一轮没有最终回复，拿半截的文字去找会一直对不上：最新的东西本来就在最底下
        if idx + 1 == stops.len() && self.busy() {
            term.send(&["C-End"]);
            return;
        }
        if jump::needle(&st.reply).is_empty() {
            self.say(t("轨迹：这一轮 Claude 没写文字回复，没法定位"), Duration::from_secs(6));
            return;
        }
        if self.missing.contains(&id) {
            self.say(
                t("轨迹：这一轮在主界面里已经看不到了（续接的会话只显示最近一段，或被 /compact 清掉）"),
                Duration::from_secs(6),
            );
            return;
        }
        let replies: Vec<String> = stops.iter().map(|s| s.reply.clone()).collect();
        let ids: Vec<String> = stops.iter().map(|s| s.id.clone()).collect();
        let (tx, cur) = (tx.clone(), self.jump_token.clone());
        let (sid, target, n, of) = (self.sid.clone(), self.target.clone(), idx + 1, stops.len());
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let refs: Vec<&str> = replies.iter().map(String::as_str).collect();
            let out = jump::jump(&term, &refs, idx, &|| cur.load(Ordering::SeqCst) != token);
            let result = match &out {
                Outcome::Found => "found".to_owned(),
                Outcome::Cancelled => "cancelled".to_owned(),
                Outcome::Timeout => {
                    let _ = tx.send(Msg::Flash(t("轨迹：这一轮没找着，先停在这儿了"), Duration::from_secs(6)));
                    "timeout".to_owned()
                }
                Outcome::Gone(top) => {
                    let gone: Vec<String> = match top {
                        Some(k) => ids[..*k].to_vec(),
                        None => vec![ids[idx].clone()],
                    };
                    let _ = tx.send(Msg::Jumped(gone.join(","), "gone".into()));
                    let _ = tx.send(Msg::Flash(
                        t("轨迹：主界面里找不到这一轮（续接的会话只显示最近一段，或被 /compact 清掉）"),
                        Duration::from_secs(6),
                    ));
                    "not-found".to_owned()
                }
            };
            if out == Outcome::Found {
                let _ = tx.send(Msg::Jumped(ids[idx].clone(), "found".into()));
            }
            let log = json!({"at": chrono::Local::now().format("%F %T").to_string(), "pane": target, "sid": sid,
                             "turn": n, "of": of, "result": result, "secs": (t0.elapsed().as_secs_f64() * 1000.0).round() / 1000.0});
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(crate::state_dir().join("jump.log"))
            {
                let _ = writeln!(f, "{log}");
            }
        });
    }
}

fn spawn_threads(tx: &Sender<Msg>, target: &str, me: &str) {
    let input = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });
    // 订阅这个 Claude 的状态变化、窗格关闭。pane.updated 不订：不能按窗格过滤，别的插件会一直把这里叫醒
    let events = tx.clone();
    let subs = json!([{"type": "pane.agent_status_changed", "pane_id": target}, {"type": "pane.closed"}, {"type": "pane.exited"}]);
    std::thread::spawn(move || {
        if let Ok(it) = herdr::subscribe(subs) {
            for ev in it {
                if events.send(Msg::Herdr(ev)).is_err() {
                    return;
                }
            }
        }
        let _ = events.send(Msg::HerdrGone);
    });
    let cmds = tx.clone();
    let path = sock_of(me);
    let _ = std::fs::remove_file(&path);
    if let Ok(l) = UnixListener::bind(&path) {
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                for line in BufReader::new(s).lines().map_while(Result::ok) {
                    let _ = cmds.send(Msg::Command(line));
                }
            }
        });
    }
}

pub fn run(cfg: Config) {
    let (Ok(target), Ok(me)) = (std::env::var("STATETRAIL_TARGET"), std::env::var("HERDR_PANE_ID")) else {
        eprintln!("statetrail pane: STATETRAIL_TARGET / HERDR_PANE_ID not set");
        std::process::exit(2);
    };
    let mut s = Trail {
        target: target.clone(),
        me: me.clone(),
        cfg,
        sid: None,
        cfg_dir: claude::default_cfg(),
        tr: None,
        status: None,
        opened: None,
        expanded: false,
        missing: HashSet::new(),
        flash: None,
        offset: 0,
        follow: true,
        no_claude_since: None,
        jump_token: Arc::new(AtomicUsize::new(0)),
        last_wide: None,
        hits: Vec::new(),
    };
    s.save_reg();
    // 右键固定交给 herdr 的菜单：轨迹自己不用右键
    let _ = herdr::call("pane.input.set", json!({"pane_id": me, "right_click": "herdr"}));
    let (tx, rx) = mpsc::channel();
    spawn_threads(&tx, &target, &me);
    s.refresh();

    let mut out = std::io::stdout();
    let _ = terminal::enable_raw_mode();
    let _ = execute!(out, terminal::EnterAlternateScreen, event::EnableMouseCapture);
    let mut term = Terminal::new(CrosstermBackend::new(out)).expect("terminal");
    let _ = run_loop(&mut s, &mut term, &tx, &rx);
    let _ = execute!(term.backend_mut(), event::DisableMouseCapture, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    s.jump_token.fetch_add(1, Ordering::SeqCst);
    for p in [crate::run_dir().join(format!("{}.json", me.replace(':', "_"))), sock_of(&me)] {
        let _ = std::fs::remove_file(p);
    }
}

fn run_loop(
    s: &mut Trail,
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    tx: &Sender<Msg>,
    rx: &Receiver<Msg>,
) -> std::io::Result<()> {
    let mut due: Vec<Instant> = Vec::new(); // 预约的补刷时间
    let mut next_tick = Instant::now() + TICK;
    let mut next_check = Instant::now() + AGENT_CHECK;
    loop {
        // 画一帧
        let size = term.size()?;
        let narrow = size.width < WIDE_MIN;
        if s.flash.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
            s.flash = None;
        }
        {
            let empty = Vec::new();
            let stops = s.tr.as_ref().map(|t| t.visible()).unwrap_or_default();
            let stops = if s.tr.is_some() { &stops } else { &empty };
            let view = ui::View {
                stops,
                narrow,
                expanded: s.expanded,
                opened: s.opened.as_deref(),
                busy: s.busy(),
                missing: &s.missing,
                flash: s.flash.as_ref().map(|(m, _)| m.as_str()),
                found: s.tr.is_some(),
            };
            let items = view.items();
            let room = size.width.saturating_sub(2) as usize;
            let rows = size.height.saturating_sub(2) as usize;
            let bottom = ui::bottom_offset(&view, &items, room, rows);
            if s.follow {
                s.offset = bottom;
            }
            s.offset = s.offset.min(bottom);
            let mut hits = Vec::new();
            term.draw(|f| hits = ui::draw(f, &view, s.offset).0)?;
            s.hits = hits;
        }

        // 等下一件事
        let now = Instant::now();
        let mut wake = next_tick.min(next_check);
        if let Some(d) = due.iter().min() {
            wake = wake.min(*d);
        }
        if let Some((_, until)) = &s.flash {
            wake = wake.min(*until);
        }
        let msg = rx.recv_timeout(wake.saturating_duration_since(now));
        let mut stale = false;
        match msg {
            Ok(Msg::Input(Event::Mouse(m))) => match m.kind {
                MouseEventKind::ScrollUp => {
                    s.offset = s.offset.saturating_sub(WHEEL);
                    s.follow = false;
                }
                MouseEventKind::ScrollDown => {
                    s.offset += WHEEL; // 画的时候按最底夹住；到底了就重新跟着走
                    s.follow = false;
                    let tr_len = s.tr.as_ref().map_or(0, |t| t.visible().len());
                    if s.offset >= tr_len {
                        s.follow = true;
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(&(_, it)) = s.hits.iter().find(|(row, _)| *row == m.row) {
                        match it {
                            Item::Fold => s.expanded = !s.expanded,
                            Item::Stop(i) => {
                                let id = s.tr.as_ref().map(|t| t.visible()[i].id.clone());
                                s.opened = id;
                                s.jump(i, tx);
                            }
                        }
                    }
                }
                _ => {}
            },
            Ok(Msg::Input(Event::Resize(..))) => {}
            Ok(Msg::Input(_)) => continue,
            Ok(Msg::Herdr(ev)) => {
                let name = ev["event"].as_str().unwrap_or("").replace('.', "_");
                let pane = ev["data"]["pane_id"].as_str().unwrap_or("");
                if matches!(name.as_str(), "pane_closed" | "pane_exited") && pane == s.target {
                    return Ok(());
                }
                if name == "pane_agent_status_changed" {
                    stale = true;
                    due.extend([now + Duration::from_millis(500), now + Duration::from_millis(1500)]);
                }
                if name.is_empty() && ev.get("error").is_some() {
                    stale = true;
                }
            }
            Ok(Msg::HerdrGone) => return Ok(()), // herdr server 没了
            Ok(Msg::Command(c)) => {
                if c.trim() == "toggle" {
                    s.toggle(size.width);
                }
            }
            Ok(Msg::Flash(m, d)) => s.say(m, d),
            Ok(Msg::Jumped(ids, how)) => {
                for id in ids.split(',') {
                    if how == "found" {
                        s.missing.remove(id);
                    } else {
                        s.missing.insert(id.to_owned());
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + TICK;
            stale = true;
        }
        if now >= next_check {
            next_check = now + AGENT_CHECK;
            if s.no_claude_since.is_none() && herdr::pane_info(&s.target).is_none_or(|i| i["agent"].as_str() != Some("claude")) {
                stale = true;
            }
        }
        if due.iter().any(|d| *d <= now) {
            due.retain(|d| *d > now);
            stale = true;
        }
        if stale {
            if !s.refresh() {
                return Ok(());
            }
            if s.no_claude_since.is_some_and(|t0| t0.elapsed() > GONE_GRACE) {
                return Ok(()); // 这一格里已经没有 Claude 了：真正关掉
            }
            if s.no_claude_since.is_some() {
                due.push(now + GONE_GRACE + Duration::from_millis(500));
            }
        }
    }
}
