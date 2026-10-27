//! 表头：第 1 行按钮（宽版 » ⇊ ⟳ + 额度查询状态，窄版 « ⇊ ⟳），下面宽版是模型账号 + ctx / 5h / 7d 柱子 + 「N 轮」，
//! 窄版是模型、账号、三个百分比各一行。和 Python 版 trail.py 的 wide_header / narrow_header 一样。

use std::path::Path;
use std::sync::OnceLock;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::claude::{self, Usage};
use crate::i18n::t;
use crate::render::{self, SLine, clip_line, width_of};
use crate::theme::colors;

pub const SLOT: usize = 3; // 图标按钮占 3 格（空格 + 图标 + 空格）
pub const GAP: usize = 1;

/// 图标三档：(nerd, unicode, text)。文字档的字跟着界面语言。
const ICONS: [(&str, &str, &str, &str); 16] = [
    ("narrow", "\u{f101}", "»", ">>"),
    ("wide", "\u{f100}", "«", "<<"),
    ("bottom", "\u{f103}", "⇊", "End"),
    ("usage", "\u{f021}", "⟳", "Ref"),
    ("model", "\u{f0e7}", "✦", ""),
    ("effort", "\u{f0e4}", "◔", ""), // 仪表盘（nf-fa-tachometer）：窄版 effort 单独一行时打头
    ("account", "\u{f007}", "@", ""),
    ("ctx", "\u{f2db}", "◧", "ctx"),
    ("5h", "\u{f017}", "◷", "5h"),
    ("7d", "\u{f073}", "▦", "7d"),
    ("git", "\u{e702}", "±", ""),
    ("files", "\u{f07c}", "▤", ""),
    ("more", "\u{f141}", "⋯", "..."),
    ("shell", "\u{f120}", "›_", ""),
    ("monitor", "\u{f201}", "▂▅", ""), // 折线图（nf-fa-line_chart）：给自己配的监视器类按钮用
    ("switch", "\u{f0ec}", "⇄", "<>"), // 账号后面：点账号换账号（模型、effort 只显示）
];

static TIER: OnceLock<usize> = OnceLock::new();

/// 本机装没装 Nerd Font：macOS 看 ~/Library/Fonts 和 /Library/Fonts 的文件名。只说明装了，不说明终端在用它。
fn nerd_font_installed() -> bool {
    let dirs = [crate::home().join("Library/Fonts"), std::path::PathBuf::from("/Library/Fonts")];
    dirs.iter().flat_map(std::fs::read_dir).flatten().flatten().any(|e| {
        let n = e.file_name().to_string_lossy().to_lowercase();
        n.contains("nerd font") || n.contains("nerdfont") || {
            let b = n.as_bytes();
            n.match_indices("nf").any(|(i, _)| {
                let before = i == 0 || matches!(b[i - 1], b' ' | b'_' | b'-');
                let after = i + 2 >= b.len() || matches!(b[i + 2], b' ' | b'_' | b'.' | b'-');
                before && after
            })
        }
    })
}

pub fn init_icons(setting: &str) {
    let tier = match setting {
        "nerd" => 0,
        "unicode" => 1,
        "text" => 2,
        _ => {
            if nerd_font_installed() {
                0
            } else {
                1
            }
        }
    };
    let _ = TIER.set(tier);
}

pub fn text_tier() -> bool {
    TIER.get() == Some(&2)
}

/// 0 nerd / 1 unicode / 2 text。
pub fn tier() -> usize {
    *TIER.get().unwrap_or(&1)
}

/// 配置里的按钮：icon 是 ICONS 里的名字就按档位查表，否则当成字形原样用（文字档都不用）。
pub fn button_icon(name: &str) -> String {
    if text_tier() || name.is_empty() {
        String::new()
    } else if ICONS.iter().any(|r| r.0 == name) {
        icon(name)
    } else {
        name.to_owned()
    }
}

/// 插件的 manifest 里没有图标，认识的这几个由我们配：(nerd, unicode)。不认识的、配置里也没给图标的插件不上底部第一层，
/// 在 ⋯ 弹出来的那一层里按名字列出。
pub const PLUGIN_ICONS: [(&str, &str, &str); 3] = [
    ("herdr-sidebar", "\u{f0db}", "▥"),      // 侧栏
    ("herdr-file-viewer", "\u{f15c}", "▭"),  // 看文件
    ("furkankly.zoetrope", "\u{f126}", "⋔"), // 会话流程图
];

pub fn icon(name: &str) -> String {
    let tier = tier();
    let Some(row) = ICONS.iter().find(|r| r.0 == name) else {
        return String::new();
    };
    match tier {
        0 => row.1.to_owned(),
        1 => row.2.to_owned(),
        _ => t(row.3),
    }
}

pub type Button = (String, String, usize); // (文字, 动作, 额外宽度)；动作是空的点了什么都不做（│）

pub fn wide_buttons() -> Vec<Button> {
    vec![
        (icon("narrow"), "narrow".into(), 0),
        (icon("bottom"), "bottom".into(), 0),
        (icon("usage"), "usage".into(), 10),
    ]
}

pub fn narrow_buttons() -> Vec<Button> {
    vec![
        (icon("wide"), "wide".into(), 0),
        (icon("bottom"), "bottom".into(), 0),
        (icon("usage"), "usage".into(), 0),
    ]
}

fn cell_w(text: &str) -> usize {
    SLOT.max(text.width() + 2)
}

/// 一排按钮占多宽（按钮之间隔 GAP）。
pub fn cells_w(buttons: &[Button]) -> usize {
    buttons.iter().map(|(t, _, e)| cell_w(t) + e).sum::<usize>() + GAP * buttons.len().saturating_sub(1)
}

pub fn buttons_line(buttons: &[Button], u: Option<&Usage>, usage_known: bool) -> SLine {
    let mut out: SLine = Vec::new();
    for (k, (text, act, extra)) in buttons.iter().enumerate() {
        if k > 0 {
            out.push((" ".repeat(GAP), None));
        }
        let w = cell_w(text);
        let pad = (w - text.width()) / 2;
        out.push((format!("{}{text}{}", " ".repeat(pad), " ".repeat(w - text.width() - pad)), None));
        if act == "usage" && *extra > 0 {
            let st = if usage_known {
                render::usage_state(u)
            } else {
                (String::new(), None)
            };
            out.extend(render::fit(st, *extra));
        }
    }
    out
}

/// 第几列（从 1 数）落在哪个按钮上；点偏一格也算（格子本身就比图标宽）。
pub fn button_at(buttons: &[Button], col: usize) -> Option<&str> {
    let mut x = 1;
    for (text, act, extra) in buttons {
        let w = cell_w(text) + extra;
        if (x..x + w).contains(&col) {
            return (!act.is_empty()).then_some(act.as_str());
        }
        x += w + GAP;
    }
    None
}

pub struct Info<'a> {
    pub cfg_dir: &'a Path,
    pub model: &'a str,
    pub ctx: &'a str,
    pub usage: Option<&'a Usage>,
    pub found: bool,
    pub turns: usize,
    pub gone: usize,
    pub flash: Option<&'a str>,
    pub up_tag: &'a str, // 「▲ 73」：列表上面还有几轮（到头了是空的）
    pub slot: usize,     // ▲ / ▼ 在行首占几列（一屏放得下就是 0）
}

/// 宽版表头；第二个值是「N 轮」那一行是第几行（从 1 数，点它什么都不做）。
pub fn wide(width: usize, i: &Info) -> (Vec<SLine>, usize) {
    let c = colors();
    let mut head = if !i.found {
        t("这个窗格里没找到 Claude 会话")
    } else if i.gone > 0 {
        t("{gone} 轮已压缩 · 当前 {n} 轮")
            .replace("{gone}", &i.gone.to_string())
            .replace("{n}", &(i.turns - i.gone).to_string())
    } else {
        t("{n} 轮").replace("{n}", &i.turns.to_string())
    };
    let mut head_line: SLine = Vec::new();
    if let Some(msg) = i.flash {
        head_line.push((std::mem::take(&mut head), None));
        head_line.push((" · ".into(), None));
        head_line.push((msg.to_owned(), Some(c.warn)));
    } else {
        head_line.push((head, None));
    }
    let mut lines = vec![buttons_line(&wide_buttons(), i.usage, true)];
    let acct = claude::current_account(i.cfg_dir);
    // 模型名和 effort 中间空一格分开（Opus 5.5 high），斜线就不要了；只显示不能点
    let model = i.model.replacen('/', " ", 1);
    lines.extend(render::status_lines(
        width,
        &model,
        i.ctx,
        i.usage,
        &acct,
        claude::auto_compact_at(),
    ));
    if !i.model.is_empty() || !acct.is_empty() {
        lines[1].push((format!(" {}", icon("switch")), Some(c.accent))); // 模型 · 账号那一行
    }
    lines.push(render::lead(head_line, i.up_tag, i.slot)); // 「N 轮」那一行放在最后，▲ 在它最前面
    let n = lines.len();
    (lines, n)
}

/// 窄版表头：拉回宽版的 « 那一行、模型、账号、ctx / 5h / 7d 百分比。放不下的按 账号 → 模型 → 7d 的顺序丢。
pub fn narrow(width: usize, i: &Info) -> Vec<SLine> {
    let c = colors();
    let room = 4.max(width.saturating_sub(2));
    let first = buttons_line(&narrow_buttons(), None, false);
    let mut lines = vec![first];
    // 提示原文按宽度折到按钮下面，最多三行（以前只画一个「!」，看不出为什么没反应）
    if let Some(msg) = i.flash {
        let mut row = String::new();
        let mut rows = Vec::new();
        for ch in msg.chars() {
            if (row.width() + ch.width().unwrap_or(0)) > room {
                rows.push(std::mem::take(&mut row));
            }
            row.push(ch);
        }
        rows.push(row);
        rows.truncate(3);
        lines.extend(rows.into_iter().map(|r| vec![(r.trim().to_owned(), Some(c.warn))]));
    }
    let tagged = |name: &str, text: &str| -> SLine {
        let ic = icon(name);
        vec![(if ic.is_empty() { text.to_owned() } else { format!("{ic} {text}") }, Some(c.grey))]
    };
    let mut fields: Vec<(u8, SLine)> = Vec::new();
    if !i.model.is_empty() {
        // `Opus 5.5/high` → 一行 `Opus5.5`、下一行 `high`：effort 单独一行、自带图标（用户定的）；只显示不能点
        let (name, effort) = i.model.split_once('/').unwrap_or((i.model, ""));
        fields.push((2, tagged("model", &name.replace(' ', ""))));
        if !effort.is_empty() {
            fields.push((2, tagged("effort", effort)));
        }
    }
    // 账号写完整用户名（用户定的），放不下才退成和别的账号不一样的那截
    let acct_field = |acct: &str| {
        let mut field = tagged("account", acct);
        field.push((format!(" {}", icon("switch")), Some(c.accent)));
        field
    };
    let full = acct_field(&claude::current_account(i.cfg_dir));
    let field = if width_of(&full) <= room { full } else { acct_field(&claude::short_account(i.cfg_dir)) };
    if !claude::current_account(i.cfg_dir).is_empty() {
        fields.push((1, field));
    }
    let lab_w = ["ctx", "5h", "7d"].iter().map(|x| icon(x).width()).max().unwrap_or(0);
    let pad = |name: &str| -> String {
        let ic = icon(name);
        let n = ic.chars().count();
        format!("{ic}{}", " ".repeat(lab_w.saturating_sub(n)))
    };
    if let Some((p, colour)) = render::ctx_pct(i.ctx, claude::auto_compact_at().unwrap_or(claude::CTX_HARD)) {
        fields.push((
            9,
            vec![(pad("ctx"), Some(c.grey)), (" ".into(), None), (format!("{p:3}%"), Some(colour))],
        ));
    }
    if let Some(u) = i.usage {
        for (label, w, prio) in [("5h", &u.five_hour, 9u8), ("7d", &u.seven_day, 3)] {
            if let Some((p, colour)) = render::quota_pct(w, label) {
                let mut line = vec![(pad(label), Some(c.grey)), (" ".into(), None), (format!("{p:3.0}%"), Some(colour))];
                // 后面跟着还有多久重置（用户 2026-10-27：「额度还剩多久更新」）
                if let Some(left) = claude::secs_left(w) {
                    line.push((format!(" {:>6}", render::left_str(left)), Some(c.grey)));
                }
                fields.push((prio, line));
            }
        }
    }
    for prio in [1u8, 2, 3] {
        fields.retain(|(p, ln)| *p != prio || width_of(ln) <= room);
    }
    lines.extend(fields.into_iter().map(|(_, l)| l));
    if i.slot > 0 {
        // ▲ 单独占表头最后一行（离列表最近），不和 7d 挤一行（用户定的）；翻到顶没有 ▲ 也留着这行，列表不跟着上下跳
        lines.push(render::lead(Vec::new(), i.up_tag, i.slot));
    }
    lines.into_iter().map(|l| clip_line(l, room)).collect()
}

pub fn plain_lines(lines: &[SLine]) -> String {
    lines.iter().map(|l| render::plain(l)).collect::<Vec<_>>().join("\n")
}

/// 表头里点了弹账号菜单的那段字：(第几行，从 0 数, 列起, 列止)。账号名到行尾（连 ⇄），按字找，宽版、窄版都一样。
pub fn hot_spots(lines: &[SLine], accts: &[String]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate().skip(1) {
        let p = render::plain(l);
        if let Some(b) = accts.iter().filter(|a| !a.is_empty()).find_map(|a| p.find(a.as_str())) {
            out.push((i, p[..b].width(), p.width()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn hot_spots_find_account() {
        let l = |s: &str| vec![(s.to_owned(), None)];
        let lines = vec![l("buttons"), l("Opus 5.5 high · siyuan.l x"), l("✦ Opus5.5"), l("@ siyuan.l2 ⇄")];
        let got = super::hot_spots(&lines, &["siyuan.l2".into(), "l2".into()]);
        assert_eq!(got, vec![(3, 2, 13)]);
    }
}
