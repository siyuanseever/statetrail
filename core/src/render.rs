//! 表头里画出来的东西：额度和上下文柱子、模型账号行、刷新按钮后面的状态。照搬 Python 版 core/render.py 的
//! status_lines 一族，阈值、颜色规则、取整方式（四舍六入五成双）都一样。一段文字带一个颜色（None = 终端默认色）。

use ratatui::style::Color;
use unicode_width::UnicodeWidthStr;

use crate::claude::{Usage, Window, secs_left};
use crate::i18n::t;
use crate::theme::colors;

pub type Seg = (String, Option<Color>);
pub type SLine = Vec<Seg>;

const BAR_FIXED: usize = 10; // 一行的固定开销：`ctx ` 4 列 + ` 100%` 5 列 + 尾巴前的那个空格 1 列

fn seg(s: impl Into<String>, c: Color) -> Seg {
    (s.into(), Some(c))
}

pub fn width_of(line: &[Seg]) -> usize {
    line.iter().map(|(s, _)| s.width()).sum()
}

pub fn plain(line: &[Seg]) -> String {
    line.iter().map(|(s, _)| s.as_str()).collect()
}

/// 按显示宽度截（中文算两格），不加省略号（Python 版 render.clip）。截了就丢掉颜色，和 Python 版一样。
pub fn clip_line(line: SLine, width: usize) -> SLine {
    if width_of(&line) <= width {
        return line;
    }
    let mut out = String::new();
    let mut w = 0;
    for c in plain(&line).chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    vec![(out, None)]
}

/// 把 tag 左对齐放在 line 前面，占 slot 列：翻到头没有 tag 时补空格，后面的字和按钮不会跟着左右挪。
/// slot 为 0（一屏放得下，不用翻）就原样返回。和 Python 版 lead() 一样。
pub fn lead(line: SLine, tag: &str, slot: usize) -> SLine {
    if slot == 0 {
        return line;
    }
    let mut out = vec![
        (tag.to_owned(), Some(colors().grey)),
        (format!("{} ", " ".repeat(slot.saturating_sub(tag.width()))), None),
    ];
    out.extend(line);
    out
}

/// 还有多久重置，窄到塞得进一行：1d14h / 2h01m / 37m（固定 6 格）。
pub fn left_str(secs: f64) -> String {
    if secs <= 0.0 {
        let s = t("已过");
        return format!("{}{s}", " ".repeat(6usize.saturating_sub(s.width())));
    }
    let secs = secs as i64;
    let (d, r) = (secs / 86400, secs % 86400);
    let (h, m) = (r / 3600, r % 3600 / 60);
    if d > 0 {
        format!("{d}d{h:02}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{:>6}", format!("{m}m"))
    }
}

fn bar_w(width: usize, tails: &[String]) -> usize {
    let longest = tails.iter().map(|t| t.width()).max().unwrap_or(6);
    let room = width as i64 - BAR_FIXED as i64 - longest as i64;
    if room < 4 { 0 } else { room as usize }
}

/// (用掉的比例, 匀速线的位置, 还剩多少秒)
fn window(w: &Window, label: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
    let left = secs_left(w);
    let span = if label == "7d" { 7.0 * 86400.0 } else { 5.0 * 3600.0 };
    let mark = left.map(|l| 1.0 - (l / span).clamp(0.0, 1.0));
    (w.utilization.map(|p| p / 100.0), mark, left)
}

fn pct3(frac: f64) -> String {
    format!("{:3.0}%", frac * 100.0)
}

/// `5h  ████▌░░░░│░░░░  24% 2h01m`：实心 = 已经用掉的；蓝色竖线 = 参照线（额度是窗口已经走过的时间，上下文是自动压缩线）。
fn bar_line(label: &str, frac: Option<f64>, mark: Option<f64>, tail: &str, bw: usize, used: Color) -> SLine {
    let c = colors();
    let lab = format!("{label:<3} ");
    let Some(frac) = frac else {
        return vec![seg(format!("{lab}{}    ? {tail}", "·".repeat(bw)), c.dim)];
    };
    let fill = ((frac.clamp(0.0, 1.0) * bw as f64).round_ties_even() as usize).min(bw);
    let mut out = vec![seg(lab, c.grey)];
    if bw > 0 {
        match mark.filter(|m| (0.0..=1.0).contains(m)) {
            None => {
                out.push(seg("█".repeat(fill), used));
                out.push(seg("░".repeat(bw - fill), c.dim));
            }
            Some(m) => {
                let i = ((m * bw as f64) as usize).min(bw.saturating_sub(1));
                out.push(seg("█".repeat(fill.min(i)), used));
                if i < fill {
                    out.push(seg("│", c.time));
                }
                out.push(seg("█".repeat(fill.saturating_sub(i + 1)), used));
                out.push(seg("░".repeat(i.saturating_sub(fill)), c.dim));
                if i >= fill {
                    out.push(seg("│", c.time));
                }
                out.push(seg("░".repeat(bw.saturating_sub((i + 1).max(fill))), c.dim));
            }
        }
        out.push((format!(" {}", pct3(frac)), None));
    } else {
        out.push((pct3(frac), None));
    }
    out.push(seg(format!(" {tail}"), c.grey));
    out
}

/// 额度柱子的颜色：按「比匀速线超前多少」，不是按用了多少。超前 20 个百分点起橙、35 个起红；用到 80% / 93% 也分别橙、红。
pub fn quota_colour(frac: Option<f64>, mark: Option<f64>) -> Color {
    let c = colors();
    let Some(frac) = frac else { return c.used };
    let over = mark.map_or(0.0, |m| frac - m);
    if frac >= 0.93 || over >= 0.35 {
        c.alert
    } else if frac >= 0.80 || over >= 0.20 {
        c.warn
    } else {
        c.used
    }
}

/// "22% (229k)" → (22, 229)
fn parse_ctx(ctx: &str) -> Option<(u64, u64)> {
    let (pct, rest) = ctx.split_once('%')?;
    let pct: u64 = pct.trim().rsplit(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    let k = rest.trim_start().strip_prefix('(')?.split_once("k)")?.0.parse().ok()?;
    Some((pct, k))
}

fn ctx_colour(ctx: &str, compact_at: u64) -> Color {
    let c = colors();
    let Some((_, k)) = parse_ctx(ctx) else { return c.used };
    let tokens = k * 1000;
    if tokens >= compact_at {
        c.alert
    } else if tokens as f64 >= compact_at as f64 * 0.8 {
        c.warn
    } else {
        c.used
    }
}

fn quota_tail(w: &Window) -> String {
    window(w, "5h").2.map_or_else(|| "     ?".to_owned(), left_str)
}

fn quota_line(label: &str, w: &Window, bw: usize) -> SLine {
    let (frac, mark, left) = window(w, label);
    let tail = left.map_or_else(|| "     ?".to_owned(), left_str);
    bar_line(label, frac, mark, &tail, bw, quota_colour(frac, mark))
}

/// 只在有话说的时候出声：正在查 ⟳，刚查失败 ✗。
fn fresh_mark(u: &Usage) -> Option<Seg> {
    let c = colors();
    if u.attempted_at_ms > u.fetched_at_ms && u.attempted_at_ms > u.error_at_ms {
        Some(seg(" ⟳", c.grey))
    } else if u.error_at_ms > u.fetched_at_ms {
        Some(seg(" ✗", c.alert))
    } else {
        None
    }
}

/// 刷新按钮后面那一小段：在查、查完了多久、查失败了。
pub fn usage_state(u: Option<&Usage>) -> Seg {
    let c = colors();
    let Some(u) = u else { return seg(t("无读数"), c.dim) };
    if u.attempted_at_ms > u.fetched_at_ms && u.attempted_at_ms > u.error_at_ms {
        return seg(t("查询中…"), c.warn);
    }
    if u.error_at_ms > u.fetched_at_ms {
        return seg(t("查询失败"), c.alert);
    }
    let age = (crate::claude::now_ms() - u.fetched_at_ms) / 1000;
    if age < 90 {
        seg(t("刚刚更新"), c.used)
    } else if age < 3600 {
        seg(t("{n} 分钟前").replace("{n}", &(age / 60).to_string()), c.dim)
    } else if age < 2 * 86400 {
        seg(t("{n} 小时前").replace("{n}", &(age / 3600).to_string()), c.dim)
    } else {
        seg(t("{n} 天前").replace("{n}", &(age / 86400).min(99).to_string()), c.warn) // 旧到这个程度要看得出来
    }
}

/// 把一段带颜色的状态塞进固定宽度：前面一个空格，短了补空格，长了截掉。
pub fn fit(s: Seg, width: usize) -> SLine {
    if width == 0 {
        return vec![];
    }
    let body = format!(" {}", s.0);
    let w = body.width();
    if w <= width {
        return vec![(body, s.1), (" ".repeat(width - w), None)];
    }
    let clipped = plain(&clip_line(vec![(body, None)], width));
    let pad = width - clipped.width();
    vec![(clipped, None), (" ".repeat(pad), None)]
}

/// 宽版表头里的模型 / 账号一行，加上 ctx / 5h / 7d 三条柱子（有几行看有没有数据）。
pub fn status_lines(width: usize, model: &str, ctx: &str, u: Option<&Usage>, account: &str, compact_at: Option<u64>) -> Vec<SLine> {
    let c = colors();
    let width = 24.max(width.saturating_sub(2));
    let parsed = parse_ctx(ctx);
    let ctx_tail = parsed.map(|(_, k)| format!("{k}k"));
    let mut tails: Vec<String> = ctx_tail.iter().cloned().collect();
    if let Some(u) = u {
        tails.push(quota_tail(&u.five_hour));
        tails.push(quota_tail(&u.seven_day));
    } else {
        tails.push(quota_tail(&Window::default()));
        tails.push(quota_tail(&Window::default()));
    }
    let bw = bar_w(width, &tails);
    let mut lines = Vec::new();
    if !model.is_empty() || !account.is_empty() {
        let mut head = model.to_owned();
        // 窄的时候宁可不显示账号，也别把模型截了（Python 版按字数比，不按显示宽度）
        if !account.is_empty() && format!("{model} · {account}").chars().count() <= width {
            head = if model.is_empty() {
                account.to_owned()
            } else {
                format!("{model} · {account}")
            };
        }
        lines.push(vec![seg(head, c.grey)]);
    }
    match (parsed, ctx_tail) {
        (Some((pct, _)), Some(tail)) => {
            let window_size = if model.to_lowercase().contains("haiku") {
                200_000.0
            } else {
                1_000_000.0
            };
            let colour = ctx_colour(ctx, compact_at.unwrap_or(crate::claude::CTX_HARD));
            let mark = compact_at.map(|a| a as f64 / window_size);
            lines.push(bar_line("ctx", Some(pct as f64 / 100.0), mark, &tail, bw, colour));
        }
        _ if !ctx.is_empty() && model.is_empty() => lines.push(vec![seg(ctx, c.grey)]),
        _ => {}
    }
    if let Some(u) = u {
        let mut five = quota_line("5h", &u.five_hour, bw);
        five.extend(fresh_mark(u));
        lines.push(five);
        lines.push(quota_line("7d", &u.seven_day, bw));
    }
    lines
}

/// 窄版用的 (百分比, 颜色)，颜色和柱子同一套规则。
pub fn quota_pct(w: &Window, label: &str) -> Option<(f64, Color)> {
    let (frac, mark, _) = window(w, label);
    frac.map(|f| (f * 100.0, quota_colour(Some(f), mark)))
}

pub fn ctx_pct(ctx: &str, compact_at: u64) -> Option<(u64, Color)> {
    parse_ctx(ctx).map(|(p, _)| (p, ctx_colour(ctx, compact_at)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn ctx_parse() {
        assert_eq!(super::parse_ctx("22% (229k)"), Some((22, 229)));
        assert_eq!(super::parse_ctx(""), None);
    }

    #[test]
    fn left_str_like_python() {
        assert_eq!(super::left_str(37.0 * 60.0), "   37m");
        assert_eq!(super::left_str(2.0 * 3600.0 + 60.0), "2h01m");
        assert_eq!(super::left_str(86400.0 + 14.0 * 3600.0), "1d14h");
    }
}
