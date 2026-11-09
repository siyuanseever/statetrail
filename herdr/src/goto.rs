//! 跳到某个 Claude（prefix+g，`statetrail goto`）：浮窗本身在 core 的 goto（tmux 版共用），这里是 herdr 那一层：
//! 工作区、窗格和它们的状态从 herdr 的 workspace.list / pane.list 来，读屏用 pane.read，跳过去用 *.focus。

use serde_json::{Value, json};
use statetrail_core::goto::{Agent, Host, Seen, Space};

use crate::herdr;

struct Herdr;

impl Host for Herdr {
    fn spaces(&self) -> Vec<Space> {
        let ws = herdr::call("workspace.list", json!({})).unwrap_or(Value::Null);
        ws["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|w| Space {
                ws: w["workspace_id"].as_str().unwrap_or_default().to_owned(),
                num: w["number"].as_u64().unwrap_or(0),
                label: w["label"].as_str().unwrap_or_default().to_owned(),
            })
            .collect()
    }

    fn agents(&self) -> Vec<Seen> {
        let panes = herdr::call("pane.list", json!({})).unwrap_or(Value::Null);
        panes["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["agent"].as_str().is_some_and(|a| !a.is_empty()))
            .map(|p| {
                let s = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
                Seen {
                    pane: s("pane_id"),
                    tab: s("tab_id"),
                    ws: s("workspace_id"),
                    status: s("agent_status"),
                    sid: p["agent_session"]["value"].as_str().map(str::to_owned),
                    cwd: s("cwd"),
                    title: s("terminal_title_stripped"),
                }
            })
            .collect()
    }

    fn screen(&self, pane: &str) -> String {
        herdr::call("pane.read", json!({"pane_id": pane, "source": "visible", "format": "ansi"}))
            .ok()
            .and_then(|r| r["read"]["text"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    fn go_pane(&self, a: &Agent) {
        let _ = herdr::call("workspace.focus", json!({"workspace_id": a.ws}));
        let _ = herdr::call("tab.focus", json!({"tab_id": a.tab}));
        let _ = herdr::call("pane.focus", json!({"pane_id": a.pane}));
    }

    fn go_ws(&self, ws: &str) {
        let _ = herdr::call("workspace.focus", json!({"workspace_id": ws}));
    }

    fn caller(&self) -> Option<String> {
        let ctx: Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default()).unwrap_or(Value::Null);
        ctx["focused_pane_id"]
            .as_str()
            .or_else(|| ctx["pane_id"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                let r = herdr::call("pane.list", json!({})).ok()?;
                r["panes"].as_array()?.iter().find(|p| p["focused"] == true)?["pane_id"]
                    .as_str()
                    .map(str::to_owned)
            })
    }
}

/// 浮窗里跑的那个程序。
pub fn run() {
    statetrail_core::goto::run(&Herdr);
}

/// statetrail goto-list：在终端里打出同样的列表（全部展开），顺便量收集一遍要多久。
pub fn cmd_list() {
    statetrail_core::goto::cmd_list(&Herdr);
}

/// 动作（prefix+g）：开浮窗。
pub fn cmd_open() {
    if let Err(e) = herdr::call("plugin.pane.open", json!({"plugin_id": crate::plugin_id(), "entrypoint": "goto"})) {
        eprintln!("statetrail: {e}");
    }
}
