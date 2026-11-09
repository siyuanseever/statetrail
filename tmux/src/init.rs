//! `statetrail-tmux init`（tmux.conf 里 `run-shell 'statetrail-tmux init'`，放在最后）和 `statetrail-tmux serve`。
//!
//! init 每次加载配置都跑，跑几遍都一样：
//! - 常驻进程没在跑就起 `serve`（轨迹的常驻进程 + 状态栏圆点，一个进程两条线；tmux 关了它自己退）
//! - 状态栏右边最前面加上所有 Claude 的圆点（#{E:@claude_dots}）；`set -g @statetrail_status off` 就不加，自己放
//! - `prefix g` 跳转浮窗，`prefix T` 开 / 关这个 Claude 旁边的轨迹；`@statetrail_keys off` 就不绑
//! - 点状态栏上的圆点跳到那个 Claude、点 session 名切过去：只在 MouseDown1Status 还是 tmux 默认的那条时才绑，
//!   自己改过的不动（把 p<n> / s<n> 两种 range 交给 `statetrail-tmux click` 就行）

use std::time::Duration;

use crate::{Config, agents, daemon, tmux};

const DEFAULT_CLICK: &str = "switch-client -t =";

/// 这把锁现在有没有人拿着（拿得到就立刻放开）
fn held(name: &str) -> bool {
    std::fs::File::create(crate::state_dir().join(name)).is_ok_and(|f| f.try_lock().is_err())
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn init() {
    let _ = std::fs::create_dir_all(crate::state_dir());
    let exe = crate::exe();
    let exe_s = exe.to_string_lossy().into_owned();
    if !held("agents.lock") || !held("daemon.lock") {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let _ = std::process::Command::new(&exe)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0) // 不跟着 run-shell 一起被收掉
            .spawn();
    }
    let opt = |name: &str| tmux::run(&["show", "-gqv", name]).unwrap_or_default().trim().to_owned();
    let mut cmd: Vec<String> = Vec::new();
    let mut push = |args: &[&str]| {
        if !cmd.is_empty() {
            cmd.push(";".into());
        }
        cmd.extend(args.iter().map(|s| s.to_string()));
    };
    if opt("@statetrail_status") != "off" {
        let right = opt("status-right");
        if !right.contains("@claude_dots") {
            push(&["set", "-g", "status-right", &format!("#{{E:@claude_dots}} {right}")]);
        }
        // 圆点一排可能比默认的右半边长
        if opt("status-right-length").parse::<u32>().unwrap_or(0) < 120 {
            push(&["set", "-g", "status-right-length", "120"]);
        }
    }
    if opt("@statetrail_keys") != "off" {
        let e = quote(&exe_s);
        push(&[
            "bind-key",
            "g",
            "run-shell",
            &format!("tmux display-popup -c '#{{client_name}}' -w 95% -h 85% -E \"{e} goto '#{{pane_id}}' '#{{client_name}}'\""),
        ]);
        push(&["bind-key", "T", "run-shell", &format!("{e} toggle '#{{pane_id}}'")]);
        let click = tmux::run(&["list-keys", "-T", "root", "MouseDown1Status"]).unwrap_or_default();
        if click.trim().is_empty() || click.trim_end().ends_with(DEFAULT_CLICK) {
            push(&[
                "bind-key",
                "-n",
                "MouseDown1Status",
                "if-shell",
                "-F",
                "#{||:#{m:p[0-9]*,#{mouse_status_range}},#{m:s[0-9]*,#{mouse_status_range}}}",
                &format!("run-shell \"{e} click '#{{mouse_status_range}}' '#{{client_name}}'\""),
                DEFAULT_CLICK,
            ]);
        }
    }
    if !cmd.is_empty() {
        let args: Vec<&str> = cmd.iter().map(String::as_str).collect();
        let _ = tmux::run(&args);
    }
}

/// 状态栏上点了圆点（p<窗格号>）或 session 名（s<session 号>）：让点的那个客户端切过去
pub fn click(range: &str, client: &str) {
    let mut args: Vec<String> = vec!["switch-client".into()];
    if !client.is_empty() {
        args.extend(["-c".into(), client.to_owned()]);
    }
    if let Some(n) = range.strip_prefix('p') {
        let pane = format!("%{n}");
        args.extend(["-t".into(), pane.clone(), ";".into(), "select-window".into(), "-t".into(), pane.clone(), ";".into(), "select-pane".into(), "-t".into(), pane]);
    } else if let Some(n) = range.strip_prefix('s') {
        args.extend(["-t".into(), format!("${n}")]);
    } else {
        return;
    }
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let _ = tmux::run(&a);
}

/// 轨迹的常驻进程和状态栏圆点，一个进程两条线（各自有锁，launchd 拉起的那两个在跑就让它们）。
/// tmux 没了（连着 15 秒连不上）就退：下次 tmux 起来，init 再拉起来。
pub fn serve(cfg: Config) {
    let _ = std::fs::create_dir_all(crate::state_dir());
    if held("agents.lock") && held("daemon.lock") {
        return; // 两样都有人在跑了（launchd 拉起的那两个）
    }
    std::thread::spawn(|| agents::run(false));
    std::thread::spawn(move || daemon::run(cfg));
    let mut down = 0;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        if tmux::run(&["list-sessions", "-F", "#{session_id}"]).is_some() {
            down = 0;
        } else {
            down += 1;
            if down >= 3 {
                std::process::exit(0);
            }
        }
    }
}
