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

/// 窗格里前台的 Claude 进程号：Claude Code 的进程名是它的版本号，或者 argv0 是 claude。
pub fn claude_pid(pane: &str) -> Option<u32> {
    let r = call("pane.process_info", json!({"pane_id": pane})).ok()?;
    for p in r["process_info"]["foreground_processes"].as_array()? {
        let name = p["name"].as_str().unwrap_or("");
        let is_version = name.split('.').count() == 3 && name.split('.').all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()));
        if p["argv0"].as_str() == Some("claude") || is_version {
            return p["pid"].as_u64().map(|x| x as u32);
        }
    }
    None
}

/// 这一格是 Claude，就返回它的会话 id。
pub fn session_id(info: &Value) -> Option<String> {
    if info["agent"].as_str() != Some("claude") {
        return None;
    }
    info["agent_session"]["value"].as_str().map(str::to_owned)
}
