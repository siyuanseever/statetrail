//! 跳转浮窗右上那块：所有 Claude 之间谁给谁传过话，画成拓扑图：一个 Claude 一个框，箭头连起来。
//! 用户 2026-11-17：「按照动态的拓扑图来更好一些」「把这个动态图放到所有的 claude 上，右侧下面再放对话轨迹」。
//! 同一天又要：图大一点、双向的要画出来、点线能看传的原话、能切最近 1 / 7 / 30 天（「就跟我们后台上差不多」）。
//!
//! 数据是真的：一个 Claude 收到别的会话传来的话，对话记录里有一行 type=user，内容是
//! `<cross-session-message from="uds:/tmp/cc-socks/<发送方 pid>.sock" from-name="<发送方名字>" …>原话</cross-session-message>`。
//! 发送方先按 pid 认（<配置目录>/sessions/<pid>.json 里的 sessionId 对上列表里的 Claude），认不出再按名字；
//! 都对不上的（已经关了的会话）也画，名字变暗。
//!
//! 对话记录大的有 1.7 GB，不能每次整份读：从末尾往前一块块读，读到比要看的时间段更早就停；
//! 读过的范围和找到的话存进状态目录的 flow.json，下次打开浮窗只读新长出来的那一截。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use regex::bytes::Regex as BRegex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::OnceLock;
use unicode_width::UnicodeWidthStr;

use crate::goto::{Agent, State};
use crate::i18n::t;
use crate::theme::colors;
use crate::ui::clip;

const CHUNK: u64 = 8 << 20;
const FRESH: i64 = 10 * 60; // 十分钟内传过的线用强调色画：「动态」
/// 能切的时间段（天）
pub const RANGES: [i64; 3] = [1, 7, 30];

#[derive(Serialize, Deserialize, Clone)]
struct Msg {
    pid: Option<u32>,
    name: String,
    ts: i64,
    text: String,
}

/// 一份对话记录读过的范围 [from, end)、读到的最早时间、里面找到的话
#[derive(Serialize, Deserialize, Default)]
struct Scan {
    from: u64,
    end: u64,
    oldest: i64,
    msgs: Vec<Msg>,
}

/// 一句传过的话：from / to 是节点（窗格 id，认不出的发送方是「~名字」）
pub struct Said {
    pub from: String,
    pub to: String,
    pub ts: i64,
    pub text: String,
}

/// 图上点到的东西
pub enum Spot {
    Node(String),
    Link(String, String),
}

#[derive(Default)]
pub struct Flow {
    files: HashMap<PathBuf, Scan>,
    loaded: bool,
    dirty: bool,
    touched: HashSet<PathBuf>,
    pid_sid: HashMap<u32, Option<String>>,
}

fn hit_re() -> &'static BRegex {
    static RE: OnceLock<BRegex> = OnceLock::new();
    RE.get_or_init(|| BRegex::new(r"cross-session-message from=").unwrap())
}

fn user_re() -> &'static BRegex {
    static RE: OnceLock<BRegex> = OnceLock::new();
    RE.get_or_init(|| BRegex::new(r#""type":"user""#).unwrap())
}

fn ts_re() -> &'static BRegex {
    static RE: OnceLock<BRegex> = OnceLock::new();
    RE.get_or_init(|| BRegex::new(r#""timestamp":"([^"]+)""#).unwrap())
}

fn body_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<cross-session-message from="(?:uds:[^"]*?/(\d+)\.sock|[^"]*)" from-name="([^"]*)"[^>]*>(.*?)</cross-session-message>"#).unwrap()
    })
}

fn rfc(s: &[u8]) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(std::str::from_utf8(s).ok()?).ok().map(|d| d.timestamp())
}

fn read_at(f: &mut std::fs::File, lo: u64, hi: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(lo)).is_ok() {
        let _ = f.take(hi - lo).read_to_end(&mut buf);
    }
    buf
}

/// 一段整行的字节里找收到的话：先按字面找标记（快），再把那一行当 JSON 解开。
fn parse_msgs(buf: &[u8]) -> Vec<Msg> {
    let mut out = Vec::new();
    let mut done = 0;
    for m in hit_re().find_iter(buf) {
        if m.start() < done {
            continue;
        }
        let s = buf[..m.start()].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let e = buf[m.end()..].iter().position(|&b| b == b'\n').map_or(buf.len(), |i| m.end() + i);
        done = e;
        let line = &buf[s..e];
        if !user_re().is_match(line) {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
        if v["type"] != "user" {
            continue;
        }
        let text = match &v["message"]["content"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
            _ => continue,
        };
        let Some(c) = body_re().captures(&text) else { continue };
        let Some(ts) = v["timestamp"].as_str().and_then(|s| rfc(s.as_bytes())) else { continue };
        out.push(Msg {
            pid: c.get(1).and_then(|p| p.as_str().parse().ok()),
            name: c[2].to_owned(),
            ts,
            text: c[3].trim().to_owned(),
        });
    }
    out
}

/// 一条线：两个节点之间一条，左头 l、右头 r；lr 是 l 传给 r 的次数，rl 反过来
struct Link {
    l: String,
    r: String,
    lr: usize,
    rl: usize,
    last: i64,
}

/// 排好的图：节点、线、每条线在两头框上的出口、框的大小和位置、每条线用哪条竖线；fits 是放不放得下
struct Lay {
    nodes: Vec<String>,
    links: Vec<Link>,
    rport: Vec<usize>,
    lport: Vec<usize>,
    bh: HashMap<String, usize>,
    bw: usize,
    gw: usize,
    gh: usize,
    at: HashMap<String, (usize, usize)>,
    chan: Vec<usize>,
    fits: bool,
}

fn layout(edges: &HashMap<(String, String), (usize, i64)>, agents: &[Agent], width: usize, height: usize) -> Lay {
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
            dfs(n, edges, &mut state, &mut back);
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
    let rank = |n: &String| (layer[n], by_col[layer[n]].iter().position(|m| *m == n).unwrap_or(0));

    // ---- 线：两个节点之间一条，左头 l、右头 r；lr 是 l 传给 r 的次数，rl 反过来 ----
    let mut links: Vec<Link> = Vec::new();
    for ((a, b), (n, ts)) in edges {
        let (l, r) = if layer[a] < layer[b] { (a, b) } else { (b, a) };
        let i = match links.iter().position(|k| &k.l == l && &k.r == r) {
            Some(i) => i,
            None => {
                links.push(Link { l: l.clone(), r: r.clone(), lr: 0, rl: 0, last: 0 });
                links.len() - 1
            }
        };
        if l == a { links[i].lr += n } else { links[i].rl += n }
        links[i].last = links[i].last.max(*ts);
    }
    links.sort_by_key(|k| (rank(&k.l), rank(&k.r)));
    // 每条线在两头框上的出口（第几行）：按另一头的位置排
    let mut rport = vec![0; links.len()];
    let mut lport = vec![0; links.len()];
    let mut ports: HashMap<&String, (usize, usize)> = HashMap::new(); // 左边几个口、右边几个口
    for n in &nodes {
        let mut right: Vec<usize> = (0..links.len()).filter(|&i| &links[i].l == n).collect();
        right.sort_by_key(|&i| rank(&links[i].r));
        for (k, &i) in right.iter().enumerate() {
            rport[i] = k;
        }
        let mut left: Vec<usize> = (0..links.len()).filter(|&i| &links[i].r == n).collect();
        left.sort_by_key(|&i| (std::cmp::Reverse(layer[&links[i].l]), rank(&links[i].l)));
        for (k, &i) in left.iter().enumerate() {
            lport[i] = k;
        }
        ports.insert(n, (left.len(), right.len()));
    }
    let bh = |n: &String| 2 + ports[n].0.max(ports[n].1).max(1);

    // ---- 位置：框按能放下的宽，列之间的空按线的多少和给的宽度铺开；竖着按给的高度铺开 ----
    let name_w = nodes.iter().map(|n| name_of(agents, n).width()).max().unwrap_or(4);
    let mut chans = vec![0usize; cols]; // 每列左边要几条竖线
    for k in &links {
        chans[layer[&k.r]] += 1;
    }
    let need = 8 + chans.iter().max().copied().unwrap_or(1); // 竖线 + 两头的数字和箭头
    let bw = (name_w + 6).min(24).min(width.saturating_sub(need * (cols - 1)) / cols).max(8);
    let gap = (width.saturating_sub(bw * cols) / (cols - 1).max(1)).clamp(need.min(9), need + 10);
    let gw = (cols * bw + (cols - 1) * gap).min(width);
    let col_h = |col: &Vec<&String>, vgap: usize| col.iter().map(|n| bh(n)).sum::<usize>() + col.len().saturating_sub(1) * vgap;
    let room = height.saturating_sub(4); // 说明一行、空行、底下没传过话的两行
    let vgap = by_col
        .iter()
        .filter(|col| col.len() > 1)
        .map(|col| (room.saturating_sub(col_h(col, 0)) / (col.len() - 1)).clamp(1, 3))
        .min()
        .unwrap_or(2);
    let gh = by_col.iter().map(|col| col_h(col, vgap)).max().unwrap_or(3).min(room.max(3));
    let mut at: HashMap<&String, (usize, usize)> = HashMap::new();
    for (l, col) in by_col.iter().enumerate() {
        let mut y = gh.saturating_sub(col_h(col, vgap)) / 2;
        for n in col {
            at.insert(n, (l * (bw + gap), y));
            y += bh(n) + vgap;
        }
    }
    // 进同一列的线各用一条竖线，离框近的给落点靠上的
    let mut chan = vec![0usize; links.len()];
    for l in 0..cols {
        let mut into: Vec<usize> = (0..links.len()).filter(|&i| layer[&links[i].r] == l).collect();
        into.sort_by_key(|&i| at[&links[i].r].1 + lport[i]);
        for (k, &i) in into.iter().enumerate() {
            chan[i] = k.min(gap.saturating_sub(9));
        }
    }

    // 放得下：框宽够写下名字（最多 16 格），竖着排得开
    let fits = bw >= (name_w + 6).min(16) && by_col.iter().all(|col| col_h(col, 1) <= room);
    Lay {
        bh: nodes.iter().map(|n| (n.clone(), bh(n))).collect(),
        at: at.into_iter().map(|(k, v)| (k.clone(), v)).collect(),
        nodes: nodes.clone(),
        links,
        rport,
        lport,
        bw,
        gw,
        gh,
        chan,
        fits,
    }
}

fn cache_path() -> PathBuf {
    crate::state_dir().join("flow.json")
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

    /// 让这份对话记录读到 since 之前：新长出来的整行读掉，往前不够早就再往前读一块。
    fn ensure(&mut self, path: &PathBuf, since: i64) {
        if !self.loaded {
            self.loaded = true;
            if let Some(m) = std::fs::read(cache_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
                self.files = m;
            }
        }
        self.touched.insert(path.clone());
        let size = std::fs::metadata(path).map_or(0, |m| m.len());
        let s = self.files.entry(path.clone()).or_default();
        if size < s.end {
            *s = Scan::default(); // 文件变短了：被重写过，从头来
        }
        if s.oldest == 0 {
            (s.from, s.end, s.oldest) = (size, size, i64::MAX);
        }
        if size == s.end && (s.from == 0 || s.oldest < since) {
            return;
        }
        let Ok(mut f) = std::fs::File::open(path) else { return };
        self.dirty = true;
        if size > s.end {
            let buf = read_at(&mut f, s.end, size);
            let cut = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            s.msgs.extend(parse_msgs(&buf[..cut]));
            s.end += cut as u64;
        }
        while s.from > 0 && s.oldest >= since {
            let lo = s.from.saturating_sub(CHUNK);
            let buf = read_at(&mut f, lo, s.from);
            // 这一块开头多半是半行，跳到第一个换行后面；整块都在一行里（大图片之类）就整块跳过
            let skip = if lo == 0 {
                0
            } else {
                match buf.iter().position(|&b| b == b'\n') {
                    Some(i) if i + 1 < buf.len() => i + 1,
                    _ => {
                        s.from = lo;
                        continue;
                    }
                }
            };
            let part = &buf[skip..];
            if let Some(ts) = ts_re().captures(part).and_then(|c| rfc(&c[1])) {
                s.oldest = s.oldest.min(ts);
            }
            let mut older = parse_msgs(part);
            older.append(&mut s.msgs);
            s.msgs = older;
            s.from = lo + skip as u64;
        }
    }

    /// 读过的存下来，下次打开浮窗只读新的。只留这次看过的那些对话记录。
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let keep: HashMap<&PathBuf, &Scan> = self.files.iter().filter(|(p, _)| self.touched.contains(*p)).collect();
        if let Ok(b) = serde_json::to_vec(&keep) {
            let tmp = cache_path().with_extension("tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(tmp, cache_path());
            }
        }
        self.dirty = false;
    }

    /// since 以后开着的这些 Claude 收到的每一句话，发送方认成节点。
    fn said(&mut self, agents: &[Agent], since: i64) -> Vec<Said> {
        let mut out = Vec::new();
        for a in agents {
            let Some(path) = &a.path else { continue };
            self.ensure(path, since);
            let got: Vec<Msg> = self.files[path].msgs.iter().filter(|m| m.ts >= since).cloned().collect();
            for m in got {
                let by_pid = m
                    .pid
                    .and_then(|p| self.sid_of(p))
                    .and_then(|sid| agents.iter().find(|b| b.sid.as_deref() == Some(sid.as_str())));
                let from = by_pid
                    .or_else(|| agents.iter().find(|b| b.name == m.name))
                    .map_or_else(|| format!("~{}", m.name), |b| b.pane.clone());
                if from != a.pane {
                    out.push(Said { from, to: a.pane.clone(), ts: m.ts, text: m.text });
                }
            }
        }
        out
    }

    /// 两个节点之间来回传的话，按时间从早到晚。
    pub fn talk(&mut self, agents: &[Agent], a: &str, b: &str, days: i64) -> Vec<Said> {
        let since = chrono::Utc::now().timestamp() - days * 86400;
        let mut v: Vec<Said> = self
            .said(agents, since)
            .into_iter()
            .filter(|s| (s.from == a && s.to == b) || (s.from == b && s.to == a))
            .collect();
        v.sort_by_key(|s| s.ts);
        v
    }

    /// 画成拓扑图（用户 2026-11-17：「你这个不是拓扑图吧？你这个是 list 列表吧，而且我看不太懂」）：
    /// 每个 Claude 一个框，按传话方向从左往右分几列（只发的在最左），框和框之间是线。
    /// 两个 Claude 之间只画一条线，两头哪头收到过话哪头就有箭头，数字写在箭头边上（双向的两头都有）。
    /// 一条线在框上有自己的一格出口，线多的框就高一点，线不挤在一起。图按给的地方铺开。
    /// 框的边框是它的状态色，上边框写它在哪个 session；选中的框、点开的线用强调色。十分钟内传过的线是亮的。
    /// 返回画好的行，和能点的地方（行、起止列、点到的是框还是线）。
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        agents: &[Agent],
        labels: &HashMap<String, String>,
        sel: Option<&str>,
        open: Option<(&str, &str)>,
        days: i64,
        width: usize,
        height: usize,
    ) -> (Vec<Line<'static>>, Vec<(u16, u16, u16, Spot)>) {
        let c = colors();
        let grey = Style::default().fg(c.grey);
        let now = chrono::Utc::now().timestamp();
        let mut edges: HashMap<(String, String), (usize, i64)> = HashMap::new();
        for s in self.said(agents, now - days * 86400) {
            let e = edges.entry((s.from, s.to)).or_insert((0, 0));
            e.0 += 1;
            e.1 = e.1.max(s.ts);
        }
        let mut out: Vec<Line<'static>> =
            vec![Line::from(Span::styled(t("框是 Claude，箭头指向收到话的那个，数字是传了几次，亮的线是十分钟内传过的。点一条线看传了什么"), grey))];
        if edges.is_empty() {
            out.push(Line::default());
            out.push(Line::from(Span::styled(t("这段时间里这些 Claude 之间没有传过话"), grey)));
            return (out, Vec::new());
        }

        // 全画放不下（时间段长了 Claude 多、线多）就只画传得最多的那些对，传得少的一对对拿掉，底下写拿掉了几对
        let mut kept = edges.clone();
        let mut dropped = 0;
        let lay = loop {
            let lay = layout(&kept, agents, width, height);
            if lay.fits || kept.len() <= 1 {
                break lay;
            }
            let pair = |(a, b): &(String, String)| if a < b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
            let mut sum: HashMap<(String, String), (usize, i64)> = HashMap::new();
            for (e, (n, ts)) in &kept {
                let v = sum.entry(pair(e)).or_insert((0, 0));
                v.0 += n;
                v.1 = v.1.max(*ts);
            }
            let Some(weak) = sum.into_iter().min_by_key(|(p, (n, ts))| (*n, *ts, p.clone())).map(|(p, _)| p) else { break lay };
            kept.retain(|e, _| pair(e) != weak);
            dropped += 1;
        };
        let Lay { nodes, links, rport, lport, bh, bw, gw, gh, at, chan, .. } = lay;
        let bh = |n: &String| bh[n];

        // ---- 画布：先画线（每格记上下左右四个方向），再画框盖在上面 ----
        let mut bits = vec![vec![0u8; gw]; gh];
        let mut lit = vec![vec![0u8; gw]; gh]; // 1 十分钟内传过，2 点开的这条
        let mut text: Vec<Vec<Option<(String, Style)>>> = vec![vec![None; gw]; gh];
        let (u, d, lft, r) = (1u8, 2u8, 4u8, 8u8);
        let mut spots: Vec<(u16, u16, u16, Spot)> = Vec::new();
        let top = out.len() + 1;
        for (i, k) in links.iter().enumerate() {
            let ((sx, sy), (tx, ty)) = (at[&k.l], at[&k.r]);
            let on = open.is_some_and(|(a, b)| (a == k.l && b == k.r) || (a == k.r && b == k.l));
            let glow = if on { 2 } else { u8::from(now - k.last < FRESH) };
            let (y0, y1) = (sy + 1 + rport[i], ty + 1 + lport[i]);
            let start = sx + bw; // 左头框右边那一格
            let cx = tx.saturating_sub(5 + chan[i]); // 这条线的竖线
            let mut mark = |y: usize, x: usize, b: u8| {
                if y < gh && x < gw {
                    bits[y][x] |= b;
                    lit[y][x] = lit[y][x].max(glow);
                }
            };
            for x in start..=cx {
                mark(y0, x, if x > start { lft } else { 0 } | if x < cx { r } else { 0 });
            }
            for y in y0.min(y1)..=y0.max(y1) {
                mark(y, cx, if y > y0.min(y1) { u } else { 0 } | if y < y0.max(y1) { d } else { 0 });
            }
            for x in cx..tx {
                mark(y1, x, if x > cx { lft } else { 0 } | if x + 1 < tx { r } else { 0 });
            }
            if y0 == y1 {
                mark(y0, cx, lft | r);
            }
            // 箭头和数字：右头收到过就是 ─3▶，左头收到过就是 ◀2─
            let st = match glow {
                2 => Style::default().fg(c.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED),
                1 => Style::default().fg(c.accent).add_modifier(Modifier::BOLD),
                _ => grey,
            };
            let head = if glow > 0 { Style::default().fg(c.accent) } else { Style::default().fg(c.dim) };
            let mut put = |y: usize, x: usize, s: String, st: Style| {
                if y < gh && x < gw {
                    text[y][x] = Some((s, st));
                }
            };
            if k.lr > 0 {
                let num = k.lr.to_string();
                put(y1, tx - 1, "▶".into(), head);
                for (j, ch) in num.chars().enumerate() {
                    put(y1, tx - 1 - num.len() + j, ch.to_string(), st);
                }
            }
            if k.rl > 0 {
                put(y0, start, "◀".into(), head);
                for (j, ch) in k.rl.to_string().chars().enumerate() {
                    put(y0, start + 1 + j, ch.to_string(), st);
                }
            }
            let link = || Spot::Link(k.l.clone(), k.r.clone());
            spots.push(((top + y0) as u16, start as u16, (cx + 1) as u16, link()));
            spots.push(((top + y1) as u16, cx as u16, tx as u16, link()));
            for y in y0.min(y1)..=y0.max(y1) {
                spots.push(((top + y) as u16, cx as u16, (cx + 1) as u16, link()));
            }
        }
        let mut boxes = Vec::new();
        for n in &nodes {
            let (x, y) = at[n];
            let h = bh(n);
            if y + h > gh || x + bw > gw {
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
            let fill = |mut cells: Vec<(String, Style)>, pad: &str, end: &str| {
                while cells.iter().map(|(s, _)| s.width()).sum::<usize>() < bw - 1 {
                    cells.push((pad.into(), border));
                }
                cells.push((end.into(), border));
                cells
            };
            // 上边框：╭ news ────╮
            let tag = if ws.is_empty() { String::new() } else { format!(" {} ", clip(&ws, bw.saturating_sub(5), true)) };
            let mut first = vec![("╭".to_string(), border)];
            first.extend(tag.chars().map(|ch| (ch.to_string(), grey)));
            let name = clip(&name_of(agents, n), bw.saturating_sub(5), true);
            let name_st = if on {
                Style::default().fg(c.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else if agent.is_some() {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(c.dim)
            };
            let mut mid = vec![("│".to_string(), border), (" ".into(), border), (icon.to_string(), ist), (" ".into(), border)];
            mid.extend(name.chars().map(|ch| (ch.to_string(), name_st)));
            let mut rows = vec![fill(first, "─", "╮"), fill(mid, " ", "│")];
            for _ in 3..h {
                rows.push(fill(vec![("│".to_string(), border)], " ", "│"));
            }
            rows.push(fill(vec![("╰".to_string(), border)], "─", "╯"));
            for (dy, cells) in rows.into_iter().enumerate() {
                let mut xx = x;
                for (s, st) in cells {
                    let w = s.width();
                    for k in 0..w.max(1) {
                        if xx + k < gw {
                            bits[y + dy][xx + k] = 0;
                            text[y + dy][xx + k] = Some((if k == 0 { s.clone() } else { String::new() }, st)); // 宽字的后半格是空串
                        }
                    }
                    xx += w;
                }
            }
            if let Some(a) = agent {
                for dy in 0..h {
                    boxes.push(((top + y + dy) as u16, x as u16, (x + bw) as u16, Spot::Node(a.pane.clone())));
                }
            }
        }
        // 框排在前面：点到框和线重叠的地方算点框
        boxes.extend(spots);
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
                            (false, false, _, _) => "─",
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
                        let st = match lit[y][x] {
                            2 => Style::default().fg(c.accent).add_modifier(Modifier::BOLD),
                            1 => Style::default().fg(c.accent),
                            _ => Style::default().fg(c.dim),
                        };
                        (ch.to_string(), st)
                    }
                };
                spans.push(Span::styled(s, st));
            }
            out.push(Line::from(spans));
        }
        if dropped > 0 {
            out.push(Line::from(Span::styled(format!("{} {dropped} {}", t("还有"), t("对传得少的放不下没画，选短一点的时间段能看全")), grey)));
        }
        // 没传过话的
        let quiet: Vec<&Agent> = agents.iter().filter(|a| !edges.keys().any(|(x, y)| *x == a.pane || *y == a.pane)).collect();
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
        (out, boxes)
    }
}

/// 节点的名字：开着的 Claude 用它的名字，认不出的发送方去掉前面的「~」。
pub fn name_of(agents: &[Agent], key: &str) -> String {
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
