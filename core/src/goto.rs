//! 跳到某个 Claude（prefix+g）：herdr 版和 tmux 版共用这一份，宿主只提供「有哪些 Claude、在什么状态、读屏、跳过去」（`Host`）。
//! 原来是 herdr 版的 `statetrail goto`：herdr 自带的 Goto 换成 tmux 版 claude-tmux-picker 的样子和按键
//! （用户 2026-10-30：「这个功能做的跟我在 tmux 里的差很多呀……把交互逻辑对齐一下，UI 也尽量对齐，或者写的更好看」）。
//! 居中的浮窗（herdr 的 popup，宽 95% 高 85%，和 tmux 的 display-popup 一样大），按工作区分组，一行一个 Claude：
//! 行号、状态（● WAIT / ● DONE / ● RUN / ● READ，颜色跟状态栏圆点一样）、名字、这个状态待了多久、它在做什么（开场那句话）。
//! 安静的（READ、做完两小时没看的）默认收起，a 展开；右边是选中那个 Claude 的实时画面，p 收起。
//! h 进到工作区那一层，l 回来；行号直接跳，/ 搜索，Enter 跳过去，q / Esc 关。

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::{execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, UNIX_EPOCH};
use unicode_width::UnicodeWidthStr;

use crate::i18n::t;
use crate::theme::colors;
use crate::transcript::Transcript;
use crate::ui::clip;

const NAME_W: usize = 24;
const AGE_W: usize = 17; // 「完成 16.9小时前」16 格，多一格当间隔
const STALE_SECS: i64 = 2 * 3600; // 做完两小时还没看：变暗、收起（tmux 版 CLAUDE_TMUX_IDLE_STALE_SECS）
const MIN_COLLAPSE: usize = 2; // 只收一个不省地方：「⋯ 收起 1 个」那行正好吃掉省下的一行
const HEAD: u64 = 256 << 10; // 读开头这么多找第一句话
const TAIL: u64 = 256 << 10; // 读末尾这么多找最后一轮
const DIGIT_WAIT: Duration = Duration::from_millis(600);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Wait,
    Done,
    Run,
    Read,
    Stale,
}

impl State {
    fn quiet(self) -> bool {
        matches!(self, State::Read | State::Stale)
    }
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub pane: String,
    pub tab: String,
    pub ws: String,
    pub num: usize,
    pub state: State,
    pub name: String,
    pub task: String,
    pub cwd: String,
    pub age: i64,
    pub reply: String,
    pub sid: Option<String>,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub ws: String,
    pub num: u64,
    pub label: String,
    pub agents: Vec<Agent>,
}

// ---- 宿主 -------------------------------------------------------------------------

/// 宿主报上来的一个 Claude。status：blocked 等你确认 / working 在跑 / done 做完了你还没看 / 其他（闲着、看过了）。
pub struct Seen {
    pub pane: String,
    pub tab: String,
    pub ws: String,
    pub status: String,
    pub sid: Option<String>,
    pub cwd: String,
    pub title: String,
}

/// 一个工作区（herdr 的 workspace，tmux 的 session），按 num 排。
pub struct Space {
    pub ws: String,
    pub num: u64,
    pub label: String,
}

pub trait Host {
    fn spaces(&self) -> Vec<Space>;
    fn agents(&self) -> Vec<Seen>;
    fn go_pane(&self, a: &Agent);
    fn go_ws(&self, ws: &str);
    /// 是从哪个窗格打开的（列表里那一个标出来）
    fn caller(&self) -> Option<String>;
}

// ---- 数据 -------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Info {
    size: u64,
    mtime: i64,
    first: String,
    last_ts: Option<i64>,
    known: bool, // last_ts 是不是真的找过了（末尾没找到、也没往前翻时是 false）
    reply: String,
}

#[derive(Default)]
struct Cache {
    paths: HashMap<String, Option<PathBuf>>,
    info: HashMap<PathBuf, Info>,
}

fn ts_secs(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts).ok().map(|d| d.timestamp())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn oneline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Cache {
    fn path(&mut self, sid: &str) -> Option<PathBuf> {
        self.paths
            .entry(sid.to_owned())
            .or_insert_with(|| crate::claude::transcript_path(sid, &crate::claude::default_cfg()))
            .clone()
    }

    /// 开头的第一句话（不会变，读一次）、最后一轮从什么时候开始、它回了什么。文件没变就不重读。
    /// run = 它正在干活：只有这时才要知道最后一句你说的话是什么时候（「已运行多久」），末尾没有就往前翻。
    fn info(&mut self, path: &PathBuf, run: bool) -> Info {
        let Ok(m) = std::fs::metadata(path) else { return Info::default() };
        let mtime = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        let size = m.len();
        let mut info = match self.info.get(path) {
            Some(i) if i.size == size && (i.known || !run) => return i.clone(),
            Some(i) if i.size == size => i.clone(),
            // 变长了：只读新追加的那段（从上次末尾前一个字节读起，那正是换行，丢掉的第一行就是它）
            Some(i) if i.size < size => {
                let mut i = i.clone();
                let new = Transcript::slice(path, i.size.saturating_sub(1), size);
                if let Some(s) = new.stops.iter().rev().find(|s| s.kind == "human") {
                    i.last_ts = s.ts.as_deref().and_then(ts_secs);
                    i.known = true;
                }
                if let Some(s) = new.visible().last()
                    && !s.reply.is_empty()
                {
                    i.reply = s.reply.clone();
                }
                i
            }
            _ => {
                let first = Transcript::slice(path, 0, HEAD)
                    .stops
                    .iter()
                    .find(|s| s.kind == "human")
                    .map(|s| oneline(&s.prompt))
                    .unwrap_or_default();
                let tr = Transcript::slice(path, size.saturating_sub(TAIL), size);
                let last = tr.stops.iter().rev().find(|s| s.kind == "human");
                Info {
                    first,
                    last_ts: last.and_then(|s| s.ts.as_deref()).and_then(ts_secs),
                    known: last.is_some() || size <= TAIL,
                    reply: tr.visible().last().map(|s| s.reply.clone()).unwrap_or_default(),
                    ..Info::default()
                }
            }
        };
        if run && !info.known {
            // 末尾可能全是工具输出（一轮里读了几 MB）：往前翻，每次翻四倍
            let mut back = TAIL * 4;
            loop {
                let tr = Transcript::slice(path, size.saturating_sub(back), size);
                if let Some(s) = tr.stops.iter().rev().find(|s| s.kind == "human") {
                    info.last_ts = s.ts.as_deref().and_then(ts_secs);
                    break;
                }
                if back >= size {
                    break;
                }
                back *= 4;
            }
            info.known = true;
        }
        info.size = size;
        info.mtime = mtime;
        self.info.insert(path.clone(), info.clone());
        info
    }
}

/// 所有工作区里的 agent，按工作区编号分组；行号在收起之前就排好，展开收起时谁的号都不变。
fn collect(host: &dyn Host, cache: &mut Cache) -> Vec<Group> {
    let mut groups: Vec<Group> = host
        .spaces()
        .into_iter()
        .map(|w| Group {
            ws: w.ws,
            num: w.num,
            label: w.label,
            agents: Vec::new(),
        })
        .collect();
    groups.sort_by_key(|g| g.num);
    let now = now();
    for p in host.agents() {
        let status = p.status.as_str();
        let path = p.sid.as_deref().and_then(|sid| cache.path(sid));
        let info = path.as_ref().map(|path| cache.info(path, status == "working")).unwrap_or_default();
        let since = match status {
            "working" => info.last_ts.unwrap_or(info.mtime),
            _ => info.mtime,
        };
        let age = if since > 0 { (now - since).max(0) } else { -1 }; // -1：找不到对话记录，不知道多久
        let state = match status {
            "blocked" => State::Wait,
            "working" => State::Run,
            "done" if age >= STALE_SECS => State::Stale,
            "done" => State::Done,
            _ => State::Read,
        };
        let mut name = p.title.clone();
        if name.trim().is_empty() {
            name = std::path::Path::new(&p.cwd)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        let agent = Agent {
            pane: p.pane,
            tab: p.tab,
            ws: p.ws.clone(),
            num: 0,
            state,
            name,
            task: info.first,
            cwd: p.cwd,
            age,
            reply: info.reply,
            sid: p.sid.clone(),
            path,
        };
        if let Some(g) = groups.iter_mut().find(|g| g.ws == p.ws) {
            g.agents.push(agent);
        }
    }
    groups.retain(|g| !g.agents.is_empty());
    let mut n = 0;
    for g in &mut groups {
        for a in &mut g.agents {
            n += 1;
            a.num = n;
        }
    }
    groups
}

// ---- 列表 -------------------------------------------------------------------------

/// 列表里只有 Claude；Header 是两个工作区之间那条分隔线（收起几个也写在线上）。
/// 工作区那一层选中的也记成 Header(gi)，但那时画在顶上那一排。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Header(usize),
    Agent(usize, usize),
}

pub struct List {
    pub groups: Vec<Group>,
    pub caller: Option<String>,
    pub show_all: bool,
    pub query: Option<String>,
}

impl List {
    fn hit(&self, a: &Agent, g: &Group) -> bool {
        let Some(q) = self.query.as_deref().map(str::to_lowercase).filter(|q| !q.is_empty()) else {
            return true;
        };
        [&a.name, &a.task, &a.cwd, &g.label].iter().any(|s| s.to_lowercase().contains(&q))
    }

    /// 这一组收起的是哪几个：安静的，不含你现在所在的那个；不到两个不收；搜索时不收。
    pub fn hidden(&self, gi: usize) -> Vec<usize> {
        if self.show_all || self.query.is_some() {
            return Vec::new();
        }
        let g = &self.groups[gi];
        let h: Vec<usize> = (0..g.agents.len())
            .filter(|&i| g.agents[i].state.quiet() && Some(&g.agents[i].pane) != self.caller.as_ref())
            .collect();
        if h.len() >= MIN_COLLAPSE { h } else { Vec::new() }
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for (gi, g) in self.groups.iter().enumerate() {
            let hidden = self.hidden(gi);
            let shown: Vec<usize> = (0..g.agents.len())
                .filter(|i| !hidden.contains(i) && self.hit(&g.agents[*i], g))
                .collect();
            if shown.is_empty() && hidden.is_empty() {
                continue;
            }
            out.push(Row::Header(gi));
            out.extend(shown.into_iter().map(|i| Row::Agent(gi, i)));
        }
        out
    }

    fn agent(&self, r: Row) -> Option<&Agent> {
        match r {
            Row::Agent(g, i) => self.groups.get(g)?.agents.get(i),
            _ => None,
        }
    }

    fn by_num(&self, n: usize) -> Option<&Agent> {
        self.groups.iter().flat_map(|g| &g.agents).find(|a| a.num == n)
    }

    fn max_num(&self) -> usize {
        self.groups.iter().flat_map(|g| &g.agents).map(|a| a.num).max().unwrap_or(0)
    }
}

fn dur(s: i64) -> String {
    if s < 60 {
        t("{n}秒").replace("{n}", &s.to_string())
    } else if s < 3600 {
        t("{n}分钟").replace("{n}", &(s / 60).to_string())
    } else if s < 48 * 3600 {
        t("{n}小时").replace("{n}", &format!("{:.1}", s as f64 / 3600.0))
    } else {
        t("{n}天").replace("{n}", &(s / 86400).to_string())
    }
}

/// 时间说的是这个状态的意思：RUN 跑了多久，WAIT 等了多久，DONE 做完多久了，READ 上次做完是多久以前。
fn age_text(state: State, secs: i64) -> String {
    if secs < 0 {
        return String::new();
    }
    let d = dur(secs);
    match state {
        State::Run => t("已运行 {d}").replace("{d}", &d),
        State::Wait => t("等确认 {d}").replace("{d}", &d),
        State::Done | State::Stale => t("完成 {d}前").replace("{d}", &d),
        State::Read => t("{d}前").replace("{d}", &d),
    }
}

fn badge(state: State) -> (&'static str, Style) {
    let c = colors();
    let bold = Modifier::BOLD;
    match state {
        // 跟状态栏那排圆点一样都是 ●，只靠颜色分：做完了没看是亮绿，看过了是灰（用户 2026-11-23：
        // 「read 和 done 不是很好区分」，绿底一块「太难看了，不能用我们的圆圈吗」）
        State::Wait => ("● WAIT", Style::default().fg(c.alert).add_modifier(bold)),
        State::Done => ("● DONE", Style::default().fg(c.done).add_modifier(bold)),
        State::Run => ("● RUN ", Style::default().fg(c.busy)),
        State::Read => ("● READ", Style::default().fg(c.dim)),
        State::Stale => ("● DONE", Style::default().add_modifier(Modifier::DIM)),
    }
}

fn col(s: &str, w: usize) -> String {
    let c = clip(s, w.saturating_sub(1), true);
    let pad = w.saturating_sub(c.width());
    format!("{c}{}", " ".repeat(pad))
}

fn tilde(p: &str) -> String {
    let home = crate::home().to_string_lossy().into_owned();
    match p.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => p.to_owned(),
    }
}

/// 一行（不含最左边 2 列的 ▌ 和空格）。
pub fn render_row(l: &List, r: Row, width: usize) -> Line<'static> {
    let c = colors();
    let grey = Style::default().fg(c.grey);
    match r {
        Row::Header(gi) => {
            // ── zumi ──────────── ⋯ 收起 7 个(7 已读) · a 展开 ──
            let g = &l.groups[gi];
            let rule = Style::default().fg(c.dim);
            let hidden = l.hidden(gi);
            let note = if hidden.is_empty() {
                String::new()
            } else {
                let read = hidden.iter().filter(|&&i| g.agents[i].state == State::Read).count();
                let mut kinds = Vec::new();
                if read > 0 {
                    kinds.push(t("{n} 已读").replace("{n}", &read.to_string()));
                }
                if hidden.len() > read {
                    kinds.push(t("{n} 搁置").replace("{n}", &(hidden.len() - read).to_string()));
                }
                let n = t("⋯ 收起 {n} 个({kinds}) · a 展开")
                    .replace("{n}", &hidden.len().to_string())
                    .replace("{kinds}", &kinds.join(" · "));
                format!(" {n} ")
            };
            let label = clip(&format!(" {} ", g.label), width.saturating_sub(4 + note.width()), true);
            let tail = if note.is_empty() { 0 } else { note.width() + 2 };
            let fill = width.saturating_sub(2 + label.width() + tail);
            let mut spans = vec![
                Span::styled("──", rule),
                Span::styled(label, Style::default().fg(c.cyan)),
                Span::styled("─".repeat(fill), rule),
            ];
            if !note.is_empty() {
                spans.push(Span::styled(note, grey));
                spans.push(Span::styled("──", rule));
            }
            Line::from(spans)
        }
        Row::Agent(..) => {
            let Some(a) = l.agent(r) else { return Line::default() };
            let here = l.caller.as_deref() == Some(a.pane.as_str());
            let (b, st) = badge(a.state);
            let num = format!("{:>4}  ", a.num);
            let num_st = if here {
                Style::default().fg(c.accent).add_modifier(Modifier::BOLD)
            } else {
                grey
            };
            let stale = a.state == State::Stale;
            let name_st = if stale {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            let fixed = 6 + 6 + 2 + NAME_W + AGE_W;
            let rest = width.saturating_sub(fixed);
            let task = if a.task.is_empty() { tilde(&a.cwd) } else { a.task.clone() };
            let age_st = if stale {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(num, num_st),
                Span::styled(b, st),
                Span::raw("  "),
                Span::styled(col(&a.name, NAME_W), name_st),
                Span::styled(col(&age_text(a.state, a.age), AGE_W), age_st),
                Span::styled(clip(&task, rest, true), grey),
            ])
        }
    }
}

// ---- 浮窗 -------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Chip {
    Close,
    All,
    Preview,
    Search,
    Enter,
    Days,
    Plain,
}

enum Hit {
    Row(Row),
    Chip(Chip),
    Ws(usize), // session 分隔线上除「⋯ 收起」以外的地方：点了切到那个 session
    Flow(crate::flow::Spot), // 右上流程图里的一个 Claude（点了选中它）或一条线（点了右下换成这条线上传的话）
    Days(i64),               // 流程图标题上的「1 天 / 7 天 / 30 天」
}

struct Popup {
    list: List,
    cache: Cache,
    ws_mode: bool,
    preview: bool,
    sel_pane: Option<String>,
    sel_ws: Option<String>,
    offset: usize,
    digits: String,
    digit_at: Instant,
    /// 右边那块要的：对话记录末尾那一段读出来的轮次、模型 / 上下文那一行；文件长度没变就不重读
    trs: HashMap<PathBuf, (u64, Transcript, String)>,
    flow: crate::flow::Flow,
    /// 流程图看最近几天；点开的那条线（两头的节点），右下就显示这条线上传的话
    days: i64,
    link: Option<(String, String)>,
    /// 点开的线往回翻了几句（滚轮在右下那块上滚），和右下那块在哪
    talk_off: usize,
    talk_at: Rect,
    /// 上一次单击的 Claude（动态里一条、或流程图上的框）和时间：同一个很快又点一次算双击，跳过去
    last_node: Option<(std::time::Instant, String)>,
    hits: Vec<(u16, u16, u16, Hit)>,
}

enum Go {
    Pane(Agent),
    Ws(String),
}

fn pref_path() -> PathBuf {
    crate::state_dir().join("goto.json")
}

impl Popup {
    fn stops(&self) -> Vec<Row> {
        if self.ws_mode {
            // 顶上那一排：每个有 Claude 的工作区都在，不管搜索和收起
            return (0..self.list.groups.len()).map(Row::Header).collect();
        }
        self.list.rows().into_iter().filter(|r| matches!(r, Row::Agent(..))).collect()
    }

    fn is_sel(&self, r: Row) -> bool {
        match r {
            Row::Header(gi) if self.ws_mode => self.sel_ws.as_deref() == Some(self.list.groups[gi].ws.as_str()),
            Row::Agent(..) if !self.ws_mode => self.list.agent(r).map(|a| &a.pane) == self.sel_pane.as_ref(),
            _ => false,
        }
    }

    fn select(&mut self, r: Row) {
        match r {
            Row::Header(gi) => self.sel_ws = Some(self.list.groups[gi].ws.clone()),
            Row::Agent(..) => self.sel_pane = self.list.agent(r).map(|a| a.pane.clone()),
        }
    }

    /// 选中的那一行还在不在停靠点里；不在（被收起、搜不到了）就落到第一个。
    fn fix_sel(&mut self) -> Option<Row> {
        let stops = self.stops();
        let cur = stops.iter().copied().find(|&r| self.is_sel(r));
        match cur {
            Some(r) => Some(r),
            None => {
                let r = *stops.first()?;
                self.select(r);
                Some(r)
            }
        }
    }

    fn move_sel(&mut self, d: isize) {
        self.link = None;
        if self.ws_mode {
            // 选中的是工作区：j 进到它的第一个 Claude，k 到它前面那个 Claude
            self.set_level(false);
            if d > 0 {
                return self.move_sel(d - 1);
            }
        }
        let stops = self.stops();
        if stops.is_empty() {
            return;
        }
        let i = stops.iter().position(|&r| self.is_sel(r)).unwrap_or(0) as isize;
        let j = (i + d).clamp(0, stops.len() as isize - 1) as usize;
        self.select(stops[j]);
    }

    /// h / l：选中挪到上 / 下一个工作区（tmux 的 session）本身：顶上那一排它亮起来，右边是它所有 Claude 的卡片，
    /// Enter 切到这个 session（用户 2026-11-10：「选择要放到 session 上呀，而不是第一个 windows 上」）。
    /// 在工作区上按 j 进到它的第一个 Claude，按 k 回到上一个 Claude；在 Claude 上按 h l 照样换工作区。
    /// 四个键随便按（用户 2026-11-09：「hl session 选择，jk window 选择……四个按键可以随意切换」）。
    fn jump_ws(&mut self, d: isize) {
        self.link = None;
        let n = self.list.groups.len() as isize;
        if n == 0 {
            return;
        }
        let cur = if self.ws_mode {
            self.list.groups.iter().position(|g| Some(&g.ws) == self.sel_ws.as_ref()).map(|i| i as isize)
        } else {
            self.fix_sel().and_then(|r| match r {
                Row::Agent(g, _) => Some(g as isize),
                _ => None,
            })
        };
        let gi = match cur {
            Some(c) => (c + d).clamp(0, n - 1),
            None if d > 0 => 0,
            None => n - 1,
        };
        self.sel_ws = Some(self.list.groups[gi as usize].ws.clone());
        self.ws_mode = true;
    }

    fn set_level(&mut self, ws: bool) {
        if ws == self.ws_mode {
            return;
        }
        if ws {
            let cur = self.fix_sel().and_then(|r| self.list.agent(r)).map(|a| a.ws.clone());
            self.sel_ws = cur.or_else(|| self.list.groups.first().map(|g| g.ws.clone()));
        } else if let Some(gi) = self.list.groups.iter().position(|g| Some(&g.ws) == self.sel_ws.as_ref()) {
            let first_in = |l: &List| l.rows().into_iter().find(|r| matches!(r, Row::Agent(g, _) if *g == gi));
            if first_in(&self.list).is_none() && self.list.query.is_none() {
                // 这个工作区的全收起来了：展开，不然回到列表看不到它们
                self.list.show_all = true;
            }
            let first = first_in(&self.list);
            if let Some(r) = first {
                self.select(r);
            }
        }
        self.ws_mode = ws;
    }

    fn chosen(&mut self) -> Option<Go> {
        let r = self.fix_sel()?;
        match r {
            Row::Header(gi) => Some(Go::Ws(self.list.groups[gi].ws.clone())),
            _ => self.list.agent(r).cloned().map(Go::Pane),
        }
    }

    fn save_pref(&self) {
        let _ = std::fs::create_dir_all(crate::state_dir());
        let _ = std::fs::write(pref_path(), json!({"preview": self.preview}).to_string());
    }

    fn chip(&mut self, c: Chip) -> Option<Option<Go>> {
        match c {
            Chip::Close => return Some(None),
            Chip::Enter => return Some(self.chosen()),
            Chip::All => self.list.show_all = !self.list.show_all,
            Chip::Preview => {
                self.preview = !self.preview;
                self.save_pref();
            }
            Chip::Search => {
                self.set_level(false);
                if self.list.query.is_none() {
                    self.list.query = Some(String::new());
                }
            }
            Chip::Days => {
                let i = crate::flow::RANGES.iter().position(|&d| d == self.days).map_or(0, |i| i + 1);
                self.days = crate::flow::RANGES[i % crate::flow::RANGES.len()];
            }
            Chip::Plain => {}
        }
        None
    }

    /// 整个浮窗（2026-11-17 改版，样子借 troupe 的面板：用户「他的界面非常的酷炫呀，我也想要这样的界面」）：
    /// 顶上一条标题栏（名字、LIVE、各状态几个、时钟），左边「Claude」框是列表、下面「动态」框是最近谁变了状态，
    /// 右边框是选中那个的实时画面，最底下一行是按键提示（能点的照样能点）。按键和以前一样。
    fn draw(&mut self, f: &mut ratatui::Frame) {
        let c = colors();
        let area = f.area();
        let grey = Style::default().fg(c.grey);
        let key_st = Style::default().fg(c.accent).add_modifier(Modifier::BOLD);
        self.hits.clear();
        if area.height < 6 || area.width < 30 {
            return;
        }
        let wide = area.width >= 100;
        let lw = if self.preview && wide { area.width * 9 / 20 } else { area.width };

        // ---- 标题栏 ----
        let all: Vec<Agent> = self.list.groups.iter().flat_map(|g| g.agents.iter().cloned()).collect();
        let n = |s: &[State]| all.iter().filter(|a| s.contains(&a.state)).count();
        let mut title = vec![
            Span::styled(" ◆ statetrail ", Style::default().fg(c.accent).add_modifier(Modifier::BOLD)),
            Span::styled("goto", grey),
            Span::raw("   "),
            Span::styled(" ● LIVE ", Style::default().fg(c.done).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
            Span::raw("   "),
        ];
        for (st, num, label) in [
            (State::Wait, n(&[State::Wait]), t("在等你")),
            (State::Done, n(&[State::Done]), t("做完了")),
            (State::Run, n(&[State::Run]), t("在跑")),
            (State::Read, n(&[State::Read, State::Stale]), t("看过了")),
        ] {
            let (b, bst) = badge(st);
            let icon = b.split(' ').next().unwrap_or("").to_owned();
            let style = if num == 0 { Style::default().fg(c.dim) } else { bst };
            title.push(Span::styled(format!("{icon} {num}"), style));
            title.push(Span::raw(" "));
            title.push(Span::styled(format!("{label}   "), if num == 0 { Style::default().fg(c.dim) } else { grey }));
        }
        f.render_widget(Paragraph::new(Line::from(title)), Rect::new(0, 0, area.width, 1));
        let clock = chrono::Local::now().format("%H:%M ").to_string();
        let cw = clock.width() as u16;
        f.render_widget(Paragraph::new(Span::styled(clock, grey)), Rect::new(area.width - cw, 0, cw, 1));

        // ---- 底部按键提示 ----
        let hint_y = area.height - 1;
        let pairs: Vec<(&str, String, Chip)> = vec![
            ("q", t("关闭"), Chip::Close),
            ("j/k", t("选"), Chip::Plain),
            ("h/l", t("工作区"), Chip::Plain),
            ("1-9", t("直跳"), Chip::Plain),
            ("a", if self.list.show_all { t("收起") } else { t("全部") }, Chip::All),
            ("p", t("预览"), Chip::Preview),
            ("/", t("搜索"), Chip::Search),
            ("t", t("时间段"), Chip::Days),
            ("⏎", t("跳转"), Chip::Enter),
        ];
        let mut x = 1u16;
        for (k, label, kind) in &pairs {
            let w = (k.width() + 1 + label.width()) as u16 + 2;
            if x + w > area.width {
                break;
            }
            let line = Line::from(vec![Span::styled(*k, key_st), Span::raw(" "), Span::styled(label.clone(), grey)]);
            f.render_widget(Paragraph::new(line), Rect::new(x, hint_y, w, 1));
            if *kind != Chip::Plain {
                self.hits.push((hint_y, x, x + w, Hit::Chip(*kind)));
            }
            x += w + 1;
        }

        // ---- 左边：Claude 列表框，下面动态框 ----
        let body_y = 1u16;
        let body_h = area.height.saturating_sub(2);
        let mut feed: Vec<&Agent> = all.iter().filter(|a| a.age >= 0).collect();
        feed.sort_by_key(|a| a.age);
        let feed_h = if body_h >= 22 { (feed.len().min(6) as u16 + 2).max(3) } else { 0 };
        let list_box = Rect::new(0, body_y, lw, body_h - feed_h);
        let total = self.stops().len();
        let pos = self.stops().iter().position(|&r| self.is_sel(r)).map_or(0, |i| i + 1);
        let inner = framed(f, list_box, "Claude", Span::styled(format!("{pos}/{total}"), grey), self.ws_mode);
        let cur = self.fix_sel();
        let mut top = inner.y;
        if let Some(q) = &self.list.query {
            let line = Line::from(vec![
                Span::styled("/ ", Style::default().fg(c.accent)),
                Span::raw(q.clone()),
                Span::styled("▏", Style::default().fg(c.accent)),
                Span::styled(format!("   {}", t("Esc 清掉搜索")), grey),
            ]);
            f.render_widget(Paragraph::new(line), Rect::new(inner.x, top, inner.width, 1));
            top += 1;
        }
        let rows = self.list.rows();
        let room = (inner.y + inner.height).saturating_sub(top) as usize;
        if let Some(i) = cur.and_then(|r| rows.iter().position(|&x| x == r)) {
            let want = if i > 0 && matches!(rows[i - 1], Row::Header(_)) { i - 1 } else { i };
            if want < self.offset {
                self.offset = want;
            } else if room > 0 && i >= self.offset + room {
                self.offset = i + 1 - room;
            }
        }
        self.offset = self.offset.min(rows.len().saturating_sub(room.max(1)));
        if rows.is_empty() {
            let msg = if self.list.query.is_some() { t("没有对得上的") } else { t("还没有开着的 Claude") };
            f.render_widget(Paragraph::new(Span::styled(format!(" {msg}"), grey)), Rect::new(inner.x, top, inner.width, 1));
        }
        let bg = c.selection.map_or_else(Style::default, |b| Style::default().bg(b));
        for (k, &r) in rows.iter().skip(self.offset).take(room).enumerate() {
            let y = top + k as u16;
            let mut line = render_row(&self.list, r, inner.width.saturating_sub(2) as usize);
            let on = self.is_sel(r);
            line.spans.insert(0, Span::styled(if on { "▌ " } else { "  " }, Style::default().fg(c.accent)));
            if on {
                line = line.style(bg);
            }
            if let Row::Header(gi) = r {
                let note = if line.spans.len() > 4 { line.spans[4].content.width() as u16 + 2 } else { 0 };
                self.hits.push((y, inner.x, inner.x + inner.width.saturating_sub(note), Hit::Ws(gi)));
            }
            f.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
            self.hits.push((y, inner.x, inner.x + inner.width, Hit::Row(r)));
        }

        if feed_h > 0 {
            let fb = Rect::new(0, body_y + body_h - feed_h, lw, feed_h);
            let fi = framed(f, fb, &t("动态"), Span::raw(""), false);
            let now = now();
            let lines: Vec<Line> = feed
                .iter()
                .take(fi.height as usize)
                .map(|a| {
                    let at = chrono::DateTime::from_timestamp(now - a.age, 0)
                        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
                        .unwrap_or_default();
                    let (b, st) = badge(a.state);
                    let icon = b.split(' ').next().unwrap_or("").to_owned();
                    let verb = match a.state {
                        State::Wait => t("在等你确认"),
                        State::Run => t("开始干活"),
                        State::Done => t("做完了"),
                        State::Read | State::Stale => t("做完了，看过了"),
                    };
                    let ws = self.list.groups.iter().find(|g| g.ws == a.ws).map(|g| g.label.clone()).unwrap_or_default();
                    // 选中的那个 Claude 的动态，跟左边列表一样：前面一道 ▌、整行选中底色
                    let on = !self.ws_mode && self.sel_pane.as_ref() == Some(&a.pane);
                    let line = Line::from(vec![
                        Span::styled(if on { "▌" } else { " " }, Style::default().fg(c.accent)),
                        Span::styled(format!("{at}  "), grey),
                        Span::styled(icon, st),
                        Span::raw(" "),
                        Span::styled(format!("{ws} · "), Style::default().fg(c.cyan)),
                        Span::styled(a.name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                        Span::styled(format!("  {verb}"), grey),
                    ]);
                    if on { line.style(bg) } else { line }
                })
                .collect();
            f.render_widget(Paragraph::new(lines), fi);
            // 点一条动态选中那个 Claude，跟点流程图上的框一样（左边收起了也展开）
            for (i, a) in feed.iter().take(fi.height as usize).enumerate() {
                self.hits.push((fi.y + i as u16, fi.x, fi.x + fi.width, Hit::Flow(crate::flow::Spot::Node(a.pane.clone()))));
            }
        }

        // ---- 右边：预览框 ----
        if lw < area.width {
            let pb = Rect::new(lw, body_y, area.width - lw, body_h);
            // 右上：所有 Claude 之间谁给谁传过话（flow.rs）；右下：选中那个的轮次
            let agents: Vec<Agent> = self.list.groups.iter().flat_map(|g| g.agents.iter().cloned()).collect();
            let labels: HashMap<String, String> = self.list.groups.iter().map(|g| (g.ws.clone(), g.label.clone())).collect();
            let sel = if self.ws_mode { None } else { self.sel_pane.clone() };
            let most = (pb.height * 3 / 5).max(8);
            let open = self.link.as_ref().map(|(a, b)| (a.as_str(), b.as_str()));
            let (lines, spots) = self.flow.draw(
                &agents,
                &labels,
                sel.as_deref(),
                open,
                self.days,
                pb.width.saturating_sub(4) as usize,
                most.saturating_sub(2) as usize,
            );
            let gh = (lines.len() as u16 + 2).clamp(5, most).min(pb.height);
            let gb = Rect::new(pb.x, pb.y, pb.width, gh);
            let gi = framed(f, gb, &t("流程"), Span::raw(""), false);
            // 标题右边：1 天 · 7 天 · 30 天，跟后台页面上选时间段一样，点哪个看哪个（t 键轮着换）
            let mut x = gb.x + gb.width.saturating_sub(2);
            for &d in crate::flow::RANGES.iter().rev() {
                let label = format!(" {} ", t(&format!("{d} 天")));
                let w = label.width() as u16;
                x = x.saturating_sub(w + 1);
                let st = if d == self.days {
                    Style::default().fg(c.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    grey
                };
                f.render_widget(Paragraph::new(Span::styled(label, st)), Rect::new(x, gb.y, w, 1));
                self.hits.push((gb.y, x, x + w, Hit::Days(d)));
            }
            for (y, x0, x1, spot) in spots {
                if y < gi.height {
                    self.hits.push((gi.y + y, gi.x + x0, gi.x + x1, Hit::Flow(spot)));
                }
            }
            f.render_widget(Paragraph::new(lines), gi);
            let tb = Rect::new(pb.x, pb.y + gh, pb.width, pb.height - gh);
            match self.link.clone() {
                Some((a, b)) => self.draw_talk(f, tb, &agents, &a, &b),
                None => self.draw_preview(f, tb, cur),
            }
        }
    }

    /// 点开流程图上的一条线：右下换成这两个 Claude 之间来回传的原话，最新的在最下面，Esc 回到轮次。
    fn draw_talk(&mut self, f: &mut ratatui::Frame, pb: Rect, agents: &[Agent], a: &str, b: &str) {
        let c = colors();
        let grey = Style::default().fg(c.grey);
        let said = self.flow.talk(agents, a, b, self.days);
        let name = |k: &str| crate::flow::name_of(agents, k);
        let title = format!("{} ⇄ {}", name(a), name(b));
        let off = if self.talk_off > 0 { format!("{} {} · ", t("往回翻了"), self.talk_off) } else { String::new() };
        let r = framed(f, pb, &title, Span::styled(format!("{off}{} {} · Esc {}", said.len(), t("条"), t("返回")), grey), true);
        let w = r.width as usize;
        let today = chrono::Local::now().date_naive();
        self.talk_at = pb;
        self.talk_off = self.talk_off.min(said.len().saturating_sub(1));
        let mut body: Vec<Line> = Vec::new();
        for s in said.iter().rev().skip(self.talk_off) {
            let at = chrono::DateTime::from_timestamp(s.ts, 0).map(|d| d.with_timezone(&chrono::Local));
            let at = at.map_or_else(String::new, |d| if d.date_naive() == today { d.format("%H:%M").to_string() } else { d.format("%m-%d %H:%M").to_string() });
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{at}  "), grey),
                Span::styled(name(&s.from), Style::default().fg(c.accent).add_modifier(Modifier::BOLD)),
                Span::styled(" → ", grey),
                Span::styled(name(&s.to), Style::default().add_modifier(Modifier::BOLD)),
            ])];
            // 原话按宽度折行，一句最多 4 行，多放几句；要看更早的在这块上滚滚轮
            let mut rows: Vec<String> = Vec::new();
            for para in s.text.lines().filter(|l| !l.trim().is_empty()) {
                let mut cur = String::new();
                for ch in para.chars() {
                    if cur.width() + ch.to_string().width() > w.saturating_sub(2) {
                        rows.push(std::mem::take(&mut cur));
                    }
                    cur.push(ch);
                }
                rows.push(cur);
            }
            if rows.len() > 4 {
                rows.truncate(4);
                rows.push("…".into());
            }
            lines.extend(rows.into_iter().map(|l| Line::from(format!("  {l}"))));
            lines.push(Line::default());
            if body.len() + lines.len() > r.height as usize && !body.is_empty() {
                break;
            }
            lines.extend(body);
            body = lines;
        }
        if said.is_empty() {
            body.push(Line::from(Span::styled(t("这段时间里这条线上没有传过话"), grey)));
        }
        let skip = body.len().saturating_sub(r.height as usize);
        f.render_widget(Paragraph::new(body.split_off(skip)), r);
    }

    fn draw_preview(&mut self, f: &mut ratatui::Frame, pb: Rect, cur: Option<Row>) {
        let c = colors();
        let grey = Style::default().fg(c.grey);
        let Some(cur) = cur else {
            framed(f, pb, &t("预览"), Span::raw(""), false);
            return;
        };
        if let Row::Header(gi) = cur {
            // 工作区那一层：每个 Claude 一张卡片——状态、名字、多久，开场那句话，最后回了什么
            let g = self.list.groups[gi].clone();
            let r = framed(f, pb, &format!("{} {}", g.num, g.label), Span::raw(""), true);
            let mut lines = Vec::new();
            for a in &g.agents {
                let (b, st) = badge(a.state);
                lines.push(Line::from(vec![
                    Span::styled(b, st),
                    Span::raw("  "),
                    Span::styled(a.name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {}", age_text(a.state, a.age)), grey),
                ]));
                let w = r.width.saturating_sub(4) as usize;
                if !a.task.is_empty() {
                    lines.push(Line::from(Span::styled(format!("    {}", clip(&a.task, w, true)), grey)));
                }
                if !a.reply.is_empty() {
                    lines.push(Line::from(format!("    {}", clip(&a.reply, w, true))));
                }
                lines.push(Line::default());
            }
            f.render_widget(Paragraph::new(lines), r);
            return;
        }
        let Some(a) = self.list.agent(cur).cloned() else { return };
        let (b, st) = badge(a.state);
        let r = framed(f, pb, &a.name, Span::styled(b.trim().to_owned(), st), false);
        // 右边是这个会话最近几轮（用户 2026-11-17：右边的实时画面「没啥意思，换掉吧」，选了「这个会话的轮次」）：
        // 头两行是状态多久、目录，模型和上下文；下面是轮次，最新的在最下面，开新章的地方画一条章名线。
        let mut head = vec![Line::from(vec![
            Span::styled(age_text(a.state, a.age), st),
            Span::styled(format!("  ·  {}", tilde(&a.cwd)), grey),
        ])];
        let w = r.width as usize;
        let Some(path) = a.path.clone() else {
            head.push(Line::from(Span::styled(t("找不到它的对话记录"), grey)));
            f.render_widget(Paragraph::new(head), r);
            return;
        };
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        if self.trs.get(&path).is_none_or(|e| e.0 != size) {
            // 只读末尾：最大的对话记录有 1.7 GB，整份读要好几秒。轮次不够填满就往前多读，最多 16 MB
            let mut back = TAIL;
            let tr = loop {
                let tr = Transcript::slice(&path, size.saturating_sub(back), size);
                if tr.visible().len() >= 40 || back >= size || back >= 16 << 20 {
                    break tr;
                }
                back *= 4;
            };
            let (m, ctx) = crate::claude::model_ctx(&path);
            let model = [m, ctx].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join("  ·  ");
            self.trs.insert(path.clone(), (size, tr, model));
        }
        let (_, tr, model) = &self.trs[&path];
        if !model.is_empty() {
            head.push(Line::from(Span::styled(model.clone(), grey)));
        }
        head.push(Line::default());
        let stops = tr.visible();
        let marks = a.sid.as_deref().map(crate::chapters::Marks::load).unwrap_or_default();
        // 章名按轮次 id 认（读的只是末尾一段，不能用从第一轮数起的章）：Haiku 起的章名，或手动标的「从这轮开新章」
        let chapter = |s: &crate::transcript::Stop| match marks.manual.get(&s.id) {
            Some(false) => None,
            Some(true) => Some(marks.auto.get(&s.id).cloned().unwrap_or_else(|| oneline(&s.prompt))),
            None => marks.auto.get(&s.id).cloned(),
        };
        let room = (r.height as usize).saturating_sub(head.len());
        // 从最新一轮往回凑，凑满为止；每轮两行（你说的、它怎么开头回的），开新章的那轮多一行章名
        let mut body: Vec<Line> = Vec::new();
        for i in (0..stops.len()).rev() {
            let s = stops[i];
            let mut lines = Vec::new();
            if let Some(title) = chapter(s) {
                let title = clip(&title, w.saturating_sub(6), true);
                let fill = w.saturating_sub(title.width() + 4);
                lines.push(Line::from(vec![
                    Span::styled("── ", Style::default().fg(c.dim)),
                    Span::styled(title, Style::default().fg(c.cyan)),
                    Span::styled(format!(" {}", "─".repeat(fill)), Style::default().fg(c.dim)),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("❯ ", Style::default().fg(c.accent)),
                Span::styled(clip(&oneline(&s.prompt), w.saturating_sub(2), true), Style::default().add_modifier(Modifier::BOLD)),
            ]));
            if !s.reply.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  → {}", clip(&oneline(&s.reply), w.saturating_sub(4), true)),
                    grey,
                )));
            }
            if body.len() + lines.len() > room {
                break;
            }
            lines.extend(body);
            body = lines;
        }
        if stops.is_empty() {
            body.push(Line::from(Span::styled(t("还没有说过话"), grey)));
        }
        head.extend(body);
        f.render_widget(Paragraph::new(head), r);
    }

    /// 一个键：Some(Some(去处)) 跳过去、Some(None) 关掉、None 接着等。
    fn key(&mut self, k: event::KeyEvent) -> Option<Option<Go>> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            return Some(None);
        }
        if let Some(q) = self.list.query.as_mut() {
            match k.code {
                KeyCode::Esc => self.list.query = None,
                KeyCode::Enter => return Some(self.chosen()),
                KeyCode::Backspace => {
                    q.pop();
                }
                KeyCode::Up => self.move_sel(-1),
                KeyCode::Down => self.move_sel(1),
                KeyCode::Char('p') if ctrl => self.move_sel(-1),
                KeyCode::Char('n') if ctrl => self.move_sel(1),
                KeyCode::Char(ch) if !ctrl => q.push(ch),
                _ => {}
            }
            return None;
        }
        match k.code {
            KeyCode::Esc if self.link.is_some() => self.link = None,
            KeyCode::Esc | KeyCode::Char('q') => return Some(None),
            KeyCode::Char('t') => return self.chip(Chip::Days),
            KeyCode::Enter => return Some(self.chosen()),
            // h l 换工作区，j k 在 Claude 之间挪，随便按
            KeyCode::Left | KeyCode::Char('h') => self.jump_ws(-1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => self.jump_ws(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Char('p') if ctrl => self.move_sel(-1),
            KeyCode::Char('n') if ctrl => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(-10_000),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(10_000),
            KeyCode::Char('a') => return self.chip(Chip::All),
            KeyCode::Char('p') => return self.chip(Chip::Preview),
            KeyCode::Char('/') => return self.chip(Chip::Search),
            KeyCode::Char(d) if d.is_ascii_digit() => {
                self.digits.push(d);
                self.digit_at = Instant::now();
                let n: usize = self.digits.parse().unwrap_or(0);
                // 再按一位也不可能有这个号了就马上跳，不然等一会儿看还有没有下一位
                if n * 10 > self.list.max_num() {
                    return self.digit_jump();
                }
            }
            _ => {}
        }
        None
    }

    fn digit_jump(&mut self) -> Option<Option<Go>> {
        let n: usize = std::mem::take(&mut self.digits).parse().unwrap_or(0);
        self.list.by_num(n).cloned().map(|a| Some(Go::Pane(a)))
    }

    fn mouse(&mut self, m: event::MouseEvent) -> Option<Option<Go>> {
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if self.link.is_some() && self.talk_at.contains(ratatui::layout::Position::new(m.column, m.row)) =>
            {
                let up = matches!(m.kind, MouseEventKind::ScrollUp);
                self.talk_off = if up { self.talk_off + 1 } else { self.talk_off.saturating_sub(1) };
            }
            MouseEventKind::ScrollUp => self.move_sel(-1),
            MouseEventKind::ScrollDown => self.move_sel(1),
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = self
                    .hits
                    .iter()
                    .find(|(y, x0, x1, _)| *y == m.row && m.column >= *x0 && m.column < *x1);
                match hit.map(|h| &h.3) {
                    Some(Hit::Chip(c)) => return self.chip(*c),
                    Some(Hit::Ws(gi)) => return Some(Some(Go::Ws(self.list.groups[*gi].ws.clone()))),
                    Some(Hit::Flow(crate::flow::Spot::Node(p))) => {
                        let p = p.clone();
                        let again = self.last_node.take().is_some_and(|(t, q)| q == p && t.elapsed().as_millis() < 400);
                        if again && !self.ws_mode && self.sel_pane.as_ref() == Some(&p) {
                            return Some(self.chosen());
                        }
                        self.last_node = Some((std::time::Instant::now(), p.clone()));
                        self.ws_mode = false;
                        self.sel_pane = Some(p);
                        self.link = None;
                        // 图上点的 Claude 在左边被收起或搜不到：展开、清掉搜索，让它真的被选中
                        let there = |s: &Self| s.stops().into_iter().any(|r| s.is_sel(r));
                        if !there(self) {
                            self.list.show_all = true;
                        }
                        if !there(self) {
                            self.list.query = None;
                        }
                    }
                    Some(Hit::Flow(crate::flow::Spot::Link(a, b))) => {
                        self.link = Some((a.clone(), b.clone()));
                        self.talk_off = 0;
                    }
                    Some(Hit::Days(d)) => self.days = *d,
                    // 分隔线上写着收起了几个：点它展开
                    Some(Hit::Row(Row::Header(gi))) => {
                        if !self.list.hidden(*gi).is_empty() {
                            self.list.show_all = true;
                        }
                    }
                    Some(Hit::Row(r)) => {
                        let r = *r;
                        self.ws_mode = false;
                        self.select(r);
                        return Some(self.chosen());
                    }
                    None => {}
                }
            }
            _ => {}
        }
        None
    }
}

/// 一个圆角框，标题写在上边框上（左边 title，右边 right），返回框里面能画的地方。lit：框用强调色（选中的是这一层）。
fn framed(f: &mut ratatui::Frame, r: Rect, title: &str, right: Span<'static>, lit: bool) -> Rect {
    use ratatui::widgets::{Block, BorderType, Borders};
    let c = colors();
    let border = Style::default().fg(if lit { c.accent } else { c.dim });
    let mut b = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Line::from(Span::styled(format!(" {title} "), Style::default().fg(c.cyan).add_modifier(Modifier::BOLD))));
    if !right.content.is_empty() {
        b = b.title(Line::from(vec![Span::raw(" "), right, Span::raw(" ")]).right_aligned());
    }
    let inner = b.inner(r);
    f.render_widget(b, r);
    Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), inner.height)
}

fn go(host: &dyn Host, g: Go) {
    match g {
        Go::Pane(a) => host.go_pane(&a),
        Go::Ws(ws) => host.go_ws(&ws),
    }
}

/// `goto-list`：在终端里打出同样的列表（全部展开），顺便量收集一遍要多久。
pub fn cmd_list(host: &dyn Host) {
    let t0 = Instant::now();
    let mut cache = Cache::default();
    let groups = collect(host, &mut cache);
    let t1 = t0.elapsed();
    let again = Instant::now();
    collect(host, &mut cache);
    eprintln!(
        "collect {:.0}ms, again {:.0}ms",
        t1.as_secs_f64() * 1000.0,
        again.elapsed().as_secs_f64() * 1000.0
    );
    let l = List {
        groups,
        caller: None,
        show_all: true,
        query: None,
    };
    for r in l.rows() {
        println!(
            "{}",
            render_row(&l, r, 120).spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        );
    }
}

/// 浮窗里跑的那个程序。
pub fn run(host: &dyn Host) {
    let mut cache = Cache::default();
    let groups = collect(host, &mut cache);
    let preview = std::fs::read(pref_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["preview"].as_bool())
        .unwrap_or(true);
    let mut p = Popup {
        list: List {
            groups,
            caller: host.caller(),
            show_all: false,
            query: None,
        },
        cache,
        ws_mode: false,
        preview,
        sel_pane: None,
        sel_ws: None,
        offset: 0,
        digits: String::new(),
        digit_at: Instant::now(),
        trs: HashMap::new(),
        flow: crate::flow::Flow::default(),
        days: 1,
        link: None,
        talk_off: 0,
        talk_at: Rect::default(),
        last_node: None,
        hits: Vec::new(),
    };
    let _ = event::poll(Duration::ZERO);
    let mut out = std::io::stdout();
    let _ = terminal::enable_raw_mode();
    let _ = execute!(out, terminal::EnterAlternateScreen, event::EnableMouseCapture);
    let Ok(mut term) = Terminal::new(CrosstermBackend::new(out)) else {
        return;
    };
    let mut refreshed = Instant::now();
    let picked = loop {
        if refreshed.elapsed() > Duration::from_secs(1) {
            p.list.groups = collect(host, &mut p.cache);
            refreshed = Instant::now();
        }
        if term.draw(|f| p.draw(f)).is_err() {
            break None;
        }
        let mut wait = Duration::from_millis(250);
        if !p.digits.is_empty() {
            let left = DIGIT_WAIT.saturating_sub(p.digit_at.elapsed());
            if left.is_zero() {
                if let Some(r) = p.digit_jump() {
                    break r;
                }
                continue;
            }
            wait = wait.min(left);
        }
        if !event::poll(wait).unwrap_or(false) {
            continue;
        }
        let r = match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => p.key(k),
            Ok(Event::Mouse(m)) => p.mouse(m),
            Ok(_) => None,
            Err(_) => Some(None),
        };
        if let Some(r) = r {
            break r;
        }
    };
    p.flow.save();
    let _ = execute!(term.backend_mut(), event::DisableMouseCapture, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    if let Some(g) = picked {
        go(host, g);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(num: usize, state: State, name: &str) -> Agent {
        Agent {
            pane: format!("w1:p{num}"),
            tab: "w1:t1".into(),
            ws: "w1".into(),
            num,
            state,
            name: name.into(),
            task: format!("{name} 的任务"),
            cwd: "/tmp".into(),
            age: 90,
            reply: String::new(),
            sid: None,
            path: None,
        }
    }

    fn list() -> List {
        List {
            groups: vec![Group {
                ws: "w1".into(),
                num: 1,
                label: "work".into(),
                agents: vec![
                    agent(1, State::Run, "api"),
                    agent(2, State::Read, "old"),
                    agent(3, State::Wait, "deploy"),
                    agent(4, State::Stale, "scratch"),
                ],
            }],
            caller: None,
            show_all: false,
            query: None,
        }
    }

    #[test]
    fn quiet_ones_fold_into_the_divider() {
        let l = list();
        assert_eq!(l.rows(), vec![Row::Header(0), Row::Agent(0, 0), Row::Agent(0, 2)]);
        // 收起几个写在分隔线上，不另占一行；线画满整行
        let line: String = render_row(&l, Row::Header(0), 80).spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line.starts_with("── work ─"), "{line}");
        assert!(line.contains(&t("{n} 已读").replace("{n}", "1")), "{line}");
        assert_eq!(line.width(), 80, "{line}");
    }

    #[test]
    fn one_quiet_one_is_not_folded_and_caller_never_is() {
        let mut l = list();
        l.caller = Some("w1:p2".into());
        // 只剩一个能收的：不收
        assert_eq!(l.rows().len(), 5);
        l.caller = None;
        l.show_all = true;
        assert_eq!(l.rows().len(), 5);
    }

    #[test]
    fn search_shows_matches_without_folding() {
        let mut l = list();
        l.query = Some("SCR".into());
        assert_eq!(l.rows(), vec![Row::Header(0), Row::Agent(0, 3)]);
    }

    #[test]
    fn row_has_number_label_name_age() {
        let l = list();
        let s: String = render_row(&l, Row::Agent(0, 2), 100)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(s.starts_with("   3  ● WAIT  deploy"), "{s}");
        assert!(
            s.contains(&t("等确认 {d}").replace("{d}", &t("{n}分钟").replace("{n}", "1"))),
            "{s}"
        );
    }

    #[test]
    fn unknown_age_is_blank() {
        // 没有对话记录（herdr 没报会话 id）时不写「0 秒前」
        assert_eq!(age_text(State::Done, -1), "");
        assert!(!age_text(State::Done, 0).is_empty());
    }
}
