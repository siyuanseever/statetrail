//! 轨迹窗格本身（`statetrail-tmux pane <Claude 窗格>`）。主线程只在一个通道上等：键盘鼠标、命令口（常驻进程、
//! 表头按钮）、后台线程（跳转、查额度、数 git）各往里送。对话记录和状态文件每秒 stat 一次（变了才读新追加的部分），
//! 不调 tmux；谁在被看由常驻进程告诉它。对应 herdr 版的 pane.rs。

use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
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
use unicode_width::UnicodeWidthStr;

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
// 焦点在这一对 Claude / 轨迹上时，多久读一次 Claude 的屏幕、看它停在哪一轮（herdr 版同一个数）。别的轨迹不读：
// 读一次屏要起一次 tmux（约 3 毫秒）
const FOLLOW_EVERY: Duration = Duration::from_millis(500);
const MANUAL_HOLD: Duration = Duration::from_secs(8); // 滚轮 / 键盘挪了选中之后，这么久不跟 Claude 的屏幕

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
    self_focused: bool, // 焦点在轨迹自己身上：你在用键盘挪选中，不跟 Claude 的屏幕（不然 Claude 一输出就把选中抢回去）
    last_screen: u64,   // 上次读到的 Claude 屏幕（哈希）：没变就不用再认
    reveal: bool,       // 选中的那一轮刚被跟着换了：画的时候把它滚进列表的可见范围
    manual_until: Option<Instant>, // 刚用滚轮 / 键盘挪过选中：这之前不跟 Claude 的屏幕（Claude 一输出就会把选中抢回去）
    up_at: Option<(usize, usize)>, // 表头里的 ▲：(第几行, 占到第几列)
    down_at: Option<usize>,        // 底部的 ▼：占到第几列
    footer_top: u16,
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
        if self.up_at.is_some_and(|(l, slot)| l == ln && col <= slot + 1) {
            self.page(-1); // 点在行首 ▲ 那一段（偏一格也算）
            return;
        }
        if let Some(row) = self.rows.get(ln.wrapping_sub(1)) {
            if let Some(a) = header::button_at(row, col).map(str::to_owned) {
                self.act(&a, tx);
            }
        } else if narrow || ln != self.head_ln {
            self.kick_usage(true, tx);
        }
    }

    /// 选中往上 / 往下挪几格（滚轮一格一格，↑↓ 也是），列表跟着滚到它。只挪选中，不跳：按 Enter 才让 Claude 跳过去
    /// （herdr 版用户 2026-10-29：「鼠标滚动时选中项上下移动」「enter 来确认跳转」）。
    fn move_sel(&mut self, d: isize) {
        let Some(tr) = &self.tr else { return };
        let stops = tr.visible();
        let Some(last) = stops.len().checked_sub(1) else { return };
        // 收着的「已压缩」那几轮挪不进去：列表里没有它们
        let cells: Vec<usize> = (0..=last).filter(|&i| self.expanded || !stops[i].compacted).collect();
        if cells.is_empty() {
            return;
        }
        let cur = stops.iter().position(|st| Some(st.id.as_str()) == self.opened.as_deref()).unwrap_or(last);
        let at = cells.iter().position(|&i| i >= cur).unwrap_or(cells.len() - 1);
        let to = at.saturating_add_signed(d).min(cells.len() - 1);
        self.opened = Some(stops[cells[to]].id.clone());
        self.reveal = true;
        self.follow = to == cells.len() - 1;
        self.manual_until = Some(Instant::now() + MANUAL_HOLD);
    }

    /// 点 ▲ / ▼：翻一屏的轮数，少翻一项留一轮重叠（边上那一轮常常只露出半截）
    fn page(&mut self, d: i32) {
        let step = self.view.2.saturating_sub(1).max(1);
        if d < 0 {
            self.offset = self.offset.saturating_sub(step);
            self.follow = false;
        } else {
            self.offset += step; // 画的时候按最底夹住；下面已经没有了就重新跟着走
            self.follow = self.view.1 <= step;
        }
    }

    /// 键盘（焦点在轨迹上时）：↑↓ / j k 挪一格，PageUp / PageDown 挪一屏，Home / End（g / G）到第一轮 / 最后一轮，
    /// Enter 让 Claude 跳到选中的那一轮，Esc 把焦点还给 Claude。返回 false = 不认识的键，不用重画。
    fn key(&mut self, code: KeyCode, tx: &Sender<Msg>) -> bool {
        let page = self.view.2.saturating_sub(1).max(1) as isize;
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-page),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(isize::MAX / 2),
            KeyCode::Enter => {
                let Some(tr) = &self.tr else { return false };
                let stops = tr.visible();
                if let Some(i) = stops.iter().position(|st| Some(st.id.as_str()) == self.opened.as_deref()) {
                    self.jump(i, tx);
                }
            }
            KeyCode::Esc => {
                self.focus_claude();
                return false;
            }
            _ => return false,
        }
        true
    }

    /// 把焦点还给 Claude（Esc、跳完）
    fn focus_claude(&mut self) {
        let _ = tmux::run(&["select-pane", "-t", &self.target]);
        self.self_focused = false;
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

    /// 读一次 Claude 的屏幕，看它停在哪一轮，把选中挪过去（herdr 版 follow_screen 一样的认法）：在最底就是最新一轮；
    /// 往上翻着就是屏幕上最上面露出开头的那一轮，它的开头已经在屏幕下半截时算上一轮（你看的多半是上一轮的结尾）；
    /// 一轮的开头都没露出来（长回复的中段）就不动。选中换了返回 true。
    fn follow_screen(&mut self) -> bool {
        let now = Instant::now();
        if self.self_focused || self.jumping_until.is_some_and(|t| now < t) || self.manual_until.is_some_and(|t| now < t) {
            return false;
        }
        let Some(tr) = &self.tr else { return false };
        let screen = tmux::capture(&self.target);
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            screen.hash(&mut h);
            h.finish()
        };
        if screen.is_empty() || std::mem::replace(&mut self.last_screen, hash) == hash {
            return false;
        }
        let stops = tr.visible();
        let Some(last) = stops.len().checked_sub(1) else { return false };
        let idx = if jump::at_bottom(&screen) {
            Some(last)
        } else {
            let replies: Vec<&str> = stops.iter().map(|s| s.reply.as_str()).collect();
            let half = screen.lines().count() / 2;
            jump::located_rows(&screen, &jump::needles_of(&replies))
                .into_iter()
                .min_by_key(|&(_, row)| row)
                .map(|(i, row)| if row > half && i > 0 { i - 1 } else { i })
        };
        debug(&format!("{} follow -> {idx:?} (bottom {})", self.me, jump::at_bottom(&screen)));
        let Some(i) = idx else { return false };
        if self.opened.as_deref() == Some(stops[i].id.as_str()) {
            return false;
        }
        self.opened = Some(stops[i].id.clone());
        self.follow = i == last;
        self.reveal = true;
        true
    }

    /// 常驻进程发来的「这个窗口有没有人在看、焦点在不在这一对上、是不是在轨迹自己身上、Claude 在哪个目录」：
    /// flags 三位，看得见 / 焦点在这一对 / 焦点在轨迹自己
    fn set_state(&mut self, flags: &str, cwd: &str) {
        self.seen = flags.starts_with('1');
        self.focused = flags.get(1..2) == Some("1");
        self.self_focused = flags.get(2..3) == Some("1");
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
        self_focused: false,
        last_screen: 0,
        reveal: false,
        manual_until: None,
        up_at: None,
        down_at: None,
        footer_top: u16::MAX,
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
    let mut next_follow = Instant::now();
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
            let mut info = header::Info {
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
            let head_len = if narrow {
                header::narrow(size.width as usize, &info).len()
            } else {
                header::wide(size.width as usize, &info).0.len()
            };
            // 列表露出来的范围决定 ▲ / ▼：列表比窗格长时下面多一行「▼ 5」（窄版的 ▲ 也多占表头一行），所以最多算两遍
            let mut foot_len = 0;
            let (mut slot, mut counts);
            loop {
                let rows = (size.height as usize).saturating_sub(head_len + 1 + foot_len + usize::from(foot_len > 0) + usize::from(narrow && foot_len > 0));
                let bottom = ui::bottom_offset(&view, &items, room, rows);
                if s.follow {
                    s.offset = bottom;
                }
                s.offset = s.offset.min(bottom);
                // 选中换了（跟着 Claude 的屏幕、滚轮、键盘）：不在露出来的范围里就滚到刚好整个露出来，一轮一轮挪时列表也一轮一轮走
                if s.reveal
                    && let Some(k) = items.iter().position(|&it| view.selected(it))
                {
                    if k < s.offset {
                        s.offset = k;
                    }
                    while s.offset < bottom && s.offset < k && !ui::fits(&view, &items, s.offset, k, rows, room) {
                        s.offset += 1;
                    }
                    s.follow = s.offset >= bottom;
                }
                counts = ui::view_counts(&view, &ui::visible(&view, &items, s.offset, rows, room));
                slot = if counts.0 > 0 || counts.1 > 0 {
                    UnicodeWidthStr::width(format!("▼{}{}", if narrow { "" } else { " " }, stops.len()).as_str())
                } else {
                    0
                };
                let want = usize::from(slot > 0);
                if want == foot_len {
                    break;
                }
                foot_len = want;
            }
            s.reveal = false;
            s.view = counts;
            let tag = |arrow: &str, n: usize| {
                if n > 0 {
                    format!("{arrow}{}{n}", if narrow { "" } else { " " })
                } else {
                    String::new()
                }
            };
            let up_tag = tag("▲", counts.0);
            info.up_tag = &up_tag;
            info.slot = slot;
            let head = if narrow {
                header::narrow(size.width as usize, &info)
            } else {
                let (lines, head_ln) = header::wide(size.width as usize, &info);
                s.head_ln = head_ln;
                lines
            };
            s.up_at = (slot > 0 && counts.0 > 0).then_some((head.len(), slot));
            // 底部只有「▼ 5」这一行（翻到底了位置照样留着，列表不跟着上下跳）；一屏放得下就没有底部
            let footer: Vec<render::SLine> = if slot > 0 {
                vec![render::lead(Vec::new(), &tag("▼", counts.1), slot)]
            } else {
                Vec::new()
            };
            s.down_at = (slot > 0 && counts.1 > 0).then_some(slot);
            let mut drawn = None;
            term.draw(|f| {
                drawn = Some(ui::draw(f, &head, &footer, &view, s.offset));
            })?;
            if let Some(d) = drawn {
                s.hits = d.hits;
                s.header_len = d.list_top.saturating_sub(1);
                s.footer_top = d.footer_top;
            }
        }
        // 等下一件事
        let now = Instant::now();
        let mut wake = next_tick.min(next_poll);
        if let Some((_, until)) = &s.flash {
            wake = wake.min(*until);
        }
        if s.focused {
            wake = wake.min(next_follow);
        }
        // 只为 stat 一下、读一下屏醒来，什么都没变时不重画（重画要读额度文件）
        let poll_only = wake < next_tick && s.flash.as_ref().is_none_or(|(_, u)| wake < *u);
        let msg = rx.recv_timeout(wake.saturating_duration_since(now));
        let timed_out = matches!(msg, Err(mpsc::RecvTimeoutError::Timeout));
        match msg {
            Ok(Msg::Input(Event::Mouse(m))) => match m.kind {
                MouseEventKind::ScrollUp => s.move_sel(-1),
                MouseEventKind::ScrollDown => s.move_sel(1),
                MouseEventKind::Down(MouseButton::Left) if m.row >= s.footer_top => {
                    if s.down_at.is_some_and(|slot| (m.column as usize).saturating_sub(1) <= slot + 1) {
                        s.page(1); // 点在行首 ▼ 那一段（偏一格也算）
                    }
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
            Ok(Msg::Input(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                if !s.key(k.code, tx) {
                    redraw = false;
                    continue;
                }
            }
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
        let mut changed = false;
        if now >= next_poll {
            next_poll = now + if s.seen { POLL_SEEN } else { POLL_IDLE };
            changed |= s.refresh();
            if s.seen {
                s.kick_git(tx);
            }
        }
        if s.focused && now >= next_follow {
            next_follow = now + FOLLOW_EVERY;
            changed |= s.follow_screen();
        }
        if !changed && poll_only && timed_out {
            redraw = false;
        }
        if now >= next_tick {
            next_tick = now + TICK;
            s.kick_usage(false, tx); // 额度旧了就去查（默认 30 分钟）；倒计时也跟着重画
        }
    }
}
