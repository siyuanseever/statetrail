//! 画轨迹：表头、一条整行的分隔线、列表。列表自己记滚动位置（不再靠 fzf，也不再读自己的屏幕），画的时候顺手记下
//! 每一行是哪一项，点击就按它认。列表的格式和 Python 版 core/render.py 的 rows_of / narrow_rows_of 一样：
//! 宽版每轮两行（轮号 时间 你说的话 / 回复的开头），轮与轮之间一行虚线；窄版每轮一行（轮号 你说的话）。
//! 不加边框、不加盒子：只有分隔线，和 Python 版（fzf 的表头分隔线）一样克制。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::i18n::t;
use crate::render::SLine;
use crate::theme::colors;
use crate::transcript::Stop;

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

    pub fn gone(&self) -> usize {
        self.stops.iter().filter(|s| s.compacted).count()
    }
}

/// 按显示宽度截（中文算两格）；ellipsis 时截了补个「…」（宽版的行，和 fzf 的 --ellipsis 一样）。
pub fn clip(s: &str, width: usize, ellipsis: bool) -> String {
    if s.width() <= width {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut w = 0;
    let room = if ellipsis { width.saturating_sub(1) } else { width };
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > room {
            break;
        }
        out.push(c);
        w += cw;
    }
    if ellipsis {
        out.push('…');
    }
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
    let c = colors();
    let faint = Style::default().add_modifier(Modifier::DIM);
    let dimc = Style::default().fg(c.dim);
    match item {
        Item::Fold => {
            let n = v.gone();
            if v.narrow {
                let text = format!("1–{n} {}", if v.expanded { "−" } else { "+" });
                return vec![Line::from(Span::styled(clip(&text, room, false), dimc))];
            }
            let act = if v.expanded { t("点这里收起") } else { t("点开展开") };
            let tpl = if n > 1 {
                t("第 1–{n} 轮已压缩 · {act}")
            } else {
                t("第 1 轮已压缩 · {act}")
            };
            vec![Line::from(Span::styled(
                clip(&tpl.replace("{n}", &n.to_string()).replace("{act}", &act), room, true),
                faint,
            ))]
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
                    Style::default().fg(c.busy)
                } else if missing {
                    dimc
                } else {
                    Style::default()
                };
                let head = format!("{n} ");
                return vec![Line::from(vec![
                    Span::styled(head.clone(), dimc),
                    Span::styled(clip(&ask, room.saturating_sub(head.width()), false), style),
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
                    Span::styled(meta.clone(), faint),
                    Span::styled(format!(" {}", t("你说的：")), Style::default().add_modifier(Modifier::BOLD)),
                ]));
                for l in wrap(st.prompt.trim(), room.saturating_sub(4).max(10)) {
                    lines.push(Line::from(format!("   {l}")));
                }
            } else {
                let rest = room.saturating_sub(meta.width() + 1);
                let ask_style = if missing { faint } else { Style::default() };
                lines.push(Line::from(vec![
                    Span::styled(meta, faint),
                    Span::raw(" "),
                    Span::styled(clip(&ask, rest, true), ask_style),
                ]));
            }
            lines.push(Line::from(Span::styled(clip(&format!("   {reply}"), room, true), faint)));
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

pub fn to_line(l: &SLine) -> Line<'static> {
    Line::from(
        l.iter()
            .map(|(s, c)| Span::styled(s.clone(), c.map_or_else(Style::default, |c| Style::default().fg(c))))
            .collect::<Vec<_>>(),
    )
}

pub struct Drawn {
    pub hits: Vec<(u16, Item)>, // 列表区里每一行对应的项
    pub list_top: u16,          // 列表从第几行开始
}

/// 画一帧：表头（左边留 2 列，和列表的指针那一列对齐）、整行的分隔线、列表。
pub fn draw(f: &mut Frame, header: &[SLine], v: &View, offset: usize) -> Drawn {
    let c = colors();
    let area = f.area();
    let room = area.width.saturating_sub(2) as usize;
    let head_h = (header.len() as u16).min(area.height);
    let head: Vec<Line> = header.iter().map(to_line).collect();
    f.render_widget(
        Paragraph::new(head),
        Rect {
            x: area.x + 2,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: head_h,
        },
    );
    let sep_y = area.y + head_h;
    if sep_y < area.y + area.height {
        let sep = Line::from(Span::styled("─".repeat(area.width as usize), Style::default().fg(c.dim)));
        f.render_widget(
            Paragraph::new(sep),
            Rect {
                x: area.x,
                y: sep_y,
                width: area.width,
                height: 1,
            },
        );
    }
    let top = sep_y + 1;
    let rows = (area.y + area.height).saturating_sub(top) as usize;
    let items = v.items();
    let mut hits = Vec::new();
    let mut lines: Vec<Line> = Vec::new();
    for &it in items.iter().skip(offset) {
        let start = lines.len();
        if start >= rows {
            break;
        }
        let selected = matches!(it, Item::Stop(i) if v.opened == Some(v.stops[i].id.as_str()));
        let bg = if selected {
            c.selection.map_or_else(Style::default, |b| Style::default().bg(b))
        } else {
            Style::default()
        };
        for l in render_item(v, it, room) {
            let mut spans = vec![Span::styled(if selected { "▌ " } else { "  " }, Style::default().fg(c.accent))];
            spans.extend(l.spans);
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            if selected && used < area.width as usize {
                spans.push(Span::raw(" ".repeat(area.width as usize - used))); // 选中行整行铺上底色
            }
            lines.push(Line::from(spans).patch_style(bg));
        }
        if !v.narrow {
            lines.push(Line::from(Span::styled(
                format!("  {}", "┈".repeat(room)),
                Style::default().fg(c.dim),
            )));
        }
        for r in start..lines.len().min(rows) {
            hits.push((top + r as u16, it));
        }
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
    Drawn { hits, list_top: top }
}
