//! 画轨迹：表头、一条整行的分隔线、列表。列表自己记滚动位置（不再靠 fzf，也不再读自己的屏幕），画的时候顺手记下
//! 每一行是哪一项，点击就按它认。列表的格式和 Python 版 core/render.py 的 rows_of / narrow_rows_of 一样：
//! 宽版每轮两行（轮号 时间 你说的话 / 回复的开头），轮与轮之间一行虚线；窄版每轮一行（轮号 你说的话）。
//! 不加边框、不加盒子：只有分隔线，和 Python 版（fzf 的表头分隔线）一样克制。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::chapters::Chapter;
use crate::i18n::t;
use crate::render::SLine;
use crate::theme::colors;
use crate::transcript::Stop;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Item {
    Fold,
    Stop(usize),    // visible() 里的下标
    Chapter(usize), // 章名那一行：View.chapters 里的下标
}

pub struct View<'a> {
    pub stops: &'a [&'a Stop],
    pub narrow: bool,
    pub expanded: bool,
    pub opened: Option<&'a str>,
    pub busy: bool,
    pub missing: &'a std::collections::HashSet<String>,
    pub chapters: &'a [Chapter], // 空 = 不分章，平铺
    pub open: &'a [bool],        // 每一章展没展开
    pub query: &'a str,          // 在搜的字（小写）；空 = 没在搜
    pub outline: bool,           // 在章那一层：选中的是章名（那一章照样展开给你看），不是轮次
}

impl View<'_> {
    pub fn items(&self) -> Vec<Item> {
        let mut out = Vec::new();
        // 在搜：只列命中的轮次；分章时命中的轮次照旧排在各自的章名下面，没命中的章不列
        if !self.query.is_empty() {
            if self.chapters.is_empty() {
                out.extend((0..self.stops.len()).filter(|&i| self.hit(i)).map(Item::Stop));
            }
            for (ci, ch) in self.chapters.iter().enumerate() {
                let hits: Vec<Item> = (ch.start..ch.end).filter(|&i| self.hit(i)).map(Item::Stop).collect();
                if !hits.is_empty() {
                    out.push(Item::Chapter(ci));
                    out.extend(hits);
                }
            }
            return out;
        }
        if self.chapters.is_empty() {
            if self.stops.iter().any(|s| s.compacted) {
                out.push(Item::Fold);
            }
            out.extend((0..self.stops.len()).filter(|&i| !self.stops[i].compacted || self.expanded).map(Item::Stop));
            return out;
        }
        // 分章时不再把压缩掉的轮次收成一行：章本来就收着，收成一行反而一个章名都看不到。压缩掉的轮次画暗
        for (ci, ch) in self.chapters.iter().enumerate() {
            out.push(Item::Chapter(ci));
            if self.open.get(ci) == Some(&true) {
                out.extend((ch.start..ch.end).map(Item::Stop));
            }
        }
        out
    }

    /// 选中的那一轮所在的章
    pub fn opened_chapter(&self) -> Option<usize> {
        let i = self.stops.iter().position(|s| Some(s.id.as_str()) == self.opened)?;
        self.chapters.iter().position(|c| (c.start..c.end).contains(&i))
    }

    /// 这一项画成选中：选中的那一轮；在章那一层、或者它所在的章收着时，是那一章的章名
    pub fn selected(&self, it: Item) -> bool {
        match it {
            Item::Stop(i) => !self.outline && self.opened == Some(self.stops[i].id.as_str()),
            Item::Chapter(ci) => {
                (self.outline || self.open.get(ci) != Some(&true)) && self.opened_chapter() == Some(ci)
            }
            Item::Fold => false,
        }
    }

    /// 第 i 轮命中在搜的字：你说的话或者 Claude 回复的那一行里有（不分大小写）
    pub fn hit(&self, i: usize) -> bool {
        matches(self.stops[i], self.query)
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
        Item::Chapter(ci) => {
            let ch = &v.chapters[ci];
            let open = v.open.get(ci) == Some(&true);
            // 章名只是辅助：整行灰色，不加粗；你主要看的是轮次，轮次用正常的深色字（用户 2026-10-29：「暗色应该用在 Conversation，
            // 浅色应该用在章节，因为章节只是起辅助作用」）
            // 章名用青色（主题的 teal），和 Claude 回复那一行的灰分开；左边的竖线也是这个颜色（用户 2026-10-29：「章节的话跟下面
            // Conversation 里 Claude 的回答能不能不要用同一种灰色」）
            let aux = Style::default().fg(c.cyan);
            let arrow = Span::styled(if open { "▾ " } else { "▸ " }, aux);
            let n = ch.end - ch.start;
            // 章与章的分隔要比轮与轮的明显（用户 2026-10-29：「章节跟章节之间的分隔应该要更明显」）：一章的轮次左边有一条竖线，
            // 章名行和宽版章前的空行不画，竖线在换章处断开（见 draw）；轮与轮之间的虚线缩进到回复下面
            // 收着时在最右边写这一章收了几轮；第几轮到第几轮不写（用户 2026-10-29：「更关键的是它折叠了多少轮」）。展开时轮次就在下面，不写。
            // 数字是要看的，用正常前景色，不跟章名一样淡
            let tail = if open {
                String::new()
            } else if v.narrow {
                n.to_string()
            } else if n == 1 {
                t("1 轮")
            } else {
                t("{n} 轮").replace("{n}", &n.to_string())
            };
            let title = clip(&ch.title, room.saturating_sub(2 + tail.width() + 1), !v.narrow);
            let pad = room.saturating_sub(2 + title.width() + tail.width());
            let head = Line::from(vec![arrow, Span::styled(title, aux), Span::raw(" ".repeat(pad)), Span::raw(tail)]);
            if v.narrow {
                return vec![head];
            }
            // 空一行只在挨着展开的那章时：收着的章一章一行连着排，不然全收着时一半是空行
            let gap = ci > 0 && (open || v.open.get(ci - 1) == Some(&true));
            if gap { vec![Line::default(), head] } else { vec![head] }
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
            let dim = missing || st.compacted && !v.chapters.is_empty(); // 主界面里已经跳不过去
            if v.narrow {
                let style = if last_busy {
                    Style::default().fg(c.busy)
                } else if dim {
                    dimc
                } else {
                    Style::default()
                };
                // 分章时轮次也不缩进：细条本来就窄（用户 2026-10-29：「你不要右移，你就直接放到最左侧」）
                let head = format!("{n} ");
                let mut spans = vec![Span::styled(head.clone(), dimc)];
                let w = room.saturating_sub(head.width());
                spans.extend(mark_hit(clip(&near(&ask, v.query, w), w, false), v.query, style));
                return vec![Line::from(spans)];
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
            let opened = !v.outline && v.opened == Some(st.id.as_str()) && st.kind == "human"; // 章那一层选中的是章名：轮次都不展开原话
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
                let ask_style = if dim { faint } else { Style::default() };
                let mut spans = vec![Span::styled(meta, faint), Span::raw(" ")];
                spans.extend(mark_hit(clip(&near(&ask, v.query, rest), rest, true), v.query, ask_style));
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(Span::styled(clip(&format!("   {reply}"), room, true), faint)));
            lines
        }
    }
}

/// 这一轮命中在搜的字 q（已经是小写）
pub fn matches(st: &Stop, q: &str) -> bool {
    st.prompt.to_lowercase().contains(q) || st.reply.to_lowercase().contains(q)
}

/// 在搜时，命中的字在这一行（room 列宽）里露不出来：从命中处前两个字开始显示，前面写「…」；露得出来就照旧从头
fn near(text: &str, query: &str, room: usize) -> String {
    if query.is_empty() {
        return text.to_owned();
    }
    let lower = text.to_lowercase();
    let Some(at) = lower.find(query) else { return text.to_owned() };
    let p = lower[..at].chars().count();
    let end: usize = text.chars().take(p + query.chars().count()).map(|c| c.width().unwrap_or(0)).sum();
    if p <= 2 || end < room {
        return text.to_owned();
    }
    format!("…{}", text.chars().skip(p - 2).collect::<String>())
}

/// 把一行里命中在搜的字涂成强调色（只涂第一处）
fn mark_hit(text: String, query: &str, style: Style) -> Vec<Span<'static>> {
    let lower = text.to_lowercase();
    // 小写后长度变了（少见的字母）就不涂，免得切错
    let at = if query.is_empty() || lower.len() != text.len() { None } else { lower.find(query) };
    match at.filter(|&a| text.is_char_boundary(a) && text.is_char_boundary(a + query.len())) {
        Some(a) => vec![
            Span::styled(text[..a].to_owned(), style),
            Span::styled(text[a..a + query.len()].to_owned(), Style::default().fg(colors().accent).add_modifier(Modifier::BOLD)),
            Span::styled(text[a + query.len()..].to_owned(), style),
        ],
        None => vec![Span::styled(text, style)],
    }
}

/// 宽版每一轮后面画一行虚线；章名后面不画，紧跟着它的轮次
/// 一章的最后一轮后面也不画：紧接着就是下一章的实线，虚线只用来分同一章里的轮次
fn dashed(v: &View, item: Item) -> bool {
    !v.narrow
        && match item {
            Item::Chapter(_) => false,
            Item::Stop(i) => !v.chapters.iter().any(|c| c.start == i + 1),
            Item::Fold => true,
        }
}

/// 每项占几行（宽版加一行虚线）。
fn height_of(v: &View, item: Item, room: usize) -> usize {
    render_item(v, item, room).len() + usize::from(dashed(v, item))
}

/// 从 offset 画起，第 k 项能不能整个露出来
pub fn fits(v: &View, items: &[Item], offset: usize, k: usize, rows: usize, room: usize) -> bool {
    k >= offset && items[offset..=k].iter().map(|&it| height_of(v, it, room)).sum::<usize>() <= rows
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
/// 每项代表一段轮次：「已压缩」那一行是第 1 到第 N 轮，收着的章是整章，展开的章名只占个位置（它的轮次各自算）。
pub fn view_counts(v: &View, shown: &[Item]) -> (usize, usize, usize) {
    let spans: Vec<(usize, usize)> = shown
        .iter()
        .map(|&it| match it {
            Item::Fold => (0, v.gone()),
            Item::Stop(i) => (i, i + 1),
            Item::Chapter(ci) => {
                let ch = &v.chapters[ci];
                if v.open.get(ci) == Some(&true) { (ch.start, ch.start) } else { (ch.start, ch.end) }
            }
        })
        .collect();
    match (spans.iter().map(|s| s.0).min(), spans.iter().map(|s| s.1).max()) {
        (Some(a), Some(b)) => (a, v.stops.len().saturating_sub(b), shown.len()),
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
        let selected = v.selected(it);
        let bg = if selected {
            c.selection.map_or_else(Style::default, |b| Style::default().bg(b))
        } else {
            Style::default()
        };
        // 最左一列：选中的画 ▌；分章时一章的轮次画一条竖线 │（章名行不画：竖线在换章处断开），不另占一列
        let bar = !v.chapters.is_empty() && matches!(it, Item::Stop(_));
        let lead = if selected {
            Span::styled("▌ ", Style::default().fg(c.accent))
        } else if bar {
            Span::styled("│ ", Style::default().fg(c.cyan))
        } else {
            Span::raw("  ")
        };
        for l in render_item(v, it, room) {
            // 宽版章名前那行空白只是分隔：选中这一章时它不画 ▌、不铺底色
            if l.width() == 0 {
                lines.push(l);
                continue;
            }
            let mut spans = vec![lead.clone()];
            spans.extend(l.spans);
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            if selected && used < area.width as usize {
                spans.push(Span::raw(" ".repeat(area.width as usize - used))); // 选中行整行铺上底色
            }
            lines.push(Line::from(spans).patch_style(bg));
        }
        if dashed(v, it) {
            lines.push(Line::from(vec![
                if bar { Span::styled("│", Style::default().fg(c.cyan)) } else { Span::raw(" ") },
                Span::styled(
                    format!("    {}", "┈".repeat(room.saturating_sub(3))), // 缩进到回复下面：它只分同一章里的轮次，比章的分隔弱
                    Style::default().fg(c.dim),
                ),
            ]));
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

/// 跨会话搜索的一条结果：你说的那句话，和它在哪（项目 · 会话标题 · 时间 · 第几轮）
pub struct Found {
    pub text: String,
    pub meta: String,
    pub open: bool, // 那个会话正开在某个窗格里
}

/// 跨会话搜索的结果盖在列表那块（top..bottom 行）上：一条两行，选中的画 ▌ 加底色，滚到选中的露出来。
/// note 是没有结果时那一行灰字。返回每一行屏幕行是第几条（点击用）。
pub fn draw_found(f: &mut Frame, top: u16, bottom: u16, items: &[Found], sel: usize, off: &mut usize, query: &str, note: &str) -> Vec<(u16, usize)> {
    let c = colors();
    let area = f.area();
    let rect = Rect { x: area.x, y: top, width: area.width, height: bottom.saturating_sub(top) };
    f.render_widget(ratatui::widgets::Clear, rect);
    let room = area.width.saturating_sub(2) as usize;
    let faint = Style::default().add_modifier(Modifier::DIM);
    if items.is_empty() {
        f.render_widget(Paragraph::new(Line::from(Span::styled(format!("  {note}"), faint))), rect);
        return Vec::new();
    }
    let per = (rect.height as usize / 2).max(1);
    if sel < *off {
        *off = sel;
    } else if sel >= *off + per {
        *off = sel + 1 - per;
    }
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    for (k, it) in items.iter().enumerate().skip(*off).take(per) {
        let on = k == sel;
        let bg = if on { c.selection.map_or_else(Style::default, |b| Style::default().bg(b)) } else { Style::default() };
        let lead = if on { Span::styled("▌ ", Style::default().fg(c.accent)) } else { Span::raw("  ") };
        let mut a = vec![lead.clone()];
        a.extend(mark_hit(clip(&near(&oneline(&it.text), query, room), room, true), query, Style::default()));
        let mut b = vec![lead, Span::styled(clip(&format!("   {}", it.meta), room.saturating_sub(4), true), faint)];
        if it.open {
            b.push(Span::styled(format!(" · {}", t("开着")), Style::default().fg(c.cyan)));
        }
        for mut spans in [a, b] {
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            if on && used < area.width as usize {
                spans.push(Span::raw(" ".repeat(area.width as usize - used)));
            }
            rows.push((top + lines.len() as u16, k));
            lines.push(Line::from(spans).patch_style(bg));
        }
    }
    f.render_widget(Paragraph::new(lines), rect);
    rows
}

/// 表头里点模型 / effort / 账号弹出来的下拉菜单：画在点的那段字正下方（x, y 是框左上角），一项一行，当前的打 ✔。
/// herdr 的 popup 只能开在屏幕正中，所以自己画在轨迹里。
pub struct Menu {
    pub x: u16,
    pub y: u16,
    pub items: Vec<(String, String, bool)>, // (交回去的 key, 显示的字, 是不是当前的)；key 为空是分隔线
}

/// 画下拉菜单，返回每一项的 (行, 列起, 列止, key)。
/// 底色：主题的选中色；"terminal" 主题没有就用深灰
fn chip_bg() -> Color {
    colors().selection.unwrap_or(Color::DarkGray)
}

/// 表头里能点的一段字（第 y 行、x0..x1 列）：平时垫一层底色像个按钮，鼠标停上去换成强调色底
pub fn mark(f: &mut Frame, y: u16, x0: u16, x1: u16, lit: bool) {
    let area = f.area();
    let style = if lit {
        // 字色设成强调色再反过来：底是强调色，字是终端底色，深浅主题都看得清
        Style::default().fg(colors().accent).bg(Color::Reset).add_modifier(Modifier::REVERSED)
    } else {
        Style::default().bg(chip_bg())
    };
    if y >= area.height {
        return;
    }
    // 尾巴上的空格不垫底（账号那一段一直算到行尾）
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
    // 鼠标停着的那一项整行垫底色
    if let Some((hy, hx)) = hover {
        let row = hy.wrapping_sub(y + 1) as usize;
        if hy > y && hy < y + h - 1 && hx > x && hx < x + w - 1 && m.items.get(row).is_some_and(|it| !it.0.is_empty()) {
            for cx in x + 1..x + w - 1 {
                f.buffer_mut()[(cx, hy)].set_style(Style::default().bg(chip_bg()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn stop(id: &str, prompt: &str, reply: &str) -> Stop {
        Stop {
            kind: "human",
            prompt: prompt.into(),
            ts: None,
            id: id.into(),
            reply: reply.into(),
            has_text: true,
            compacted: false,
        }
    }

    fn screen(narrow: bool, width: u16, open: &[bool]) -> Vec<String> {
        let s = [
            stop("a", "做 A", "好的，A 做完了"),
            stop("b", "再改一下 A", "改好了"),
            stop("c", "新需求 C", "C 做完了"),
        ];
        let st: Vec<&Stop> = s.iter().collect();
        let ch = [
            Chapter { start: 0, end: 2, title: "甲任务".into() },
            Chapter { start: 2, end: 3, title: "丙任务".into() },
        ];
        let missing = std::collections::HashSet::new();
        let v = View {
            stops: &st,
            narrow,
            expanded: false,
            opened: None,
            busy: false,
            missing: &missing,
            chapters: &ch,
            open,
            query: "",
            outline: false,
        };
        let mut term = ratatui::Terminal::new(TestBackend::new(width, 16)).unwrap();
        term.draw(|f| {
            draw(f, &[], &[], &v, 0);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                let mut line = String::new();
                let mut skip = 0;
                for x in 0..buf.area.width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let sym = buf[(x, y)].symbol();
                    skip = sym.width().saturating_sub(1);
                    line += sym;
                }
                line.trim_end().to_owned()
            })
            .collect()
    }

    /// 宽版：章名是一条补满的实线；同一章里的轮次之间是虚线，章的最后一轮后面不画虚线
    #[test]
    fn chapter_is_a_rule() {
        let w = screen(false, 40, &[true, true]);
        for l in &w {
            println!("|{l}");
        }
        assert_eq!(w[1], "  ▾ 甲任务"); // 章名行左边没有竖线；展开时不写轮数
        assert!(w[2].starts_with("│  1") && w[3].starts_with("│ ")); // 轮次和回复左边有竖线
        assert!(w[4].starts_with("│    ┈")); // 轮与轮之间：缩进的虚线，竖线不断
        assert_eq!(w[7], ""); // 换章：空一行，竖线断开
        assert!(w[8].starts_with("  ▾ 丙任务") && w[9].starts_with("│  3"));
        let n = screen(true, 17, &[true, true]);
        for l in &n {
            println!("|{l}");
        }
        assert_eq!(n[1], "  ▾ 甲任务");
        assert!(n[2].starts_with("│ 1") && n[3].starts_with("│ 2") && n[4] == "  ▾ 丙任务" && n[5].starts_with("│ 3"));
    }

    /// 在搜：只列命中的轮次和它们所在的章；命中在一行后面时从命中处前面一点开始显示
    #[test]
    fn search_lists_hits_only() {
        let mk = |p: &str| Stop {
            kind: "human",
            prompt: p.into(),
            ts: None,
            id: p.into(),
            reply: "好".into(),
            has_text: true,
            compacted: false,
        };
        let owned = [mk("改颜色"), mk("这一句话很长很长很长很长，后面才说到 Search 功能"), mk("发布")];
        let st: Vec<&Stop> = owned.iter().collect();
        let ch = [Chapter { start: 0, end: 2, title: "甲".into() }, Chapter { start: 2, end: 3, title: "乙".into() }];
        let missing = std::collections::HashSet::new();
        let v = View {
            stops: &st,
            narrow: true,
            expanded: false,
            opened: None,
            busy: false,
            missing: &missing,
            chapters: &ch,
            open: &[true, true],
            query: "search",
            outline: false,
        };
        assert!(matches!(v.items()[..], [Item::Chapter(0), Item::Stop(1)]));
        assert!(near(&st[1].prompt, "search", 20).starts_with("…到 Search"));
        assert!(near(&st[1].prompt, "search", 80).starts_with("这一句"));
    }

    /// 章那一层：选中的那一章照样展开（轮次露出来），但亮的是章名，不是里面的轮次
    #[test]
    fn outline_selects_open_chapter() {
        let mk = |p: &str| Stop {
            kind: "human",
            prompt: p.into(),
            ts: None,
            id: p.into(),
            reply: "好".into(),
            has_text: true,
            compacted: false,
        };
        let owned = [mk("a"), mk("b"), mk("c")];
        let st: Vec<&Stop> = owned.iter().collect();
        let ch = [Chapter { start: 0, end: 2, title: "甲".into() }, Chapter { start: 2, end: 3, title: "乙".into() }];
        let missing = std::collections::HashSet::new();
        let mut v = View {
            stops: &st,
            narrow: true,
            expanded: false,
            opened: Some("a"),
            busy: false,
            missing: &missing,
            chapters: &ch,
            open: &[true, false],
            query: "",
            outline: true,
        };
        assert!(matches!(v.items()[..], [Item::Chapter(0), Item::Stop(0), Item::Stop(1), Item::Chapter(1)]));
        assert!(v.selected(Item::Chapter(0)) && !v.selected(Item::Stop(0)));
        v.outline = false;
        assert!(!v.selected(Item::Chapter(0)) && v.selected(Item::Stop(0)));
    }

    /// 收着的章：最右边写收了几轮，不写第几轮到第几轮
    #[test]
    fn shut_chapter_shows_count() {
        let w = screen(false, 40, &[false, true]);
        assert!(w[1].starts_with("  ▸ 甲任务") && w[1].ends_with(&format!(" {}", t("{n} 轮").replace("{n}", "2"))) && w[1].width() == 40, "{:?}", w[1]);
        assert!(!w[1].contains("1–2"));
        let n = screen(true, 17, &[false, true]);
        assert!(n[1].starts_with("  ▸ 甲任务") && n[1].ends_with(" 2") && n[1].width() == 17, "{:?}", n[1]);
    }
}
