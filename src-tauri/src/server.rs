//! 局域网文件服务(axum 重写版; 大体沿袭 Python 版 API 形态, 但已扩展
//! 分片续传/子目录/WebSocket 等, 且续传必须携带 X-Upload-Id —— 与旧 Python 版不再完全兼容)。
//!
//! API:
//!   GET  /                    → 前端页面(嵌入 ../dist)
//!   GET  /api/files           → 文件列表 JSON(递归, 相对路径作文件名, 隐藏 .ccpart)
//!   GET  /api/download/*name  → 流式下载(Content-Disposition 带 RFC5987)
//!   POST /api/upload          → 请求头 X-Filename(URL 编码, 支持子目录) + X-File-Size
//!                                + X-Upload-Offset(断点续传); 分片追加写 .ccpart,
//!                                收齐后转正为最终文件
//!   GET  /api/upload_progress → 查询断点 { received } (按 name)
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
    collections::VecDeque,
    net::{SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex, OnceLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, oneshot};

use crate::config::{
    APP_VERSION, CHAT_MAX, CHAT_PREVIEW_CHARS, CHUNK_HINT, DEFAULT_NICK, DEFAULT_PORT,
    LOG_LINE_MAX, MSG_MAX_CHARS, NICK_MAX_CHARS, NOISY_GET_PATHS, PIN_PREF_FILE, POLL_CHAT_MS,
    POLL_FILES_MS, POLL_LOGS_MS, POLL_STATUS_MS, PORT_PREF_FILE, PORT_PROBE_EXTRA, PORT_RANGE_END,
    REQ_LOG_MAX,
    SHARE_DIR_NAME, SHARE_DIR_NAME_LEGACY, SHARE_PREF_FILE, UPLOAD_FREE_CHECK_BYTES,
    UPLOAD_IDLE_TIMEOUT_SECS, UPLOAD_MAX_DEPTH, UPLOAD_PART_BUSY_SECS, UPLOAD_PART_META_SUFFIX,
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

fn req_log() -> &'static Mutex<VecDeque<String>> {
    static LOGS: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn push_log(line: String) {
    // 统一消毒: ① 剥 CR/LF 防日志注入(?nick=%0A 伪造条目/嫁祸 IP);
    // ② 整行硬截断——hyper 允许数百 KB 的 URL, 1000 条不截断可把内存推高数百 MB
    let clean: String = line
        .replace(['\r', '\n'], " ")
        .chars()
        .take(LOG_LINE_MAX)
        .collect();
    let mut q = req_log().lock().unwrap_or_else(|e| e.into_inner());
    q.push_back(format!("[{}] {}", now_hms(), clean));
    while q.len() > REQ_LOG_MAX {
        q.pop_front();
    }
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

/// 本地时间 HH:MM:SS.mmm(chrono Local, 修复旧版 UTC 简化展示)
fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
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
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(target);
        c
    };
    cmd.spawn().map_err(|e| e.to_string())?;
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
            return Some(l);
        }
        drop(l);
    }
    tokio::net::TcpListener::bind(("0.0.0.0", p)).await.ok()
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
    let _guard = START_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = preferred {
        LAST_PREFERRED_PORT.store(p, Ordering::Relaxed);
        // 同步落盘: 应用重启后 last_preferred_port() 才有值可回放
        let _ = std::fs::write(port_pref_file(), p.to_string());
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
    clean_stale_parts(&share);   // 清理过期续传断点(期内保留, 支持重启后续传)

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
            match bind_dualstack(p).await {
                Some(l) => {
                    picked = Some((l, p));
                    break;
                }
                None => continue,
            }
        }
        let (listener, port) = picked.ok_or_else(|| {
            format!("{DEFAULT_PORT}-{} 端口均被占用", PORT_RANGE_END - 1)
        })?;

        PORT.store(port, Ordering::Relaxed);
        push_log(format!("服务已启动 端口 {port} 目录 {}", share.display()));
        if preferred.is_some() && preferred != Some(port) {
            push_log(format!("首选端口 {} 被占用，已顺延至 {port}", preferred.unwrap_or(0)));
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

/// WS 踢出通道: 停服时先让所有 WS handler 主动退出 —— 否则 hyper 的优雅关闭会
/// 等待挂着的 WebSocket(手机息屏不回包)直至超时, 停服在主线程上卡住数秒。
fn ws_kick() -> &'static broadcast::Sender<()> {
    static TX: OnceLock<broadcast::Sender<()>> = OnceLock::new();
    TX.get_or_init(|| broadcast::channel(4).0)
}

/// 停止服务(踢掉 WS 后等待优雅关闭, 带超时)。
/// 不等待的话紧接着 start() 会因旧监听端口未释放而顺延到别的端口, 用户刚复制的地址即失效。
pub fn stop() -> bool {
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
        true
    } else {
        false
    }
}

/// 应用启动时自动拉起(失败仅打日志, 不阻断 GUI)。
/// 回放上次指定的端口: 重启应用后地址不漂移
pub fn autostart() -> Result<u16, String> {
    start(last_preferred_port())
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
        // Windows 非法字符(其余平台也统一拒绝, 保证跨端文件名一致)
        if seg.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
            return None;
        }
        // 控制字符(CR/LF/TAB/NUL 等, Windows 文件名非法, 落 macOS/Linux 会创建出
        // 带换行的文件名, 跨端互传必出问题)与 RTL 覆写符(U+202A-E, "exe.jpg" 反转
        // 伪装扩展名的社工手法)一律上传拒绝 —— 既有含这些字符的文件仍可下载/删除
        if seg.chars().any(|c| {
            c.is_control() || ('\u{202A}'..='\u{202E}').contains(&c)
        }) {
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
            // (写入被静默丢弃或挂起, rename 转正必失败 → 上传"成功"后文件消失)
            let stem = seg.split('.').next().unwrap_or("").to_ascii_uppercase();
            if matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL"
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
        if last_l.ends_with(UPLOAD_PART_SUFFIX) || last_l.ends_with(UPLOAD_PART_META_SUFFIX) {
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
/// 调用方必须在断点写锁的临界区内调用: 同名上传的开始与"完成转正 rename"都持同一把
/// 锁, 串行化后两个并发会话才不会选中同一个空闲名。
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

/// 共享目录遍历时跳过断点产物(中间文件与其 meta; 大小写不敏感——exFAT/U盘 拷来的变体同样跳)
fn is_part_artifact(fname: &str) -> bool {
    let f = fname.to_ascii_lowercase();
    f.ends_with(UPLOAD_PART_SUFFIX) || f.ends_with(UPLOAD_PART_META_SUFFIX)
}

/// 系统垃圾文件: 不进列表、上传拒绝。.DS_Store 记录了目录内曾出现过的文件名(隐私),
/// Thumbs.db/desktop.ini 是 Windows 缩略图缓存, ._ * 是 exFAT 的 AppleDouble 元数据
fn is_junk_file(fname: &str) -> bool {
    let f = fname.to_ascii_lowercase();
    f == ".ds_store" || f == "thumbs.db" || f == "desktop.ini" || f.starts_with("._")
}

/// 同一断点文件的写互斥: 客户端超时重试后, 旧请求的 handler 可能仍挂着写同一个
/// .ccpart, 两个写者交错会撕裂文件。按路径取独立锁(Weak 缓存: 无上传者时自动可回收,
/// 定期清理死条目, 内存有界) —— 不用固定桶, 避免不相关文件(如整目录上传里的大视频)
/// 哈希同桶时小文件被长时间排队直至客户端超时。
fn part_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<std::collections::HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    // 键做 case-fold: macOS APFS 默认大小写不敏感, Report.zip 与 report.zip 落盘是
    // 同一个 .ccpart —— 按字节区分会拿到两把锁并发交错写(正是本锁要防的撕裂);
    // Linux 上大小写不同的两个文件共享一把锁只是多余串行化, 无正确性影响
    let key = path.to_string_lossy().to_lowercase();
    let mut m = LOCKS
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap();
    if m.len() > 256 {
        m.retain(|_, w| w.strong_count() > 0); // 顺手清掉已无持有者的条目
    }
    if let Some(w) = m.get(&key) {
        if let Some(l) = w.upgrade() {
            return l;
        }
    }
    let l = Arc::new(tokio::sync::Mutex::new(()));
    m.insert(key, Arc::downgrade(&l));
    l
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
        [(header::CONTENT_TYPE, "image/png".to_string())],
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
    match Frontend::get(path) {
        Some(f) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref().to_string())], f.data).into_response()
        }
        // SPA 兜底: 未知路径回 index.html
        None => match Frontend::get("index.html") {
            Some(f) => (
                [(header::CONTENT_TYPE, "text/html".to_string())],
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
        push_log(format!("下载失败 {name}: 文件不存在 ← {}", peer));
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        push_log(format!("下载失败 {name}: 打开失败 ← {}", peer));
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    // 目录必须拒绝: File::open 对目录在 Unix 上成功, ReaderStream 读到 EISDIR
    // 中途断流, 客户端拿到的是"半截/损坏的下载"而非明确错误
    if !meta.is_file() {
        return err(StatusCode::BAD_REQUEST, "不是文件");
    }
    let Ok(std_file) = std::fs::File::open(&path) else {
        push_log(format!("下载失败 {name}: 打开失败 ← {}", peer));
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
            let s: u64 = s.parse().map_err(|_| ())?;
            if e.is_empty() {
                // N-
                if s >= size {
                    return Err(());
                }
                start = s;
                return Ok(true);
            }
            // N-M; 多区间("0-99,200-300" 的尾段含逗号)与畸形尾段 parse 失败 → 一并走整体 200
            let e: u64 = e.parse().map_err(|_| ())?;
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
            push_log(format!("下载失败 {name}: 定位失败({e}) ← {}", peer));
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

async fn upload(
    State(share): State<PathBuf>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    let peer = normalize_peer(addr.ip());
    let raw = headers
        .get("X-Filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // 请求头未经过 axum 解码, 在此解一次; safe_rel_path 不再解码(与 Path/Query 语义对齐,
    // 避免含字面 %XX 的文件名被双重解码后指向别的文件)
    let decoded = percent_decode_str(raw).decode_utf8_lossy().to_string();
    let Some(name) = safe_rel_path(&decoded, true) else {
        return err(StatusCode::BAD_REQUEST, "缺少或非法的文件名");
    };
    // Windows MAX_PATH(260) 预检: 深目录整目录上传时 create_dir_all 会报 ERROR_FILENAME_EXCED_RANGE,
    // 不预检的话前端把它当网络问题空转重试 8 次才报错; 提前 400 + 明确文案。
    // 按 UTF-16 code unit 计(MAX_PATH 的真实计量, 中文 1 字=1 单元, 否则 3 倍过度保守);
    // 预检用最长的内部产物后缀 .ccpart.meta(比 .ccpart 再多 5 字节), 防过预检后 meta 写入静默失败
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let full: std::path::PathBuf = share.join(format!("{name}{UPLOAD_PART_META_SUFFIX}"));
        let units: usize = full.as_os_str().encode_wide().count();
        if units > 259 {
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
    // 断点续传: 本片起始偏移(0 = 从头); 服务端按偏移追加写断点文件
    let offset: u64 = headers
        .get("X-Upload-Offset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // 上传会话标识: 新一次上传(新 id)会覆盖旧断点, 杜绝"同名改内容续传拼出脏文件"。
    // **必须消毒**: id 会原样写入 meta 首行, 含换行等控制字符的 id 可伪造 meta 行 ——
    // 把"最终落盘名"行换成 ..\ 越界路径, 转正 rename 即把内容写到共享目录之外
    // (任意位置写文件)。真实客户端的 id 由 encodeURIComponent(name)|size|mtime 构成,
    // 纯可打印 ASCII, 消毒零误伤
    let upload_id = headers
        .get("X-Upload-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if upload_id.len() > 200 || upload_id.chars().any(|c| c.is_control()) {
        return err(StatusCode::BAD_REQUEST, "非法的会话标识");
    }
    let final_path = share.join(&name); // 相对路径逐段已校验, 不会越界
    // part/meta 按"文件名+会话指纹"派生(见 session_part_path): 每会话独立断点文件
    let (part_path, meta_path) = session_part_path(&share, &name, &upload_id);
    let need = size.saturating_sub(offset);
    if need > 0 {
        if let Ok(free) = fs2::available_space(&share) {
            if need > free {
                push_log(format!(
                    "上传拒绝 {name}: 磁盘空间不足，需要 {}，可用 {} ← {}",
                    humans(need),
                    humans(free),
                    peer
                ));
                return err(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "磁盘空间不足，需要 {}，可用 {}",
                        humans(need),
                        humans(free)
                    ),
                );
            }
        }
    }
    // 锁分三段(并行分片的关键): PHASE1 短锁做建会话/校验 → 放锁流式收包(多连接才能
    // 同时收不同区间, 全程持锁会把并行串行化) → PHASE3 短锁登记账本+收齐转正。
    // 等锁带超时: 另一设备正在传同名文件时本请求不能无限排队
    let part_mutex = part_lock(&part_path);
    let part_guard = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        part_mutex.lock(),
    )
    .await
    {
        Ok(g) => g,
        Err(_) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "该文件正被其他设备上传，请稍后再试", "received": 0 })),
            )
                .into_response()
        }
    };
    // meta 三行: 会话 id / 同名自动改名的最终落盘名 / 分片账本 JSON(offset→len)。
    // 有账本 = 并行分片会话(part 预分配为全量大小, 各连接写各自区间, 收齐转正);
    // 无账本 = 旧版顺序会话(跨版本兼容: 断点按文件长度对齐)
    let meta_raw = tokio::fs::read_to_string(&meta_path).await.unwrap_or_default();
    let mut meta_lines = meta_raw.lines();
    let meta_id = meta_lines.next().unwrap_or("").trim().to_string();
    // 最终名出自磁盘上的 meta(可能被旧版注入或手工伪造): 经 safe_rel_path 可信化,
    // 含 ..\\ 等越界形态一律回落空串(按原名处理), 转正/完成核对永不逃出共享目录
    let meta_final = meta_lines
        .next()
        .map(|l| safe_rel_path(l.trim(), false).unwrap_or_default())
        .unwrap_or_default();
    let meta_ledger: Option<std::collections::HashMap<String, u64>> =
        meta_lines.next().and_then(|l| serde_json::from_str(l).ok());
    let session_ours = !meta_id.is_empty() && meta_id == upload_id && meta_ledger.is_some();
    let ledger = meta_ledger.unwrap_or_default();
    let coverage: u64 = ledger.values().sum();
    // 最终落盘名: 本会话已存在 → 沿用 meta 记录(改名只在建会话时决定一次,
    // 格子重试/断点续传不再重排); 新会话 → 此刻 dedup 选定(持锁, 与他人转正串行不撞名)
    let chosen = if !meta_id.is_empty() && meta_id == upload_id && !meta_final.is_empty() {
        meta_final.clone()
    } else {
        dedup_rel_name(&share, &name)
    };
    let have = tokio::fs::metadata(&part_path).await.map(|m| m.len()).unwrap_or(0);
    if offset == 0 {
        if have == 0 && session_ours && !meta_final.is_empty() {
            // 本会话已转正而断点不在(完成记录): 恰为目标大小直接确认完成
            if size > 0 {
                if let Ok(fm) = tokio::fs::metadata(share.join(&meta_final)).await {
                    if fm.is_file() && fm.len() == size {
                        return Json(serde_json::json!({
                            "ok": true, "received": size, "done": true, "final": meta_final
                        }))
                        .into_response();
                    }
                }
            }
        }
        if !session_ours || have == 0 {
            // 建会话(含本会话完成记录已失效的重建: 上面完成核对未命中而落下, 说明最终
            // 文件被外部改动/删除, 旧账本不可信, 必须清零重来 —— 否则残留账本会让
            // 部分written就"收齐"提前转正, 稀疏未写区间成零洞)。
            // 覆盖他人断点前做占用保护(最近 30s 仍在写 = 对方活着, 拒绝接管 ——
            // 否则两台设备互截断点陷入"从零重启"拉锯; 写入停止超时则放行覆盖)
            if !meta_id.is_empty() && meta_id != upload_id {
                let busy = tokio::fs::metadata(&part_path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .map(|d| d.as_secs() < UPLOAD_PART_BUSY_SECS)
                    .unwrap_or(false);
                if busy {
                    return (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({
                            "error": "该文件正被其他设备上传，请稍后再试",
                            "received": coverage,
                            "busy": true
                        })),
                    )
                        .into_response()
                }
            }
            // part 预分配为声明大小(稀疏文件不占实际磁盘): 并行各连接才能任意区间 seek+写
            let pf = match tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&part_path)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    push_log(format!("上传失败 {name}: 创建断点文件失败({e}) ← {}", peer));
                    return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
                }
            };
            if let Err(e) = pf.set_len(size).await {
                push_log(format!("上传失败 {name}: 预分配断点失败({e}) ← {}", peer));
                return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
            }
            drop(pf);
            let _ = tokio::fs::write(&meta_path, format!("{upload_id}\n{chosen}\n{{}}")).await;
            let declared = if size > 0 { humans(size) } else { "未知大小".into() };
            push_log(format!(
                "上传开始 {name} (声明 {declared}) ← {}{}",
                peer,
                if chosen != name { format!(" · 已有同名，将改存为 {chosen}") } else { String::new() }
            ));
        }
    } else {
        // 分片/续传必须携带会话 id: 否则无 id 请求的字节可能拼进他人会话
        if upload_id.trim().is_empty() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "续传点已失效，请从头重传", "received": 0 })),
            )
                .into_response();
        }
        if have == 0 {
            // part 不在: 本会话已完成转正(完成记录按**最终名**核对, 按原始名核对不上
            // 改名后的 xxx(1).zip 会误判未完成触发整份重传) 或 无会话
            if meta_id == upload_id && !meta_final.is_empty() {
                if size > 0 {
                    if let Ok(fm) = tokio::fs::metadata(share.join(&meta_final)).await {
                        if fm.is_file() && fm.len() == size {
                            return Json(serde_json::json!({
                                "ok": true, "received": size, "done": true, "final": meta_final
                            }))
                            .into_response();
                        }
                    }
                }
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({ "error": "续传点已失效，请从头重传", "received": 0 })),
                )
                    .into_response();
            }
            if meta_id.is_empty() {
                // 断点与 meta 都不存在(旧版本无完成记录): 另一设备大概率已转正,
                // 最终文件恰为目标大小时直接确认完成, 避免败者整份重传
                if size > 0 {
                    if let Ok(fm) = tokio::fs::metadata(&final_path).await {
                        if fm.len() == size {
                            return Json(serde_json::json!({ "ok": true, "received": size, "done": true }))
                                .into_response();
                        }
                    }
                }
            }
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "续传点已失效，请从头重传", "received": 0 })),
            )
                .into_response();
        }
        if session_ours {
            // 并行格子: part 已预分配为全量大小, 任意格子直接写, 收齐判定在账本
            if size > 0 && offset >= size {
                return err(StatusCode::BAD_REQUEST, "分片偏移越界");
            }
        } else if meta_id == upload_id {
            // 旧版顺序会话续传(无账本): 断点按文件长度对齐(原逻辑不变)
            if have < offset {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({ "error": "续传点已失效，请从头重传", "received": have })),
                )
                    .into_response();
            }
            if have > offset {
                if let Ok(f) = tokio::fs::OpenOptions::new().write(true).open(&part_path).await {
                    let _ = f.set_len(offset).await;
                }
            }
        } else {
            // 断点属于另一次上传(同名新内容) → 视为无效, 让客户端从头重传
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "续传点已失效，请从头重传", "received": 0 })),
            )
                .into_response();
        }
    }
    // 越界预检必须在建目录之前: create_dir_all 会跟随符号链接, 先建后查会把目录建到界外。
    // 对"最近存在的祖先"做 canonical 校验(其下若链向界外, 在此拦截, 且未创建任何东西)
    if let Some(parent) = part_path.parent() {
        let mut probe = parent.to_path_buf();
        while !probe.exists() {
            match probe.parent() {
                Some(p) => probe = p.to_path_buf(),
                None => break,
            }
        }
        // canonicalize 失败(权限错误等)一律拒绝——静默放行等于跳过整道越界校验
        let (Ok(base), Ok(probe_canon)) = (share.canonicalize(), probe.canonicalize()) else {
            return err(StatusCode::INTERNAL_SERVER_ERROR, "路径校验失败");
        };
        if !probe_canon.starts_with(&base) {
            return err(StatusCode::BAD_REQUEST, "路径越界");
        }
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            push_log(format!("上传 {name}: 建目录失败({e}) ← {addr}"));
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("创建目录失败: {e}"));
        }
        // 事后复验: 预检与 create_dir_all 之间存在 TOCTOU 窗口(中间目录被换成指向界外的
        // 符号链接), 建完再查一次; 发现越界则回滚 —— 用非递归 remove_dir 而非
        // remove_dir_all: 刚建的目录必为空, 非空即失败, 永不误删换链目标处的既有数据
        match (parent.canonicalize(), share.canonicalize()) {
            (Ok(parent_canon), Ok(base)) if parent_canon.starts_with(&base) => {}
            (Ok(_), Ok(_)) => {
                let _ = tokio::fs::remove_dir(parent).await;
                return err(StatusCode::BAD_REQUEST, "路径越界");
            }
            _ => return err(StatusCode::INTERNAL_SERVER_ERROR, "路径校验失败"),
        }
    }
    // 截断只发生在建会话时(上面); 分片写入一律不截断, 防止覆盖其他连接已写的区间
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&part_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            push_log(format!("上传失败 {name}: 打开断点失败({e}) ← {}", peer));
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
        }
    };
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
        push_log(format!("上传失败 {name}: 定位写入位置失败({e}) ← {}", peer));
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
    }
    // 放锁进入流式收包: 其他连接此刻可并行收同一文件的不同区间
    drop(part_guard);
    let started = std::time::Instant::now();
    let mut stream = body.into_data_stream();
    let mut received: u64 = offset;
    // 未知大小(size=0)无法预检磁盘: 每写 UPLOAD_FREE_CHECK_BYTES 复查一次剩余空间
    let mut next_free_check: u64 = UPLOAD_FREE_CHECK_BYTES;
    loop {
        // 空闲超时: 半开连接(hyper 无 body 超时)的残留 handler 靠它退出并释放写锁
        let next =
            tokio::time::timeout(std::time::Duration::from_secs(UPLOAD_IDLE_TIMEOUT_SECS), stream.next())
                .await;
        let chunk = match next {
            Ok(Some(c)) => c,
            Ok(None) => break, // body 正常结束
            Err(_) => {
                push_log(format!(
                    "上传中断 {name}: 连接空闲超时，已保留断点 {}，可续传 ← {}",
                    humans(received),
                    peer
                ));
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(serde_json::json!({ "error": "上传中断", "received": received })),
                )
                    .into_response();
            }
        };
        match chunk {
            Ok(c) => {
                if let Err(e) = file.write_all(&c).await {
                    push_log(format!(
                        "上传失败 {name}: 写入失败({e})，已保留断点 {} ← {}",
                        humans(received),
                        peer
                    ));
                    return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
                }
                received += c.len() as u64;
                // 边收边查, 不等 body 收完: 声明 100 字节实际灌 100GB 时,
                // 旧逻辑全部落盘后才截断, 磁盘早已打满
                if size > 0 && received > size {
                    let _ = file.set_len(size).await;
                    push_log(format!(
                        "上传拒绝 {name}: 实际流量超出声明大小({}) ← {}",
                        humans(size),
                        peer
                    ));
                    return err(StatusCode::BAD_REQUEST, "实际流量超出声明大小");
                }
                if size == 0 && received >= next_free_check {
                    next_free_check = received + UPLOAD_FREE_CHECK_BYTES;
                    if let Ok(free) = fs2::available_space(&share) {
                        if free < CHUNK_HINT as u64 {
                            push_log(format!(
                                "上传中止 {name}: 磁盘剩余不足({}) ← {}",
                                humans(free),
                                peer
                            ));
                            return (
                                StatusCode::INSUFFICIENT_STORAGE,
                                Json(serde_json::json!({ "error": "磁盘空间不足", "received": received })),
                            )
                                .into_response();
                        }
                    }
                }
            }
            Err(_) => {
                // 断点保留(不再删除): 客户端可通过 upload_progress 查询后续传
                push_log(format!(
                    "上传中断 {name}: 连接断开，已保留断点 {}，可续传 ← {}",
                    humans(received),
                    peer
                ));
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({ "error": "上传中断", "received": received })),
                )
                    .into_response();
            }
        }
    }
    if size > 0 && received > size {
        // 超出声明大小的字节视为脏数据: 截断到 size(与磁盘预检的预留上限一致)
        let _ = file.set_len(size).await;
        push_log(format!(
            "上传超额 {name}: 收到 {} 超过声明 {}，已截断 ← {}",
            humans(received),
            humans(size),
            peer
        ));
        received = size;
    }
    // PHASE 3: 短锁登记本分片 + 收齐判定。流式期间会话若被他人接管(meta 变了),
    // 本片作废 → 409, 客户端按新会话重排。锁持到转正结束: 与他人建会话/转正串行
    let _part_guard = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        part_mutex.lock(),
    )
    .await
    {
        Ok(g) => g,
        Err(_) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "该文件正被其他设备上传，请稍后再试", "received": 0 })),
            )
                .into_response()
        }
    };
    let meta_now = tokio::fs::read_to_string(&meta_path).await.unwrap_or_default();
    let mut now_lines = meta_now.lines();
    let now_id = now_lines.next().unwrap_or("").trim().to_string();
    // 最终名可信化(同 PHASE 1): 转正 rename 前最后一道越界防线
    let now_final = now_lines
        .next()
        .map(|l| safe_rel_path(l.trim(), false).unwrap_or_default())
        .unwrap_or_default();
    let now_ledger: Option<std::collections::HashMap<String, u64>> =
        now_lines.next().and_then(|l| serde_json::from_str(l).ok());
    if now_id != upload_id || now_ledger.is_none() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "续传点已失效，请从头重传", "received": 0 })),
        )
            .into_response();
    }
    let mut led = now_ledger.unwrap();
    // 本分片写入的区间 [offset, received) 登记进账本; 同格重试幂等覆盖
    let wrote = received - offset;
    led.insert(offset.to_string(), wrote);
    let coverage: u64 = led.values().sum();
    let _ = tokio::fs::write(
        &meta_path,
        format!(
            "{upload_id}\n{now_final}\n{}",
            serde_json::to_string(&led).unwrap_or_else(|_| "{}".into())
        ),
    )
    .await;
    if size > 0 && coverage < size {
        // 未收齐: 200 确认本片(带回全局覆盖进度), 断点与账本保留待续。
        // 中间不 fsync: 账本是完成度的唯一事实, 掉电丢页缓存只损失未登记的区间
        return Json(serde_json::json!({ "ok": true, "received": coverage.min(size), "done": false }))
            .into_response();
    }
    // 收齐才落盘: 先 fsync 数据再改名, 掉电不会出现"有文件名无数据"。
    // (sync 的是本请求的句柄, fsync 按 inode 刷盘, 其他连接写区间的脏页一并落盘)
    let _ = file.sync_all().await;
    drop(file);
    let final_name = if now_final.is_empty() { name.clone() } else { now_final };
    let final_path = share.join(&final_name);
    if let Some(parent) = final_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    // Windows 的 rename 不覆盖已存在目标, 先删; 删不掉(被资源管理器预览/杀软/Word
    // 占用无 FILE_SHARE_DELETE)时记日志, 不再静默
    if final_path.exists() {
        if let Err(e) = tokio::fs::remove_file(&final_path).await {
            push_log(format!("上传 {name}: 覆盖旧文件失败({e}), 将重试转正 ← {}", peer));
        }
    }
    // 转正带退避重试: 字节已完整落在断点文件里, 占用者通常毫秒~秒级释放;
    // 不重试的话用户看到"传完了却报上传中断", 且断点白占磁盘
    let mut renamed = tokio::fs::rename(&part_path, &final_path).await;
    for _ in 0..5 {
        if renamed.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        if final_path.exists() {
            let _ = tokio::fs::remove_file(&final_path).await;
        }
        renamed = tokio::fs::rename(&part_path, &final_path).await;
    }
    match renamed {
        Ok(()) => {
            // meta 保留为"完成记录"(id + 最终名 + 账本): 最终响应丢失/页面刷新后的补偿查询
            // 靠它确认已完成, 避免整份重传; 同名新上传(offset=0)会覆写它,
            // 残留记录由 clean_stale_parts 按 mtime(>TTL) 回收, 不占列表
            push_log(format!(
                "上传完成 {final_name} ({}, {}ms{}{}) ← {}",
                humans(coverage),
                started.elapsed().as_millis(),
                if offset > 0 { format!("，续传自 {}", humans(offset)) } else { String::new() },
                if final_name != name { format!("，改名自 {name}") } else { String::new() },
                peer
            ));
            broadcast_event("files:changed");
            // final: 同名自动改名后的实际落盘名(相对路径), 前端据此提示"已存为 xxx";
            // received 报全局覆盖量(并行时末格自身偏移 ≠ 全量)
            Json(serde_json::json!({
                "ok": true, "received": coverage.min(size), "done": true, "final": final_name
            }))
            .into_response()
        }
        Err(e) => {
            push_log(format!("上传失败 {name}: 转正失败({e}) ← {}", peer));
            err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"))
        }
    }
}

/// 断点查询: 返回该文件已有断点字节数(0 = 无断点/不属于本次上传会话, 从头传)。
/// 纯只读: 不创建任何目录(否则任意查询串即可在共享目录里刷出空目录树)。
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
    let mut ledger: Option<std::collections::HashMap<String, u64>> =
        meta_lines.next().and_then(|l| serde_json::from_str(l).ok());
    // 并行分片会话: part 是预分配的稀疏文件, 长度恒等于全量 —— 完成度只能看账本
    let mut received = match &ledger {
        Some(led) => led.values().sum::<u64>(),
        None => part_len.unwrap_or(0),
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
    if id_ok && ledger.is_none() && part_len.is_none() && received == 0 {
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
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            push_log(format!("删除 {name} ← {}", peer));
            broadcast_event("files:changed");
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(e) => {
            push_log(format!("删除失败 {name}: {e} ← {}", peer));
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

async fn ws_handler(ws: axum::extract::ws::WebSocketUpgrade) -> Response {
    let mut rx = event_bus().subscribe();
    let mut kick = ws_kick().subscribe();
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

fn pin_fails() -> &'static Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static FAILS: OnceLock<Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    FAILS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 配对尝试准入: 配对成功清零该 IP 计数并放行; 失败时若处于退避锁定窗内
/// 返回 false(拒绝且**不计数不延长** —— 轰炸无法把锁定推到无穷, 窗口到期自动解),
/// 否则记一次失败并放行比较。调用方语义: false → 429; true 且 !ok → 401
fn pin_admit(peer: std::net::IpAddr, ok: bool) -> bool {
    let mut m = pin_fails().lock().unwrap_or_else(|e| e.into_inner());
    if ok {
        m.remove(&peer);
        return true;
    }
    let now = std::time::Instant::now();
    if m.len() > 1024 {
        m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < PIN_FAIL_MAX_SECS);
    }
    let e = m.entry(peer).or_insert((0, now));
    let (n, prev_t) = (e.0, e.1);
    if n >= PIN_FAIL_FREE {
        // 已失败 n 次 → 下次尝试须距上次失败 15s×2^(n-阈值), 封顶 1h
        let extra = n.saturating_sub(PIN_FAIL_FREE).min(8) as u32;
        let wait = PIN_FAIL_BASE_SECS
            .checked_shl(extra)
            .unwrap_or(PIN_FAIL_MAX_SECS)
            .min(PIN_FAIL_MAX_SECS);
        if now.duration_since(prev_t) < std::time::Duration::from_secs(wait) {
            return false;
        }
    }
    e.0 = n.saturating_add(1);
    e.1 = now;
    true
}

#[tauri::command]
pub fn set_access_pin_cmd(enable: bool) -> serde_json::Value {
    let mut slot = access_pin().lock().unwrap_or_else(|e| e.into_inner());
    if !enable {
        *slot = None;
        let _ = std::fs::remove_file(pin_pref_file());   // 同步撤盘, 重启后不再回载
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
    match open_with_system(&url) {
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
    let path = share_dir().join(&name);
    if !path.is_file() {
        return serde_json::json!({ "ok": false, "error": "文件不存在" });
    }
    let p = path.to_string_lossy().to_string();
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
    let host_name = host.split(':').next().unwrap_or("");
    // IPv6 字面量 Host("[::1]:1688")不能按 ':' 硬切(会截出 "["), 单独解析括号内地址
    let host_ok = if host_name.starts_with('[') {
        host_name[1..]
            .split(']')
            .next()
            .map(|s| s.parse::<std::net::IpAddr>().is_ok())
            .unwrap_or(false)
    } else {
        // 允许: IP / localhost / *.local(mDNS 主机名, 如 MyMac.local; 公网域名无法占 .local,
        // DNS 重绑定经 .local 不可行) —— 其余域名一律拒绝
        host_name == "localhost"
            || host_name.parse::<std::net::IpAddr>().is_ok()
            || host_name.ends_with(".local")
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
        .map(|kv| if kv.starts_with("pin=") { "pin=***" } else { kv })
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
            let ok = given.as_deref() == Some(pin.as_str());
            // 爆破防护: 只统计"实际提交了配对码"的尝试(未配对设备的无码轮询不计,
            // 否则正常页面十几秒把自己轰进锁定)。带码连续失败达阈值后按 IP 指数退避。
            // 锁定窗内一律 429 —— 正确码也必须拒: 若放行, 对/错码的响应可区分,
            // 攻击者伪造 Accept: text/html(导航形态)即可全速逐个试码, 退避形同虚设。
            // 逐 IP 计数使攻击者只能锁住自己的源地址, 不存在"锁害他人"; 窗不因
            // 轰炸延长(pin_admit 锁定中不记数), 到期自动解, 最短 15s 对真人无感
            let admitted = pin_admit(peer, ok);
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
        .route("/api/delete/*name", delete(delete_file))
        .route("/api/chat", get(chat_get).post(chat_post))
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

    /// 上传一个分片: 返回 (状态码, JSON 响应)。sess = 会话 id(真实客户端按
    /// 名字+大小+修改时间派生, 同会话分片同 id, 新会话新 id)
    fn upload(port: u16, sess: &str, name: &str, offset: u64, total: u64, data: &[u8]) -> (u16, serde_json::Value) {
        let head = format!(
            "POST /api/upload HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             X-Filename: {}\r\nX-File-Size: {total}\r\nX-Upload-Offset: {offset}\r\n\
             X-Upload-Id: {sess}\r\nContent-Type: application/octet-stream\r\n\
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

    #[test]
    fn upload_rename_resume_download_e2e() {
        let _serial = SERIAL.lock().unwrap();
        // 还原端口偏好: start() 会把测试端口持久化, 不还原会污染用户下次启动的地址
        let pref = port_pref_file();
        let orig_pref = std::fs::read_to_string(&pref).ok();

        let port = start(Some(17701)).expect("start server on 17701");
        let share = share_dir();
        let base = "__e2e_同名改名.txt";
        let v1 = "__e2e_同名改名(1).txt";
        let v2 = "__e2e_同名改名(2).txt";
        for n in [base, v1, v2] {
            let _ = std::fs::remove_file(share.join(n)); // 预清理上次异常残留
        }
        clean_parts(&share, base);

        // 1) 首次上传分两片: 首片 5 字节(共 10) → 中间片确认, 断点保留
        let (st, j) = upload(port, "e2e-id-1", base, 0, 10, b"AAAAA");
        assert_eq!(st, 200, "首片: {j}");
        assert_eq!(j["done"], serde_json::json!(false));
        assert_eq!(j["received"], serde_json::json!(5), "received 应为账本覆盖量");

        // 2) 断点查询: 账本会话报覆盖量并下发 chunks(客户端跨重启按格续传的依据)
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/upload_progress?name={}&id=e2e-id-1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(base)
            ),
        );
        assert_eq!(st, 200);
        let q: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(q["received"], serde_json::json!(5), "断点查询应报覆盖量 5");
        assert_eq!(q["chunks"]["0"], serde_json::json!(5), "账本应含已收区间");

        // 3) 续传后 5 字节 → 收齐转正; 目录无同名 → 不改名
        let (st, j) = upload(port, "e2e-id-1", base, 5, 10, b"BBBBB");
        assert_eq!(st, 200, "续传片: {j}");
        assert_eq!(j["done"], serde_json::json!(true));
        assert_eq!(j["final"], serde_json::json!(base));
        assert_eq!(
            std::fs::read(share.join(base)).unwrap(),
            b"AAAAABBBBB",
            "两片拼接内容应完整"
        );

        // 3b) 完成记录: 转正后 meta 保留, 断点查询按最终名核对大小 → 报满量(不再误判未完成)
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/upload_progress?name={}&id=e2e-id-1&size=10 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(base)
            ),
        );
        assert_eq!(st, 200);
        let q: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(q["received"], serde_json::json!(10), "完成记录应让断点查询报满量");

        // 3b-2) 会话隔离: 同名新会话(不同 id)查询绝不能拿到上一会话的进度与账本 ——
        // 否则客户端把旧格子当"已覆盖"直接跳过: 大小相同时假报"完成", 新内容没上传、
        // 也不触发同名改名(实测踩过的"改名不生效"回归)
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/upload_progress?name={}&id=e2e-id-other&size=10 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(base)
            ),
        );
        assert_eq!(st, 200);
        let q: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(q["received"], serde_json::json!(0), "他人会话不得报进度");
        assert!(q["chunks"].is_null(), "他人账本不得下发");

        // 3c) 完成后的补偿续传请求(offset=满量, 响应丢失场景) → 直接确认 done, 不重传不覆盖
        let (st, j) = upload(port, "e2e-id-1", base, 10, 10, b"");
        assert_eq!(st, 200, "补偿续传: {j}");
        assert_eq!(j["done"], serde_json::json!(true));
        assert_eq!(j["final"], serde_json::json!(base));
        assert_eq!(std::fs::read(share.join(base)).unwrap(), b"AAAAABBBBB", "补偿确认不得改动文件");

        // 4) 同名再传(新会话新 id, 如同真实客户端不同内容同名) → 落为 (1), 原文件不被覆盖
        let (st, j) = upload(port, "e2e-id-2", base, 0, 6, b"second");
        assert_eq!(st, 200, "同名第 2 次: {j}");
        assert_eq!(j["final"], serde_json::json!(v1));
        assert_eq!(std::fs::read(share.join(base)).unwrap(), b"AAAAABBBBB", "原文件不得被覆盖");

        // 4b) 改名会话的完成记录: 按 meta 最终名 (1) 核对大小 → 报完成
        // (按原始名核对不上改名后的文件, 会误判未完成 → 整份重传, 正是本次修的回归)
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/upload_progress?name={}&id=e2e-id-2&size=6 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(base)
            ),
        );
        assert_eq!(st, 200);
        let q: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(q["received"], serde_json::json!(6), "改名会话应按 meta 最终名确认完成");

        // 5) 第三次 → (2)
        let (st, j) = upload(port, "e2e-id-3", base, 0, 6, b"third!");
        assert_eq!(st, 200);
        assert_eq!(j["final"], serde_json::json!(v2));

        // 6) 下载全量
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/download/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(base)
            ),
        );
        assert_eq!(st, 200);
        assert_eq!(body, b"AAAAABBBBB");

        // 7) HEAD 预检: 200 + 正确 Content-Length + 空体(axum 自动 HEAD, 前端下载预检依赖它)
        let (st, h, body) = http_raw(
            port,
            &format!(
                "HEAD /api/download/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(base)
            ),
        );
        assert_eq!(st, 200);
        assert!(h.to_ascii_lowercase().contains("content-length: 10"), "head: {h}");
        assert!(body.is_empty(), "HEAD 不应带响应体");

        // 8) Range 单区间 → 206 + 截断后的字节
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/download/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=0-4\r\nConnection: close\r\n\r\n",
                enc(base)
            ),
        );
        assert_eq!(st, 206);
        assert_eq!(body, b"AAAAA");

        // 9) 超出文件大小 → 416
        let (st, _h, _body) = http_raw(
            port,
            &format!(
                "GET /api/download/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=100-200\r\nConnection: close\r\n\r\n",
                enc(base)
            ),
        );
        assert_eq!(st, 416);

        // 清理测试文件 + 还原端口偏好
        for n in [base, v1, v2] {
            let _ = std::fs::remove_file(share.join(n));
        }
        clean_parts(&share, base);
        stop();
        match orig_pref {
            Some(s) => { let _ = std::fs::write(&pref, s); }
            None => { let _ = std::fs::remove_file(&pref); }
        }
    }

    #[test]
    fn upload_parallel_out_of_order() {
        let _serial = SERIAL.lock().unwrap();
        // 并行分片核心语义: 格子乱序到达(哪条连接先完成取决于链路), 账本逐格登记,
        // 收齐才转正 —— 服务端必须对任意完成顺序健壮
        let pref = port_pref_file();
        let orig_pref = std::fs::read_to_string(&pref).ok();

        let port = start(Some(17702)).expect("start server on 17702");
        let share = share_dir();
        let name = "__e2e_并行乱序.bin";
        let _ = std::fs::remove_file(share.join(name));
        clean_parts(&share, name);

        // 格 A(0-10) 先到: 建会话 + 预分配
        let (st, j) = upload(port, "e2e-par", name, 0, 30, b"AAAAAAAAAA");
        assert_eq!(st, 200, "格 A: {j}");
        assert_eq!(j["done"], serde_json::json!(false));
        assert_eq!(j["received"], serde_json::json!(10));

        // 格 C(20-30) 先于格 B 到达(乱序)
        let (st, j) = upload(port, "e2e-par", name, 20, 30, b"CCCCCCCCCC");
        assert_eq!(st, 200, "格 C: {j}");
        assert_eq!(j["done"], serde_json::json!(false));
        assert_eq!(j["received"], serde_json::json!(20));

        // 中途查询: 账本应含 A/C 两格
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/upload_progress?name={}&id=e2e-par HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(name)
            ),
        );
        assert_eq!(st, 200);
        let q: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(q["received"], serde_json::json!(20));
        assert_eq!(q["chunks"]["20"], serde_json::json!(10), "乱序格也应登记进账本");

        // 格 B(10-20) 补齐 → 转正, 内容按区间各就各位
        let (st, j) = upload(port, "e2e-par", name, 10, 30, b"BBBBBBBBBB");
        assert_eq!(st, 200, "格 B: {j}");
        assert_eq!(j["done"], serde_json::json!(true));
        assert_eq!(j["received"], serde_json::json!(30), "done 响应应报全量覆盖");
        assert_eq!(
            std::fs::read(share.join(name)).unwrap(),
            b"AAAAAAAAAABBBBBBBBBBCCCCCCCCCC",
            "乱序写入必须按区间落位"
        );

        let _ = std::fs::remove_file(share.join(name));
        clean_parts(&share, name);
        stop();
        match orig_pref {
            Some(s) => { let _ = std::fs::write(&pref, s); }
            None => { let _ = std::fs::remove_file(&pref); }
        }
    }
}
