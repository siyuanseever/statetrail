//! 轨迹分章：一件事（一个需求、一个问题）一章，章下面是轮次（用户 2026-10-28：「不同的任务需求起一个一级标题，对话轮次是二级的」）。
//! 哪一轮开新章：Haiku 读你说的话判断（auto），或者你在一轮上右键手动标（manual 盖过 auto）。
//! 记在 <状态目录>/chapters/<会话 id>.json，按轮次 id 记：同一个会话的几个轨迹共用，重启不丢。
//! 为什么要模型：Claude 自己的标题一个会话只有一个；压缩边界、两句话隔多久都和换不换任务对不上（RUST-DESIGN「分章」一节有实测）。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::transcript::Stop;

pub const BATCH: usize = 80; // 一次最多判断多少句（老会话第一次打开时分几批补）
const CONTEXT: usize = 6; // 带上前面已经分过的几句，模型才知道上一章在干什么
const SAY_MAX: usize = 300; // 每句话最多送多少个字
const TITLE_MAX: usize = 24; // 没有模型起的标题时，用这一轮你说的话的开头
const LOCK_STALE: Duration = Duration::from_secs(120);

#[derive(Default, Serialize, Deserialize)]
pub struct Marks {
    #[serde(default)]
    pub auto: HashMap<String, String>, // 轮次 id → Haiku 起的章名（这一轮开新章）
    #[serde(default)]
    pub upto: Option<String>, // Haiku 判断到哪一轮了（这一轮和它以前都判过）
    #[serde(default)]
    pub manual: HashMap<String, bool>, // 轮次 id → 手动：true 从这一轮开新章，false 并回上一章
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub start: usize, // visible() 里的下标
    pub end: usize,   // 不含
    pub title: String,
}

fn dir() -> PathBuf {
    crate::state_dir().join("chapters")
}

fn path_of(sid: &str) -> PathBuf {
    dir().join(format!("{sid}.json"))
}

fn oneline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Marks {
    pub fn load(sid: &str) -> Self {
        std::fs::read(path_of(sid))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// 写临时文件再换名：另一个轨迹同时在读也读不到半截
    pub fn save(&self, sid: &str) {
        let _ = std::fs::create_dir_all(dir());
        let tmp = dir().join(format!(".{sid}.{}.tmp", std::process::id()));
        if let Ok(b) = serde_json::to_vec_pretty(self)
            && std::fs::write(&tmp, b).is_ok()
        {
            let _ = std::fs::rename(&tmp, path_of(sid));
        }
    }

    fn starts_at(&self, id: &str) -> bool {
        match self.manual.get(id) {
            Some(&m) => m,
            None => self.auto.contains_key(id),
        }
    }

    /// 切成章。一章都没标过就返回空：轨迹照旧平铺。第一轮总是第一章的开头。
    pub fn chapters(&self, stops: &[&Stop]) -> Vec<Chapter> {
        if stops.is_empty() || !stops.iter().skip(1).any(|s| self.starts_at(&s.id)) && self.auto.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<Chapter> = Vec::new();
        for (i, s) in stops.iter().enumerate() {
            if i == 0 || self.starts_at(&s.id) {
                if let Some(last) = out.last_mut() {
                    last.end = i;
                }
                let title = self
                    .auto
                    .get(&s.id)
                    .cloned()
                    .unwrap_or_else(|| oneline(&s.prompt).chars().take(TITLE_MAX).collect());
                out.push(Chapter { start: i, end: stops.len(), title });
            }
        }
        out
    }

    /// 还没让 Haiku 判断过的、你说的话（下标）。
    pub fn pending(&self, stops: &[&Stop]) -> Vec<usize> {
        let from = self
            .upto
            .as_deref()
            .and_then(|u| stops.iter().position(|s| s.id == u))
            .map_or(0, |k| k + 1);
        (from..stops.len()).filter(|&i| stops[i].kind == "human").collect()
    }
}

/// 只让一个轨迹在调 Haiku：一下开了 20 多个轨迹也不会同时起 20 多个 claude。锁文件放久了（进程被杀）就当没锁。
pub struct Lock(PathBuf);

impl Lock {
    pub fn take() -> Option<Lock> {
        let _ = std::fs::create_dir_all(dir());
        let p = dir().join(".lock");
        if std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().unwrap_or_default() > LOCK_STALE)
        {
            let _ = std::fs::remove_file(&p);
        }
        std::fs::OpenOptions::new().write(true).create_new(true).open(&p).ok().map(|_| Lock(p))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const SYSTEM: &str = "你把一个人和编程助手的对话按「任务」分章。输入是他按顺序说的话，每句前面是轮号。\
新任务 = 换了一件要做的事（新需求、新问题、新功能、另一个 bug）；同一件事的追问、确认、补充、纠正、「好」「继续」「发」都不是。\
只判断「要判断的」那一段；有「前面已经分好的」时，它的最后一章可能延续下来，第一句不一定是新章；没有时第一句一定是新章。\
每个新章给出起点轮号和章名：4～12 个字的名词短语，说做的是什么，用他说话的语言。";

const SCHEMA: &str = r#"{"type":"object","properties":{"chapters":{"type":"array","items":{"type":"object","properties":{"start":{"type":"integer"},"title":{"type":"string"}},"required":["start","title"]}}},"required":["chapters"]}"#;

/// 拼给 Haiku 的输入：前面已经分好的几句（带上一章的章名）+ 要判断的。
pub fn request(marks: &Marks, stops: &[&Stop], new: &[usize]) -> String {
    let say = |i: usize| format!("{}. {}", i + 1, oneline(&stops[i].prompt).chars().take(SAY_MAX).collect::<String>());
    let mut out = String::new();
    let first = new.first().copied().unwrap_or(0);
    let before: Vec<usize> = (0..first).filter(|&i| stops[i].kind == "human").collect();
    if !before.is_empty() {
        let ch = marks.chapters(&stops[..first]);
        out += "前面已经分好的";
        if let Some(c) = ch.last() {
            out += &format!("（最后一章「{}」从第 {} 轮开始）", c.title, c.start + 1);
        }
        out += "：\n";
        for &i in &before[before.len().saturating_sub(CONTEXT)..] {
            out += &say(i);
            out += "\n";
        }
        out += "\n";
    }
    out += "要判断的：\n";
    for &i in new {
        out += &say(i);
        out += "\n";
    }
    out
}

/// 从 claude -p --output-format json 的输出里取出 [(轮号, 章名)]，只留要判断的那几轮。
pub fn parse(out: &[u8], new: &[usize]) -> Result<Vec<(usize, String)>, String> {
    let v: Value = serde_json::from_slice(out).map_err(|e| format!("not json: {e}"))?;
    if v["is_error"].as_bool() == Some(true) {
        return Err(v["result"].as_str().unwrap_or("error").chars().take(200).collect());
    }
    let list = v["structured_output"]["chapters"].as_array().ok_or("no structured_output")?;
    Ok(list
        .iter()
        .filter_map(|c| {
            let n = c["start"].as_u64()? as usize;
            let title = oneline(c["title"].as_str()?);
            (n > 0 && new.contains(&(n - 1)) && !title.is_empty()).then_some((n - 1, title))
        })
        .collect())
}

/// 调一次 Haiku（阻塞，几秒到十几秒，放线程里跑）。用这个会话自己的账号；不留会话记录、不带工具、不读设置（不跑 hook）。
pub fn ask(cfg_dir: &Path, input: &str) -> Result<Vec<u8>, String> {
    let _ = std::fs::create_dir_all(dir());
    let mut cmd = Command::new("claude");
    cmd.args(["-p", "--model", "haiku", "--no-session-persistence", "--restricted", "--tools", "", "--strict-mcp-config"])
        .args(["--system-prompt", SYSTEM, "--output-format", "json", "--json-schema", SCHEMA])
        .current_dir(dir()) // 不在任何项目里：不读项目的 CLAUDE.md
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if crate::home().join(".claude") != cfg_dir {
        cmd.env("CLAUDE_CONFIG_DIR", cfg_dir); // 默认账号不设：设了钥匙串里的登录项名字就变了
    }
    let mut child = cmd.spawn().map_err(|e| format!("claude: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() && out.stdout.is_empty() {
        return Err(String::from_utf8_lossy(&out.stderr).chars().take(200).collect());
    }
    Ok(out.stdout)
}

/// 把这一批的结果并进文件（重新读一遍再写：同一个会话可能有两个轨迹）。判过的轮次里手动标过的不动。
pub fn merge(sid: &str, found: &[(usize, String)], ids: &[(usize, String)]) {
    let mut m = Marks::load(sid);
    for (i, title) in found {
        if let Some((_, id)) = ids.iter().find(|(k, _)| k == i) {
            m.auto.insert(id.clone(), title.clone());
        }
    }
    if let Some((_, id)) = ids.last() {
        m.upto = Some(id.clone());
    }
    m.save(sid);
}

/// 记一行日志：什么时候、哪个会话、判了几句、开了几章、花了多久 / 出了什么错
pub fn log(sid: &str, n: usize, result: &Result<Vec<(usize, String)>, String>, secs: f64) {
    let line = json!({"at": chrono::Local::now().format("%F %T").to_string(), "sid": sid, "asked": n,
        "found": result.as_ref().map(Vec::len).ok(), "error": result.as_ref().err(), "secs": (secs * 10.0).round() / 10.0});
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir().join("chapters.log")) {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(kind: &'static str, id: &str, prompt: &str) -> Stop {
        Stop {
            kind,
            prompt: prompt.into(),
            ts: None,
            id: id.into(),
            reply: String::new(),
            has_text: true,
            compacted: false,
        }
    }

    #[test]
    fn chapters_follow_marks() {
        let s = [stop("human", "a", "做 A"), stop("human", "b", "好"), stop("notify", "c", "x"), stop("human", "d", "做 D 这件事")];
        let st: Vec<&Stop> = s.iter().collect();
        let mut m = Marks::default();
        assert!(m.chapters(&st).is_empty()); // 没标过：平铺
        m.auto.insert("a".into(), "甲".into());
        m.auto.insert("d".into(), "丁".into());
        let ch = m.chapters(&st);
        assert_eq!(ch.iter().map(|c| (c.start, c.end, c.title.as_str())).collect::<Vec<_>>(), [(0, 3, "甲"), (3, 4, "丁")]);
        m.manual.insert("d".into(), false); // 手动并回上一章
        m.manual.insert("b".into(), true); // 手动开新章：没有章名就用那句话
        let ch = m.chapters(&st);
        assert_eq!(ch.iter().map(|c| (c.start, c.end, c.title.as_str())).collect::<Vec<_>>(), [(0, 1, "甲"), (1, 4, "好")]);
    }

    #[test]
    fn pending_and_request() {
        let s = [stop("human", "a", "做 A"), stop("notify", "b", "x"), stop("human", "c", "做 C\n第二行")];
        let st: Vec<&Stop> = s.iter().collect();
        let mut m = Marks::default();
        assert_eq!(m.pending(&st), [0, 2]);
        assert_eq!(request(&m, &st, &[0, 2]), "要判断的：\n1. 做 A\n3. 做 C 第二行\n");
        m.upto = Some("a".into());
        m.auto.insert("a".into(), "甲".into());
        assert_eq!(m.pending(&st), [2]);
        assert_eq!(
            request(&m, &st, &[2]),
            "前面已经分好的（最后一章「甲」从第 1 轮开始）：\n1. 做 A\n\n要判断的：\n3. 做 C 第二行\n"
        );
    }

    #[test]
    fn parse_keeps_asked_turns() {
        let out = r#"{"is_error":false,"structured_output":{"chapters":[{"start":1,"title":"old"},{"start":3,"title":" 新 事 "}]}}"#;
        assert_eq!(parse(out.as_bytes(), &[2]), Ok(vec![(2, "新 事".into())]));
        assert!(parse(br#"{"is_error":true,"result":"rate limited"}"#, &[2]).is_err());
    }
}
