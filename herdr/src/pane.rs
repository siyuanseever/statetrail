//! 轨迹窗格本身（`statetrail pane`）。主线程只在一个通道上等：键盘鼠标、herdr 事件、别的进程发来的命令（toggle）、
//! 跳转线程的结果各有一个线程往里送，空闲时不轮询、不醒。对应 Python 版的 Sidebar.run。

use crossterm::event::{self, Event, MouseButton, MouseEventKind};
use crossterm::{execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::header;
use crate::i18n::t;
use crate::chapters::{self, Chapter, Marks};
use crate::jump::{self, Outcome, Term};
use crate::render;
use crate::switch;
use crate::tools::{self, Plugin, sh_quote};
use crate::transcript::Transcript;
use crate::ui::{self, Item};
use crate::{Config, FRAME, WIDE_MIN, claude, herdr};

const TICK: Duration = Duration::from_secs(60); // 没有事件时多久醒一次
// 焦点在这一对 Claude / 轨迹上时，多久读一次 Claude 的屏幕、看它停在哪一轮（用户 2026-10-28：「根据我在 Claude Code 里
// 查看的历史消息或者最新底部，更新右侧轨迹的选中」）。别的轨迹不读：一次读屏约 4KB、走 socket 不到 1ms
const FOLLOW_EVERY: Duration = Duration::from_millis(500);
const CLASSIFY_RETRY: Duration = Duration::from_secs(600); // 调 Haiku 分章失败了（没网、没额度）：隔这么久再试
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
    Jumped(String, String),            // (轮次 id, 结果)
    Wake,                              // 后台查完额度了：重画
    PluginTab(String, String, String), // (插件, 动作, 它开出来的标签页)
    Classified(bool),                  // 后台分完一批章（成功没有）：重读分章文件
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
    pair_focused: bool,          // herdr 的焦点在这个 Claude 或这个轨迹上：只有这时才跟着 Claude 的屏幕走
    last_screen: u64,            // 上次读到的 Claude 屏幕（哈希）：没变就不用再认
    jumping_until: Option<Instant>, // 点了一轮、正在替 Claude 翻屏：这期间不跟，免得和翻屏打架
    reveal: bool,                // 选中的那一轮刚被跟着换了：画的时候把它滚进列表的可见范围
    marks: Marks,                // 这个会话哪几轮开新章（chapters.rs）
    chaps: Vec<Chapter>,         // 上一帧切出来的章（点击时按它认）
    items: Vec<Item>,            // 上一帧列表里的项（滚轮按它数轮次）
    bottom: usize,               // 上一帧停在最底时的 offset
    chap_open: Vec<bool>,        // 上一帧每一章展没展开
    chap_fold: HashMap<String, bool>, // 你点过的章（按开头那一轮的 id）：展开 / 收起；没点过的展开
    classifying: bool,           // 正在后台调 Haiku 分章
    next_classify: Instant,
    no_claude_since: Option<Instant>,
    jump_token: Arc<AtomicUsize>,
    last_wide: Option<i64>,
    hits: Vec<(u16, Item)>,
    header_len: u16,
    head_ln: usize,                                // 宽版「N 轮」那一行（点它什么都不做）
    hot: Vec<(usize, usize, usize)>, // 表头里点了弹账号菜单的那段字（header::hot_spots）
    menu: Option<ui::Menu>,                        // 开着的下拉菜单
    menu_hits: Vec<(u16, u16, u16, String)>,
    hover: Option<(u16, u16)>, // 鼠标停在哪（行, 列）：能点的字、菜单项停上去就亮起来
    mc: (Option<u64>, (String, String)), // (对话记录大小, (模型, 上下文))：没变就不重读尾巴
    // 刚换了模型 / effort：(换之前对话记录里的模型, 换成的)。对话记录要等下一条回复才写新模型，在那之前表头先显示换成的
    last_kick: Option<Instant>,
    view: (usize, usize, usize),     // (列表上面还有几轮, 下面还有几轮, 露出来几项)
    up_at: Option<(usize, usize)>,   // 表头里的 ▲：(第几行, 占到第几列)
    down_at: Option<(usize, usize)>, // 底部的 ▼：(第几行, 占到第几列)
    footer_top: u16,
    titled: Option<String>, // 上次拿来给 tab 起名的会话名
    code: CodeSig,          // 启动时的程序
    code_new: Option<CodeSig>,
    restart: Option<PathBuf>,      // 插件更新了：主循环退出后在这一格原地换成这个目录里的程序
    tool: Option<(String, usize)>, // 开着的工具：(窗格, 第几个按钮)
    tool_check: Option<Instant>,   // 焦点离开了工具：到这个时候再看一眼，还不在它上面就关
    tool_sticky: bool,             // 这个工具点外面不关（命令行）
    plugins: Vec<Plugin>,
    plugins_mtime: Option<std::time::SystemTime>,
    plugin_tabs: HashMap<(String, String), String>, // (插件, 动作) → 它开出来的标签页：再点直接切过去
    foot_rows: Vec<Vec<header::Button>>,
    foot_shift: (usize, usize), // (▼ 在底部第几行, 那一行的按钮往右挪了几列)
}

type CodeSig = (PathBuf, Option<std::time::SystemTime>);

/// herdr 的插件登记：装、卸、开、关插件都会写它。
fn registry_path() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .and_then(|c| c.ancestors().nth(3).map(|h| h.join("plugins.json")))
        .unwrap_or_else(|| crate::home().join(".config/herdr/plugins.json"))
}

/// 这个程序所在的插件目录（bin/statetrail 的上一级）。
fn plugin_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent().map(PathBuf::from))
        .unwrap_or_default()
}

/// (herdr 登记的插件目录, 那里 bin/statetrail 的修改时间)。herdr plugin update 会装进一个新目录，旧目录里的文件不变，
/// 所以要看 herdr 的登记（plugins.json）；本地 link 的目录换了新编译的程序，修改时间会变。
fn code_sig() -> CodeSig {
    let here = plugin_root();
    let root = std::fs::read(registry_path())
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
        if info["focused"].as_bool() == Some(true) {
            self.pair_focused = true;
        }
        if info["agent"].as_str() == Some("claude") {
            if self.no_claude_since.take().is_some() && self.sid.is_some() {
                // Claude 退出又起来了（比如换账号 --resume 同一个会话）：会话 id 可能没变，配置目录要重认
                self.cfg_dir = claude::config_dir_of_pid(herdr::claude_pid(&self.target));
            }
        } else if self.no_claude_since.is_none() {
            self.no_claude_since = Some(Instant::now());
        }
        let sid = herdr::session_id(&self.target, &info);
        if sid != self.sid {
            // /clear、/resume 换了会话
            self.sid = sid;
            self.tr = None;
            self.opened = None;
            self.expanded = false;
            self.missing.clear();
            self.follow = true;
            self.chap_fold.clear();
            self.marks = self.sid.as_deref().map(Marks::load).unwrap_or_default();
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
            "usage" => {
                self.kick_usage(true, tx);
                self.plugins = tools::load_plugins(&self.cfg); // 刚装 / 刚卸的插件，点一下 ⟳ 底部就跟上
            }
            "plugmenu" => self.plugin_menu(width),
            a if a.starts_with("plug:") => {
                if let Some(p) = a[5..].parse::<usize>().ok().and_then(|i| self.plugins.get(i)) {
                    let (id, action) = (p.id.clone(), p.action.clone());
                    self.run_plugin(&id, &action, tx);
                }
            }
            a if a.starts_with("tool:") => {
                if let Ok(i) = a[5..].parse::<usize>() {
                    self.run_tool(i);
                }
            }
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
    /// 鼠标底下是哪一段能点的字 / 哪一项菜单；没变就不用重画
    fn hover_key(&self, (row, col): (u16, u16)) -> Option<(u16, u16)> {
        if self.menu.is_some() {
            return self
                .menu_hits
                .iter()
                .find(|(r, a, b, _)| *r == row && (*a..*b).contains(&col))
                .map(|h| (h.0, h.1));
        }
        let col = (col as usize).checked_sub(2)?; // 表头从第 3 列画起
        self.hot
            .iter()
            .find(|(i, a, b)| *i == row as usize && (*a..*b).contains(&col))
            .map(|h| (h.0 as u16, h.1 as u16))
    }

    fn header_click(&mut self, ln: usize, col: usize, width: u16, tx: &Sender<Msg>) {
        let narrow = width < WIDE_MIN;
        if self.up_at.is_some_and(|(l, slot)| l == ln && col <= slot + 1) {
            self.page(-1); // 点在行首 ▲ 那一段（偏一格也算）
            return;
        }
        if ln == 1 {
            let buttons = if narrow { header::narrow_buttons() } else { header::wide_buttons() };
            if let Some(a) = header::button_at(&buttons, col).map(str::to_owned) {
                self.act(&a, width, tx);
            }
        } else if let Some(&(i, start, _)) = self
            .hot
            .iter()
            .find(|(i, a, b)| *i + 1 == ln && (*a..*b).contains(&col.saturating_sub(1)))
        {
            self.open_menu((start + 2) as u16, (i + 1) as u16);
        } else if narrow || ln != self.head_ln {
            self.kick_usage(true, tx);
        }
    }

    /// 点 ▲ / ▼：翻一屏的轮数，少翻一项留一轮重叠（边上那一轮常常只露出半截）。
    /// 读一次 Claude 的屏幕，看它停在哪一轮，把选中挪过去。在最底就是最新一轮；往上翻着就是屏幕上最上面露出开头的那一轮，
    /// 它的开头已经在屏幕下半截时算上一轮（你看的多半是上一轮的结尾）；一轮的开头都没露出来（长回复的中段）就不动。
    /// 选中换了返回 true。
    fn follow_screen(&mut self) -> bool {
        if self.jumping_until.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        let Some(tr) = &self.tr else { return false };
        let screen = HerdrTerm { pane: self.target.clone(), height: 0 }.screen();
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            screen.hash(&mut h);
            h.finish()
        };
        if screen.is_empty() || std::mem::replace(&mut self.last_screen, hash) == hash {
            return false;
        }
        let stops = tr.visible();
        let Some(last) = stops.len().checked_sub(1) else { return false };
        let idx = if jump::at_bottom(&screen) {
            Some(last)
        } else {
            let replies: Vec<&str> = stops.iter().map(|s| s.reply.as_str()).collect();
            let half = screen.lines().count() / 2;
            jump::located_rows(&screen, &jump::needles_of(&replies))
                .into_iter()
                .min_by_key(|&(_, row)| row)
                .map(|(i, row)| if row > half && i > 0 { i - 1 } else { i })
        };
        let Some(i) = idx else { return false };
        if self.opened.as_deref() == Some(stops[i].id.as_str()) {
            return false;
        }
        self.opened = Some(stops[i].id.clone());
        self.reveal = true;
        true
    }

    /// 滚轮：按轮次数，翻 WHEEL 轮，章名不占步数（用户 2026-10-29：「滑动就要在对应的 conversation 级别」「章节只是一个辅助作用」）。
    /// 停在一章的第一轮上时把章名也露出来。
    fn wheel(&mut self, down: bool) {
        let (mut k, mut n) = (self.offset.min(self.items.len().saturating_sub(1)), 0);
        while n < WHEEL && if down { k + 1 < self.items.len() } else { k > 0 } {
            k = if down { k + 1 } else { k - 1 };
            n += usize::from(matches!(self.items[k], Item::Stop(_)));
        }
        if k > 0 && matches!(self.items[k - 1], Item::Chapter(_)) && matches!(self.items[k], Item::Stop(_)) {
            k -= 1;
        }
        self.offset = k;
        self.follow = down && k >= self.bottom; // 到底了就重新跟着走
    }

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

    /// 底部：自己的按钮，后面（│ 隔开）是其他 herdr 插件的按钮，放不下的收进 ⋯。▼ 放在按钮行最前面；窄版放不下
    /// （三个按钮就占了 13 列里的 12 列），▼ 放在第二行最前面（插件那一行，没插件就单独一行）。工具开着时底部不变：
    /// 不高亮、不加关闭按钮，再点一次同一个按钮就关（用户定的）。和 Python 版 footer 一样。
    fn footer_rows(&self, narrow: bool, room: usize, slot: usize) -> (Vec<Vec<header::Button>>, usize) {
        let shift = if slot > 0 { slot + 1 } else { 0 };
        let tools: Vec<header::Button> = self
            .cfg
            .buttons
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let ic = header::button_icon(&b.icon);
                (if ic.is_empty() { t(&b.label) } else { ic }, format!("tool:{i}"), 0)
            })
            .collect();
        // 第一层只放有图标的插件；icons = "text"（全写成字）时照样写名字。没图标的只在 ⋯ 里（按名字列出）
        let text = header::text_tier();
        let plug: Vec<header::Button> = self
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.icon.is_empty() || text)
            .map(|(i, p)| {
                (
                    if p.icon.is_empty() { p.label.clone() } else { p.icon.clone() },
                    format!("plug:{i}"),
                    0,
                )
            })
            .collect();
        let hidden = plug.len() < self.plugins.len();
        let room = room.saturating_sub(shift);
        let fit = |head: Vec<header::Button>| -> Vec<header::Button> {
            let more = (header::icon("more"), "plugmenu".to_owned(), 0);
            let mut all = head.clone();
            all.extend(plug.iter().cloned());
            if !hidden && header::cells_w(&all) <= room {
                return all;
            }
            let mut keep = head;
            for c in &plug {
                let mut trial = keep.clone();
                trial.extend([c.clone(), more.clone()]);
                if header::cells_w(&trial) > room {
                    break;
                }
                keep.push(c.clone());
            }
            keep.push(more);
            keep
        };
        if narrow {
            // 第一行放得下几个工具就放几个（默认四个只放得下三个），剩下的挪到第二行，排在插件前面
            let full = room + shift;
            let mut first: Vec<header::Button> = Vec::new();
            let mut rest: Vec<header::Button> = Vec::new();
            for b in tools {
                let mut trial = first.clone();
                trial.push(b.clone());
                if rest.is_empty() && (first.is_empty() || header::cells_w(&trial) <= full) {
                    first = trial;
                } else {
                    rest.push(b);
                }
            }
            let mut rows = vec![first];
            if !self.plugins.is_empty() {
                rows.push(fit(rest));
            } else if !rest.is_empty() || slot > 0 {
                rows.push(rest);
            }
            let at = rows.len() - 1;
            (rows, at)
        } else if self.plugins.is_empty() {
            (vec![tools], 0)
        } else {
            let mut head = tools;
            if !head.is_empty() {
                head.push(("│".into(), String::new(), 0));
            }
            (vec![fit(head)], 0)
        }
    }

    /// 配置里的按钮。command：在 Claude 那一格的目录里开本插件的 tool 窗格，经 sh -c 跑它；
    /// action：调别的 herdr 插件的动作，先把焦点交回 Claude（点轨迹时焦点到了轨迹上，对方插件按焦点干活）。
    fn run_tool(&mut self, index: usize) {
        let Some(button) = self.cfg.buttons.get(index).cloned() else {
            return;
        };
        if button.command.is_empty() {
            if let Some((plugin, action)) = button.action.rsplit_once('.') {
                let _ = herdr::call("pane.focus", json!({"pane_id": self.target}));
                let r = herdr::call(
                    "plugin.action.invoke",
                    json!({"plugin_id": plugin, "action_id": action,
                           "context": {"focused_pane_id": self.target, "invocation_source": crate::plugin_id()}}),
                );
                if let Err(e) = r {
                    self.say(format!("{}: {e}", button.action), Duration::from_secs(6));
                }
            }
            return;
        }
        if let Some((_, open)) = self.tool {
            // 再点同一个按钮 = 关；点另一个 = 换过去
            self.close_tool(true);
            if open == index {
                return;
            }
        }
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut cmd = button.command.clone();
        let first = cmd.split_whitespace().next().unwrap_or("").trim_matches(['\'', '"']);
        let prog = first.replace("${SHELL}", &shell).replace("$SHELL", &shell); // 命令行按钮的 "$SHELL"：你的登录 shell
        // 没装：照常在下面开工具窗格，里面写着安装命令，回车就装，装完直接打开（以前只在表头闪一下提示，像是没反应）
        let missing = !tools::which(&prog);
        let want = herdr::pane_info(&self.target)
            .and_then(|i| i["foreground_cwd"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let cwd = if want.is_empty() { String::new() } else { tools::live_dir(&want) };
        if !want.is_empty() && cwd != want {
            // 不然 cd 失败，窗格一闪就关，什么也看不到（Python 版用户 2026-10-25 报的）
            let home = crate::home().to_string_lossy().into_owned();
            let short = match cwd.strip_prefix(&home) {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
                _ => cwd.clone(),
            };
            self.say(
                t("Claude 的目录已经不在了，改在 {d} 打开").replace("{d}", &short),
                Duration::from_secs(8),
            );
        }
        let lazygit = prog == "lazygit" && !missing;
        if button.pick_repo && lazygit && !cwd.is_empty() && tools::git_root(&cwd).is_none() {
            // 不在仓库里：下面只有一个仓库就直接开它，好几个就先让你点一个（statetrail pickrepo），一个都没有就只提示
            let repos = tools::find_repos(std::path::Path::new(&cwd), 2);
            match repos.len() {
                0 => {
                    self.say(t("这里没有 Git 仓库"), Duration::from_secs(6));
                    return;
                }
                1 => cmd = format!("exec lazygit -p {}", sh_quote(&repos[0].to_string_lossy())),
                _ => {
                    let file = self.run_file("repos");
                    let list: String = repos.iter().map(|r| format!("{}\n", r.display())).collect();
                    let _ = std::fs::write(&file, list);
                    let exe = std::env::current_exe().unwrap_or_default();
                    cmd = format!(
                        "exec {} pickrepo {}",
                        sh_quote(&exe.to_string_lossy()),
                        sh_quote(&file.to_string_lossy())
                    );
                }
            }
        }
        if missing {
            cmd = install_prompt(&prog, &cmd);
        }
        let mut env = json!({"STATETRAIL_CWD": cwd, "STATETRAIL_CMD": cmd,
                             "PATH": std::env::var("PATH").unwrap_or_default(), "SHELL": shell});
        if lazygit {
            // 在最外层按 Esc 也退出，和 yazi 一样（叠加在你自己的配置后面）
            let dir = std::process::Command::new("lazygit")
                .arg("--print-config-dir")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .unwrap_or_default();
            let mine = PathBuf::from(if dir.is_empty() { "/nonexistent" } else { &dir }).join("config.yml");
            let ours = plugin_root().join("tools/lazygit.yml");
            let mut files: Vec<String> = Vec::new();
            if mine.exists() {
                files.push(mine.to_string_lossy().into_owned());
            }
            files.push(ours.to_string_lossy().into_owned());
            env["LG_CONFIG_FILE"] = json!(files.join(","));
        }
        // 从 Claude 下面切出来，占 Claude 那一格高度的 height：Claude 宽度不变、只是变矮，轨迹一直露在旁边能点。
        // 没用的两条路：herdr 的 overlay 会盖满整个标签页，连轨迹一起盖住；popup 只能居中，开着时整个会话只认它，
        // 轨迹点不了、点外面也不关（Python 版 2026-10-25 实测）。
        let r = herdr::call(
            "plugin.pane.open",
            json!({"plugin_id": crate::plugin_id(), "entrypoint": "tool", "placement": "split",
                   "target_pane_id": self.target, "direction": "down", "focus": true, "env": env}),
        );
        let pane = match r.as_ref().map(|r| r["plugin_pane"]["pane"]["pane_id"].as_str()) {
            Ok(Some(p)) => p.to_owned(),
            Ok(None) => {
                self.say(format!("{prog}: no pane"), Duration::from_secs(6));
                return;
            }
            Err(e) => {
                self.say(format!("{prog}: {e}"), Duration::from_secs(6));
                return;
            }
        };
        // 切出来是对半分；挪中间那条线：工具要高就把工具往上推，要矮就把 Claude 往下推（只挪两格之间那条线）
        let h = button.height.unwrap_or(self.cfg.tool_height).clamp(0.1, 0.9);
        if h > 0.5 {
            let _ = herdr::call("pane.resize", json!({"pane_id": pane, "direction": "up", "amount": h - 0.5}));
        } else if h < 0.5 {
            let _ = herdr::call(
                "pane.resize",
                json!({"pane_id": self.target, "direction": "down", "amount": 0.5 - h}),
            );
        } else {
            // 正好对半分也把线挪一下再挪回来：plugin.pane.open 切出来不会马上改 Claude 终端的行数（herdr 0.9.5，
            // 自带的 prefix + - 会），Claude 还按原来的高度画、只露出上半截看不见输入框，关掉时才一下子整屏重画，
            // 就是「关的时候闪一下」（用户 2026-10-26 报的，命令行按钮没写 height，正好走这里）。挪线会让 herdr 重算尺寸。
            for d in ["up", "down"] {
                let _ = herdr::call("pane.resize", json!({"pane_id": pane, "direction": d, "amount": 0.01}));
            }
        }
        self.tool = Some((pane, index));
        self.tool_check = None;
        self.tool_sticky = !button.close_on_blur;
    }

    /// 关掉开着的工具；Claude 那一格自己长回原来的高度，焦点还给 Claude。
    fn close_tool(&mut self, focus: bool) {
        if let Some((pane, _)) = self.tool.take() {
            let _ = herdr::call("pane.close", json!({"pane_id": pane}));
            if focus {
                let _ = herdr::call("pane.focus", json!({"pane_id": self.target}));
            }
        }
        self.tool_check = None;
    }

    /// 焦点离开工具 0.4 秒后：焦点（这个 tab 里）还不在工具上就关（照 tmux 版，免得一闪而过的焦点变化把它关了）。
    fn check_tool(&mut self) {
        self.tool_check = None;
        let Some((pane, _)) = &self.tool else { return };
        let tab = herdr::pane_info(&self.target).and_then(|i| i["tab_id"].as_str().map(str::to_owned));
        let focused = herdr::call("pane.list", json!({})).ok().and_then(|r| {
            r["panes"]
                .as_array()?
                .iter()
                .find(|p| p["focused"].as_bool() == Some(true) && p["tab_id"].as_str() == tab.as_deref())
                .and_then(|p| p["pane_id"].as_str().map(str::to_owned))
        });
        if focused.as_deref() != Some(pane.as_str()) {
            self.close_tool(true);
        }
    }

    /// 调别的插件的 action：先把焦点交回 Claude（对方插件按焦点干活，比如 zoetrope 画的是焦点所在那个 agent 的会话）。
    /// 插件要是开了一个新标签页，就切过去并记下来：再点同一个按钮直接切到那一页，不再开第二个。只开窗格、不开标签页的
    /// 插件（比如 sidebar）不记，照旧每次都调。
    fn run_plugin(&mut self, plugin: &str, action: &str, tx: &Sender<Msg>) {
        let key = (plugin.to_owned(), action.to_owned());
        let tabs = || -> Vec<String> {
            herdr::call("tab.list", json!({}))
                .ok()
                .and_then(|r| {
                    r["tabs"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|t| t["tab_id"].as_str().map(str::to_owned)).collect())
                })
                .unwrap_or_default()
        };
        let before = tabs();
        if let Some(tab) = self.plugin_tabs.get(&key) {
            if before.contains(tab) {
                let _ = herdr::call("tab.focus", json!({"tab_id": tab}));
                return;
            }
            self.plugin_tabs.remove(&key); // 那一页已经被你关了：重新开
        }
        let _ = herdr::call("pane.focus", json!({"pane_id": self.target}));
        let r = herdr::call(
            "plugin.action.invoke",
            json!({"plugin_id": plugin, "action_id": action,
                   "context": {"focused_pane_id": self.target, "invocation_source": crate::plugin_id()}}),
        );
        if let Err(e) = r {
            self.say(format!("{plugin}: {e}"), Duration::from_secs(6));
            return;
        }
        let tx = tx.clone();
        std::thread::spawn(move || {
            // 插件的命令是另起进程跑的，新标签页过一会儿才出现：最多看 3 秒
            for _ in 0..30 {
                std::thread::sleep(Duration::from_millis(100));
                if let Some(new) = tabs().into_iter().find(|t| !before.contains(t)) {
                    let _ = herdr::call("tab.focus", json!({"tab_id": new}));
                    let _ = tx.send(Msg::PluginTab(key.0, key.1, new));
                    return;
                }
            }
        });
    }

    /// ⋯：用 herdr 的 popup 列出所有插件的所有 action（statetrail menu），选中的那一条经 socket 送回来再调。
    /// popup 开着时整个会话只认它，所以不在 popup 里直接调：等它关了，这边再调。
    /// 点了表头的账号名（连 ⇄）：在那段字正下方（x, y）画一个下拉菜单，列出所有账号。
    /// 模型、effort、ultracode 只显示不能点：只改这个会话得替你在 Claude 里开 /model 选择框按键，屏幕会跟着动，
    /// Claude 在跑时还按不了（用户 2026-10-27：「如果我们做的体验不好就不要做可交互了」）。
    fn open_menu(&mut self, x: u16, y: u16) {
        let items = switch::accounts()
            .iter()
            .map(|a| (format!("acct {}", a.name), a.name.clone(), a.dir == self.cfg_dir))
            .collect();
        self.menu = Some(ui::Menu { x, y, items });
    }

    /// 后台让 Haiku 判断还没判过的话哪几句开新章（只在你看着这个 Claude 时做：没看的会话不花额度）。
    fn classify(&mut self, tx: &Sender<Msg>) {
        if self.cfg.chapters != "auto" || self.classifying || Instant::now() < self.next_classify {
            return;
        }
        let (Some(sid), Some(tr)) = (self.sid.clone(), &self.tr) else { return };
        let stops = tr.visible();
        let new: Vec<usize> = self.marks.pending(&stops).into_iter().take(chapters::BATCH).collect();
        if new.is_empty() {
            return;
        }
        let Some(lock) = chapters::Lock::take() else {
            self.next_classify = Instant::now() + Duration::from_secs(5); // 别的轨迹在调：等一会儿
            return;
        };
        let input = chapters::request(&self.marks, &stops, &new);
        let ids: Vec<(usize, String)> = new.iter().map(|&i| (i, stops[i].id.clone())).collect();
        let (cfg_dir, tx) = (self.cfg_dir.clone(), tx.clone());
        self.classifying = true;
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let r = chapters::ask(&cfg_dir, &input).and_then(|out| chapters::parse(&out, &new));
            chapters::log(&sid, new.len(), &r, t0.elapsed().as_secs_f64());
            if let Ok(found) = &r {
                chapters::merge(&sid, found, &ids);
            }
            drop(lock);
            let _ = tx.send(Msg::Classified(r.is_ok()));
        });
    }

    /// 在一轮 / 一章的章名上右键：从这一轮开新章，或者把这一章并回上一章。菜单文字不超过 11 列：细条只有 17 列宽，框和边距占 6 列
    fn chapter_menu(&mut self, it: Item, x: u16, y: u16) {
        let Some(tr) = &self.tr else { return };
        let stops = tr.visible();
        let (i, starts) = match it {
            Item::Chapter(ci) => match self.chaps.get(ci) {
                Some(c) => (c.start, true),
                None => return,
            },
            Item::Stop(i) => (i, self.chaps.iter().any(|c| c.start == i)),
            Item::Fold => return,
        };
        if i == 0 || i >= stops.len() {
            return; // 第一轮总是第一章的开头
        }
        let (key, label) = if starts {
            (format!("chap join {}", stops[i].id), t("并回上一章"))
        } else {
            (format!("chap new {}", stops[i].id), t("这里开新章"))
        };
        self.menu = Some(ui::Menu { x, y, items: vec![(key, label, false)] });
    }

    /// 换账号：要等 Claude 退出重起，几秒到几十秒，放到后台线程里做，进展和结果在表头闪出来。
    fn apply_setting(&mut self, kind: &str, value: &str, tx: &Sender<Msg>) {
        if kind == "chap" {
            // 手动标的记进分章文件，盖过 Haiku 的判断（重新读一遍再写：另一个轨迹可能刚写过）
            if let (Some(sid), Some((how, id))) = (self.sid.clone(), value.split_once(' ')) {
                let mut m = Marks::load(&sid);
                m.manual.insert(id.to_owned(), how == "new");
                m.save(&sid);
                self.marks = m;
            }
            return;
        }
        if kind != "acct" {
            return;
        }
        let (target, value, tx) = (self.target.clone(), value.to_owned(), tx.clone());
        self.say(t("正在换…"), Duration::from_secs(30));
        std::thread::spawn(move || {
            let say = |m: String, secs: u64| {
                let _ = tx.send(Msg::Flash(m, Duration::from_secs(secs)));
            };
            let accts = switch::accounts();
            let r = match accts.iter().find(|a| a.name == value) {
                Some(to) => {
                    let opts = switch::Opts {
                        dry: false,
                        stop_bg: false,
                    };
                    let (st, msg) = switch::switch_one(&target, to, &accts, &opts, &|m| say(m, 60), false);
                    if st == switch::State::Ok { Ok(msg) } else { Err(msg) }
                }
                None => Err(value.clone()),
            };
            match r {
                Ok(m) => say(m, 8),
                Err(m) => say(m, 10),
            }
        });
    }

    /// ⋯：在轨迹里就地弹一个下拉菜单（和模型 / 账号那几个一样），贴着底部按钮、靠右。
    /// 只列底部没放下的插件，一个插件一行、只写名字，点了调它的默认动作（用户 2026-10-26：「每一个插件只显示一个
    /// 标题就可以了，其它功能不要展示了」）。以前用 herdr 的 popup，只能开在屏幕正中间，还把每个插件的所有动作都列出来。
    fn plugin_menu(&mut self, width: u16) {
        let room = (width as usize).saturating_sub(4).max(4); // 框 2 列 + 前面 1 格空 + 右边留 1 格（框宽超出时 draw_menu 收到窗格宽）
        let shown: std::collections::HashSet<&str> = self.foot_rows.iter().flatten().map(|b| b.1.as_str()).collect();
        let items: Vec<_> = self
            .plugins
            .iter()
            .enumerate()
            .filter(|(i, _)| !shown.contains(format!("plug:{i}").as_str()))
            .map(|(_, p)| {
                let label = if unicode_width::UnicodeWidthStr::width(p.label.as_str()) <= room {
                    p.label.clone()
                } else {
                    crate::render::plain(&crate::render::clip_line(vec![(p.label.clone(), None)], room - 1)) + "…"
                };
                (format!("plug {} {}", p.id, p.action), label, false)
            })
            .collect();
        if items.is_empty() {
            return;
        }
        let h = items.len() as u16 + 2;
        self.menu = Some(ui::Menu { x: u16::MAX, y: self.footer_top.saturating_sub(h), items });
    }

    /// herdr 的插件登记（plugins.json）改过没有：装、卸、开、关插件都会写它。只看修改时间，不问 herdr。
    fn plugins_changed(&mut self) -> bool {
        let m = std::fs::metadata(registry_path()).and_then(|m| m.modified()).ok();
        let changed = self.plugins_mtime.is_some() && m != self.plugins_mtime;
        self.plugins_mtime = m;
        changed
    }

    fn run_file(&self, ext: &str) -> PathBuf {
        crate::run_dir().join(format!("{}.{ext}", self.me.replace(':', "_")))
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
        self.jumping_until = Some(Instant::now() + Duration::from_secs(7)); // 翻屏最多 6 秒
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

/// 工具没装时工具窗格里跑的：说找不到，给出安装命令。默认按钮的几个工具 Homebrew 里都有同名的包，有 brew 就回车直接装、
/// 装完接着打开；别的只说找不到，回车关掉。
fn install_prompt(prog: &str, cmd: &str) -> String {
    let brew = ["brew", "/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
        .into_iter()
        .find(|b| tools::which(b))
        .filter(|_| ["lazygit", "yazi"].contains(&prog));
    let head = t("找不到 {p}").replace("{p}", prog);
    match brew {
        Some(b) => format!(
            "printf '\\n  %s\\n\\n    $ brew install %s\\n\\n  %s ' {} {} {}; read -r _ || exit; \
             {} install {} && {{ clear; {cmd}; }} || {{ printf '\\n  %s ' {}; read -r _; }}",
            sh_quote(&head),
            sh_quote(prog),
            sh_quote(&t("按回车安装，点外面或 Ctrl-C 取消")),
            sh_quote(b),
            sh_quote(prog),
            sh_quote(&t("没装上，按回车关闭")),
        ),
        None => format!(
            "printf '\\n  %s\\n\\n  %s ' {} {}; read -r _",
            sh_quote(&head),
            sh_quote(&t("先装上它，或者在配置里改这个按钮的命令。按回车关闭")),
        ),
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
    // 订阅这个 Claude 的状态变化、窗格关闭。pane.updated 不订：不能按窗格过滤，别的插件会一直把这里叫醒。
    // pane.focused：工具窗格开着时，点了别处（焦点挪走）就关掉它，和 tmux 版「点浮窗外面就关」一样
    let events = tx.clone();
    let subs = json!([{"type": "pane.agent_status_changed", "pane_id": target}, {"type": "pane.closed"},
                      {"type": "pane.exited"}, {"type": "pane.focused"}]);
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
        pair_focused: false,
        last_screen: 0,
        jumping_until: None,
        reveal: false,
        marks: Marks::default(),
        chaps: Vec::new(),
        items: Vec::new(),
        bottom: 0,
        chap_open: Vec::new(),
        chap_fold: HashMap::new(),
        classifying: false,
        next_classify: Instant::now(),
        no_claude_since: None,
        jump_token: Arc::new(AtomicUsize::new(0)),
        last_wide: None,
        hits: Vec::new(),
        header_len: 0,
        head_ln: 0,
        hot: Vec::new(),
        menu: None,
        menu_hits: Vec::new(),
        hover: None,
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
        tool: None,
        tool_check: None,
        tool_sticky: false,
        plugins: Vec::new(),
        plugins_mtime: None,
        plugin_tabs: HashMap::new(),
        foot_rows: Vec::new(),
        foot_shift: (0, 0),
    };
    s.save_reg();
    // 右键固定交给 herdr 的菜单：轨迹自己不用右键
    let _ = herdr::call("pane.input.set", json!({"pane_id": me, "right_click": "herdr"}));
    let (tx, rx) = mpsc::channel();
    spawn_threads(&tx, &target, &me);
    s.refresh();
    s.kick_usage(false, &tx);
    s.plugins_changed(); // 先记下 plugins.json 现在的样子，之后变了才重读
    s.plugins = tools::load_plugins(&s.cfg);

    wait_for_size(&me);
    let mut out = std::io::stdout();
    let _ = terminal::enable_raw_mode();
    let _ = execute!(out, terminal::EnterAlternateScreen, event::EnableMouseCapture);
    let mut term = Terminal::new(CrosstermBackend::new(out)).expect("terminal");
    let _ = run_loop(&mut s, &mut term, &tx, &rx);
    s.close_tool(false);
    let _ = execute!(term.backend_mut(), event::DisableMouseCapture, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    s.jump_token.fetch_add(1, Ordering::SeqCst);
    // 原地重启时登记留着：换程序那一下，别让自动打开以为这里没有轨迹、再开一个
    if s.restart.is_none() {
        let _ = std::fs::remove_file(crate::run_dir().join(format!("{}.json", me.replace(':', "_"))));
    }
    let _ = std::fs::remove_file(sock_of(&me));
    for ext in ["menu", "repos"] {
        let _ = std::fs::remove_file(s.run_file(ext));
    }
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
    let mut next_follow = Instant::now();
    let mut next_check = Instant::now() + AGENT_CHECK;
    let mut redraw = true;
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
        if std::mem::replace(&mut redraw, true) {
            let (model, ctx) = s.model_ctx();
            let usage = claude::read_usage(&s.cfg_dir, s.sid.as_deref(), s.cfg.live_usage);
            let stops = s.tr.as_ref().map(|t| t.visible()).unwrap_or_default();
            let chaps = if s.cfg.chapters == "off" { Vec::new() } else { s.marks.chapters(&stops) };
            // 展开哪几章：默认全展开（用户 2026-10-29：「默认不要折叠呀」），你点章名收起的才收着
            let chap_open: Vec<bool> = chaps
                .iter()
                .map(|c| s.chap_fold.get(&stops[c.start].id).copied().unwrap_or(true))
                .collect();
            let view = ui::View {
                stops: &stops,
                narrow,
                expanded: s.expanded,
                opened: s.opened.as_deref(),
                busy: s.busy(),
                missing: &s.missing,
                chapters: &chaps,
                open: &chap_open,
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
            let foot_base = if narrow { s.footer_rows(true, room, 0).0.len() } else { 1 };
            let mut foot_len = foot_base;
            let (mut slot, mut counts);
            loop {
                let rows = (size.height as usize).saturating_sub(head_len + 1 + 1 + foot_len);
                let bottom = ui::bottom_offset(&view, &items, room, rows);
                s.bottom = bottom;
                if s.follow {
                    s.offset = bottom;
                }
                s.offset = s.offset.min(bottom);
                // 选中跟着 Claude 的屏幕换了：不在露出来的范围里就滚过去（停在它上面一项）
                if s.reveal
                    && let Some(k) = items.iter().position(|&it| view.selected(it))
                    && !ui::visible(&view, &items, s.offset, rows, room).contains(&items[k])
                {
                    s.offset = k.saturating_sub(1).min(bottom);
                    s.follow = s.offset >= bottom;
                }
                counts = ui::view_counts(&view, &ui::visible(&view, &items, s.offset, rows, room));
                slot = if counts.0 > 0 || counts.1 > 0 {
                    unicode_width::UnicodeWidthStr::width(format!("▼{}{}", if narrow { "" } else { " " }, stops.len()).as_str())
                } else {
                    0
                };
                let want = if narrow && slot > 0 { foot_base.max(2) } else { foot_base };
                if want == foot_len {
                    break;
                }
                foot_len = want;
            }
            s.reveal = false;
            s.view = counts;
            s.chaps = chaps.clone();
            s.items = items.clone();
            s.chap_open = chap_open.clone();
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
            let accts = [claude::current_account(&s.cfg_dir), claude::short_account(&s.cfg_dir)];
            s.hot = header::hot_spots(&head, &accts);
            s.up_at = (slot > 0 && counts.0 > 0).then_some((head.len(), slot));
            // 底部：宽版一行按钮，▼ 在行首；窄版三个按钮就占满了，▼ 在第二行（插件那一行）
            let down_tag = tag("▼", counts.1);
            let (rows, at) = s.footer_rows(narrow, room, slot);
            let mut footer: Vec<render::SLine> = rows
                .iter()
                .map(|r| {
                    if r.is_empty() {
                        Vec::new()
                    } else {
                        header::buttons_line(r, None, false)
                    }
                })
                .collect();
            footer[at] = render::lead(std::mem::take(&mut footer[at]), &down_tag, slot);
            let footer: Vec<render::SLine> = footer.into_iter().map(|l| render::clip_line(l, room)).collect();
            s.down_at = (slot > 0 && counts.1 > 0).then_some((at, slot));
            s.foot_shift = (at, if slot > 0 { slot + 1 } else { 0 });
            s.foot_rows = rows;
            let mut drawn = None;
            let mut menu_hits = Vec::new();
            term.draw(|f| {
                drawn = Some(ui::draw(f, &head, &footer, &view, s.offset));
                // 能点的字：平时垫底色，鼠标停上去换强调色底（菜单开着时只亮菜单项）
                let on = if s.menu.is_none() { s.hover.and_then(|h| s.hover_key(h)) } else { None };
                for &(i, a, b) in &s.hot {
                    let lit = on == Some((i as u16, a as u16));
                    ui::mark(f, i as u16, (a + 2) as u16, (b + 2) as u16, lit);
                }
                if let Some(m) = &s.menu {
                    menu_hits = ui::draw_menu(f, m, s.hover);
                }
            })?;
            s.menu_hits = menu_hits;
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
        if let Some(t) = s.tool_check {
            wake = wake.min(t);
        }
        // 只为跟 Claude 的屏幕醒来、选中又没变时不重画（重画要读额度文件）
        let follow_only = s.pair_focused && next_follow < wake;
        if s.pair_focused {
            wake = wake.min(next_follow);
        }
        let msg = rx.recv_timeout(wake.saturating_duration_since(now));
        let timed_out = matches!(msg, Err(mpsc::RecvTimeoutError::Timeout));
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
                MouseEventKind::Moved => {
                    let was = s.hover.and_then(|h| s.hover_key(h));
                    s.hover = Some((m.row, m.column));
                    if s.hover_key((m.row, m.column)) == was {
                        redraw = false;
                        continue;
                    }
                }
                MouseEventKind::ScrollUp => s.wheel(false),
                MouseEventKind::ScrollDown => s.wheel(true), // 画的时候按最底夹住
                MouseEventKind::Down(_) if s.menu.is_some() => {
                    // 下拉菜单开着：点中一项就去换，点哪儿都先把菜单收起来
                    let hit = s
                        .menu_hits
                        .iter()
                        .find(|(r, a, b, _)| *r == m.row && (*a..*b).contains(&m.column))
                        .map(|h| h.3.clone());
                    s.menu = None;
                    s.menu_hits.clear();
                    match hit.as_deref().and_then(|k| k.split_once(' ')) {
                        Some(("plug", rest)) => {
                            if let Some((plugin, action)) = rest.split_once(' ') {
                                let (plugin, action) = (plugin.to_owned(), action.to_owned());
                                s.run_plugin(&plugin, &action, tx);
                            }
                        }
                        Some((kind, value)) => s.apply_setting(kind, value, tx),
                        None => {}
                    }
                }
                MouseEventKind::Down(MouseButton::Left) if m.row >= s.footer_top => {
                    let (row, col) = ((m.row - s.footer_top) as usize, (m.column as usize).saturating_sub(1));
                    if s.down_at.is_some_and(|(r, slot)| r == row && col <= slot + 1) {
                        s.page(1);
                    } else {
                        let col = if row == s.foot_shift.0 {
                            col.saturating_sub(s.foot_shift.1)
                        } else {
                            col
                        };
                        // 行首给 ▼ 留了位置，按钮往右挪了这么多
                        let act = s.foot_rows.get(row).and_then(|r| header::button_at(r, col)).map(str::to_owned);
                        if let Some(a) = act {
                            s.act(&a, size.width, tx);
                        }
                    }
                }
                MouseEventKind::Down(MouseButton::Left) if m.row < s.header_len => {
                    // 表头的字从第 3 列开始（左边 2 列和列表的指针对齐）；列号从 1 数
                    s.header_click(m.row as usize + 1, (m.column as usize).saturating_sub(1), size.width, tx);
                }
                MouseEventKind::Down(MouseButton::Right) if s.cfg.chapters != "off" => {
                    if let Some(&(_, it)) = s.hits.iter().find(|(row, _)| *row == m.row) {
                        s.chapter_menu(it, m.column, m.row + 1);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(&(_, it)) = s.hits.iter().find(|(row, _)| *row == m.row) {
                        match it {
                            Item::Fold => s.expanded = !s.expanded,
                            Item::Chapter(ci) => {
                                if let (Some(c), Some(tr)) = (s.chaps.get(ci), &s.tr) {
                                    let open = s.chap_open.get(ci).copied().unwrap_or(false);
                                    s.chap_fold.insert(tr.visible()[c.start].id.clone(), !open);
                                }
                            }
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
            Ok(Msg::Input(_)) => {
                redraw = false; // 键盘：轨迹不用
                continue;
            }
            Ok(Msg::Herdr(ev)) => {
                let name = ev["event"].as_str().unwrap_or("").replace('.', "_");
                let pane = ev["data"]["pane_id"].as_str().unwrap_or("");
                if matches!(name.as_str(), "pane_closed" | "pane_exited") && pane == s.target {
                    return Ok(());
                }
                if matches!(name.as_str(), "pane_closed" | "pane_exited") && s.tool.as_ref().is_some_and(|(p, _)| p == pane) {
                    // 在工具里按 q / Esc / exit 退出了，窗格自己关了；herdr 会把焦点给隔壁（轨迹），还给 Claude
                    s.tool = None;
                    s.tool_check = None;
                    let _ = herdr::call("pane.focus", json!({"pane_id": s.target}));
                }
                if name == "pane_focused" {
                    s.pair_focused = pane == s.target || pane == s.me;
                    if s.tool.as_ref().is_some_and(|(p, _)| p != pane) && !s.tool_sticky {
                        s.tool_check = Some(now + Duration::from_millis(400));
                    }
                    redraw = false; // 别的窗格换了焦点：不用重画
                    continue;
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
                let parts: Vec<&str> = c.split_whitespace().collect();
                match parts.as_slice() {
                    ["toggle"] => s.toggle(size.width),
                    // statetrail switch 换完账号：会话 id 没变，按新进程重认配置目录（账号、额度跟着变）
                    ["rescan"] => {
                        s.cfg_dir = claude::config_dir_of_pid(herdr::claude_pid(&s.target));
                    }
                    _ => {}
                }
            }
            Ok(Msg::Flash(m, d)) => s.say(m, d),
            Ok(Msg::Wake) => {}
            Ok(Msg::PluginTab(plugin, action, tab)) => {
                s.plugin_tabs.insert((plugin, action), tab);
                redraw = false;
                continue;
            }
            Ok(Msg::Classified(ok)) => {
                s.classifying = false;
                if ok {
                    if let Some(sid) = &s.sid {
                        s.marks = Marks::load(sid);
                    }
                } else {
                    s.next_classify = Instant::now() + CLASSIFY_RETRY;
                }
            }
            Ok(Msg::Jumped(ids, how)) => {
                s.jumping_until = None;
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
        if s.tool_check.is_some_and(|t| now >= t) {
            s.check_tool();
        }
        if s.pair_focused && now >= next_follow {
            next_follow = now + FOLLOW_EVERY;
            if !s.follow_screen() && timed_out && follow_only {
                redraw = false;
            }
        }
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
            // 会话 id 后报上来（report-agent-session）、对话记录文件后出现时状态都不变，没有事件：没找到就隔一会儿再查
            if s.tr.is_none() {
                stale = true;
            }
            if s.plugins_changed() {
                s.plugins = tools::load_plugins(&s.cfg); // 装了 / 卸了别的 herdr 插件：底部的插件按钮跟上
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
        if s.pair_focused {
            s.classify(tx); // 有没判过的话就去分章（一批分完接着下一批）
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn install_prompt_is_valid_sh() {
        for prog in ["lazygit", "nosuchtool"] {
            let script = super::install_prompt(prog, "exec lazygit");
            let ok = std::process::Command::new("sh")
                .args(["-n", "-c", &script])
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "{script}");
        }
    }
}
