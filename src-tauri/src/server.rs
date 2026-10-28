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
    LOG_LINE_MAX, MSG_MAX_CHARS, NICK_MAX_CHARS, NOISY_GET_PATHS, POLL_CHAT_MS, POLL_FILES_MS,
    POLL_LOGS_MS, POLL_STATUS_MS, PORT_PROBE_EXTRA, PORT_RANGE_END, REQ_LOG_MAX, SHARE_DIR_NAME,
    SHARE_DIR_NAME_LEGACY, SHARE_PREF_FILE, UPLOAD_FREE_CHECK_BYTES, UPLOAD_IDLE_TIMEOUT_SECS,
    UPLOAD_MAX_DEPTH, UPLOAD_PART_META_SUFFIX, UPLOAD_PART_SUFFIX, UPLOAD_PART_TTL_SECS,
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
    let mut q = req_log().lock().unwrap();
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

pub fn share_dir() -> PathBuf { share_slot().lock().unwrap().clone() }

/// 更换共享目录(与 PyQt 版一致: 运行中锁定, 停止后下次启动生效); 同步持久化选择
pub fn set_share_dir(p: PathBuf) {
    *share_slot().lock().unwrap() = p.clone();
    let _ = std::fs::write(share_pref_file(), p.to_string_lossy().as_bytes());
}


pub fn is_running() -> bool { running_slot().lock().unwrap().is_some() }

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
    const TUNNEL_PREFIXES: &[&str] = &["utun", "tun", "tap", "wg", "ppp", "bridge", "vethernet", "docker", "vmnet", "virbr"];
    let is_tunnel = |name: &str| {
        let n = name.to_ascii_lowercase();
        TUNNEL_PREFIXES.iter().any(|p| n.starts_with(p))
    };
    let is_private = |ip: &std::net::Ipv4Addr| {
        ip.is_private() || ip.is_link_local()
    };
    let mut private: Vec<String> = Vec::new();
    let mut other: Vec<String> = Vec::new();
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
                if is_private(&v4.ip) && !private.contains(&s) {
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
    // 全部失败才回落探测值/回环(此时二维码应配合 UI 提示"未检测到局域网地址")
    probe.unwrap_or_else(|| "127.0.0.1".into())
}

// ---------------------------------------------------------------- //
// 启停管理
// ---------------------------------------------------------------- //

/// 启动服务(优先尝试指定端口, 被占用自动顺延);返回实际端口。
/// 注意:tokio 的 socket 操作必须在 runtime 上下文内,故整体包进 block_on。
/// 用户最近一次指定的首选端口(0=未指定): 托盘"启动"/应用重启时回放,
/// 防止重启后端口静默漂移回默认值、用户早先复制给手机的地址/二维码失效
static LAST_PREFERRED_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
pub fn last_preferred_port() -> Option<u16> {
    match LAST_PREFERRED_PORT.load(Ordering::Relaxed) {
        0 => None,
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
    }
    {
        let lock = running_slot().lock().unwrap();
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
            match tokio::net::TcpListener::bind(("0.0.0.0", p)).await {
                Ok(l) => {
                    picked = Some((l, p));
                    break;
                }
                Err(_) => continue,
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
        *running_slot().lock().unwrap() =
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
    let mut lock = running_slot().lock().unwrap();
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
    let running = running_slot().lock().unwrap().is_some();
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
    // 通配路由捕获值带前导 "/", 先去掉再做相对路径校验(读路径不拦保留设备名, 否则既有文件取不回)
    let Some(name) = safe_rel_path(raw.trim_start_matches('/'), false) else {
        return err(StatusCode::BAD_REQUEST, "非法的文件名");
    };
    let Some(path) = safe_join_existing(&share, &name) else {
        push_log(format!("下载失败 {name}: 文件不存在 ← {}", addr.ip()));
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        push_log(format!("下载失败 {name}: 打开失败 ← {}", addr.ip()));
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    // 目录必须拒绝: File::open 对目录在 Unix 上成功, ReaderStream 读到 EISDIR
    // 中途断流, 客户端拿到的是"半截/损坏的下载"而非明确错误
    if !meta.is_file() {
        return err(StatusCode::BAD_REQUEST, "不是文件");
    }
    let Ok(std_file) = std::fs::File::open(&path) else {
        push_log(format!("下载失败 {name}: 打开失败 ← {}", addr.ip()));
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
        // 仅接受单区间 "N-" / "N-M"; 多区间/畸形一律按整体 200 处理(下载场景无需多区间)
        let ok = match rv.split_once('-') {
            Some((a, b)) => {
                let s_ok = !a.is_empty() && a.trim().parse::<u64>().is_ok();
                let e = b.trim();
                if e.is_empty() {
                    // N-
                    if s_ok {
                        start = a.trim().parse().unwrap_or(0);
                        true
                    } else { false }
                } else if s_ok {
                    // N-M
                    let (s, e2) = (a.trim().parse::<u64>().unwrap_or(0), e.parse::<u64>().unwrap_or(0));
                    if s <= e2 && s < size {
                        start = s;
                        end_excl = (e2 + 1).min(size);
                        true
                    } else { false }
                } else { false }
            }
            None => false,
        };
        if ok && start >= size {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{size}"))],
                "范围超出文件大小",
            )
                .into_response();
        }
        ok
    } else {
        false
    };
    let mut tokio_file = tokio::fs::File::from_std(std_file);
    if is_partial {
        if let Err(e) = tokio_file.seek(std::io::SeekFrom::Start(start)).await {
            push_log(format!("下载失败 {name}: 定位失败({e}) ← {}", addr.ip()));
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
        addr.ip(),
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
    // 不预检的话前端把它当网络问题空转重试 8 次才报错; 提前 400 + 明确文案
    #[cfg(windows)]
    {
        let full_len = share.join(format!("{name}{UPLOAD_PART_SUFFIX}")).as_os_str().len();
        if full_len > 259 {
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
    // 上传会话标识: 新一次上传(新 id)会覆盖旧断点, 杜绝"同名改内容续传拼出脏文件"
    let upload_id = headers
        .get("X-Upload-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let final_path = share.join(&name); // 相对路径逐段已校验, 不会越界
    let part_path = share.join(format!("{name}{UPLOAD_PART_SUFFIX}"));
    let meta_path = part_meta_path(&part_path);
    let need = size.saturating_sub(offset);
    if need > 0 {
        if let Ok(free) = fs2::available_space(&share) {
            if need > free {
                push_log(format!(
                    "上传拒绝 {name}: 磁盘空间不足，需要 {}，可用 {} ← {}",
                    humans(need),
                    humans(free),
                    addr.ip()
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
    // 同一断点文件同一时刻只允许一个写者(客户端超时重试后旧 handler 可能仍挂着)。
    // 等锁带超时: 另一设备正在传同名大文件时, 本请求不能无限排队 ——
    // 客户端单片超时才 30s, 排队到天荒地老只会 8 次重试全数撞死后报"上传中断"
    let part_mutex = part_lock(&part_path);
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
    // 断点校验必须持锁进行(TOCTOU): 否则"校验时断点有效、写入前被并发的 offset0 请求截断"
    // 会 seek 到空文件深处写出零洞; 409 带服务端实际断点, 客户端据此重排
    if offset > 0 {
        // 续传必须携带会话 id: 否则两个都不带 id 的客户端先后传同名文件,
        // 空 meta 与空 id 校验通过, 后者的字节直接拼在前者半截上, 无告警转正为损坏文件
        if upload_id.trim().is_empty() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "续传点已失效，请从头重传", "received": 0 })),
            )
                .into_response();
        }
        let have = tokio::fs::metadata(&part_path).await.map(|m| m.len()).unwrap_or(0);
        let meta_id = tokio::fs::read_to_string(&meta_path).await.unwrap_or_default();
        if have == 0 && meta_id.trim().is_empty() {
            // 断点与 meta 都不存在: 另一设备(相同 upId 语义 = 同名同大小同修改时间,
            // 即同一内容)大概率已转正。最终文件恰为目标大小时直接确认完成——
            // 否则败者会从头重传整份并覆盖, 大文件白白浪费整份带宽
            if size > 0 {
                if let Ok(fm) = tokio::fs::metadata(&final_path).await {
                    if fm.len() == size {
                        return Json(serde_json::json!({ "ok": true, "received": size, "done": true }))
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
        if have == 0 || (meta_id.trim() != upload_id.trim()) {
            // 无断点, 或断点属于另一次上传(同名新内容) → 视为无效, 让客户端从头重传
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "续传点已失效，请从头重传", "received": 0 })),
            )
                .into_response();
        }
        if have < offset {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "续传点已失效，请从头重传", "received": have })),
            )
                .into_response();
        }
        if have > offset {
            // 客户端回退到更早断点: 截断到请求偏移
            if let Ok(f) = tokio::fs::OpenOptions::new().write(true).open(&part_path).await {
                let _ = f.set_len(offset).await;
            }
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
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // offset 0 = 从头传: 必须截断旧断点, 否则更长的陈旧断点会在转正时拖进垃圾尾巴
        .truncate(offset == 0)
        .open(&part_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            push_log(format!("上传失败 {name}: 创建文件失败({e}) ← {}", addr.ip()));
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
        }
    };
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
        push_log(format!("上传失败 {name}: 定位断点失败({e}) ← {}", addr.ip()));
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
    }
    // 从头开始(或首片)时固化本次上传会话 id, 后续分片/断点查询据此校验身份
    if offset == 0 {
        let _ = tokio::fs::write(&meta_path, upload_id.as_bytes()).await;
    }
    let declared = if size > 0 { humans(size) } else { "未知大小".into() };
    if offset == 0 {
        push_log(format!(
            "上传开始 {name} (声明 {declared}) ← {}{}",
            addr.ip(),
            if final_path.exists() { " · 覆盖同名文件" } else { "" }
        ));
    }
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
                    addr.ip()
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
                        addr.ip()
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
                        addr.ip()
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
                                addr.ip()
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
                    addr.ip()
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
            addr.ip()
        ));
        received = size;
    }
    let _ = file.sync_all().await;
    drop(file);
    if size > 0 && received < size {
        // 分片协议: 本片 body 正常收完但整个文件未收齐 → 200 确认本片, 断点保留待续
        return Json(serde_json::json!({ "ok": true, "received": received, "done": false }))
            .into_response();
    }
    if let Some(parent) = final_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    // Windows 的 rename 不覆盖已存在目标, 先删; 删不掉(被资源管理器预览/杀软/Word
    // 占用无 FILE_SHARE_DELETE)时记日志, 不再静默
    if final_path.exists() {
        if let Err(e) = tokio::fs::remove_file(&final_path).await {
            push_log(format!("上传 {name}: 覆盖旧文件失败({e}), 将重试转正 ← {}", addr.ip()));
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
            let _ = tokio::fs::remove_file(&meta_path).await;
            push_log(format!(
                "上传完成 {name} ({}, {}ms{}) ← {}",
                humans(received),
                started.elapsed().as_millis(),
                if offset > 0 { format!("，续传自 {}", humans(offset)) } else { String::new() },
                addr.ip()
            ));
            broadcast_event("files:changed");
            Json(serde_json::json!({ "ok": true, "received": received, "done": true })).into_response()
        }
        Err(e) => {
            push_log(format!("上传失败 {name}: 转正失败({e}) ← {}", addr.ip()));
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
    let part = share.join(format!("{name}{UPLOAD_PART_SUFFIX}"));
    let mut received = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
    // 带会话 id 查询时校验断点归属: 断点属于别的上传 → 返回 0(从头传), 防同名拼接脏文件
    if let Some(id) = params.get("id") {
        let meta_id = tokio::fs::read_to_string(part_meta_path(&part))
            .await
            .unwrap_or_default();
        if meta_id.trim() != id.trim() {
            received = 0;
        }
    }
    Json(serde_json::json!({ "received": received })).into_response()
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
    let Some(name) = safe_rel_path(raw.trim_start_matches('/'), false) else {
        return err(StatusCode::BAD_REQUEST, "非法的文件名");
    };
    let Some(path) = safe_join_existing(&share, &name) else {
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            push_log(format!("删除 {name} ← {}", addr.ip()));
            broadcast_event("files:changed");
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(e) => {
            push_log(format!("删除失败 {name}: {e} ← {}", addr.ip()));
            err(StatusCode::INTERNAL_SERVER_ERROR, format!("删除失败: {e}"))
        }
    }
}

async fn chat_get() -> Response {
    let q = chat_log().lock().unwrap();
    Json(serde_json::json!({ "messages": q.iter().collect::<Vec<_>>() })).into_response()
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
    let text = body.message.trim().to_string();
    if text.is_empty() {
        return err(StatusCode::BAD_REQUEST, "消息内容不能为空");
    }
    let nickname = (body.nickname.unwrap_or_default().trim().to_string())
        .chars()
        .take(NICK_MAX_CHARS)
        .collect::<String>();
    let nickname = if nickname.is_empty() { DEFAULT_NICK.into() } else { nickname };
    let message = text.chars().take(MSG_MAX_CHARS).collect::<String>();
    // id 分配与入队必须在同一临界区: 分两段加锁时并发发送会算出重复 id,
    // 前端 :key="m.id" 出现重复键导致列表渲染错乱
    let msg = {
        let mut q = chat_log().lock().unwrap();
        let id = q.back().map(|m| m.id + 1).unwrap_or(1);
        let msg = ChatMsg {
            id,
            nickname,
            message,
            timestamp: now_f64(),
            ip: addr.ip().to_string(),
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
    push_log(format!("聊天 {}: {preview}{dots} ← {}", msg.nickname, addr.ip()));
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

#[tauri::command]
pub fn control_status_cmd() -> serde_json::Value {
    let (running, port, ip) = status();
    let share = share_dir().to_string_lossy().to_string();
    let (free, total) = disk_space();
    serde_json::json!({
        "running": running, "port": port, "ip": ip, "share": share,
        "free": free.map(humans).unwrap_or_else(|| "无法获取".into()),
        "total": total.map(humans).unwrap_or_else(|| "无法获取".into()),
        "version": APP_VERSION,
        "config": frontend_config(),
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
pub fn set_share_dir_cmd(path: String) -> serde_json::Value {
    if !Path::new(&path).is_dir() {
        return serde_json::json!({ "ok": false, "error": "目录不存在" });
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
    let q = req_log().lock().unwrap();
    Json(serde_json::json!({ "logs": q.iter().cloned().collect::<Vec<_>>() })).into_response()
}

/// 清空服务日志(保留一条标记, 便于确认清空动作已生效)。
pub fn clear_logs() {
    req_log().lock().unwrap().clear();
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
    // 允许: IP / localhost / *.local(mDNS 主机名, 如 MyMac.local; 公网域名无法占 .local,
    // DNS 重绑定经 .local 不可行) —— 其余域名一律拒绝
    let host_ok = host_name == "localhost"
        || host_name.parse::<std::net::IpAddr>().is_ok()
        || host_name.ends_with(".local");
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

/// 请求日志中间件: 每个 HTTP 请求记一行(方法/路径/状态/耗时/来源)。
/// 高频轮询噪音不记录, 清单见 config.rs 的 NOISY_GET_PATHS
/// (与前端 config.js 的轮询路径一一对应, 前端调整轮询时需同步)。
async fn log_middleware(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(resp) = browser_origin_guard(&req) {
        return resp;
    }
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // 控制接口(启停/日志/清空)仅限本机: 桌面壳走 127.0.0.1 回环或 Tauri 命令,
    // 局域网任意设备可 curl 调用的话, 任何人能关停服务/读走全部活动日志(文件名+IP+聊天预览)
    if path.starts_with("/api/control/") && !addr.ip().is_loopback() {
        return err(StatusCode::FORBIDDEN, "控制接口仅限本机访问");
    }
    let query = req.uri().query().unwrap_or("").to_string();
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
        || path == "/favicon.ico";
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
            format!("?{query}")
        };
        push_log(format!(
            "{method} {path}{q} → {} {size} {ms}ms ← {} ({who})",
            res.status().as_u16(),
            addr.ip()
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
