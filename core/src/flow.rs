//! 跳转浮窗右上那块：所有 Claude 之间谁给谁传过话（最近 24 小时），画成拓扑图：一个 Claude 一个框，箭头连起来。
//! 用户 2026-11-17：「按照动态的拓扑图来更好一些」「把这个动态图放到所有的 claude 上，右侧下面再放对话轨迹」。
//!
//! 数据是真的：一个 Claude 收到别的会话传来的话，对话记录里有一行 type=user，内容是
//! `<cross-session-message from="uds:/tmp/cc-socks/<发送方 pid>.sock" from-name="<发送方名字>" …>`。
//! 发送方先按 pid 认（<配置目录>/sessions/<pid>.json 里的 sessionId 对上列表里的 Claude），认不出再按名字；
//! 都对不上的（已经关了的会话）也画，名字变暗。只读每份对话记录的末尾 2 MB，文件长度没变就不重读。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::OnceLock;
use unicode_width::UnicodeWidthStr;

use crate::goto::{Agent, State};
use crate::i18n::t;
use crate::theme::colors;
use crate::ui::clip;

const TAIL: u64 = 2 << 20;
const WINDOW: i64 = 24 * 3600;
const FRESH: i64 = 10 * 60; // 十分钟内传过的线用强调色画：「动态」

struct Msg {
    pid: Option<u32>,
    name: String,
    ts: i64,
}

#[derive(Default)]
pub struct Flow {
    msgs: HashMap<PathBuf, (u64, Vec<Msg>)>,
    pid_sid: HashMap<u32, Option<String>>,
}

fn re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"cross-session-message from=\\"uds:[^"\\]*?/(\d+)\.sock\\" from-name=\\"([^"\\]*)\\""#).unwrap())
}

fn ts_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""timestamp":"([^"]+)""#).unwrap())
}

fn scan(path: &PathBuf, size: u64) -> Vec<Msg> {
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let start = size.saturating_sub(TAIL);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(size - start).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .filter(|l| l.contains("\"type\":\"user\"") && l.contains("cross-session-message from="))
        .filter_map(|l| {
            let c = re().captures(l)?;
            let ts = ts_re()
                .captures(l)
                .and_then(|m| chrono::DateTime::parse_from_rfc3339(&m[1]).ok())
                .map(|d| d.timestamp())?;
            Some(Msg { pid: c[1].parse().ok(), name: c[2].to_owned(), ts })
        })
        .collect()
}

impl Flow {
    fn sid_of(&mut self, pid: u32) -> Option<String> {
        self.pid_sid
            .entry(pid)
            .or_insert_with(|| {
                crate::claude::config_dirs().into_iter().find_map(|d| {
                    let b = std::fs::read(d.join("sessions").join(format!("{pid}.json"))).ok()?;
                    let v: serde_json::Value = serde_json::from_slice(&b).ok()?;
                    v["sessionId"].as_str().map(str::to_owned)
                })
            })
            .clone()
    }

    /// 最近 24 小时谁给谁传过话：(发送方, 收到的) → (几次, 最近一次)。节点是窗格 id，认不出的发送方是「~名字」。
    fn edges(&mut self, agents: &[Agent], now: i64) -> HashMap<(String, String), (usize, i64)> {
        let mut edges: HashMap<(String, String), (usize, i64)> = HashMap::new();
        for a in agents {
            let Some(path) = &a.path else { continue };
            let size = std::fs::metadata(path).map_or(0, |m| m.len());
            if self.msgs.get(path).is_none_or(|e| e.0 != size) {
                self.msgs.insert(path.clone(), (size, scan(path, size)));
            }
            let senders: Vec<(Option<u32>, String, i64)> = self.msgs[path]
                .1
                .iter()
                .filter(|m| now - m.ts < WINDOW)
                .map(|m| (m.pid, m.name.clone(), m.ts))
                .collect();
            for (pid, name, ts) in senders {
                let by_pid = pid
                    .and_then(|p| self.sid_of(p))
                    .and_then(|sid| agents.iter().find(|b| b.sid.as_deref() == Some(sid.as_str())));
                let from = by_pid
                    .or_else(|| agents.iter().find(|b| b.name == name))
                    .map_or_else(|| format!("~{name}"), |b| b.pane.clone());
                if from == a.pane {
                    continue;
                }
                let e = edges.entry((from, a.pane.clone())).or_insert((0, 0));
                e.0 += 1;
                e.1 = e.1.max(ts);
            }
        }
        edges
    }

    /// 画成拓扑图（用户 2026-11-17：「你这个不是拓扑图吧？你这个是 list 列表吧，而且我看不太懂」）：
    /// 每个 Claude 一个框，按传话方向从左往右分几列（只发的在最左），框和框之间是带箭头的线，线上写传了几次。
    /// 框的边框是它的状态色，上边框写它在哪个 session；选中的那个框用强调色加粗。十分钟内传过的线是亮的。
    /// 返回画好的行，和每个框占的地方（行、起止列、窗格 id），点了选中它。
    pub fn draw(
        &mut self,
        agents: &[Agent],
        labels: &HashMap<String, String>,
        sel: Option<&str>,
        width: usize,
        height: usize,
    ) -> (Vec<Line<'static>>, Vec<(u16, u16, u16, String)>) {
        let c = colors();
        let grey = Style::default().fg(c.grey);
        let now = chrono::Utc::now().timestamp();
        let edges = self.edges(agents, now);
        let mut out: Vec<Line<'static>> = vec![Line::from(Span::styled(t("框是 Claude，箭头从传话的指向收话的，数字是传了几次，亮的线是十分钟内传过的"), grey))];
        if edges.is_empty() {
            out.push(Line::default());
            out.push(Line::from(Span::styled(t("最近一天这些 Claude 之间没有传过话"), grey)));
            return (out, Vec::new());
        }

        // ---- 分列：去掉成环的那条（深搜碰到还在栈上的），剩下的按最长路径分列 ----
        let mut nodes: Vec<String> = edges.keys().flat_map(|(a, b)| [a.clone(), b.clone()]).collect();
        nodes.sort();
        nodes.dedup();
        let out_w = |n: &String| edges.iter().filter(|((a, _), _)| a == n).map(|(_, v)| v.0).sum::<usize>();
        nodes.sort_by_key(|n| (edges.keys().any(|(_, b)| b == n), std::cmp::Reverse(out_w(n)), n.clone()));
        let mut back: HashSet<(String, String)> = HashSet::new();
        let mut state: HashMap<String, u8> = HashMap::new(); // 1 在栈上，2 走完
        fn dfs(n: &str, edges: &HashMap<(String, String), (usize, i64)>, st: &mut HashMap<String, u8>, back: &mut HashSet<(String, String)>) {
            st.insert(n.to_owned(), 1);
            let mut kids: Vec<&String> = edges.keys().filter(|(a, _)| a == n).map(|(_, b)| b).collect();
            kids.sort();
            for b in kids {
                match st.get(b) {
                    Some(1) => {
                        back.insert((n.to_owned(), b.clone()));
                    }
                    Some(_) => {}
                    None => dfs(b, edges, st, back),
                }
            }
            st.insert(n.to_owned(), 2);
        }
        for n in &nodes {
            if !state.contains_key(n) {
                dfs(n, &edges, &mut state, &mut back);
            }
        }
        let fwd: Vec<(&String, &String)> = edges.keys().filter(|e| !back.contains(*e)).map(|(a, b)| (a, b)).collect();
        let mut layer: HashMap<&String, usize> = nodes.iter().map(|n| (n, 0)).collect();
        for _ in 0..nodes.len() {
            for (a, b) in &fwd {
                let want = layer[a] + 1;
                if layer[b] < want {
                    layer.insert(b, want);
                }
            }
        }
        let cols = layer.values().max().map_or(1, |m| m + 1);
        let mut by_col: Vec<Vec<&String>> = vec![Vec::new(); cols];
        for n in &nodes {
            by_col[layer[n]].push(n);
        }
        // 每列里按上一列连过来的那些的平均位置排，线少交叉
        for l in 1..cols {
            let prev = by_col[l - 1].clone();
            let pos = |n: &String| {
                let ps: Vec<usize> = fwd.iter().filter(|(_, b)| *b == n).filter_map(|(a, _)| prev.iter().position(|p| p == a)).collect();
                if ps.is_empty() { usize::MAX } else { ps.iter().sum::<usize>() * 100 / ps.len() }
            };
            by_col[l].sort_by_key(|n| pos(n));
        }

        // ---- 位置：框宽按能放下的算，列之间留 7 格走线 ----
        const GAP: usize = 7;
        let name_w = nodes.iter().map(|n| self_name(agents, n).width()).max().unwrap_or(4);
        let bw = (name_w + 5).min(22).min(width.saturating_sub(GAP * (cols - 1)) / cols).max(8);
        let rows_n = by_col.iter().map(Vec::len).max().unwrap_or(1);
        let gh = (rows_n * 4).saturating_sub(1).min(height.saturating_sub(out.len()));
        let gw = (cols * bw + (cols - 1) * GAP).min(width);
        let mut at: HashMap<&String, (usize, usize)> = HashMap::new();
        for (l, col) in by_col.iter().enumerate() {
            let top = (rows_n - col.len()) * 2;
            for (i, n) in col.iter().enumerate() {
                at.insert(n, (l * (bw + GAP), top + i * 4));
            }
        }

        // ---- 画布：先画线（每格记上下左右四个方向），再画框盖在上面 ----
        let mut bits = vec![vec![0u8; gw]; gh];
        let mut lit = vec![vec![false; gw]; gh];
        let mut text: Vec<Vec<Option<(String, Style)>>> = vec![vec![None; gw]; gh];
        let (u, d, lft, r) = (1u8, 2u8, 4u8, 8u8);
        let hseg = |bits: &mut Vec<Vec<u8>>, lit: &mut Vec<Vec<bool>>, y: usize, x0: usize, x1: usize, hot: bool| {
            let (a, b) = (x0.min(x1), x0.max(x1));
            for x in a..=b {
                if y < gh && x < gw {
                    if x > a { bits[y][x] |= lft; }
                    if x < b { bits[y][x] |= r; }
                    lit[y][x] |= hot;
                }
            }
        };
        let vseg = |bits: &mut Vec<Vec<u8>>, lit: &mut Vec<Vec<bool>>, x: usize, y0: usize, y1: usize, hot: bool| {
            let (a, b) = (y0.min(y1), y0.max(y1));
            for y in a..=b {
                if y < gh && x < gw {
                    if y > a { bits[y][x] |= u; }
                    if y < b { bits[y][x] |= d; }
                    lit[y][x] |= hot;
                }
            }
        };
        let mut sorted: Vec<(&(String, String), &(usize, i64))> = edges.iter().filter(|(e, _)| !back.contains(*e)).collect();
        sorted.sort_by_key(|(e, _)| (*e).clone());
        for ((a, b), (n, ts)) in sorted {
            let ((sx, sy), (tx, ty)) = (at[a], at[b]);
            let hot = now - ts < FRESH;
            let (y0, y1) = (sy + 1, ty + 1);
            let start = sx + bw; // 出发框右边那一格
            let cx = tx.saturating_sub(5); // 收话框前面那条竖线
            hseg(&mut bits, &mut lit, y0, start, cx, hot);
            if start <= cx {
                bits[y0.min(gh - 1)][start.min(gw - 1)] |= lft;
            }
            vseg(&mut bits, &mut lit, cx, y0, y1, hot);
            hseg(&mut bits, &mut lit, y1, cx, tx.saturating_sub(1), hot);
            // 数字和箭头写在最后一段上：竖线 ─ 数字 ─ ▶
            let st = if hot { Style::default().fg(c.accent).add_modifier(Modifier::BOLD) } else { grey };
            let num = n.to_string();
            for (k, ch) in num.chars().enumerate() {
                let x = cx + 1 + k;
                if y1 < gh && x + 1 < tx {
                    text[y1][x] = Some((ch.to_string(), st));
                }
            }
            if y1 < gh && tx >= 1 && tx - 1 < gw {
                text[y1][tx - 1] = Some(("▶".into(), if hot { Style::default().fg(c.accent) } else { Style::default().fg(c.dim) }));
            }
        }
        let mut hits = Vec::new();
        for n in &nodes {
            let (x, y) = at[n];
            if y + 2 >= gh || x + bw > gw {
                continue;
            }
            let agent = agents.iter().find(|a| &a.pane == n);
            let on = agent.is_some_and(|a| Some(a.pane.as_str()) == sel);
            let (icon, ist) = agent.map_or(("◌", Style::default().fg(c.dim)), |a| state_icon(a.state));
            let mut border = agent.map_or(Style::default().fg(c.dim), |a| state_icon(a.state).1.remove_modifier(Modifier::BOLD));
            if on {
                border = Style::default().fg(c.accent).add_modifier(Modifier::BOLD);
            }
            let ws = agent.and_then(|a| labels.get(&a.ws)).cloned().unwrap_or_default();
            // 上边框：╭ news ────╮
            let tag = if ws.is_empty() { String::new() } else { format!(" {} ", clip(&ws, bw.saturating_sub(5), true)) };
            let mut top = vec![("╭".to_string(), border)];
            top.extend(tag.chars().map(|ch| (ch.to_string(), grey)));
            while top.iter().map(|(s, _)| s.width()).sum::<usize>() < bw - 1 {
                top.push(("─".into(), border));
            }
            top.push(("╮".into(), border));
            let name = clip(&self_name(agents, n), bw.saturating_sub(5), true);
            let name_st = if on {
                Style::default().fg(c.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else if agent.is_some() {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(c.dim)
            };
            let mut mid = vec![("│".to_string(), border), (" ".into(), border), (icon.to_string(), ist), (" ".into(), border)];
            mid.extend(name.chars().map(|ch| (ch.to_string(), name_st)));
            while mid.iter().map(|(s, _)| s.width()).sum::<usize>() < bw - 1 {
                mid.push((" ".into(), border));
            }
            mid.push(("│".into(), border));
            let bot: Vec<(String, Style)> = std::iter::once(("╰".to_string(), border))
                .chain((0..bw - 2).map(|_| ("─".to_string(), border)))
                .chain(std::iter::once(("╯".to_string(), border)))
                .collect();
            for (dy, cells) in [top, mid, bot].into_iter().enumerate() {
                let mut xx = x;
                for (s, st) in cells {
                    let w = s.width();
                    if xx < gw {
                        bits[y + dy][xx] = 0;
                        text[y + dy][xx] = Some((s, st));
                        for k in 1..w {
                            if xx + k < gw {
                                bits[y + dy][xx + k] = 0;
                                text[y + dy][xx + k] = Some((String::new(), st)); // 宽字的后半格
                            }
                        }
                    }
                    xx += w;
                }
            }
            if let Some(a) = agent {
                for dy in 0..3 {
                    hits.push(((out.len() + 1 + y + dy) as u16, x as u16, (x + bw) as u16, a.pane.clone()));
                }
            }
        }
        // ---- 画布变成一行行 ----
        out.push(Line::default());
        for y in 0..gh {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for x in 0..gw {
                let (s, st) = match &text[y][x] {
                    Some((s, st)) => (s.clone(), *st),
                    None => {
                        let b = bits[y][x];
                        let ch = match (b & u != 0, b & d != 0, b & lft != 0, b & r != 0) {
                            (false, false, false, false) => " ",
                            (_, _, true, true) if b & (u | d) == 0 => "─",
                            (false, false, _, _) => "─",
                            (true, true, false, false) => "│",
                            (_, _, false, false) => "│",
                            (false, true, false, true) => "╭",
                            (false, true, true, false) => "╮",
                            (true, false, false, true) => "╰",
                            (true, false, true, false) => "╯",
                            (true, true, false, true) => "├",
                            (true, true, true, false) => "┤",
                            (false, true, true, true) => "┬",
                            (true, false, true, true) => "┴",
                            (true, true, true, true) => "┼",
                        };
                        let st = if lit[y][x] { Style::default().fg(c.accent) } else { Style::default().fg(c.dim) };
                        (ch.to_string(), st)
                    }
                };
                spans.push(Span::styled(s, st));
            }
            out.push(Line::from(spans));
        }
        // 没传过话的
        let quiet: Vec<&Agent> = agents.iter().filter(|a| !nodes.contains(&a.pane)).collect();
        if !quiet.is_empty() {
            out.push(Line::default());
            let head = t("没传过话：");
            let mut spans = vec![Span::styled(head.clone(), grey)];
            let mut used = head.width();
            for (i, a) in quiet.iter().enumerate() {
                let (icon, st) = state_icon(a.state);
                let piece = format!("{icon} {}  ", a.name);
                let more = format!("+{}", quiet.len() - i);
                if used + piece.width() + more.width() > width {
                    spans.push(Span::styled(more, grey));
                    break;
                }
                spans.push(Span::styled(format!("{icon} "), st));
                spans.push(Span::styled(format!("{}  ", a.name), grey));
                used += piece.width();
            }
            out.push(Line::from(spans));
        }
        (out, hits)
    }
}

fn self_name(agents: &[Agent], key: &str) -> String {
    agents.iter().find(|a| a.pane == key).map_or_else(|| key.trim_start_matches('~').to_owned(), |a| a.name.clone())
}

fn state_icon(s: State) -> (&'static str, Style) {
    let c = colors();
    match s {
        State::Wait => ("⏸", Style::default().fg(c.alert)),
        State::Done => ("✔", Style::default().fg(c.done)),
        State::Run => ("▶", Style::default().fg(c.busy)),
        State::Read => ("✓", Style::default().fg(c.time)),
        State::Stale => ("✔", Style::default().add_modifier(Modifier::DIM)),
    }
}
