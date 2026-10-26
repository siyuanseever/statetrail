//! Claude Code 这边的东西：配置目录、找对话记录、账号、模型和上下文、订阅额度（5h / 7d）。
//! 对应 Python 版 core/claude.py，规则一样；读数文件（usage-*.json、fetch.log）也是同一份格式、同一个目录。

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::i18n::t;

pub const CTX_HARD: u64 = 500_000; // 没配 autoCompactWindow 时的上下文目标线
const TAIL_BYTES: u64 = 4 * 1024 * 1024;
const FORCE_GAP_MS: i64 = 20_000; // 两次手动刷新至少隔这么久
const URL: &str = "https://api.anthropic.com/api/oauth/usage";

pub fn default_cfg() -> PathBuf {
    crate::home().join(".claude")
}

/// 可能的 CLAUDE_CONFIG_DIR：默认的 ~/.claude，加上 ~/.claude-*（一个账号一个目录）。STATETRAIL_CONFIG_DIRS（冒号分隔）可以改。
pub fn config_dirs() -> Vec<PathBuf> {
    if let Ok(env) = std::env::var("STATETRAIL_CONFIG_DIRS") {
        return env.split(':').filter(|d| !d.is_empty()).map(crate::expand).collect();
    }
    let mut extra: Vec<PathBuf> = std::fs::read_dir(crate::home())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".claude-") && e.path().join("projects").is_dir())
        .map(|e| e.path())
        .collect();
    extra.sort();
    let mut out = vec![default_cfg()];
    out.extend(extra);
    out
}

/// 这个进程的 Claude 用的是哪个配置目录：Claude Code 给每个进程写 <配置目录>/sessions/<pid>.json。
pub fn config_dir_of_pid(pid: Option<u32>) -> PathBuf {
    if let Some(pid) = pid {
        for d in config_dirs() {
            if d.join("sessions").join(format!("{pid}.json")).exists() {
                return d;
            }
        }
    }
    default_cfg()
}

/// 会话 id → 对话记录。先在它自己的配置目录里找，再找别的；同一个 id 有好几份就取最新改过的。
pub fn transcript_path(sid: &str, cfg_dir: &Path) -> Option<PathBuf> {
    let name = format!("{sid}.jsonl");
    let mut dirs = vec![cfg_dir.to_owned()];
    dirs.extend(config_dirs().into_iter().filter(|d| d != cfg_dir));
    for d in dirs {
        let mut hits: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(d.join("projects"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|p| p.path().join(&name))
            .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
            .collect();
        hits.sort();
        if let Some((_, p)) = hits.pop() {
            return Some(p);
        }
    }
    None
}

// ---- 账号（.claude.json）-----------------------------------------------------------

/// .claude.json 里要用的两段。这个文件可能很大（每个项目的历史都在里面），只解出这两段，其余跳过不建对象。
#[derive(Deserialize, Default, Clone)]
struct ClaudeJson {
    #[serde(rename = "oauthAccount", default)]
    account: Option<Account>,
    #[serde(rename = "cachedUsageUtilization", default)]
    cached: Option<Value>,
}

#[derive(Deserialize, Default, Clone)]
struct Account {
    #[serde(rename = "emailAddress", default)]
    email: Option<String>,
    #[serde(rename = "accountUuid", default)]
    uuid: Option<String>,
}

static CJ: Mutex<Option<HashMap<PathBuf, (SystemTime, ClaudeJson)>>> = Mutex::new(None);

fn claude_json(cfg_dir: &Path) -> ClaudeJson {
    let conf = if cfg_dir == default_cfg() {
        crate::home().join(".claude.json")
    } else {
        cfg_dir.join(".claude.json")
    };
    let Ok(mt) = std::fs::metadata(&conf).and_then(|m| m.modified()) else {
        return ClaudeJson::default();
    };
    let mut guard = CJ.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some((t, v)) = cache.get(&conf)
        && *t == mt
    {
        return v.clone();
    }
    let v: ClaudeJson = std::fs::File::open(&conf)
        .ok()
        .and_then(|f| serde_json::from_reader(std::io::BufReader::new(f)).ok())
        .unwrap_or_default();
    cache.insert(conf, (mt, v.clone()));
    v
}

/// 这个配置目录登的是哪个账号，取邮箱 @ 前面那段。
pub fn current_account(cfg_dir: &Path) -> String {
    let email = claude_json(cfg_dir).account.and_then(|a| a.email).unwrap_or_default();
    email.split('@').next().unwrap_or("").to_owned()
}

/// 账号缩写：和别的账号比，只留不一样的那截（you.a / you.b → a / b）。
pub fn short_account(cfg_dir: &Path) -> String {
    let me = current_account(cfg_dir);
    let others: Vec<String> = config_dirs()
        .iter()
        .filter(|d| d.as_path() != cfg_dir)
        .map(|d| current_account(d))
        .filter(|o| !o.is_empty() && *o != me)
        .collect();
    if me.is_empty() || others.is_empty() {
        return me;
    }
    let mut pre = me.clone();
    for o in &others {
        let n = pre.chars().zip(o.chars()).take_while(|(a, b)| a == b).count();
        pre = pre.chars().take(n).collect();
    }
    match pre.rfind('.') {
        Some(i) => me[i + 1..].to_owned(),
        None => me,
    }
}

// ---- 模型和上下文 -------------------------------------------------------------------

/// claude-opus-5-5 → Opus 5.5，claude-haiku-4-5-20251001 → Haiku 4.5
pub fn short_model(mid: &str) -> String {
    let base = mid.replace("claude-", "");
    let base = base.split('[').next().unwrap_or("");
    let parts: Vec<&str> = base
        .split('-')
        .filter(|p| !(p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit())))
        .collect();
    let Some(first) = parts.first() else { return String::new() };
    let ver: Vec<&str> = parts[1..]
        .iter()
        .copied()
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    let mut name: String = first
        .chars()
        .take(1)
        .flat_map(char::to_uppercase)
        .chain(first.chars().skip(1).flat_map(char::to_lowercase))
        .collect();
    if !ver.is_empty() {
        name = format!("{name} {}", ver.join("."));
    }
    name.trim().to_owned()
}

#[derive(Deserialize)]
struct UsageLine {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(rename = "isSidechain", default)]
    sidechain: Option<bool>,
    #[serde(rename = "compactMetadata", default)]
    compact: Option<CompactMeta>,
    #[serde(default)]
    message: Option<UsageMsg>,
    #[serde(default)]
    effort: Option<Value>,
}

#[derive(Deserialize)]
struct CompactMeta {
    #[serde(rename = "postTokens", default)]
    post: Option<u64>,
}

#[derive(Deserialize)]
struct UsageMsg {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<HashMap<String, Value>>,
}

/// 从一段（尾巴上的）行里，倒着找最新一条主线回复的上下文大小 (tokens, model, effort)。
fn scan_usage(lines: &[&[u8]]) -> (Option<u64>, Option<String>, Option<String>) {
    // 压缩分界线之后 Claude 还没回过话：用量取分界线上的 post，模型 / effort 接着往前找（压缩不换模型）
    let (mut tokens, mut model, mut effort) = (None, None, None);
    for raw in lines.iter().rev() {
        let Ok(e) = serde_json::from_slice::<UsageLine>(raw) else {
            continue;
        };
        if e.sidechain == Some(true) {
            continue;
        }
        if e.kind.as_deref() == Some("system") && e.subtype.as_deref() == Some("compact_boundary") {
            if let (None, Some(post)) = (tokens, e.compact.and_then(|c| c.post)) {
                tokens = Some(post);
            }
            continue;
        }
        let Some(msg) = e.message else { continue };
        if e.kind.as_deref() != Some("assistant") {
            continue;
        }
        let Some(u) = msg.usage.filter(|u| !u.is_empty()) else { continue };
        if model.is_none() {
            model = msg.model;
        }
        if effort.is_none() {
            effort = e.effort.and_then(|v| v.as_str().map(str::to_owned)).filter(|s| !s.is_empty());
        }
        if model.as_deref() == Some("<synthetic>") {
            model = None;
            continue;
        }
        if tokens.is_some() {
            return (tokens, model, effort);
        }
        let n = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
            "output_tokens",
        ]
        .iter()
        .map(|k| u.get(*k).and_then(Value::as_u64).unwrap_or(0))
        .sum();
        return (Some(n), model, effort);
    }
    (tokens, model, effort)
}

/// 先看最后 256 KB，找不到再翻倍，最多 4 MB（每次 Claude 回复都会叫一次，别每次都读 4 MB）。
fn last_usage(path: &Path) -> (Option<u64>, Option<String>, Option<String>) {
    let mut span = 256 * 1024;
    loop {
        let Ok(mut f) = std::fs::File::open(path) else {
            return (None, None, None);
        };
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = f.seek(SeekFrom::Start(size.saturating_sub(span)));
        let mut buf = Vec::new();
        let _ = f.read_to_end(&mut buf);
        let lines: Vec<&[u8]> = buf.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
        let found = scan_usage(&lines);
        if (found.0.is_some() && found.1.is_some()) || span >= TAIL_BYTES || span >= size {
            return found;
        }
        span *= 2;
    }
}

/// (`Opus 5.5/high`, `11% (111k)`)：百分比按模型的上下文窗口算，haiku 20 万，其余 100 万。
pub fn model_ctx(path: &Path) -> (String, String) {
    let (tokens, mid, effort) = last_usage(path);
    let mut name = short_model(mid.as_deref().unwrap_or(""));
    if let (false, Some(e)) = (name.is_empty(), &effort) {
        name = format!("{name}/{e}");
    }
    let Some(tokens) = tokens else { return (name, String::new()) };
    let window = if mid.as_deref().is_some_and(|m| m.contains("haiku")) {
        200_000
    } else {
        1_000_000
    };
    (name, format!("{}% ({}k)", tokens * 100 / window, tokens / 1000))
}

/// 自动压缩的那条线：settings.json 的 autoCompactWindow，没写就是 CTX_HARD；关了自动压缩就是 None。
pub fn auto_compact_at() -> Option<u64> {
    let Some(d) = std::fs::read(default_cfg().join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return Some(CTX_HARD);
    };
    if d["autoCompactEnabled"] == json!(false) {
        return None;
    }
    Some(d["autoCompactWindow"].as_u64().filter(|&n| n > 0).unwrap_or(CTX_HARD))
}

// ---- 订阅额度 -------------------------------------------------------------------------

#[derive(Clone, Default, Debug)]
pub struct Window {
    pub utilization: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Clone, Default, Debug)]
pub struct Usage {
    pub fetched_at_ms: i64,
    pub attempted_at_ms: i64,
    pub error_at_ms: i64,
    pub five_hour: Window,
    pub seven_day: Window,
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn parse_ts(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s).ok().or_else(|| {
        // Python 的 fromisoformat 也认不带时区的写法：当成本地时间
        let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
        naive.and_local_timezone(chrono::Local).single().map(|d| d.fixed_offset())
    })
}

/// 还剩多少秒重置；认不出就是 None。
pub fn secs_left(w: &Window) -> Option<f64> {
    let d = parse_ts(w.resets_at.as_deref()?)?;
    Some((d.timestamp_millis() - now_ms()) as f64 / 1000.0)
}

fn window_of(v: &Value) -> Window {
    Window {
        utilization: v["utilization"].as_f64(),
        resets_at: v["resets_at"].as_str().map(str::to_owned),
    }
}

fn fresh(w: &Window) -> bool {
    secs_left(w).is_some_and(|s| s > 0.0)
}

/// 一个配置目录一份读数，文件名带目录路径 sha256 的前 8 位（和 Python 版同名，两版共用节流）。
pub fn usage_file(cfg_dir: &Path) -> PathBuf {
    crate::state_dir().join(format!("usage-{}.json", &sha256_hex(cfg_dir.to_string_lossy().as_bytes())[..8]))
}

/// Claude Code 自己缓存在 .claude.json 里的额度：不碰凭据。不是当前登录账号的整个不用；过了重置时间的窗口当成没有读数。
fn cached_usage(cfg_dir: &Path) -> Option<Usage> {
    let d = claude_json(cfg_dir);
    let c = d.cached.filter(|c| c.as_object().is_some_and(|m| !m.is_empty()))?;
    let me = d.account.and_then(|a| a.uuid);
    if let (Some(me), Some(theirs)) = (me, c["accountUuid"].as_str())
        && me != theirs
    {
        return None;
    }
    let mut u = Usage {
        fetched_at_ms: c["fetchedAtMs"].as_i64().unwrap_or(0),
        ..Default::default()
    };
    for (w, slot) in [("five_hour", &mut u.five_hour), ("seven_day", &mut u.seven_day)] {
        let win = window_of(&c["utilization"][w]);
        if fresh(&win) {
            *slot = win;
        }
    }
    Some(u)
}

/// 可选的 statusLine 桥接（Python 版 statusline.py）记下的这个会话的额度。resets_at 是 Unix 秒。
fn statusline_usage(sid: Option<&str>) -> Option<Usage> {
    let sid = sid?;
    let d: Value = serde_json::from_slice(&std::fs::read(crate::state_dir().join("statusline").join(format!("{sid}.json"))).ok()?).ok()?;
    let mut u = Usage {
        fetched_at_ms: d["fetched_at_ms"].as_i64().unwrap_or(0),
        ..Default::default()
    };
    for (w, slot) in [("five_hour", &mut u.five_hour), ("seven_day", &mut u.seven_day)] {
        let win = &d["rate_limits"][w];
        let reset = win["resets_at"]
            .as_f64()
            .and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0))
            .map(|d| d.with_timezone(&chrono::Local).to_rfc3339());
        *slot = Window {
            utilization: win["used_percentage"].as_f64(),
            resets_at: reset,
        };
    }
    Some(u)
}

fn read_live(cfg_dir: &Path) -> Value {
    std::fs::read(usage_file(cfg_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(json!({}))
}

fn live_usage(cfg_dir: &Path, live: bool) -> Option<Usage> {
    if !live {
        return None;
    }
    let v = read_live(cfg_dir);
    if !v.is_object() || v.as_object().is_some_and(|m| m.is_empty()) {
        return None;
    }
    Some(Usage {
        fetched_at_ms: v["fetched_at_ms"].as_i64().unwrap_or(0),
        attempted_at_ms: v["attempted_at_ms"].as_i64().unwrap_or(0),
        error_at_ms: v["error_at_ms"].as_i64().unwrap_or(0),
        five_hour: window_of(&v["five_hour"]),
        seven_day: window_of(&v["seven_day"]),
    })
}

/// 表头用的额度，一层一层兜底：实时查询、statusLine 桥接、Claude Code 的缓存，谁最新用谁；都没有就是 None。
pub fn read_usage(cfg_dir: &Path, sid: Option<&str>, live: bool) -> Option<Usage> {
    let got = [live_usage(cfg_dir, live), statusline_usage(sid), cached_usage(cfg_dir)];
    let mut u = got.into_iter().flatten().max_by_key(|u| u.fetched_at_ms)?;
    for w in [&mut u.five_hour, &mut u.seven_day] {
        if !fresh(w) {
            *w = Window::default(); // 过了重置时间的旧百分比不作数
        }
    }
    Some(u)
}

/// 自动刷新的读数旧了没有（默认 30 分钟）。
pub fn usage_stale(cfg_dir: &Path) -> bool {
    let v = read_live(cfg_dir);
    let last = v["fetched_at_ms"]
        .as_i64()
        .unwrap_or(0)
        .max(v["attempted_at_ms"].as_i64().unwrap_or(0));
    now_ms() - last >= interval_ms()
}

fn interval_ms() -> i64 {
    60_000
        * std::env::var("STATETRAIL_USAGE_REFRESH_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30)
}

fn keychain_service(cfg_dir: &Path) -> String {
    if cfg_dir == default_cfg() {
        "Claude Code-credentials".into()
    } else {
        format!("Claude Code-credentials-{}", &sha256_hex(cfg_dir.to_string_lossy().as_bytes())[..8])
    }
}

/// Claude Code 存的 OAuth access token：macOS 钥匙串，没有就读 <配置目录>/.credentials.json。只在内存里用这一次。
fn token(cfg_dir: &Path) -> Result<String, String> {
    let mut raw = std::process::Command::new("security")
        .args(["find-generic-password", "-s", &keychain_service(cfg_dir), "-w"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if raw.is_empty() {
        raw = std::fs::read_to_string(cfg_dir.join(".credentials.json")).map_err(|_| t("找不到 Claude Code 的登录凭据"))?;
    }
    let o: Value = serde_json::from_str(raw.trim()).map_err(|_| t("凭据格式认不出"))?;
    let o = &o["claudeAiOauth"];
    if o["expiresAt"].as_i64().unwrap_or(0) < now_ms() {
        return Err(t("令牌已过期，开一次 Claude Code 会自动续上"));
    }
    o["accessToken"].as_str().map(str::to_owned).ok_or_else(|| t("凭据格式认不出"))
}

/// 查一次。用系统的 curl：令牌从 stdin 给它（不出现在进程列表里），代理环境变量 curl 自己会认。
fn fetch(cfg_dir: &Path) -> Result<(Window, Window), String> {
    let tok = token(cfg_dir)?;
    let mut child = std::process::Command::new("curl")
        .args(["-sS", "--max-time", "15", "-K", "-", "-w", "\n%{http_code}", URL])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| t("查询失败（{err}）").replace("{err}", "curl"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(
            stdin,
            "header = \"Authorization: Bearer {tok}\"\nheader = \"anthropic-beta: oauth-2025-04-20\"\nuser-agent = \"statetrail\""
        );
    }
    let out = child
        .wait_with_output()
        .map_err(|_| t("查询失败（{err}）").replace("{err}", "curl"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", "000"));
    if code != "200" {
        return Err(if code == "000" {
            t("查询失败（{err}）").replace("{err}", "network")
        } else {
            t("查询失败 HTTP {code}").replace("{code}", code)
        });
    }
    let d: Value = serde_json::from_str(body).map_err(|_| t("查询失败（{err}）").replace("{err}", "JSON"))?;
    Ok((window_of(&d["five_hour"]), window_of(&d["seven_day"])))
}

fn save(path: &Path, v: &Value) {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn log_fetch(cfg_dir: &Path, err: Option<&str>) {
    let log = crate::state_dir().join("fetch.log");
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > 100_000) {
        let _ = std::fs::write(&log, "");
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
        let _ = writeln!(f, "{}\t{}\t{}", now_ms() / 1000, cfg_dir.display(), err.unwrap_or("ok"));
    }
}

/// 去查一次这个账号的真实额度。自动的最多每 30 分钟一次（失败的也算），手动的两次至少隔 20 秒；
/// 好几个轨迹同时在看同一个账号时用文件锁串起来，只发一个请求。要一两秒，放后台线程里调。
pub fn refresh_usage(cfg_dir: &Path, force: bool) {
    if !cfg_dir.is_dir() {
        return;
    }
    let _ = std::fs::create_dir_all(crate::state_dir());
    let path = usage_file(cfg_dir);
    let Ok(lock) = std::fs::File::create(path.with_extension("lock")) else {
        return;
    };
    if lock.try_lock().is_err() {
        return; // 别的轨迹正在查
    }
    let mut live = read_live(cfg_dir);
    if !live.is_object() {
        live = json!({});
    }
    let now = now_ms();
    let last = live["fetched_at_ms"]
        .as_i64()
        .unwrap_or(0)
        .max(live["attempted_at_ms"].as_i64().unwrap_or(0));
    if !force && now - last < interval_ms() {
        return;
    }
    if force && now - live["attempted_at_ms"].as_i64().unwrap_or(0) < FORCE_GAP_MS {
        return;
    }
    live["attempted_at_ms"] = json!(now);
    save(&path, &live); // 先记下「在查」：表头显示「查询中…」
    let r = fetch(cfg_dir);
    log_fetch(cfg_dir, r.as_ref().err().map(String::as_str));
    match r {
        Ok((five, seven)) => {
            live["five_hour"] = json!({"utilization": five.utilization, "resets_at": five.resets_at});
            live["seven_day"] = json!({"utilization": seven.utilization, "resets_at": seven.resets_at});
            live["fetched_at_ms"] = json!(now_ms());
            if let Some(m) = live.as_object_mut() {
                m.remove("error");
            }
        }
        Err(e) => {
            live["error"] = json!(e);
            live["error_at_ms"] = json!(now_ms());
        }
    }
    save(&path, &live);
}

// ---- sha256（文件名和钥匙串条目名要和 Python 版 / Claude Code 一致）--------------------------

fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be,
        0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa,
        0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85,
        0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
        0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f,
        0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn sha256_matches_known_values() {
        assert_eq!(
            super::sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            super::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn short_model_like_python() {
        assert_eq!(super::short_model("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(super::short_model("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(super::short_model("claude-sonnet-5-5[1m]"), "Sonnet 5.5");
    }

    #[test]
    fn model_survives_compact() {
        let a = br#"{"type":"assistant","effort":"high","message":{"model":"claude-opus-5-5","usage":{"input_tokens":500000}}}"#;
        let b = br#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"postTokens":30000}}"#;
        let u = br#"{"type":"user","message":{"content":"hi"}}"#;
        let got = super::scan_usage(&[&a[..], &b[..], &u[..]]);
        assert_eq!(got, (Some(30000), Some("claude-opus-5-5".into()), Some("high".into())));
        // 分界线之后回过话：用量取那条回复的
        let c = br#"{"type":"assistant","message":{"model":"claude-sonnet-5-5","usage":{"input_tokens":40000}}}"#;
        let got = super::scan_usage(&[&a[..], &b[..], &c[..]]);
        assert_eq!(got, (Some(40000), Some("claude-sonnet-5-5".into()), None));
    }
}
