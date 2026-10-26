//! 底部按钮用到的：找目录、找 git 仓库、找程序，列出装了的其他 herdr 插件。和 Python 版 trail.py 的
//! live_dir / git_root / find_repos / load_plugins 一样。

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::header::{self, PLUGIN_ICONS};
use crate::{Config, herdr};

/// 这个目录还在就原样返回；被删了（比如 Claude 开在一个已经收掉的 worktree 里）就退到还在的上一级。
pub fn live_dir(path: &str) -> String {
    let mut p = Path::new(path);
    while !p.as_os_str().is_empty() && !p.is_dir() {
        p = p.parent().unwrap_or(Path::new(""));
    }
    if p.as_os_str().is_empty() {
        crate::home().to_string_lossy().into_owned()
    } else {
        p.to_string_lossy().into_owned()
    }
}

/// path 所在的 git 仓库的根；不在仓库里就是 None。
pub fn git_root(path: &str) -> Option<String> {
    let o = std::process::Command::new("git")
        .args(["-C", path, "rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// path 下面一到 depth 层里的 git 仓库（有 .git 的目录，worktree 的 .git 是文件也算），按名字排。
/// 隐藏目录、node_modules 不进；找到仓库就不再往它里面找。
pub fn find_repos(path: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(path, depth, &mut out);
    out
}

fn walk(d: &Path, left: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
    for e in entries {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let p = e.path();
        if p.join(".git").exists() {
            out.push(p);
        } else if left > 1 {
            walk(&p, left - 1, out);
        }
    }
}

/// 在 PATH 里找程序；带 / 的直接看文件在不在。
pub fn which(prog: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let ok = |p: &Path| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if prog.contains('/') {
        return ok(Path::new(prog));
    }
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| ok(&d.join(prog))))
}

/// 给 sh 用的单引号包起来。
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 「监视器」按钮用的 btop 配置：默认布局在 Claude 下面那一块里挤，磁盘列出同一块盘的七八个分区，内存被挤成一窄条。
/// 第一次用时从你自己的 btop 配置复制一份到插件状态目录，只改两行：不要网络框、磁盘只看系统盘。
/// 之后在 btop 里改的设置留在这份里（btop 退出时写回 -c 给的文件），不碰 ~/.config/btop。
pub fn btop_conf() -> Option<PathBuf> {
    let path = crate::state_dir().join("btop.conf");
    if path.exists() {
        return Some(path);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".config"))
        .join("btop/btop.conf");
    let text = lean_btop(&std::fs::read_to_string(base).unwrap_or_default());
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, text).ok()?;
    Some(path)
}

fn lean_btop(orig: &str) -> String {
    let set = [("shown_boxes", "\"cpu mem proc\""), ("disks_filter", "\"/\"")];
    let mut out: Vec<String> = orig
        .lines()
        .filter(|l| !set.iter().any(|(k, _)| l.split('=').next().is_some_and(|x| x.trim() == *k)))
        .map(str::to_owned)
        .collect();
    out.extend(set.iter().map(|(k, v)| format!("{k} = {v}")));
    out.join("\n") + "\n"
}

pub struct Plugin {
    pub id: String,
    pub label: String,
    pub icon: String,
    pub action: String,
    pub actions: Vec<(String, String)>, // (action id, 标题)
}

/// 插件的第一个 action 默认绑什么：有的插件第一个 action 是 overlay，herdr 0.9.5 的 overlay 会盖满整个标签页、
/// 连轨迹一起盖住，这种改绑它自己的别的开法。zoetrope 选 open-tab 不选 open-split：split 开在 Claude 旁边，
/// 把 Claude 从 83 列挤到 42 列；tab 开一个新标签页，原来那一页一点不动（Python 版 2026-10-25 实测）。
const PLUGIN_DEFAULT_ACTION: [(&str, &str); 1] = [("furkankly.zoetrope", "open-tab")];

fn list(v: &Value, key: &str) -> Vec<Value> {
    v.get(key).unwrap_or(v).as_array().cloned().unwrap_or_default()
}

/// 装了、开着的其他 herdr 插件，按 herdr 列出来的顺序。每个插件绑它 manifest 里第一个能在这台机器上、对着窗格用的
/// action（plugin.action.list 是按字母排的，所以顺序看 manifest）；配置 [plugins."<id>"] 可以藏、换绑、换图标、换名字。
pub fn load_plugins(cfg: &Config) -> Vec<Plugin> {
    if !cfg.plugin_buttons {
        return Vec::new();
    }
    let (Ok(plugs), Ok(acts)) = (herdr::call("plugin.list", json!({})), herdr::call("plugin.action.list", json!({}))) else {
        return Vec::new();
    };
    let (plugs, acts) = (list(&plugs, "plugins"), list(&acts, "actions"));
    let plat = if cfg!(target_os = "macos") { "macos" } else { "linux" };
    let me = crate::plugin_id();
    let has = |a: &Value, k: &str, want: &str| a[k].as_array().is_none_or(|v| v.is_empty() || v.iter().any(|x| x == want));
    let mut out = Vec::new();
    for p in &plugs {
        let Some(pid) = p["plugin_id"].as_str() else { continue };
        let mine = cfg.plugins.get(pid).cloned().unwrap_or_default();
        if pid == me || p["enabled"].as_bool() == Some(false) || mine.hidden {
            continue;
        }
        let mut avail: Vec<&Value> = acts
            .iter()
            .filter(|a| a["plugin_id"] == pid && a["action_id"].is_string() && has(a, "platforms", plat) && has(a, "contexts", "pane"))
            .collect();
        let order: Vec<String> = p["manifest_path"]
            .as_str()
            .and_then(|m| std::fs::read_to_string(m).ok())
            .and_then(|s| s.parse::<toml::Table>().ok())
            .and_then(|t| {
                Some(
                    t.get("actions")?
                        .as_array()?
                        .iter()
                        .filter_map(|a| a.get("id")?.as_str().map(str::to_owned))
                        .collect(),
                )
            })
            .unwrap_or_default();
        avail.sort_by_key(|a| order.iter().position(|o| a["action_id"] == o.as_str()).unwrap_or(order.len()));
        let ids: Vec<String> = avail.iter().filter_map(|a| a["action_id"].as_str().map(str::to_owned)).collect();
        if ids.is_empty() {
            continue;
        }
        let fallback = PLUGIN_DEFAULT_ACTION.iter().find(|(id, _)| *id == pid).map(|(_, a)| *a);
        let action = [Some(mine.action.as_str()), fallback]
            .into_iter()
            .flatten()
            .find(|x| ids.iter().any(|i| i == x))
            .unwrap_or(&ids[0])
            .to_owned();
        let label = if mine.label.is_empty() {
            let name = p["name"].as_str().unwrap_or(pid);
            let last = name.rsplit('.').next().unwrap_or(name);
            last.strip_prefix("herdr-").unwrap_or(last).to_owned()
        } else {
            mine.label.clone()
        };
        let icon = match header::tier() {
            2 => String::new(),
            _ if !mine.icon.is_empty() => mine.icon.clone(),
            t => PLUGIN_ICONS
                .iter()
                .find(|r| r.0 == pid)
                .map_or("", |r| if t == 0 { r.1 } else { r.2 })
                .to_owned(),
        };
        let actions = avail
            .iter()
            .filter_map(|a| {
                let id = a["action_id"].as_str()?.to_owned();
                let title = a["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(&id).to_owned();
                Some((id, title))
            })
            .collect();
        out.push(Plugin {
            id: pid.to_owned(),
            label,
            icon,
            action,
            actions,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn lean_btop_overrides_two_keys() {
        let got = super::lean_btop("color_theme = \"x\"\nshown_boxes = \"cpu mem net proc\"\ndisks_filter = \"\"\n");
        assert_eq!(got, "color_theme = \"x\"\nshown_boxes = \"cpu mem proc\"\ndisks_filter = \"/\"\n");
        assert_eq!(super::lean_btop(""), "shown_boxes = \"cpu mem proc\"\ndisks_filter = \"/\"\n");
    }

    use super::*;

    #[test]
    fn repos_and_dirs() {
        let base = std::env::temp_dir().join(format!("statetrail-test-{}", std::process::id()));
        for d in [
            "b/.git",
            "a/sub/.git",
            "a/sub/inner/.git",
            ".hidden/x/.git",
            "node_modules/y/.git",
            "c/d/e/.git",
        ] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::create_dir_all(base.join("w")).unwrap();
        std::fs::write(base.join("w/.git"), "gitdir: elsewhere").unwrap(); // worktree 的 .git 是文件
        let names: Vec<String> = find_repos(&base, 2)
            .iter()
            .map(|p| p.strip_prefix(&base).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a/sub", "b", "w"]); // 不进隐藏目录、node_modules，不进仓库里面，最多两层
        let gone = base.join("b/deleted/deeper");
        assert_eq!(live_dir(&gone.to_string_lossy()), base.join("b").to_string_lossy());
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
