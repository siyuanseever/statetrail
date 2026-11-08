//! 弹出来选一项的扁平列表：Git 按钮的选仓库（`statetrail pickrepo`）。
//! Python 版用 fzf 做；这里自己画，样子照它：最上面一行灰字说明，下面一行一项，选中那一行 ▌ 加底色。
//! 不画树、不画框。点一下或回车就选，Esc 不选。

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::{execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::i18n::t;
use crate::render::{self, SLine};
use crate::theme::colors;
use crate::ui::to_line;

pub struct Row {
    pub key: String,
    pub line: SLine,
}

/// 选中了返回那一项的 key；filter = true 时打字筛选（插件菜单），false 时 j / k / q 也能用（选仓库）。
pub fn pick(note: &str, rows: &[Row], filter: bool) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let _ = event::poll(std::time::Duration::ZERO); // 先装上窗口大小变化的信号处理（见 pane::run）
    let mut out = std::io::stdout();
    let _ = terminal::enable_raw_mode();
    let _ = execute!(out, terminal::EnterAlternateScreen, event::EnableMouseCapture);
    let mut term = Terminal::new(CrosstermBackend::new(out)).ok()?;
    let picked = run(&mut term, note, rows, filter);
    let _ = execute!(term.backend_mut(), event::DisableMouseCapture, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    picked
}

fn run(term: &mut Terminal<CrosstermBackend<std::io::Stdout>>, note: &str, rows: &[Row], filter: bool) -> Option<String> {
    let c = colors();
    let mut query = String::new();
    let (mut sel, mut offset) = (0usize, 0usize);
    let top: u16 = if filter { 2 } else { 1 };
    loop {
        let q = query.to_lowercase();
        let shown: Vec<&Row> = rows
            .iter()
            .filter(|r| q.is_empty() || render::plain(&r.line).to_lowercase().contains(&q))
            .collect();
        sel = sel.min(shown.len().saturating_sub(1));
        let size = term.size().ok()?;
        let room = size.height.saturating_sub(top) as usize;
        if sel < offset {
            offset = sel;
        } else if room > 0 && sel >= offset + room {
            offset = sel + 1 - room;
        }
        term.draw(|f| {
            let area = f.area();
            let mut head = Vec::new();
            if filter {
                head.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(c.accent)),
                    Span::raw(query.clone()),
                ]));
            }
            head.push(Line::from(Span::styled(format!("  {note}"), Style::default().fg(c.grey))));
            f.render_widget(
                Paragraph::new(head),
                Rect {
                    height: top.min(area.height),
                    ..area
                },
            );
            let bg = c.selection.map_or_else(Style::default, |b| Style::default().bg(b));
            let lines: Vec<Line> = shown
                .iter()
                .enumerate()
                .skip(offset)
                .take(room)
                .map(|(i, r)| {
                    let mut l = to_line(&render::clip_line(r.line.clone(), (area.width as usize).saturating_sub(2)));
                    if i == sel {
                        l.spans.insert(0, Span::styled("▌ ", Style::default().fg(c.accent)));
                        l = l.style(bg);
                    } else {
                        l.spans.insert(0, Span::raw("  "));
                    }
                    l
                })
                .collect();
            f.render_widget(
                Paragraph::new(lines),
                Rect {
                    y: area.y + top,
                    height: area.height.saturating_sub(top),
                    ..area
                },
            );
        })
        .ok()?;
        match event::read().ok()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Esc => return None,
                    KeyCode::Char('c') if ctrl => return None,
                    KeyCode::Enter => return shown.get(sel).map(|r| r.key.clone()),
                    KeyCode::Up => sel = sel.saturating_sub(1),
                    KeyCode::Down => sel += 1,
                    KeyCode::Char('p') if ctrl => sel = sel.saturating_sub(1),
                    KeyCode::Char('n') if ctrl => sel += 1,
                    KeyCode::Backspace if filter => {
                        query.pop();
                    }
                    KeyCode::Char(ch) if filter && !ctrl => query.push(ch),
                    KeyCode::Char('q') => return None,
                    KeyCode::Char('k') => sel = sel.saturating_sub(1),
                    KeyCode::Char('j') => sel += 1,
                    _ => {}
                }
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => sel = sel.saturating_sub(1),
                MouseEventKind::ScrollDown => sel += 1,
                MouseEventKind::Down(MouseButton::Left) if m.row >= top => {
                    let i = offset + (m.row - top) as usize;
                    if let Some(r) = shown.get(i) {
                        return Some(r.key.clone());
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// Git 按钮在不是仓库的目录里：一行一个仓库（名字、当前分支，有没提交的改动就加个 ●），点一下或回车就用 lazygit
/// 打开它，Esc 退出。扁平的一列，不画树、不画框（用户要的，不要 VS Code 那种展开树）。
pub fn cmd_pickrepo(file: &str) {
    let repos: Vec<String> = std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    let git = |r: &str, args: &[&str]| -> String {
        std::process::Command::new("git")
            .arg("-C")
            .arg(r)
            .args(args)
            .stderr(std::process::Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let infos: Vec<(String, bool)> = std::thread::scope(|sc| {
        let hs: Vec<_> = repos
            .iter()
            .map(|r| {
                sc.spawn(move || {
                    let mut b = git(r, &["branch", "--show-current"]);
                    if b.is_empty() {
                        b = git(r, &["rev-parse", "--short", "HEAD"]);
                    }
                    (b, !git(r, &["status", "--porcelain"]).is_empty())
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    });
    let base = std::env::current_dir().unwrap_or_default();
    let names: Vec<String> = repos
        .iter()
        .map(|r| {
            std::path::Path::new(r)
                .strip_prefix(&base)
                .map_or_else(|_| r.clone(), |p| p.to_string_lossy().into_owned())
        })
        .collect();
    let w = names
        .iter()
        .map(|n| unicode_width::UnicodeWidthStr::width(n.as_str()))
        .max()
        .unwrap_or(0);
    let c = colors();
    let rows: Vec<Row> = repos
        .iter()
        .zip(&names)
        .zip(&infos)
        .map(|((r, n), (b, dirty))| {
            let pad = w - unicode_width::UnicodeWidthStr::width(n.as_str());
            let mut line: SLine = vec![(format!("{n}{}  ", " ".repeat(pad)), None), (b.clone(), Some(c.grey))];
            if *dirty {
                line.push(("  ●".into(), Some(c.warn)));
            }
            Row { key: r.clone(), line }
        })
        .collect();
    if let Some(r) = pick(&t("选一个仓库，Esc 退出"), &rows, false) {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new("lazygit").args(["-p", &r]).exec();
        eprintln!("statetrail: lazygit: {err}");
    }
}
