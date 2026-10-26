//! 换账号：把 herdr 里空闲的 Claude 会话换到另一个账号。退出 → 在原窗格里用另一个账号的配置目录 --resume 同一个会话 id。
//! 从 tmux 版 claude-tmux-sessions/bin/switch_account.py 搬过来，规则、文字一样，只是把 tmux 的读屏 / 发键换成 herdr 的。
//!
//! 几个账号共用同一份会话记录（~/.claude-l2/projects 是指向 ~/.claude/projects 的软链），所以接着聊上下文不丢。
//! 账号在 ~/.config/claude-tmux/accounts.json 里配；没配就只有默认的 ~/.claude 一个，切不了。
//! 安全闸（任何一条不满足就跳过那个窗格、不动它）：
//!   - 必须是空闲（工作中 / 在等确认的不碰）
//!   - 输入框里不能有没发出去的草稿（退出会丢）；灰色的建议输入不算
//!   - 进程必须是交互式 claude（队友、后台会话不碰）
//!   - 已经是目标账号的跳过
//!   - 挂着后台任务 / 定时任务的不碰（退出会把它们停掉），除非调用方确认过（--stop-bg）
//! 退出后等它真回到 shell 才敲新命令；敲完等「新进程的会话文件出现在目标账号目录里、会话 id 没变」才算成功。
//!
//! 用法：statetrail switch --to <账号> [--dry-run] [--stop-bg] [--json] <窗格>...
//! --json：每有进展打一行 {"pane","state","msg"}（state：running / ok / skip / fail），usage-monitor 的「切到 l2」按钮读它。

use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::herdr::{self, ClaudeProc};
use crate::i18n::t;

const TAIL: u32 = 40; // 读屏幕最后多少行来判断
const TEAM_WAIT: Duration = Duration::from_secs(600); // 让领队收队后，最多等这么久队友全部退出
const IDLE_WAIT: Duration = Duration::from_secs(180); // 队友都退出后，最多等领队这一轮停下来
const SHELLS: [&str; 6] = ["zsh", "bash", "fish", "sh", "-zsh", "-bash"];
const KEEP_BOOL: [&str; 5] = ["--dangerously-skip-permissions", "--chrome", "--no-chrome", "--verbose", "--ide"];
const KEEP_VAL: [&str; 7] = [
    "--permission-mode",
    "--model",
    "--effort",
    "--add-dir",
    "--settings",
    "--agent",
    "--append-system-prompt",
];

pub struct Account {
    pub name: String,
    pub dir: PathBuf,
    env: Vec<(String, String)>,
}

/// ~/.config/claude-tmux/accounts.json：{"accounts": {"l": {"config_dir": "~/.claude"}, "l2": {"config_dir": "~/.claude-l2", "env": {...}}}}。
/// config_dir 就是那个账号的 CLAUDE_CONFIG_DIR；env 里的值支持 ~。和 tmux 版读同一份。
pub fn accounts() -> Vec<Account> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| crate::home().join(".config"), PathBuf::from);
    let raw: Value = std::fs::read(base.join("claude-tmux/accounts.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let mut out: Vec<Account> = raw["accounts"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, a)| Account {
            name: name.clone(),
            dir: crate::expand(a["config_dir"].as_str().unwrap_or("~/.claude")),
            env: a["env"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), crate::expand(v.as_str()?).to_string_lossy().into_owned())))
                .collect(),
        })
        .collect();
    if out.is_empty() {
        out.push(Account {
            name: "default".into(),
            dir: crate::claude::default_cfg(),
            env: Vec::new(),
        });
    }
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Running,
    Ok,
    Skip,
    Fail,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Ok => "ok",
            State::Skip => "skip",
            State::Fail => "fail",
        }
    }
}

type Outcome = (State, String);

fn skip(msg: &str) -> Outcome {
    (State::Skip, t(msg))
}

fn fail(msg: String) -> Outcome {
    (State::Fail, msg)
}

fn join(names: &[String]) -> String {
    names.join(if crate::i18n::lang() == "en" { ", " } else { "、" })
}

// ---- 读屏、发键 ---------------------------------------------------------------------

/// 屏幕最后 TAIL 行（去掉空行）。ansi = true 时带颜色，用来认灰色的建议输入。
fn read(pane: &str, ansi: bool) -> Vec<String> {
    herdr::call(
        "pane.read",
        json!({"pane_id": pane, "source": "recent", "lines": TAIL, "format": if ansi { "ansi" } else { "text" }}),
    )
    .ok()
    .and_then(|r| r["read"]["text"].as_str().map(str::to_owned))
    .unwrap_or_default()
    .lines()
    .map(|l| l.trim_end_matches(['\r', ' ']).to_owned())
    .filter(|l| !strip_ansi(l).trim().is_empty())
    .collect()
}

fn keys(pane: &str, k: &[&str]) {
    let _ = herdr::call("pane.send_keys", json!({"pane_id": pane, "keys": k}));
}

/// 往输入框里打一行字再回车（中间停一下，不然 Claude Code 会把回车当成粘贴的一部分）。
fn type_line(pane: &str, text: &str) {
    let _ = herdr::call("pane.send_text", json!({"pane_id": pane, "text": text}));
    std::thread::sleep(Duration::from_millis(400));
    keys(pane, &["enter"]);
}

/// 特征串照 Claude Code 现在的界面写，界面改了就会认不出（落到「空闲」，于是更保守的输入框检查还会兜一道）。
fn screen_state(lines: &[String]) -> &'static str {
    let text = lines.join("\n");
    if ["Do you want", "❯ 1. Yes", "Esc to cancel"].iter().any(|k| text.contains(k)) {
        "ask"
    } else if text.contains("esc to interrupt") {
        "busy"
    } else {
        "idle"
    }
}

/// 底部状态行里有没有后台任务 / 定时任务（「2 shells still running」「1 background task」「scheduled task」）。
fn has_background(lines: &[String]) -> bool {
    let tail = lines[lines.len().saturating_sub(4)..].join("\n");
    if tail.contains("scheduled task") || tail.contains("Background work") {
        return true;
    }
    let words: Vec<&str> = tail.split_whitespace().collect();
    words
        .windows(2)
        .any(|w| w[0].bytes().all(|b| b.is_ascii_digit()) && (w[1].starts_with("shell") || w[1].starts_with("background")))
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            for c in it.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 一段带颜色的文字里，有没有把字设成暗色 / 斜体 / 灰色的 SGR。真正敲进去的字不带任何颜色（herdr 0.9.5 实测）。
fn ghostly(head: &str) -> bool {
    let mut rest = head;
    while let Some(i) = rest.find("\x1b[") {
        rest = &rest[i + 2..];
        let Some(end) = rest.find(|c: char| c.is_ascii_alphabetic()) else {
            break;
        };
        let (body, kind) = (&rest[..end], &rest[end..end + 1]);
        rest = &rest[end + 1..];
        if kind != "m" {
            continue;
        }
        let p: Vec<u32> = body.split([';', ':']).map(|x| x.parse().unwrap_or(0)).collect();
        let mut i = 0;
        while i < p.len() {
            match p[i] {
                2 | 3 => return true,
                38 | 48 if p.get(i + 1) == Some(&5) => {
                    if p[i] == 38 && p.get(i + 2).is_some_and(|n| (238..=250).contains(n)) {
                        return true;
                    }
                    i += 2;
                }
                38 | 48 if p.get(i + 1) == Some(&2) => {
                    if p[i] == 38 && p.len() >= i + 5 && p[i + 2] == p[i + 3] && p[i + 3] == p[i + 4] && (64..=200).contains(&p[i + 2]) {
                        return true;
                    }
                    i += 4;
                }
                _ => {}
            }
            i += 1;
        }
    }
    false
}

/// 输入框里的字是不是 Claude Code 的灰色 / 斜体「建议输入」（按 Tab 才会变成真输入）：看提示符之后、字之前的颜色。
fn is_ghost(raw: &str, text: &str) -> bool {
    let probe: String = text.chars().take(3).collect();
    let Some(i) = raw.find(&probe) else { return false };
    let head = raw[..i].rfind('❯').map_or(&raw[..i], |j| &raw[j..i]);
    ghostly(head)
}

/// 输入框里没发出去的字；空 / 只有占位提示 / 灰色建议输入返回 ""；认不出输入框返回 None。
/// 只认紧跟在横线后面的那一行：输入框上边一定是一条横线（开着队友时下面那块队友列表里也有「❯」，不能从下往上找第一个 ❯）。
fn draft_text(lines: &[String], styled: &[String]) -> Option<String> {
    for i in (1..lines.len()).rev() {
        if !lines[i - 1].trim_start().starts_with("───") {
            continue;
        }
        let ln = lines[i].trim_start_matches(|c: char| c.is_whitespace() || c == '│');
        let Some(rest) = ln.strip_prefix('❯').or_else(|| ln.strip_prefix('>')) else {
            continue;
        };
        let text = rest.trim_end_matches(|c: char| c.is_whitespace() || c == '│').trim_start();
        if text.is_empty() || text.starts_with("Try \"") {
            return Some(String::new());
        }
        let probe: String = text.chars().take(3).collect();
        if let Some(raw) = styled.iter().rev().find(|r| strip_ansi(r).contains(&probe))
            && is_ghost(raw, text)
        {
            return Some(String::new());
        }
        return Some(text.to_owned());
    }
    None
}

// ---- 进程、队友 ---------------------------------------------------------------------

fn session_file(pid: u32, dir: &std::path::Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(dir.join("sessions").join(format!("{pid}.json"))).ok()?).ok()
}

/// 窗格又回到 shell 了：前台只剩 shell，没有 claude。
fn at_shell(pane: &str) -> bool {
    let Ok(r) = herdr::call("pane.process_info", json!({"pane_id": pane})) else {
        return false;
    };
    let fg = r["process_info"]["foreground_processes"].as_array().cloned().unwrap_or_default();
    !fg.is_empty() && fg.iter().all(|p| SHELLS.contains(&p["name"].as_str().unwrap_or("")))
}

/// 原命令行里留下启动选项，换掉 --resume / --continue。
fn relaunch_args(argv: &[String], sid: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        if KEEP_BOOL.contains(&a) {
            out.push(a.to_owned());
        } else if KEEP_VAL.contains(&a) && i + 1 < argv.len() {
            out.extend([a.to_owned(), argv[i + 1].clone()]);
            i += 1;
        } else if a.split_once('=').is_some_and(|(k, _)| KEEP_VAL.contains(&k)) {
            out.push(a.to_owned());
        }
        i += 1;
    }
    out.extend(["--resume".into(), sid.to_owned()]);
    out
}

fn quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// 同一个标签页里除了 pane 自己以外的队友 → [(窗格, 队友名)]。队友是领队起的 claude，命令行带 --agent-id。
fn team_mates(pane: &str) -> Vec<(String, String)> {
    let Some(tab) = herdr::pane_info(pane).and_then(|i| i["tab_id"].as_str().map(str::to_owned)) else {
        return Vec::new();
    };
    let Ok(r) = herdr::call("pane.list", json!({})) else {
        return Vec::new();
    };
    r["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["tab_id"].as_str() == Some(&tab) && p["agent"].as_str() == Some("claude"))
        .filter_map(|p| p["pane_id"].as_str().filter(|q| *q != pane).map(str::to_owned))
        .filter_map(|q| {
            let a = herdr::claude_proc(&q)?.argv;
            a.iter().any(|x| x == "--agent-id").then(|| {
                let name = a.iter().position(|x| x == "--agent-name").and_then(|i| a.get(i + 1)).cloned();
                (q.clone(), name.unwrap_or(q))
            })
        })
        .collect()
}

/// 往领队输入框里打一句话让它收队，等队友都退出、领队停下来。成了返回 None，否则返回失败的结果。
/// 队友没法单独换账号（它们是领队起的进程），所以先让领队把结果收回来、把队友关掉；领队换了账号以后再开队友，就是新账号的。
fn disband(pane: &str, mates: &[(String, String)], log: &dyn Fn(String)) -> Option<Outcome> {
    let names = join(&mates.iter().map(|m| m.1.clone()).collect::<Vec<_>>());
    let msg = t("【切账号】要把这个会话换到另一个账号，得先收队：请让 {names} 把手上的结果交回来，然后把它们全部关掉。全部关掉后就停下，别的什么都不用做。")
        .replace("{names}", &names);
    log(t("让领队收队（{n} 个队友）").replace("{n}", &mates.len().to_string()));
    type_line(pane, &msg);
    let end = Instant::now() + TEAM_WAIT;
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let left = team_mates(pane);
        if left.is_empty() {
            break;
        }
        if Instant::now() >= end {
            let names = join(&left.into_iter().map(|m| m.1).collect::<Vec<_>>());
            return Some(fail(
                t("已让领队收队，{m} 分钟内队友没退完（还剩 {names}），没再动")
                    .replace("{m}", &(TEAM_WAIT.as_secs() / 60).to_string())
                    .replace("{names}", &names),
            ));
        }
        log(t("等队友退出（还剩 {n} 个）").replace("{n}", &left.len().to_string()));
    }
    log(t("队友都退了，等领队停下来"));
    let end = Instant::now() + IDLE_WAIT;
    while Instant::now() < end {
        std::thread::sleep(Duration::from_secs(2));
        if screen_state(&read(pane, false)) == "idle" && !busy(pane) {
            return None;
        }
    }
    Some(fail(t("队友都退了，但领队一直没停下来，没再动")))
}

/// herdr 认的状态：工作中 / 卡在确认上。
fn busy(pane: &str) -> bool {
    herdr::pane_info(pane).is_some_and(|i| matches!(i["agent_status"].as_str(), Some("working" | "blocked")))
}

// ---- 切一个窗格 -------------------------------------------------------------------

pub struct Opts {
    pub dry: bool,
    pub stop_bg: bool,
}

pub fn switch_one(pane: &str, to: &Account, accts: &[Account], o: &Opts, log: &dyn Fn(String), disbanded: bool) -> Outcome {
    if herdr::pane_info(pane).is_none_or(|i| i["agent"].as_str() != Some("claude")) {
        return skip("窗格里没找到交互式 claude 进程");
    }
    let Some(ClaudeProc { pid, argv, shell }) = herdr::claude_proc(pane) else {
        return skip("窗格里没找到交互式 claude 进程");
    };
    let src = crate::claude::config_dir_of_pid(Some(pid));
    if src == to.dir {
        return (State::Skip, t("已经是 {to}").replace("{to}", &to.name));
    }
    if std::env::var("HERDR_PANE_ID").as_deref() == Ok(pane) {
        return skip("这是当前正在用的窗格");
    }
    let lines = read(pane, false);
    if busy(pane) || screen_state(&lines) != "idle" {
        return skip("正在工作或等你确认，没动");
    }
    if !o.stop_bg && has_background(&lines) {
        return skip("它挂着后台任务/定时任务，切换会把它们停掉，没动");
    }
    if argv.iter().any(|a| a == "--agent-id") {
        return skip("队友：随领队收队，不单独切");
    }
    let info = session_file(pid, &src);
    let sid = match info.as_ref() {
        Some(v) if v["kind"].as_str() == Some("interactive") => v["sessionId"].as_str().map(str::to_owned),
        _ => None,
    };
    let Some(sid) = sid else {
        return skip("不是交互式会话（队友/后台），没动");
    };
    if shell.is_none_or(|s| s == pid) {
        return skip("窗格里没有 shell，没法原地重启，没动");
    }
    // 输入框先查：收队要往里打字，里面有你没发出去的字就不能动
    match draft_text(&lines, &read(pane, true)) {
        None => return skip("认不出输入框，为防丢字没动"),
        Some(d) if !d.is_empty() => {
            let d: String = d.chars().take(20).collect();
            return (State::Skip, t("输入框里有没发出去的字（{d}…），没动").replace("{d}", &d));
        }
        _ => {}
    }
    let mates = team_mates(pane);
    if !mates.is_empty() {
        // 重启领队会把它带的队友弄断：先让它收队，收完再切
        if disbanded {
            return fail(t("收过一次队了，窗口里还有队友，没再动"));
        }
        if o.dry {
            let names = join(&mates.iter().map(|m| m.1.clone()).collect::<Vec<_>>());
            return (
                State::Ok,
                t("（演练）会先让领队收队（{names}），收完再退出重启").replace("{names}", &names),
            );
        }
        if let Some(bad) = disband(pane, &mates, log) {
            return bad;
        }
        return switch_one(pane, to, accts, o, log, true);
    }
    // 用 env 写法，不用 shell 函数（比如 claude2）：不是每个窗格的 shell 里都定义了它。别的账号设过的变量先 -u 掉。
    let mut unset: Vec<&str> = accts.iter().flat_map(|a| a.env.iter().map(|(k, _)| k.as_str())).collect();
    unset.push("CLAUDE_CONFIG_DIR");
    unset.sort();
    unset.dedup();
    let mut line = String::from("env");
    for k in unset {
        line += &format!(" -u {k}");
    }
    if to.dir != crate::claude::default_cfg() {
        line += &format!(" CLAUDE_CONFIG_DIR={}", quote(&to.dir.to_string_lossy()));
    }
    for (k, v) in &to.env {
        line += &format!(" {k}={}", quote(v));
    }
    line += " claude";
    for a in relaunch_args(&argv, &sid) {
        line += &format!(" {}", quote(&a));
    }
    if o.dry {
        return (State::Ok, t("（演练）会退出再执行：{line}").replace("{line}", &line));
    }
    log(t("{pane} 退出").replace("{pane}", pane));
    type_line(pane, "/exit");
    let mut out = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(500));
        if at_shell(pane) {
            out = true;
            break;
        }
        if read(pane, false).iter().any(|l| l.contains("Background work is running")) {
            if o.stop_bg {
                keys(pane, &["enter"]); // 调用方确认过要停：默认选中第 1 项「Exit and stop tasks」
                continue;
            }
            keys(pane, &["esc"]); // 「退出并停掉后台任务」的确认：取消，留下
            return skip("它有后台任务，退出会停掉它们，已取消退出，没动");
        }
    }
    if !out {
        return fail(t("它没有退出（可能刚好开始工作），没再动"));
    }
    std::thread::sleep(Duration::from_millis(300));
    type_line(pane, &line);
    // 等新进程的会话文件出现在目标账号目录里，且会话 id 没变，才算切成功
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(np) = herdr::claude_pid(pane)
            && np != pid
            && session_file(np, &to.dir).is_some_and(|v| v["sessionId"].as_str() == Some(&sid))
        {
            // 旁边的轨迹按进程认账号，会话 id 没变它不会自己重查：叫它一声
            for (sb, tg) in crate::sidebars() {
                if tg == pane {
                    crate::pane::send_command(&sb, "rescan");
                }
            }
            return (State::Ok, t("已切到 {to}，接着原会话").replace("{to}", &to.name));
        }
    }
    fail(
        t("已执行 {what}，但 30 秒内没看到它在 {to} 下起来，请看一眼窗格")
            .replace("{what}", &line)
            .replace("{to}", &to.name),
    )
}

// ---- 命令行 ---------------------------------------------------------------------

pub fn cmd_switch(args: &[String]) {
    let accts = accounts();
    let (mut to, mut panes, mut json_out) = (None, Vec::new(), false);
    let mut o = Opts {
        dry: false,
        stop_bg: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--to" => to = it.next().cloned(),
            "--dry-run" => o.dry = true,
            "--stop-bg" => o.stop_bg = true,
            "--json" => json_out = true,
            "--accounts" => {
                // usage-monitor 用：列出账号名（和 accounts.json 一致）
                println!(
                    "{}",
                    json!(accts.iter().map(|a| json!({"name": a.name, "dir": a.dir})).collect::<Vec<_>>())
                );
                return;
            }
            p => panes.push(p.to_owned()),
        }
    }
    let Some(to) = to.and_then(|n| accts.iter().find(|a| a.name == n)) else {
        let names: Vec<&str> = accts.iter().map(|a| a.name.as_str()).collect();
        eprintln!(
            "usage: statetrail switch --to <{}> [--dry-run] [--stop-bg] [--json] <pane>...",
            names.join("|")
        );
        std::process::exit(2);
    };
    if panes.is_empty() {
        eprintln!("{}", t("没有要切的窗格"));
        std::process::exit(2);
    }
    let out = Mutex::new(std::io::stdout());
    let emit = |pane: &str, st: State, msg: &str| {
        let mut w = out.lock().unwrap_or_else(|e| e.into_inner());
        if json_out {
            let _ = writeln!(w, "{}", json!({"pane": pane, "state": st.as_str(), "msg": msg}));
        } else if st != State::Running {
            let _ = writeln!(w, "{:5} {:6} {msg}", st.as_str(), pane);
        }
        let _ = w.flush();
    };
    // 一个窗格一个线程（互不相干；收队可能要等好几分钟）
    std::thread::scope(|s| {
        for p in &panes {
            let (emit, accts, o) = (&emit, &accts, &o);
            s.spawn(move || {
                emit(p, State::Running, &t("切换中…"));
                let (st, msg) = switch_one(p, to, accts, o, &|m| emit(p, State::Running, &m), false);
                emit(p, st, &msg);
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: &[&str]) -> Vec<String> {
        x.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn draft() {
        let rule = "──────────────────────────";
        assert_eq!(draft_text(&v(&[rule, "❯\u{a0}hello draft", rule]), &[]), Some("hello draft".into()));
        assert_eq!(draft_text(&v(&[rule, "❯", rule]), &[]), Some(String::new()));
        assert_eq!(draft_text(&v(&[rule, "❯ Try \"fix lint errors\"", rule]), &[]), Some(String::new()));
        assert_eq!(draft_text(&v(&["no box here"]), &[]), None);
        // 队友列表里选中的那一行也有 ❯，但上面不是横线
        assert_eq!(
            draft_text(&v(&[rule, "❯ ", rule, "  ⏵⏵ auto mode on", "  ❯ ◯ polish-draft  首开体验"]), &[]),
            Some(String::new())
        );
        // 灰色建议输入（herdr 用 24 位色输出）
        let ghost = "\x1b[0m❯\u{a0}\x1b[38;2;153;153;153mcommit this\x1b[0m";
        assert_eq!(
            draft_text(&v(&[rule, "❯\u{a0}commit this", rule]), &v(&[ghost])),
            Some(String::new())
        );
        let dim = "\x1b[0m❯\u{a0}\x1b[2mcommit this";
        assert_eq!(draft_text(&v(&[rule, "❯\u{a0}commit this", rule]), &v(&[dim])), Some(String::new()));
        // 真敲进去的字不带颜色；提示符本身的颜色不算（它在 ❯ 前面）
        let real = "\x1b[38;2;153;153;153m❯\x1b[0m\u{a0}commit this";
        assert_eq!(
            draft_text(&v(&[rule, "❯\u{a0}commit this", rule]), &v(&[real])),
            Some("commit this".into())
        );
    }

    #[test]
    fn truecolor_is_not_dim() {
        // 38;2;r;g;b 里的 2 不是「暗色」
        assert!(!ghostly("\x1b[38;2;255;193;7m"));
        assert!(ghostly("\x1b[38;5;244m"));
        assert!(ghostly("\x1b[3m"));
    }

    #[test]
    fn background() {
        assert!(has_background(&v(&["x", "  ⏵⏵ auto mode on · 2 shells still running"])));
        assert!(has_background(&v(&["1 background task"])));
        assert!(!has_background(&v(&["  ⏵⏵ auto mode on (shift+tab to cycle) · ← 6 agents"])));
    }

    #[test]
    fn relaunch() {
        let a = v(&[
            "claude",
            "--model",
            "opus",
            "--resume",
            "old",
            "--dangerously-skip-permissions",
            "--effort=high",
            "-p",
        ]);
        assert_eq!(
            relaunch_args(&a, "new"),
            v(&[
                "--model",
                "opus",
                "--dangerously-skip-permissions",
                "--effort=high",
                "--resume",
                "new"
            ])
        );
        assert_eq!(quote("/Users/x/.claude-l2"), "/Users/x/.claude-l2");
        assert_eq!(quote("claude2,l2"), "claude2,l2");
        assert_eq!(quote("a b'c"), "'a b'\\''c'");
    }
}
