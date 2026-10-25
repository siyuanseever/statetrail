//! 让 Claude 自己的屏幕滚到某一轮，那一轮的最终回复停在屏幕上方。逐行照搬 Python 版 core/jump.py：
//! 从近的那一头开始（Ctrl+Home / Ctrl+End），往那一轮翻页（PageUp / PageDown 正好半屏），每翻一下读一次屏，
//! 直到那一轮回复的开头出现，再把它推到屏幕顶部附近。你自己的话（❯ 开头的行）不算。

use std::thread::sleep;
use std::time::{Duration, Instant};

const REDRAW_WAIT: Duration = Duration::from_millis(15); // 翻一次之后先等多久再读屏
const JUMP_BUDGET: Duration = Duration::from_secs(6); // 一次跳转最多翻这么久
const MAX_ROLLS: usize = 400;
const AT_BOTTOM: &str = "Jump to bottom"; // Claude 往上翻了才在底部显示这句
const PROMPT: &str = "❯";

pub trait Term {
    fn screen(&self) -> String;
    fn send(&self, keys: &[&str]);
    fn height(&self) -> usize;
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Found,
    Cancelled,
    Timeout,
    Gone(Option<usize>), // 翻到头也没有：主界面里只剩第 k 轮以后的
}

/// 去掉空白和 markdown 记号字符再比（屏幕上的行和针都过一遍）。
pub fn norm(text: &str) -> String {
    text.chars()
        .filter(|c| !(c.is_whitespace() || matches!(c, '*' | '`' | '_' | '~' | '#' | '>' | '|' | '\\')))
        .collect()
}

/// [text](url) → text
fn unlink(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        if let Some(close) = after.find(']')
            && after[close + 1..].starts_with('(')
            && let Some(end) = after[close + 2..].find(')')
        {
            out.push_str(&rest[..open]);
            out.push_str(&after[..close]);
            rest = &after[close + 2 + end + 1..];
            continue;
        }
        out.push_str(&rest[..open + 1]);
        rest = after;
    }
    out.push_str(rest);
    out
}

/// 一轮回复开头的一小段（规范化过）：第一行的前 12 个字。
pub fn needle(reply: &str) -> String {
    for line in reply.lines() {
        let n = norm(&unlink(line));
        if !n.is_empty() {
            return n.chars().take(12).collect();
        }
    }
    String::new()
}

/// 每轮一根针；整个会话里不唯一的置空，免得把差着很多轮的两块屏幕认成同一处。
pub fn needles_of(replies: &[&str]) -> Vec<String> {
    let needles: Vec<String> = replies.iter().map(|r| needle(r)).collect();
    needles
        .iter()
        .map(|n| {
            if needles.iter().filter(|m| *m == n).count() > 1 {
                String::new()
            } else {
                n.clone()
            }
        })
        .collect()
}

fn rows_of(text: &str) -> Vec<String> {
    text.lines().filter(|r| !r.trim_start().starts_with(PROMPT)).map(norm).collect()
}

/// 屏幕上现在有哪几轮（下标升序）。
pub fn locate(screen: &str, needles: &[String]) -> Vec<usize> {
    let rows = rows_of(screen);
    (0..needles.len())
        .filter(|&i| !needles[i].is_empty() && rows.iter().any(|r| r.contains(&needles[i])))
        .collect()
}

pub fn jump(term: &dyn Term, replies: &[&str], idx: usize, cancelled: &dyn Fn() -> bool) -> Outcome {
    let target = needle(replies[idx]);
    let needles = needles_of(replies);
    let height = term.height();
    let pages = if height >= 30 { 2 } else { 1 };
    let from_top = (idx as f64) < replies.len() as f64 / 2.0;
    let deadline = Instant::now() + JUMP_BUDGET;

    let needle_row = |text: &str| -> Option<usize> {
        text.lines()
            .position(|row| norm(row).contains(&target) && !row.trim_start().starts_with(PROMPT))
    };
    // 发键、等屏幕不再变再返回；什么都没动（到头了）返回 None。等的是「不再变」，不是「变了」：
    // 一次发很多个翻页时 Claude 会先画一次、再接着消化剩下的按键
    let press = |before: &str, keys: &[&str]| -> Option<String> {
        term.send(keys);
        sleep(REDRAW_WAIT);
        let (mut now, mut last) = (term.screen(), None::<String>);
        for _ in 0..60 {
            if now != before && last.as_deref() == Some(now.as_str()) {
                return Some(now);
            }
            last = Some(now);
            sleep(Duration::from_millis(10));
            now = term.screen();
        }
        if now == before { None } else { Some(now) }
    };
    let where_ = |text: &str| -> Option<usize> {
        let rows = rows_of(text);
        (0..needles.len())
            .filter(|&i| !needles[i].is_empty() && rows.iter().any(|r| r.contains(&needles[i])))
            .next_back()
    };
    let lift = |mut now: String| {
        let down = format!("\x1b[<65;10;{}M", height / 2);
        for _ in 0..10 {
            let Some(r) = needle_row(&now) else { return };
            if r <= 4 || !now.contains(AT_BOTTOM) {
                return; // 已经在最底：往下推不动
            }
            let next = if r - 4 >= height / 2 {
                press(&now, &["PageDown"])
            } else {
                let keys: Vec<&str> = std::iter::repeat_n(down.as_str(), (r - 4).min(2)).collect();
                press(&now, &keys)
            };
            match next {
                Some(n) => now = n,
                None => return,
            }
        }
    };
    // Some(true) 找到 / 被取消，Some(false) 翻到头没有，None 超时
    let search = |nudge: bool| -> Option<bool> {
        let mut now = term.screen();
        if from_top || now.contains(AT_BOTTOM) {
            now = press(&now, &[if from_top { "C-Home" } else { "C-End" }]).unwrap_or_else(|| term.screen());
        }
        let (mut last_turn, mut since) = (None::<usize>, 0usize);
        let (mut prev_down, mut fine) = (from_top, false);
        for _ in 0..MAX_ROLLS {
            if needle_row(&now).is_some() {
                if nudge {
                    lift(now);
                }
                return Some(true);
            }
            if cancelled() {
                return Some(true);
            }
            if Instant::now() > deadline {
                return None;
            }
            // 认出停在第几轮，就用刚走过的那一段算「一轮几个半屏」，把剩下的路按 60% 一次翻掉；方向只准掉头一次
            let cur = where_(&now);
            let (mut steps, mut down) = (pages, prev_down);
            if let (Some(cur), false) = (cur, fine) {
                down = cur < idx;
                if down != prev_down {
                    fine = true;
                } else if let Some(last) = last_turn
                    && cur != last
                    && since > 0
                {
                    let rate = since as f64 / (cur as f64 - last as f64).abs();
                    steps = pages.max(((rate * (idx as f64 - cur as f64).abs() * 0.6) as usize).min(MAX_ROLLS));
                }
                last_turn = Some(cur);
                since = 0;
            }
            prev_down = down;
            let keys: Vec<&str> = std::iter::repeat_n(if down { "PageDown" } else { "PageUp" }, steps).collect();
            match press(&now, &keys) {
                Some(n) => now = n,
                None => return Some(false),
            }
            since += steps;
        }
        Some(false)
    };

    for attempt in 0..2 {
        let found = search(attempt == 0);
        let Some(found) = found else { return Outcome::Timeout };
        if cancelled() {
            return Outcome::Cancelled;
        }
        if found && needle_row(&term.screen()).is_some() {
            return Outcome::Found;
        }
    }
    term.send(&["C-Home"]);
    sleep(Duration::from_millis(400));
    let top = locate(&term.screen(), &needles);
    term.send(&["C-End"]);
    Outcome::Gone(top.first().copied().filter(|&k| k > idx))
}
