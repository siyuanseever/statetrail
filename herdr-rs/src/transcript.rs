//! Claude Code 对话记录（<配置目录>/projects/*/<会话 id>.jsonl）的增量读取、切轮次。
//! 和 Python 版 core/transcript.py 的规则一样（两边对同一份记录切出来的轮次一致），做法不同：
//! 逐行读，每行只解出需要的几个字段，其余部分（工具输出、图片……）跳过不建对象，所以内存只跟轮数有关。

use serde::Deserialize;
use serde::de::{self, Deserializer, IgnoredAny, SeqAccess, Visitor};
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::i18n::t;

const NOISE_PREFIXES: [&str; 4] = [
    "<local-command",
    "<command-name>",
    "<command-message>",
    "This session is being continued from a previous conversation",
];
pub const PROMPT_MAX: usize = 4000; // 展开时最多显示你那句话的多少个字
const REPLY_MAX: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    pub kind: &'static str, // human / notify / peer
    pub prompt: String,
    pub ts: Option<String>,
    pub id: String,
    pub reply: String,
    pub has_text: bool,
    pub compacted: bool,
}

#[derive(Deserialize)]
struct Raw<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    subtype: Option<Cow<'a, str>>,
    #[serde(rename = "isSidechain", default)]
    sidechain: Option<bool>,
    #[serde(rename = "isMeta", default)]
    meta: Option<bool>,
    #[serde(borrow, default)]
    origin: Option<Origin<'a>>,
    #[serde(borrow, default)]
    message: Option<Msg<'a>>,
    #[serde(borrow, default)]
    uuid: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(rename = "aiTitle", borrow, default)]
    ai_title: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct Origin<'a> {
    #[serde(borrow, default)]
    kind: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct Msg<'a> {
    #[serde(borrow, default)]
    content: Option<Content<'a>>,
}

#[derive(Deserialize)]
struct Block<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
}

/// message.content：一个字符串，或者一串块（只看块的 type 和 text）。手写反序列化，
/// 不用 untagged 枚举——那样 serde 会先把整个值（几 MB 的工具输出）缓存一遍再试。
enum Content<'a> {
    Text(Cow<'a, str>),
    Blocks { text: String, tool_result: bool },
    Other,
}

impl<'de: 'a, 'a> Deserialize<'de> for Content<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = Content<'a>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string or a list of blocks")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(Content::Text(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Content::Text(Cow::Owned(v.to_owned())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(Content::Text(Cow::Owned(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let (mut parts, mut tool_result) = (Vec::new(), false);
                while let Some(b) = seq.next_element::<Block>()? {
                    match b.kind.as_deref() {
                        Some("text") => parts.push(b.text.unwrap_or_default().into_owned()),
                        Some("tool_result") => tool_result = true,
                        _ => {}
                    }
                }
                Ok(Content::Blocks {
                    text: parts.join("\n"),
                    tool_result,
                })
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Content::Other)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Content::Other)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(Content::Other)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(Content::Other)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(Content::Other)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(Content::Other)
            }
        }
        d.deserialize_any(V(std::marker::PhantomData))
    }
}

impl Content<'_> {
    fn text(&self) -> &str {
        match self {
            Content::Text(s) => s,
            Content::Blocks { text, .. } => text,
            Content::Other => "",
        }
    }
}

/// 一行里去掉 markdown 记号；下划线只剥强调用的（_重点_、__粗体__），标识符里的（created_at、my_var）留着：
/// 一个下划线两边都是字母数字才留。和 Python 版的两个正则等价。
fn first_line(s: &str) -> String {
    for line in s.lines() {
        let line: Vec<char> = line.chars().filter(|c| !matches!(c, '#' | '*' | '`' | '>' | '|')).collect();
        let alnum = |i: usize| line.get(i).is_some_and(|c| c.is_ascii_alphanumeric());
        let kept: String = line
            .iter()
            .enumerate()
            .filter(|&(i, &c)| c != '_' || (i > 0 && alnum(i - 1) && alnum(i + 1)))
            .map(|(_, &c)| c)
            .collect();
        let kept = kept.trim_matches(|c| c == ' ' || c == '-');
        if !kept.is_empty() {
            return kept.to_owned();
        }
    }
    String::new()
}

fn slash_command(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('/') else { return false };
    let name_len = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '-'))
        .unwrap_or(rest.len());
    name_len > 0 && rest[name_len..].chars().next().is_none_or(char::is_whitespace)
}

/// 这一条开始了新的一轮吗：(kind, 文字)。规则和 Python 版 _trigger 一样。
fn trigger(e: &Raw) -> Option<(&'static str, String)> {
    if e.kind.as_deref() != Some("user") || e.sidechain == Some(true) {
        return None;
    }
    let content = e.message.as_ref().and_then(|m| m.content.as_ref());
    if let Some(Content::Blocks { tool_result: true, .. }) = content {
        return None;
    }
    let kind = e.origin.as_ref().and_then(|o| o.kind.as_deref());
    let text = content.map_or("", Content::text).trim();
    match kind {
        Some("task-notification") => return Some(("notify", t("后台任务完成"))),
        Some("peer") => return Some(("peer", t("收到其他会话的消息"))),
        _ => {}
    }
    if e.meta == Some(true) || text.is_empty() {
        return None;
    }
    if kind == Some("human") {
        return Some(("human", text.to_owned()));
    }
    if NOISE_PREFIXES.iter().any(|p| text.starts_with(p)) || text.starts_with('<') || slash_command(text) {
        return None;
    }
    Some(("human", text.to_owned()))
}

pub struct Transcript {
    pub path: PathBuf,
    pos: u64,
    size: u64,
    pub stops: Vec<Stop>,
    pub title: Option<String>,
    buf: Vec<u8>,
}

impl Transcript {
    pub fn new(path: &Path) -> Self {
        Transcript {
            path: path.to_owned(),
            pos: 0,
            size: u64::MAX,
            stops: Vec::new(),
            title: None,
            buf: Vec::new(),
        }
    }

    /// 读新追加的部分；返回这次有没有读到新东西。最后一行可能还没写完，留到下次。
    pub fn update(&mut self) -> bool {
        let Ok(size) = std::fs::metadata(&self.path).map(|m| m.len()) else {
            return false;
        };
        if size == self.size {
            return false;
        }
        if size < self.pos {
            *self = Transcript::new(&self.path.clone()); // 被截断 / 重写了：从头来
        }
        let start = self.pos;
        let Ok(mut f) = File::open(&self.path) else { return false };
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return false;
        }
        let mut r = BufReader::with_capacity(64 * 1024, f);
        let mut buf = std::mem::take(&mut self.buf);
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) if buf.last() != Some(&b'\n') => break,
                Ok(n) => {
                    self.pos += n as u64;
                    if let Ok(e) = serde_json::from_slice::<Raw>(&buf) {
                        self.feed(&e);
                    } else if std::str::from_utf8(&buf).is_err() {
                        // 少见：行里有坏掉的 UTF-8。Python 版是按 replace 解码以后再解析，这里也一样
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        if let Ok(e) = serde_json::from_str::<Raw>(&text) {
                            self.feed(&e);
                        }
                    }
                }
            }
        }
        if buf.capacity() > 256 << 10 {
            buf = Vec::new(); // 碰到过很长的一行（工具输出、图片）：读完就还掉，别一直占着
        }
        self.buf = buf;
        self.size = size;
        self.pos > start
    }

    fn feed(&mut self, e: &Raw) {
        match (e.kind.as_deref(), e.subtype.as_deref()) {
            (Some("ai-title"), _) => {
                if let Some(t) = e.ai_title.as_deref()
                    && !t.is_empty()
                {
                    self.title = Some(t.to_owned());
                }
                return;
            }
            (Some("system"), Some("compact_boundary")) => {
                for st in &mut self.stops {
                    st.compacted = true; // Claude 的屏幕从这里重新开始：更早的轮次在主界面里已经看不到了
                }
                return;
            }
            _ => {}
        }
        if let Some((kind, text)) = trigger(e) {
            self.stops.push(Stop {
                kind,
                prompt: text.chars().take(PROMPT_MAX).collect(),
                ts: e.timestamp.as_deref().map(str::to_owned),
                id: e.uuid.as_deref().unwrap_or_default().to_owned(),
                reply: String::new(),
                has_text: false,
                compacted: false,
            });
            return;
        }
        if e.kind.as_deref() == Some("assistant")
            && e.sidechain != Some(true)
            && let Some(cur) = self.stops.last_mut()
        {
            let txt = e.message.as_ref().and_then(|m| m.content.as_ref()).map_or("", Content::text).trim();
            if !txt.is_empty() {
                cur.has_text = true;
                cur.reply = first_line(txt).chars().take(REPLY_MAX).collect();
            }
        }
    }

    /// 侧栏列出的轮次：Claude 什么都没说的通知 / 消息那一轮不列；你说的话还没有回复（还在干、或被打断）的照样列。
    pub fn visible(&self) -> Vec<&Stop> {
        self.stops.iter().filter(|s| s.has_text || s.kind == "human").collect()
    }
}
