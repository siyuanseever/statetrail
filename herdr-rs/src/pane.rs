//! 轨迹窗格本身（`statetrail pane`）。主线程只在一个通道上等：键盘鼠标、herdr 事件、别的进程发来的命令（toggle）、
//! 跳转线程的结果各有一个线程往里送，空闲时不轮询、不醒。对应 Python 版的 Sidebar.run。

use crossterm::event::{self, Event, MouseButton, MouseEventKind};
use crossterm::{execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::header;
use crate::i18n::t;
use crate::jump::{self, Outcome, Term};
use crate::render;
use crate::transcript::Transcript;
use crate::ui::{self, Item};
use crate::{Config, FRAME, WIDE_MIN, claude, herdr};

const TICK: Duration = Duration::from_secs(60); // 没有事件时多久醒一次
const AGENT_CHECK: Duration = Duration::from_secs(5); // Claude 退出了却没等到事件：这么久补查一次
const GONE_GRACE: Duration = Duration::from_secs(2); // Claude 退出后过这么久就关掉轨迹，不留残骸
const WHEEL: usize = 3; // 滚轮一格翻几项
const TAB_NAME_MAX: usize = 24; // tab 名最多多少列（中文算两格）

enum Msg {
    Input(Event),
    Herdr(Value),
    HerdrGone,
    Command(String),
    Flash(String, Duration),
    Jumped(String, String), // (轮次 id, 结果)
    Wake,                   // 后台查完额度了：重画
}

fn sock_of(pane: &str) -> PathBuf {
    crate::run_dir().join(format!("{}.sock", pane.replace(':', "_")))
}

/// 给开着的轨迹发一条命令（toggle 用）。
pub fn send_command(pane: &str, cmd: &str) {
    if let Ok(mut s) = UnixStream::connect(sock_of(pane)) {
        let _ = writeln!(s, "{cmd}");
    }
}

struct HerdrTerm {
    pane: String,
    height: usize,
}

impl Term for HerdrTerm {
    fn screen(&self) -> String {
        herdr::call("pane.read", json!({"pane_id": self.pane, "source": "visible"}))
            .ok()
            .and_then(|r| r["read"]["text"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }
    fn send(&self, keys: &[&str]) {
        let text: String = keys
            .iter()
            .map(|k| match *k {
                "C-Home" => "\x1b[1;5H",
                "C-End" => "\x1b[1;5F",
                "PageUp" => "\x1b[5~",
                "PageDown" => "\x1b[6~",
                raw => raw,
            })
            .collect();
        let _ = herdr::call("pane.send_text", json!({"pane_id": self.pane, "text": text}));
    }
    fn height(&self) -> usize {
        self.height
    }
}

struct Trail {
    target: String,
    me: String,
    cfg: Config,
    sid: Option<String>,
    cfg_dir: PathBuf,
    tr: Option<Transcript>,
    status: Option<String>,
    opened: Option<String>,
    expanded: bool,
    missing: HashSet<String>,
    flash: Option<(String, Instant)>,
    offset: usize,
    follow: bool, // 停在最底：列表变长时跟着走
    no_claude_since: Option<Instant>,
    jump_token: Arc<AtomicUsize>,
    last_wide: Option<i64>,
    hits: Vec<(u16, Item)>,
    header_len: u16,
    head_ln: usize,                      // 宽版「N 轮」那一行（点它什么都不做）
    mc: (Option<u64>, (String, String)), // (对话记录大小, (模型, 上下文))：没变就不重读尾巴
    last_kick: Option<Instant>,
    view: (usize, usize, usize),     // (列表上面还有几轮, 下面还有几轮, 露出来几项)
    up_at: Option<(usize, usize)>,   // 表头里的 ▲：(第几行, 占到第几列)
    down_at: Option<(usize, usize)>, // 底部的 ▼：(第几行, 占到第几列)
    footer_top: u16,
    titled: Option<String>, // 上次拿来给 tab 起名的会话名
    code: CodeSig,          // 启动时的程序
    code_new: Option<CodeSig>,
    restart: Option<PathBuf>, // 插件更新了：主循环退出后在这一格原地换成这个目录里的程序
}

type CodeSig = (PathBuf, Option<std::time::SystemTime>);

/// (herdr 登记的插件目录, 那里 bin/statetrail 的修改时间)。herdr plugin update 会装进一个新目录，旧目录里的文件不变，
/// 所以要看 herdr 的登记（plugins.json）；本地 link 的目录换了新编译的程序，修改时间会变。
fn code_sig() -> CodeSig {
    let here = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent().map(PathBuf::from))
        .unwrap_or_default();
    let registry = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .and_then(|c| c.ancestors().nth(3).map(|h| h.join("plugins.json")))
        .unwrap_or_else(|| crate::home().join(".config/herdr/plugins.json"));
    let root = std::fs::read(&registry)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| {
            v.as_array()?
                .iter()
                .find(|p| p["plugin_id"].as_str() == Some(crate::plugin_id().as_str()))
                .and_then(|p| p["plugin_root"].as_str().map(PathBuf::from))
        })
        .unwrap_or(here);
    let mtime = std::fs::metadata(root.join("bin/statetrail")).and_then(|m| m.modified()).ok();
    (root, mtime)
}

impl Trail {
    fn busy(&self) -> bool {
        matches!(self.status.as_deref(), Some("working" | "blocked"))
    }

    /// 问 herdr 这个 Claude 现在什么状态、跑的哪个会话，读对话记录新追加的部分。返回 false = Claude 那一格没了。
    fn refresh(&mut self) -> bool {
        let Some(info) = herdr::pane_info(&self.target) else { return false };
        self.name_tab(&info);
        self.status = info["agent_status"].as_str().map(str::to_owned);
        if info["agent"].as_str() == Some("claude") {
            self.no_claude_since = None;
        } else if self.no_claude_since.is_none() {
            self.no_claude_since = Some(Instant::now());
        }
        let sid = herdr::session_id(&info);
        if sid != self.sid {
            // /clear、/resume 换了会话
            self.sid = sid;
            self.tr = None;
            self.opened = None;
            self.expanded = false;
            self.missing.clear();
            self.follow = true;
            if self.sid.is_some() {
                self.cfg_dir = claude::config_dir_of_pid(herdr::claude_pid(&self.target));
            }
        }
        if let (Some(sid), None) = (&self.sid, &self.tr)
            && let Some(p) = claude::transcript_path(sid, &self.cfg_dir)
        {
            self.tr = Some(Transcript::new(&p));
        }
        if let Some(tr) = &mut self.tr {
            tr.update();
        }
        true
    }

    fn model_ctx(&mut self) -> (String, String) {
        let Some(tr) = &self.tr else {
            return (String::new(), String::new());
        };
        let size = std::fs::metadata(&tr.path).map(|m| m.len()).ok();
        if size != self.mc.0 {
            self.mc = (size, claude::model_ctx(&tr.path));
        }
        self.mc.1.clone()
    }

    /// （live_usage 打开时）额度旧了（默认 30 分钟）就在后台线程里去查一次；claude::refresh_usage 自己还会再节流一道，
    /// 所有轨迹共用（同一个账号用文件锁串起来），这里多叫一次也不会多发请求。
    fn kick_usage(&mut self, force: bool, tx: &Sender<Msg>) {
        if !self.cfg.live_usage {
            if force {
                self.say(
                    t("额度来自 Claude Code 的缓存；要实时查询，在配置里设 live_usage = true"),
                    Duration::from_secs(8),
                );
            }
            return;
        }
        if !force && (!claude::usage_stale(&self.cfg_dir) || self.last_kick.is_some_and(|k| k.elapsed() < Duration::from_secs(60))) {
            return;
        }
        self.last_kick = Some(Instant::now());
        let (cfg, tx) = (self.cfg_dir.clone(), tx.clone());
        std::thread::spawn(move || {
            claude::refresh_usage(&cfg, force);
            let _ = tx.send(Msg::Wake); // 查完了：表头跟上
        });
    }

    fn act(&mut self, act: &str, width: u16, tx: &Sender<Msg>) {
        match act {
            "usage" => self.kick_usage(true, tx),
            "bottom" => {
                self.jump_token.fetch_add(1, Ordering::SeqCst);
                HerdrTerm {
                    pane: self.target.clone(),
                    height: 0,
                }
                .send(&["C-End"]);
                self.follow = true;
            }
            "narrow" | "wide" => self.toggle(width),
            _ => {}
        }
    }

    /// 点表头：第 1 行是按钮；宽版「N 轮」那一行什么都不做；模型、柱子那几行点了去查一次真实额度。
    fn header_click(&mut self, ln: usize, col: usize, width: u16, tx: &Sender<Msg>) {
        let narrow = width < WIDE_MIN;
        if self.up_at.is_some_and(|(l, slot)| l == ln && col <= slot + 1) {
            self.page(-1); // 点在行首 ▲ 那一段（偏一格也算）
            return;
        }
        if ln == 1 {
            let buttons = if narrow { header::narrow_buttons() } else { header::wide_buttons() };
            if let Some(a) = header::button_at(&buttons, col) {
                self.act(a, width, tx);
            }
        } else if narrow || ln != self.head_ln {
            self.kick_usage(true, tx);
        }
    }

    /// 点 ▲ / ▼：翻一屏的轮数，少翻一项留一轮重叠（边上那一轮常常只露出半截）。
    fn page(&mut self, d: i32) {
        let step = self.view.2.saturating_sub(1).max(1);
        if d < 0 {
            self.offset = self.offset.saturating_sub(step);
            self.follow = false;
        } else {
            self.offset += step; // 画的时候按最底夹住；下面已经没有了就重新跟着走
            self.follow = self.view.1 <= step;
        }
    }

    /// 用 Claude 的会话名（终端标题，Claude 会按话题自己改）给 tab 命名。只动「没被你手动起过名」的 tab：
    /// label 还是 herdr 默认的编号，或者还是我们上次起的那个名字。和 Python 版 name_tab 一样。
    fn name_tab(&mut self, info: &Value) {
        let title = info["terminal_title_stripped"].as_str().unwrap_or("").trim().to_owned();
        let Some(tab) = info["tab_id"].as_str() else { return };
        if !self.cfg.tab_names || title.is_empty() || self.titled.as_deref() == Some(title.as_str()) {
            return;
        }
        self.titled = Some(title.clone());
        let name = if unicode_width::UnicodeWidthStr::width(title.as_str()) > TAB_NAME_MAX {
            format!(
                "{}…",
                render::plain(&render::clip_line(vec![(title.clone(), None)], TAB_NAME_MAX - 1))
            )
        } else {
            title
        };
        let Ok(r) = herdr::call("tab.get", json!({"tab_id": tab})) else {
            return;
        };
        let path = crate::state_dir().join("tab-names.json");
        let mut ours: serde_json::Map<String, Value> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let label = r["tab"]["label"].as_str().unwrap_or("");
        let number = r["tab"]["number"].as_i64().map(|n| n.to_string()).unwrap_or_default();
        if label == name || (label != number && Some(label) != ours.get(tab).and_then(Value::as_str)) {
            return;
        }
        if herdr::call("tab.rename", json!({"tab_id": tab, "label": name})).is_ok() {
            ours.insert(tab.to_owned(), json!(name));
            let _ = std::fs::write(&path, Value::Object(ours).to_string());
        }
    }

    /// 插件的程序换了（连着两次看到同一份新的，免得装到一半就换），而且新程序跑得起来：返回新目录。
    fn code_changed(&mut self) -> Option<PathBuf> {
        let sig = code_sig();
        if sig == self.code || sig.1.is_none() {
            self.code_new = None;
            return None;
        }
        if self.code_new.as_ref() != Some(&sig) {
            self.code_new = Some(sig);
            return None;
        }
        let ok = std::process::Command::new(sig.0.join("bin/statetrail"))
            .arg("version")
            .output()
            .is_ok_and(|o| o.status.success());
        ok.then_some(sig.0)
    }

    fn widths(&self) -> Option<(i64, i64)> {
        let r = herdr::call("pane.layout", json!({"pane_id": self.me})).ok()?;
        let w = |id: &str| r["layout"]["panes"].as_array()?.iter().find(|p| p["pane_id"] == id)?["rect"]["width"].as_i64();
        Some((w(&self.target)?, w(&self.me)?))
    }

    /// 把轨迹调到 cols 列：挪 Claude 和轨迹之间那条线（缩窄对 Claude 做 right，加宽对轨迹做 left，
    /// 反过来会去挤外侧的窗格）。herdr 把比例夹在 0.1–0.9，再窄它会自己夹住。
    fn set_width(&self, cols: i64) {
        let Some((tw, sw)) = self.widths() else { return };
        let total = (tw + sw) as f64;
        if sw > cols {
            let _ = herdr::call(
                "pane.resize",
                json!({"pane_id": self.target, "direction": "right", "amount": (sw - cols) as f64 / total}),
            );
        } else if sw < cols {
            let _ = herdr::call(
                "pane.resize",
                json!({"pane_id": self.me, "direction": "left", "amount": (cols - sw) as f64 / total}),
            );
        }
    }

    /// 拉到窄版（herdr 允许的最窄）或拉回宽版（上次的宽度，至少 width）。画哪一版按窗格实际宽度定。
    fn toggle(&mut self, width: u16) {
        if width >= WIDE_MIN {
            self.last_wide = Some(width as i64);
            self.set_width(1);
        } else if let Some((tw, sw)) = self.widths() {
            let total = tw + sw;
            let cols = (self.last_wide.unwrap_or(0) + FRAME).max((total as f64 * self.cfg.width).round() as i64);
            let cols = cols.min(total - crate::CLAUDE_MIN - FRAME);
            if cols >= WIDE_MIN as i64 + FRAME {
                self.set_width(cols);
            } else {
                self.say(
                    t("放不下宽版：要给 Claude 留 {n} 列").replace("{n}", &crate::CLAUDE_MIN.to_string()),
                    Duration::from_secs(6),
                );
            }
        }
    }

    fn say(&mut self, msg: String, secs: Duration) {
        self.flash = Some((msg, Instant::now() + secs));
    }

    fn save_reg(&self) {
        let _ = std::fs::create_dir_all(crate::run_dir());
        let reg = json!({"sidebar": self.me, "target": self.target, "pid": std::process::id()});
        let _ = std::fs::write(
            crate::run_dir().join(format!("{}.json", self.me.replace(':', "_"))),
            reg.to_string(),
        );
    }

    /// 点了一轮：跳转放在线程里，主线程照常画；新的一次点击让旧的那次停下。
    fn jump(&mut self, idx: usize, tx: &Sender<Msg>) {
        let Some(tr) = &self.tr else { return };
        let stops = tr.visible();
        let st = stops[idx];
        let id = st.id.clone();
        let token = self.jump_token.fetch_add(1, Ordering::SeqCst) + 1;
        let term = HerdrTerm {
            pane: self.target.clone(),
            height: herdr::pane_info(&self.target)
                .and_then(|i| i["scroll"]["viewport_rows"].as_u64())
                .unwrap_or(40) as usize,
        };
        // agent 还在干的那一轮没有最终回复，拿半截的文字去找会一直对不上：最新的东西本来就在最底下
        if idx + 1 == stops.len() && self.busy() {
            term.send(&["C-End"]);
            return;
        }
        if jump::needle(&st.reply).is_empty() {
            self.say(t("轨迹：这一轮 Claude 没写文字回复，没法定位"), Duration::from_secs(6));
            return;
        }
        if self.missing.contains(&id) {
            self.say(
                t("轨迹：这一轮在主界面里已经看不到了（续接的会话只显示最近一段，或被 /compact 清掉）"),
                Duration::from_secs(6),
            );
            return;
        }
        let replies: Vec<String> = stops.iter().map(|s| s.reply.clone()).collect();
        let ids: Vec<String> = stops.iter().map(|s| s.id.clone()).collect();
        let (tx, cur) = (tx.clone(), self.jump_token.clone());
        let (sid, target, n, of) = (self.sid.clone(), self.target.clone(), idx + 1, stops.len());
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let refs: Vec<&str> = replies.iter().map(String::as_str).collect();
            let out = jump::jump(&term, &refs, idx, &|| cur.load(Ordering::SeqCst) != token);
            let result = match &out {
                Outcome::Found => "found".to_owned(),
                Outcome::Cancelled => "cancelled".to_owned(),
                Outcome::Timeout => {
                    let _ = tx.send(Msg::Flash(t("轨迹：这一轮没找着，先停在这儿了"), Duration::from_secs(6)));
                    "timeout".to_owned()
                }
                Outcome::Gone(top) => {
                    let gone: Vec<String> = match top {
                        Some(k) => ids[..*k].to_vec(),
                        None => vec![ids[idx].clone()],
                    };
                    let _ = tx.send(Msg::Jumped(gone.join(","), "gone".into()));
                    let _ = tx.send(Msg::Flash(
                        t("轨迹：主界面里找不到这一轮（续接的会话只显示最近一段，或被 /compact 清掉）"),
                        Duration::from_secs(6),
                    ));
                    "not-found".to_owned()
                }
            };
            if out == Outcome::Found {
                let _ = tx.send(Msg::Jumped(ids[idx].clone(), "found".into()));
            }
            let log = json!({"at": chrono::Local::now().format("%F %T").to_string(), "pane": target, "sid": sid,
                             "turn": n, "of": of, "result": result, "secs": (t0.elapsed().as_secs_f64() * 1000.0).round() / 1000.0});
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(crate::state_dir().join("jump.log"))
            {
                let _ = writeln!(f, "{log}");
            }
        });
    }
}

fn spawn_threads(tx: &Sender<Msg>, target: &str, me: &str) {
    let input = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });
    // 订阅这个 Claude 的状态变化、窗格关闭。pane.updated 不订：不能按窗格过滤，别的插件会一直把这里叫醒
    let events = tx.clone();
    let subs = json!([{"type": "pane.agent_status_changed", "pane_id": target}, {"type": "pane.closed"}, {"type": "pane.exited"}]);
    std::thread::spawn(move || {
        if let Ok(it) = herdr::subscribe(subs) {
            for ev in it {
                if events.send(Msg::Herdr(ev)).is_err() {
                    return;
                }
            }
        }
        let _ = events.send(Msg::HerdrGone);
    });
    let cmds = tx.clone();
    let path = sock_of(me);
    let _ = std::fs::remove_file(&path);
    if let Ok(l) = UnixListener::bind(&path) {
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                for line in BufReader::new(s).lines().map_while(Result::ok) {
                    let _ = cmds.send(Msg::Command(line));
                }
            }
        });
    }
}

/// 画第一帧之前等一下（大小已经对上就不等，最多 0.4 秒）。herdr 在窗格第一次输出之后才把终端调到布局里的大小，
/// 而程序起来后 0.3 秒内就输出的话，这一下会丢，终端一直比边框里少 1 列（实测：不等 3 次都停在 41 列；
/// 推迟 0.3 秒以上，第一帧按 41 画、紧接着就收到改成 42 的信号，10 次都是）。Python 版起得慢，碰不到。
fn wait_for_size(me: &str) {
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        let want = herdr::call("pane.layout", json!({"pane_id": me})).ok().and_then(|r| {
            let rect = r["layout"]["panes"].as_array()?.iter().find(|p| p["pane_id"] == me)?["rect"].clone();
            Some(((rect["width"].as_i64()? - FRAME) as u16, (rect["height"].as_i64()? - FRAME) as u16))
        });
        if want.is_some_and(|w| terminal::size().is_ok_and(|s| s == w)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn run(cfg: Config) {
    // 先让 crossterm 装上窗口大小变化的信号处理：herdr 开出窗格后紧接着调宽度，程序起得快的话那个信号会在
    // 开始读事件之前就来，漏掉了就一直按开出来时的大小画（实测少 1 列）
    let _ = event::poll(Duration::ZERO);
    let (Ok(target), Ok(me)) = (std::env::var("STATETRAIL_TARGET"), std::env::var("HERDR_PANE_ID")) else {
        eprintln!("statetrail pane: STATETRAIL_TARGET / HERDR_PANE_ID not set");
        std::process::exit(2);
    };
    let mut s = Trail {
        target: target.clone(),
        me: me.clone(),
        cfg,
        sid: None,
        cfg_dir: claude::default_cfg(),
        tr: None,
        status: None,
        opened: None,
        expanded: false,
        missing: HashSet::new(),
        flash: None,
        offset: 0,
        follow: true,
        no_claude_since: None,
        jump_token: Arc::new(AtomicUsize::new(0)),
        last_wide: None,
        hits: Vec::new(),
        header_len: 0,
        head_ln: 0,
        mc: (None, (String::new(), String::new())),
        last_kick: None,
        view: (0, 0, 0),
        up_at: None,
        down_at: None,
        footer_top: u16::MAX,
        titled: None,
        code: code_sig(),
        code_new: None,
        restart: None,
    };
    s.save_reg();
    // 右键固定交给 herdr 的菜单：轨迹自己不用右键
    let _ = herdr::call("pane.input.set", json!({"pane_id": me, "right_click": "herdr"}));
    let (tx, rx) = mpsc::channel();
    spawn_threads(&tx, &target, &me);
    s.refresh();
    s.kick_usage(false, &tx);

    wait_for_size(&me);
    let mut out = std::io::stdout();
    let _ = terminal::enable_raw_mode();
    let _ = execute!(out, terminal::EnterAlternateScreen, event::EnableMouseCapture);
    let mut term = Terminal::new(CrosstermBackend::new(out)).expect("terminal");
    let _ = run_loop(&mut s, &mut term, &tx, &rx);
    let _ = execute!(term.backend_mut(), event::DisableMouseCapture, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    s.jump_token.fetch_add(1, Ordering::SeqCst);
    // 原地重启时登记留着：换程序那一下，别让自动打开以为这里没有轨迹、再开一个
    if s.restart.is_none() {
        let _ = std::fs::remove_file(crate::run_dir().join(format!("{}.json", me.replace(':', "_"))));
    }
    let _ = std::fs::remove_file(sock_of(&me));
    if let Some(root) = s.restart {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(root.join("bin/statetrail"))
            .arg("pane")
            .current_dir(&root)
            .exec();
        eprintln!("statetrail: restart failed: {err}");
    }
}

fn run_loop(
    s: &mut Trail,
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    tx: &Sender<Msg>,
    rx: &Receiver<Msg>,
) -> std::io::Result<()> {
    // 预约的补刷时间。刚起来的头两秒多看几次窗口大小：herdr 调宽度的信号可能在装好处理之前就来了
    let start = Instant::now();
    let mut due: Vec<Instant> = [200, 1000, 2000].iter().map(|ms| start + Duration::from_millis(*ms)).collect();
    let mut next_tick = Instant::now() + TICK;
    let mut next_check = Instant::now() + AGENT_CHECK;
    loop {
        // 画一帧
        let size = term.size()?;
        if std::env::var_os("STATETRAIL_DEBUG").is_some() {
            let os = crossterm::terminal::size().unwrap_or((0, 0));
            let line = format!(
                "{} term.size={}x{} crossterm={}x{}\n",
                chrono::Local::now().format("%T%.3f"),
                size.width,
                size.height,
                os.0,
                os.1
            );
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(crate::state_dir().join("debug.log"))
                .and_then(|mut f| f.write_all(line.as_bytes()));
        }
        let narrow = size.width < WIDE_MIN;
        if s.flash.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
            s.flash = None;
        }
        {
            let (model, ctx) = s.model_ctx();
            let usage = claude::read_usage(&s.cfg_dir, s.sid.as_deref(), s.cfg.live_usage);
            let stops = s.tr.as_ref().map(|t| t.visible()).unwrap_or_default();
            let view = ui::View {
                stops: &stops,
                narrow,
                expanded: s.expanded,
                opened: s.opened.as_deref(),
                busy: s.busy(),
                missing: &s.missing,
            };
            let items = view.items();
            let room = size.width.saturating_sub(2) as usize;
            let mut info = header::Info {
                cfg_dir: &s.cfg_dir,
                model: &model,
                ctx: &ctx,
                usage: usage.as_ref(),
                found: s.tr.is_some(),
                turns: stops.len(),
                gone: view.gone(),
                flash: s.flash.as_ref().map(|(m, _)| m.as_str()),
                up_tag: "",
                slot: 0,
            };
            let head_len = if narrow {
                header::narrow(size.width as usize, &info).len()
            } else {
                header::wide(size.width as usize, &info).0.len()
            };
            // 列表露出来的范围决定 ▲ / ▼；窄版的 ▼ 要多一行底部，列表就少一行，所以最多算两遍
            let mut foot_len = 1;
            let (mut slot, mut counts);
            loop {
                let rows = (size.height as usize).saturating_sub(head_len + 1 + 1 + foot_len);
                let bottom = ui::bottom_offset(&view, &items, room, rows);
                if s.follow {
                    s.offset = bottom;
                }
                s.offset = s.offset.min(bottom);
                counts = ui::view_counts(&view, &ui::visible(&view, &items, s.offset, rows, room));
                slot = if counts.0 > 0 || counts.1 > 0 {
                    unicode_width::UnicodeWidthStr::width(format!("▼{}{}", if narrow { "" } else { " " }, stops.len()).as_str())
                } else {
                    0
                };
                let want = if narrow && slot > 0 { 2 } else { 1 };
                if want == foot_len {
                    break;
                }
                foot_len = want;
            }
            s.view = counts;
            let tag = |arrow: &str, n: usize| {
                if n > 0 {
                    format!("{arrow}{}{n}", if narrow { "" } else { " " })
                } else {
                    String::new()
                }
            };
            let up_tag = tag("▲", counts.0);
            info.up_tag = &up_tag;
            info.slot = slot;
            let head = if narrow {
                header::narrow(size.width as usize, &info)
            } else {
                let (lines, head_ln) = header::wide(size.width as usize, &info);
                s.head_ln = head_ln;
                lines
            };
            s.up_at = (slot > 0 && counts.0 > 0).then_some((head.len(), slot));
            // 底部：宽版一行（第 4 步放按钮，▼ 在行首）；窄版三个按钮就占满了，▼ 在第二行
            let down_tag = tag("▼", counts.1);
            let mut footer: Vec<render::SLine> = vec![Vec::new()];
            if foot_len == 2 {
                footer.push(Vec::new());
            }
            let at = footer.len() - 1;
            footer[at] = render::lead(std::mem::take(&mut footer[at]), &down_tag, slot);
            s.down_at = (slot > 0 && counts.1 > 0).then_some((at, slot));
            let mut drawn = None;
            term.draw(|f| drawn = Some(ui::draw(f, &head, &footer, &view, s.offset)))?;
            if let Some(d) = drawn {
                s.hits = d.hits;
                s.header_len = d.list_top.saturating_sub(1);
                s.footer_top = d.footer_top;
            }
        }

        // 等下一件事
        let now = Instant::now();
        let mut wake = next_tick.min(next_check);
        if let Some(d) = due.iter().min() {
            wake = wake.min(*d);
        }
        if let Some((_, until)) = &s.flash {
            wake = wake.min(*until);
        }
        let msg = rx.recv_timeout(wake.saturating_duration_since(now));
        let mut stale = false;
        match msg {
            Ok(Msg::Input(Event::Mouse(m))) => match {
                if std::env::var_os("STATETRAIL_DEBUG").is_some() && !matches!(m.kind, MouseEventKind::Moved) {
                    let line = format!(
                        "{} mouse {:?} row={} col={} header_len={}\n",
                        chrono::Local::now().format("%T%.3f"),
                        m.kind,
                        m.row,
                        m.column,
                        s.header_len
                    );
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(crate::state_dir().join("debug.log"))
                        .and_then(|mut f| f.write_all(line.as_bytes()));
                }
                m.kind
            } {
                MouseEventKind::ScrollUp => {
                    s.offset = s.offset.saturating_sub(WHEEL);
                    s.follow = false;
                }
                MouseEventKind::ScrollDown => {
                    s.offset += WHEEL; // 画的时候按最底夹住；到底了就重新跟着走
                    s.follow = false;
                    let tr_len = s.tr.as_ref().map_or(0, |t| t.visible().len());
                    if s.offset >= tr_len {
                        s.follow = true;
                    }
                }
                MouseEventKind::Down(MouseButton::Left) if m.row >= s.footer_top => {
                    let (row, col) = ((m.row - s.footer_top) as usize, (m.column as usize).saturating_sub(1));
                    if s.down_at.is_some_and(|(r, slot)| r == row && col <= slot + 1) {
                        s.page(1);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) if m.row < s.header_len => {
                    // 表头的字从第 3 列开始（左边 2 列和列表的指针对齐）；列号从 1 数
                    s.header_click(m.row as usize + 1, (m.column as usize).saturating_sub(1), size.width, tx);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(&(_, it)) = s.hits.iter().find(|(row, _)| *row == m.row) {
                        match it {
                            Item::Fold => s.expanded = !s.expanded,
                            Item::Stop(i) => {
                                let id = s.tr.as_ref().map(|t| t.visible()[i].id.clone());
                                s.opened = id;
                                s.jump(i, tx);
                            }
                        }
                    }
                }
                _ => {}
            },
            Ok(Msg::Input(Event::Resize(..))) => {}
            Ok(Msg::Input(_)) => continue,
            Ok(Msg::Herdr(ev)) => {
                let name = ev["event"].as_str().unwrap_or("").replace('.', "_");
                let pane = ev["data"]["pane_id"].as_str().unwrap_or("");
                if matches!(name.as_str(), "pane_closed" | "pane_exited") && pane == s.target {
                    return Ok(());
                }
                if name == "pane_agent_status_changed" {
                    stale = true;
                    due.extend([now + Duration::from_millis(500), now + Duration::from_millis(1500)]);
                }
                if name.is_empty() && ev.get("error").is_some() {
                    stale = true;
                }
            }
            Ok(Msg::HerdrGone) => return Ok(()), // herdr server 没了
            Ok(Msg::Command(c)) => {
                if c.trim() == "toggle" {
                    s.toggle(size.width);
                }
            }
            Ok(Msg::Flash(m, d)) => s.say(m, d),
            Ok(Msg::Wake) => {}
            Ok(Msg::Jumped(ids, how)) => {
                for id in ids.split(',') {
                    if how == "found" {
                        s.missing.remove(id);
                    } else {
                        s.missing.insert(id.to_owned());
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + TICK;
            stale = true;
            s.kick_usage(false, tx); // 没有事件时醒来：额度旧了就去查（默认 30 分钟）
        }
        if now >= next_check {
            next_check = now + AGENT_CHECK;
            if let Some(root) = s.code_changed() {
                s.restart = Some(root);
                return Ok(()); // 插件更新了：收掉界面，原地换成新程序（run 里 exec）
            }
            if s.no_claude_since.is_none() && herdr::pane_info(&s.target).is_none_or(|i| i["agent"].as_str() != Some("claude")) {
                stale = true;
            }
        }
        if due.iter().any(|d| *d <= now) {
            due.retain(|d| *d > now);
            stale = true;
        }
        if stale {
            if !s.refresh() {
                return Ok(());
            }
            if s.no_claude_since.is_some_and(|t0| t0.elapsed() > GONE_GRACE) {
                return Ok(()); // 这一格里已经没有 Claude 了：真正关掉
            }
            if s.no_claude_since.is_some() {
                due.push(now + GONE_GRACE + Duration::from_millis(500));
            }
        }
    }
}
