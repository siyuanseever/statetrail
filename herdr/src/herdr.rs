//! herdr 的 socket 接口：一行一个 JSON 请求、一行一个回答（newline-delimited JSON）。
//! 和 Python 版 `trail.py` 的 `call` / `subscribe` 对应：一次请求一条连接，事件订阅单独占一条长连接。

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn socket_path() -> PathBuf {
    std::env::var_os("HERDR_SOCKET_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".config/herdr/herdr.sock"))
}

fn send(stream: &mut UnixStream, method: &str, params: Value) -> Result<()> {
    let mut line = serde_json::to_vec(&json!({"id": method, "method": method, "params": params})).map_err(|e| Error(e.to_string()))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    Ok(())
}

fn answer(raw: &[u8]) -> Result<Value> {
    let mut v: Value = serde_json::from_slice(raw).map_err(|e| Error(format!("bad answer: {e}")))?;
    if let Some(err) = v.get("error") {
        let code = err.get("code").and_then(Value::as_str).unwrap_or("error");
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(Error(format!("{code}: {msg}")));
    }
    Ok(v.get_mut("result").map(Value::take).unwrap_or(Value::Null))
}

/// 一次请求一条连接。
pub fn call(method: &str, params: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    send(&mut stream, method, params)?;
    let mut line = Vec::new();
    BufReader::new(stream).read_until(b'\n', &mut line)?;
    answer(&line)
}

/// 订阅事件：返回一个读事件的迭代器（每次一条 JSON），连接断了就结束。
pub fn subscribe(subs: Value) -> Result<impl Iterator<Item = Value>> {
    let mut stream = UnixStream::connect(socket_path())?;
    send(&mut stream, "events.subscribe", json!({"subscriptions": subs}))?;
    let mut reader = BufReader::new(stream);
    let mut ack = Vec::new();
    reader.read_until(b'\n', &mut ack)?;
    answer(&ack)?;
    Ok(std::iter::from_fn(move || {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_slice(&line).ok(),
        }
    }))
}

pub fn pane_info(pane: &str) -> Option<Value> {
    call("pane.get", json!({"pane_id": pane}))
        .ok()
        .and_then(|mut v| v.get_mut("pane").map(Value::take))
}

/// Claude Code 的进程名是它的版本号（2.1.310）。
fn is_version(name: &str) -> bool {
    name.split('.').count() == 3 && name.split('.').all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()))
}

/// 窗格里的 Claude 进程：进程号、命令行参数，和窗格里 shell 的进程号。
pub struct ClaudeProc {
    pub pid: u32,
    pub argv: Vec<String>,
    pub shell: Option<u32>,
}

/// 找窗格里的 Claude 进程。Claude 干活时前台进程组可能是它起的子进程（caffeinate、python），claude 本身不在里面，
/// 这时去 shell 的直接子进程里找（ps 的 ucomm 是进程名，也就是版本号；这条路拿到的参数是按空格切开的）。
pub fn claude_proc(pane: &str) -> Option<ClaudeProc> {
    let r = call("pane.process_info", json!({"pane_id": pane})).ok()?;
    let info = &r["process_info"];
    let shell = info["shell_pid"].as_u64().map(|x| x as u32);
    for p in info["foreground_processes"].as_array().into_iter().flatten() {
        if p["argv0"].as_str() == Some("claude") || is_version(p["name"].as_str().unwrap_or("")) {
            let argv = p["argv"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(str::to_owned))
                .collect();
            return p["pid"].as_u64().map(|x| ClaudeProc {
                pid: x as u32,
                argv,
                shell,
            });
        }
    }
    let sh = shell?.to_string();
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,ucomm=,args="])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|ln| {
        let mut rest = ln.trim_start();
        let mut f = Vec::new();
        for _ in 0..3 {
            let (a, b) = rest.split_once(char::is_whitespace)?;
            f.push(a);
            rest = b.trim_start();
        }
        (f[1] == sh && (is_version(f[2]) || f[2] == "claude")).then(|| ClaudeProc {
            pid: f[0].parse().unwrap_or(0),
            argv: rest.split_whitespace().map(str::to_owned).collect(),
            shell,
        })
    })
}

pub fn claude_pid(pane: &str) -> Option<u32> {
    claude_proc(pane).map(|p| p.pid)
}

/// 这一格是 Claude，就返回它的会话 id。herdr 的 Claude 集成只在 SessionStart 时报会话 id，装集成之前就开着的会话
/// 没有，这时退回去读 <配置目录>/sessions/<claude 进程号>.json 里的 sessionId。
pub fn session_id(pane: &str, info: &Value) -> Option<String> {
    if info["agent"].as_str() != Some("claude") {
        return None;
    }
    if let Some(sid) = info["agent_session"]["value"].as_str() {
        return Some(sid.to_owned());
    }
    let pid = claude_pid(pane)?;
    let f = crate::claude::config_dir_of_pid(Some(pid))
        .join("sessions")
        .join(format!("{pid}.json"));
    let v: Value = serde_json::from_slice(&std::fs::read(f).ok()?).ok()?;
    v["sessionId"].as_str().map(str::to_owned)
}
