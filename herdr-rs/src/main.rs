//! statetrail（herdr 版，Rust）：Claude Code 会话轨迹，停在 Claude 窗格右边。
//! 子命令和 Python 版 trail.py 一一对应：pane（轨迹窗格本身）、toggle（插件动作）、auto（事件钩子：默认打开）。

mod claude;
mod herdr;
mod i18n;
mod jump;
mod pane;
mod transcript;
mod ui;

use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;

pub const MIN_WIDTH: i64 = 120; // Claude 那一格至少这么宽才自动开
pub const WIDE_MIN: u16 = 30; // 轨迹窗格（tty）至少这么宽才画宽版
pub const FRAME: i64 = 2; // herdr 给窗格画的边框：tty 比 pane.layout 的矩形窄 2 列
pub const CLAUDE_MIN: i64 = 110; // 宽版总要给 Claude 留这么多列

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

pub fn plugin_id() -> String {
    std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "statetrail".into())
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state/herdr/plugins").join(plugin_id()))
}

/// 每个轨迹一份登记 <轨迹窗格>.json：{sidebar, target, pid}，和 Python 版同一个格式（发版前的资源检查靠它找进程）。
pub fn run_dir() -> PathBuf {
    state_dir().join("run")
}

fn config_dir() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config/herdr/plugins/config").join(plugin_id()))
}

/// 配置（<配置目录>/config.toml），和 Python 版同名同义；这一步只用到这几项，文件不在或写坏了就全用默认值。
#[derive(Clone)]
pub struct Config {
    pub auto_open: bool,
    pub narrow_default: bool,
    pub width: f64,
    pub lang: String,
}

pub fn config() -> Config {
    let table: toml::Table = std::fs::read_to_string(config_dir().join("config.toml"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    Config {
        auto_open: table.get("auto_open").and_then(toml::Value::as_bool).unwrap_or(true),
        narrow_default: table.get("default_mode").and_then(toml::Value::as_str) == Some("narrow"),
        width: table
            .get("width")
            .and_then(toml::Value::as_float)
            .map_or(0.25, |w| w.clamp(0.1, 0.9)),
        lang: table.get("lang").and_then(toml::Value::as_str).unwrap_or("").to_owned(),
    }
}

/// {轨迹窗格: Claude 窗格}，只留还活着的；窗格已经不在、放了 10 秒以上的文件顺手删掉。
pub fn sidebars() -> HashMap<String, String> {
    let Ok(r) = herdr::call("pane.list", json!({})) else {
        return HashMap::new();
    };
    let alive: Vec<&str> = r["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["pane_id"].as_str())
        .collect();
    let _ = std::fs::create_dir_all(run_dir());
    let mut out = HashMap::new();
    let entries: Vec<_> = std::fs::read_dir(run_dir()).into_iter().flatten().flatten().collect();
    for e in &entries {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let reg: Option<Value> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
        if let Some((sb, tg)) = reg.as_ref().and_then(|v| Some((v["sidebar"].as_str()?, v["target"].as_str()?)))
            && alive.contains(&sb)
        {
            out.insert(sb.to_owned(), tg.to_owned());
        }
    }
    // 轨迹被强杀、没来得及收拾：它名下的文件（.json / .sock）按名字认，窗格已经不在、放了 10 秒以上的删掉
    let keep: Vec<String> = out.keys().map(|sb| sb.replace(':', "_")).collect();
    for e in &entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let stem = name.split('.').next().unwrap_or("");
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or_default().as_secs() > 10);
        if !keep.iter().any(|k| k == stem) && old.unwrap_or(false) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    out
}

fn rects(pane: &str) -> HashMap<String, Value> {
    herdr::call("pane.layout", json!({"pane_id": pane}))
        .ok()
        .and_then(|r| r["layout"]["panes"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| Some((p["pane_id"].as_str()?.to_owned(), p["rect"].clone())))
        .collect()
}

/// 宽版的列数（窗格矩形）：width 那么宽，但给 Claude 留够 CLAUDE_MIN 列；放不下宽版就是 0（用窄版）。
pub fn wide_cols(total: i64, width: f64) -> i64 {
    let cols = ((total as f64) * width).round() as i64;
    let cols = cols.min(total - CLAUDE_MIN - FRAME);
    if cols >= WIDE_MIN as i64 + FRAME { cols } else { 0 }
}

fn open_sidebar(target: &str, cfg: &Config) -> herdr::Result<String> {
    let r = herdr::call(
        "plugin.pane.open",
        json!({"plugin_id": plugin_id(), "entrypoint": "trail", "placement": "split", "target_pane_id": target,
               "direction": "right", "focus": false, "env": {"STATETRAIL_TARGET": target}}),
    )?;
    let pane = r["plugin_pane"]["pane"]["pane_id"].as_str().unwrap_or_default().to_owned();
    // 轨迹进程起来要一会儿，它写登记之前紧跟着来的第二个事件会以为还没开、再开一个：这里先写上
    // 只在还没有登记时写：轨迹进程起得快，可能已经写好了带进程号的那份，别把它盖掉
    let _ = std::fs::create_dir_all(run_dir());
    if let Ok(mut f) = std::fs::File::create_new(run_dir().join(format!("{}.json", pane.replace(':', "_")))) {
        let _ = std::io::Write::write_all(&mut f, json!({"sidebar": pane, "target": target}).to_string().as_bytes());
    }
    // 开出来是对半分：对 Claude 那一格做 resize right，把中间那条线推到 width（挪的只是两格之间那条线）
    let ratio = if cfg.narrow_default {
        0.1
    } else {
        let r = rects(target);
        let w = |p: &str| r.get(p).and_then(|v| v["width"].as_i64()).unwrap_or(0);
        let total = w(target) + w(&pane);
        let cols = wide_cols(total, cfg.width);
        if total > 0 && cols > 0 { cols as f64 / total as f64 } else { 0.1 }
    };
    herdr::call(
        "pane.resize",
        json!({"pane_id": target, "direction": "right", "amount": 0.5 - ratio}),
    )?;
    Ok(pane)
}

fn locked<T>(f: impl FnOnce() -> T) -> T {
    let _ = std::fs::create_dir_all(state_dir());
    let lock = File::create(state_dir().join("open.lock"));
    if let Ok(l) = &lock {
        let _ = l.lock(); // 开 / 切要串行：焦点一变会连着来好几个事件，各起一个进程
    }
    f()
}

/// 事件钩子：tab 里正好有一个 Claude、够宽、还没开着时，在它右边开轨迹。
fn cmd_auto(cfg: &Config) {
    if !cfg.auto_open {
        return;
    }
    let raw = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
    let ev: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let data = if ev["data"].is_object() { &ev["data"] } else { &ev };
    let pane = data["pane_id"].as_str().or_else(|| data["pane"]["pane_id"].as_str());
    let tab = data["tab_id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| data["pane"]["tab_id"].as_str().map(str::to_owned))
        .or_else(|| {
            pane.and_then(herdr::pane_info)
                .and_then(|i| i["tab_id"].as_str().map(str::to_owned))
        })
        .or_else(|| std::env::var("HERDR_TAB_ID").ok());
    let Some(tab) = tab else { return };
    locked(|| {
        let Ok(r) = herdr::call("pane.list", json!({})) else { return };
        let claudes: Vec<String> = r["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["tab_id"].as_str() == Some(&tab) && p["agent"].as_str() == Some("claude"))
            .filter_map(|p| p["pane_id"].as_str().map(str::to_owned))
            .collect();
        let [c] = claudes.as_slice() else { return }; // 好几个 Claude（agent teams）时再切一块谁都看不清，不开
        if sidebars().values().any(|t| t == c) {
            return;
        }
        let w = rects(c).get(c).and_then(|v| v["width"].as_i64()).unwrap_or(0);
        if w >= MIN_WIDTH {
            let _ = open_sidebar(c, cfg);
        }
    });
}

/// 插件动作：焦点所在的 Claude 旁边没有轨迹就开一个；有就让它在宽版 / 窄版之间切换。
fn cmd_toggle(cfg: &Config) {
    let ctx: Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default()).unwrap_or(Value::Null);
    let focused = std::env::args()
        .nth(2)
        .or_else(|| ctx["focused_pane_id"].as_str().map(str::to_owned))
        .or_else(|| std::env::var("HERDR_PANE_ID").ok());
    let Some(focused) = focused else { return };
    locked(|| {
        for (sb, tg) in sidebars() {
            if focused == sb || focused == tg {
                pane::send_command(&sb, "toggle");
                return;
            }
        }
        if herdr::pane_info(&focused).as_ref().and_then(herdr::session_id).is_some() {
            let _ = open_sidebar(&focused, cfg);
        }
    });
}

fn main() {
    let cfg = config();
    i18n::init(&cfg.lang);
    match std::env::args().nth(1).as_deref() {
        Some("pane") => pane::run(cfg),
        Some("auto") => cmd_auto(&cfg),
        Some("toggle") => cmd_toggle(&cfg),
        // 开发用：把一份对话记录切出来的轮次一行一个 JSON 打出来，和 Python 版对比
        Some("dump") => {
            let mut tr = transcript::Transcript::new(std::path::Path::new(&std::env::args().nth(2).unwrap_or_default()));
            tr.update();
            for s in tr.visible() {
                println!(
                    "{}",
                    json!({"id": s.id, "kind": s.kind, "prompt": s.prompt, "reply": s.reply, "ts": s.ts,
                                      "compacted": s.compacted, "has_text": s.has_text})
                );
            }
        }
        _ => {
            eprintln!("usage: statetrail {{pane|auto|toggle}}");
            std::process::exit(2);
        }
    }
}
