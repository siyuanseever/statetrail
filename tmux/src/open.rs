//! 开、关轨迹（和 Python 版 trail-sidebar.sh 一样的规矩），以及 Git 按钮在不是仓库的目录里时的选仓库浮窗。
//!
//! 窗格上的标记和 Python 侧栏通用（claude-tmux-sessions 的 tmux 配置、resurrect 钩子、float-tool.sh 都认这几个）：
//!   @trail_for <Claude 窗格>   打在轨迹窗格上
//!   @trail_shell <Claude 窗格> 打在轨迹上面那条命令行上（不算侧栏，也不算「窗口里又多了一格」）
//!   @trail_off 1              打在 Claude 窗格上：你关过，常驻进程就不再自动开
//!   @nofocus 1                点轨迹不抢焦点（tmux.conf.local 的 MouseDown1Pane 认它，只转发点击）
//!   @resurrect_skip 1         tmux-resurrect 恢复时不留空壳（bin/resurrect_hooks.py 认它）

use crate::repos::{find_repos, git_root, live_dir, sh_quote};
use crate::tmux;
use crate::{Config, NARROW};

/// 窗口里某个选项等于 value 的那一格
pub fn find_pane(any: &str, opt: &str, value: &str) -> Option<String> {
    tmux::run(&["list-panes", "-t", any, "-F", &format!("#{{pane_id}} #{{{opt}}}")])?
        .lines()
        .find_map(|l| {
            let (p, v) = l.split_once(' ')?;
            (v == value).then(|| p.to_owned())
        })
}

/// 窗口里平铺着几个 Claude（Claude Code 的队友 agent teams 开出来的每一格都是 Claude；你自己切的命令行不算）。
pub fn tiled_claudes(any: &str) -> usize {
    tmux::run(&[
        "list-panes",
        "-t",
        any,
        "-F",
        "#{pane_floating_flag}#{@picker_float}#{@trail_for}#{@trail_shell} #{pane_current_command}",
    ])
    .unwrap_or_default()
    .lines()
    .filter(|l| l.split_once(' ').is_some_and(|(flags, cmd)| flags == "0" && tmux::is_version(cmd)))
    .count()
}

fn shell_height(cfg: &Config) -> i64 {
    tmux::run(&["show", "-gqv", "@trail_shell_height"])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(cfg.shell_height)
}

/// 在 caller（Claude 窗格）右边开轨迹，上面切一条命令行。窗口里已经有队友：改成浮在右边，不挤领队。
pub fn open(caller: &str, cfg: &Config) -> Option<String> {
    let cmd = format!("exec {} pane {caller}", sh_quote(&crate::exe().to_string_lossy()));
    if tiled_claudes(caller) > 1 {
        return open_float(caller, &cmd);
    }
    let size = if cfg.narrow_default {
        NARROW.to_string()
    } else {
        format!("{}%", (cfg.width * 100.0).round() as i64)
    };
    // -d：焦点留在 Claude 那边，接着打字不受影响
    let sb = tmux::run(&["split-window", "-h", "-d", "-l", &size, "-t", caller, "-P", "-F", "#{pane_id}", &cmd])?;
    let sb = sb.trim().to_owned();
    mark(&sb, caller);
    open_shell(&sb, caller, shell_height(cfg));
    Some(sb)
}

fn mark(sb: &str, caller: &str) {
    tmux::set_opt(sb, "@trail_for", caller);
    tmux::set_opt(sb, "@nofocus", "1");
    tmux::set_opt(sb, "@resurrect_skip", "1");
}

/// 轨迹上面那条命令行（-b 切在上方：轨迹点得多，放下面离手近）。目录跟 Claude 一样，不设 @nofocus：点它就是要打字。
fn open_shell(sb: &str, caller: &str, h: i64) {
    if h <= 0 {
        return;
    }
    let cwd = tmux::display(caller, "#{pane_current_path}").unwrap_or_default();
    let h = h.to_string();
    let mut args = vec!["split-window", "-v", "-b", "-d", "-l", &h, "-t", sb, "-P", "-F", "#{pane_id}"];
    if !cwd.is_empty() {
        args.extend(["-c", &cwd]);
    }
    if let Some(sh) = tmux::run(&args) {
        let sh = sh.trim();
        tmux::set_opt(sh, "@trail_shell", caller);
        tmux::set_opt(sh, "@resurrect_skip", "1");
    }
}

/// 底部 ›_ 按钮：在 Claude 下面切出一条命令行（Claude 那一格的一半高，目录一样），再点一次关掉。和 herdr 插件一样，
/// 点外面不关：跑到一半的 ssh 不能因为点了一下 Claude 就没了。打 @trail_shell：常驻进程不把它算成队友，轨迹收掉时一起收。
pub fn toggle_shell(caller: &str) {
    if let Some(sh) = find_pane(caller, "@trail_shell", caller) {
        let _ = tmux::run(&["kill-pane", "-t", &sh]);
        let _ = tmux::run(&["select-pane", "-t", caller]);
        return;
    }
    let cwd = tmux::display(caller, "#{pane_current_path}").unwrap_or_default();
    let mut args = vec!["split-window", "-v", "-l", "50%", "-t", caller, "-P", "-F", "#{pane_id}"];
    if !cwd.is_empty() {
        args.extend(["-c", &cwd]);
    }
    if let Some(sh) = tmux::run(&args) {
        let sh = sh.trim();
        tmux::set_opt(sh, "@trail_shell", caller);
        tmux::set_opt(sh, "@resurrect_skip", "1");
    }
}

/// 队友模式：浮在窗口右边，盖住队友那一侧的一部分，领队那一条原样不动。不切命令行（浮窗再切一块更挤）。
fn open_float(caller: &str, cmd: &str) -> Option<String> {
    let wh = tmux::display(caller, "#{window_width} #{window_height}")?;
    let (ww, wh) = wh.split_once(' ')?;
    let (ww, wh): (i64, i64) = (ww.parse().ok()?, wh.parse().ok()?);
    // 浮窗四周各占一格边框：内容区最多 (宽-2)×(高-2)，左上角至少在 (1,1)
    let w = (ww * 34 / 100).max(40).min(ww - 2);
    let (x, y, h) = ((ww - 1 - w).to_string(), "1".to_string(), (wh - 2).to_string());
    let w = w.to_string();
    let sp = tmux::run(&["new-pane", "-d", "-t", caller, "-P", "-F", "#{pane_id}", "-x", &w, "-y", &h, "-X", &x, "-Y", &y, cmd])?;
    let sp = sp.trim().to_owned();
    mark(&sp, caller);
    // Claude Code 再加队友时会 select-layout，把浮窗吸进平铺树：被吸进去就自己关掉，别留一个乱窗格
    let hook = format!("if -F '#{{!:#{{pane_floating_flag}}}}' {{ run-shell -b \"tmux kill-pane -t {sp} 2>/dev/null; true\" }}");
    let _ = tmux::run(&["set-hook", "-p", "-t", &sp, "window-layout-changed", &hook]);
    Some(sp)
}

/// 关掉 owner（Claude 窗格）的轨迹和它上面那条命令行，并记住是你关的。两格的号都先查出来再杀：
/// 焦点在命令行里时先杀了它，后面就查不到了。
pub fn close(owner: &str) {
    tmux::set_opt(owner, "@trail_off", "1");
    let sh = find_pane(owner, "@trail_shell", owner);
    let sb = find_pane(owner, "@trail_for", owner);
    for p in [sb, sh].into_iter().flatten() {
        let _ = tmux::run(&["kill-pane", "-t", &p]);
    }
}

/// 状态栏「轨迹」按钮：caller 是 Claude 就开 / 关它旁边的轨迹；是轨迹或它上面的命令行，就关掉这一套。
pub fn toggle(caller: &str, cfg: &Config) {
    if caller.is_empty() {
        return;
    }
    let owner = tmux::display(caller, "#{@trail_for}#{@trail_shell}").unwrap_or_default();
    if !owner.is_empty() {
        close(&owner);
        return;
    }
    if find_pane(caller, "@trail_for", caller).is_some() {
        close(caller);
        return;
    }
    let mut st = tmux::Status::default();
    st.reload();
    if tmux::session_of(caller, &st).is_none() {
        tmux::message(caller, &crate::i18n::t("轨迹：这个窗格里没找到 Claude 会话"));
        return;
    }
    tmux::unset_opt(caller, "@trail_off");
    let _ = open(caller, cfg);
}

// ---- Git 按钮：不在仓库里时先选仓库 ------------------------------------------------------

/// 开一个浮在右边的工具窗（和 float-tool.sh 同一套约定：@picker_float 是 tool:git，点外面就关）。
/// 窗口里已经开着一个工具：先关掉它；关的就是同一种就到此为止（再点一次 = 关）。
pub fn float_tool(caller: &str, kind: &str, dir: &str, cmd: &str) {
    if let Some(list) = tmux::run(&["list-panes", "-t", caller, "-F", "#{pane_id} #{@picker_float}"]) {
        for l in list.lines() {
            if let Some((p, k)) = l.split_once(' ')
                && !k.is_empty()
            {
                let _ = tmux::run(&["kill-pane", "-t", p]);
                if k == kind {
                    return;
                }
            }
        }
    }
    let Some(size) = tmux::display(caller, "#{window_width} #{window_height}") else { return };
    let Some((ww, wh)) = size.split_once(' ').and_then(|(a, b)| Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?))) else {
        return;
    };
    let pct = std::fs::read_to_string(crate::home().join(".local/state/float-tool-width"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(60)
        .clamp(30, 95);
    let w = (ww - 1) * pct / 100;
    let (x, h) = ((ww - 1 - w).to_string(), (wh - 2).to_string());
    let w = w.to_string();
    let Some(fp) = tmux::run(&["new-pane", "-t", caller, "-P", "-F", "#{pane_id}", "-x", &w, "-y", &h, "-X", &x, "-Y", "1", "-c", dir, cmd]) else {
        return;
    };
    let fp = fp.trim();
    tmux::set_opt(fp, "@picker_float", kind);
    tmux::set_opt(fp, "@picker_caller", caller);
    let ft = crate::config().scripts.join("float-tool.sh");
    let _ = tmux::run(&["set-hook", "-p", "-t", fp, "pane-focus-out", &format!("run-shell -b '\"{}\" --maybe-close {fp}'", ft.display())]);
    let hook = format!("if -F '#{{!:#{{pane_floating_flag}}}}' {{ run-shell -b \"tmux kill-pane -t {fp} 2>/dev/null; true\" }}");
    let _ = tmux::run(&["set-hook", "-p", "-t", fp, "window-layout-changed", &hook]);
}

/// Git 按钮：目录在仓库里就照旧交给 float-tool.sh；不在（比如 ~/projects）就看下面两层的仓库：
/// 只有一个直接开它，好几个先列出来点一个（名字、分支、有改动标 ●），一个都没有就只说一句。返回要闪的话。
pub fn git_button(caller: &str, cfg: &Config) -> Option<String> {
    let cwd = live_dir(&tmux::display(caller, "#{pane_current_path}").unwrap_or_default());
    if git_root(&cwd).is_some() {
        spawn(&cfg.scripts.join("float-tool.sh"), &[caller, "git"]);
        return None;
    }
    let repos = find_repos(std::path::Path::new(&cwd), 2);
    match repos.as_slice() {
        [] => Some(crate::i18n::t("不在 git 仓库里，下面两层也没有仓库")),
        [one] => {
            float_tool(caller, "tool:git", &cwd, &format!("exec lazygit -p {}", sh_quote(&one.to_string_lossy())));
            None
        }
        _ => {
            let _ = std::fs::create_dir_all(crate::run_dir());
            let file = crate::run_dir().join(format!("{caller}.repos"));
            let list: Vec<String> = repos.iter().map(|r| r.to_string_lossy().into_owned()).collect();
            let _ = std::fs::write(&file, list.join("\n"));
            let cmd = format!("exec {} pickrepo {}", sh_quote(&crate::exe().to_string_lossy()), sh_quote(&file.to_string_lossy()));
            float_tool(caller, "tool:git", &cwd, &cmd);
            None
        }
    }
}

/// 浮窗里跑的：列出仓库，点一个就在那里开 lazygit。
pub fn cmd_pickrepo(file: &str) {
    crate::picker::cmd_pickrepo(file);
}

/// 后台跑一个脚本，不等它
pub fn spawn(prog: &std::path::Path, args: &[&str]) {
    use std::process::Stdio;
    if let Ok(mut c) = std::process::Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || c.wait()); // 收尸：轨迹常驻，不收会留僵尸进程
    }
}
