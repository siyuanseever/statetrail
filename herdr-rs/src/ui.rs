//! 画轨迹：表头、一条整行的分隔线、列表。列表自己记滚动位置（不再靠 fzf，也不再读自己的屏幕），画的时候顺手记下
//! 每一行是哪一项，点击就按它认。列表的格式和 Python 版 core/render.py 的 rows_of / narrow_rows_of 一样：
//! 宽版每轮两行（轮号 时间 你说的话 / 回复的开头），轮与轮之间一行虚线；窄版每轮一行（轮号 你说的话）。
//! 不加边框、不加盒子：只有分隔线，和 Python 版（fzf 的表头分隔线）一样克制。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
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
    pub footer_top: u16,        // 底部第一行在第几行
}

/// 列表区从 offset 开始画，露出来的是哪几项（第一行露出来就算）。
pub fn visible(v: &View, items: &[Item], offset: usize, rows: usize, room: usize) -> Vec<Item> {
    let mut used = 0;
    let mut out = Vec::new();
    for &it in items.iter().skip(offset) {
        if used >= rows {
            break;
        }
        out.push(it);
        used += height_of(v, it, room);
    }
    out
}

/// 列表上面还有几轮、下面还有几轮、露出来几项：和 Python 版 read_view 的算法一样（它是读屏，这里是算）。
/// 「已压缩」那一行露出来时，上面算 0 轮；那一行代表的是第 1 到第 N 轮。
pub fn view_counts(v: &View, shown: &[Item]) -> (usize, usize, usize) {
    let nums: Vec<usize> = shown
        .iter()
        .filter_map(|it| if let Item::Stop(i) = it { Some(i + 1) } else { None })
        .collect();
    let fold = shown.first() == Some(&Item::Fold);
    match (nums.first(), nums.last()) {
        (Some(&a), Some(&b)) => (if fold { 0 } else { a - 1 }, v.stops.len() - b, nums.len() + usize::from(fold)),
        _ if fold => (0, v.stops.len() - v.gone(), 1),
        _ => (0, 0, 0),
    }
}

/// 画一帧：表头（左边留 2 列，和列表的指针那一列对齐）、整行的分隔线、列表、整行的分隔线、底部。
pub fn draw(f: &mut Frame, header: &[SLine], footer: &[SLine], v: &View, offset: usize) -> Drawn {
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
    let foot_h = footer.len() as u16;
    let footer_top = (area.y + area.height).saturating_sub(foot_h);
    let rows = footer_top.saturating_sub(1).saturating_sub(top) as usize; // 底部上面还有一条分隔线
    if footer_top > top {
        let sep = Line::from(Span::styled("─".repeat(area.width as usize), Style::default().fg(c.dim)));
        f.render_widget(
            Paragraph::new(sep),
            Rect {
                x: area.x,
                y: footer_top - 1,
                width: area.width,
                height: 1,
            },
        );
        let foot: Vec<Line> = footer.iter().map(to_line).collect();
        f.render_widget(
            Paragraph::new(foot),
            Rect {
                x: area.x + 2,
                y: footer_top,
                width: area.width.saturating_sub(2),
                height: foot_h,
            },
        );
    }
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
    Drawn {
        hits,
        list_top: top,
        footer_top,
    }
}

/// 表头里点模型 / effort / 账号弹出来的下拉菜单：画在点的那段字正下方（x, y 是框左上角），一项一行，当前的打 ✔。
/// herdr 的 popup 只能开在屏幕正中，所以自己画在轨迹里。
pub struct Menu {
    pub x: u16,
    pub y: u16,
    pub items: Vec<(String, String, bool)>, // (交回去的 key, 显示的字, 是不是当前的)；key 为空是分隔线
}

/// 画下拉菜单，返回每一项的 (行, 列起, 列止, key)。
/// 表头里能点的一段字（第 y 行、x0..x1 列）：平时下划线，鼠标停上去反色
pub fn mark(f: &mut Frame, y: u16, x0: u16, x1: u16, lit: bool) {
    let area = f.area();
    let style = if lit {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().add_modifier(Modifier::UNDERLINED)
    };
    if y >= area.height {
        return;
    }
    // 尾巴上的空格不画线（账号那一段一直算到行尾）
    let mut end = x1.min(area.width);
    while end > x0 && f.buffer_mut()[(end - 1, y)].symbol().trim().is_empty() {
        end -= 1;
    }
    for x in x0..end {
        f.buffer_mut()[(x, y)].set_style(style);
    }
}

pub fn draw_menu(f: &mut Frame, m: &Menu, hover: Option<(u16, u16)>) -> Vec<(u16, u16, u16, String)> {
    let c = colors();
    let area = f.area();
    let inner = m.items.iter().map(|(_, l, _)| l.width() + 4).max().unwrap_or(4) as u16;
    let w = (inner + 2).min(area.width);
    let h = (m.items.len() as u16 + 2).min(area.height);
    let x = m.x.min(area.width.saturating_sub(w));
    let y = m.y.min(area.height.saturating_sub(h));
    let rect = Rect { x, y, width: w, height: h };
    f.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(c.accent));
    let lines: Vec<Line> = m
        .items
        .iter()
        .map(|(key, label, cur)| {
            if key.is_empty() {
                Line::from(Span::styled("─".repeat(inner as usize), Style::default().fg(c.dim)))
            } else if *cur {
                Line::from(Span::styled(
                    format!(" {label} ✔"),
                    Style::default().fg(c.accent).add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(format!(" {label}"))
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), rect);
    // 鼠标停着的那一项整行反色
    if let Some((hy, hx)) = hover {
        let row = hy.wrapping_sub(y + 1) as usize;
        if hy > y && hy < y + h - 1 && hx > x && hx < x + w - 1 && m.items.get(row).is_some_and(|it| !it.0.is_empty()) {
            for cx in x + 1..x + w - 1 {
                f.buffer_mut()[(cx, hy)].set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
    m.items
        .iter()
        .enumerate()
        .filter(|(i, (k, _, _))| !k.is_empty() && y + 1 + (*i as u16) < y + h - 1)
        .map(|(i, (k, _, _))| (y + 1 + i as u16, x + 1, x + w - 1, k.clone()))
        .collect()
}
