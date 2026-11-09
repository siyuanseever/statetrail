//! 看 Claude 的屏幕认它现在的状态：在干活 / 等你确认 / 闲着。
//!
//! 规则（detect/claude.toml）和匹配办法照搬 herdr 的 src/detect（manifest.rs 里的 region、compiled_gate_matches 等），
//! 出处 https://github.com/herdrdev/herdr ，Apache License 2.0，Copyright herdr contributors。
//! 只搬了 Claude 规则用得到的区域；codex 专用的几个（› 提示符那一套）没搬，遇到返回空。
//!
//! 规则：每条有一个区域（屏幕的哪一块）和一组条件，命中的里面优先级最高的那条说了算；一条都没中就是闲着。
//! 条件：contains 不分大小写、全部都要有；regex 对整块；line_regex 要有一行对得上；all 全中、any 中一个、not 一个都不能中。

use regex::Regex;
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Blocked,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub state: State,
    /// 命中的规则说「这一屏不算数」（比如开着对话记录查看器）：沿用上一次的状态
    pub skip: bool,
    pub rule: &'static str,
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    rules: Vec<Rule>,
}

#[derive(Deserialize)]
struct Rule {
    id: String,
    state: Option<String>,
    #[serde(default)]
    priority: i32,
    #[serde(default = "whole_recent")]
    region: String,
    #[serde(default)]
    skip_state_update: bool,
    #[serde(flatten)]
    gate: Gate,
}

fn whole_recent() -> String {
    "whole_recent".into()
}

#[derive(Deserialize, Default)]
struct Gate {
    #[serde(default)]
    all: Vec<Gate>,
    #[serde(default)]
    any: Vec<Gate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<Gate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

struct CGate {
    all: Vec<CGate>,
    any: Vec<CGate>,
    not_gate: Vec<CGate>,
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

struct CRule {
    id: &'static str,
    state: State,
    priority: i32,
    region: String,
    skip: bool,
    gate: CGate,
}

fn compile(g: &Gate) -> CGate {
    let re = |v: &[String]| v.iter().filter_map(|p| Regex::new(p).ok()).collect();
    CGate {
        all: g.all.iter().map(compile).collect(),
        any: g.any.iter().map(compile).collect(),
        not_gate: g.not_gate.iter().map(compile).collect(),
        contains: g.contains.iter().map(|s| s.to_lowercase()).collect(),
        regex: re(&g.regex),
        line_regex: re(&g.line_regex),
    }
}

fn rules() -> &'static [CRule] {
    static R: OnceLock<Vec<CRule>> = OnceLock::new();
    R.get_or_init(|| {
        let m: Manifest = toml::from_str(include_str!("detect/claude.toml")).unwrap_or(Manifest { rules: Vec::new() });
        m.rules
            .into_iter()
            .map(|r| CRule {
                state: match r.state.as_deref() {
                    Some("idle") => State::Idle,
                    Some("working") => State::Working,
                    Some("blocked") => State::Blocked,
                    _ => State::Unknown,
                },
                id: Box::leak(r.id.into_boxed_str()),
                priority: r.priority,
                region: r.region,
                skip: r.skip_state_update,
                gate: compile(&r.gate),
            })
            .collect()
    })
}

/// screen：capture-pane 拿到的可见屏幕（纯文字）；title：窗格标题（Claude 用 OSC 设的终端标题）。
pub fn detect(screen: &str, title: &str) -> Detection {
    let mut best: Option<&CRule> = None;
    for r in rules() {
        let text = region(screen, title, &r.region);
        if !matches(&r.gate, text, &text.to_lowercase()) {
            continue;
        }
        if best.is_none_or(|b| r.priority > b.priority) {
            best = Some(r);
        }
    }
    match best {
        Some(r) => Detection { state: r.state, skip: r.skip, rule: r.id },
        None => Detection { state: State::Idle, skip: false, rule: "" },
    }
}

fn matches(g: &CGate, text: &str, lower: &str) -> bool {
    g.contains.iter().all(|n| lower.contains(n.as_str()))
        && g.regex.iter().all(|r| r.is_match(text))
        && g.line_regex.iter().all(|r| text.lines().any(|l| r.is_match(l)))
        && g.all.iter().all(|n| matches(n, text, lower))
        && (g.any.is_empty() || g.any.iter().any(|n| matches(n, text, lower)))
        && !g.not_gate.iter().any(|n| matches(n, text, lower))
}

fn region<'a>(screen: &'a str, title: &'a str, spec: &str) -> &'a str {
    let spec = spec.trim();
    match spec {
        "osc_title" => title,
        "osc_progress" => "", // tmux 不转交 OSC 9;4 进度
        "whole_recent" => screen,
        "prompt_box_body" => prompt_box_body(screen).unwrap_or(""),
        "above_prompt_box" => above_prompt_box(screen),
        "last_non_empty_above_prompt_box" => last_non_empty_line(above_prompt_box(screen)),
        "after_last_horizontal_rule" => after_last_horizontal_rule(screen),
        _ => {
            if let Some(n) = region_count(spec, "bottom_lines") {
                return bottom_lines(screen, n);
            }
            if let Some(n) = region_count(spec, "bottom_non_empty_lines") {
                return bottom_non_empty_lines(screen, n);
            }
            ""
        }
    }
}

fn region_count(spec: &str, name: &str) -> Option<usize> {
    spec.strip_prefix(name)?.strip_prefix('(')?.strip_suffix(')')?.parse().ok()
}

fn bottom_lines(c: &str, n: usize) -> &str {
    let lines: Vec<&str> = c.lines().collect();
    slice_from(c, &lines, lines.len().saturating_sub(n))
}

fn bottom_non_empty_lines(c: &str, n: usize) -> &str {
    let lines: Vec<&str> = c.lines().collect();
    let Some(start) = lines.iter().enumerate().rev().filter(|(_, l)| !l.trim().is_empty()).take(n).last().map(|(i, _)| i) else {
        return "";
    };
    slice_from(c, &lines, start)
}

fn prompt_box_body(c: &str) -> Option<&str> {
    let lines: Vec<&str> = c.lines().collect();
    let top = prompt_box_top(&lines)?;
    let start = line_start(c, &lines, top + 1);
    let end_i = lines[top + 1..].iter().position(|l| is_rule(l)).map(|r| top + 1 + r).unwrap_or(lines.len());
    let end = line_start(c, &lines, end_i);
    Some(&c[start.min(c.len())..end.min(c.len())])
}

fn above_prompt_box(c: &str) -> &str {
    let lines: Vec<&str> = c.lines().collect();
    let Some(top) = prompt_box_top(&lines) else { return c };
    &c[..line_start(c, &lines, top).min(c.len())]
}

fn after_last_horizontal_rule(c: &str) -> &str {
    let (mut last, mut off) = (0usize, 0usize);
    for l in c.lines() {
        let next = off + l.len() + 1;
        if is_rule(l) {
            last = next.min(c.len());
        }
        off = next;
    }
    &c[last..]
}

fn last_non_empty_line(c: &str) -> &str {
    c.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("")
}

/// 输入框的上边框：从下往上数第二条横线。
fn prompt_box_top(lines: &[&str]) -> Option<usize> {
    let mut n = 0;
    for i in (0..lines.len()).rev() {
        if is_rule(lines[i]) {
            n += 1;
            if n == 2 {
                return Some(i);
            }
        }
    }
    None
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    let k = t.chars().take_while(|&ch| ch == '─').count();
    if k == 0 {
        return false;
    }
    let bytes = t.char_indices().nth(k).map(|(i, _)| i).unwrap_or(t.len());
    t[bytes..].trim_start().is_empty() || k >= 3
}

fn slice_from<'a>(c: &'a str, lines: &[&str], i: usize) -> &'a str {
    &c[line_start(c, lines, i).min(c.len())..]
}

fn line_start(c: &str, lines: &[&str], i: usize) -> usize {
    lines[..i.min(lines.len())].iter().map(|l| l.len() + 1).sum::<usize>().min(c.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "────────────────────────────────────────";

    #[test]
    fn all_rules_load() {
        assert!(rules().len() >= 15, "{}", rules().len());
        // 编不过的正则会被跳过、条件变松：这里要求一条都不许丢
        fn check(g: &Gate) {
            for p in g.regex.iter().chain(&g.line_regex) {
                Regex::new(p).unwrap_or_else(|e| panic!("{p}: {e}"));
            }
            g.all.iter().chain(&g.any).chain(&g.not_gate).for_each(check);
        }
        let m: Manifest = toml::from_str(include_str!("detect/claude.toml")).unwrap();
        m.rules.iter().for_each(|r| check(&r.gate));
    }

    #[test]
    fn idle_prompt_box() {
        let s = format!("⏺ done.\n\n{RULE}\n❯ \n{RULE}\n  ⏵⏵ auto mode on (shift+tab to cycle)\n");
        let d = detect(&s, "✳ title");
        assert_eq!(d.state, State::Idle, "{}", d.rule);
    }

    #[test]
    fn spinner_title_is_working() {
        assert_eq!(detect("", "◐ fixing").state, State::Working);
    }

    #[test]
    fn interrupt_footer_is_working() {
        let s = format!("✻ Thinking… (12s · ↓ 1.2k tokens)\n\n{RULE}\n❯ \n{RULE}\n  ⏵⏵ auto mode on · esc to interrupt\n");
        assert_eq!(detect(&s, "✳ x").state, State::Working);
    }

    #[test]
    fn bash_permission_is_blocked() {
        let s = format!(
            "{RULE}\n Bash command\n\n   rm -rf build\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and don't ask again\n   3. No\n\n Esc to cancel · Tab to amend\n"
        );
        assert_eq!(detect(&s, "✳ x").state, State::Blocked);
    }

    #[test]
    fn background_agents_is_working() {
        let s = format!("⏺ ok\n\n✻ Waiting for 2 background agents to finish\n\n{RULE}\n❯ \n{RULE}\n");
        assert_eq!(detect(&s, "✳ x").state, State::Working);
    }
}
