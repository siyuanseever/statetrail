//! 跨会话搜索：本机所有 Claude 会话里你说过的话（用户 2026-10-29：「我也不知道我之前跟他发的那个消息到底是在哪个 Claude Code 的哪个位置了」）。
//! 会话记录本机有 7.7 GB、532 个（10-29），每打一个字都全读一遍太慢：第一次读一遍，只留你说的话，存进 state_dir/search/index.jsonl；
//! 以后按文件大小和修改时间认，只重读变了的。轮次的编号和轨迹一样（Transcript::visible 里的下标），跳过去时对得上。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::transcript::Transcript;

#[derive(Serialize, Deserialize, Clone)]
pub struct Turn {
    pub n: usize, // visible() 里的下标
    pub id: String,
    pub ts: Option<String>,
    pub prompt: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Session {
    pub path: PathBuf,
    pub cfg_dir: PathBuf,
    size: u64,
    pub mtime: u64,
    pub sid: String,
    pub cwd: String,
    pub title: Option<String>,
    pub turns: Vec<Turn>,
}

fn index_path() -> PathBuf {
    crate::state_dir().join("search").join("index.v2.jsonl") // v2：去掉了脚本跑的会话；v1 那份删掉
}

/// 所有账号目录下的会话记录：<配置目录>/projects/<项目>/<会话>.jsonl（子 Agent 的在更深一层，不算）
fn files() -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for cfg in crate::claude::config_dirs() {
        // 几个账号目录的 projects 可能链到同一处（本机 ~/.claude-l2/projects → ~/.claude/projects）：按真实路径只读一遍
        let Ok(real) = std::fs::canonicalize(cfg.join("projects")) else { continue };
        if seen.contains(&real) {
            continue;
        }
        seen.push(real);
        for proj in std::fs::read_dir(cfg.join("projects")).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(proj.path()).into_iter().flatten().flatten() {
                let p = f.path();
                if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                    out.push((cfg.clone(), p));
                }
            }
        }
    }
    out
}

fn load() -> HashMap<PathBuf, Session> {
    let Ok(f) = std::fs::File::open(index_path()) else { return HashMap::new() };
    BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<Session>(&l).ok())
        .map(|s| (s.path.clone(), s))
        .collect()
}

fn save(all: &[Session]) {
    let path = index_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    let tmp = path.with_extension("tmp");
    let ok = std::fs::File::create(&tmp).is_ok_and(|f| {
        let mut w = std::io::BufWriter::new(f);
        all.iter().all(|s| serde_json::to_writer(&mut w, s).is_ok() && w.write_all(b"\n").is_ok()) && w.flush().is_ok()
    });
    if ok {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 记录开头几行里某个字段的值（"cwd"：会话在哪个目录里开的；"entrypoint"：怎么开的）
fn head_field(head: &str, key: &str) -> String {
    let pat = format!("\"{key}\":\"");
    let Some(at) = head.find(&pat) else { return String::new() };
    let rest = &head[at + pat.len()..];
    rest[..rest.find('"').unwrap_or(rest.len())].replace("\\\\", "\\")
}

fn parse(cfg_dir: &Path, path: &Path, size: u64, mtime: u64) -> Session {
    let mut head = Vec::new();
    let _ = std::fs::File::open(path).map(|f| f.take(256 << 10).read_to_end(&mut head));
    let head = String::from_utf8_lossy(&head);
    let mut tr = Transcript::new(path);
    // 脚本里 claude -p 跑的（entrypoint 是 sdk-cli，本机 10-29 有 473 个，日报、通讯录之类）不是你亲手发的话：不读，只记个空的免得下次再读
    // 手开的是 cli / claude-desktop
    if head_field(&head, "entrypoint") != "sdk-cli" {
        tr.update();
    }
    let turns = tr
        .visible()
        .iter()
        .enumerate()
        .filter(|(_, st)| st.kind == "human")
        .map(|(n, st)| Turn {
            n,
            id: st.id.clone(),
            ts: st.ts.clone(),
            prompt: st.prompt.clone(),
        })
        .collect();
    Session {
        path: path.to_owned(),
        cfg_dir: cfg_dir.to_owned(),
        size,
        mtime,
        sid: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        cwd: head_field(&head, "cwd"),
        title: tr.title.clone(),
        turns,
    }
}

/// 读索引、重读变了的会话、存回去；progress(已读, 要重读的总数)。返回有你说过话的会话，最近改过的在前。
pub fn update(mut progress: impl FnMut(usize, usize)) -> Vec<Session> {
    let _ = std::fs::remove_file(crate::state_dir().join("search").join("index.jsonl"));
    let mut old = load();
    let mut stale = Vec::new();
    let mut all = Vec::new();
    for (cfg, p) in files() {
        let Ok(m) = std::fs::metadata(&p) else { continue };
        let mtime = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        match old.remove(&p) {
            Some(s) if s.size == m.len() && s.mtime == mtime => all.push(s),
            _ => stale.push((cfg, p, m.len(), mtime)),
        }
    }
    let total = stale.len();
    for (i, (cfg, p, size, mtime)) in stale.into_iter().enumerate() {
        progress(i, total);
        all.push(parse(&cfg, &p, size, mtime));
    }
    if total > 0 {
        save(&all);
    }
    all.retain(|s| !s.turns.is_empty());
    all.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    all
}

/// 搜：q 已经是小写；按那句话发的时间，最近的在前，最多 limit 条
pub fn find<'a>(all: &'a [Session], q: &str, limit: usize) -> Vec<(&'a Session, &'a Turn)> {
    if q.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<(&Session, &Turn)> = all
        .iter()
        .flat_map(|s| s.turns.iter().map(move |t| (s, t)))
        .filter(|(_, t)| t.prompt.to_lowercase().contains(q))
        .collect();
    out.sort_by(|a, b| b.1.ts.cmp(&a.1.ts)); // ISO 时间，按字面比就是按时间比
    out.truncate(limit);
    out
}

/// statetrail find <字>：在终端里列出来（也用来量建索引要多久）
pub fn cmd_find(q: &str) {
    let t0 = std::time::Instant::now();
    let all = update(|i, n| {
        if i % 50 == 0 {
            eprint!("\r{i}/{n}");
        }
    });
    eprintln!("\r{} sessions, {:.1}s", all.len(), t0.elapsed().as_secs_f64());
    let q = q.trim().to_lowercase();
    for (s, t) in find(&all, &q, 50) {
        let one: String = t.prompt.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
        println!(
            "{}  {}  #{}  {}  {}",
            t.ts.as_deref().unwrap_or("").get(..16).unwrap_or(""),
            s.title.as_deref().unwrap_or(&s.sid[..8.min(s.sid.len())]),
            t.n + 1,
            s.cwd,
            one
        );
    }
}
