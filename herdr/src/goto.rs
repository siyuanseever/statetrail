//! 跳到某个 Claude（prefix+g，`statetrail goto`）：herdr 自带的 Goto 换成 tmux 版 claude-tmux-picker 的样子和按键
//! （用户 2026-10-30：「这个功能做的跟我在 tmux 里的差很多呀……把交互逻辑对齐一下，UI 也尽量对齐，或者写的更好看」）。
//! 居中的浮窗（herdr 的 popup，宽 95% 高 85%，和 tmux 的 display-popup 一样大），按工作区分组，一行一个 Claude：
//! 行号、状态（⏸ WAIT / ✔ DONE / ▶ RUN / ✓ READ）、名字、这个状态待了多久、它在做什么（开场那句话）。
//! 安静的（READ、做完两小时没看的）默认收起，a 展开；右边是选中那个 Claude 的实时画面，p 收起。
//! h 进到工作区那一层，l 回来；行号直接跳，/ 搜索，Enter 跳过去，q / Esc 关。

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::{execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, UNIX_EPOCH};
use unicode_width::UnicodeWidthStr;

use crate::herdr;
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
}

#[derive(Clone, Debug)]
pub struct Group {
    pub ws: String,
    pub num: u64,
    pub label: String,
    pub agents: Vec<Agent>,
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
fn collect(cache: &mut Cache) -> Vec<Group> {
    let ws = herdr::call("workspace.list", json!({})).unwrap_or(Value::Null);
    let panes = herdr::call("pane.list", json!({})).unwrap_or(Value::Null);
    let mut groups: Vec<Group> = ws["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|w| Group {
            ws: w["workspace_id"].as_str().unwrap_or_default().to_owned(),
            num: w["number"].as_u64().unwrap_or(0),
            label: w["label"].as_str().unwrap_or_default().to_owned(),
            agents: Vec::new(),
        })
        .collect();
    groups.sort_by_key(|g| g.num);
    let now = now();
    for p in panes["panes"].as_array().into_iter().flatten() {
        if p["agent"].as_str().is_none_or(str::is_empty) {
            continue;
        }
        let s = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
        let status = s("agent_status");
        let info = p["agent_session"]["value"]
            .as_str()
            .and_then(|sid| cache.path(sid))
            .map(|path| cache.info(&path, status == "working"))
            .unwrap_or_default();
        let since = match status.as_str() {
            "working" => info.last_ts.unwrap_or(info.mtime),
            _ => info.mtime,
        };
        let age = if since > 0 { (now - since).max(0) } else { 0 };
        let state = match status.as_str() {
            "blocked" => State::Wait,
            "working" => State::Run,
            "done" if age >= STALE_SECS => State::Stale,
            "done" => State::Done,
            _ => State::Read,
        };
        let cwd = s("cwd");
        let mut name = s("terminal_title_stripped");
        if name.trim().is_empty() {
            name = std::path::Path::new(&cwd)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        let ws = s("workspace_id");
        let agent = Agent {
            pane: s("pane_id"),
            tab: s("tab_id"),
            ws: ws.clone(),
            num: 0,
            state,
            name,
            task: info.first,
            cwd,
            age,
            reply: info.reply,
        };
        if let Some(g) = groups.iter_mut().find(|g| g.ws == ws) {
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Header(usize),
    Agent(usize, usize),
    Note(usize),
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
            if !hidden.is_empty() {
                out.push(Row::Note(gi));
            }
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
        State::Wait => ("⏸ WAIT", Style::default().fg(c.alert).add_modifier(bold)),
        State::Done => ("✔ DONE", Style::default().fg(c.done).add_modifier(bold)),
        State::Run => ("▶ RUN ", Style::default().fg(c.busy)),
        State::Read => ("✓ READ", Style::default().fg(c.time)),
        State::Stale => ("✔ DONE", Style::default().add_modifier(Modifier::DIM)),
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
            let g = &l.groups[gi];
            let mut spans = vec![Span::styled(
                format!("▾ {} {}", g.num, g.label),
                Style::default().fg(c.cyan).add_modifier(Modifier::BOLD),
            )];
            let count = |s: State| g.agents.iter().filter(|a| a.state == s).count();
            for s in [State::Wait, State::Done, State::Run, State::Read, State::Stale] {
                let n = count(s);
                if n > 0 {
                    let (b, st) = badge(s);
                    let icon = b.split(' ').next().unwrap_or("");
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(format!("{icon} {n}"), st));
                }
            }
            Line::from(spans)
        }
        Row::Note(gi) => {
            let g = &l.groups[gi];
            let hidden = l.hidden(gi);
            let read = hidden.iter().filter(|&&i| g.agents[i].state == State::Read).count();
            let mut kinds = Vec::new();
            if read > 0 {
                kinds.push(t("{n} 已读").replace("{n}", &read.to_string()));
            }
            if hidden.len() > read {
                kinds.push(t("{n} 搁置").replace("{n}", &(hidden.len() - read).to_string()));
            }
            let note = t("⋯ 收起 {n} 个({kinds}) · a 展开")
                .replace("{n}", &hidden.len().to_string())
                .replace("{kinds}", &kinds.join(" · "));
            Line::from(Span::styled(clip(&format!("      {note}"), width, true), grey))
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

// ---- 预览：选中那个 Claude 的实时画面 ------------------------------------------------------

fn sgr(mut st: Style, p: &str) -> Style {
    let v: Vec<u16> = if p.is_empty() {
        vec![0]
    } else {
        p.split([';', ':']).map(|x| x.parse().unwrap_or(0)).collect()
    };
    let mut i = 0;
    while i < v.len() {
        match v[i] {
            0 => st = Style::default(),
            1 => st = st.add_modifier(Modifier::BOLD),
            2 => st = st.add_modifier(Modifier::DIM),
            3 => st = st.add_modifier(Modifier::ITALIC),
            4 => st = st.add_modifier(Modifier::UNDERLINED),
            7 => st = st.add_modifier(Modifier::REVERSED),
            22 => st = st.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => st = st.remove_modifier(Modifier::ITALIC),
            24 => st = st.remove_modifier(Modifier::UNDERLINED),
            27 => st = st.remove_modifier(Modifier::REVERSED),
            n @ 30..=37 => st = st.fg(Color::Indexed((n - 30) as u8)),
            39 => st = st.fg(Color::Reset),
            n @ 40..=47 => st = st.bg(Color::Indexed((n - 40) as u8)),
            49 => st = st.bg(Color::Reset),
            n @ 90..=97 => st = st.fg(Color::Indexed((n - 90 + 8) as u8)),
            n @ 100..=107 => st = st.bg(Color::Indexed((n - 100 + 8) as u8)),
            n @ (38 | 48) => {
                let c = match v.get(i + 1) {
                    Some(5) => {
                        i += 2;
                        v.get(i).map(|&x| Color::Indexed(x as u8))
                    }
                    Some(2) => {
                        i += 4;
                        match (v.get(i - 2), v.get(i - 1), v.get(i)) {
                            (Some(&r), Some(&g), Some(&b)) => Some(Color::Rgb(r as u8, g as u8, b as u8)),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(c) = c {
                    st = if n == 38 { st.fg(c) } else { st.bg(c) };
                }
            }
            _ => {}
        }
        i += 1;
    }
    st
}

/// 一行带颜色的终端文字（只认 SGR，其余转义丢掉）。
pub fn ansi_line(s: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut st = Style::default();
    let mut buf = String::new();
    let mut it = s.chars().peekable();
    while let Some(ch) = it.next() {
        match ch {
            '\x1b' => {
                if it.peek() == Some(&'[') {
                    it.next();
                    let mut p = String::new();
                    let mut fin = ' ';
                    for c in it.by_ref() {
                        if c.is_ascii_alphabetic() || c == '~' {
                            fin = c;
                            break;
                        }
                        p.push(c);
                    }
                    if fin == 'm' {
                        if !buf.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut buf), st));
                        }
                        st = sgr(st, &p);
                    }
                } else {
                    it.next();
                }
            }
            '\r' => {}
            '\u{a0}' => buf.push(' '),
            c => buf.push(c),
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, st));
    }
    Line::from(spans)
}

fn plain(s: &str) -> String {
    ansi_line(s).spans.iter().map(|s| s.content.as_ref()).collect()
}

fn is_rule(s: &str) -> bool {
    let p = plain(s);
    let p = p.trim();
    p.chars().filter(|&c| c == '─').count() >= 10 && p.starts_with('─')
}

/// 去掉 Claude 屏幕最下面的输入框（上下两条横线夹着 ❯）和它下面的提示行，再去掉末尾的空行。
pub fn strip_chrome(lines: Vec<String>) -> Vec<String> {
    let mut lines = lines;
    let n = lines.len();
    let rules: Vec<usize> = (n.saturating_sub(10)..n).filter(|&i| is_rule(&lines[i])).collect();
    if rules.len() >= 2 {
        lines.truncate(rules[rules.len() - 2]);
    }
    while lines.last().is_some_and(|l| plain(l).trim().is_empty()) {
        lines.pop();
    }
    lines
}

fn screen(pane: &str) -> Vec<String> {
    let text = herdr::call("pane.read", json!({"pane_id": pane, "source": "visible", "format": "ansi"}))
        .ok()
        .and_then(|r| r["read"]["text"].as_str().map(str::to_owned))
        .unwrap_or_default();
    strip_chrome(text.split('\n').map(|l| l.trim_end_matches('\r').to_owned()).collect())
}

// ---- 浮窗 -------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Chip {
    Close,
    Level,
    All,
    Preview,
    Search,
    Enter,
    Plain,
}

enum Hit {
    Row(Row),
    Chip(Chip),
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
    shot: Option<(String, Instant, Vec<String>)>,
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
        self.list
            .rows()
            .into_iter()
            .filter(|r| {
                if self.ws_mode {
                    matches!(r, Row::Header(_))
                } else {
                    matches!(r, Row::Agent(..))
                }
            })
            .collect()
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
            Row::Note(_) => {}
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
        let stops = self.stops();
        if stops.is_empty() {
            return;
        }
        let i = stops.iter().position(|&r| self.is_sel(r)).unwrap_or(0) as isize;
        let j = (i + d).clamp(0, stops.len() as isize - 1) as usize;
        self.select(stops[j]);
    }

    fn set_level(&mut self, ws: bool) {
        if ws == self.ws_mode {
            return;
        }
        if ws {
            let cur = self.fix_sel().and_then(|r| self.list.agent(r)).map(|a| a.ws.clone());
            self.sel_ws = cur.or_else(|| self.list.groups.first().map(|g| g.ws.clone()));
        } else if let Some(gi) = self.list.groups.iter().position(|g| Some(&g.ws) == self.sel_ws.as_ref()) {
            let first = self.list.rows().into_iter().find(|r| matches!(r, Row::Agent(g, _) if *g == gi));
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
            Chip::Level => self.set_level(!self.ws_mode),
            Chip::All => self.list.show_all = !self.list.show_all,
            Chip::Preview => {
                self.preview = !self.preview;
                self.save_pref();
            }
            Chip::Search => {
                if self.list.query.is_none() {
                    self.list.query = Some(String::new());
                }
            }
            Chip::Plain => {}
        }
        None
    }

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let c = colors();
        let area = f.area();
        let grey = Style::default().fg(c.grey);
        self.hits.clear();
        let wide = area.width >= 100;
        let lw = if self.preview && wide { area.width / 2 } else { area.width };

        // 第一行：能点的词画成按钮（和 tmux 版提示栏一样）
        let chips: Vec<(String, Chip)> = vec![
            (t("✕ 关闭"), Chip::Close),
            (t("j/k 选"), Chip::Plain),
            (t("数字直跳"), Chip::Plain),
            (if self.ws_mode { t("l 回到列表") } else { t("h 工作区") }, Chip::Level),
            (if self.list.show_all { t("a 收起") } else { t("a 全部") }, Chip::All),
            (t("p 预览"), Chip::Preview),
            (t("/ 搜索"), Chip::Search),
            (t("Enter 跳转"), Chip::Enter),
        ];
        let mut x = 1u16;
        let mut marks = Vec::new();
        for (label, kind) in &chips {
            let text = format!(" {label} ");
            let w = text.width() as u16;
            if x + w > lw {
                break;
            }
            let st = if *kind == Chip::Plain { grey } else { Style::default() };
            f.render_widget(Paragraph::new(Line::from(Span::styled(text, st))), Rect::new(x, 0, w, 1));
            if *kind != Chip::Plain {
                marks.push((x, x + w));
                self.hits.push((0, x, x + w, Hit::Chip(*kind)));
            }
            x += w + 1;
        }
        for (x0, x1) in marks {
            crate::ui::mark(f, 0, x0, x1, false);
        }

        let mut top = 1u16;
        if let Some(q) = &self.list.query {
            let line = Line::from(vec![
                Span::styled("  / ", Style::default().fg(c.accent)),
                Span::raw(q.clone()),
                Span::styled("▏", Style::default().fg(c.accent)),
                Span::styled(format!("   {}", t("Esc 清掉搜索")), grey),
            ]);
            f.render_widget(Paragraph::new(line), Rect::new(0, 1, lw, 1));
            top = 2;
        }

        // 列表
        let rows = self.list.rows();
        let cur = self.fix_sel();
        let room = area.height.saturating_sub(top) as usize;
        if let Some(i) = cur.and_then(|r| rows.iter().position(|&x| x == r)) {
            // 选中的是一组的第一个时把组名也露出来
            let want = if i > 0 && matches!(rows[i - 1], Row::Header(_)) { i - 1 } else { i };
            if want < self.offset {
                self.offset = want;
            } else if room > 0 && i >= self.offset + room {
                self.offset = i + 1 - room;
            }
        }
        self.offset = self.offset.min(rows.len().saturating_sub(room.max(1)));
        let total = self.stops().len();
        let pos = self.stops().iter().position(|&r| self.is_sel(r)).map_or(0, |i| i + 1);
        let counter = format!("{pos}/{total} ");
        let cw = counter.width() as u16;
        if lw > cw + x {
            f.render_widget(Paragraph::new(Span::styled(counter, grey)), Rect::new(lw - cw, 0, cw, 1));
        }
        if rows.is_empty() {
            let msg = if self.list.query.is_some() {
                t("没有对得上的")
            } else {
                t("还没有开着的 Claude")
            };
            f.render_widget(Paragraph::new(Span::styled(format!("  {msg}"), grey)), Rect::new(0, top, lw, 1));
        }
        let bg = c.selection.map_or_else(Style::default, |b| Style::default().bg(b));
        for (k, &r) in rows.iter().skip(self.offset).take(room).enumerate() {
            let y = top + k as u16;
            let mut line = render_row(&self.list, r, lw.saturating_sub(3) as usize);
            let on = self.is_sel(r);
            line.spans
                .insert(0, Span::styled(if on { "▌ " } else { "  " }, Style::default().fg(c.accent)));
            if on {
                line = line.style(bg);
            }
            f.render_widget(Paragraph::new(line), Rect::new(0, y, lw.saturating_sub(1), 1));
            self.hits.push((y, 0, lw, Hit::Row(r)));
        }

        if lw < area.width {
            let px = lw + 1;
            let pw = area.width - px;
            let ph = area.height;
            let sep: Vec<Line> = (0..ph).map(|_| Line::from(Span::styled("│", Style::default().fg(c.dim)))).collect();
            f.render_widget(Paragraph::new(sep), Rect::new(lw, 0, 1, ph));
            self.draw_preview(f, Rect::new(px + 1, 0, pw.saturating_sub(1), ph), cur);
        }
    }

    fn draw_preview(&mut self, f: &mut ratatui::Frame, r: Rect, cur: Option<Row>) {
        let c = colors();
        let grey = Style::default().fg(c.grey);
        let Some(cur) = cur else { return };
        if let Row::Header(gi) = cur {
            // 工作区那一层：每个 Claude 一张卡片——状态、名字、多久，开场那句话，最后回了什么
            let g = self.list.groups[gi].clone();
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("{} {}", g.num, g.label),
                    Style::default().fg(c.cyan).add_modifier(Modifier::BOLD),
                )),
                Line::default(),
            ];
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
        let fresh = self
            .shot
            .as_ref()
            .is_some_and(|(p, at, _)| *p == a.pane && at.elapsed() < Duration::from_millis(500));
        if !fresh {
            self.shot = Some((a.pane.clone(), Instant::now(), screen(&a.pane)));
        }
        let shot = self.shot.as_ref().map(|s| s.2.clone()).unwrap_or_default();
        let body_h = r.height.saturating_sub(2) as usize;
        let lines: Vec<Line> = shot[shot.len().saturating_sub(body_h)..].iter().map(|l| ansi_line(l)).collect();
        f.render_widget(Paragraph::new(lines), Rect::new(r.x, r.y, r.width, body_h as u16));
        let (b, st) = badge(a.state);
        let foot_y = r.y + r.height.saturating_sub(1);
        f.render_widget(
            Paragraph::new(Span::styled("─".repeat(r.width as usize), Style::default().fg(c.dim))),
            Rect::new(r.x, foot_y.saturating_sub(1), r.width, 1),
        );
        let foot = Line::from(vec![
            Span::styled(b, st),
            Span::raw("  "),
            Span::raw(age_text(a.state, a.age)),
            Span::styled(format!("   {}", tilde(&a.cwd)), grey),
        ]);
        f.render_widget(Paragraph::new(foot), Rect::new(r.x, foot_y, r.width, 1));
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
            KeyCode::Esc | KeyCode::Char('q') => return Some(None),
            KeyCode::Enter => return Some(self.chosen()),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Char('p') if ctrl => self.move_sel(-1),
            KeyCode::Char('n') if ctrl => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(-10_000),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(10_000),
            KeyCode::Left | KeyCode::Char('h') => self.set_level(true),
            KeyCode::Right | KeyCode::Char('l') => self.set_level(false),
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
            MouseEventKind::ScrollUp => self.move_sel(-1),
            MouseEventKind::ScrollDown => self.move_sel(1),
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = self
                    .hits
                    .iter()
                    .find(|(y, x0, x1, _)| *y == m.row && m.column >= *x0 && m.column < *x1);
                match hit.map(|h| &h.3) {
                    Some(Hit::Chip(c)) => return self.chip(*c),
                    Some(Hit::Row(Row::Note(_))) => self.list.show_all = true,
                    Some(Hit::Row(r)) => {
                        let r = *r;
                        if matches!(r, Row::Header(_)) {
                            self.ws_mode = true;
                        } else {
                            self.ws_mode = false;
                        }
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

fn go(g: Go) {
    match g {
        Go::Pane(a) => {
            let _ = herdr::call("workspace.focus", json!({"workspace_id": a.ws}));
            let _ = herdr::call("tab.focus", json!({"tab_id": a.tab}));
            let _ = herdr::call("pane.focus", json!({"pane_id": a.pane}));
        }
        Go::Ws(ws) => {
            let _ = herdr::call("workspace.focus", json!({"workspace_id": ws}));
        }
    }
}

fn caller() -> Option<String> {
    let ctx: Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default()).unwrap_or(Value::Null);
    ctx["focused_pane_id"]
        .as_str()
        .or_else(|| ctx["pane_id"].as_str())
        .map(str::to_owned)
        .or_else(|| {
            let r = herdr::call("pane.list", json!({})).ok()?;
            r["panes"].as_array()?.iter().find(|p| p["focused"] == true)?["pane_id"]
                .as_str()
                .map(str::to_owned)
        })
}

/// statetrail goto-list：在终端里打出同样的列表（全部展开），顺便量收集一遍要多久。
pub fn cmd_list() {
    let t0 = Instant::now();
    let mut cache = Cache::default();
    let groups = collect(&mut cache);
    let t1 = t0.elapsed();
    let again = Instant::now();
    collect(&mut cache);
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

/// 动作（prefix+g）：开浮窗。
pub fn cmd_open() {
    if let Err(e) = herdr::call("plugin.pane.open", json!({"plugin_id": crate::plugin_id(), "entrypoint": "goto"})) {
        eprintln!("statetrail: {e}");
    }
}

/// 浮窗里跑的那个程序。
pub fn run() {
    let mut cache = Cache::default();
    let groups = collect(&mut cache);
    let preview = std::fs::read(pref_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["preview"].as_bool())
        .unwrap_or(true);
    let mut p = Popup {
        list: List {
            groups,
            caller: caller(),
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
        shot: None,
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
            p.list.groups = collect(&mut p.cache);
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
    let _ = execute!(term.backend_mut(), event::DisableMouseCapture, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    if let Some(g) = picked {
        go(g);
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
    fn quiet_ones_fold_under_a_note() {
        let l = list();
        assert_eq!(l.rows(), vec![Row::Header(0), Row::Agent(0, 0), Row::Agent(0, 2), Row::Note(0)]);
        let note: String = render_row(&l, Row::Note(0), 80).spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(note.contains(&t("{n} 已读").replace("{n}", "1")), "{note}");
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
        assert!(s.starts_with("   3  ⏸ WAIT  deploy"), "{s}");
        assert!(
            s.contains(&t("等确认 {d}").replace("{d}", &t("{n}分钟").replace("{n}", "1"))),
            "{s}"
        );
    }

    #[test]
    fn ansi_colours_survive() {
        let l = ansi_line("\x1b[38;2;1;2;3mab\x1b[0m\x1b[1mc\x1b[0m\r");
        assert_eq!(l.spans.len(), 2);
        assert_eq!(l.spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert!(l.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn input_box_is_cut() {
        let rule = "─".repeat(30);
        let lines = vec![
            "hello".to_string(),
            String::new(),
            format!("{rule} proj ─"),
            "❯ ".into(),
            rule.clone(),
            "  auto mode".into(),
        ];
        assert_eq!(strip_chrome(lines), vec!["hello".to_string()]);
    }
}
