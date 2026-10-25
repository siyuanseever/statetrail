//! Claude Code 这边的东西：配置目录、找对话记录。第 1 步只搬找对话记录要用的；
//! 额度、模型、上下文在第 2 步搬（对应 Python 版 core/claude.py 的其余部分）。

use std::path::{Path, PathBuf};

pub fn default_cfg() -> PathBuf {
    crate::home().join(".claude")
}

/// 可能的 CLAUDE_CONFIG_DIR：默认的 ~/.claude，加上 ~/.claude-*（一个账号一个目录）。STATETRAIL_CONFIG_DIRS（冒号分隔）可以改。
pub fn config_dirs() -> Vec<PathBuf> {
    if let Ok(env) = std::env::var("STATETRAIL_CONFIG_DIRS") {
        return env.split(':').filter(|d| !d.is_empty()).map(crate::expand).collect();
    }
    let mut extra: Vec<PathBuf> = std::fs::read_dir(crate::home())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".claude-") && e.path().join("projects").is_dir())
        .map(|e| e.path())
        .collect();
    extra.sort();
    let mut out = vec![default_cfg()];
    out.extend(extra);
    out
}

/// 这个进程的 Claude 用的是哪个配置目录：Claude Code 给每个进程写 <配置目录>/sessions/<pid>.json。
pub fn config_dir_of_pid(pid: Option<u32>) -> PathBuf {
    if let Some(pid) = pid {
        for d in config_dirs() {
            if d.join("sessions").join(format!("{pid}.json")).exists() {
                return d;
            }
        }
    }
    default_cfg()
}

/// 会话 id → 对话记录。先在它自己的配置目录里找，再找别的；同一个 id 有好几份就取最新改过的。
pub fn transcript_path(sid: &str, cfg_dir: &Path) -> Option<PathBuf> {
    let name = format!("{sid}.jsonl");
    let mut dirs = vec![cfg_dir.to_owned()];
    dirs.extend(config_dirs().into_iter().filter(|d| d != cfg_dir));
    for d in dirs {
        let mut hits: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(d.join("projects"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|p| p.path().join(&name))
            .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
            .collect();
        hits.sort();
        if let Some((_, p)) = hits.pop() {
            return Some(p);
        }
    }
    None
}
