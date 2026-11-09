//! 给别的程序（别的 Claude 会话做跨会话转派）用的两个只读命令，输出 JSON：
//!
//! - `statetrail-tmux list --json [--turns N]`：每个 Claude 窗格现在在干什么
//! - `statetrail-tmux search <词…> --json [--since <ISO 时间>] [--cwd <前缀>] [--limit N] [--no-chapters]`：这事在哪个会话里说过
//!
//! 只读：不改状态（不把 DONE 变 READ、不跳、不调 Haiku 分章）。search 会先把搜索索引补到最新（和轨迹里按 Tab 时一样），
//! 那是缓存，不是状态。状态和状态栏圆点同一份（agents 常驻进程写的 agents.json）。
//! 需求见 docs/AGENT-CLI-REQUEST.md（2026-11-09，另一个 Claude 会话提的，用户同意）。

use serde_json::{Value, json};
use statetrail_core::{chapters::Marks, claude, search, transcript::Transcript};

use crate::{agents, tmux};

const CUT: usize = 200; // prompt / reply_head 截到这么多字

fn cut(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        one
    } else {
        one.chars().take(n).collect::<String>() + "…"
    }
}

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

/// `list --json`
pub fn list(args: &[String]) {
    let n_turns: usize = opt(args, "--turns").and_then(|v| v.parse().ok()).unwrap_or(5);
    let states: Value = std::fs::read(agents::state_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut st = tmux::Status::default();
    st.reload();
    let fmt = "#{pane_id}\t#{session_name}\t#{window_index}\t#{window_name}\t#{pane_current_command}\t\
#{?#{m:*--agent-id*,#{pane_start_command}},1,0}\t#{pane_current_path}";
    let mut out = Vec::new();
    for line in tmux::run(&["list-panes", "-a", "-F", fmt]).unwrap_or_default().lines() {
        let f: Vec<&str> = line.splitn(7, '\t').collect();
        let [pane, sess, widx, wname, cmd, mate, cwd] = f.as_slice() else { continue };
        if !(*cmd == "claude" || tmux::is_version(cmd)) {
            continue;
        }
        let sid = st.session(pane);
        let cfg = st.config_dir(pane).unwrap_or_else(|| claude::config_dir_of_pid(tmux::claude_pid(pane)));
        let (mut chapter, mut turns) = (Value::Null, Vec::new());
        if let Some(path) = sid.as_deref().and_then(|s| claude::transcript_path(s, &cfg)) {
            let mut tr = Transcript::new(&path);
            tr.update();
            let stops = tr.visible();
            let chaps = Marks::load(sid.as_deref().unwrap_or_default()).chapters(&stops);
            chapter = chaps.last().map_or(Value::Null, |c| json!(c.title));
            for (i, s) in stops.iter().enumerate().skip(stops.len().saturating_sub(n_turns)) {
                turns.push(json!({"n": i + 1, "ts": s.ts, "prompt": cut(&s.prompt, CUT), "reply_head": cut(&s.reply, CUT)}));
            }
        }
        let a = &states[*pane];
        out.push(json!({
            "pane": pane,
            "session": sess,
            "window": {"index": widx.parse::<u64>().unwrap_or(0), "name": wname},
            "sid": sid,
            "cwd": cwd,
            "config_dir": cfg,
            "state": tmux::agent_state(a),
            "state_since": a["since"].as_str(),
            "mate": *mate == "1",
            "chapter": chapter,
            "turns": turns,
        }));
    }
    println!("{}", Value::Array(out));
}

/// 每个词都要出现（不分大小写）
fn hit_all(words: &[String], text: &str) -> bool {
    let t = text.to_lowercase();
    words.iter().all(|w| t.contains(w.as_str()))
}

/// `search <词…> --json`
pub fn search(args: &[String]) {
    let mut words = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        match a.as_str() {
            "--since" | "--cwd" | "--limit" => skip = true,
            x if x.starts_with("--") => {}
            x => words.push(x.to_lowercase()),
        }
    }
    if words.is_empty() {
        eprintln!("usage: statetrail-tmux search <word…> --json [--since <ISO time>] [--cwd <prefix>] [--limit N] [--no-chapters]");
        std::process::exit(2);
    }
    let since = opt(args, "--since").unwrap_or("");
    let cwd = opt(args, "--cwd").map(|c| crate::expand(c).to_string_lossy().into_owned());
    let limit: usize = opt(args, "--limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let chapters = !args.iter().any(|a| a == "--no-chapters");
    let all = search::update(|_, _| {});
    let mut st = tmux::Status::default();
    st.reload();
    let open: std::collections::HashMap<String, String> = st.sessions().into_iter().map(|(p, s)| (s, p)).collect();
    let mut hits: Vec<(String, Value)> = Vec::new();
    for s in &all {
        if cwd.as_deref().is_some_and(|c| !s.cwd.starts_with(c)) {
            continue;
        }
        let pane = open.get(&s.sid).cloned();
        let base = |n: usize, ts: &Option<String>| {
            json!({"sid": s.sid, "cwd": s.cwd, "title": s.title, "pane": pane, "n": n, "ts": ts})
        };
        for t in &s.turns {
            if t.ts.as_deref().unwrap_or("") < since || !hit_all(&words, &t.prompt) {
                continue;
            }
            let mut v = base(t.n + 1, &t.ts);
            v["kind"] = json!("turn");
            v["prompt"] = json!(cut(&t.prompt, CUT));
            hits.push((t.ts.clone().unwrap_or_default(), v));
        }
        if chapters {
            // 章名（Haiku 起的）：按这一章开头那一轮记
            for (id, title) in Marks::load(&s.sid).auto {
                let Some(t) = s.turns.iter().find(|t| t.id == id) else { continue };
                if t.ts.as_deref().unwrap_or("") < since || !hit_all(&words, &title) {
                    continue;
                }
                let mut v = base(t.n + 1, &t.ts);
                v["kind"] = json!("chapter");
                v["chapter"] = json!(title);
                v["prompt"] = json!(cut(&t.prompt, CUT));
                hits.push((t.ts.clone().unwrap_or_default(), v));
            }
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.truncate(limit);
    println!("{}", Value::Array(hits.into_iter().map(|(_, v)| v).collect()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_words_must_match() {
        let w = vec!["评论".to_string(), "keyboard".to_string()];
        assert!(hit_all(&w, "评论页 Keyboard 遮挡"));
        assert!(!hit_all(&w, "评论页输入框"));
    }

    #[test]
    fn cut_keeps_one_line() {
        assert_eq!(cut("a\n  b   c", 10), "a b c");
        assert_eq!(cut("abcdef", 3), "abc…");
    }

    #[test]
    fn options_are_read() {
        let a: Vec<String> = ["评论", "--limit", "3", "--json"].iter().map(|s| s.to_string()).collect();
        assert_eq!(opt(&a, "--limit"), Some("3"));
        assert_eq!(opt(&a, "--since"), None);
    }
}
