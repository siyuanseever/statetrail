//! 颜色跟着 herdr 的主题走，和 Python 版 theme.py 一样：读 herdr 的 config.toml 里 [theme] name 和 [theme.custom]，
//! 在 themes.json（从 herdr v0.9.5 源码提取的 18 套调色板，编进程序）里查出来。语义照 herdr：
//! overlay1 柱子正常时的填充、overlay0 次要文字、surface1 底纹和分隔线、peach 警告、red 要注意、
//! blue 匀速线 / 自动压缩线、teal 主界面正显示着的轮次、yellow 工作中、green 完成、accent 选中。

use ratatui::style::Color;
use serde_json::Value;
use std::sync::OnceLock;

static THEMES: &str = include_str!("themes.json");
static COLORS: OnceLock<Colors> = OnceLock::new();

#[derive(Clone, Copy)]
pub struct Colors {
    pub used: Color,
    pub grey: Color,
    pub dim: Color,
    pub warn: Color,
    pub alert: Color,
    pub time: Color,
    pub cyan: Color,
    pub busy: Color,
    pub done: Color,
    pub accent: Color,
    pub selection: Option<Color>,
}

const ALIASES: [(&str, &str); 14] = [
    ("catppuccin-mocha", "catppuccin"),
    ("latte", "catppuccin-latte"),
    ("light", "catppuccin-latte"),
    ("tokyonight", "tokyo-night"),
    ("tokyo-day", "tokyo-night-day"),
    ("tokyonight-day", "tokyo-night-day"),
    ("gruvbox-dark", "gruvbox"),
    ("onedark", "one-dark"),
    ("onelight", "one-light"),
    ("solarized-dark", "solarized"),
    ("lotus", "kanagawa-lotus"),
    ("rosepine", "rose-pine"),
    ("rosepine-dawn", "rose-pine-dawn"),
    ("dawn", "rose-pine-dawn"),
];

/// 调色板里的一个颜色：#rrggbb / #rgb，或者 ratatui 的颜色名（"terminal" 主题用终端自己的 16 色）。
fn parse(c: &str) -> Option<Color> {
    let c = c.trim().to_ascii_lowercase();
    if let Some(hex) = c.strip_prefix('#') {
        let v: Vec<u8> = match hex.len() {
            6 => (0..3).filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()).collect(),
            3 => hex.chars().filter_map(|ch| ch.to_digit(16).map(|d| (d * 17) as u8)).collect(),
            _ => vec![],
        };
        return (v.len() == 3).then(|| Color::Rgb(v[0], v[1], v[2]));
    }
    Some(match c.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" => Color::Gray,
        "darkgray" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        _ => Color::Reset,
    })
}

fn herdr_config() -> toml::Table {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".config"));
    std::fs::read_to_string(base.join("herdr/config.toml"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default()
}

fn load() -> Colors {
    let all: Value = serde_json::from_str(THEMES).unwrap_or(Value::Null);
    let conf = herdr_config();
    let th = conf.get("theme").and_then(toml::Value::as_table);
    let name = th.and_then(|t| t.get("name")).and_then(toml::Value::as_str).unwrap_or("catppuccin");
    let name = name.to_ascii_lowercase().replace([' ', '_'], "-");
    let name = ALIASES
        .iter()
        .find(|(a, _)| *a == name)
        .map_or(name.as_str(), |(_, b)| b)
        .to_owned();
    let pal = all["themes"].get(&name).unwrap_or(&all["themes"]["catppuccin"]);
    let custom = th.and_then(|t| t.get("custom")).and_then(toml::Value::as_table);
    let get = |k: &str| -> Color {
        let v = custom
            .and_then(|c| c.get(k))
            .and_then(toml::Value::as_str)
            .or_else(|| pal[k].as_str())
            .unwrap_or("reset");
        parse(v).unwrap_or(Color::Reset)
    };
    let sel = custom
        .and_then(|c| c.get("selection_bg"))
        .and_then(toml::Value::as_str)
        .or_else(|| pal["selection_bg"].as_str())
        .unwrap_or("reset");
    Colors {
        used: get("overlay1"),
        grey: get("overlay0"),
        dim: get("surface1"),
        warn: get("peach"),
        alert: get("red"),
        time: get("blue"),
        cyan: get("teal"),
        busy: get("yellow"),
        done: get("green"),
        accent: get("accent"),
        selection: (sel != "reset").then(|| parse(sel)).flatten(),
    }
}

pub fn colors() -> Colors {
    *COLORS.get_or_init(load)
}
