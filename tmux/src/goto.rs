//! 跳到某个 Claude（prefix+g，`statetrail-tmux goto <窗格>`，在 display-popup 里跑）：浮窗本身在 core 的 goto（和 herdr 版共用），
//! 这里是 tmux 那一层：session 当工作区、窗口当标签页；每个 Claude 的状态读 agents 常驻进程写的 agents.json（和状态栏圆点一样），
//! 会话 id 见 tmux::Status（先看 Claude 自己的会话文件、再看钩子的状态文件）；读屏 capture-pane -e，跳过去 switch-client + select-window + select-pane。

use serde_json::Value;
use statetrail_core::goto::{Agent, Host, Seen, Space};

use crate::{agents, tmux};

struct Tmux {
    caller: Option<String>,
    client: Option<String>, // 哪个客户端开的浮窗：切 session 时点名切它（Collie 的只读客户端挂着时，tmux 自己挑的「当前客户端」可能是它，会被拒）
}

impl Tmux {
    fn switch<'a>(&'a self, args: &mut Vec<&'a str>) {
        if let Some(c) = &self.client {
            args.extend(["-c", c.as_str()]);
        }
    }
}

/// Claude 的终端标题前面是它的状态字（✳ 闲着、◐ 在跑）加一个空格：去掉，剩下会话名
fn strip_glyph(title: &str) -> String {
    let mut c = title.chars();
    match (c.next(), c.clone().next()) {
        (Some(g), Some(' ')) if !g.is_ascii() => c.as_str().trim().to_owned(),
        _ => title.trim().to_owned(),
    }
}

impl Host for Tmux {
    fn spaces(&self) -> Vec<Space> {
        tmux::run(&["list-sessions", "-F", "#{session_id}\t#{session_name}"])
            .unwrap_or_default()
            .lines()
            .enumerate()
            .filter_map(|(i, l)| {
                let (id, name) = l.split_once('\t')?;
                Some(Space { ws: id.to_owned(), num: i as u64 + 1, label: name.to_owned() })
            })
            .collect()
    }

    fn agents(&self) -> Vec<Seen> {
        let states: Value = std::fs::read(agents::state_file())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut st = tmux::Status::default();
        st.reload();
        let fmt = "#{pane_id}\t#{window_id}\t#{session_id}\t#{pane_current_command}\t#{pane_current_path}\t\
#{?#{m:*--agent-id*,#{pane_start_command}},1,0}\t#{pane_title}";
        tmux::run(&["list-panes", "-a", "-F", fmt])
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.splitn(7, '\t').collect();
                let [pane, win, sess, cmd, cwd, mate, title] = f.as_slice() else { return None };
                if !(*cmd == "claude" || tmux::is_version(cmd)) || *mate == "1" {
                    return None;
                }
                let status = match states[*pane].as_str() {
                    Some("wait") => "blocked",
                    Some("run") => "working",
                    Some("done") => "done",
                    _ => "idle",
                };
                Some(Seen {
                    pane: pane.to_string(),
                    tab: win.to_string(),
                    ws: sess.to_string(),
                    status: status.into(),
                    sid: st.session(pane),
                    cwd: cwd.to_string(),
                    title: strip_glyph(title),
                })
            })
            .collect()
    }

    fn screen(&self, pane: &str) -> String {
        tmux::run(&["capture-pane", "-e", "-p", "-t", pane]).unwrap_or_default()
    }

    fn go_pane(&self, a: &Agent) {
        let mut args = vec!["switch-client"];
        self.switch(&mut args);
        args.extend(["-t", &a.pane, ";", "select-window", "-t", &a.pane, ";", "select-pane", "-t", &a.pane]);
        let _ = tmux::run(&args);
    }

    fn go_ws(&self, ws: &str) {
        let mut args = vec!["switch-client"];
        self.switch(&mut args);
        args.extend(["-t", ws]);
        let _ = tmux::run(&args);
    }

    fn caller(&self) -> Option<String> {
        self.caller.clone()
    }
}

/// 浮窗里跑的那个程序：`statetrail-tmux goto <从哪个窗格打开的> [客户端]`
pub fn run(caller: &str, client: &str) {
    let some = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    statetrail_core::goto::run(&Tmux { caller: some(caller), client: some(client) });
}

/// `statetrail-tmux goto-list`：在终端里打出同样的列表（全部展开），顺便量一遍要多久
pub fn list() {
    statetrail_core::goto::cmd_list(&Tmux { caller: None, client: None });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_glyph_is_dropped() {
        assert_eq!(strip_glyph("✳ 新闻小程序德语"), "新闻小程序德语");
        assert_eq!(strip_glyph("◐ fix bug"), "fix bug");
        assert_eq!(strip_glyph("plain title"), "plain title");
    }
}
