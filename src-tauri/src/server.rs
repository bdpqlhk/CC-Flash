//! 局域网文件服务(axum 重写版, 协议与 Python 版完全一致)。
//!
//! API:
//!   GET  /                    → 前端页面(嵌入 ../dist)
//!   GET  /api/files           → 文件列表 JSON
//!   GET  /api/download/:name  → 流式下载(Content-Disposition 带 RFC5987)
//!   POST /api/upload          → 请求头 X-Filename(URL 编码) + 原始体流式落盘;
//!                                先查磁盘剩余空间;失败自动清理半截文件
//!   GET  /api/chat            → 聊天记录(保留最近 200 条)
//!   POST /api/chat            → 昵称截 20 字、消息截 2000 字
//!   DELETE /api/delete/:name  → 删除文件
//!   GET  /api/info            → { ip, port, running, version, config }
//!   GET/POST /api/control/*   → 桌面端服务启停/状态/日志(仅桌面壳使用)
//!
//! 路径安全:URL 解码 → 跨平台 basename(拒绝空/./..);下载/删除前再做
//! canonicalize + 前缀越界校验,与 Python 版双重防护等价。

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
    sync::{atomic::Ordering, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;

use crate::config::{
    APP_VERSION, CHAT_MAX, CHAT_PREVIEW_CHARS, CHUNK_HINT, DEFAULT_NICK, DEFAULT_PORT,
    MSG_MAX_CHARS, NICK_MAX_CHARS, NOISY_GET_PATHS, POLL_CHAT_MS, POLL_FILES_MS, POLL_LOGS_MS,
    POLL_STATUS_MS, PORT_PROBE_EXTRA, PORT_RANGE_END, REQ_LOG_MAX, SHARE_DIR_NAME,
    SHARE_DIR_NAME_LEGACY, SHARE_PREF_FILE,
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
    let mut q = req_log().lock().unwrap();
    q.push_back(format!("[{}] {}", now_hms(), line));
    while q.len() > REQ_LOG_MAX {
        q.pop_front();
    }
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
    #[cfg(target_os = "windows")]
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".into());
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(SHARE_PREF_FILE)
}

fn default_share_dir() -> PathBuf {
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
    // 用户曾手动选择过目录 → 恢复该选择(目录仍存在时)
    if let Ok(saved) = std::fs::read_to_string(share_pref_file()) {
        let saved = saved.trim();
        if !saved.is_empty() && Path::new(saved).is_dir() {
            return PathBuf::from(saved);
        }
    }
    new
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
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]).arg(target);
        c
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
    let s = UdpSocket::bind("0.0.0.0:0").ok();
    if let Some(s) = s {
        if s.connect("10.255.255.255:1").is_ok() {
            if let Ok(addr) = s.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    "127.0.0.1".into()
}

// ---------------------------------------------------------------- //
// 启停管理
// ---------------------------------------------------------------- //

/// 启动服务(优先尝试指定端口, 被占用自动顺延);返回实际端口。
/// 注意:tokio 的 socket 操作必须在 runtime 上下文内,故整体包进 block_on。
pub fn start(preferred: Option<u16>) -> Result<u16, String> {
    {
        let lock = running_slot().lock().unwrap();
        if let Some(h) = lock.as_ref() {
            return Ok(h.port);
        }
    }
    let share = share_dir();
    std::fs::create_dir_all(&share)
        .map_err(|e| format!("共享目录不可用: {e}"))?;

    tauri::async_runtime::block_on(async move {
        let mut candidates: Vec<u16> = Vec::new();
        if let Some(p0) = preferred {
            candidates.push(p0);
            candidates.extend(p0 + 1..p0 + 1 + PORT_PROBE_EXTRA);
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
        Ok(port)
    })
}

/// 停止服务(优雅关闭, 不阻塞调用方)。
pub fn stop() -> bool {
    let mut lock = running_slot().lock().unwrap();
    if let Some(h) = lock.take() {
        push_log("服务已停止".into());
        let _ = h.shutdown.send(());
        drop(h.task); // daemon 语义: 不等待
        true
    } else {
        false
    }
}

/// 应用启动时自动拉起(失败仅打日志, 不阻断 GUI)。
pub fn autostart() -> Result<u16, String> {
    start(None)
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

fn safe_name(raw: &str) -> Option<String> {
    let decoded = percent_decode_str(raw).decode_utf8_lossy().to_string();
    let name = decoded.replace('\\', "/");
    let name = Path::new(&name).file_name()?.to_string_lossy().to_string();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(name)
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

async fn files(State(share): State<PathBuf>) -> Response {
    let mut out: Vec<FileEntry> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&share) {
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if !md.is_file() {
                continue;
            }
            let modified = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            out.push(FileEntry {
                name: e.file_name().to_string_lossy().to_string(),
                size: md.len(),
                modified,
            });
        }
    }
    out.sort_by(|a, b| b.modified.total_cmp(&a.modified));
    Json(serde_json::json!({ "files": out })).into_response()
}

async fn download(
    State(share): State<PathBuf>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    AxPath(name): AxPath<String>,
) -> Response {
    let Some(name) = safe_name(&name) else {
        return err(StatusCode::BAD_REQUEST, "非法的文件名");
    };
    let Some(path) = safe_join_existing(&share, &name) else {
        push_log(format!("下载失败 {name}: 文件不存在 ← {}", addr.ip()));
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    let Ok(std_file) = std::fs::File::open(&path) else {
        push_log(format!("下载失败 {name}: 打开失败 ← {}", addr.ip()));
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    push_log(format!("下载 {name} ({}) ← {}", humans(size), addr.ip()));
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    let enc = percent_encoding::utf8_percent_encode(&name, FRAGMENT);
    let reader = tokio::io::BufReader::new(tokio::fs::File::from_std(std_file));
    let stream = tokio_util::io::ReaderStream::with_capacity(reader, CHUNK_HINT);
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, mime.essence_str().parse().unwrap());
    // 显式给出长度: 浏览器原生下载可显示进度, 前端也可预知大小
    if size > 0 {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    }
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename*=UTF-8''{enc}"
        ))
        .unwrap(),
    );
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    (headers, Body::from_stream(stream)).into_response()
}

async fn upload(
    State(share): State<PathBuf>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let raw = headers
        .get("X-Filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(name) = safe_name(raw) else {
        return err(StatusCode::BAD_REQUEST, "缺少或非法的文件名");
    };
    let size: u64 = headers
        .get("X-File-Size")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if size > 0 {
        if let Ok(free) = fs2::available_space(&share) {
            if size > free {
                push_log(format!(
                    "上传拒绝 {name}: 磁盘空间不足(需要 {}，可用 {}) ← {}",
                    humans(size),
                    humans(free),
                    addr.ip()
                ));
                return err(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "磁盘空间不足，需要 {}，可用 {}",
                        humans(size),
                        humans(free)
                    ),
                );
            }
        }
    }
    let path = share.join(&name); // basename 已保证不越界
    let overwrite = path.exists();
    let mut file = match tokio::fs::File::create(&path).await {
        Ok(f) => f,
        Err(e) => {
            push_log(format!("上传失败 {name}: 创建文件失败({e}) ← {}", addr.ip()));
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
        }
    };
    push_log(format!(
        "上传开始 {name} (声明 {}) ← {}{}",
        if size > 0 { humans(size) } else { "未知大小".into() },
        addr.ip(),
        if overwrite { " · 覆盖同名文件" } else { "" }
    ));
    let started = std::time::Instant::now();
    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(c) => {
                if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut file, &c).await {
                    let _ = tokio::fs::remove_file(&path).await;
                    push_log(format!(
                        "上传失败 {name}: 写入失败({e})，已清理半截文件 ← {}",
                        addr.ip()
                    ));
                    return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
                }
                written += c.len() as u64;
            }
            Err(_) => {
                let _ = tokio::fs::remove_file(&path).await;
                push_log(format!(
                    "上传中断 {name}: 连接断开，已清理半截文件 ← {}",
                    addr.ip()
                ));
                return err(StatusCode::INTERNAL_SERVER_ERROR, "上传中断");
            }
        }
    }
    let _ = file.sync_all().await;
    push_log(format!(
        "上传完成 {name} ({}, {}ms) ← {}",
        humans(written),
        started.elapsed().as_millis(),
        addr.ip()
    ));
    Json(serde_json::json!({ "ok": true, "name": name, "size": written })).into_response()
}

fn humans(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", units[i])
}

async fn delete_file(
    State(share): State<PathBuf>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    AxPath(name): AxPath<String>,
) -> Response {
    let Some(name) = safe_name(&name) else {
        return err(StatusCode::BAD_REQUEST, "非法的文件名");
    };
    let Some(path) = safe_join_existing(&share, &name) else {
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            push_log(format!("删除 {name} ← {}", addr.ip()));
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
    let q = chat_log().lock().unwrap();
    let id = q.back().map(|m| m.id + 1).unwrap_or(1);
    let msg = ChatMsg {
        id,
        nickname,
        message,
        timestamp: now_f64(),
        ip: addr.ip().to_string(),
    };
    drop(q);
    let mut q = chat_log().lock().unwrap();
    q.push_back(msg.clone());
    while q.len() > CHAT_MAX {
        q.pop_front();
    }
    drop(q);
    let preview: String = msg.message.chars().take(CHAT_PREVIEW_CHARS).collect();
    let dots = if msg.message.chars().count() > CHAT_PREVIEW_CHARS { "…" } else { "" };
    push_log(format!("聊天 {}: {preview}{dots} ← {}", msg.nickname, addr.ip()));
    Json(serde_json::json!({ "ok": true, "id": id })).into_response()
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
    let Some(name) = safe_name(&name) else {
        return serde_json::json!({ "ok": false, "error": "非法的文件名" });
    };
    let path = share_dir().join(&name);
    if !path.is_file() {
        return serde_json::json!({ "ok": false, "error": "文件不存在" });
    }
    let p = path.to_string_lossy().to_string();
    #[cfg(target_os = "macos")]
    let res = std::process::Command::new("osascript").args(["-e", &format!(
        "set the clipboard to POSIX file \"{}\"", p.replace('"', "\\\""))])
        .output();
    #[cfg(target_os = "windows")]
    let res = {
        let ps = format!(
            "Set-Clipboard -LiteralPath '{}'", p.replace('\'', "''"));
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .output()
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

async fn ctrl_start() -> Response {
    match start(None) {
        Ok(port) => Json(serde_json::json!({ "ok": true, "port": port })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn ctrl_stop() -> Response {
    Json(serde_json::json!({ "ok": stop() })).into_response()
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

/// 请求日志中间件: 每个 HTTP 请求记一行(方法/路径/状态/耗时/来源)。
/// 高频轮询噪音不记录, 清单见 config.rs 的 NOISY_GET_PATHS
/// (与前端 config.js 的轮询路径一一对应, 前端调整轮询时需同步)。
async fn log_middleware(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    // 昵称: 优先 X-Nickname 头, 其次 ?nick= 参数(浏览器端避免自定义头触发 CORS 预检)
    let qnick = query.split('&').find_map(|kv| kv.strip_prefix("nick=")).map(|v| {
        percent_encoding::percent_decode_str(v).decode_utf8_lossy().to_string()
    });
    let nick = qnick.unwrap_or_else(|| {
        req.headers()
            .get("X-Nickname")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    });
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
        .route("/api/download/:name", get(download))
        .route("/api/upload", post(upload))
        .route("/api/delete/:name", delete(delete_file))
        .route("/api/chat", get(chat_get).post(chat_post))
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
