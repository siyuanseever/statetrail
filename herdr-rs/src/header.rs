//! 表头：第 1 行按钮（宽版 » ⇊ ⟳ + 额度查询状态，窄版 « ⇊ ⟳），下面宽版是模型账号 + ctx / 5h / 7d 柱子 + 「N 轮」，
//! 窄版是模型、账号、三个百分比各一行。和 Python 版 trail.py 的 wide_header / narrow_header 一样。

use std::path::Path;
use std::sync::OnceLock;
use unicode_width::UnicodeWidthStr;

use crate::claude::{self, Usage};
use crate::i18n::t;
use crate::render::{self, SLine, clip_line, width_of};
use crate::theme::colors;

pub const SLOT: usize = 3; // 图标按钮占 3 格（空格 + 图标 + 空格）
pub const GAP: usize = 1;

/// 图标三档：(nerd, unicode, text)。文字档的字跟着界面语言。
const ICONS: [(&str, &str, &str, &str); 13] = [
    ("narrow", "\u{f101}", "»", ">>"),
    ("wide", "\u{f100}", "«", "<<"),
    ("bottom", "\u{f103}", "⇊", "End"),
    ("usage", "\u{f021}", "⟳", "Ref"),
    ("model", "\u{f0e7}", "✦", ""),
    ("account", "\u{f007}", "@", ""),
    ("ctx", "\u{f2db}", "◧", "ctx"),
    ("5h", "\u{f017}", "◷", "5h"),
    ("7d", "\u{f073}", "▦", "7d"),
    ("git", "\u{e702}", "±", ""),
    ("files", "\u{f07c}", "▤", ""),
    ("more", "\u{f141}", "⋯", "..."),
    ("shell", "\u{f120}", "›_", ""),
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

pub fn icon(name: &str) -> String {
    let tier = *TIER.get().unwrap_or(&1);
    let Some(row) = ICONS.iter().find(|r| r.0 == name) else {
        return String::new();
    };
    match tier {
        0 => row.1.to_owned(),
        1 => row.2.to_owned(),
        _ => t(row.3),
    }
}

pub type Button = (String, &'static str, usize); // (文字, 动作, 额外宽度)

pub fn wide_buttons() -> Vec<Button> {
    vec![
        (icon("narrow"), "narrow", 0),
        (icon("bottom"), "bottom", 0),
        (icon("usage"), "usage", 10),
    ]
}

pub fn narrow_buttons() -> Vec<Button> {
    vec![
        (icon("wide"), "wide", 0),
        (icon("bottom"), "bottom", 0),
        (icon("usage"), "usage", 0),
    ]
}

fn cell_w(text: &str) -> usize {
    SLOT.max(text.width() + 2)
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
        if *act == "usage" && *extra > 0 {
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
pub fn button_at(buttons: &[Button], col: usize) -> Option<&'static str> {
    let mut x = 1;
    for (text, act, extra) in buttons {
        let w = cell_w(text) + extra;
        if (x..x + w).contains(&col) {
            return Some(act);
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
    lines.extend(render::status_lines(
        width,
        i.model,
        i.ctx,
        i.usage,
        &claude::current_account(i.cfg_dir),
        claude::auto_compact_at(),
    ));
    lines.push(head_line);
    let n = lines.len();
    (lines, n)
}

/// 窄版表头：拉回宽版的 « 那一行、模型、账号、ctx / 5h / 7d 百分比。放不下的按 账号 → 模型 → 7d 的顺序丢。
pub fn narrow(width: usize, i: &Info) -> Vec<SLine> {
    let c = colors();
    let room = 4.max(width.saturating_sub(2));
    let mut first = buttons_line(&narrow_buttons(), None, false);
    if i.flash.is_some() {
        first.push((" !".into(), Some(c.warn)));
    }
    let mut lines = vec![first];
    let tagged = |name: &str, text: &str| -> SLine {
        let ic = icon(name);
        vec![(if ic.is_empty() { text.to_owned() } else { format!("{ic} {text}") }, Some(c.grey))]
    };
    let mut fields: Vec<(u8, SLine)> = Vec::new();
    if !i.model.is_empty() {
        // `Opus 5.5/high` → `Opus5.5/hi`：窄版一行只有十来列
        let (name, effort) = i.model.split_once('/').unwrap_or((i.model, ""));
        let mut short = name.replace(' ', "");
        if !effort.is_empty() {
            short = format!("{short}/{}", effort.chars().take(2).collect::<String>());
        }
        fields.push((2, tagged("model", &short)));
    }
    let acct = claude::short_account(i.cfg_dir);
    if !acct.is_empty() {
        fields.push((1, tagged("account", &acct)));
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
                fields.push((
                    prio,
                    vec![(pad(label), Some(c.grey)), (" ".into(), None), (format!("{p:3.0}%"), Some(colour))],
                ));
            }
        }
    }
    for prio in [1u8, 2, 3] {
        fields.retain(|(p, ln)| *p != prio || width_of(ln) <= room);
    }
    lines.extend(fields.into_iter().map(|(_, l)| l));
    lines.into_iter().map(|l| clip_line(l, room)).collect()
}

pub fn plain_lines(lines: &[SLine]) -> String {
    lines.iter().map(|l| render::plain(l)).collect::<Vec<_>>().join("\n")
}
