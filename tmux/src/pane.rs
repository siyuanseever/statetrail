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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

use crate::chapters::{self, Chapter, Marks};
use crate::header::{self, Button};
use crate::i18n::t;
use crate::jump::{self, Outcome, Term};
use crate::transcript::Transcript;
use crate::ui::matches as hit;
use crate::ui::{self, Item};
use crate::{Config, WIDE_MIN, claude, open, render, search, tmux};

const POLL_SEEN: Duration = Duration::from_secs(1); // 看得见时多久 stat 一次对话记录和状态文件
const POLL_IDLE: Duration = Duration::from_secs(5); // 没人看时
const TICK: Duration = Duration::from_secs(60); // 额度旧了没有、倒计时
const GIT_EVERY: Duration = Duration::from_secs(10); // Git 按钮后面的数字多久数一次（看得见时）
// 焦点在这一对上、窗口有了新输出时读一次 Claude 的屏幕，过这么久再读一次（有输出的时间只精确到秒，一秒里的后半截会漏）。
// 不定时读：读一次要起一次 tmux 客户端，隔一秒冷启动一次约 10 毫秒 CPU
const FOLLOW_AGAIN: Duration = Duration::from_millis(1200);
const CLASSIFY_RETRY: Duration = Duration::from_secs(600); // 调 Haiku 分章失败了（没网、没额度）：隔这么久再试
const CODE_CHECK: Duration = Duration::from_secs(5); // 多久看一眼程序文件换了没有（只 stat 一下）
const MANUAL_HOLD: Duration = Duration::from_secs(8); // 滚轮 / 键盘挪了选中之后，这么久不跟 Claude 的屏幕

enum Msg {
    Input(Event),
    Command(String),
    Flash(String, Duration),
    Jumped(String, String), // (轮次 id，用逗号连着, 结果)
    Git(Option<usize>),
    Classified(bool), // 后台分完一批章（成功没有）：重读分章文件
    Rescan,           // 换完账号：按新进程重认配置目录
    Indexing(usize, usize),       // 跨会话搜索在建索引：(已读, 总数)
    Indexed(Vec<search::Session>), // 索引建好了
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
    activity: String,   // 窗口最后一次有输出的时间（常驻进程说的）
    follow_due: Vec<Instant>, // 什么时候该读 Claude 的屏幕
    manual_until: Option<Instant>, // 刚用滚轮 / 键盘挪过选中：这之前不跟 Claude 的屏幕（Claude 一输出就会把选中抢回去）
    up_at: Option<(usize, usize)>, // 表头里的 ▲：(第几行, 占到第几列)
    down_at: Option<usize>,        // 底部的 ▼：占到第几列
    foot: Vec<Button>,             // 最底下那一行按钮
    foot_row: usize,               // 按钮在底部的第几行（窄版有 ▼ 时是第 2 行）
    foot_shift: usize,             // 宽版行首给 ▼ 留了位置，按钮往右挪了这么多
    footer_top: u16,
    last_wide: Option<i64>, // 切到窄版之前的宽度：切回宽版时回到它
    marks: Marks,           // 这个会话哪几轮开新章（core 的 chapters）
    chaps: Vec<Chapter>,    // 上一帧切出来的章（点击时按它认）
    chap_open: Vec<bool>,   // 上一帧每一章展没展开
    outline: bool,          // 在章那一层：所有章收着，选中按章挪（← 进来，→ / Enter 出去）
    reveal_top: bool,       // 刚从章那一层进到一章：画的时候把这一章的章名顶到列表最上面
    classifying: bool,      // 正在后台调 Haiku 分章
    next_classify: Instant,
    menu: Option<ui::Menu>, // 开着的右键菜单
    menu_hits: Vec<(u16, u16, u16, String)>,
    search: Option<String>,      // 按 / 或点 ⌕ 打开的搜索：在搜的字（None = 没在搜）
    everywhere: bool,            // 搜索框里按了 Tab：搜本机所有会话（轨迹窗格临时放大到整个窗口）
    index: Option<Vec<search::Session>>, // 跨会话搜索的索引：搜索关掉就扔
    indexing: Option<(usize, usize)>,
    found_sel: usize,
    found_off: usize,
    found_rows: Vec<(u16, usize)>,          // 跨会话结果：屏幕第几行是第几条
    open_sids: HashMap<String, String>,     // 开着的会话：会话 id → Claude 窗格
    goto: Option<(String, Instant)>,        // 别处搜到了这个会话的某一轮（轮次 id）：会话读到它就跳过去
    code: Option<std::time::SystemTime>,     // 起来时程序文件的修改时间
    code_new: Option<std::time::SystemTime>,
    restart: bool,                           // 程序换了：主循环退出后原地换成新程序
    hot: Vec<(usize, usize, usize)>,         // 表头里点了弹账号菜单的那段字（header::hot_spots_after）
    cwd: String,  // Claude 的目录（Git 按钮后面的数字数这里）
    git: Option<usize>,
    next_git: Instant,
    jump_token: Arc<AtomicUsize>,
    jumping_until: Option<Instant>,
    hits: Vec<(u16, Item)>,
    header_len: u16,
    rows: Vec<Vec<Button>>, // 表头最上面的按钮（宽版一行，窄版两行）
    head_ln: usize,         // 宽版「N 轮」那一行（点它什么都不做，除了后面的「/ 搜索」）
    search_at: Option<(usize, usize)>, // 「N 轮」那一行里「/ 搜索」那几个字的列（和 hot 一样从表头文字的第 0 列数）
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
            self.outline = false;
            self.marks = self.sid.as_deref().map(Marks::load).unwrap_or_default();
            // 别的轨迹搜到了这个会话、新开窗口恢复它：读它留下的那一轮（两分钟内的才算）
            if let Some(sid) = &self.sid {
                let f = crate::run_dir().join("goto").join(sid);
                let age = std::fs::metadata(&f).ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok());
                if let (Some(age), Ok(id)) = (age, std::fs::read_to_string(&f)) {
                    if age < Duration::from_secs(120) {
                        self.goto = Some((id.trim().to_owned(), Instant::now()));
                    }
                    let _ = std::fs::remove_file(&f);
                }
            }
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
                if let Some(m) = open::git_button(&self.target) {
                    self.say(m, Duration::from_secs(5));
                }
            }
            "files" => {
                if let Some(m) = open::files_button(&self.target) {
                    self.say(m, Duration::from_secs(5));
                }
            }
            "close" => open::close(&self.target), // 连同自己一起关掉；记下是你关的，常驻进程不再自动开
            "narrow" | "wide" => self.toggle_width(),
            "search" => self.open_search(),
            _ => {}
        }
    }

    /// 窄版（17 列）和宽版之间切：挪的是 Claude 和轨迹之间那条线，上面那条命令行跟着一起变。
    /// 宽版回到上次的宽度，没有就按配置的比例（默认这一对的 34%）。画哪一版按窗格实际宽度定。
    fn toggle_width(&mut self) {
        let Some(w) = tmux::display(&self.me, "#{pane_width}").and_then(|w| w.parse::<i64>().ok()) else { return };
        let cols = if w >= WIDE_MIN as i64 {
            self.last_wide = Some(w);
            crate::NARROW
        } else {
            let total = tmux::display(&self.target, "#{pane_width}")
                .and_then(|x| x.parse::<i64>().ok())
                .unwrap_or(0)
                + w
                + 1;
            self.last_wide.unwrap_or(((total as f64) * self.cfg.width).round() as i64).max(WIDE_MIN as i64)
        };
        let _ = tmux::run(&["resize-pane", "-t", &self.me, "-x", &cols.to_string()]);
    }

    /// 点表头：最上面几行是按钮；宽版「N 轮」那一行什么都不做；模型、柱子那几行点了去查一次真实额度。
    fn header_click(&mut self, ln: usize, col: usize, narrow: bool, tx: &Sender<Msg>) {
        if self.up_at.is_some_and(|(l, slot)| l == ln && col <= slot + 1) {
            self.page(-1); // 点在行首 ▲ 那一段（偏一格也算）
            return;
        }
        if !narrow && ln == self.head_ln && self.search_at.is_some_and(|(a, b)| (a..b + 1).contains(&col.saturating_sub(1))) {
            self.open_search();
            return;
        }
        if let Some(&(i, start, _)) = self.hot.iter().find(|(i, a, b)| *i + 1 == ln && (*a..*b).contains(&col.saturating_sub(1))) {
            self.account_menu((start + 2) as u16, (i + 1) as u16);
            return;
        }
        if let Some(row) = self.rows.get(ln.wrapping_sub(1)) {
            if let Some(a) = header::button_at(row, col).map(str::to_owned) {
                self.act(&a, tx);
            }
        }
        // 点表头别处不再强制查额度（点进轨迹常落在这里，误触会多发请求）：要刷新点 ⟳
    }

    /// 选中往上 / 往下挪几格（滚轮一格一格，↑↓ 也是），列表跟着滚到它。只挪选中，不跳：按 Enter 才让 Claude 跳过去
    /// （herdr 版用户 2026-10-29：「鼠标滚动时选中项上下移动」「enter 来确认跳转」）。
    fn move_sel(&mut self, d: isize) {
        if self.everywhere {
            let n = self.found().len();
            self.found_sel = self.found_sel.saturating_add_signed(d).min(n.saturating_sub(1));
            return;
        }
        let q = self.query();
        let Some(tr) = &self.tr else { return };
        let stops = tr.visible();
        let Some(last) = stops.len().checked_sub(1) else { return };
        // 章那一层一章一格（选中记成它的第一轮）；轮那一层一轮一格，挪进别的章那一章就展开、原来那章收起。
        // 不分章时收着的「已压缩」那几轮挪不进去：列表里没有它们
        let cells: Vec<std::ops::Range<usize>> = if !q.is_empty() {
            (0..=last).filter(|&i| hit(stops[i], &q)).map(|i| i..i + 1).collect()
        } else if self.outline && !self.chaps.is_empty() {
            self.chaps.iter().map(|c| c.start..c.end.min(last + 1)).collect()
        } else if self.chaps.is_empty() && !self.expanded {
            (0..=last).filter(|&i| !stops[i].compacted).map(|i| i..i + 1).collect()
        } else {
            (0..=last).map(|i| i..i + 1).collect()
        };
        if cells.is_empty() {
            return;
        }
        let cur = stops.iter().position(|st| Some(st.id.as_str()) == self.opened.as_deref()).unwrap_or(last);
        let at = cells.iter().position(|r| r.contains(&cur) || r.start > cur).unwrap_or(cells.len() - 1);
        let to = at.saturating_add_signed(d).min(cells.len() - 1);
        self.opened = Some(stops[cells[to].start].id.clone());
        self.reveal = true;
        self.follow = to == cells.len() - 1;
        self.manual_until = Some(Instant::now() + MANUAL_HOLD);
    }

    /// 在章和轮两层之间来回（herdr 版用户 2026-10-29：「在层级之间来回跳转选择」）。← 选中跳到当前这一轮所在的章名，
    /// ↑↓ 按章挪，选中的那一章照样展开；→ / Enter 进到这一章，选中它的第一轮，章名顶到最上面。
    fn outline(&mut self, up: bool) -> bool {
        if self.chaps.is_empty() || self.outline == up {
            return false;
        }
        self.outline = up;
        if up {
            self.reveal = true;
        } else {
            let Some(tr) = &self.tr else { return false };
            let stops = tr.visible();
            let cur = stops.iter().position(|st| Some(st.id.as_str()) == self.opened.as_deref());
            if let Some(c) = cur.and_then(|i| self.chaps.iter().find(|c| (c.start..c.end).contains(&i))) {
                self.opened = stops.get(c.start).map(|st| st.id.clone());
            }
            self.reveal_top = true;
        }
        self.follow = false;
        self.manual_until = Some(Instant::now() + MANUAL_HOLD);
        true
    }

    /// 在搜的字（小写，去掉两头空白）；没在搜是空串
    fn query(&self) -> String {
        self.search.as_deref().map(|q| q.trim().to_lowercase()).unwrap_or_default()
    }

    /// 打开搜索框：焦点给轨迹（点轨迹不抢焦点，打字得在这里），轮那一层
    fn open_search(&mut self) {
        let _ = tmux::run(&["select-pane", "-t", &self.me]);
        self.self_focused = true;
        self.search = Some(String::new());
        self.outline = false;
    }

    /// 搜索框里的字变了：选中最新的那一处命中
    fn pick_hit(&mut self) {
        let q = self.query();
        let Some(tr) = &self.tr else { return };
        if let Some(st) = tr.visible().into_iter().rev().find(|st| hit(st, &q)) {
            self.opened = Some(st.id.clone());
        }
        self.reveal = true;
        self.follow = false;
    }

    /// 搜索框开着时的键：打字搜，↑↓ / 滚轮在命中的轮次里挪，Enter 让 Claude 跳过去并关掉搜索，Esc 关掉搜索（选中留在原处），
    /// Tab 在「这个会话」和「本机所有会话」之间切
    fn search_key(&mut self, code: KeyCode, tx: &Sender<Msg>) -> bool {
        let page = self.view.2.saturating_sub(1).max(1) as isize;
        match code {
            KeyCode::Esc => {
                self.close_search();
                self.focus_claude();
            }
            KeyCode::Tab => self.set_everywhere(!self.everywhere, tx),
            KeyCode::Enter if self.everywhere => self.open_found(self.found_sel, tx),
            KeyCode::Enter => {
                let q = self.query();
                let Some(tr) = &self.tr else { return false };
                let stops = tr.visible();
                let i = stops.iter().position(|st| Some(st.id.as_str()) == self.opened.as_deref());
                if let Some(i) = i.filter(|&i| hit(stops[i], &q)) {
                    self.search = None;
                    self.reveal = true;
                    self.jump(i, tx);
                    self.focus_claude();
                }
            }
            KeyCode::Backspace => {
                if let Some(q) = &mut self.search {
                    q.pop();
                }
                self.pick_hit();
                self.found_sel = 0;
            }
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-page),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::Char(c) => {
                if let Some(q) = &mut self.search {
                    q.push(c);
                }
                self.pick_hit();
                self.found_sel = 0;
            }
            _ => return false,
        }
        true
    }

    /// 关掉搜索：跨会话的话把窗格缩回去、扔掉索引；选中留在原处
    fn close_search(&mut self) {
        if self.everywhere {
            self.set_zoom(false);
        }
        self.everywhere = false;
        self.index = None;
        self.search = None;
        self.reveal = true;
    }

    /// 轨迹窗格临时放大到整个窗口（细条放不下哪个项目、哪个会话）
    fn set_zoom(&self, on: bool) {
        let zoomed = tmux::display(&self.me, "#{window_zoomed_flag}").as_deref() == Some("1");
        if zoomed != on {
            let _ = tmux::run(&["resize-pane", "-Z", "-t", &self.me]);
        }
    }

    /// 跨会话 / 只搜这个会话 来回切（搜索框里按 Tab）。切过去时窗格放大，在后台更新索引
    fn set_everywhere(&mut self, on: bool, tx: &Sender<Msg>) {
        self.everywhere = on;
        self.set_zoom(on);
        if !on {
            self.reveal = true;
            return;
        }
        self.found_sel = 0;
        self.found_off = 0;
        self.st.reload();
        let alive: std::collections::HashSet<String> = tmux::run(&["list-panes", "-a", "-F", "#{pane_id}"])
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        self.open_sids = self
            .st
            .sessions()
            .into_iter()
            .filter(|(p, _)| alive.contains(p))
            .map(|(p, sid)| (sid, p))
            .collect();
        if self.indexing.is_none() {
            self.indexing = Some((0, 0));
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut last = Instant::now();
                let all = search::update(|i, n| {
                    if last.elapsed() > Duration::from_millis(200) {
                        last = Instant::now();
                        let _ = tx.send(Msg::Indexing(i, n));
                    }
                });
                let _ = tx.send(Msg::Indexed(all));
            });
        }
    }

    /// 跨会话搜到的结果
    fn found(&self) -> Vec<(&search::Session, &search::Turn)> {
        self.index.as_deref().map_or_else(Vec::new, |all| search::find(all, &self.query(), 300))
    }

    /// 打开第 k 条结果：是这个会话就直接跳；开在别的窗格里就切过去，让那边的轨迹跳；没开就在这个 tmux 会话里
    /// 新开一个窗口 claude --resume 恢复它，它的轨迹读到那一轮再跳
    fn open_found(&mut self, k: usize, tx: &Sender<Msg>) {
        let Some((sid, id, cwd, cfg, label)) = self.found().get(k).map(|(s, t)| {
            let proj = Path::new(&s.cwd).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            (s.sid.clone(), t.id.clone(), s.cwd.clone(), s.cfg_dir.clone(), s.title.clone().unwrap_or(proj))
        }) else {
            return;
        };
        self.close_search();
        if self.sid.as_deref() == Some(sid.as_str()) {
            let i = self.tr.as_ref().and_then(|tr| tr.visible().iter().position(|st| st.id == id));
            if let Some(i) = i {
                self.opened = Some(id);
                self.reveal = true;
                self.jump(i, tx);
            }
            self.focus_claude();
            return;
        }
        if let Some(pane) = self.open_sids.get(&sid).cloned() {
            let _ = tmux::run(&["switch-client", "-t", &pane]);
            let _ = tmux::run(&["select-window", "-t", &pane]);
            let _ = tmux::run(&["select-pane", "-t", &pane]);
            match open::find_pane(&pane, "@trail_for", &pane) {
                Some(sb) if send_command(&sb, &format!("goto {id}")) => {}
                _ => self.leave_goto(&sid, &id), // 那边还没开轨迹：常驻进程开出来以后读这个
            }
            return;
        }
        self.leave_goto(&sid, &id);
        let session = tmux::display(&self.me, "#{session_name}").unwrap_or_default();
        let mut args: Vec<String> = vec!["new-window".into(), "-P".into(), "-F".into(), "#{pane_id}".into()];
        args.extend(["-t".into(), format!("{session}:"), "-n".into(), label]);
        if Path::new(&cwd).is_dir() {
            args.extend(["-c".into(), cwd]);
        }
        if cfg != claude::default_cfg() {
            args.extend(["-e".into(), format!("CLAUDE_CONFIG_DIR={}", cfg.display())]);
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match tmux::run(&refs) {
            Some(p) => {
                // -n 起的名字只是 Claude 设标题之前的占位：-n 会把这个窗口的自动改名关掉，打开回来，名字跟着 Claude 的终端标题走
                // （用户 2026-11-09：「tmux windows name 怎么不会根据我的claude name 更换了呢」）
                let _ = tmux::run(&["set", "-wu", "-t", p.trim(), "automatic-rename"]);
                tmux::send_keys(p.trim(), &[&format!("claude --resume {sid}"), "Enter"]);
            }
            None => self.say(t("开不了新窗口"), Duration::from_secs(4)),
        }
    }

    /// 记下要跳的那一轮：那个会话的轨迹认出会话后读它（两分钟内有效）
    fn leave_goto(&self, sid: &str, id: &str) {
        let dir = crate::run_dir().join("goto");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(sid), id);
    }

    /// 后台让 Haiku 判断还没判过的话哪几句开新章。只在焦点在这一对上时做：没看的会话不花额度。
    fn classify(&mut self, tx: &Sender<Msg>) {
        if self.cfg.chapters != "auto" || self.classifying || Instant::now() < self.next_classify {
            return;
        }
        let (Some(sid), Some(tr)) = (self.sid.clone(), &self.tr) else { return };
        let stops = tr.visible();
        let new: Vec<usize> = self.marks.pending(&stops).into_iter().take(chapters::BATCH).collect();
        if new.is_empty() {
            return;
        }
        let Some(lock) = chapters::Lock::take() else {
            self.next_classify = Instant::now() + Duration::from_secs(5); // 别的轨迹在调：等一会儿
            return;
        };
        let input = chapters::request(&self.marks, &stops, &new);
        let ids: Vec<(usize, String)> = new.iter().map(|&i| (i, stops[i].id.clone())).collect();
        let (cfg_dir, tx) = (self.cfg_dir.clone(), tx.clone());
        self.classifying = true;
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let r = chapters::ask(&cfg_dir, &input).and_then(|out| chapters::parse(&out, &new));
            chapters::log(&sid, new.len(), &r, t0.elapsed().as_secs_f64());
            if let Ok(found) = &r {
                chapters::merge(&sid, found, &ids);
            }
            drop(lock);
            let _ = tx.send(Msg::Classified(r.is_ok()));
        });
    }

    /// 在一轮 / 一章的章名上右键：从这一轮开新章，或者把这一章并回上一章
    fn chapter_menu(&mut self, it: Item, x: u16, y: u16) {
        let Some(tr) = &self.tr else { return };
        let stops = tr.visible();
        let (i, starts) = match it {
            Item::Chapter(ci) => match self.chaps.get(ci) {
                Some(c) => (c.start, true),
                None => return,
            },
            Item::Stop(i) => (i, self.chaps.iter().any(|c| c.start == i)),
            Item::Fold => return,
        };
        if i == 0 || i >= stops.len() {
            return; // 第一轮总是第一章的开头
        }
        let (key, label) = if starts {
            (format!("chap join {}", stops[i].id), t("并回上一章"))
        } else {
            (format!("chap new {}", stops[i].id), t("这里开新章"))
        };
        self.menu = Some(ui::Menu { x, y, items: vec![(key, label, false)] });
    }

    /// 点表头里的账号名：列出 ~/.config/claude-tmux/accounts.json 里的账号，现在这个打勾
    fn account_menu(&mut self, x: u16, y: u16) {
        let items: Vec<(String, String, bool)> = accounts()
            .into_iter()
            .map(|(name, dir)| (format!("acct {name}"), name, dir == self.cfg_dir))
            .collect();
        if items.len() < 2 {
            self.say(
                t("只配了一个账号，在 {path} 里加上另一个").replace("{path}", "~/.config/claude-tmux/accounts.json"),
                Duration::from_secs(6),
            );
            return;
        }
        self.menu = Some(ui::Menu { x, y, items });
    }

    /// 把这个 Claude 换到另一个账号：交给 claude-tmux-sessions 的 switch_account.py（正在干活、输入框里有没发的字就不动；
    /// 窗口里有队友先让领队收队）。要等 Claude 退出重起，几秒到几十秒，放后台线程里；切换期间写 run/<Claude 窗格>.switch，
    /// 常驻进程看到就不会把 Claude 退出那几秒当成「Claude 没了」收掉轨迹。
    fn switch_account(&mut self, to: String, tx: &Sender<Msg>) {
        let script = self.cfg.scripts.join("switch_account.py");
        if !script.exists() {
            self.say(t("没找到 {path}").replace("{path}", &script.to_string_lossy()), Duration::from_secs(8));
            return;
        }
        let (target, tx) = (self.target.clone(), tx.clone());
        let file = crate::run_dir().join(format!("{target}.switch"));
        let put = move |state: &str, msg: &str| {
            let v = serde_json::json!({"state": state, "msg": msg, "at": chrono::Local::now().timestamp()});
            let tmp = file.with_extension("tmp");
            if std::fs::write(&tmp, v.to_string()).is_ok() {
                let _ = std::fs::rename(&tmp, &file);
            }
        };
        put("running", "");
        self.say(t("正在换…"), Duration::from_secs(60));
        std::thread::spawn(move || {
            let out = std::process::Command::new("python3")
                .arg(&script)
                .args(["--to", &to, "--pane", &target])
                .stdin(std::process::Stdio::null())
                .output();
            // 一行一个窗格：「状态 窗格 说明」
            let line = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default();
            let state = line.split_whitespace().next().unwrap_or("fail");
            let msg = line.split_once(target.as_str()).map_or(line.as_str(), |(_, m)| m.trim()).to_owned();
            put(state, &msg);
            let _ = tx.send(Msg::Rescan);
            let _ = tx.send(Msg::Flash(t("切账号：{msg}").replace("{msg}", &msg), Duration::from_secs(10)));
        });
    }

    /// 菜单里点了一项
    fn apply_menu(&mut self, key: &str, tx: &Sender<Msg>) {
        if let Some(("acct", name)) = key.split_once(' ') {
            let cur = accounts().into_iter().find(|(_, d)| *d == self.cfg_dir).map(|(n, _)| n);
            if cur.as_deref() != Some(name) {
                self.switch_account(name.to_owned(), tx);
            }
            return;
        }
        if let Some(("chap", value)) = key.split_once(' ') {
            // 手动标的记进分章文件，盖过 Haiku 的判断（重新读一遍再写：另一个轨迹可能刚写过）
            if let (Some(sid), Some((how, id))) = (self.sid.clone(), value.split_once(' ')) {
                let mut m = Marks::load(&sid);
                m.manual.insert(id.to_owned(), how == "new");
                m.save(&sid);
                self.marks = m;
            }
            self.focus_claude(); // 右键会把焦点给轨迹（tmux 默认的右键先 select-pane）
        }
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
        if self.search.is_some() && self.menu.is_none() {
            return self.search_key(code, tx);
        }
        let page = self.view.2.saturating_sub(1).max(1) as isize;
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-page),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(isize::MAX / 2),
            KeyCode::Char('/') => self.open_search(),
            KeyCode::Left | KeyCode::Char('h') => return self.outline(true),
            KeyCode::Right | KeyCode::Char('l') => return self.outline(false),
            KeyCode::Enter if self.outline => return self.outline(false),
            KeyCode::Esc if self.menu.is_some() => {
                self.menu = None;
                self.menu_hits.clear();
            }
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
        if self.self_focused || self.search.is_some() || self.jumping_until.is_some_and(|t| now < t) || self.manual_until.is_some_and(|t| now < t) {
            return false;
        }
        let Some(tr) = &self.tr else { return false };
        let screen = tmux::capture(&self.target);
        debug(&format!("{} capture", self.me));
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
    fn set_state(&mut self, flags: &str, activity: &str, cwd: &str) {
        let was = self.focused;
        self.seen = flags.starts_with('1');
        self.focused = flags.get(1..2) == Some("1");
        // 刚切过来、或者窗口有了新输出（Claude 在写、你在 Claude 里翻屏）：读一次屏，过一会儿再读一次
        if self.focused && (!was || activity != self.activity) {
            let now = Instant::now();
            self.follow_due = vec![now, now + FOLLOW_AGAIN];
        }
        self.activity = activity.to_owned();
        self.self_focused = flags.get(2..3) == Some("1");
        if cwd != self.cwd {
            self.cwd = cwd.to_owned();
            self.next_git = Instant::now();
        }
    }
}

/// 按钮的布局和 herdr 插件一样（用户 2026-11-09：「UI按钮的布局呢」）：
/// 最上面一行 « / » 切宽窄、⇊ 到最底、⟳ 刷新（宽版后面带额度多新）；最底下一行 Git、文件。
/// 命令行不做按钮：照旧在轨迹上面切一条（用户 2026-11-09 试过底部按钮后：「我还是喜欢之前的，上边直接打开一个pane来命令行」）。
/// 搜索按 /，关掉点状态栏的轨迹按钮。每个按钮占 3 格，点偏一格也算。
fn button_rows(narrow: bool) -> Vec<Vec<Button>> {
    let b = |name: &str, act: &str, extra: usize| (header::icon(name), act.to_owned(), extra);
    if narrow {
        vec![vec![b("wide", "wide", 0), b("bottom", "bottom", 0), b("usage", "usage", 0)]]
    } else {
        vec![vec![b("narrow", "narrow", 0), b("bottom", "bottom", 0), b("usage", "usage", 18)]]
    }
}

/// 最底下那一行：Git（宽版后面带没提交的文件数）、文件。
fn foot_buttons(narrow: bool) -> Vec<Button> {
    let b = |name: &str, act: &str, extra: usize| (header::icon(name), act.to_owned(), extra);
    vec![b("git", "git", if narrow { 0 } else { 4 }), b("files", "files", 0)]
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

/// ~/.config/claude-tmux/accounts.json 里的账号：(名字, 配置目录)。和 switch_account.py 读的是同一份
fn accounts() -> Vec<(String, PathBuf)> {
    let v: serde_json::Value = std::fs::read(crate::home().join(".config/claude-tmux/accounts.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut out: Vec<(String, PathBuf)> = v["accounts"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, a)| (name.clone(), crate::expand(a["config_dir"].as_str().unwrap_or("~/.claude"))))
        .collect();
    out.sort();
    out
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
        activity: String::new(),
        follow_due: if seen && pa == "1" { vec![Instant::now()] } else { Vec::new() },
        manual_until: None,
        up_at: None,
        down_at: None,
        foot: Vec::new(),
        foot_row: 0,
        foot_shift: 0,
        footer_top: u16::MAX,
        last_wide: None,
        marks: Marks::default(),
        chaps: Vec::new(),
        chap_open: Vec::new(),
        outline: false,
        reveal_top: false,
        classifying: false,
        next_classify: Instant::now(),
        menu: None,
        menu_hits: Vec::new(),
        search: None,
        everywhere: false,
        index: None,
        indexing: None,
        found_sel: 0,
        found_off: 0,
        found_rows: Vec::new(),
        open_sids: HashMap::new(),
        goto: None,
        code: crate::exe_mtime(),
        code_new: None,
        restart: false,
        hot: Vec::new(),
        cwd: cwd.to_owned(),
        git: None,
        next_git: Instant::now(),
        jump_token: Arc::new(AtomicUsize::new(0)),
        jumping_until: None,
        hits: Vec::new(),
        header_len: 0,
        rows: Vec::new(),
        head_ln: 0,
        search_at: None,
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
    if s.everywhere {
        s.set_zoom(false);
    }
    let _ = std::fs::remove_file(sock_of(&me));
    if s.restart {
        crate::restart(); // 原地换成新程序：同一格、同样的参数，位置和宽度都不变
    }
}

fn run_loop(
    s: &mut Trail,
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    tx: &Sender<Msg>,
    rx: &Receiver<Msg>,
) -> std::io::Result<()> {
    let mut next_tick = Instant::now() + TICK;
    let mut next_poll = Instant::now();
    let mut next_code = Instant::now() + CODE_CHECK;
    let mut redraw = true;
    loop {
        let size = term.size()?;
        let narrow = size.width < WIDE_MIN;
        if s.flash.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
            s.flash = None;
        }
        // 有要跳的一轮（别处搜到的）：会话读到它就跳；一分钟还没读到就算了
        if let Some((id, at)) = s.goto.clone() {
            let i = s.tr.as_ref().and_then(|tr| tr.visible().iter().position(|st| st.id == id));
            if let Some(i) = i {
                s.goto = None;
                s.opened = Some(id);
                s.reveal = true;
                s.follow = false;
                s.jump(i, tx);
            } else if at.elapsed() > Duration::from_secs(60) {
                s.goto = None;
            }
        }
        if std::mem::replace(&mut redraw, true) {
            let (model, ctx) = s.model_ctx();
            let usage = claude::read_usage(&s.cfg_dir, s.sid.as_deref(), s.cfg.live_usage);
            let stops = s.tr.as_ref().map(|t| t.visible()).unwrap_or_default();
            let none = HashSet::new();
            let chaps = if s.cfg.chapters == "off" { Vec::new() } else { s.marks.chapters(&stops) };
            // 跨会话搜索时列表那块画结果（draw_found），这个会话的轮次一条都不列
            let nostops: Vec<&crate::transcript::Stop> = Vec::new();
            let listed: &[&crate::transcript::Stop] = if s.everywhere { &nostops } else { &stops };
            let chaps = if s.everywhere { Vec::new() } else { chaps };
            let query = s.query();
            // 同时只展开一章：选中的那一轮所在的章，别的章都收着（herdr 版用户 2026-10-29 定的）。没选中时算最新一轮
            let sel = stops
                .iter()
                .position(|st| Some(st.id.as_str()) == s.opened.as_deref())
                .unwrap_or(stops.len().saturating_sub(1));
            let chap_open: Vec<bool> = chaps.iter().map(|c| (c.start..c.end).contains(&sel) || !query.is_empty()).collect();
            let view = ui::View {
                stops: listed,
                narrow,
                expanded: s.expanded,
                opened: s.opened.as_deref(),
                busy: s.busy(),
                missing: if s.missing.is_empty() { &none } else { &s.missing },
                chapters: &chaps,
                open: &chap_open,
                query: &query,
                outline: s.outline && !chaps.is_empty(),
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
                search_hint: s.search.is_none(),
            };
            let head_len = if narrow {
                header::narrow(size.width as usize, &info).len()
            } else {
                header::wide(size.width as usize, &info).0.len()
            } + usize::from(s.search.is_some());
            // 列表露出来的范围决定 ▲ / ▼：底部一直有一行按钮（上面一条线），列表比窗格长时 ▼ 放在按钮那一行的行首，
            // 窄版放不下，▼ 单独占按钮上面一行；窄版的 ▲ 也多占表头一行。所以最多算两遍
            let mut foot_len = 0;
            let (mut slot, mut counts);
            loop {
                let rows = (size.height as usize).saturating_sub(head_len + 1 + 2 + usize::from(narrow && foot_len > 0) * 2);
                let bottom = ui::bottom_offset(&view, &items, room, rows);
                if s.follow {
                    s.offset = bottom;
                }
                s.offset = s.offset.min(bottom);
                // 选中换了（跟着 Claude 的屏幕、滚轮、键盘）：不在露出来的范围里就滚到刚好整个露出来，一轮一轮挪时列表也一轮一轮走
                // 往上出了范围时连它头上的章名一起露出来；刚进到一章时章名顶到最上面
                let head_of = |k: usize| if k > 0 && matches!(items[k - 1], Item::Chapter(_)) { k - 1 } else { k };
                if s.reveal_top
                    && let Some(k) = items.iter().position(|&it| view.selected(it))
                {
                    s.offset = head_of(k).min(bottom);
                    s.follow = s.offset >= bottom;
                } else if s.reveal
                    && let Some(k) = items.iter().position(|&it| view.selected(it))
                {
                    if k < s.offset {
                        s.offset = head_of(k);
                    }
                    // 在章那一层选中的是章名：连它展开的轮次一起露出来，放不下就章名顶到最上面
                    let mut end = k;
                    if view.outline {
                        while matches!(items.get(end + 1), Some(Item::Stop(_))) {
                            end += 1;
                        }
                    }
                    while s.offset < bottom && s.offset < k && !ui::fits(&view, &items, s.offset, end, rows, room) {
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
            s.reveal_top = false;
            s.view = counts;
            s.chaps = chaps.clone();
            s.chap_open = chap_open.clone();
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
                s.search_at = lines.get(head_ln - 1).and_then(|l| {
                    let p = crate::render::plain(l);
                    let w = t("/ 搜索");
                    p.rfind(w.as_str()).map(|b| (UnicodeWidthStr::width(&p[..b]), UnicodeWidthStr::width(&p[..b + w.len()])))
                });
                lines
            };
            s.up_at = (slot > 0 && counts.0 > 0).then_some((head.len(), slot));
            let accts = [claude::current_account(&s.cfg_dir), claude::short_account(&s.cfg_dir)];
            // 账号名能点（换账号）只在装了 claude-tmux-sessions 的 switch_account.py 时：第一版不带换账号
            s.hot = if crate::script("switch_account.py").is_some() { header::hot_spots_after(&head, &accts, s.rows.len()) } else { Vec::new() };
            let mut head = head;
            let found: Vec<ui::Found> = s
                .found()
                .iter()
                .map(|(ss, tt)| {
                    let proj = Path::new(&ss.cwd).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    // 会话那一行：项目 · 会话名，缺哪个不写；都没有就写会话 id 的前 8 位（不能是空的一行）
                    let mut head: Vec<String> = [Some(proj), ss.title.clone()].into_iter().flatten().filter(|x| !x.is_empty()).collect();
                    if head.is_empty() {
                        head.push(ss.sid.chars().take(8).collect());
                    }
                    ui::Found {
                        group: ss.sid.clone(),
                        head: head.join(" · "),
                        text: tt.prompt.clone(),
                        meta: ui::when(tt.ts.as_deref()),
                        open: s.sid.as_deref() != Some(ss.sid.as_str()) && s.open_sids.contains_key(&ss.sid),
                    }
                })
                .collect();
            // 搜索框：表头最后一行
            if let Some(q) = &s.search {
                let n = if s.everywhere {
                    found.len()
                } else if query.is_empty() {
                    0
                } else {
                    (0..stops.len()).filter(|&i| view.hit(i)).count()
                };
                head.push(ui::search_bar(q, s.everywhere, s.indexing, n, narrow, room));
            }
            // 底部：按钮一行；▼ 宽版放在行首，窄版单独占上面一行（翻到底了位置照样留着，列表不跟着上下跳）
            s.foot = foot_buttons(narrow);
            let buttons = header::buttons_line_with(&s.foot, None, false, s.git);
            let footer: Vec<render::SLine> = if slot == 0 {
                vec![buttons]
            } else if narrow {
                vec![render::lead(Vec::new(), &tag("▼", counts.1), slot), buttons]
            } else {
                vec![render::lead(buttons, &tag("▼", counts.1), slot)]
            };
            let footer: Vec<render::SLine> = footer.into_iter().map(|l| render::clip_line(l, room)).collect();
            s.foot_shift = if slot > 0 && !narrow { slot + 1 } else { 0 };
            s.foot_row = footer.len() - 1;
            s.down_at = (slot > 0 && counts.1 > 0).then_some(slot);
            let mut drawn = None;
            let mut menu_hits = Vec::new();
            let (mut found_rows, mut found_off) = (Vec::new(), s.found_off);
            term.draw(|f| {
                let d = ui::draw(f, &head, &footer, &view, s.offset);
                if s.everywhere {
                    let note = if s.index.is_none() {
                        t("正在读本机的会话记录…")
                    } else if query.is_empty() {
                        t("打字搜本机所有会话里你说过的话")
                    } else {
                        t("没有")
                    };
                    let mut off = s.found_off;
                    let bottom = if footer.is_empty() { d.footer_top } else { d.footer_top.saturating_sub(1) };
                    found_rows = ui::draw_found(f, d.list_top, bottom, &found, s.found_sel, &mut off, &query, &note);
                    found_off = off;
                }
                drawn = Some(d);
                // 能点的账号名垫一层底色
                for &(i, a, b) in &s.hot {
                    ui::mark(f, i as u16, (a + 2) as u16, (b + 2) as u16, false);
                }
                if let Some(m) = &s.menu {
                    menu_hits = ui::draw_menu(f, m, None);
                }
            })?;
            s.menu_hits = menu_hits;
            s.found_rows = found_rows;
            s.found_off = found_off;
            if let Some(d) = drawn {
                s.hits = d.hits;
                s.header_len = d.list_top.saturating_sub(1);
                s.footer_top = d.footer_top;
            }
        }
        // 等下一件事
        let now = Instant::now();
        let mut wake = next_tick.min(next_poll).min(next_code);
        if let Some((_, until)) = &s.flash {
            wake = wake.min(*until);
        }
        if s.focused
            && let Some(d) = s.follow_due.iter().min()
        {
            wake = wake.min(*d);
        }
        // 只为 stat 一下、读一下屏醒来，什么都没变时不重画（重画要读额度文件）
        let poll_only = wake < next_tick && s.flash.as_ref().is_none_or(|(_, u)| wake < *u);
        let msg = rx.recv_timeout(wake.saturating_duration_since(now));
        let timed_out = matches!(msg, Err(mpsc::RecvTimeoutError::Timeout));
        match msg {
            Ok(Msg::Input(Event::Mouse(m))) => match m.kind {
                MouseEventKind::Down(_) if s.menu.is_some() => {
                    // 菜单开着：点中一项就去办，点哪儿都先把菜单收起来
                    let hit = s
                        .menu_hits
                        .iter()
                        .find(|(r, a, b, _)| *r == m.row && (*a..*b).contains(&m.column))
                        .map(|h| h.3.clone());
                    s.menu = None;
                    s.menu_hits.clear();
                    match hit {
                        Some(k) => s.apply_menu(&k, tx),
                        None => s.focus_claude(),
                    }
                }
                MouseEventKind::Down(MouseButton::Right) if s.cfg.chapters != "off" => {
                    if let Some(&(_, it)) = s.hits.iter().find(|(row, _)| *row == m.row) {
                        s.chapter_menu(it, m.column, m.row + 1);
                    }
                    if s.menu.is_none() {
                        s.focus_claude();
                    }
                }
                MouseEventKind::ScrollUp => s.move_sel(-1),
                MouseEventKind::ScrollDown => s.move_sel(1),
                MouseEventKind::Down(MouseButton::Left) if m.row >= s.footer_top => {
                    let (row, col) = ((m.row - s.footer_top) as usize, (m.column as usize).saturating_sub(1));
                    if row == 0 && s.down_at.is_some_and(|slot| col <= slot + 1) {
                        s.page(1); // 点在行首 ▼ 那一段（偏一格也算）
                    } else if row == s.foot_row
                        && let Some(a) = header::button_at(&s.foot, col.saturating_sub(s.foot_shift)).map(str::to_owned)
                    {
                        s.act(&a, tx);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) if m.row < s.header_len => {
                    // 表头的字从第 3 列开始（左边 2 列和列表的指针对齐）；列号从 1 数
                    s.header_click(m.row as usize + 1, (m.column as usize).saturating_sub(1), narrow, tx);
                }
                MouseEventKind::Down(MouseButton::Left) if s.everywhere => {
                    if let Some(&(_, k)) = s.found_rows.iter().find(|(row, _)| *row == m.row) {
                        s.open_found(k, tx);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(&(_, it)) = s.hits.iter().find(|(row, _)| *row == m.row) {
                        match it {
                            Item::Fold => s.expanded = !s.expanded,
                            Item::Stop(i) => {
                                s.opened = s.tr.as_ref().map(|t| t.visible()[i].id.clone());
                                s.jump(i, tx);
                            }
                            Item::Chapter(ci) => {
                                if let (Some(c), Some(tr)) = (s.chaps.get(ci), &s.tr) {
                                    // 轮那一层点展开着的那一章 = 退到章那一层；点别的章、或者在章那一层点章名 = 进到那一章、选中它的第一轮
                                    if !s.outline && s.chap_open.get(ci) == Some(&true) {
                                        s.outline = true;
                                        s.reveal = true;
                                    } else {
                                        s.opened = tr.visible().get(c.start).map(|st| st.id.clone());
                                        s.outline = false;
                                        s.reveal_top = true;
                                    }
                                    s.follow = false;
                                    s.manual_until = Some(Instant::now() + MANUAL_HOLD);
                                }
                            }
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
                let parts: Vec<&str> = c.splitn(4, ' ').collect();
                match parts.as_slice() {
                    // 状态消息本身不重画：重画有输出，窗口「有输出的时间」就变，常驻进程又发一条，每秒都画一遍
                    ["state", flags, act, cwd] => {
                        s.set_state(flags, act, cwd);
                        redraw = false;
                    }
                    ["state", flags, act] => {
                        s.set_state(flags, act, "");
                        redraw = false;
                    }
                    ["rescan"] => s.rescan(),
                    // 别的轨迹的跨会话搜索选中了这个会话的一轮
                    ["goto", id] => s.goto = Some(((*id).to_owned(), Instant::now())),
                    ["quit"] => return Ok(()),
                    _ => {}
                }
            }
            Ok(Msg::Flash(m, d)) => s.say(m, d),
            Ok(Msg::Wake) => {}
            Ok(Msg::Indexing(i, n)) => s.indexing = Some((i, n)),
            Ok(Msg::Indexed(all)) => {
                s.indexing = None;
                if s.everywhere {
                    s.index = Some(all);
                }
            }
            Ok(Msg::Rescan) => s.rescan(),
            Ok(Msg::Classified(ok)) => {
                s.classifying = false;
                if ok {
                    if let Some(sid) = &s.sid {
                        s.marks = Marks::load(sid);
                    }
                } else {
                    s.next_classify = Instant::now() + CLASSIFY_RETRY;
                }
            }
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
        if s.focused && s.follow_due.iter().any(|d| *d <= now) {
            s.follow_due.retain(|d| *d > now);
            changed |= s.follow_screen();
        }
        if s.focused {
            s.classify(tx); // 有没判过的话就去分章（一批分完接着下一批）
        }
        if changed {
            redraw = true;
        } else if poll_only && timed_out {
            redraw = false;
        }
        if now >= next_code {
            next_code = now + CODE_CHECK;
            if crate::code_changed(s.code, &mut s.code_new) {
                s.restart = true;
                return Ok(());
            }
        }
        if now >= next_tick {
            next_tick = now + TICK;
            s.kick_usage(false, tx); // 额度旧了就去查（默认 30 分钟）；倒计时也跟着重画
        }
    }
}
