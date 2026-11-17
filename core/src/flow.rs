//! 跳转浮窗右上那块：所有 Claude 之间谁给谁传过话（最近 24 小时），画成一行一个节点的树。
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
const INDENT: usize = 4;

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

/// 图里的一行：画好的 Line，和这一行是哪个 Claude（点了选中它）
pub struct Row {
    pub line: Line<'static>,
    pub pane: Option<String>,
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

    /// 画成一行一行：每棵树从只发不收的那个开始，孩子缩进一级，线上写传了几次；没传过话的放最后一行。
    pub fn rows(&mut self, agents: &[Agent], labels: &HashMap<String, String>, sel: Option<&str>, width: usize) -> Vec<Row> {
        let c = colors();
        let grey = Style::default().fg(c.grey);
        let dim = Style::default().fg(c.dim);
        let now = chrono::Utc::now().timestamp();

        // 收：每个有对话记录的 Claude 收到过谁的话
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

        let mut out: Vec<Row> = Vec::new();
        if edges.is_empty() {
            out.push(Row { line: Line::from(Span::styled(t("最近一天这些 Claude 之间没有传过话"), grey)), pane: None });
            return out;
        }
        // 排成树：根是只发不收的（没有就挑发得最多的），孩子按传的次数排；已经画过的不再画第二遍
        let nodes: HashSet<&String> = edges.keys().flat_map(|(a, b)| [a, b]).collect();
        let out_w = |n: &String| edges.iter().filter(|((a, _), _)| a == n).map(|(_, v)| v.0).sum::<usize>();
        let has_in = |n: &String| edges.keys().any(|(_, b)| b == n);
        let mut order: Vec<&String> = nodes.iter().copied().collect();
        order.sort_by_key(|n| (has_in(n), std::cmp::Reverse(out_w(n)), (*n).clone()));
        let mut placed: HashSet<String> = HashSet::new();
        // (节点, 从父节点来的线 (次数, 最近一次), 前缀里每一级是不是还有后面的兄弟)
        let mut items: Vec<(String, Option<(usize, i64)>, Vec<bool>)> = Vec::new();
        fn walk(
            n: &str,
            via: Option<(usize, i64)>,
            trail: Vec<bool>,
            edges: &HashMap<(String, String), (usize, i64)>,
            placed: &mut HashSet<String>,
            items: &mut Vec<(String, Option<(usize, i64)>, Vec<bool>)>,
        ) {
            items.push((n.to_owned(), via, trail.clone()));
            let mut kids: Vec<(&String, (usize, i64))> = edges
                .iter()
                .filter(|((a, b), _)| a == n && !placed.contains(b))
                .map(|((_, b), v)| (b, *v))
                .collect();
            kids.sort_by_key(|(b, v)| (std::cmp::Reverse(v.0), (*b).clone()));
            for (b, _) in &kids {
                placed.insert((*b).clone());
            }
            let last = kids.len().saturating_sub(1);
            for (i, (b, v)) in kids.into_iter().enumerate() {
                let mut tr = trail.clone();
                tr.push(i < last);
                walk(b, Some(v), tr, edges, placed, items);
            }
        }
        for n in order {
            if placed.insert(n.clone()) {
                if !items.is_empty() {
                    items.push((String::new(), None, Vec::new())); // 两棵树之间空一行
                }
                walk(n, None, Vec::new(), &edges, &mut placed, &mut items);
            }
        }

        for (key, via, trail) in items {
            if key.is_empty() {
                out.push(Row { line: Line::default(), pane: None });
                continue;
            }
            let mut spans: Vec<Span> = Vec::new();
            // 前面几级的竖线
            for &more in trail.iter().take(trail.len().saturating_sub(1)) {
                spans.push(Span::styled(if more { "│" } else { " " }.to_owned() + &" ".repeat(INDENT + 4), dim));
            }
            if let (Some((n, ts)), Some(&more)) = (via, trail.last()) {
                let st = if now - ts < FRESH { Style::default().fg(c.accent) } else { dim };
                let count = format!("{n}");
                spans.push(Span::styled(if more { "├─" } else { "└─" }, dim));
                spans.push(Span::styled(count.clone(), st));
                spans.push(Span::styled(format!("{}▶ ", "─".repeat((INDENT + 1).saturating_sub(count.width()))), st));
            }
            let agent = agents.iter().find(|a| a.pane == key);
            let on = agent.is_some_and(|a| Some(a.pane.as_str()) == sel);
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            let room = width.saturating_sub(used);
            match agent {
                Some(a) => {
                    let (icon, st) = state_icon(a.state);
                    let ws = labels.get(&a.ws).cloned().unwrap_or_default();
                    let name_st = if on {
                        Style::default().fg(c.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED)
                    } else {
                        Style::default().add_modifier(Modifier::BOLD)
                    };
                    spans.push(Span::styled(format!("{icon} "), st));
                    let name = clip(&a.name, room.saturating_sub(2), true);
                    let rest = room.saturating_sub(2 + name.width() + 2);
                    spans.push(Span::styled(name, name_st));
                    if rest > 3 && !ws.is_empty() {
                        spans.push(Span::styled(format!("  {}", clip(&ws, rest, true)), grey));
                    }
                }
                None => {
                    let name = key.trim_start_matches('~');
                    spans.push(Span::styled(clip(&format!("◌ {name}"), room, true), dim));
                }
            }
            out.push(Row { line: Line::from(spans), pane: agent.map(|a| a.pane.clone()) });
        }
        // 没传过话的
        let quiet: Vec<&Agent> = agents.iter().filter(|a| !nodes.contains(&a.pane)).collect();
        if !quiet.is_empty() {
            out.push(Row { line: Line::default(), pane: None });
            let head = t("没传过话：");
            let mut spans = vec![Span::styled(head.clone(), grey)];
            let mut used = head.width();
            for (i, a) in quiet.iter().enumerate() {
                let (icon, st) = state_icon(a.state);
                let piece = format!("{icon} {}", a.name);
                let more = format!(" +{}", quiet.len() - i);
                if used + piece.width() + 2 + more.width() > width {
                    spans.push(Span::styled(more, grey));
                    break;
                }
                spans.push(Span::styled(format!("{icon} "), st));
                let on = Some(a.pane.as_str()) == sel;
                spans.push(Span::styled(
                    format!("{}  ", a.name),
                    if on { Style::default().fg(c.accent).add_modifier(Modifier::REVERSED) } else { grey },
                ));
                used += piece.width() + 2;
            }
            out.push(Row { line: Line::from(spans), pane: None });
        }
        out
    }
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
