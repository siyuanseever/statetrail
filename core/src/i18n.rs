//! 界面文字的中英切换，和 Python 版 core/i18n.py 一样：中文原文就是键，zh 原样返回，en 去 en.tsv 里查，
//! 查不到也原样返回。词表编进程序里。取语言的顺序：STATETRAIL_LANG > 配置里的 lang > 系统 locale > en。

use std::collections::HashMap;
use std::sync::OnceLock;

static TSV: &str = include_str!("en.tsv");
static LANG: OnceLock<&'static str> = OnceLock::new();
static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();

fn norm(v: &str) -> Option<&'static str> {
    let v = v.trim().to_ascii_lowercase();
    if v.starts_with("en") {
        Some("en")
    } else if v.starts_with("zh") {
        Some("zh")
    } else {
        None
    }
}

fn from_locale() -> &'static str {
    for k in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(k)
            && !v.is_empty()
        {
            return if v.to_ascii_lowercase().starts_with("zh") { "zh" } else { "en" };
        }
    }
    "en"
}

/// 程序开始时调一次：配置里的 lang（环境变量 STATETRAIL_LANG 还是优先）。
pub fn init(config_lang: &str) {
    let lang = std::env::var("STATETRAIL_LANG")
        .ok()
        .and_then(|v| norm(&v))
        .or_else(|| norm(config_lang))
        .unwrap_or_else(from_locale);
    let _ = LANG.set(lang);
}

pub fn lang() -> &'static str {
    LANG.get().copied().unwrap_or_else(from_locale)
}

/// 界面上的一句话：zh 原样，en 查表（查不到也原样）。
pub fn t(s: &str) -> String {
    if lang() != "en" {
        return s.to_owned();
    }
    let table = TABLE.get_or_init(|| {
        TSV.lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('\t'))
            .collect()
    });
    table.get(s).map_or_else(|| s.to_owned(), |v| (*v).to_owned())
}
