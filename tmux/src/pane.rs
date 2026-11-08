//! 轨迹窗格本身（`statetrail-tmux pane <Claude 窗格>`）。主线程只在一个通道上等：键盘鼠标、命令口（常驻进程、
//! 表头按钮）、后台线程（跳转、查额度、数 git）各往里送。对话记录和状态文件每秒 stat 一次（变了才读新追加的部分），
//! 不调 tmux；谁在被看由常驻进程告诉它。对应 herdr 版的 pane.rs。

use crossterm::event::{self, Event, MouseButton, MouseEventKind};
use crossterm::{execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::header::{self, Button};
use crate::i18n::t;
use crate::jump::{self, Outcome, Term};
use crate::transcript::Transcript;
use crate::ui::{self, Item};
use crate::{Config, WIDE_MIN, claude, open, render, tmux};

const POLL_SEEN: Duration = Duration::from_secs(1); // 看得见时多久 stat 一次对话记录和状态文件
const POLL_IDLE: Duration = Duration::from_secs(5); // 没人看时
const TICK: Duration = Duration::from_secs(60); // 额度旧了没有、倒计时
const GIT_EVERY: Duration = Duration::from_secs(10); // Git 按钮后面的数字多久数一次（看得见时）

enum Msg {
    Input(Event),
    Command(String),
    Flash(String, Duration),
    Jumped(String, String), // (轮次 id，用逗号连着, 结果)
    Git(Option<usize>),
    Wake,
}

pub fn sock_of(pane: &str) -> PathBuf {
    crate::run_dir().join(format!("{pane}.sock"))
}

/// 给开着的轨迹发一条命令（常驻进程用）。发不出去（轨迹刚起来、已经退了）返回 false。
pub fn send_command(pane: &str, cmd: &str) -> bool {
    UnixStream::connect(sock_of(pane)).and_then(|mut s| writeln!(s, "{cmd}")).is_ok()
}

struct TmuxTerm {
    pane: String,
    height: usize,
}

impl Term for TmuxTerm {
    fn screen(&self) -> String {
        tmux::capture(&self.pane)
    }
    fn send(&self, keys: &[&str]) {
        tmux::send_keys(&self.pane, keys);
    }
    fn height(&self) -> usize {
        self.height
    }
}

struct Trail {
    target: String,
    me: String,
    cfg: Config,
    st: tmux::Status,
    sid: Option<String>,
    cfg_dir: PathBuf,
    tr: Option<Transcript>,
    tr_size: Option<u64>,
    state: Option<String>, // 状态文件里这个 Claude 的状态：running / blocked / input …
    opened: Option<String>,
    expanded: bool,
    missing: HashSet<String>,
    flash: Option<(String, Instant)>,
    offset: usize,
    follow: bool, // 停在最底：列表变长时跟着走
    seen: bool,   // 这个窗口正被看着（常驻进程说的）
    focused: bool, // 焦点在这个 Claude / 轨迹上（只有这时才去读 Claude 的屏幕）
    cwd: String,  // Claude 的目录（Git 按钮后面的数字数这里）
    git: Option<usize>,
    next_git: Instant,
    jump_token: Arc<AtomicUsize>,
    jumping_until: Option<Instant>,
    hits: Vec<(u16, Item)>,
    header_len: u16,
    rows: Vec<Vec<Button>>, // 表头最上面的按钮（宽版一行，窄版两行）
    head_ln: usize,         // 宽版「N 轮」那一行（点它什么都不做）
    mc: (Option<u64>, (String, String)),
    last_kick: Option<Instant>,
    view: (usize, usize, usize),
    next_sid_check: Instant,
}

impl Trail {
    fn busy(&self) -> bool {
        matches!(self.state.as_deref(), Some("running" | "blocked"))
    }

    /// 状态文件、对话记录变了没有：变了就读（对话记录只读新追加的部分）。返回有没有变。
    fn refresh(&mut self) -> bool {
        let mut changed = false;
        if self.st.reload() {
            let state = self.st.state(&self.target);
            changed |= state != self.state;
            self.state = state;
        }
        let sid = self.st.session(&self.target).or_else(|| {
            // hook 还没写过这个会话：按 Claude 进程认（要起 ps，隔一会儿才试一次）
            if self.sid.is_none() && Instant::now() >= self.next_sid_check {
                self.next_sid_check = Instant::now() + Duration::from_secs(5);
                tmux::session_of(&self.target, &self.st)
            } else {
                self.sid.clone()
            }
        });
        if sid != self.sid {
            // /clear、/resume、换了账号重开：换了会话
            self.sid = sid;
            self.tr = None;
            self.tr_size = None;
            self.opened = None;
            self.expanded = false;
            self.missing.clear();
            self.follow = true;
            self.rescan();
            changed = true;
        }
        if let (Some(sid), None) = (&self.sid, &self.tr)
            && let Some(p) = claude::transcript_path(sid, &self.cfg_dir)
        {
            self.tr = Some(Transcript::new(&p));
        }
        if let Some(tr) = &mut self.tr {
            let size = std::fs::metadata(&tr.path).map(|m| m.len()).ok();
            if size != self.tr_size {
                self.tr_size = size;
                tr.update();
                changed = true;
            }
        }
        changed
    }

    /// 按 Claude 进程重认配置目录（账号、额度跟着变）
    fn rescan(&mut self) {
        self.cfg_dir = claude::config_dir_of_pid(tmux::claude_pid(&self.target));
    }

    fn model_ctx(&mut self) -> (String, String) {
        let Some(tr) = &self.tr else {
            return (String::new(), String::new());
        };
        if self.tr_size != self.mc.0 {
            self.mc = (self.tr_size, claude::model_ctx(&tr.path));
        }
        self.mc.1.clone()
    }

    /// 额度旧了（默认 30 分钟）就在后台去查一次；force = 点了 ⟳。usage-refresh.py 自己还会再节流一道。
    fn kick_usage(&mut self, force: bool, tx: &Sender<Msg>) {
        if !self.cfg.live_usage {
            if force {
                self.say(t("额度只读现有读数；要实时查询，在配置里设 live_usage = true"), Duration::from_secs(8));
            }
            return;
        }
        if !force && (!claude::usage_stale(&self.cfg_dir) || self.last_kick.is_some_and(|k| k.elapsed() < Duration::from_secs(60))) {
            return;
        }
        self.last_kick = Some(Instant::now());
        let (cfg, tx) = (self.cfg_dir.clone(), tx.clone());
        std::thread::spawn(move || {
            claude::refresh_usage(&cfg, force);
            let _ = tx.send(Msg::Wake);
        });
    }

    /// Git 按钮后面的数字：这个目录里没提交的文件数（git status --porcelain 的行数），后台数
    fn kick_git(&mut self, tx: &Sender<Msg>) {
        if self.cwd.is_empty() || Instant::now() < self.next_git {
            return;
        }
        self.next_git = Instant::now() + GIT_EVERY;
        let (cwd, tx) = (self.cwd.clone(), tx.clone());
        std::thread::spawn(move || {
            let n = std::process::Command::new("git")
                .args(["-C", &cwd, "status", "--porcelain"])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| o.stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count());
            let _ = tx.send(Msg::Git(n));
        });
    }

    fn say(&mut self, msg: String, secs: Duration) {
        self.flash = Some((msg, Instant::now() + secs));
    }

    fn act(&mut self, act: &str, tx: &Sender<Msg>) {
        match act {
            "bottom" => {
                self.jump_token.fetch_add(1, Ordering::SeqCst);
                tmux::send_keys(&self.target, &["C-End"]);
                self.follow = true;
                self.opened = self.tr.as_ref().and_then(|tr| tr.visible().last().map(|s| s.id.clone()));
            }
            "usage" => {
                self.rescan(); // ⟳ 也重认账号
                self.kick_usage(true, tx);
            }
            "git" => {
                if let Some(m) = open::git_button(&self.target, &self.cfg) {
                    self.say(m, Duration::from_secs(5));
                }
            }
            "files" => open::spawn(&self.cfg.scripts.join("float-tool.sh"), &[&self.target, "files"]),
            "close" => open::close(&self.target), // 连同自己一起关掉；记下是你关的，常驻进程不再自动开
            _ => {}
        }
    }

    /// 点表头：最上面几行是按钮；宽版「N 轮」那一行什么都不做；模型、柱子那几行点了去查一次真实额度。
    fn header_click(&mut self, ln: usize, col: usize, narrow: bool, tx: &Sender<Msg>) {
        if let Some(row) = self.rows.get(ln.wrapping_sub(1)) {
            if let Some(a) = header::button_at(row, col).map(str::to_owned) {
                self.act(&a, tx);
            }
        } else if narrow || ln != self.head_ln {
            self.kick_usage(true, tx);
        }
    }

    /// 点了一轮：跳转放在线程里，主线程照常画；新的一次点击让旧的那次停下。
    fn jump(&mut self, idx: usize, tx: &Sender<Msg>) {
        let Some(tr) = &self.tr else { return };
        let stops = tr.visible();
        let st = stops[idx];
        let id = st.id.clone();
        let token = self.jump_token.fetch_add(1, Ordering::SeqCst) + 1;
        let term = TmuxTerm {
            pane: self.target.clone(),
            height: tmux::display(&self.target, "#{pane_height}")
                .and_then(|h| h.parse().ok())
                .unwrap_or(40),
        };
        // 还在干的那一轮没有最终回复，拿半截的文字去找会一直对不上：最新的东西本来就在最底下
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
        self.jumping_until = Some(Instant::now() + Duration::from_secs(7)); // 翻屏最多 6 秒
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let refs: Vec<&str> = replies.iter().map(String::as_str).collect();
            let out = jump::jump(&term, &refs, idx, &|| cur.load(Ordering::SeqCst) != token);
            let result = match &out {
                Outcome::Found => {
                    let _ = tx.send(Msg::Jumped(ids[idx].clone(), "found".into()));
                    "found"
                }
                Outcome::Cancelled => "cancelled",
                Outcome::Timeout => {
                    let _ = tx.send(Msg::Flash(t("轨迹：这一轮没找着，先停在这儿了"), Duration::from_secs(6)));
                    "timeout"
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
                    "not-found"
                }
            };
            let log = serde_json::json!({"at": chrono::Local::now().format("%F %T").to_string(), "pane": target, "sid": sid,
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

    /// 常驻进程发来的「这个窗口有没有人在看、焦点在不在这一对上、Claude 在哪个目录」：flags 两位，看得见 / 焦点在这
    fn set_state(&mut self, flags: &str, cwd: &str) {
        self.seen = flags.starts_with('1');
        self.focused = flags.get(1..2) == Some("1");
        if cwd != self.cwd {
            self.cwd = cwd.to_owned();
            self.next_git = Instant::now();
        }
    }
}

/// 表头最上面的按钮：宽版一行（⇊ 到最底、Git + 没提交的文件数、文件、⟳ + 额度多新、✕ 关掉），
/// 窄版放不下，分两行（⇊ ⟳ ✕ / Git 文件）。每个按钮占 3 格，点偏一格也算。
fn button_rows(narrow: bool) -> Vec<Vec<Button>> {
    let b = |name: &str, act: &str, extra: usize| (header::icon(name), act.to_owned(), extra);
    let close = (if header::text_tier() { "x".into() } else { "✕".into() }, "close".to_owned(), 0);
    if narrow {
        vec![
            vec![b("bottom", "bottom", 0), b("usage", "usage", 0), close],
            vec![b("git", "git", 0), b("files", "files", 0)],
        ]
    } else {
        vec![vec![b("bottom", "bottom", 0), b("git", "git", 4), b("files", "files", 0), b("usage", "usage", 10), close]]
    }
}

fn spawn_threads(tx: &Sender<Msg>, me: &str) {
    let input = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });
    let cmds = tx.clone();
    let _ = std::fs::create_dir_all(crate::run_dir());
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

/// STATETRAIL_DEBUG=1 时往状态目录的 debug.log 记一行（开发用）
fn debug(line: &str) {
    if std::env::var_os("STATETRAIL_DEBUG").is_some()
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(crate::state_dir().join("debug.log"))
    {
        let _ = writeln!(f, "{} {line}", chrono::Local::now().format("%T%.3f"));
    }
}

pub fn run(cfg: Config, target: &str) {
    let _ = event::poll(Duration::ZERO); // 先装上窗口大小变化的信号处理
    let me = std::env::var("TMUX_PANE").unwrap_or_default();
    if target.is_empty() || me.is_empty() {
        eprintln!("statetrail-tmux pane <claude-pane>: run inside tmux");
        std::process::exit(2);
    }
    // 起来时自己问一次「有没有人在看、Claude 在哪个目录」，之后由常驻进程告诉它
    let probe = tmux::display(target, "#{window_active} #{session_attached} #{pane_active} #{pane_current_path}").unwrap_or_default();
    let mut f = probe.splitn(4, ' ');
    let (wa, att, pa, cwd) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
    let seen = wa == "1" && !matches!(att, "" | "0");
    let mut s = Trail {
        target: target.to_owned(),
        me: me.clone(),
        cfg,
        st: tmux::Status::default(),
        sid: None,
        cfg_dir: claude::default_cfg(),
        tr: None,
        tr_size: None,
        state: None,
        opened: None,
        expanded: false,
        missing: HashSet::new(),
        flash: None,
        offset: 0,
        follow: true,
        seen,
        focused: seen && pa == "1",
        cwd: cwd.to_owned(),
        git: None,
        next_git: Instant::now(),
        jump_token: Arc::new(AtomicUsize::new(0)),
        jumping_until: None,
        hits: Vec::new(),
        header_len: 0,
        rows: Vec::new(),
        head_ln: 0,
        mc: (None, (String::new(), String::new())),
        last_kick: None,
        view: (0, 0, 0),
        next_sid_check: Instant::now(),
    };
    let (tx, rx) = mpsc::channel();
    spawn_threads(&tx, &me);
    s.refresh();
    s.kick_usage(false, &tx);

    let mut out = std::io::stdout();
    let _ = terminal::enable_raw_mode();
    let _ = execute!(out, terminal::EnterAlternateScreen, event::EnableMouseCapture);
    let mut term = Terminal::new(CrosstermBackend::new(out)).expect("terminal");
    let _ = run_loop(&mut s, &mut term, &tx, &rx);
    let _ = execute!(term.backend_mut(), event::DisableMouseCapture, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    s.jump_token.fetch_add(1, Ordering::SeqCst);
    let _ = std::fs::remove_file(sock_of(&me));
}

fn run_loop(
    s: &mut Trail,
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    tx: &Sender<Msg>,
    rx: &Receiver<Msg>,
) -> std::io::Result<()> {
    let mut next_tick = Instant::now() + TICK;
    let mut next_poll = Instant::now();
    let mut redraw = true;
    loop {
        let size = term.size()?;
        let narrow = size.width < WIDE_MIN;
        if s.flash.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
            s.flash = None;
        }
        if std::mem::replace(&mut redraw, true) {
            let (model, ctx) = s.model_ctx();
            let usage = claude::read_usage(&s.cfg_dir, s.sid.as_deref(), s.cfg.live_usage);
            let stops = s.tr.as_ref().map(|t| t.visible()).unwrap_or_default();
            let none = HashSet::new();
            let view = ui::View {
                stops: &stops,
                narrow,
                expanded: s.expanded,
                opened: s.opened.as_deref(),
                busy: s.busy(),
                missing: if s.missing.is_empty() { &none } else { &s.missing },
                chapters: &[],
                open: &[],
                query: "",
                outline: false,
            };
            let items = view.items();
            let room = size.width.saturating_sub(2) as usize;
            s.rows = button_rows(narrow);
            let info = header::Info {
                cfg_dir: &s.cfg_dir,
                model: &model,
                ctx: &ctx,
                usage: usage.as_ref(),
                found: s.tr.is_some(),
                turns: stops.len(),
                gone: view.gone(),
                flash: s.flash.as_ref().map(|(m, _)| m.as_str()),
                up_tag: "",
                slot: 0,
                buttons: Some(&s.rows),
                git: s.git,
            };
            let head = if narrow {
                header::narrow(size.width as usize, &info)
            } else {
                let (lines, head_ln) = header::wide(size.width as usize, &info);
                s.head_ln = head_ln;
                lines
            };
            let rows = (size.height as usize).saturating_sub(head.len() + 1);
            let bottom = ui::bottom_offset(&view, &items, room, rows);
            if s.follow {
                s.offset = bottom;
            }
            s.offset = s.offset.min(bottom);
            s.view = ui::view_counts(&view, &ui::visible(&view, &items, s.offset, rows, room));
            let footer: Vec<render::SLine> = Vec::new();
            let mut drawn = None;
            term.draw(|f| {
                drawn = Some(ui::draw(f, &head, &footer, &view, s.offset));
            })?;
            if let Some(d) = drawn {
                s.hits = d.hits;
                s.header_len = d.list_top.saturating_sub(1);
            }
        }

        // 等下一件事
        let now = Instant::now();
        let mut wake = next_tick.min(next_poll);
        if let Some((_, until)) = &s.flash {
            wake = wake.min(*until);
        }
        // 只为 stat 一下醒来、什么都没变时不重画（重画要读额度文件）
        let poll_only = next_poll < next_tick && s.flash.as_ref().is_none_or(|(_, u)| next_poll < *u);
        let msg = rx.recv_timeout(wake.saturating_duration_since(now));
        let timed_out = matches!(msg, Err(mpsc::RecvTimeoutError::Timeout));
        match msg {
            Ok(Msg::Input(Event::Mouse(m))) => match m.kind {
                MouseEventKind::ScrollUp => {
                    s.offset = s.offset.saturating_sub(1);
                    s.follow = false;
                }
                MouseEventKind::ScrollDown => {
                    s.offset += 1;
                    s.follow = s.view.1 <= 1;
                }
                MouseEventKind::Down(MouseButton::Left) if m.row < s.header_len => {
                    // 表头的字从第 3 列开始（左边 2 列和列表的指针对齐）；列号从 1 数
                    s.header_click(m.row as usize + 1, (m.column as usize).saturating_sub(1), narrow, tx);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(&(_, it)) = s.hits.iter().find(|(row, _)| *row == m.row) {
                        match it {
                            Item::Fold => s.expanded = !s.expanded,
                            Item::Stop(i) => {
                                s.opened = s.tr.as_ref().map(|t| t.visible()[i].id.clone());
                                s.jump(i, tx);
                            }
                            Item::Chapter(_) => {}
                        }
                    }
                }
                _ => {
                    redraw = false;
                    continue;
                }
            },
            Ok(Msg::Input(Event::Resize(..))) => {}
            Ok(Msg::Input(_)) => {
                redraw = false;
                continue;
            }
            Ok(Msg::Command(c)) => {
                debug(&format!("{} cmd {c}", s.me));
                let parts: Vec<&str> = c.splitn(3, ' ').collect();
                match parts.as_slice() {
                    ["state", flags, cwd] => s.set_state(flags, cwd),
                    ["state", flags] => s.set_state(flags, ""),
                    ["rescan"] => s.rescan(),
                    ["quit"] => return Ok(()),
                    _ => {}
                }
            }
            Ok(Msg::Flash(m, d)) => s.say(m, d),
            Ok(Msg::Wake) => {}
            Ok(Msg::Git(n)) => {
                if n == s.git {
                    redraw = false;
                    continue;
                }
                s.git = n;
            }
            Ok(Msg::Jumped(ids, how)) => {
                s.jumping_until = None;
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
        if now >= next_poll {
            next_poll = now + if s.seen { POLL_SEEN } else { POLL_IDLE };
            let changed = s.refresh();
            if s.seen {
                s.kick_git(tx);
            }
            if !changed && poll_only && timed_out {
                redraw = false;
            }
        }
        if now >= next_tick {
            next_tick = now + TICK;
            s.kick_usage(false, tx); // 额度旧了就去查（默认 30 分钟）；倒计时也跟着重画
        }
    }
}
