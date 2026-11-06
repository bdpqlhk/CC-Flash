//! 局域网文件服务(axum 重写版; 大体沿袭 Python 版 API 形态, 但已扩展
//! 子目录/WebSocket 等 —— 与旧 Python 版及 ≤1.1.7 分片客户端不再兼容:
//! 协议已改为单请求整文件直传, X-Upload-Offset/X-Upload-Id 头被忽略, 旧客户端
//! 分片上传会产生碎片文件, 需两端同版本配套使用)。
//!
//! API:
//!   GET  /                    → 前端页面(嵌入 ../dist)
//!   GET  /api/files           → 文件列表 JSON(递归, 相对路径作文件名, 隐藏断点/标记产物)
//!   GET  /api/download/*name  → 流式下载(Content-Disposition 带 RFC5987)
//!   POST /api/upload          → 请求头 X-Filename(URL 编码, 支持子目录) + X-File-Size;
//!                                单请求整文件直传直写最终名(同名原子占位 + (1)(2)… 顺延),
//!                                写首字节前落 .{名}.ccup 完整性标记, 成功即删、启动见标记清半截
//!   GET  /api/upload_progress → 断点查询(整文件直传下恒返回 0; 兼容遗留 .ccpart 断点)
//!   GET  /api/chat            → 聊天记录(保留最近 200 条)
//!   POST /api/chat            → 昵称截 20 字、消息截 2000 字
//!   DELETE /api/delete/*name  → 删除文件(支持子目录)
//!   GET  /api/info            → { ip, port, running, version, config }
//!   GET/POST /api/control/*   → 桌面端服务启停/状态/日志(仅桌面壳使用)
//!   GET  /ws                  → WebSocket 事件推送(聊天/列表/状态变化, 轮询为降级通道)
//!
//! 路径安全:URL 解码 → 逐段校验(拒绝空/./.. 与非法字符);下载/删除前再做
//! canonicalize + 前缀越界校验,双重防护等价于旧版 basename 方案。

use axum::{
    body::Body,
    extract::{Path as AxPath, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use futures::StreamExt;
use percent_encoding::percent_decode_str;
use rust_embed::RustEmbed;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use std::sync::atomic::AtomicBool;
use tokio::sync::{broadcast, oneshot};

use crate::config::{
    APP_VERSION, CHAT_MAX, CHAT_PREVIEW_CHARS, CHUNK_HINT, DEFAULT_NICK, DEFAULT_PORT,
    LOG_LINE_MAX, MSG_MAX_CHARS, NICK_MAX_CHARS, NOISY_GET_PATHS, PIN_PREF_FILE, POLL_CHAT_MS,
    POLL_FILES_MS, POLL_LOGS_MS, POLL_STATUS_MS, PORT_PREF_FILE, PORT_PROBE_EXTRA, PORT_RANGE_END,
    REQ_LOG_MAX,
    SHARE_DIR_NAME, SHARE_DIR_NAME_LEGACY, SHARE_PREF_FILE, UPLOAD_FREE_CHECK_BYTES,
    UPLOAD_IDLE_TIMEOUT_SECS, UPLOAD_MAX_DEPTH,
    UPLOAD_PART_META_SUFFIX,
    UPLOAD_PART_SUFFIX, UPLOAD_PART_TTL_SECS,
};

// ---------------------------------------------------------------- //
// 全局状态
// ---------------------------------------------------------------- //

#[derive(Clone, Serialize)]
pub struct ChatMsg {
    pub id: u64,
    pub nickname: String,
    pub message: String,
    pub timestamp: f64,
    pub ip: String,
}

fn chat_log() -> &'static Mutex<VecDeque<ChatMsg>> {
    static CHAT: OnceLock<Mutex<VecDeque<ChatMsg>>> = OnceLock::new();
    CHAT.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// 结构化日志条目(loguru 风格): 全量时间戳 + 级别 + 来源位置 + 消息
#[derive(serde::Serialize, Clone)]
struct LogEntry {
    /// 本地时间 YYYY-MM-DD HH:MM:SS.mmm(托盘常驻应用跨天运行, 日期必须带)
    ts: String,
    /// info→INFO / ok→SUCCESS / warn→WARNING / err→ERROR(前端映射展示)
    level: String,
    /// 来源位置 `文件名:行号`(loguru <location> 的对应物, track_caller 捕获调用点)
    loc: String,
    msg: String,
}

fn req_log() -> &'static Mutex<VecDeque<LogEntry>> {
    static LOGS: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// track_caller 链式透传: push_log_* 又是 track_caller, Location::caller()
/// 拿到的是**最原始调用点**的 文件名:行号
#[track_caller]
fn push_log_lvl(level: &str, line: String) {
    let at = std::panic::Location::caller();
    let file = at.file().rsplit(['/', '\\']).next().unwrap_or(at.file());
    // 统一消毒: ① 剥 CR/LF 防日志注入(?nick=%0A 伪造条目/嫁祸 IP);
    // ② 整行硬截断——hyper 允许数百 KB 的 URL, 不截断可把内存推高数百 MB
    let clean: String = line
        .replace(['\r', '\n'], " ")
        .chars()
        .take(LOG_LINE_MAX)
        .collect();
    let mut q = req_log().lock().unwrap_or_else(|e| e.into_inner());
    q.push_back(LogEntry {
        ts: now_ts(),
        level: level.to_string(),
        loc: format!("{file}:{}", at.line()),
        msg: clean,
    });
    while q.len() > REQ_LOG_MAX {
        q.pop_front();
    }
}

#[track_caller]
fn push_log(line: String) {
    push_log_lvl("info", line);
}
/// 常用级别便捷封装: 成功 / 失败 / 可疑——调用点直接表意, 桌面面板按级着色
#[track_caller]
fn push_log_ok(line: String) {
    push_log_lvl("ok", line);
}
#[track_caller]
fn push_log_err(line: String) {
    push_log_lvl("err", line);
}
#[track_caller]
fn push_log_warn(line: String) {
    push_log_lvl("warn", line);
}

// ---- WebSocket 事件总线: 聊天/列表/状态变化即时推送, 前端轮询降级为兜底 ---- //

fn event_bus() -> &'static broadcast::Sender<String> {
    static TX: OnceLock<broadcast::Sender<String>> = OnceLock::new();
    TX.get_or_init(|| broadcast::channel(64).0)
}

/// 广播事件给所有已连接的 WebSocket 客户端(失败静默: 无人在线属正常)
fn broadcast_event(kind: &str) {
    let _ = event_bus().send(format!("{{\"type\":\"{kind}\"}}"));
}

/// 本地时间 YYYY-MM-DD HH:MM:SS(精确到秒; 耗时类信息在消息体里自带毫秒)
fn now_ts() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub struct ServerHandle {
    pub port: u16,
    shutdown: oneshot::Sender<()>,
    task: tauri::async_runtime::JoinHandle<()>,
}

fn running_slot() -> &'static Mutex<Option<ServerHandle>> {
    static RUNNING: OnceLock<Mutex<Option<ServerHandle>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(None))
}

static PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// 共享目录选择持久化文件(家目录隐藏文件, 内容为一行路径)
fn share_pref_file() -> PathBuf {
    // Android: $HOME 无效, 偏好文件放 app 专属目录(setup 已注入), 否则手动选择的
    // 共享目录写入静默失败、重启回落默认
    #[cfg(target_os = "android")]
    if let Some(d) = platform_default_dir() {
        return Path::new(d).join(SHARE_PREF_FILE);
    }
    #[cfg(target_os = "windows")]
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".into());
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(SHARE_PREF_FILE)
}

fn default_share_dir() -> PathBuf {
    // 用户曾手动选择过目录 → 恢复该选择(目录仍存在时); 优先级最高(含移动端)
    if let Ok(saved) = std::fs::read_to_string(share_pref_file()) {
        let saved = saved.trim();
        if !saved.is_empty() && Path::new(saved).is_dir() {
            return PathBuf::from(saved);
        }
    }
    // Android 应用进程 $HOME 无效(落到 "/" 或相对路径 → EACCES, 服务启动即失败):
    // 由 lib.rs setup 注入 app 专属目录(父); 共享目录为其下 CC-Flash 子目录,
    // 偏好文件平级置放(不进共享目录内部, 否则会被 HTTP 列表当普通文件展示)
    #[cfg(target_os = "android")]
    if let Some(d) = platform_default_dir() {
        return Path::new(d).join(SHARE_DIR_NAME);
    }
    #[cfg(target_os = "windows")]
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".into());
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let new = Path::new(&home).join(SHARE_DIR_NAME);
    // 品牌更名迁移: 旧目录存在且新目录不存在时整体改名, 保证用户文件不"消失"
    let old = Path::new(&home).join(SHARE_DIR_NAME_LEGACY);
    if !new.exists() && old.is_dir() {
        if std::fs::rename(&old, &new).is_ok() {
            push_log(format!("已将共享目录 {} 迁移为 {}", old.display(), new.display()));
        }
    }
    new
}

/// Android 专属: setup 阶段注入的默认共享目录(app 专属外部/内部存储)。
/// 非 Android 平台无此机制, default_share_dir 走 $HOME 分支。
#[cfg(target_os = "android")]
static PLATFORM_DEFAULT_DIR: OnceLock<String> = OnceLock::new();
#[cfg(target_os = "android")]
pub fn set_platform_default_dir(dir: String) {
    let _ = PLATFORM_DEFAULT_DIR.set(dir);
}
#[cfg(target_os = "android")]
fn platform_default_dir() -> Option<&'static str> {
    PLATFORM_DEFAULT_DIR.get().map(String::as_str)
}

fn share_slot() -> &'static Mutex<PathBuf> {
    static SHARE: OnceLock<Mutex<PathBuf>> = OnceLock::new();
    SHARE.get_or_init(|| Mutex::new(default_share_dir()))
}

pub fn share_dir() -> PathBuf { share_slot().lock().unwrap_or_else(|e| e.into_inner()).clone() }

/// 更换共享目录(与 PyQt 版一致: 运行中锁定, 停止后下次启动生效); 同步持久化选择
pub fn set_share_dir(p: PathBuf) {
    *share_slot().lock().unwrap_or_else(|e| e.into_inner()) = p.clone();
    let _ = std::fs::write(share_pref_file(), p.to_string_lossy().as_bytes());
}


pub fn is_running() -> bool { running_slot().lock().unwrap_or_else(|e| e.into_inner()).is_some() }

/// 调系统默认程序打开文件/目录/URL(macOS: open / Windows: start / Linux: xdg-open)
pub fn open_with_system(target: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(target);
        c
    };
    // Windows: 绝不经过 cmd shell —— cmd 会把 & | ^ 等解释为命令分隔符/转义符,
    // 且 Rust 对参数的 \" 转义 cmd 并不认(引号包裹防不住), 聊天链接/文件名可注入命令。
    // URL 走 rundll32 的 FileProtocolHandler, 文件/目录走 explorer, 均为直接参数传递无 shell 解析。
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let is_url = target.starts_with("http://") || target.starts_with("https://");
        if is_url {
            let mut c = std::process::Command::new("rundll32");
            c.arg("url.dll,FileProtocolHandler").arg(target);
            c
        } else {
            let mut c = std::process::Command::new("explorer");
            c.arg(target);
            c
        }
    };
    // Android: 用 am 发 ACTION_VIEW 意图交给系统浏览器(壳内下载/外链的落地通道);
    // WebView 无 DownloadListener, 锚点下载在安卓壳内不会有任何动作
    #[cfg(target_os = "android")]
    let mut cmd = {
        let mut c = std::process::Command::new("am");
        c.arg("start")
            .arg("-a")
            .arg("android.intent.action.VIEW")
            .arg("-d")
            .arg(target);
        c
    };
    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        target_os = "android"
    )))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(target);
        c
    };
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    // open/xdg-open/am 都是短命启动器: 立即后台 wait 回收, 否则每次打开文件
    // 都在托盘常驻进程下留一个僵尸子进程
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// 打开本地文件/目录(路径必须存在)
pub fn open_path(path: &str) -> Result<(), String> {
    if !Path::new(path).exists() {
        return Err(format!("路径不存在: {path}"));
    }
    open_with_system(path)
}

/// 本机局域网 IP(UDP connect 探测, 不发包; 失败回退 127.0.0.1)
pub fn lan_ip() -> String {
    // 旧实现只做一次 UDP connect 探测(源地址由默认路由决定): 开 VPN/TUN/双网卡的机器
    // 会拿到隧道地址(如 10.8.0.2), 二维码手机扫码必失败; IPv6-only 则静默回落 127.0.0.1。
    // 现改为枚举真实网卡: 隧道类接口(utun/tun/tap/wg/ppp/bridge/vEthernet)一律排除,
    // 优先 RFC1918 私网地址; 默认路由探测结果若落在真实网卡上则优先它(双网卡场景准头)
    let probe = UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|s| {
            s.connect("10.255.255.255:1").ok()?;
            s.local_addr().ok()
        })
        .map(|a| a.ip().to_string());
    const TUNNEL_PREFIXES: &[&str] = &["utun", "tun", "tap", "wg", "ppp", "vethernet", "docker", "vmnet", "virbr"];
    let is_tunnel = |name: &str| {
        let n = name.to_ascii_lowercase();
        TUNNEL_PREFIXES.iter().any(|p| n.starts_with(p))
    };
    // bridge*(macOS 互联网共享/热点)不硬过滤: 手机直连 Mac 热点时唯一可达地址就是它,
    // 但普通场景它只是冗余 —— 降为末位候选(真实网卡/探测都失效时才取)
    let is_bridge = |name: &str| name.to_ascii_lowercase().starts_with("bridge");
    let is_private = |ip: &std::net::Ipv4Addr| {
        ip.is_private() || ip.is_link_local()
    };
    let mut private: Vec<String> = Vec::new();
    let mut other: Vec<String> = Vec::new();
    let mut bridge_last: Option<String> = None;
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if is_tunnel(&iface.name) {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                if v4.ip.is_loopback() {
                    continue;
                }
                let s = v4.ip.to_string();
                if is_bridge(&iface.name) {
                    if bridge_last.is_none() {
                        bridge_last = Some(s);
                    }
                } else if is_private(&v4.ip) && !private.contains(&s) {
                    private.push(s);
                } else if !other.contains(&s) {
                    other.push(s);
                }
            }
        }
    }
    // 默认路由探测结果落在真实网卡的私网地址上 → 直接用(双网卡时最准)
    if let Some(p) = &probe {
        if private.iter().any(|x| x == p) {
            return p.clone();
        }
    }
    if let Some(first) = private.first() {
        return first.clone();
    }
    if let Some(first) = other.first() {
        return first.clone();
    }
    if let Some(b) = bridge_last {
        return b; // macOS 互联网共享场景: 手机直连热点时唯一可达地址
    }
    // 全部失败才回落探测值/回环(此时二维码应配合 UI 提示"未检测到局域网地址")
    probe.unwrap_or_else(|| "127.0.0.1".into())
}

// ---------------------------------------------------------------- //
// 启停管理
// ---------------------------------------------------------------- //

/// 绑定端口: 优先 IPv6 双栈([::], 默认同时接收 v4-mapped 连接), 并自检 v4 连通后采用;
/// 无 IPv6 / bindv6only=1 / 地址被占时回落纯 IPv4(0.0.0.0) —— 老环境行为不变, 不缺 v4。
async fn bind_dualstack(p: u16) -> Option<tokio::net::TcpListener> {
    if let Ok(l) = tokio::net::TcpListener::bind(("::", p)).await {
        // 少数系统 net.ipv6.bindv6only=1: [::] 绑上但 v4 不通, 此时必须放弃回落,
        // 否则局域网里绝大多数 IPv4 设备全部失联
        if tokio::net::TcpStream::connect(("127.0.0.1", p)).await.is_ok() {
            push_log(format!("监听 [::]:{p}（IPv6 双栈，兼容 v4-mapped 连接）"));
            return Some(l);
        }
        drop(l);
    }
    match tokio::net::TcpListener::bind(("0.0.0.0", p)).await {
        Ok(l) => {
            push_log(format!("监听 0.0.0.0:{p}（仅 IPv4 —— 本机无 IPv6 或 bindv6only=1）"));
            Some(l)
        }
        Err(_) => None,
    }
}

/// 启动服务(优先尝试指定端口, 被占用自动顺延);返回实际端口。
/// 注意:tokio 的 socket 操作必须在 runtime 上下文内,故整体包进 block_on。
/// 用户最近一次指定的首选端口(0=未指定): 托盘"启动"/应用重启时回放,
/// 防止重启后端口静默漂移回默认值、用户早先复制给手机的地址/二维码失效
static LAST_PREFERRED_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// 端口偏好持久化文件(与目录偏好同目录; Android 放 app 专属目录)
fn port_pref_file() -> PathBuf {
    #[cfg(target_os = "android")]
    if let Some(d) = platform_default_dir() {
        return Path::new(d).join(PORT_PREF_FILE);
    }
    #[cfg(target_os = "windows")]
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".into());
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(PORT_PREF_FILE)
}

pub fn last_preferred_port() -> Option<u16> {
    match LAST_PREFERRED_PORT.load(Ordering::Relaxed) {
        // 进程内未指定过 → 回放磁盘偏好: 重启应用后端口不漂移回默认,
        // 用户早先复制给手机的地址/二维码保持有效
        0 => std::fs::read_to_string(port_pref_file())
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .filter(|p| *p > 0),
        p => Some(p),
    }
}

pub fn start(preferred: Option<u16>) -> Result<u16, String> {
    // 整个 start 流程(检查+bind+写入)持同一把锁: 否则并发双启(托盘连点 + HTTP ctrl_start,
    // 或两个 LAN 客户端同时 POST /api/control/start)都通过 None 检查、各自 bind,
    // 后者覆写 running_slot 时 drop 前者的 shutdown Sender → 前一个服务自杀式关停
    static START_GUARD: Mutex<()> = Mutex::new(());
    // 等在途 stop 收尾再拿锁(stop 入口先置 STOPPING 再抢本锁, 顺序天然互斥),
    // 拿到锁后复查一次 —— 关闭"检查通过后 stop 才开始"的残余空档, 地址不再漂移
    let _guard = loop {
        let stop_deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(3500);
        while STOPPING.load(Ordering::SeqCst) && std::time::Instant::now() < stop_deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let g = START_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        if !STOPPING.load(Ordering::SeqCst) {
            break g;
        }
        drop(g); // stop 正持锁做 take/shutdown, 让路后重来
    };
    if let Some(p) = preferred {
        LAST_PREFERRED_PORT.store(p, Ordering::Relaxed);
        // 磁盘偏好延迟到**实际绑定成功**后落盘(见 picked 之后): 全端口占用启动失败时,
        // 不能把死端口写进偏好文件, 否则下次自启先撞一次死端口再顺延
    }
    {
        let lock = running_slot().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = lock.as_ref() {
            return Ok(h.port);
        }
    }
    let share = share_dir();
    std::fs::create_dir_all(&share)
        .map_err(|e| format!("共享目录不可用: {e}"))?;
    // 顺序敏感: 必须先清硬中断残留 —— 若先跑 clean_stale_parts(7天TTL按 is_part_artifact
    // 删标记), 老标记被删而半截成品留下, 后者就再也识别不出来
    clean_incomplete_uploads(&share); // 标记在 → 成品必是半截(启动时无在途上传)
    clean_stale_parts(&share);         // 再清理过期续传断点(期内保留, 支持重启后续传)

    tauri::async_runtime::block_on(async move {
        let mut candidates: Vec<u16> = Vec::new();
        if let Some(p0) = preferred {
            candidates.push(p0);
            // u32 算术防 u16 溢出(p0 接近 65535 时 p0+1 在 debug 构建 panic)
            candidates.extend(
                (u32::from(p0) + 1..u32::from(p0) + 1 + u32::from(PORT_PROBE_EXTRA))
                    .filter_map(|p| u16::try_from(p).ok()),
            );
        }
        candidates.extend(DEFAULT_PORT..PORT_RANGE_END);
        let mut picked: Option<(tokio::net::TcpListener, u16)> = None;
        let mut tried = std::collections::HashSet::new();
        for p in candidates {
            if !tried.insert(p) { continue; }
            if open_lan() {
                // 双栈 + v4 自检 + 无 IPv6/bindv6only=1 回落 0.0.0.0 ——
                // 此前只裸 bind("[::]"): 禁 IPv6 的主机全部端口启动失败,
                // bindv6only=1 时 IPv4 设备全拒连(回落逻辑只存在于未编译的测试代码)
                match bind_dualstack(p).await {
                    Some(l) => {
                        picked = Some((l, p));
                        break;
                    }
                    None => continue,
                }
            } else {
                match tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
                    Ok(l) => {
                        push_log(format!("监听 127.0.0.1:{p}（安全模式）"));
                        picked = Some((l, p));
                        break;
                    }
                    Err(_) => continue,
                }
            }
        }
        let (listener, port) = match picked {
            Some(v) => v,
            None => {
                let e = format!("{DEFAULT_PORT}-{} 端口均被占用", PORT_RANGE_END - 1);
                push_log_err(format!("服务启动失败: {e}"));
                return Err(e);
            }
        };

        PORT.store(port, Ordering::Relaxed);
        // 绑定成功才落盘偏好, 且写**实际生效端口**(含被占顺延): 重启后回放的就是
        // 手机上还能打开的那个地址, 而不是曾被占用过的首选值
        if preferred.is_some() {
            let _ = std::fs::write(port_pref_file(), port.to_string());
        }
        push_log_ok(format!("服务已启动 端口 {port} 目录 {}", share.display()));
        if preferred.is_some() && preferred != Some(port) {
            push_log_warn(format!("首选端口 {} 被占用，已顺延至 {port}", preferred.unwrap_or(0)));
        }
        let n = std::fs::read_dir(&share).map(|it| it.count()).unwrap_or(0);
        push_log(format!("共享目录当前 {n} 个文件 · 访问地址: http://{}:{port}", lan_ip()));

        let (tx, rx) = oneshot::channel::<()>();
        let app = router(share).into_make_service_with_connect_info::<SocketAddr>();
        let task = tauri::async_runtime::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        *running_slot().lock().unwrap_or_else(|e| e.into_inner()) =
            Some(ServerHandle { port, shutdown: tx, task });
        broadcast_event("status:up");
        Ok(port)
    })
}

/// 服务启动时清理**过期**的上传断点文件(*.ccpart 及其 .meta, 超过 UPLOAD_PART_TTL_SECS)。
/// 期内保留: 服务重启后客户端重选同一文件, 会话指纹命中即可续传
/// (Android 退后台被杀/电脑重启不再无声归零); 只有过期垃圾才回收磁盘。
/// 不跟随符号链接: 否则会删到共享目录之外(或把外部目录列进文件表)。
fn clean_stale_parts(share: &Path) {
    fn expired(md: &std::fs::Metadata) -> bool {
        // 比较"距今的年龄": now - mtime。曾误写成 mtime - UNIX_EPOCH(mtime 的 epoch
        // 秒值 ~1.79e9 恒 ≥ TTL), 所有文件都被判过期, 7 天保留成空实现
        match md.modified() {
            // 未来 mtime(系统时钟被改过)按未过期保留: 宁多留 7 天也不误删进行中断点
            Ok(t) => SystemTime::now()
                .duration_since(t)
                .map(|d| d.as_secs() >= UPLOAD_PART_TTL_SECS)
                .unwrap_or(false),
            Err(_) => true, // 时间戳拿不到按过期处理
        }
    }
    fn walk(dir: &Path, n: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = e.metadata() else { continue }; // DirEntry::metadata 不跟随符号链接
            if md.is_dir() {
                walk(&p, n);
            } else if md.is_file()
                && p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map_or(false, is_part_artifact)
                && expired(&md)
                && std::fs::remove_file(&p).is_ok()
            {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(share, &mut n);
    if n > 0 {
        push_log(format!("已清理 {n} 个过期的上传断点文件(超过 {}s)", UPLOAD_PART_TTL_SECS));
    }
}

/// 硬中断残留清理(#1 数据完整性): upload 在写入首个字节前先创建 `.{文件名}.ccup` 标记,
/// 成功后删除; 崩溃/停服超时 abort/进程被杀都不会走到错误清理路径 —— 启动时见标记即认定
/// 对应成品是半截文件, 连标记一并删除。仅启动执行(此刻必无在途上传)。
/// 残余窗口: "占位创建后、标记创建前"崩溃会留下 0 字节成品(无标记)——空文件肉眼可辨,
/// 不属"与完整文件无法区分"的静默损坏, 接受。
fn clean_incomplete_uploads(share: &Path) {
    fn walk(dir: &Path, n: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                walk(&p, n);
                continue;
            }
            let Some(fname) = p.file_name().and_then(|s| s.to_str()) else { continue };
            if !(fname.starts_with('.') && fname.ends_with(".ccup")) {
                continue;
            }
            // ".报告.pdf.ccup" → 成品名 "报告.pdf"
            let final_name = &fname[1..fname.len() - ".ccup".len()];
            let final_path = dir.join(final_name);
            // 内容 "ok" = 上传已完整落盘只是标记删除失败 → 只删标记, 保形成品
            let done = std::fs::read_to_string(&p)
                .map(|s| s.trim() == "ok")
                .unwrap_or(false);
            let mut removed = false;
            if done {
                removed |= std::fs::remove_file(&p).is_ok();
                if removed {
                    push_log_warn(format!("清理未删除的完成标记(成品完好) {final_name}"));
                }
            } else {
                if std::fs::metadata(&final_path).is_ok() {
                    removed |= std::fs::remove_file(&final_path).is_ok();
                    push_log_warn(format!("清理硬中断残留的半截文件 {final_name}"));
                }
                removed |= std::fs::remove_file(&p).is_ok();
            }
            if removed {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(share, &mut n);
    if n > 0 {
        push_log(format!("已清理 {n} 个硬中断残留(半截成品及其标记)"));
    }
}

/// WS 踢出通道: 停服时先让所有 WS handler 主动退出 —— 否则 hyper 的优雅关闭会
/// 等待挂着的 WebSocket(手机息屏不回包)直至超时, 停服在主线程上卡住数秒。
fn ws_kick() -> &'static broadcast::Sender<()> {
    static TX: OnceLock<broadcast::Sender<()>> = OnceLock::new();
    TX.get_or_init(|| broadcast::channel(4).0)
}

/// 停止服务(踢掉 WS 后等待优雅关闭, 带超时)。
/// 不等待的话紧接着 start() 会因旧监听端口未释放而顺延到别的端口, 用户刚复制的地址即失效。
/// stop() 在途标志: 优雅关闭在 running_slot 锁外进行(最长约 2.3s), START_GUARD 拦不住
/// 并发 start —— 旧 listener 未释放时 start 会顺延绑到下一个端口(地址/二维码漂移失效)。
/// start 在拿到 START_GUARD 后等待此标志清除再绑定。
static STOPPING: AtomicBool = AtomicBool::new(false);

pub fn stop() -> bool {
    // 入口即置位: 必须先于抢 START_GUARD —— 否则 start 在"检查通过后、stop 开始前"
    // 的空档拿到锁直接 bind, 仍会与在途优雅关闭抢端口(地址漂移窗口没关死)
    STOPPING.store(true, Ordering::SeqCst);
    let mut lock = running_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = lock.take() {
        push_log("服务已停止".into());
        broadcast_event("status:down");
        let _ = ws_kick().send(());
        let _ = h.shutdown.send(());
        let task = h.task;
        drop(lock); // 别持有全局锁等待关闭
        tauri::async_runtime::block_on(async move {
            let mut task = task;
            if tokio::time::timeout(std::time::Duration::from_secs(2), &mut task)
                .await
                .is_err()
            {
                task.abort(); // 超时必须显式中止: 直接 drop JoinHandle 是 detach 不是取消,
                              // 反复启停会在后台累积僵尸 serve 任务/连接
                // abort 后短等任务真正退出: 否则紧接的 start() 可能因旧 listener
                // 尚未释放而不必要地顺延端口(网页端表现为地址漂移失联)
                let _ = tokio::time::timeout(std::time::Duration::from_millis(300), &mut task).await;
            }
        });
        STOPPING.store(false, Ordering::SeqCst);
        true
    } else {
        STOPPING.store(false, Ordering::SeqCst);
        false
    }
}

/// 应用启动时自动拉起(失败仅打日志, 不阻断 GUI)。
/// 回放上次指定的端口: 重启应用后地址不漂移
pub fn autostart() -> Result<u16, String> {
    start(last_preferred_port())
}

static OPEN_LAN: AtomicBool = AtomicBool::new(true);

fn open_lan() -> bool { OPEN_LAN.load(Ordering::Relaxed) }

pub fn set_open_lan(open: bool) {
    OPEN_LAN.store(open, Ordering::Relaxed);
    push_log(if open { "已开放局域网访问".into() } else { "已启用安全模式：仅本机可访问".into() });
}

pub fn status() -> (bool, u16, String) {
    let running = running_slot().lock().unwrap_or_else(|e| e.into_inner()).is_some();
    (running, PORT.load(Ordering::Relaxed), lan_ip())
}

// ---------------------------------------------------------------- //
// 路径安全 / 工具
// ---------------------------------------------------------------- //

const FRAGMENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'`');

/// 相对路径校验: 输入应为**已解码**字符串(axum Path/Query 提取器自带一次解码;
/// X-Filename 请求头由 upload 处理解码一次后再传入)。逐段检查(拒绝空/./.. 与非法字符),
/// 允许子目录(整目录上传), 段数上限与文件列表递归深度一致。
/// 返回以 "/" 连接的相对路径; 保证 join 后不越出共享目录。
fn safe_rel_path(raw: &str, for_upload: bool) -> Option<String> {
    let norm = raw.replace('\\', "/");
    let mut parts: Vec<String> = Vec::new();
    for seg in norm.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return None;
        }
        // Windows 非法字符只在上传入口拒绝；读路径必须允许既有 Unix 合法文件，
        // 否则列表可见但下载/删除会全部 400。
        if for_upload && seg.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
            return None;
        }
        // 控制字符(CR/LF/TAB/NUL 等, Windows 文件名非法, 落 macOS/Linux 会创建出
        // 带换行的文件名, 跨端互传必出问题)与 RTL 覆写符(U+202A-E, "exe.jpg" 反转
        // 伪装扩展名的社工手法)一律上传拒绝 —— 既有含这些字符的文件仍可下载/删除
        if for_upload
            && seg.chars()
                .any(|c| c.is_control() || ('\u{202A}'..='\u{202E}').contains(&c))
        {
            return None;
        }
        // 以下三类"Windows 侧非法、别处合法"的形态仅上传入口拒绝 ——
        // 共享目录里的既有文件(报告 .pdf / v1.1..txt / aux.pdf, 在 macOS/Linux 完全合法)
        // 若读路径也拦, 会列表可见但下载/删除全 400, 既取不回也清不掉
        if for_upload {
            // 前导/尾随空格: 不在服务端静默改名, 否则客户端展示名与落盘名不一致
            if seg.trim() != seg {
                return None;
            }
            if seg.ends_with('.') {
                return None; // Windows 保留形态(结尾点)
            }
            // Windows 保留设备名: NUL/CON/COM1… 打开的是设备而非文件
            // (写入被静默丢弃或挂起, rename 转正必失败 → 上传"成功"后文件消失)。
            // stem 必须再 trim: Windows 解析时剥掉点前尾随空格, "con .txt" 实为 CON 设备;
            // 另 CONIN$/CONOUT$ 同属保留设备名
            let stem = seg.split('.').next().unwrap_or("").trim().to_ascii_uppercase();
            if matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
                    | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
                    | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
            ) {
                return None;
            }
        }
        parts.push(seg.to_string());
    }
    if parts.is_empty() || parts.len() > UPLOAD_MAX_DEPTH {
        return None;
    }
    // 保留内部产物后缀(所有调用点): 否则上传同名文件会从列表消失, 且下次启动被断点清理误删
    if let Some(last) = parts.last() {
        let last_l = last.to_ascii_lowercase();
        if last_l.ends_with(UPLOAD_PART_SUFFIX)
            || last_l.ends_with(UPLOAD_PART_META_SUFFIX)
            || last_l.ends_with(&format!("{}.tmp", UPLOAD_PART_META_SUFFIX))
            // 完整性标记 .{名}.ccup: 上传拒绝(否则可伪造标记让启动清理误删同名真文件),
            // 读/删同样拒绝(标记非用户内容, 不可下载不可删)
            || last_l.ends_with(".ccup")
        {
            return None;
        }
        // 系统垃圾文件仅上传入口拒绝: 既有 .DS_Store 等仍可下载/删除(否则清不掉)
        if for_upload && is_junk_file(last) {
            return None;
        }
    }
    Some(parts.join("/"))
}

/// 断点旁路元数据文件(记录上传会话 id): <part>.meta
fn part_meta_path(part: &Path) -> PathBuf {
    let mut s = part.as_os_str().to_owned();
    s.push(UPLOAD_PART_META_SUFFIX);
    PathBuf::from(s)
}

/// 会话私有断点文件: <name>.<idhash>.ccpart —— **part 按会话隔离**。
/// 共享单一 part 文件时, 接管竞态会静默污染内容: A 传到一半连接僵死(字节滞留内核
/// 缓冲), 30s 无写入后 B 接管(truncate 重置 part); A 的连接复活把迟到字节继续写进
/// 同一个文件 —— A 的账本登记会被 id 校验拒绝, 但**字节已落进 B 的文件**: 若 B 恰好
/// 已登记完成同一区间, 迟到字节覆盖其上, 收齐转正即产出脏数据且无任何告警。
/// part 名带会话指纹后, 接管的新会话用全新文件, 旧会话的迟到字节写进的是无人消费
/// 的旧文件(由 clean_stale_parts 按 TTL 回收), 窗口彻底关闭。
/// idhash = FNV-1a 64 位十六进制: 派生自已消毒的 upload_id, 同会话稳定可续传,
/// 不同会话(不同名/不同内容/不同 mtime)不同名
fn session_part_path(share: &Path, name: &str, upload_id: &str) -> (PathBuf, PathBuf) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in upload_id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let part = share.join(format!("{name}.{h:016x}{UPLOAD_PART_SUFFIX}"));
    (part.clone(), part_meta_path(&part))
}

/// 同名自动改名(上传不覆盖既有文件): `报告.pdf` 已存在 → 依次探测 `报告(1).pdf`、
/// `报告(2).pdf` … 取第一个空闲名; 目录无同名时原样返回。
/// 只改末段文件名, 子目录前缀不动; 无扩展名/点开头文件(.gitignore, 首段为空)整体视为
/// stem, 避免 ".gitignore" 被拆成空 stem + "gitignore" 扩展名。循环上限防病态目录。
/// 上传入口 create_new 撞名后调用(整文件直传路径); 单测覆盖变体规则
fn dedup_rel_name(dir: &Path, rel: &str) -> String {
    if !dir.join(rel).exists() {
        return rel.to_string();
    }
    let (dirp, file) = match rel.rsplit_once('/') {
        Some((d, f)) => (format!("{d}/"), f),
        None => (String::new(), rel),
    };
    let (stem, ext) = match file.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, Some(e)),
        _ => (file, None),
    };
    for i in 1..10000u32 {
        let cand = match ext {
            Some(e) => format!("{stem}({i}).{e}"),
            None => format!("{stem}({i})"),
        };
        let full = format!("{dirp}{cand}");
        if !dir.join(&full).exists() {
            return full;
        }
    }
    rel.to_string() // 万级变体全占(实际不可达): 回落旧版覆盖行为
}

/// 共享目录遍历时跳过断点产物(中间文件与其 meta 及原子写临时名; 大小写不敏感——exFAT/U盘 拷来的变体同样跳)
fn is_part_artifact(fname: &str) -> bool {
    let f = fname.to_ascii_lowercase();
    f.ends_with(UPLOAD_PART_SUFFIX)
        || f.ends_with(UPLOAD_PART_META_SUFFIX)
        || f.ends_with(&format!("{}.tmp", UPLOAD_PART_META_SUFFIX))
        // 完整性标记(见 upload 的 .{名}.ccup): 硬中断残留的半截成品凭它识别,
        // 不进列表、不参与下载; 启动时 clean_incomplete_uploads 见标记即删
        || (f.starts_with('.') && f.ends_with(".ccup"))
}

/// 系统垃圾文件: 不进列表、上传拒绝。.DS_Store 记录了目录内曾出现过的文件名(隐私),
/// Thumbs.db/desktop.ini 是 Windows 缩略图缓存, ._ * 是 exFAT 的 AppleDouble 元数据
fn is_junk_file(fname: &str) -> bool {
    let f = fname.to_ascii_lowercase();
    f == ".ds_store" || f == "thumbs.db" || f == "desktop.ini" || f.starts_with("._")
}



/// 正在流式写入的会话计数。取消请求不能直接删除仍被 handler 打开的 part，
/// 否则 Unix 上会写入已删除 inode、Windows 上会因句柄占用删除失败。
fn active_uploads() -> &'static Mutex<HashMap<String, usize>> {
    static ACTIVE: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cancel_requests() -> &'static Mutex<HashSet<String>> {
    static CANCEL: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CANCEL.get_or_init(|| Mutex::new(HashSet::new()))
}

fn active_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// 在途计数减一(上传成功/失败/中止均调用; 归零即移除 key)
fn drop_active(key: &str) {
    let mut m = active_uploads().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = m.get_mut(key) {
        *c = c.saturating_sub(1);
        if *c == 0 {
            m.remove(key);
        }
    }
}

/// 上传失败统一清理: 半截成品 + 完整性标记(.{名}.ccup) + 在途计数
async fn abort_upload(path: &std::path::Path, marker: &std::path::Path, key: &str) {
    let _ = tokio::fs::remove_file(path).await;
    let _ = tokio::fs::remove_file(marker).await;
    drop_active(key);
}

/// 一次性分享票据：授权“某个相对路径的单次下载”，永不授予上传/删除/API 能力。
/// 承载式(bearer)语义: 凭 ticket 字符串本身 + 文件名绑定 + TTL 生效, 不校验来源 IP ——
/// 创建方是桌面壳(127.0.0.1)、接收方必然是局域网另一台设备, 校验 owner 会让分享永远失败。
struct ShareTicket {
    name: String,
    expires_at: SystemTime,
}

fn share_tickets() -> &'static Mutex<HashMap<String, ShareTicket>> {
    static TICKETS: OnceLock<Mutex<HashMap<String, ShareTicket>>> = OnceLock::new();
    TICKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

const SHARE_TICKET_TTL_SECS: u64 = 10 * 60;

fn random_token() -> String {
    // 6 位十六进制(24bit): 按需求精简到最短。风险边界可控 —— 凭证是承载式且
    // 绑定单文件、10 分钟 TTL、下载成功即焚, 猜中也只是多下载这一个文件;
    // 24bit 空间在 10min 窗口内全速喷码命中率约千分之几/活跃票据, 且票据只能
    // 由分享方签发(攻击者不能自造)。同名冲突由 HashMap 覆盖 + TTL 剪枝兜底。
    let mut bytes = [0u8; 3];
    getrandom::getrandom(&mut bytes).expect("CSPRNG 不可用");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn prune_tickets_locked(tickets: &mut HashMap<String, ShareTicket>) {
    let now = SystemTime::now();
    tickets.retain(|_, ticket| ticket.expires_at > now);
}

#[derive(serde::Deserialize)]
struct ShareTicketRequest {
    name: String,
}

async fn share_ticket(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    Json(req): Json<ShareTicketRequest>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    let Some(name) = safe_rel_path(&req.name, false) else {
        return err(StatusCode::BAD_REQUEST, "非法的文件名");
    };
    let share = share_dir();
    if safe_join_existing(&share, &name)
        .and_then(|p| p.metadata().ok())
        .map(|m| m.is_file())
        != Some(true)
    {
        return err(StatusCode::NOT_FOUND, "文件不存在");
    }
    let ticket = {
        let generated = random_token();
        let mut tickets = share_tickets().lock().unwrap_or_else(|e| e.into_inner());
        prune_tickets_locked(&mut tickets);
        tickets.insert(
            generated.clone(),
            ShareTicket {
                name: name.clone(),
                expires_at: SystemTime::now() + std::time::Duration::from_secs(SHARE_TICKET_TTL_SECS),
            },
        );
        if tickets.len() > 2048 {
            if let Some(k) = tickets.keys().next().cloned() {
                tickets.remove(&k);
            }
        }
        generated
    };
    push_log(format!("生成分享票据 {name} ← {peer}"));
    Json(serde_json::json!({ "ticket": ticket, "ttl": SHARE_TICKET_TTL_SECS })).into_response()
}

/// 校验票据但不移除(peek): 有效返回票据 id。
/// raw_path 是线上原始(百分号编码)形态: 逐段解码后再走安全校验 ——
/// 票据创建时存的是解码后的文件名, 不解码则中文名票据永远匹配不上;
/// 先解码后校验也保证 %2E%2E 等穿越形态仍被 safe_rel_path 拦下。
fn valid_ticket(query: &str, raw_path: &str) -> Option<String> {
    let ticket = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("ticket="))
        .map(|v| percent_encoding::percent_decode_str(v).decode_utf8_lossy().to_string())?;
    let decoded = raw_path
        .split('/')
        .map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/");
    let name = safe_rel_path(decoded.trim_start_matches('/'), false)?;
    let mut tickets = share_tickets().lock().unwrap_or_else(|e| e.into_inner());
    prune_tickets_locked(&mut tickets); // 过期票在此被移除
    tickets
        .get(&ticket)
        .filter(|t| t.name == name)
        .map(|_| ticket)
}

/// 移除票据(消费): 仅在"完整成功 GET"后由中间件调用, 见 log_middleware 内注释。
fn consume_share_ticket(query: &str, raw_path: &str) -> bool {
    let Some(id) = valid_ticket(query, raw_path) else {
        return false;
    };
    share_tickets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
        .is_some()
}







fn safe_join_existing(share: &Path, name: &str) -> Option<PathBuf> {
    let p = share.join(name);
    let canon = p.canonicalize().ok()?;
    let base = share.canonicalize().ok()?;
    canon.starts_with(base).then_some(canon)
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

// ---------------------------------------------------------------- //
// 前端静态资源(嵌入 ../dist, 由 vite build 产出)
// ---------------------------------------------------------------- //

#[derive(RustEmbed)]
#[folder = "../dist"]
struct Frontend;

const FAVICON: &[u8] = include_bytes!("../assets/favicon.png");

async fn favicon_handler() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        FAVICON,
    )
        .into_response()
}

async fn static_handler(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // 未知 API 路径返回 404 JSON(与 Python 版一致), 不做 SPA 兜底
    if path == "api" || path.starts_with("api/") {
        return err(StatusCode::NOT_FOUND, "Not Found");
    }
    let path = if path.is_empty() { "index.html" } else { path };
    // no-store: 手机浏览器缓存住旧版 index.html/JS 时, 新服务端的协议头
    // (如 X-Upload-Hash)会对旧页面整链 400 —— 升级后必须保证拿到新页面
    match Frontend::get(path) {
        Some(f) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                f.data,
            )
                .into_response()
        }
        // SPA 兜底: 未知路径回 index.html
        None => match Frontend::get("index.html") {
            Some(f) => (
                [
                    (header::CONTENT_TYPE, "text/html".to_string()),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                f.data,
            )
                .into_response(),
            None => err(StatusCode::NOT_FOUND, "前端资源未构建(先执行 npm run build)"),
        },
    }
}

// ---------------------------------------------------------------- //
// API 实现
// ---------------------------------------------------------------- //

#[derive(Serialize)]
struct FileEntry {
    name: String,
    size: u64,
    modified: f64,
}

/// 递归收集文件(深度上限 8, 跳过上传断点临时文件); name 为相对路径
fn collect_files(dir: &Path, base: &Path, depth: usize, out: &mut Vec<FileEntry>) {
    // depth 为当前目录相对层级(根=0), 其内文件路径段数 = depth+1;
    // 用 >= 而非 >: 否则 8 层目录里的文件(9 段)会进列表, 但 safe_rel_path 拒收 9 段,
    // 出现"列得出、下载/删除全 400"的自相矛盾
    if depth >= UPLOAD_MAX_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            collect_files(&p, base, depth + 1, out);
            continue;
        }
        let fname = e.file_name().to_string_lossy().to_string();
        if is_part_artifact(&fname) || is_junk_file(&fname) {
            continue; // 断点续传中间文件/元数据、系统垃圾文件(.DS_Store 等)不进列表
        }
        if md.is_symlink() {
            continue; // 符号链接不列(下载/删除的越界校验会拒绝, 列出只会 404)
        }
        let modified = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let rel = p
            .strip_prefix(base)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(FileEntry {
            name: rel,
            size: md.len(),
            modified,
        });
    }
}

async fn files(State(share): State<PathBuf>) -> Response {
    // 同步递归遍历放进阻塞线程: 大目录时别把 tokio worker 卡住(饿死 WS 心跳等其他任务)
    let out = tokio::task::spawn_blocking(move || {
        let mut out: Vec<FileEntry> = Vec::new();
        collect_files(&share, &share, 0, &mut out);
        out.sort_by(|a, b| b.modified.total_cmp(&a.modified));
        out
    })
    .await
    .unwrap_or_default();
    Json(serde_json::json!({ "files": out })).into_response()
}

async fn download(
    State(share): State<PathBuf>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    AxPath(raw): AxPath<String>,
) -> Response {
    use tokio::io::AsyncSeekExt;
    let peer = normalize_peer(addr.ip());
    // 通配路由捕获值带前导 "/", 先去掉再做相对路径校验(读路径不拦保留设备名, 否则既有文件取不回)
    let Some(name) = safe_rel_path(raw.trim_start_matches('/'), false) else {
        return err(StatusCode::BAD_REQUEST, "非法的文件名");
    };
    let Some(path) = safe_join_existing(&share, &name) else {
        push_log_err(format!("下载失败 {name}: 文件不存在 ← {}", peer));
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        push_log_err(format!("下载失败 {name}: 打开失败 ← {}", peer));
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    // 目录必须拒绝: File::open 对目录在 Unix 上成功, ReaderStream 读到 EISDIR
    // 中途断流, 客户端拿到的是"半截/损坏的下载"而非明确错误
    if !meta.is_file() {
        return err(StatusCode::BAD_REQUEST, "不是文件");
    }
    let Ok(std_file) = std::fs::File::open(&path) else {
        push_log_err(format!("下载失败 {name}: 打开失败 ← {}", peer));
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    let size = meta.len();
    // Range 支持(单区间): 浏览器断下重试会带 Range: bytes=N- , 不支持的话 10GB 传到
    // 8GB 断掉只能从零重来; 服务端回 206 + Content-Range, Accept-Ranges 声明能力
    let mut start: u64 = 0;
    let mut end_excl: u64 = size;
    let is_partial = if let Some(rv) = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("bytes="))
    {
        // 单区间解析: Ok(true)=有效区间(206) / Ok(false)=畸形, 忽略走整体 200 / Err=超出文件(416)
        let parsed: Result<bool, ()> = (|| {
            let (a, b) = rv.split_once('-').ok_or(())?;
            let s = a.trim();
            let e = b.trim();
            if s.is_empty() {
                // 后缀区间 bytes=-N(取末尾 N 字节): 下载场景用不到, 忽略走整体 200
                return Ok(false);
            }
            // 语法无效的 Range(畸形首段)按 RFC 9110 应忽略并回整体 200, 而非 416
            // (416 只用于"语法有效但不可满足")
            let Ok(s) = s.parse::<u64>() else { return Ok(false) };
            if e.is_empty() {
                // N-
                if s >= size {
                    return Err(());
                }
                start = s;
                return Ok(true);
            }
            // N-M; 多区间("0-99,200-300" 的尾段含逗号)与畸形尾段语法无效 → 忽略走整体 200
            let Ok(e) = e.parse::<u64>() else { return Ok(false) };
            if s > e {
                return Ok(false);
            }
            if s >= size {
                return Err(());
            }
            start = s;
            // saturating_add 防 e=u64::MAX 时溢出(debug panic / release 回绕出畸形 206)
            end_excl = e.saturating_add(1).min(size);
            Ok(true)
        })();
        match parsed {
            Ok(v) => v,
            Err(()) => {
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{size}"))],
                    "范围超出文件大小",
                )
                    .into_response()
            }
        }
    } else {
        false
    };
    let mut tokio_file = tokio::fs::File::from_std(std_file);
    if is_partial {
        if let Err(e) = tokio_file.seek(std::io::SeekFrom::Start(start)).await {
            push_log_err(format!("下载失败 {name}: 定位失败({e}) ← {}", peer));
            return err(StatusCode::INTERNAL_SERVER_ERROR, "读取失败");
        }
    }
    // N-M 区间必须用 take 截断到 end_excl: ReaderStream 读到文件 EOF 为止,
    // 不截断会多发 M 之后的字节, 与 CONTENT_LENGTH 声明不符 → 破坏 HTTP 分帧
    let bounded = tokio::io::AsyncReadExt::take(tokio_file, if is_partial { end_excl - start } else { u64::MAX });
    let reader = tokio::io::BufReader::with_capacity(CHUNK_HINT, bounded);
    let stream = tokio_util::io::ReaderStream::with_capacity(reader, CHUNK_HINT);
    push_log(format!(
        "下载 {name} ({}) ← {}{}",
        humans(if is_partial { end_excl - start } else { size }),
        peer,
        if is_partial { format!(" · 续传自 {start}") } else { String::new() }
    ));
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    let enc = percent_encoding::utf8_percent_encode(&name, FRAGMENT);
    let mut rh = HeaderMap::new();
    rh.insert(header::CONTENT_TYPE, mime.essence_str().parse().unwrap());
    rh.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    if is_partial {
        rh.insert(
            header::CONTENT_RANGE,
            format!(
                "bytes {start}-{end}/{size}",
                end = end_excl.saturating_sub(1)
            )
            .parse()
            .unwrap(),
        );
        rh.insert(header::CONTENT_LENGTH, HeaderValue::from(end_excl - start));
    } else if size > 0 {
        // 显式给出长度: 浏览器原生下载可显示进度, 前端也可预知大小
        rh.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    }
    rh.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{enc}")).unwrap(),
    );
    rh.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    let status = if is_partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
    (status, rh, Body::from_stream(stream)).into_response()
}

/// 上传 —— **1.1.2 语义回滚**: 单请求整文件直传、同名直接覆盖、无断点/账本/改名/会话。
/// 仅消费 X-Filename / X-File-Size(其余头一律忽略, 遗留客户端多发无害);
/// 相对路径支持"选择文件夹"入口(独立保留的 UI 功能), 路径防线沿用 safe_rel_path + canonical 预检。
async fn upload(
    State(share): State<PathBuf>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    use tokio::io::AsyncWriteExt;
    let peer = normalize_peer(addr.ip());
    let raw = headers
        .get("X-Filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // 请求头未经过 axum 解码, 在此解一次; safe_rel_path 不再解码(避免双重解码)
    let decoded = percent_decode_str(raw).decode_utf8_lossy().to_string();
    let Some(name) = safe_rel_path(&decoded, true) else {
        return err(StatusCode::BAD_REQUEST, "缺少或非法的文件名");
    };
    // Windows MAX_PATH(260) 预检: 深目录整目录上传时 create_dir_all/open 会报
    // ERROR_FILENAME_EXCED_RANGE, 不预检会在写盘阶段才失败且文案笼统; 提前 400 + 明确文案。
    // 按 UTF-16 code unit 计(MAX_PATH 的真实计量, 中文 1 字=1 单元, 否则 3 倍过度保守);
    // 整文件直传直接落最终名, 检查最终路径即可(v1.1.5-1.1.7 还需给 .ccpart.meta 留余量, 已无此需求)
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let units: usize = share.join(&name).as_os_str().encode_wide().count();
        if units > 259 {
            push_log_warn(format!("上传拒绝 {name}: 路径过长({units} UTF-16 单元 > 260)"));
            return err(
                StatusCode::BAD_REQUEST,
                "路径过长（Windows 单路径上限 260 字符），请缩短目录层级或文件名",
            );
        }
    }
    let size: u64 = headers
        .get("X-File-Size")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if size > 0 {
        if let Ok(free) = fs2::available_space(&share) {
            if size > free {
                push_log_warn(format!(
                    "上传拒绝 {name}: 磁盘空间不足(需要 {}，可用 {}) ← {peer}",
                    humans(size),
                    humans(free)
                ));
                return err(
                    StatusCode::BAD_REQUEST,
                    format!("磁盘空间不足，需要 {}，可用 {}", humans(size), humans(free)),
                );
            }
        }
    }
    let path = share.join(&name); // safe_rel_path 逐段已校验, 不会越界
    // 越界预检必须在建目录之前: create_dir_all 会跟随符号链接, 先建后查会把目录建到界外。
    // 对"最近存在的祖先"做 canonical 校验(其下若链向界外, 在此拦截, 且未创建任何东西)
    if let Some(parent) = path.parent() {
        let mut probe = parent.to_path_buf();
        while !probe.exists() {
            match probe.parent() {
                Some(p) => probe = p.to_path_buf(),
                None => break,
            }
        }
        let (Ok(base), Ok(probe_canon)) = (share.canonicalize(), probe.canonicalize()) else {
            return err(StatusCode::INTERNAL_SERVER_ERROR, "路径校验失败");
        };
        if !probe_canon.starts_with(&base) {
            return err(StatusCode::BAD_REQUEST, "路径越界");
        }
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            push_log_err(format!("上传 {name}: 建目录失败({e}) ← {addr}"));
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("创建目录失败: {e}"));
        }
        // 事后复验(TOCTOU 窗口): 发现越界回滚 —— 非递归删除, 此刻必为空目录
        match (parent.canonicalize(), share.canonicalize()) {
            (Ok(parent_canon), Ok(base)) if parent_canon.starts_with(&base) => {}
            (Ok(_), Ok(_)) => {
                let _ = tokio::fs::remove_dir(parent).await;
                return err(StatusCode::BAD_REQUEST, "路径越界");
            }
            _ => return err(StatusCode::INTERNAL_SERVER_ERROR, "路径校验失败"),
        }
    }
    // 同名自动改名(1.1.5/1.1.6 策略恢复): 绝不覆盖已有文件 —— create_new 原子占位,
    // 并发同名被抢先则 dedup_rel_name 顺延 (1)(2)… 重试, 无 check-then-create 竞态
    let mut final_name = name.clone();
    let (path, mut file) = loop {
        // 首轮直接抢原名; 被占则每轮从**原名**重算第一个空闲变体 (1)(2)…
        let p = share.join(&final_name);
        match tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&p)
            .await
        {
            Ok(f) => break (p, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let next = dedup_rel_name(&share, &name);
                if next == final_name {
                    return err(StatusCode::INTERNAL_SERVER_ERROR, "同名文件名变体已用尽");
                }
                // Windows 审查回归: 入口 MAX_PATH 预检只测了原名, 顺延变体 +"(1)"
                // 可能刚超出 260 —— 不复查会落到笼统的「写入失败」500
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStrExt;
                    let units = share.join(&next).as_os_str().encode_wide().count();
                    if units > 259 {
                        push_log_warn(format!("上传拒绝 {name}: 同名改名后路径过长({units} UTF-16 单元 > 260)"));
                        return err(
                            StatusCode::BAD_REQUEST,
                            "同名自动改名后路径超过 Windows 260 字符上限，请缩短文件名或所在目录",
                        );
                    }
                }
                final_name = next;
            }
            Err(e) => {
                push_log_err(format!("上传失败 {name}: 创建文件失败({e}) ← {peer}"));
                return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
            }
        }
    };
    // 完整性标记(#1 数据完整性): 写首个字节前先落标记, 成功后删 ——
    // 崩溃/停服 abort/进程被杀不会走下面的错误清理, 启动时 clean_incomplete_uploads
    // 凭标记识别半截成品并连标记一并删除(否则截断文件以真实文件名永久残留、与完整文件无法区分)
    let base = final_name.rsplit('/').next().unwrap_or(final_name.as_str());
    let Some(dir) = path.parent() else {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "路径异常");
    };
    let marker = dir.join(format!(".{base}.ccup"));
    match tokio::fs::OpenOptions::new().create_new(true).write(true).open(&marker).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // 陈旧标记(正常应被启动清理移除): 清掉重建, 保证本次上传受保护
            let _ = tokio::fs::remove_file(&marker).await;
            if tokio::fs::OpenOptions::new().create_new(true).write(true).open(&marker).await.is_err() {
                let _ = tokio::fs::remove_file(&path).await;
                return err(StatusCode::INTERNAL_SERVER_ERROR, "写入标记创建失败");
            }
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&path).await; // 回收占位, 不留无标记空文件
            push_log_err(format!("上传 {final_name}: 标记创建失败({e}) ← {peer}"));
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
        }
    }
    // 在途计数(#2 数据一致性): delete_file 见 key 即 409 —— 消除"删在途文件 →
    // 上传方收 200 但字节已被移走"的假成功竞态
    let akey = active_key(&path);
    *active_uploads()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(akey.clone())
        .or_insert(0) += 1;
    push_log(format!(
        "上传开始 {name} (声明 {}) ← {peer}{}",
        if size > 0 { humans(size) } else { "未知大小".into() },
        if final_name != name {
            format!(" · 已有同名，将改存为 {final_name}")
        } else {
            String::new()
        }
    ));
    let started = std::time::Instant::now();
    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;
    let mut last_check: u64 = 0;
    loop {
        // 空闲超时(审查回归): 半开连接(信号黑洞/锁屏切换无 RST)会让读循环永久悬挂 ——
        // 120s 无任何字节即中止并清理, 同时限定半截文件可见/可下载的窗口
        let chunk = match tokio::time::timeout(
            std::time::Duration::from_secs(UPLOAD_IDLE_TIMEOUT_SECS),
            stream.next(),
        )
        .await
        {
            Ok(c) => c,
            Err(_) => {
                abort_upload(&path, &marker, &akey).await;
                push_log_warn(format!(
                    "上传中止 {name}: 空闲超时({}s 无数据)，已清理半截文件 ← {peer}",
                    UPLOAD_IDLE_TIMEOUT_SECS
                ));
                return err(StatusCode::GATEWAY_TIMEOUT, "上传超时，请重试");
            }
        };
        let Some(c) = chunk else {
            break; // body 正常结束
        };
        let Ok(c) = c else {
            abort_upload(&path, &marker, &akey).await;
            push_log_err(format!("上传中断 {name}: 连接断开，已清理半截文件 ← {peer}"));
            return err(StatusCode::INTERNAL_SERVER_ERROR, "上传中断");
        };
        // 周期查盘(审查回归): size=0 与谎报 size 的请求体绕过了启动预检,
        // 每 64MB 复查一次, 剩余不足即中止 —— 防灌满共享盘
        if written - last_check >= UPLOAD_FREE_CHECK_BYTES {
            if let Ok(free) = fs2::available_space(&share) {
                if free < UPLOAD_FREE_CHECK_BYTES {
                    abort_upload(&path, &marker, &akey).await;
                    push_log_warn(format!(
                        "上传中止 {name}: 磁盘剩余不足，已写入 {}，已清理半截文件 ← {peer}",
                        humans(written)
                    ));
                    return err(StatusCode::INSUFFICIENT_STORAGE, "磁盘空间不足，上传已中止");
                }
            }
            last_check = written;
        }
        if let Err(e) = file.write_all(&c).await {
            abort_upload(&path, &marker, &akey).await;
            push_log_err(format!("上传失败 {name}: 写入失败({e})，已清理半截文件 ← {peer}"));
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
        }
        written += c.len() as u64;
    }
    let _ = file.sync_all().await;
    // 声明了大小就必须收满: 分片/提前"正常收尾"的截断体不能当成功 ——
    // 否则标记被当"已完成"移除, 落盘即静默损坏文件, 对端拿到 Content-Length 自洽的半截
    if size > 0 && written != size {
        abort_upload(&path, &marker, &akey).await;
        push_log_warn(format!(
            "上传中止 {final_name}: 声明 {size} 字节, 实际收到 {written} 字节, 已清理 ← {peer}"
        ));
        return err(
            StatusCode::BAD_REQUEST,
            format!("上传不完整: 声明 {size} 字节, 实际收到 {written} 字节"),
        );
    }
    // 完整性标记使命完成: 成品已完整落盘。删除失败(Windows 杀软/NFS 短暂锁)时
    // 不能一走了之 —— 留下空标记会让下次启动把已回 200 的完整成品当半截删掉;
    // 改写入 "ok", 启动清理见完成标记只删标记本身
    if tokio::fs::remove_file(&marker).await.is_err() {
        let _ = tokio::fs::write(&marker, b"ok").await;
    }
    drop_active(&akey);
    push_log_ok(format!(
        "上传完成 {final_name} ({}, {}ms{}{}) ← {peer}",
        humans(written),
        started.elapsed().as_millis(),
        if final_name != name {
            format!("，改名自 {name}")
        } else {
            String::new()
        },
        ""
    ));
    // 广播列表变更(审查回归: 整文件直传回滚重写 handler 时丢失) —— WS 在线时前端
    // 已停掉 5s 列表轮询, 无此广播则手机上传完成后桌面列表永不自动刷新
    let _ = event_bus().send(serde_json::json!({
        "type": "files:changed", "action": "uploaded",
    }).to_string());
    // final: 改名后的实际落盘名(相对路径), 前端据此提示"已存为 xxx"
    Json(serde_json::json!({
        "ok": true,
        "name": name,
        "final": final_name,
        "size": written
    }))
    .into_response()
}

/// 断点查询: 返回该文件已有断点字节数(0 = 无断点/不属于本次上传会话, 从头传)。
/// 纯只读: 不创建任何目录(否则任意查询串即可在共享目录里刷出空目录树)。
#[derive(serde::Deserialize, serde::Serialize, Clone)]
struct LedgerCell {
    len: u64,
    #[serde(default)]
    hash: String,
}

/// 只接受形如 `offset => {len, hash}` 的格子; 长度为 0 或 hash 缺失视为形状非法。
/// **空对象 `{}` 是合法账本**(会话已建、尚无格登记): 拒空会把它回落成 part 长度 ——
/// 并行会话 part 预分配为全量, 回落即谎报 received=size → 前端复活路径假完成丢文件。
fn valid_ledger(raw: Option<serde_json::Value>) -> Option<HashMap<String, LedgerCell>> {
    let raw = raw?.as_object()?.clone();
    let mut out = HashMap::new();
    for (k, v) in raw {
        let Ok(off) = k.parse::<u64>() else { return None };
        let cell: LedgerCell = serde_json::from_value(v).ok()?;
        if cell.len == 0 || cell.hash.is_empty() {
            return None;
        }
        out.insert(off.to_string(), cell);
    }
    Some(out)
}

/// meta 第三行三态:
/// - Missing  = 旧版两行 meta(真 legacy 顺序会话, part 未预分配, 长度=真实已收, 可兜底);
/// - Invalid  = 有行但形状非法(v1.1.5/6 数字账本 `{"0":n}` 或损坏) —— 内容不可信:
///   进度绝不回落 part_len(预分配=全量 → 假完成), 也不得走 legacy 截断分支(会把
///   并行会话 part 截断并伪造 [0,offset) 覆盖 → 零洞脏文件), offset==0 重建、offset>0 409;
/// - Valid     = 并行账本(含空 `{}`)。
enum LedgerState {
    Missing,
    Invalid,
    Valid(HashMap<String, LedgerCell>),
}

fn ledger_state(line3: Option<&str>) -> LedgerState {
    match line3 {
        None => LedgerState::Missing,
        Some(l) => match serde_json::from_str::<serde_json::Value>(l) {
            Err(_) => LedgerState::Invalid,
            Ok(v) => match valid_ledger(Some(v)) {
                Some(m) => LedgerState::Valid(m),
                None => LedgerState::Invalid,
            },
        },
    }
}

async fn upload_progress(
    State(share): State<PathBuf>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let raw = params.get("name").map(String::as_str).unwrap_or("");
    let Some(name) = safe_rel_path(raw, true) else {
        return err(StatusCode::BAD_REQUEST, "缺少或非法的文件名");
    };
    // part 路径按会话指纹派生(带 id 时, 与 upload 一致); 无 id 的查询回落旧命名
    // (仅升级接力时可能命中升级前的旧断点)
    let id_param = params.get("id").cloned().unwrap_or_default();
    let (part, part_meta) = if id_param.is_empty() {
        let p = share.join(format!("{name}{UPLOAD_PART_SUFFIX}"));
        (p.clone(), part_meta_path(&p))
    } else {
        session_part_path(&share, &name, &id_param)
    };
    let part_len = tokio::fs::metadata(&part).await.map(|m| m.len()).ok();
    // meta 三行: 会话 id / 最终落盘名 / 分片账本(并行会话)
    let meta = tokio::fs::read_to_string(&part_meta)
        .await
        .unwrap_or_default();
    let mut meta_lines = meta.lines();
    let meta_id = meta_lines.next().unwrap_or("").trim().to_string();
    let meta_final = meta_lines
        .next()
        .map(|l| safe_rel_path(l.trim(), false).unwrap_or_default())
        .unwrap_or_default();
    let line3 = meta_lines.next();
    let state = ledger_state(line3);
    let mut ledger = match &state {
        LedgerState::Valid(cells) => Some(cells.clone()),
        _ => None,
    };
    // 并行分片会话: part 是预分配的稀疏文件, 长度恒等于全量 —— 完成度只能看账本。
    // 三态分流(关键): Valid 看账本; Missing(真 legacy, part 未预分配)才可用长度兜底;
    // Invalid(形状非法)一律 0 —— 否则预分配长度被当成全量 → 前端复活路径假完成丢文件
    let mut received = match &state {
        LedgerState::Valid(cells) => cells.values().map(|c| c.len).sum::<u64>(),
        LedgerState::Missing => part_len.unwrap_or(0),
        LedgerState::Invalid => 0,
    };
    // 带会话 id 查询时校验断点归属: 断点属于别的上传 → 返回 0(从头传), 防同名拼接脏文件。
    // **账本同样必须按归属下发**: 同名新内容是不同会话, 若把上一会话的账本发给它,
    // 客户端会把这些格子当"已覆盖"直接跳过 —— 大小相同时全部跳过 → 假报"完成",
    // 新内容根本没上传, 也不会触发同名改名(正是实测遇到的"改名不生效")
    if let Some(id) = params.get("id") {
        if meta_id != id.trim() {
            received = 0;
            ledger = None;
        } else if part_len.is_none() && !meta_final.is_empty() {
            // 断点不在但 meta 记录本会话 = 已转正: 按 meta 的**最终名**核对大小
            // (按原始名核对不上改名后的 xxx(1).zip, 会误判未完成触发整份重传)
            if let Some(s) = params.get("size").and_then(|v| v.parse::<u64>().ok()) {
                if s > 0 {
                    if let Ok(fm) = tokio::fs::metadata(share.join(&meta_final)).await {
                        if fm.is_file() && fm.len() == s {
                            received = s;
                        }
                    }
                }
            }
        }
    }
    // 断点与 meta 都不在但最终文件(原始名)已转正且大小吻合(可选 size 参数) → 报告完成:
    // 大分片下"响应在途冻结/丢失 → 客户端闲置超时中止重查"的场景,
    // 不至于把已转正的文件从头重传(与 upload handler 的"恰为目标大小即确认"同语义;
    // 兜底旧版单行 meta/无 meta 的会话 —— 并行会话账本存在时不可走此径, 空账本会被误判完成)。
    // **只对本会话或不带 id 的旧版查询生效**: 同名新会话(id 不匹配)绝不能套用
    // 上一会话的完成事实, 否则同样会假报"完成"跳过上传、不触发改名
    let id_ok = params.get("id").map_or(true, |id| meta_id == id.trim());
    // 只有真 legacy(第三行缺失)才允许"断点不在但同名成品大小吻合"的兜底:
    // Invalid(账本形状非法)走此径会拿他人同尺寸文件冒充完成
    if id_ok && matches!(state, LedgerState::Missing) && part_len.is_none() && received == 0 {
        if let Some(s) = params.get("size").and_then(|v| v.parse::<u64>().ok()) {
            if s > 0 {
                if let Ok(fm) = tokio::fs::metadata(share.join(&name)).await {
                    if fm.is_file() && fm.len() == s {
                        received = s;
                    }
                }
            }
        }
    }
    Json(serde_json::json!({
        "received": received,
        "chunks": ledger,
    }))
    .into_response()
}

/// 用户取消上传时立即清理断点, 已写入数据不再等 7 天 TTL 白占磁盘。
/// 只允许删除与 id 匹配的会话产物, 防止猜 id 删他人断点。
async fn upload_cancel(
    State(share): State<PathBuf>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let raw = params.get("name").map(String::as_str).unwrap_or("");
    let Some(name) = safe_rel_path(raw, true) else {
        return err(StatusCode::BAD_REQUEST, "缺少或非法的文件名");
    };
    let Some(id) = params.get("id").filter(|s| !s.trim().is_empty()) else {
        return err(StatusCode::BAD_REQUEST, "缺少会话标识");
    };
    let id = id.trim();
    let (part, meta) = session_part_path(&share, &name, id);
    let key = active_key(&part);
    // 判定与处置必须在同一把 active 锁内完成(inc 也在该锁内):
    // ① check 与 insert 分两次加锁的窗口里 handler 可能全部退出, 陈旧标志会在
    //    下一个同路径新会话的首个 guard drop 时被误消费;
    // ② 直删路径持锁执行, 消除"看到 active==0 但新 handler 刚 inc 进来"的窗口
    {
        let m = active_uploads().lock().unwrap_or_else(|e| e.into_inner());
        if m.get(&key).copied().unwrap_or(0) > 0 {
            cancel_requests()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key);
            return Json(serde_json::json!({ "ok": true, "removed": false, "pending": true }))
                .into_response();
        }
        let mut removed = false;
        if std::fs::metadata(&part).is_ok() {
            removed |= std::fs::remove_file(&part).is_ok();
        }
        if std::fs::metadata(&meta).is_ok() {
            removed |= std::fs::remove_file(&meta).is_ok();
        }
        drop(m);
        if removed { push_log(format!("取消上传并清理断点 {name}")); }
        Json(serde_json::json!({ "ok": true, "removed": removed })).into_response()
    }
}

/// 大小格式化 [同步: App.vue fmtSize] —— B 不带小数, 其余一位小数, 单位表两端一致
fn humans(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", units[i])
    } else {
        format!("{v:.1} {}", units[i])
    }
}

async fn delete_file(
    State(share): State<PathBuf>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    AxPath(raw): AxPath<String>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    let Some(name) = safe_rel_path(raw.trim_start_matches('/'), false) else {
        return err(StatusCode::BAD_REQUEST, "非法的文件名");
    };
    let Some(path) = safe_join_existing(&share, &name) else {
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    // 在途上传保护(#2): 直写最终名期间文件已存在, 无防护的删除会让上传方
    // "收到 200 但字节已全部消失"; key 与 upload 的占位路径同构(share.join(名))
    {
        let m = active_uploads().lock().unwrap_or_else(|e| e.into_inner());
        if m.get(&active_key(&share.join(&name))).copied().unwrap_or(0) > 0 {
            return err(StatusCode::CONFLICT, "该文件正在上传中，请等上传完成后再删除");
        }
    }
    // 标记存在 = 正在写入(占位→计数之间还有建标记一步, 计数兜不到的微窗口由它兜)
    if let Some(parent) = share.join(&name).parent() {
        if let Some(base) = Path::new(&name).file_name() {
            let marker = parent.join(format!(".{}.ccup", base.to_string_lossy()));
            if std::fs::metadata(&marker).is_ok() {
                return err(StatusCode::CONFLICT, "该文件正在上传中，请等上传完成后再删除");
            }
        }
    }
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            push_log(format!("删除 {name} ← {}", peer));
            broadcast_event("files:changed");
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(e) => {
            push_log_err(format!("删除失败 {name}: {e} ← {}", peer));
            err(StatusCode::INTERNAL_SERVER_ERROR, format!("删除失败: {e}"))
        }
    }
}

async fn chat_get() -> Response {
    let q = chat_log().lock().unwrap_or_else(|e| e.into_inner());
    // ip 仅进服务端日志, 不随接口下发 —— 局域网内没必要让所有客户端互相看到地址
    let msgs: Vec<serde_json::Value> = q
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id, "nickname": m.nickname,
                "message": m.message, "timestamp": m.timestamp,
            })
        })
        .collect();
    Json(serde_json::json!({ "messages": msgs })).into_response()
}

#[derive(serde::Deserialize)]
struct ChatIn {
    nickname: Option<String>,
    message: String,
}

async fn chat_post(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    Json(body): Json<ChatIn>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    // 剥 NUL: 前端 mdChat 用 \u0000 做占位符包裹已转义的代码块, 消息体携带 NUL
    // 可伪造占位符把代码块内容"搬运"到任意位置(渲染错乱; 无 XSS, 占位符内容已转义)
    let text = body.message.trim().chars().filter(|c| *c != '\0').collect::<String>();
    if text.is_empty() {
        return err(StatusCode::BAD_REQUEST, "消息内容不能为空");
    }
    let nickname = (body.nickname.unwrap_or_default().trim().to_string())
        .chars()
        .filter(|c| *c != '\0')
        .take(NICK_MAX_CHARS)
        .collect::<String>();
    let nickname = if nickname.is_empty() { DEFAULT_NICK.into() } else { nickname };
    let message = text.chars().take(MSG_MAX_CHARS).collect::<String>();
    // id 分配与入队必须在同一临界区: 分两段加锁时并发发送会算出重复 id,
    // 前端 :key="m.id" 出现重复键导致列表渲染错乱
    let msg = {
        let mut q = chat_log().lock().unwrap_or_else(|e| e.into_inner());
        let id = q.back().map(|m| m.id + 1).unwrap_or(1);
        let msg = ChatMsg {
            id,
            nickname,
            message,
            timestamp: now_f64(),
            ip: peer.to_string(),
        };
        q.push_back(msg.clone());
        while q.len() > CHAT_MAX {
            q.pop_front();
        }
        msg
    };
    let id = msg.id;
    let preview: String = msg.message.chars().take(CHAT_PREVIEW_CHARS).collect();
    let dots = if msg.message.chars().count() > CHAT_PREVIEW_CHARS { "…" } else { "" };
    push_log(format!("聊天 {}: {preview}{dots} ← {}", msg.nickname, peer));
    broadcast_event("chat:new");
    Json(serde_json::json!({ "ok": true, "id": id })).into_response()
}

// ---------------------------------------------------------------- //
// WebSocket: 事件推送(聊天/列表/状态), 轮询为降级通道
// ---------------------------------------------------------------- //

async fn ws_handler(
    ws: axum::extract::ws::WebSocketUpgrade,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    let mut rx = event_bus().subscribe();
    let mut kick = ws_kick().subscribe();
    push_log(format!("WS 连接建立 ← {peer}"));
    ws.on_upgrade(move |mut socket: axum::extract::ws::WebSocket| async move {
        // 应用层心跳: 半开连接(漫游/AP 休眠)靠它暴露, 客户端看门狗据此强制重连
        let mut hb = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = kick.recv() => break, // 停服: 主动断开, 别让优雅关闭等挂起的连接
                _ = hb.tick() => {
                    if socket
                        .send(axum::extract::ws::Message::Text(
                            "{\"type\":\"hb\"}".into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                ev = rx.recv() => match ev {
                    Ok(msg) => {
                        if socket.send(axum::extract::ws::Message::Text(msg)).await.is_err() {
                            break;
                        }
                    }
                    // 慢消费者: 跳过积压事件, 客户端收到事件后本就会全量拉取
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                incoming = socket.recv() => match incoming {
                    Some(Ok(axum::extract::ws::Message::Ping(p))) => {
                        if socket.send(axum::extract::ws::Message::Pong(p)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(axum::extract::ws::Message::Close(_))) | None => break,
                    _ => {}
                },
            }
            // 循环退出(停服踢出/发送失败/客户端主动关闭)统一记一条断开
            push_log(format!("WS 连接断开 ← {peer}"));
        }
    })
}

/// 下发给前端的可调参数: config.rs 为唯一事实, 前端收到后覆盖本地兜底值(消灭双写漂移)
fn frontend_config() -> serde_json::Value {
    serde_json::json!({
        "nickMax": NICK_MAX_CHARS,
        "msgMax": MSG_MAX_CHARS,
        "defaultNick": DEFAULT_NICK,
        "chatMax": CHAT_MAX,
        "poll": {
            "chat": POLL_CHAT_MS,
            "files": POLL_FILES_MS,
            "status": POLL_STATUS_MS,
            "logs": POLL_LOGS_MS,
        },
    })
}

async fn info() -> Response {
    let (running, port, ip) = status();
    Json(serde_json::json!({
        "ip": ip, "port": port, "running": running,
        "version": APP_VERSION,
        "config": frontend_config(),
    }))
    .into_response()
}

async fn ctrl_status() -> Response {
    let (running, port, ip) = status();
    Json(serde_json::json!({
        "running": running, "port": port, "ip": ip,
        "version": APP_VERSION,
        "config": frontend_config(),
    }))
    .into_response()
}

// ---- Tauri 命令(桌面窗口经 invoke 调用, 免端口发现/CORS) ----

/// 磁盘空间(共享目录所在卷): (可用, 总量)
pub fn disk_space() -> (Option<u64>, Option<u64>) {
    (
        fs2::available_space(share_dir()).ok(),
        fs2::total_space(share_dir()).ok(),
    )
}

/// 访问码持久化文件(与目录/端口偏好同目录; Android 放 app 专属目录)
fn pin_pref_file() -> PathBuf {
    #[cfg(target_os = "android")]
    if let Some(d) = platform_default_dir() {
        return Path::new(d).join(PIN_PREF_FILE);
    }
    #[cfg(target_os = "windows")]
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".into());
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(PIN_PREF_FILE)
}

/// 访问码(可选): Some(pin) 时非回环的 API/WS 请求必须携带 ?pin= 或 X-Access-Pin。
/// 状态持久化(重启回载, 保护不静默消失), HTTP /api/info 不下发码值; 回环(本机/桌面壳)豁免
fn access_pin() -> &'static Mutex<Option<String>> {
    static PIN: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    PIN.get_or_init(|| {
        // 启动回载: 文件存在且内容非空即视为开启(码值即文件内容)
        let saved = std::fs::read_to_string(pin_pref_file())
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty());
        Mutex::new(saved)
    })
}

// ---- 访问码爆破防护: 按来源 IP 记连续失败次数, 达到阈值后指数退避 ---- //
// 6 位数字码共 90 万组合, 无节流时局域网百级 req/s 数小时即可穷举。
// 只锁"错误尝试", 成功配对立即清零; 锁定窗口不因持续轰炸延长(到期自动解,
// 攻击者无法把合法用户永久锁在外面); 表容量有界, 超限时回收 1h 内无失败的旧条目

/// 连续失败达到该次数后开始退避(前几次留给手滑输错的真实用户)
const PIN_FAIL_FREE: u32 = 8;
/// 基础退避秒数: 第 8 次失败锁 15s, 之后每次翻倍, 上限 1h
const PIN_FAIL_BASE_SECS: u64 = 15;
const PIN_FAIL_MAX_SECS: u64 = 3600;
/// IPv6 /64 聚合层(隐私扩展轮换接口地址不换前缀, 轮换刷不掉聚合窗口):
/// 阈值放宽 5 倍、基础退避 60s —— 提高分布式试码成本, 又不误伤同网段真人
const PIN6_FAIL_FREE: u32 = 40;
const PIN6_BASE_SECS: u64 = 60;

fn pin_fails() -> &'static Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static FAILS: OnceLock<Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    FAILS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn pin_prefix_fails() -> &'static Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static FAILS: OnceLock<Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    FAILS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// IPv6 聚合键: /64 前缀(同网段设备同前缀, 隐私扩展只换接口标识); v4 与已折叠的
/// v4-mapped 返回 None —— v4 无轮换诉求, 单 IP 层足够
fn pin_prefix6(ip: std::net::IpAddr) -> Option<std::net::IpAddr> {
    match ip {
        std::net::IpAddr::V6(v6) => {
            let o = v6.octets();
            let mut p = [0u8; 16];
            p[..8].copy_from_slice(&o[..8]);
            Some(std::net::IpAddr::V6(p.into()))
        }
        _ => None,
    }
}

/// 滑动退避锁定判定: 累计失败 ≥ free 后, 距上次失败不足 base×2^(n-free) 秒(封顶 max)内为锁定。
/// 锁定窗内**不计数不延长**(轰炸无法把窗口推到无穷, 到期自动解)
fn pin_locked(
    map: &mut std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>,
    key: std::net::IpAddr,
    now: std::time::Instant,
    free: u32,
    base: u64,
    max: u64,
) -> bool {
    let Some(&(n, t)) = map.get(&key) else {
        return false;
    };
    if n < free {
        return false;
    }
    let extra = n.saturating_sub(free).min(16) as u32;
    let wait = base.checked_shl(extra).unwrap_or(max).min(max);
    now.duration_since(t) < std::time::Duration::from_secs(wait)
}

fn pin_count(
    map: &mut std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>,
    key: std::net::IpAddr,
    now: std::time::Instant,
) -> u32 {
    let e = map.entry(key).or_insert((0, now));
    e.0 = e.0.saturating_add(1);
    e.1 = now;
    e.0
}

/// 常量时间比较(纵深防御): 等长后逐字节异或累计, 无逐字符短路时序差。
/// 网络抖动下时序攻击实操不可行, 此处成本为零
fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 配对尝试准入(双层滑动退避): ① 单 IP 指数退避(公平: 只锁自己) ② IPv6 额外按 /64
/// 聚合计数(隐私扩展轮换接口地址刷新不了聚合窗口; 阈值放宽 5 倍防误伤同网段真人)。
/// 成功清零两层; 失败时任一层处于锁定窗内 → false(429 且两层都不计数不延长)。
/// 调用方语义: false → 429; true 且 !ok → 401
fn pin_admit(peer: std::net::IpAddr, ok: bool) -> bool {
    let now = std::time::Instant::now();
    let pfx = pin_prefix6(peer);
    // 锁判定先行(与调用处"锁内正确码也必须拒"的注释对齐): 若正确码在锁内短路放行,
    // 锁窗就成为对/错 oracle(200 vs 429) —— 攻击者每窗全速试一轮且失败不延长窗口,
    // 指数退避形同虚设, 6位码可逐窗穷举。代价: 被锁的就是攻击者自己的设备(自锁),
    // 同 /64 他人等一个窗周期后任一正确码即清零恢复 —— 可用性换安全, 值得。
    // ② 聚合层: /64 窗口锁定中 → 429(两层都不计数, 轮换地址与持续轰炸同样徒劳)
    if let Some(p) = pfx {
        let mut pm = pin_prefix_fails().lock().unwrap_or_else(|e| e.into_inner());
        if pm.len() > 4096 {
            pm.retain(|_, (_, t)| now.duration_since(*t).as_secs() < PIN_FAIL_MAX_SECS);
        }
        if pin_locked(&mut pm, p, now, PIN6_FAIL_FREE, PIN6_BASE_SECS, PIN_FAIL_MAX_SECS) {
            return false;
        }
        drop(pm);
    }
    // ① 单 IP 层: 锁定窗内不计数不延长
    {
        let mut m = pin_fails().lock().unwrap_or_else(|e| e.into_inner());
        if m.len() > 1024 {
            m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < PIN_FAIL_MAX_SECS);
        }
        if pin_locked(&mut m, peer, now, PIN_FAIL_FREE, PIN_FAIL_BASE_SECS, PIN_FAIL_MAX_SECS) {
            return false;
        }
        if ok {
            // 两层都未锁定才走到: 正确码清零两层计数并放行
            m.remove(&peer);
            drop(m);
            if let Some(p) = pfx {
                pin_prefix_fails().lock().unwrap_or_else(|e| e.into_inner()).remove(&p);
            }
            return true;
        }
        // 失败才计数(ok 已在上方返回): /64 聚合层与单 IP 层各记一次 ——
        // 锁序改为"判定先行"后前缀计数原在判定块内, 重排时必须补回, 否则前缀锁永不触发
        if let Some(p) = pfx {
            let mut pm = pin_prefix_fails().lock().unwrap_or_else(|e| e.into_inner());
            pin_count(&mut pm, p, now);
        }
        let n = pin_count(&mut m, peer, now);
        if n == PIN_FAIL_FREE {
            // 锁定窗从此刻开始(此后窗口内该 IP 一律 429): 桌面日志给出明确安全事件。
            // 次数用 n(含本次)而非自增前值 —— 否则第 8 次失败日志写成 7 次
            push_log_warn(format!(
                "访问码连续输错 {n} 次，已锁定 {peer}，{}s 内拒绝重试(指数退避)",
                PIN_FAIL_BASE_SECS
            ));
        }
    }
    true
}

#[tauri::command]
pub fn set_access_pin_cmd(enable: bool) -> serde_json::Value {
    let mut slot = access_pin().lock().unwrap_or_else(|e| e.into_inner());
    if !enable {
        *slot = None;
        let _ = std::fs::remove_file(pin_pref_file());   // 同步撤盘, 重启后不再回载
        push_log_warn("访问令牌已关闭 —— 局域网设备不再需要配对".into());
        return serde_json::json!({ "ok": true });
    }
    // 6 位数字: 时间纳秒 ^ 进程id ^ 自增序号, 经 splitmix64 扩散 ——
    // 避免纯时间戳类的可预测性(旁观者知道大概开启时刻也猜不中)
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ ((std::process::id() as u64) << 32)
        ^ SEQ
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    let pin = format!("{:06}", 100000 + x % 900000);
    *slot = Some(pin.clone());
    let _ = std::fs::write(pin_pref_file(), &pin);   // 落盘: 重启后保护自动恢复
    push_log_ok("访问令牌已开启（已生成新配对码，旧分享链接随之失效）".into());
    // Unix 下收成 0600: 配对码不能对同机其他本地用户可读
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(pin_pref_file(), std::fs::Permissions::from_mode(0o600));
    }
    serde_json::json!({ "ok": true, "pin": pin })
}

#[tauri::command]
pub fn control_status_cmd() -> serde_json::Value {
    let (running, port, ip) = status();
    let share = share_dir().to_string_lossy().to_string();
    let (free, total) = disk_space();
    let pin = access_pin().lock().unwrap_or_else(|e| e.into_inner()).clone();
    serde_json::json!({
        "running": running, "port": port, "ip": ip, "share": share,
        "free": free.map(humans).unwrap_or_else(|| "无法获取".into()),
        "total": total.map(humans).unwrap_or_else(|| "无法获取".into()),
        "version": APP_VERSION,
        "config": frontend_config(),
        // 访问码仅供可信桌面壳(设置页展示), HTTP 通道不下发
        "pinEnabled": pin.is_some(),
        "openLan": open_lan(),
        "pin": pin.unwrap_or_default(),
    })
}

#[tauri::command]
pub fn control_start_cmd(port: Option<u16>) -> serde_json::Value {
    match start(port) {
        Ok(port) => serde_json::json!({ "ok": true, "port": port }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    }
}

#[tauri::command]
pub fn control_stop_cmd() -> serde_json::Value {
    serde_json::json!({ "ok": stop() })
}

#[tauri::command]
pub fn control_clear_logs_cmd() -> serde_json::Value {
    clear_logs();
    serde_json::json!({ "ok": true })
}

#[tauri::command]
pub fn set_open_lan_cmd(open: bool) -> serde_json::Value {
    set_open_lan(open);
    serde_json::json!({ "ok": true, "openLan": open })
}

/// 高风险共享目录: 家目录本身 / 家目录的祖先(/Users、C:\Users 等) / 磁盘根。
/// 共享这些位置等于把个人全部文件对整个 Wi-Fi 开放读写删, 前端须二次确认(force)后才放行
fn is_dangerous_share_dir(p: &Path) -> bool {
    #[cfg(target_os = "windows")]
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var("HOME").unwrap_or_default();
    let home_path = Path::new(&home);
    if !home.is_empty() {
        // 家目录本身, 或其任意祖先(覆盖 / 、/Users 、C:\ 、C:\Users)
        if p == home_path || home_path.starts_with(p) {
            return true;
        }
    }
    // 文件系统根(parent 为 None, 如 "/")
    p.parent().is_none()
}

#[tauri::command]
pub fn set_share_dir_cmd(path: String, force: Option<bool>) -> serde_json::Value {
    // 运行中拒绝: 服务持的是启动时的目录快照, 热改只会造成"UI 显示与实际共享不一致";
    // 也堵住"注入脚本趁服务运行改目录、下次启动静默共享别处"的路径(纵深防御)
    if is_running() {
        return serde_json::json!({ "ok": false, "error": "服务运行中不可更改共享目录，请先停止服务" });
    }
    if !Path::new(&path).is_dir() {
        return serde_json::json!({ "ok": false, "error": "目录不存在" });
    }
    if !force.unwrap_or(false) && is_dangerous_share_dir(Path::new(&path)) {
        return serde_json::json!({ "ok": false, "dangerous": true, "error": "高风险目录" });
    }
    set_share_dir(PathBuf::from(&path));
    if force.unwrap_or(false) {
        push_log_warn(format!("共享目录已切换为 {path}（高风险目录，用户已二次确认）"));
    } else {
        push_log(format!("共享目录已切换为 {path}（下次启动生效）"));
    }
    serde_json::json!({ "ok": true })
}

#[tauri::command]
pub fn open_path_cmd(path: String) -> serde_json::Value {
    match open_path(&path) {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    }
}

#[tauri::command]
pub fn open_url_cmd(url: String) -> serde_json::Value {
    let allowed = url.starts_with("http://") || url.starts_with("https://");
    match if allowed { open_with_system(&url) } else { Err("仅允许 http/https 链接".into()) } {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    }
}

/// 复制文件本体到系统剪贴板(访达/资源管理器可直接粘贴)。
/// macOS: osascript POSIX file · Windows: PowerShell Set-Clipboard · Linux: 暂不支持
#[tauri::command]
pub fn copy_file_cmd(name: String) -> serde_json::Value {
    let Some(name) = safe_rel_path(&name, false) else {
        return serde_json::json!({ "ok": false, "error": "非法的文件名" });
    };
    // 读路径放行了 Windows 非法字符(既有 Unix 合法文件也要能复制), 盘符形态
    // `share.join("C:x")` 在 Windows 会整体替换基准目录 —— 必须 canonical + 前缀校验
    let share = share_dir();
    let Some(path) = safe_join_existing(&share, &name) else {
        return serde_json::json!({ "ok": false, "error": "文件不存在或路径越界" });
    };
    if !path.is_file() {
        return serde_json::json!({ "ok": false, "error": "文件不存在" });
    }
    let p = path.to_string_lossy().to_string();
    // Windows canonicalize 返回 \\?\verbatim 形态, 资源管理器 CF_HDROP 粘贴对
    // verbatim 路径兼容性差(可能粘出无效项) —— 校验已由 canonical 完成, 这里只剥展示形态
    #[cfg(windows)]
    let p = {
        if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else if let Some(rest) = p.strip_prefix(r"\\?\") {
            rest.to_string()
        } else {
            p
        }
    };
    // macOS: 路径经 ARGV 传参而非拼进 AppleScript 源码 —— 共享目录路径本身可含 \" 组合
    // (macOS 均为合法文件名字符), 字符串转义在 AppleScript 里不可靠, 拼接可导致代码注入
    #[cfg(target_os = "macos")]
    let res = std::process::Command::new("osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "set the clipboard to POSIX file (item 1 of argv)",
            "-e",
            "end run",
            "--",
            &p,
        ])
        .output();
    // Windows: Set-Clipboard 没有 -LiteralPath 参数(只有 -Value, 放进去的是路径文本);
    // 要"资源管理器直接粘贴出文件"必须写 CF_HDROP 文件丢弃列表 → SetFileDropList。
    // STA 是 Clipboard 调用的线程模型要求
    #[cfg(target_os = "windows")]
    let res = {
        let ps = format!(
            "Add-Type -AssemblyName System.Windows.Forms; \
             $c = New-Object System.Collections.Specialized.StringCollection; \
             $c.Add('{}') | Out-Null; \
             [System.Windows.Forms.Clipboard]::SetFileDropList($c)",
            p.replace('\'', "''"));
        let mut cmd = std::process::Command::new("powershell");
        cmd.args(["-STA", "-NoProfile", "-Command", &ps]);
        // CREATE_NO_WINDOW: 否则每点一次"复制"都闪一个 PowerShell 黑窗(像流氓软件)
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        cmd.output()
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let res: Result<std::process::Output, std::io::Error> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported, "Linux/Android 暂不支持复制文件到剪贴板"));
    match res {
        Ok(out) if out.status.success() => {
            serde_json::json!({ "ok": true })
        }
        Ok(out) => serde_json::json!({
            "ok": false,
            "error": String::from_utf8_lossy(&out.stderr).trim().to_string()
        }),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    }
}

/// HTTP 控制端点不能在 async handler 里 block_on(start/stop 内部用了 block_on,
/// 在 tokio runtime 中会 panic "Cannot start a runtime from within a runtime"):
/// 挪到独立线程执行, 带超时回收。
fn run_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || { let _ = tx.send(f()); });
    rx.recv_timeout(std::time::Duration::from_secs(10)).ok()
}

async fn ctrl_start() -> Response {
    match run_blocking(|| start(last_preferred_port())) {
        Some(Ok(port)) => Json(serde_json::json!({ "ok": true, "port": port })).into_response(),
        Some(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
        None => err(StatusCode::GATEWAY_TIMEOUT, "启动超时"),
    }
}

async fn ctrl_stop() -> Response {
    match run_blocking(|| stop()) {
        Some(ok) => Json(serde_json::json!({ "ok": ok })).into_response(),
        None => err(StatusCode::GATEWAY_TIMEOUT, "停止超时"),
    }
}

async fn ctrl_logs() -> Response {
    let q = req_log().lock().unwrap_or_else(|e| e.into_inner());
    Json(serde_json::json!({ "logs": q.iter().cloned().collect::<Vec<_>>() })).into_response()
}

/// 清空服务日志(保留一条标记, 便于确认清空动作已生效)。
pub fn clear_logs() {
    req_log().lock().unwrap_or_else(|e| e.into_inner()).clear();
    push_log("日志已清空".into());
}

async fn ctrl_logs_clear() -> Response {
    clear_logs();
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// 从 User-Agent 猜测设备类型(仅用于日志展示)。
fn device_kind(ua: &str) -> String {
    for (kw, label) in [
        ("iPhone", "iPhone"),
        ("iPad", "iPad"),
        ("Android", "Android"),
        ("MicroMessenger", "微信内置"),
        ("Macintosh", "Mac"),
        ("Windows", "Windows"),
        ("Linux", "Linux"),
    ] {
        if ua.contains(kw) {
            return label.to_string();
        }
    }
    "未知设备".into()
}

/// 浏览器跨源防护: 阻断"恶意网页借用户浏览器打本机服务"(drive-by CSRF/数据外带)。
/// 无鉴权的局域网定位不变, 但至少不能让互联网上的任意页面操作本机接口:
/// - Host 必须是 IP 或 localhost(挡 DNS 重绑定);
/// - 带 Origin 的请求(浏览器跨源/预检后真实请求)只放行 自身 / WebView(tauri://localhost、tauri.localhost);
/// - 无 Origin 的非浏览器客户端(curl、局域网 App)不受影响。
fn browser_origin_guard(req: &axum::extract::Request) -> Option<Response> {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // IPv6 字面量 Host("[::1]:1688")不能按 ':' 硬切(会截出 "[")——必须先判括号形态,
    // 从原始 host 上取 "]" 之前的 "[::1" 解析; 只有非括号 Host 才按 ':' 摘端口
    let host_ok = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']')
            .next()
            .map(|s| s.parse::<std::net::IpAddr>().is_ok())
            .unwrap_or(false)
    } else {
        let host_name = host.split(':').next().unwrap_or("");
        // 允许: IP / localhost。曾放行 *.local —— 但同 Wi-Fi 攻击者可自架 mDNS 应答:
        // 先让受害者从攻击者机器加载页面, 再把 evil.local 重绑到受害者 IP, 页面同源
        // 请求直连其本机服务(源地址是本机 → is_local_source 豁免 pin), 攻击者 JS 即可
        // 拿到无码的 files/upload/delete。二维码/地址栏给的都是 IP, 收紧不损失正常路径
        host_name == "localhost" || host_name.parse::<std::net::IpAddr>().is_ok()
    };
    if !host_ok {
        return Some(err(StatusCode::BAD_REQUEST, "非法的 Host"));
    }
    if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        // 全部精确匹配: 子串匹配可被 tauri.localhost.evil.com 绕过(该域名合法注册即含此子串),
        // 叠加 CorsLayer::permissive 的 ACAO:* 后攻击者 JS 可读取全部响应
        let self_ok = origin == format!("http://{host}")
            || origin == "tauri://localhost"          // macOS/iOS WKWebView
            || origin == "http://tauri.localhost"     // Windows WebView2
            || origin == "https://tauri.localhost"    // Android WebView(https 源)
            // tauri dev: 窗口加载自 devServer(5173); 仅回环源, 风险限本机开发场景
            || origin.starts_with("http://localhost:")
            || origin.starts_with("http://127.0.0.1:");
        if !self_ok {
            return Some(err(StatusCode::FORBIDDEN, "跨源请求被拒绝"));
        }
    }
    None
}

/// 日志脱敏: 查询串里的访问码不落日志(日志可能被截图/复制分享, 码一泄即失效)
fn mask_pin(query: &str) -> String {
    query
        .split('&')
        .map(|kv| if kv.starts_with("pin=") { "pin=***" } else if kv.starts_with("ticket=") { "ticket=***" } else { kv })
        .collect::<Vec<_>>()
        .join("&")
}

/// 判定请求来源是否为本机: 回环地址, 或源地址是本机任一网卡的地址。
/// 同一台电脑的浏览器用局域网 IP 访问本机服务时(如 http://10.x.x.x:1688),
/// TCP 源地址是本机网卡 IP 而非 127.0.0.1 —— 只判回环会把"坐在电脑前的人"当成
/// 远程设备锁在访问码外面。伪造该源地址需完成 TCP 握手(收不到 SYN-ACK), 不可冒充
fn is_local_source(ip: std::net::IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    // 判定"该 IP 是否挂在本机网卡上": 对其绑定一个 0 端口 UDP socket ——
    // 本机地址内核必接受, 非本机地址 EADDRNOTAVAIL。不依赖网卡枚举
    // (get_if_addrs 异常时也不会误判成远程), 覆盖全部接口类型
    std::net::UdpSocket::bind(std::net::SocketAddr::new(ip, 0)).is_ok()
}

/// 双栈监听([::])下 IPv4 连接的源地址是 v4-mapped 形态(::ffff:a.b.c.d), 而
/// is_loopback()/is_private()/UDP 绑定探测都只认纯 v4 —— 127.0.0.1 会被控制接口
/// 误判成远程设备 403(桌面窗口的日志面板因此一直拉不到), 同机浏览器会被访问码误拦。
/// 统一折回纯 v4 再判定; 真实 IPv6 客户端(非映射形态)原样保留
fn normalize_peer(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(std::net::IpAddr::V4)
            .unwrap_or(ip),
        _ => ip,
    }
}

/// 请求日志中间件: 每个 HTTP 请求记一行(方法/路径/状态/耗时/来源)。
/// 高频轮询噪音不记录, 清单见 config.rs 的 NOISY_GET_PATHS
/// (与前端 config.js 的轮询路径一一对应, 前端调整轮询时需同步)。
async fn log_middleware(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let peer = normalize_peer(addr.ip());
    if let Some(resp) = browser_origin_guard(&req) {
        return resp;
    }
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // 控制接口(启停/日志/清空)仅限本机: 桌面壳走 127.0.0.1 回环或 Tauri 命令,
    // 局域网任意设备可 curl 调用的话, 任何人能关停服务/读走全部活动日志(文件名+IP+聊天预览)
    if path.starts_with("/api/control/") && !peer.is_loopback() {
        return err(StatusCode::FORBIDDEN, "控制接口仅限本机访问");
    }
    let query = req.uri().query().unwrap_or("").to_string();
    // 访问码(可选, 设置页开启): 非本机来源的 API/WS 请求必须携带 ?pin= 或 X-Access-Pin。
    // 本机来源(回环 + 本机网卡地址, 覆盖"同机浏览器经局域网 IP 访问")豁免 ——
    // 在电脑前的人本就拥有完全控制权; 安卓/iOS 壳走 127.0.0.1 同样命中。
    // 静态页面不走此中间件, 网页端先加载页面再由 401 引导输入访问码
    if (path == "/api" || path.starts_with("/api/") || path == "/ws") && !is_local_source(peer) {
        // 分享票据先于访问码校验: 票据是一次性、绑定单文件的下载凭证 ——
        // 若放在 pin 之后, 开启访问码时仅带 ?ticket= 的分享链接会先吃 401,
        // 接收方被迫索要全站配对码, 与"链接仅授权下载该文件"的承诺矛盾
        if path.starts_with("/api/download/") {
            // 单次剥离前缀(与 handler 的 AxPath 对齐): trim_start_matches 会循环剥,
            // /api/download/api/download/x 会被剥成 x 造成票据绑定错位(#3)
            let raw_path = path.strip_prefix("/api/download").unwrap_or("");
            if valid_ticket(&query, raw_path).is_some() {
                // peek 不销票; 完整成功 GET 才消费(#4) —— HEAD/Range 断点续传与
                // 失败重试在 TTL(10min) 内仍可用, 一次性语义由"全量成功即焚"保证
                // Range 从 0 起(bytes=0- / bytes=0-123)语义就是全量下载, 同样销票 ——
                // 否则一个链接可用 Range 头无限次拉全文; 从 N>0 的续传不销票(TTL 内可断点)
                let range = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok());
                let full_get = method == "GET"
                    && match range {
                        None => true,
                        Some(r) => r.trim().starts_with("bytes=0-"),
                    };
                let res = next.run(req).await;
                if full_get && res.status().is_success() {
                    consume_share_ticket(&query, raw_path);
                }
                return res;
            }
        }
        let pin_enabled = access_pin().lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(pin) = pin_enabled {
            let given = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("pin="))
                .map(|v| percent_encoding::percent_decode_str(v).decode_utf8_lossy().to_string())
                .or_else(|| {
                    req.headers()
                        .get("X-Access-Pin")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from)
                });
            let ok = ct_eq(given.as_deref().unwrap_or(""), &pin);
            // 爆破防护: 只统计"实际提交了配对码"的尝试(未配对设备的无码轮询不计,
            // 否则正常页面十几秒把自己轰进锁定)。带码连续失败达阈值后按 IP 指数退避。
            // 锁定窗内一律 429 —— 正确码也必须拒: 若放行, 对/错码的响应可区分,
            // 攻击者伪造 Accept: text/html(导航形态)即可全速逐个试码, 退避形同虚设。
            // 逐 IP 计数使攻击者只能锁住自己的源地址, 不存在"锁害他人"; 窗不因
            // 轰炸延长(pin_admit 锁定中不记数), 到期自动解, 最短 15s 对真人无感
            // 只有真正提交了配对码才进入失败计数；无码请求直接走 401，
            // 否则正常页面轮询会把自己轰进退避锁定。
            let admitted = if given.is_some() { pin_admit(peer, ok) } else { true };
            let is_nav = method == "GET"
                && req
                    .headers()
                    .get(header::ACCEPT)
                    .and_then(|v| v.to_str().ok())
                    .map_or(false, |a| a.contains("text/html"));
            if !admitted {
                // 导航请求(地址栏/表单)回可重新提交的 HTML 表单页, 其余回 JSON;
                // 两者状态码同为 429 —— 不给"这次码对没对"的任何信号
                if is_nav {
                    let esc = path
                        .replace('&', "&amp;")
                        .replace('"', "&quot;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;");
                    let html = format!(
                        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
                         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
                         <title>尝试过于频繁</title></head>\
                         <body style=\"font-family:-apple-system,'PingFang SC','Microsoft YaHei',sans-serif;background:#0d1017;color:#e1e4ed;display:flex;min-height:100vh;align-items:center;justify-content:center;margin:0\">\
                         <div style=\"background:#161b26;border:1px solid rgba(255,255,255,0.08);border-radius:14px;padding:30px 26px;max-width:340px;width:88%;text-align:center\">\
                         <h1 style=\"font-size:18px;margin:0 0 8px\">尝试过于频繁</h1>\
                         <p style=\"font-size:13px;color:#9ca3b8;margin:0 0 16px;line-height:1.6\">配对尝试过于频繁，请稍后再试</p>\
                         <form method=\"get\" action=\"{esc}\">\
                         <input name=\"pin\" required placeholder=\"配对码\" style=\"width:100%;box-sizing:border-box;padding:13px 12px;font-size:16px;text-align:center;letter-spacing:3px;border:1px solid rgba(255,255,255,0.1);border-radius:12px;background:rgba(255,255,255,0.06);color:#e1e4ed;outline:none\">\
                         <button type=\"submit\" style=\"width:100%;margin-top:14px;padding:13px 0;font-size:15px;font-weight:600;letter-spacing:8px;text-indent:8px;border:none;border-radius:12px;background:linear-gradient(135deg,#4f8af7,#7c6ff7);color:#fff;cursor:pointer\">配 对</button>\
                         </form></div></body></html>"
                    );
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())],
                        html,
                    )
                        .into_response();
                }
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(serde_json::json!({
                        "error": "配对尝试过于频繁，请稍后再试",
                        "pinRequired": true
                    })),
                )
                    .into_response();
            }
            if !ok {
                // 浏览器地址栏直接打开分享链接(导航请求, Accept 含 text/html)时返回可输入
                // 访问码的 HTML 中转页而非裸 JSON —— 拿到旧链接/未带码的接收方当场补输
                // 正确码即可继续下载; fetch(网页端)仍是 JSON, 由页面 401 弹框统一处理
                if is_nav {
                    // path 是线上原始(已编码)形态, 仅做 HTML 属性转义, 不得再走一次
                    // 百分号编码(否则 %E6.. 变 %25E6.. 双重编码指错文件)
                    let esc = path
                        .replace('&', "&amp;")
                        .replace('"', "&quot;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;");
                    let html = format!(
                        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
                         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
                         <title>需要配对</title></head>\
                         <body style=\"font-family:-apple-system,'PingFang SC','Microsoft YaHei',sans-serif;background:#0d1017;color:#e1e4ed;display:flex;min-height:100vh;align-items:center;justify-content:center;margin:0\">\
                         <div style=\"background:#161b26;border:1px solid rgba(255,255,255,0.08);border-radius:14px;padding:30px 26px;max-width:340px;width:88%;text-align:center\">\
                         <h1 style=\"font-size:18px;margin:0 0 8px\">需要配对</h1>\
                         <p style=\"font-size:13px;color:#9ca3b8;margin:0 0 16px;line-height:1.6\">输入电脑「访问令牌」完成配对</p>\
                         <form method=\"get\" action=\"{esc}\">\
                         <input name=\"pin\" required placeholder=\"配对码\" style=\"width:100%;box-sizing:border-box;padding:13px 12px;font-size:16px;text-align:center;letter-spacing:3px;border:1px solid rgba(255,255,255,0.1);border-radius:12px;background:rgba(255,255,255,0.06);color:#e1e4ed;outline:none\">\
                         <button type=\"submit\" style=\"width:100%;margin-top:14px;padding:13px 0;font-size:15px;font-weight:600;letter-spacing:8px;text-indent:8px;border:none;border-radius:12px;background:linear-gradient(135deg,#4f8af7,#7c6ff7);color:#fff;cursor:pointer\">配 对</button>\
                         </form></div></body></html>"
                    );
                    return (
                        StatusCode::UNAUTHORIZED,
                        [(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())],
                        html,
                    )
                        .into_response();
                }
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({ "error": "需要配对码", "pinRequired": true })),
                )
                    .into_response();
            }
        }
    }
    // 昵称: 优先 X-Nickname 头, 其次 ?nick= 参数(浏览器端避免自定义头触发 CORS 预检)
    let qnick = query.split('&').find_map(|kv| kv.strip_prefix("nick=")).map(|v| {
        percent_encoding::percent_decode_str(v).decode_utf8_lossy().to_string()
    });
    // 日志昵称消毒: 剥 CR/LF 防伪造日志条目; 截断到 NICK_MAX(查询参数昵称不受 chat_post 截断约束)
    let nick: String = qnick
        .unwrap_or_else(|| {
            req.headers()
                .get("X-Nickname")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        })
        .replace(['\r', '\n'], " ")
        .chars()
        .take(NICK_MAX_CHARS)
        .collect();
    let device = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(device_kind)
        .unwrap_or_else(|| "未知设备".into());
    let started = std::time::Instant::now();
    let res = next.run(req).await;
    let noisy = (method == "GET" && NOISY_GET_PATHS.contains(&path.as_str()))
        || path == "/favicon.ico"
        // 并行分片上传: 每个大文件几十个成功分片请求, 逐条记会把日志环冲爆;
        // 开始/完成/中断由 upload handler 自己记关键日志, 中间片仅失败时留痕
        || (path == "/api/upload" && method == "POST" && res.status().is_success());
    if !noisy {
        let ms = started.elapsed().as_millis();
        let size = res
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(humans)
            .unwrap_or_else(|| "-".into());
        let who = if nick.is_empty() {
            device
        } else {
            format!("{nick}·{device}")
        };
        let q = if query.is_empty() {
            String::new()
        } else {
            format!("?{}", mask_pin(&query))
        };
        push_log(format!(
            "{method} {path}{q} → {} {size} {ms}ms ← {} ({who})",
            res.status().as_u16(),
            peer
        ));
    }
    res
}

// ---------------------------------------------------------------- //
// 路由组装
// ---------------------------------------------------------------- //

fn router(share: PathBuf) -> Router {
    Router::new()
        .route("/api/files", get(files))
        .route("/api/download/*name", get(download))
        .route("/api/upload", post(upload))
        .route("/api/upload_progress", get(upload_progress))
        .route("/api/upload_cancel", post(upload_cancel))
        .route("/api/delete/*name", delete(delete_file))
        .route("/api/chat", get(chat_get).post(chat_post))
        .route("/api/share-ticket", post(share_ticket))
        .route("/ws", get(ws_handler))
        .route("/api/info", get(info))
        .route("/api/control/status", get(ctrl_status))
        .route("/api/control/start", post(ctrl_start))
        .route("/api/control/stop", post(ctrl_stop))
        .route("/api/control/logs", get(ctrl_logs))
        .route("/api/control/logs/clear", post(ctrl_logs_clear))
        .route("/favicon.ico", get(favicon_handler))
        .layer(axum::middleware::from_fn(log_middleware))
        .fallback(static_handler)
        .layer(tower_http::cors::CorsLayer::permissive()) // 桌面窗口跨源调用 API
        .with_state(share)
}

// ---------------------------------------------------------------- //
// 自检(cargo test): 同名自动改名 + 分片续传 + 下载(全量/HEAD/Range)。
// 直连 lib 起真实服务(不经 GUI/单实例), 用裸 TcpStream 走 HTTP —— 与浏览器
// 行为同源。测试文件写入实际共享目录(__e2e_ 前缀, 结束即删); start 会把测试
// 端口写进端口偏好文件, 结束时还原, 避免污染用户下次启动的地址
// ---------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// 极简 percent-encode(路径段): 测试用中文文件名过 X-Filename/URL 需要
    fn enc(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// 裸 HTTP 客户端: 返回 (状态码, 响应头文本, 响应体)。依赖 Connection: close 让
    /// hyper 回完即断, read_to_end 才有尽头
    fn http_raw(port: u16, req: &str) -> (u16, String, Vec<u8>) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        let pos = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("no header terminator");
        let head = String::from_utf8_lossy(&buf[..pos]).to_string();
        let status: u16 = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, head, buf[pos + 4..].to_vec())
    }

    /// 服务是全局单例(running_slot): 两个 e2e 并行跑会互相关掉对方的服务,
    /// 用串行锁保证同一时刻只有一个测试在操作服务
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 直接调用手机上行 handler，使用独立临时目录，不改用户共享目录或端口偏好。
    #[test]
    fn notification_events_cover_upload_delete_and_full_chat() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let share = std::env::temp_dir().join(format!("ccflash-notification-test-{}", std::process::id()));
        std::fs::create_dir_all(&share).unwrap();
        let peer = "192.168.1.50:43210".parse::<SocketAddr>().unwrap();
        let mut events = event_bus().subscribe();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let response = super::upload(State(share.clone()), axum::extract::ConnectInfo(peer), HeaderMap::new(), Body::empty()).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(events.try_recv().is_err(), "失败上传不得发出完成通知");

            let mut headers = HeaderMap::new();
            headers.insert("X-Filename", "phone.txt".parse().unwrap());
            headers.insert("X-File-Size", "5".parse().unwrap());
            let response = super::upload(State(share.clone()), axum::extract::ConnectInfo(peer), headers, Body::from("hello")).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(std::fs::read(share.join("phone.txt")).unwrap(), b"hello");
            let event: serde_json::Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
            assert_eq!(event, serde_json::json!({ "type": "files:changed", "action": "uploaded" }));

            let response = delete_file(State(share.clone()), axum::extract::ConnectInfo(peer), AxPath("phone.txt".into())).await;
            assert_eq!(response.status(), StatusCode::OK);
            let event: serde_json::Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
            assert_eq!(event, serde_json::json!({ "type": "files:changed" }));

            let saved = {
                let mut log = chat_log().lock().unwrap_or_else(|e| e.into_inner());
                std::mem::take(&mut *log)
            };
            for i in 0..=CHAT_MAX {
                let response = chat_post(axum::extract::ConnectInfo(peer), Json(ChatIn {
                    nickname: Some("phone".into()), message: format!("message {i}"),
                })).await;
                assert_eq!(response.status(), StatusCode::OK);
                let event: serde_json::Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
                assert_eq!(event["type"], "chat:new");
            }
            let mut log = chat_log().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(log.len(), CHAT_MAX);
            assert_eq!(log.back().unwrap().id, (CHAT_MAX + 1) as u64);
            *log = saved;
        });
        std::fs::remove_dir_all(share).unwrap();
    }

    /// 上传一个分片: 返回 (状态码, JSON 响应)。sess = 会话 id(真实客户端按
    /// 名字+大小+修改时间派生, 同会话分片同 id, 新会话新 id)
    fn upload(port: u16, sess: &str, name: &str, offset: u64, total: u64, data: &[u8]) -> (u16, serde_json::Value) {
        let head = format!(
            "POST /api/upload HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             X-Filename: {}\r\nX-File-Size: {total}\r\nX-Upload-Offset: {offset}\r\n\
             X-Upload-Id: {sess}\r\nContent-Type: application/octet-stream\r\n\
             X-Upload-Hash: test-{sess}-{offset}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            enc(name),
            data.len()
        );
        let mut full = head.into_bytes();
        full.extend_from_slice(data);
        let req = String::from_utf8(full).unwrap();
        let (st, _h, body) = http_raw(port, &req);
        (st, serde_json::from_slice(&body).unwrap_or_default())
    }

    #[test]
    fn dedup_picks_first_free_variant() {
        let tmp = std::env::temp_dir().join(format!("ccflash-dedup-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // 无同名 → 原样返回
        assert_eq!(dedup_rel_name(&tmp, "报告.pdf"), "报告.pdf");
        std::fs::write(tmp.join("报告.pdf"), b"x").unwrap();
        assert_eq!(dedup_rel_name(&tmp, "报告.pdf"), "报告(1).pdf");
        std::fs::write(tmp.join("报告(1).pdf"), b"x").unwrap();
        assert_eq!(dedup_rel_name(&tmp, "报告.pdf"), "报告(2).pdf");
        // 无扩展名
        std::fs::write(tmp.join("README"), b"x").unwrap();
        assert_eq!(dedup_rel_name(&tmp, "README"), "README(1)");
        // 点开头文件: 整体作 stem, 不拆出空 stem
        std::fs::write(tmp.join(".gitignore"), b"x").unwrap();
        assert_eq!(dedup_rel_name(&tmp, ".gitignore"), ".gitignore(1)");
        // 子目录: 前缀不动, 只改末段
        std::fs::create_dir_all(tmp.join("子目录")).unwrap();
        std::fs::write(tmp.join("子目录/图.png"), b"x").unwrap();
        assert_eq!(dedup_rel_name(&tmp, "子目录/图.png"), "子目录/图(1).png");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 清理某文件名的全部会话断点产物: part 文件名带会话指纹(idhash), 需按前缀扫
    fn clean_parts(share: &Path, name: &str) {
        let prefix = format!("{name}.");
        let Ok(rd) = std::fs::read_dir(share) else { return };
        for e in rd.flatten() {
            let fn_ = e.file_name().to_string_lossy().to_string();
            if fn_.starts_with(&prefix) && (fn_.ends_with(".ccpart") || fn_.ends_with(".ccpart.meta")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// 1.1.2 语义 e2e: 单请求整文件直传落盘一致; 同名再次上传 = 直接覆盖(不再改名);
    /// 下载/HEAD/Range/416/缺文件名链路保持
    #[test]
    fn upload_overwrite_then_download_e2e() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let pref = port_pref_file();
        let orig_pref = std::fs::read_to_string(&pref).ok();
        let port = start(Some(17711)).expect("start server on 17711");
        let share = share_dir();
        let name = "__e2e_v112_upload.txt";
        let _ = std::fs::remove_file(share.join(name));

        // 1) 首传: 200 + 磁盘内容一致(单请求整文件)
        let (st, j) = upload(port, "any", name, 0, 6, b"AAAAAA");
        assert_eq!(st, 200, "首传应成功: {j}");
        assert_eq!(j["ok"], serde_json::json!(true));
        assert_eq!(std::fs::read(share.join(name)).unwrap(), b"AAAAAA", "落盘内容必须一致");

        // 2) 同名再传不同内容 → 自动改存 (1), 原文件不覆盖(1.1.5/1.1.6 改名策略)
        let (st, j) = upload(port, "other", name, 0, 6, b"BBBBBB");
        assert_eq!(st, 200, "同名改名应成功: {j}");
        let v1 = "__e2e_v112_upload(1).txt";
        assert_eq!(j["final"], serde_json::json!(v1), "应改存为 (1) 变体: {j}");
        assert_eq!(std::fs::read(share.join(name)).unwrap(), b"AAAAAA", "原文件不得被覆盖");
        assert_eq!(std::fs::read(share.join(v1)).unwrap(), b"BBBBBB", "(1) 应为新内容");

        // 3) 下载全量(原文件)
        let (_st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/download/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(name)
            ),
        );
        assert_eq!(body, b"AAAAAA");

        // 4) HEAD: content-length + 空体
        let (st, h, body) = http_raw(
            port,
            &format!(
                "HEAD /api/download/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(name)
            ),
        );
        assert_eq!(st, 200);
        assert!(h.to_ascii_lowercase().contains("content-length: 6"), "head: {h}");
        assert!(body.is_empty(), "HEAD 不应带响应体");

        // 5) Range 单区间 → 206
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/download/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=1-3\r\nConnection: close\r\n\r\n",
                enc(name)
            ),
        );
        assert_eq!(st, 206);
        assert_eq!(body, b"AAA", "Range bytes=1-3 为闭区间 3 字节");

        // 6) 越界 Range → 416
        let (st, _h, _body) = http_raw(
            port,
            &format!(
                "GET /api/download/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=100-200\r\nConnection: close\r\n\r\n",
                enc(name)
            ),
        );
        assert_eq!(st, 416);

        // 7) 缺 X-Filename → 400(1.1.2 协议唯一必需头之一)
        let (st, _h, _body) = http_raw(
            port,
            "POST /api/upload HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(st, 400, "缺文件名必须 400");

        let _ = std::fs::remove_file(share.join(name));
        let _ = std::fs::remove_file(share.join("__e2e_v112_upload(1).txt"));
        stop();
        match orig_pref {
            Some(s) => { let _ = std::fs::write(&pref, s); }
            None => { let _ = std::fs::remove_file(&pref); }
        }
    }

    /// 进度端点(休眠保留)三态回归: 空账本/旧数字账本绝不回落预分配 part 长度谎报完成
    #[test]
    fn empty_or_invalid_ledger_progress_never_reports_full() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let pref = port_pref_file();
        let orig_pref = std::fs::read_to_string(&pref).ok();
        let port = start(Some(17712)).expect("start server on 17712");
        let share = share_dir();
        let name = "__e2e_ledger_progress.txt";
        let _ = std::fs::remove_file(share.join(name));
        clean_parts(&share, name);

        // 场景 1: 会话已建(预分配 10 字节)、meta 第三行是空账本 {} → 必须报 0
        let (part, meta) = session_part_path(&share, name, "e2e-empty-ledger");
        std::fs::write(&part, vec![0u8; 10]).unwrap();
        std::fs::write(&meta, format!("e2e-empty-ledger\n{name}\n{{}}")).unwrap();
        let (_st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/upload_progress?name={}&id=e2e-empty-ledger&size=10 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(name)
            ),
        );
        let q: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(q["received"], serde_json::json!(0), "空账本不得回落 part 长度谎报完成: {q}");
        assert!(q["chunks"].is_object(), "空账本应下发空对象而非 null");

        // 场景 2: 旧版数字账本 {"0":5}(v1.1.5/6 形状) → 形状不可信, 同样必须报 0
        let (part2, meta2) = session_part_path(&share, name, "e2e-num-ledger");
        std::fs::write(&part2, vec![0u8; 10]).unwrap();
        std::fs::write(&meta2, format!("e2e-num-ledger\n{name}\n{{\"0\":5}}")).unwrap();
        let (_st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/upload_progress?name={}&id=e2e-num-ledger&size=10 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(name)
            ),
        );
        let q: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(q["received"], serde_json::json!(0), "非法形状账本不得回落 part 长度: {q}");
        assert!(q["chunks"].is_null(), "非法形状账本不得下发");

        let _ = std::fs::remove_file(share.join(name));
        clean_parts(&share, name);
        stop();
        match orig_pref {
            Some(s) => { let _ = std::fs::write(&pref, s); }
            None => { let _ = std::fs::remove_file(&pref); }
        }
    }

    /// 取消端点(休眠保留): 手工构造的会话断点可按 id 清理, 无在途 handler 时直删
    #[test]
    fn upload_cancel_cleans_breakpoint() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let pref = port_pref_file();
        let orig_pref = std::fs::read_to_string(&pref).ok();
        let port = start(Some(17713)).expect("start server on 17713");
        let share = share_dir();
        let name = "__e2e_cancel.txt";
        let _ = std::fs::remove_file(share.join(name));
        clean_parts(&share, name);

        // 手工构造会话断点(新上传协议不再产生会话文件, 该端点为遗留休眠接口)
        let (part, meta) = session_part_path(&share, name, "e2e-cancel");
        std::fs::write(&part, b"AAAAA").unwrap();
        std::fs::write(&meta, format!("e2e-cancel\n{name}\n{{}}")).unwrap();

        let (_st, _h, body) = http_raw(
            port,
            &format!(
                "POST /api/upload_cancel?name={}&id=e2e-cancel HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                enc(name)
            ),
        );
        let q: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(q["ok"], serde_json::json!(true), "cancel 应成功: {q}");
        assert_eq!(q["removed"], serde_json::json!(true), "无在途 handler 时应直删: {q}");
        assert!(!part.exists(), "part 应被删除");
        assert!(!meta.exists(), "meta 应被删除");

        let _ = std::fs::remove_file(share.join(name));
        clean_parts(&share, name);
        stop();
        match orig_pref {
            Some(s) => { let _ = std::fs::write(&pref, s); }
            None => { let _ = std::fs::remove_file(&pref); }
        }
    }
}
