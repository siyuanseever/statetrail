//! 画轨迹：表头（第 1 步只有「N 轮」那一行）、一条分隔线、列表。列表自己记滚动位置（不再靠 fzf，也不再读自己的屏幕），
//! 画的时候顺手记下每一行是哪一项，点击就按它认。格式和 Python 版 core/render.py 的 rows_of / narrow_rows_of 一样：
//! 宽版每轮两行（轮号 时间 你说的话 / 回复的开头），轮与轮之间一行虚线；窄版每轮一行（轮号 你说的话）。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::i18n::t;
use crate::transcript::Stop;

pub const GREY: Color = Color::Rgb(108, 108, 108);
pub const DIM: Color = Color::Rgb(88, 88, 88);
pub const BUSY: Color = Color::Rgb(215, 135, 0);
pub const ACCENT: Color = Color::Rgb(0, 135, 215);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Item {
    Fold,
    Stop(usize), // visible() 里的下标
}

pub struct View<'a> {
    pub stops: &'a [&'a Stop],
    pub narrow: bool,
    pub expanded: bool,
    pub opened: Option<&'a str>,
    pub busy: bool,
    pub missing: &'a std::collections::HashSet<String>,
    pub flash: Option<&'a str>,
    pub found: bool, // 找到了这个 Claude 的会话没有
}

impl View<'_> {
    pub fn items(&self) -> Vec<Item> {
        let mut out = Vec::new();
        if self.stops.iter().any(|s| s.compacted) {
            out.push(Item::Fold);
        }
        for (i, s) in self.stops.iter().enumerate() {
            if !s.compacted || self.expanded {
                out.push(Item::Stop(i));
            }
        }
        out
    }

    fn gone(&self) -> usize {
        self.stops.iter().filter(|s| s.compacted).count()
    }
}

/// 按显示宽度截（中文算两格），截了就补个省略号。
pub fn clip(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

fn oneline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in s.lines() {
        let (mut cur, mut w) = (String::new(), 0);
        for c in para.chars() {
            let cw = c.width().unwrap_or(0);
            if w + cw > width {
                lines.push(std::mem::take(&mut cur));
                w = 0;
            }
            cur.push(c);
            w += cw;
        }
        lines.push(cur);
    }
    lines
}

/// 「14:47」，不是今天的写成「10/24 14:47」。
pub fn when(ts: Option<&str>) -> String {
    let Some(d) = ts.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) else {
        return String::new();
    };
    let local = d.with_timezone(&chrono::Local);
    if local.date_naive() == chrono::Local::now().date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%m/%d %H:%M").to_string()
    }
}

/// 一项画出来的几行（不含左边 2 列：指针 + 空格，由 draw 加）。
fn render_item(v: &View, item: Item, room: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(DIM);
    match item {
        Item::Fold => {
            let n = v.gone();
            let text = if v.narrow {
                format!("1–{n} {}", if v.expanded { "−" } else { "+" })
            } else {
                let act = if v.expanded { t("点这里收起") } else { t("点开展开") };
                let tpl = if n > 1 {
                    t("第 1–{n} 轮已压缩 · {act}")
                } else {
                    t("第 1 轮已压缩 · {act}")
                };
                tpl.replace("{n}", &n.to_string()).replace("{act}", &act)
            };
            vec![Line::from(Span::styled(clip(&text, room), dim))]
        }
        Item::Stop(i) => {
            let st = v.stops[i];
            let n = i + 1;
            let ask = if st.kind == "human" {
                oneline(&st.prompt)
            } else {
                t("（{p}）").replace("{p}", &st.prompt)
            };
            let last_busy = v.busy && n == v.stops.len();
            let missing = v.missing.contains(&st.id);
            if v.narrow {
                let style = if last_busy {
                    Style::default().fg(BUSY)
                } else if missing {
                    dim
                } else {
                    Style::default()
                };
                let head = format!("{n} ");
                return vec![Line::from(vec![
                    Span::styled(head.clone(), dim),
                    Span::styled(clip(&ask, room.saturating_sub(head.width())), style),
                ])];
            }
            let mut gone = String::new();
            if st.compacted {
                gone += &format!(" · {}", t("已压缩"));
            }
            if missing {
                gone += &format!(" · {}", t("主界面已无"));
            }
            let meta = format!(" {n} {}{gone}", when(st.ts.as_deref()));
            let reply = if last_busy {
                t("（Claude 还在干活…）")
            } else if st.reply.is_empty() {
                t("（没有文字回复）")
            } else {
                st.reply.clone()
            };
            let opened = v.opened == Some(st.id.as_str()) && st.kind == "human";
            let mut lines = Vec::new();
            if opened {
                lines.push(Line::from(vec![
                    Span::styled(meta.clone(), dim),
                    Span::styled(format!(" {}", t("你说的：")), Style::default().add_modifier(Modifier::BOLD)),
                ]));
                for l in wrap(st.prompt.trim(), room.saturating_sub(4).max(10)) {
                    lines.push(Line::from(format!("   {l}")));
                }
            } else {
                let rest = room.saturating_sub(meta.width() + 1);
                let ask_style = if missing { dim } else { Style::default() };
                lines.push(Line::from(vec![
                    Span::styled(meta, dim),
                    Span::raw(" "),
                    Span::styled(clip(&ask, rest), ask_style),
                ]));
            }
            lines.push(Line::from(Span::styled(clip(&format!("   {reply}"), room), dim)));
            lines
        }
    }
}

/// 每项占几行（宽版加一行虚线）。
fn height_of(v: &View, item: Item, room: usize) -> usize {
    render_item(v, item, room).len() + usize::from(!v.narrow)
}

/// 停在最底时，第一项该是第几项：让最后一项整个露出来。
pub fn bottom_offset(v: &View, items: &[Item], room: usize, rows: usize) -> usize {
    let mut used = 0;
    for (k, &it) in items.iter().enumerate().rev() {
        used += height_of(v, it, room);
        if used > rows {
            return (k + 1).min(items.len().saturating_sub(1));
        }
    }
    0
}

/// 画一帧；返回列表区里每一行对应的项（点击按它认）和露出来的范围 (第一项, 最后一项)。
pub fn draw(f: &mut Frame, v: &View, offset: usize) -> (Vec<(u16, Item)>, Option<(usize, usize)>) {
    let area = f.area();
    let room = area.width.saturating_sub(2) as usize;
    let grey = Style::default().fg(GREY);
    let head = if !v.found {
        t("这个窗格里没找到 Claude 会话")
    } else if v.gone() > 0 {
        t("{gone} 轮已压缩 · 当前 {n} 轮")
            .replace("{gone}", &v.gone().to_string())
            .replace("{n}", &(v.stops.len() - v.gone()).to_string())
    } else {
        t("{n} 轮").replace("{n}", &v.stops.len().to_string())
    };
    let head = match v.flash {
        Some(msg) => format!("{head} · {msg}"),
        None => head,
    };
    let header = vec![
        Line::from(Span::styled(clip(&head, room), grey)),
        Line::from(Span::styled("─".repeat(area.width as usize), Style::default().fg(DIM))),
    ];
    f.render_widget(
        Paragraph::new(header),
        Rect {
            x: area.x + 2,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: 2.min(area.height),
        },
    );

    let top = area.y + 2;
    let rows = area.height.saturating_sub(2) as usize;
    let items = v.items();
    let mut hits = Vec::new();
    let mut lines: Vec<Line> = Vec::new();
    let (mut first, mut last) = (None, None);
    for &it in items.iter().skip(offset) {
        let body = render_item(v, it, room);
        let selected = matches!(it, Item::Stop(i) if v.opened == Some(v.stops[i].id.as_str()));
        let start = lines.len();
        if start >= rows {
            break;
        }
        for l in body {
            let mut spans = vec![Span::styled(if selected { "▌ " } else { "  " }, Style::default().fg(ACCENT))];
            spans.extend(l.spans);
            lines.push(Line::from(spans));
        }
        if !v.narrow {
            lines.push(Line::from(Span::styled(
                format!("  {}", "┈".repeat(room)),
                Style::default().fg(DIM),
            )));
        }
        for r in start..lines.len().min(rows) {
            hits.push((top + r as u16, it));
        }
        first.get_or_insert(it);
        last = Some(it);
    }
    lines.truncate(rows);
    f.render_widget(
        Paragraph::new(lines),
        Rect {
            x: area.x,
            y: top,
            width: area.width,
            height: rows as u16,
        },
    );
    let pos = |it: Item| items.iter().position(|&x| x == it).unwrap_or(0);
    (hits, first.zip(last).map(|(a, b)| (pos(a), pos(b))))
}
