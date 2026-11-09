//! statetrail 里和宿主无关的部分：读对话记录、额度、画面（表头、列表、柱子）、跳转、分章、搜索。
//! herdr 版（`herdr/`）和 tmux 版（`tmux/`）都用它；和宿主打交道的那一层（开窗格、读屏、发键、事件）各写各的。
//! 宿主启动时先调 `set_state_dir` 定下状态目录（分章、索引、额度读数都放那里）。

pub mod chapters;
pub mod claude;
pub mod detect;
pub mod header;
pub mod i18n;
pub mod jump;
pub mod picker;
pub mod render;
pub mod repos;
pub mod search;
pub mod theme;
pub mod transcript;
pub mod ui;

use std::path::PathBuf;
use std::sync::OnceLock;

static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

/// 宿主定下的状态目录；只认第一次设的。
pub fn set_state_dir(dir: PathBuf) {
    let _ = STATE_DIR.set(dir);
}

/// 状态目录：宿主没设过（测试里）就是 ~/.local/state/statetrail。
pub fn state_dir() -> PathBuf {
    STATE_DIR.get().cloned().unwrap_or_else(|| home().join(".local/state/statetrail"))
}
