//! statetrail（tmux 版，Rust）：Claude Code 会话轨迹，贴在 tmux 里 Claude 窗格的右边。
//! 替掉 claude-tmux-sessions 里 Python + fzf 的侧栏（sidebar/trail_daemon.py、claude_trail.py、trail-sidebar.sh）。
//!
//!   statetrail-tmux toggle <窗格>   开 / 关这个 Claude 旁边的轨迹（状态栏按钮、表头 ✕ 调这个）
//!   statetrail-tmux pane <Claude 窗格>  轨迹本体（开出来的窗格里跑的就是它）
//!   statetrail-tmux daemon          常驻：默认打开、Claude 没了就收、队友来了就让、告诉轨迹谁在被看
//!   statetrail-tmux pickrepo <目录>  Git 按钮在不是仓库的目录里：先选一个仓库再开 lazygit
//!
//! 和宿主无关的部分（对话记录、额度、画面、跳转、分章、搜索）在仓库根目录的 core/，herdr 版也用它。

mod agents;
mod daemon;
mod goto;
mod open;
mod pane;
mod tmux;

pub use statetrail_core::{chapters, claude, expand, header, home, i18n, jump, picker, render, repos, search, theme, transcript, ui};

use std::path::{Path, PathBuf};

pub const WIDE_MIN: u16 = 30; // 轨迹窗格至少这么宽才画宽版
pub const NARROW: i64 = 17; // 窄版的列数

pub fn state_dir() -> PathBuf {
    std::env::var_os("STATETRAIL_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state/statetrail-tmux"))
}

/// 每个轨迹一个命令口 run/<轨迹窗格>.sock（常驻进程告诉它谁在被看、表头 ✕、别的轨迹搜到了它的会话）
pub fn run_dir() -> PathBuf {
    state_dir().join("run")
}

fn config_dir() -> PathBuf {
    std::env::var_os("STATETRAIL_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config/statetrail-tmux"))
}

fn config_table() -> toml::Table {
    std::fs::read_to_string(config_dir().join("config.toml"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default()
}

/// 配置（~/.config/statetrail-tmux/config.toml），每一项都可以不写；文件不在或写坏了就全用默认值。
/// tmux 的选项 @trail_auto（0 = 不自动开）和 @trail_shell_height（上面命令行的行数）照旧认，和 Python 侧栏一样。
#[derive(Clone)]
pub struct Config {
    pub auto_open: bool,
    pub narrow_default: bool,
    pub width: f64,      // 宽版占窗口的比例
    pub min_width: i64,  // Claude 窗格至少这么宽才自动开
    pub shell_height: i64,
    pub lang: String,
    pub icons: String,
    pub chapters: String, // "auto" / "manual" / "off"
    pub live_usage: bool, // 额度旧了去查（经 usage-refresh.py）；false 只读现有的读数
    pub scripts: PathBuf, // claude-tmux-sessions 的 bin/（float-tool.sh、switch_account.py）
}

pub fn config() -> Config {
    let t = config_table();
    let b = |k: &str, d: bool| t.get(k).and_then(toml::Value::as_bool).unwrap_or(d);
    let s = |k: &str, d: &str| t.get(k).and_then(toml::Value::as_str).unwrap_or(d).to_owned();
    let i = |k: &str, d: i64| t.get(k).and_then(toml::Value::as_integer).unwrap_or(d);
    Config {
        auto_open: b("auto_open", true),
        narrow_default: s("default_mode", "narrow") != "wide", // 2026-11-09 起默认窄版，和 herdr 插件一样（用户：「默认宽度有点宽了，改成跟 herdr插件里一样」）
        width: t
            .get("width")
            .and_then(toml::Value::as_float)
            .map_or(0.34, |w| w.clamp(0.1, 0.9)),
        min_width: i("min_width", 120),
        shell_height: i("shell_height", 8), // 轨迹上面那条命令行（用户 2026-11-09 试过底部按钮后要回这个）
        lang: s("lang", ""),
        icons: Some(s("icons", "auto"))
            .filter(|v| ["auto", "nerd", "unicode", "text"].contains(&v.as_str()))
            .unwrap_or_else(|| "auto".into()),
        chapters: Some(s("chapters", "auto"))
            .filter(|v| ["auto", "manual", "off"].contains(&v.as_str()))
            .unwrap_or_else(|| "auto".into()),
        live_usage: b("live_usage", true),
        scripts: t
            .get("scripts")
            .and_then(toml::Value::as_str)
            .map(expand)
            .unwrap_or_else(default_scripts),
    }
}

/// claude-tmux-sessions 的 bin/：~/.claude/hooks/float-tool.sh 是 install.sh 装的软链，顺着它找回仓库。
fn default_scripts() -> PathBuf {
    let link = home().join(".claude/hooks/float-tool.sh");
    std::fs::read_link(&link)
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| home().join("projects/claude-tmux-sessions/bin"))
}

/// 这个程序自己（开轨迹窗格时让它跑这个路径；换了新版本它会发现、原地换上）
pub fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("statetrail-tmux"))
}

/// 程序文件的修改时间：换了新版本（install.sh 编好后改名换上），开着的轨迹和常驻进程据此原地重启
pub fn exe_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(exe()).and_then(|m| m.modified()).ok()
}

/// 程序换了（连着两次看到同一份新的，免得装到一半就换），而且新程序跑得起来。seen 记着上次看到的那份。
pub fn code_changed(start: Option<std::time::SystemTime>, seen: &mut Option<std::time::SystemTime>) -> bool {
    let now = exe_mtime();
    if now == start || now.is_none() {
        *seen = None;
        return false;
    }
    if *seen != now {
        *seen = now;
        return false;
    }
    std::process::Command::new(exe())
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 原地换成新程序（同样的参数）；换不了就返回
pub fn restart() {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(exe()).args(std::env::args().skip(1)).exec();
    eprintln!("statetrail-tmux: restart failed: {err}");
}

// ---- 额度：用 claude-tmux-sessions 的读数和刷新脚本，和 tmux 底栏、Python 侧栏同一份 ----

fn live_file(cfg_dir: &Path) -> PathBuf {
    cfg_dir.join("tmux-usage-live.json")
}

/// 去查一次真实额度：usage-refresh.py 自己节流（在飞的请求也算），多叫一次不会多发请求。要一两秒，调用方放线程里。
fn live_refresh(cfg_dir: &Path, force: bool) {
    let script = home().join(".claude/hooks/usage-refresh.py");
    if !script.exists() {
        return;
    }
    let mut cmd = std::process::Command::new("python3");
    cmd.arg(&script);
    if force {
        cmd.arg("--force");
    }
    let _ = cmd
        .arg("--config")
        .arg(cfg_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// 界面语言：配置里的 lang > 环境变量 CLAUDE_TMUX_LANG > ~/.config/claude-tmux/lang > 中文。
/// 后两步和 claude-tmux-sessions 的 lib/i18n.sh 一样，tmux 底栏、picker 和轨迹说同一种话。
fn lang(cfg: &Config) -> String {
    if !cfg.lang.is_empty() {
        return cfg.lang.clone();
    }
    if let Ok(v) = std::env::var("CLAUDE_TMUX_LANG")
        && !v.trim().is_empty()
    {
        return v.trim().to_owned();
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    std::fs::read_to_string(base.join("claude-tmux/lang"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "zh".into())
}

fn theme_source() -> Option<toml::Table> {
    Some(config_table())
}

fn main() {
    // launchd 起的常驻进程 PATH 里没有 Homebrew（git、lazygit、python3 在那里）
    if let Ok(p) = std::env::var("PATH")
        && !p.contains("/opt/homebrew/bin")
    {
        unsafe { std::env::set_var("PATH", format!("/opt/homebrew/bin:/usr/local/bin:{p}")) };
    }
    statetrail_core::set_state_dir(state_dir());
    theme::set_source(theme_source);
    claude::set_live_source(claude::LiveSource {
        file: live_file,
        refresh: live_refresh,
    });
    let cfg = config();
    i18n::init(&lang(&cfg));
    header::init_icons(&cfg.icons);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("pane") => pane::run(cfg, &arg(1)),
        Some("toggle") => open::toggle(&arg(1), &cfg),
        Some("daemon") => daemon::run(cfg),
        Some("agents") => agents::run(arg(1) == "--once"),
        Some("goto") => goto::run(&arg(1), &arg(2)),
        Some("goto-list") => goto::list(),
        Some("pickrepo") => open::cmd_pickrepo(&arg(1)),
        // 开发用：调 n 次 list-panes（量一次 tmux 调用的开销）
        Some("bench-tmux") => {
            for _ in 0..arg(1).parse::<usize>().unwrap_or(50) {
                let _ = tmux::run(&["list-panes", "-a", "-F", daemon::FMT]);
            }
        }
        Some("version") => println!("statetrail-tmux {}", env!("CARGO_PKG_VERSION")),
        // 开发用：一份对话记录切出来的轮次，一行一个 JSON
        Some("dump") => {
            let mut tr = transcript::Transcript::new(Path::new(&arg(1)));
            tr.update();
            for s in tr.visible() {
                println!("{}", serde_json::json!({"id": s.id, "kind": s.kind, "prompt": s.prompt, "reply": s.reply}));
            }
        }
        _ => {
            eprintln!("usage: statetrail-tmux {{toggle <pane>|pane <claude-pane>|daemon|agents [--once]|goto <pane> [client]|goto-list|pickrepo <dir>|version}}");
            std::process::exit(2);
        }
    }
}
