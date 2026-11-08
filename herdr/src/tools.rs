//! 底部按钮用到的：找目录、找 git 仓库、找程序，列出装了的其他 herdr 插件。和 Python 版 trail.py 的
//! live_dir / git_root / find_repos / load_plugins 一样。

use serde_json::{Value, json};

use crate::header::{self, PLUGIN_ICONS};
pub use statetrail_core::repos::{find_repos, git_root, live_dir, sh_quote, which};
use crate::{Config, herdr};

pub struct Plugin {
    pub id: String,
    pub label: String,
    pub icon: String,
    pub action: String,
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
        out.push(Plugin {
            id: pid.to_owned(),
            label,
            icon,
            action,
        });
    }
    out
}
