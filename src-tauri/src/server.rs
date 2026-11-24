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
//!   POST /api/chat            → 昵称截 20 字、消息上限 20 万字(超限 400, 可带 quote 引用快照)
//!   DELETE /api/chat/{id}     → 删除单条消息
//!   GET/POST/DELETE /api/clipboard → 同一服务下所有设备共用的文本剪贴板(2 万字上限)
//!   DELETE /api/delete/*name  → 删除文件(支持子目录)
//!   GET  /api/info            → { ip, port, running, version, config }
//!   GET/POST /api/control/*   → 桌面端服务启停/状态/日志(仅桌面壳使用)
//!   GET  /ws                  → WebSocket 事件推送(聊天/列表/状态变化, 轮询为降级通道)
//!
//! 路径安全:URL 解码 → 逐段校验(拒绝空/./.. 与非法字符);下载/删除前再做
//! canonicalize + 前缀越界校验,双重防护等价于旧版 basename 方案。

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path as AxPath, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use futures_util::StreamExt;
use percent_encoding::percent_decode_str;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use std::sync::atomic::{AtomicBool, AtomicU16};
use tokio::sync::{broadcast, oneshot};

use crate::config::{
    APP_VERSION, CHAT_MAX, CHAT_PREVIEW_CHARS, CHAT_TOTAL_MAX_CHARS, CHUNK_HINT, DEFAULT_NICK,
    DEFAULT_PORT, FILES_LIST_MAX, LOG_LINE_MAX, MSG_MAX_CHARS, NICK_MAX_CHARS, NOISY_GET_PATHS,
    PIN_ENABLED_PREF_FILE, PIN_PREF_FILE,
    POLL_CHAT_MS, POLL_FILES_MS, POLL_LOGS_MS, POLL_STATUS_MS, PORT_PREF_FILE, PORT_PROBE_EXTRA,
    PORT_RANGE_END, REQ_LOG_MAX,
    SHARE_DIR_NAME, SHARE_DIR_NAME_LEGACY, SHARE_PREF_FILE, TICKET_FAIL_FREE,
    TICKET_FAIL_WINDOW_SECS, UPLOAD_FREE_CHECK_BYTES, UPLOAD_FREE_RESERVE_BYTES,
    UPLOAD_IDLE_TIMEOUT_SECS, UPLOAD_MAX_DEPTH, UPLOAD_MAX_DURATION_SECS,
    UPLOAD_PART_META_SUFFIX,
    UPLOAD_PART_SUFFIX, UPLOAD_PART_TTL_SECS,
};

// ---------------------------------------------------------------- //
// 全局状态
// ---------------------------------------------------------------- //

#[derive(Clone, Deserialize, Serialize)]
pub struct ChatMsg {
    pub id: u64,
    pub nickname: String,
    pub message: String,
    pub timestamp: f64,
    /// 来源 IP 只用于服务端日志, 不落盘也不下发给客户端
    #[serde(skip_serializing, default)]
    #[allow(dead_code)]
    pub ip: String,
    /// 引用回复: 被引用消息的快照(昵称 + 内容), 原消息删除后引用仍可读
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<ChatQuote>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ChatQuote {
    pub id: u64,
    pub nickname: String,
    pub message: String,
}

/// 文件投递事件：与文本聊天分开持久化，避免整目录上传把 200 条聊天上限挤爆；
/// GET /api/chat 一并下发，客户端在时间线里渲染为可下载/分享的文件卡片。
/// 刷新浏览器或重启 App 后仍能看到"发送过哪些文件"。
#[derive(Clone, Deserialize, Serialize)]
pub struct FileEvent {
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub uploader: String,
    pub timestamp: f64,
}

const FILE_EVENT_MAX: usize = 200;

fn file_events_file() -> Option<PathBuf> {
    if cfg!(test) { return None; }
    ui_pref_dir().map(|d| d.join("file-history.json"))
}

fn file_events() -> &'static Mutex<VecDeque<FileEvent>> {
    static F: OnceLock<Mutex<VecDeque<FileEvent>>> = OnceLock::new();
    F.get_or_init(|| {
        let q = file_events_file()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str::<VecDeque<FileEvent>>(&raw).ok())
            .unwrap_or_default();
        Mutex::new(q)
    })
}

/// 聊天记录持久化文件(与主题偏好同目录)。服务重启/应用重启后恢复最近 CHAT_MAX 条,
/// 修复"关掉 CC-Flash 再打开聊天全空"的问题。
fn chat_history_file() -> Option<PathBuf> {
    // 单测共享进程/真实用户目录: 测试时不读写持久化, 保持 chat_log 的全新基线
    if cfg!(test) { return None; }
    ui_pref_dir().map(|d| d.join("chat-history.json"))
}

fn load_chat_history() -> VecDeque<ChatMsg> {
    let Some(path) = chat_history_file() else { return VecDeque::new(); };
    let Ok(raw) = std::fs::read_to_string(&path) else { return VecDeque::new(); };
    let mut q: VecDeque<ChatMsg> = match serde_json::from_str(&raw) {
        Ok(q) => q,
        Err(e) => {
            // 损坏时不能直接按空历史继续: 下一条消息会用新队列覆盖原文件, 旧记录
            // 永久消失。把原文件改名保留(用户/开发者还可人工恢复), 再以空历史启动。
            let stamp = now_f64().trunc() as u64;
            let bad = path.with_file_name(format!("chat-history.json.bad-{stamp}"));
            let kept = std::fs::rename(&path, &bad).is_ok();
            push_log_warn(if kept {
                format!(
                    "聊天历史解析失败({e})，原文件已保留为 {}",
                    bad.file_name().and_then(|s| s.to_str()).unwrap_or("chat-history.json.bad")
                )
            } else {
                format!("聊天历史解析失败({e})，原文件保留失败，请手动备份后再继续")
            });
            VecDeque::new()
        }
    };
    let mut total = chat_history_chars(&q);
    while q.len() > CHAT_MAX || total > CHAT_TOTAL_MAX_CHARS {
        let Some(old) = q.pop_front() else { break };
        total = total.saturating_sub(chat_msg_chars(&old));
    }
    q
}

/// 聊天历史落盘串行化: 调用方在锁内只做内存序列化, 慢磁盘 IO 交给 spawn_blocking。
/// 序号保证"较新的快照不会被迟到的旧写覆盖"; 固定 tmp 名由保存锁串行化, 不会互相踩踏。
static CHAT_SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CHAT_SAVED_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn chat_save_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

fn chat_history_json(q: &VecDeque<ChatMsg>) -> Option<Vec<u8>> {
    serde_json::to_vec(q).ok()
}

fn write_chat_history_bytes(path: &Path, json: &[u8]) -> bool {
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
    }
    // 内部是"临时文件 + fsync + 原子 rename"; 失败必须返回 false,
    // 否则序号继续前移, 下一次更旧/更小快照也可能被误认为已落盘。
    write_secret_atomic(path, json).is_ok()
}

/// 异步落盘: 聊天 handler 不再在 async 上下文里做磁盘 IO(共享目录/磁盘忙时
/// 会阻塞 tokio worker, 拖慢 WS 心跳与所有 API)。首次调用后按序号串行写。
fn schedule_chat_save(json: Option<Vec<u8>>) {
    let (Some(path), Some(json)) = (chat_history_file(), json) else { return };
    let seq = CHAT_SAVE_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let task = move || {
        // 300ms 合并窗口: 连续发送/粘贴时只落最新快照(序号检查会跳过旧任务),
        // 避免 20 万字消息配合大历史时每条都整段重写 + fsync。
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _g = chat_save_lock().lock().unwrap_or_else(|e| e.into_inner());
        if seq <= CHAT_SAVED_SEQ.load(Ordering::SeqCst) {
            return; // 已有更新的快照落盘
        }
        if write_chat_history_bytes(&path, &json) {
            CHAT_SAVED_SEQ.store(seq, Ordering::SeqCst);
        }
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let _ = handle.spawn_blocking(task);
    } else {
        task();
    }
}

/// 文件事件的落盘与聊天同款：合并窗口 + 序号，确保迟到的旧快照不会覆盖新快照。
static FILE_SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FILE_SAVED_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn file_save_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

fn schedule_file_events_save() {
    let Some(path) = file_events_file() else { return };
    let json = {
        let q = file_events().lock().unwrap_or_else(|e| e.into_inner());
        match serde_json::to_vec(&*q) {
            Ok(j) => j,
            Err(e) => {
                push_log_err(format!("文件历史序列化失败: {e}"));
                return;
            }
        }
    };
    let seq = FILE_SAVE_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let task = move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _g = file_save_lock().lock().unwrap_or_else(|e| e.into_inner());
        if seq <= FILE_SAVED_SEQ.load(Ordering::SeqCst) {
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if write_secret_atomic(&path, &json).is_ok() {
            FILE_SAVED_SEQ.store(seq, Ordering::SeqCst);
        }
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let _ = handle.spawn_blocking(task);
    } else {
        task();
    }
}

/// 上传完成后记账为一条"文件消息"，并广播 chat:new 让所有端刷新时间线。
fn record_file_event(name: String, size: u64, uploader: String) {
    {
        let mut q = file_events().lock().unwrap_or_else(|e| e.into_inner());
        // 同名重传只保留最新一条: 否则同一路径会在时间线里出现多张卡片
        q.retain(|e| e.name != name);
        q.push_back(FileEvent {
            name,
            size,
            uploader,
            timestamp: now_f64(),
        });
        while q.len() > FILE_EVENT_MAX {
            q.pop_front();
        }
    }
    schedule_file_events_save();
    broadcast_event("chat:new");
}

/// 单调递增的消息 id: 删除最新一条后绝不复用它的 id —— 另一端未及时刷新的
/// "删除 id=X" 会把刚发出的新消息误删。
static NEXT_CHAT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_chat_id() -> u64 {
    NEXT_CHAT_ID.fetch_add(1, Ordering::SeqCst) + 1
}

/// 单条消息计入历史预算的字符数(昵称 + 正文 + 引用快照)。
fn chat_msg_chars(m: &ChatMsg) -> usize {
    m.nickname.chars().count()
        + m.message.chars().count()
        + m.quote
            .as_ref()
            .map(|q| q.nickname.chars().count() + q.message.chars().count())
            .unwrap_or(0)
}
fn chat_history_chars(q: &VecDeque<ChatMsg>) -> usize {
    q.iter().map(chat_msg_chars).sum()
}

fn chat_log() -> &'static Mutex<VecDeque<ChatMsg>> {
    static CHAT: OnceLock<Mutex<VecDeque<ChatMsg>>> = OnceLock::new();
    CHAT.get_or_init(|| {
        let q = load_chat_history();
        let max = q.iter().map(|m| m.id).max().unwrap_or(0);
        NEXT_CHAT_ID.store(max, Ordering::SeqCst);
        Mutex::new(q)
    })
}

// ---------------------------------------------------------------- //
// 附近设备(最小实现): 已连接/最近活跃的局域网设备, 供网页端与桌面端展示
// 不引入 UDP/mDNS, 复用现有 HTTP/WS; 浏览器端仍以扫码/手动地址作为入口兜底
// ---------------------------------------------------------------- //

// 对方每 3s 宣告一次; 30s 允许短暂丢包/休眠而不把设备从列表直接移除。
// 前端在线态另有 12s 宽限, 避免 15s TTL 与 3s 轮询之间出现状态抖动。
const DEVICE_TTL_SECS: f64 = 30.0;

#[derive(Clone, Serialize, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub ip: String,
    pub port: u16,
    pub nickname: String,
    pub platform: String,
    pub busy: bool,
    pub last_seen: f64,
    /// 发现报文里的会话令牌: bye 必须带回同一令牌才允许移除设备。
    /// 令牌随 announce 明文广播，能防住无嗅探能力的盲伪造，但不构成强认证；
    /// 不序列化给前端，避免无意义地下发。
    #[serde(skip_serializing, default)]
    pub token: String,
}

#[derive(Deserialize)]
struct HelloIn {
    nickname: Option<String>,
}

/// 附近设备只来自 UDP 多播发现(带真实监听端口)。浏览器客户端没有自己的
/// 监听端口, 无法接收点对点投递 —— 之前把它们登记成 `ip:服务端端口`,
/// 用户点击"发送"必然连到不存在的服务, 现只保留本机昵称上报。
const DISCOVERY_PEERS_MAX: usize = 64;

// 真正的局域网多播发现: 桌面实例每 5s 向 239.255.42.99:53318 广播自身,
// 同时监听其他实例; 浏览器/手机无法主动多播, 仍复用 HTTP hello 上报。
const DISCOVERY_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);
/// IPv6 link-local 多播组(自定义 group id, ff02::/16 内有效)
const DISCOVERY_GROUP_V6: std::net::Ipv6Addr =
    std::net::Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xccf1);
const DISCOVERY_PORT: u16 = 53318;
/// 发现端口顺延范围: 53318 被占用时依次尝试后续端口; 发送端向整个范围广播,
/// 因此不同实例即使因冲突落在不同端口也能互相发现。
const DISCOVERY_PORT_LAST: u16 = 53321;
const DISCOVERY_APP: &str = "ccflash";
static DISCOVERY_ACTIVE_PORT: AtomicU16 = AtomicU16::new(0);
fn discovery_active_port() -> u16 {
    match DISCOVERY_ACTIVE_PORT.load(Ordering::Relaxed) {
        0 => DISCOVERY_PORT,
        p => p,
    }
}

fn multicast_peers() -> &'static Mutex<HashMap<String, DeviceInfo>> {
    static PEERS: OnceLock<Mutex<HashMap<String, DeviceInfo>>> = OnceLock::new();
    PEERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 只有"id 相同且来源确实是本机"才算自己的回环包。两台机器如果复制了同一份
/// discovery-id(整机镜像/克隆配置), 仅凭 id 相同会互相忽略; 此时来源 IP 不是本机,
/// 必须按正常设备处理。
fn is_self_discovery_packet(id: Option<&str>, src_ip: std::net::IpAddr) -> bool {
    id == Some(discovery_id()) && is_local_source(src_ip)
}

/// 本次进程的发现会话令牌: announce 携带, bye 必须匹配。
/// 每进程轮换, 不落盘; 老版本对端没有 tok 时按兼容处理(允许空令牌)。
fn discovery_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let mut bytes = [0u8; 8];
        fill_random(&mut bytes);
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    })
}

fn discovery_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        // 持久化到 app_config_dir/discovery-id: 设备重启后 id 不变,
        // 其他端的收藏不会丢, 列表也不会出现"同一台设备两个条目"。
        // 测试不落盘, 避免污染真实用户配置。
        let file = if cfg!(test) {
            None
        } else {
            ui_pref_dir().map(|d| d.join("discovery-id"))
        };
        if let Some(p) = &file {
            if let Ok(s) = std::fs::read_to_string(p) {
                let t = s.trim().to_string();
                if t.len() == 16
                    && t.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                {
                    return t;
                }
            }
        }
        let mut b = [0u8; 8];
        fill_random(&mut b);
        let id: String = b.iter().map(|x| format!("{x:02x}")).collect();
        if let Some(p) = &file {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p, &id);
        }
        id
    })
}

/// 最近发现过的设备 IP(最多 32): 多播被企业/路由器过滤时, 启动和手动刷新
/// 会直接向这些地址发单播 probe, 不再依赖组播能否通过。
fn peer_cache_file() -> Option<PathBuf> {
    if cfg!(test) { return None; }
    ui_pref_dir().map(|d| d.join("peer-cache.json"))
}

fn peer_cache() -> &'static Mutex<Vec<String>> {
    static CACHE: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let saved = peer_cache_file()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();
        Mutex::new(saved.into_iter().filter(|s| !s.trim().is_empty()).take(32).collect())
    })
}

fn cached_peer_ips() -> Vec<String> {
    peer_cache().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn remember_peer_ip(ip: &str) {
    let ip = ip.trim();
    if ip.is_empty() { return; }
    let mut cache = peer_cache().lock().unwrap_or_else(|e| e.into_inner());
    if cache.iter().any(|x| x == ip) { return; }
    cache.push(ip.to_string());
    if cache.len() > 32 { cache.remove(0); }
    if let Some(path) = peer_cache_file() {
        if let Ok(json) = serde_json::to_string(&*cache) {
            let _ = write_secret_atomic(&path, json.as_bytes());
        }
    }
}

fn local_name_slot() -> &'static Mutex<String> {
    static NAME: OnceLock<Mutex<String>> = OnceLock::new();
    NAME.get_or_init(|| Mutex::new(String::new()))
}

fn set_local_name(name: &str) {
    let name = name.trim();
    if name.is_empty() { return; }
    *local_name_slot().lock().unwrap_or_else(|e| e.into_inner()) = name.chars().take(24).collect();
}

fn local_name() -> String {
    let configured = local_name_slot().lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !configured.is_empty() { return configured; }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "CC Flash 电脑".into())
}

#[derive(Default, Clone, Copy)]
struct IngestResult {
    new_device: bool,
    changed: bool,
    /// 本条报文是否被接受并刷新了 last_seen（即使字段未变）。
    heard: bool,
}

/// 解析并登记一条发现报文。返回是否首次出现/状态变化(用于广播 devices:changed)。
/// 独立成函数以便直接跑单测, 不依赖真实网卡 / 端口 / 等待 5s 广播周期。
fn ingest_peer_packet_ex(v: &serde_json::Value, src: std::net::SocketAddr) -> IngestResult {
    if v.get("app").and_then(|x| x.as_str()) != Some(DISCOVERY_APP) {
        return IngestResult::default();
    }
    // 只接受私网/链路本地/回环之外的公网来源一律丢弃: 发现目标是局域网设备,
    // 公网地址既点不通也不该出现在「附近设备」里(前端还有一道 isLanHost 兜底)。
    if !is_lan_peer(src.ip()) {
        return IngestResult::default();
    }
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    // 自身包(多播 loop)与异常 id 直接丢弃
    if id.is_empty() || id.len() > 64 || is_self_discovery_packet(Some(&id), src.ip()) {
        return IngestResult::default();
    }
    let port = v.get("port").and_then(|x| x.as_u64()).unwrap_or(0);
    // 系统端口(1-1023)不可能是 CC-Flash 服务: 拒绝可减少"伪造包把发送方
    // 指向内网其他服务"的 SSRF 面; 0/超界同样拒绝。
    if !(1024..=u16::MAX as u64).contains(&port) {
        return IngestResult::default();
    }
    let clean = |s: &str| {
        s.chars()
            .filter(|c| !c.is_control() && !('\u{202A}'..='\u{202E}').contains(c))
            .take(24)
            .collect::<String>()
    };
    let nickname = clean(v.get("name").and_then(|x| x.as_str()).unwrap_or("CC Flash 电脑"));
    let nickname = if nickname.is_empty() {
        "未命名设备".into()
    } else {
        nickname
    };
    let platform = clean(v.get("platform").and_then(|x| x.as_str()).unwrap_or(""));
    let token = v.get("tok").and_then(|x| x.as_str()).unwrap_or("")
        .chars().filter(|c| c.is_ascii_hexdigit()).take(64).collect::<String>();
    let busy = v.get("busy").and_then(|x| x.as_bool()).unwrap_or(false);
    let ip = source_ip_string(src);
    let mut map = multicast_peers().lock().unwrap_or_else(|e| e.into_inner());
    let known = map.contains_key(&id);
    let previous = map.get(&id).cloned();
    // 已登记设备 id 只允许从原 IP 续期: id 是明文广播的, 否则同网段任意主机
    // 可用"已知 id + 自己的 IP"覆盖目标条目(列表显示真设备名, 发送却落到攻击者)。
    // 真实设备换 IP(DHCP)时最多等旧条目 TTL(30s)自然过期后重新登记。
    if let Some(prev) = &previous {
        if prev.ip != ip {
            // 同一台设备会同时发出 IPv4 与 IPv6 link-local 宣告。若先到的 IPv6
            // 包把地址锁定，另一族会被这里的"换 IP"保护永久拒绝：设备显示在线，
            // 但直发用的 `fe80::%zone` URL 在 WKWebView/WebView2 里往往不可用
            // （IPv6 zone 不在 URL 规范内），表现为"发送失败：离线/需要配对码"。
            // 允许 IPv6 link-local → IPv4 的同设备迁移（令牌一致才放行）；
            // 反向（IPv4 → link-local）忽略，保持 IPv4 优先。
            let prev_link_local_v6 = prev.ip.starts_with("fe80:") || prev.ip.contains('%');
            let new_ipv4 = !ip.contains(':');
            // 迁移必须证明是同一进程: 双方令牌都非空且一致。旧版空令牌不迁移,
            // 否则局域网伪造者可用公开的 id 把发送目标劫持到自己的 IPv4。
            let token_ok = !prev.token.is_empty() && prev.token == token;
            if !(prev_link_local_v6 && new_ipv4 && token_ok) {
                return IngestResult::default();
            }
        }
        // 同 id 但令牌变化：只接受空令牌的旧版对端；否则伪造者可用已知 id
        // 改掉端口/昵称。设备正常重启会先让旧条目 TTL 过期。
        if !prev.token.is_empty() && !token.is_empty() && prev.token != token {
            return IngestResult::default();
        }
    }
    let same_ip_other = map.values().any(|d| d.ip == ip && d.id != id);
    // 同一 IP 已有一个未过期 id 时，不允许"新 id"直接把真实设备挤掉：
    // UDP 源 IP 可伪造，否则攻击者可用随机 id 驱逐目标并伪造名称/端口。
    // 真设备换 id/换 DHCP 地址时，旧条目最多 30s 自然过期后再登记。
    if !known && same_ip_other {
        return IngestResult::default();
    }
    if !known && map.len() >= DISCOVERY_PEERS_MAX {
        return IngestResult::default();
    }
    // 同一源 IP 只保留最新 id: 设备重启换 id 时不堆重复项, 也挡住单 IP 用
    // 随机 id 洪泛占满 64 条上限(不同 IP 的伪造仍受上限约束)。
    map.retain(|_, d| d.ip != ip);
    let is_new = !known;
    let changed = previous.map(|old| {
        old.port != port as u16
            || old.nickname != nickname
            || old.busy != busy
            || old.platform != platform
            || old.ip != ip
    }).unwrap_or(false);
    map.insert(
        id.clone(),
        DeviceInfo {
            id,
            ip: ip.clone(),
            port: port as u16,
            nickname,
            platform,
            busy,
            last_seen: now_f64(),
            token,
        },
    );
    drop(map);
    remember_peer_ip(&ip);
    IngestResult { new_device: is_new, changed: is_new || changed, heard: true }
}

/// 单测兼容: 只关心是否首次出现
#[cfg(test)]
fn ingest_peer_packet(v: &serde_json::Value, src_ip: std::net::IpAddr) -> bool {
    ingest_peer_packet_ex(v, std::net::SocketAddr::new(src_ip, 0)).new_device
}

/// 在线判定与服务端 TTL 分开: 12s 无声宣告只表示"暂时失联", 30s 才从列表移除。
/// 前端直接使用服务端下发的 online, 不再用浏览器时钟自行推算。
const PEER_ONLINE_GRACE_SECS: f64 = 12.0;

/// 设备列表快照: 剔除 30s 未更新的条目, 按最后活跃排序, 同一 ip:port 只留最新一条。
fn device_snapshot(now: f64) -> Vec<DeviceInfo> {
    let mut list: Vec<DeviceInfo> = {
        let mut peers = multicast_peers().lock().unwrap_or_else(|e| e.into_inner());
        peers.retain(|_, d| now - d.last_seen < DEVICE_TTL_SECS);
        peers.values().cloned().collect()
    };
    list.sort_by(|a, b| b.last_seen.total_cmp(&a.last_seen));
    // 用元组而不是 "ip:port" 字符串: IPv6 地址本身含冒号, 字符串拼键有碰撞可能
    let mut seen: HashSet<(String, u16)> = HashSet::new();
    list.retain(|d| seen.insert((d.ip.clone(), d.port)));
    list
}

/// 广播准入: 服务在跑、开放了局域网、且已绑定端口才广播。
/// 安全模式(仅 127.0.0.1)下必须为 false —— 否则别人会看到手机却连不上。
/// 设备 JSON: 在 DeviceInfo 的公开字段上补一个服务端计算的 online，
/// 避免各客户端时钟/网络延迟导致在线态判断不一致。
fn device_json(d: &DeviceInfo, now: f64) -> serde_json::Value {
    let mut v = serde_json::to_value(d).unwrap_or_else(|_| serde_json::json!({}));
    v["online"] = serde_json::json!(now - d.last_seen < PEER_ONLINE_GRACE_SECS);
    v
}

fn device_list_json(now: f64) -> Vec<serde_json::Value> {
    device_snapshot(now).iter().map(|d| device_json(d, now)).collect()
}

fn schedule_devices_presence() {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    static PENDING: AtomicBool = AtomicBool::new(false);
    const MIN_GAP_MS: u64 = 1000;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let last = LAST_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now_ms.saturating_sub(last) >= MIN_GAP_MS {
        LAST_MS.store(now_ms, std::sync::atomic::Ordering::Relaxed);
        broadcast_devices_presence();
        return;
    }
    // 高频报文合并: 1s 内只再补一次尾包，保证最终状态一定送达。
    if PENDING.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let wait = std::time::Duration::from_millis(
        MIN_GAP_MS.saturating_sub(now_ms.saturating_sub(last)),
    );
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        LAST_MS.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        PENDING.store(false, std::sync::atomic::Ordering::Relaxed);
        broadcast_devices_presence();
    });
}

/// 实时在线态推送: 每条有效宣告/bye 后把所有设备连同 online 一起发给订阅端，
/// 前端直接应用，不再等 3s 轮询或自己算时间差。
fn broadcast_devices_presence() {
    let now = now_f64();
    let payload = serde_json::json!({
        "type": "devices:presence",
        "devices": device_list_json(now),
    });
    let _ = event_bus().send(payload.to_string());
}

fn broadcast_allowed(running: bool, lan_active: bool, port: u16) -> bool {
    running && lan_active && port != 0
}

/// 启动多播监听/广播线程(整个进程一次)。端口被占用或系统不支持多播时静默降级,
/// 不影响 HTTP 服务与浏览器端扫码/手动地址。
/// 用 AtomicBool 而不是 OnceLock: 首次 bind 失败(端口被别的程序占用)时可复位,
/// 下次启动服务时重试, 不需要重启整个 App。
static DISCOVERY_STARTED: AtomicBool = AtomicBool::new(false);
static DISCOVERY_JOINED: AtomicBool = AtomicBool::new(false);
static DISCOVERY_V6_JOINED: AtomicBool = AtomicBool::new(false);

/// 发现不可用时给前端的可读原因: 只展示"多播端口被占用/加入组播失败"这类
/// 用户能动手解决的情况, 不要把底层错误码直接抛给手机页面。
fn discovery_error() -> &'static Mutex<String> {
    static E: OnceLock<Mutex<String>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(String::new()))
}

fn set_discovery_error(msg: impl Into<String>) {
    *discovery_error().lock().unwrap_or_else(|e| e.into_inner()) = msg.into();
}

/// 附近设备接口附带发现诊断: 前端在"设备为空"时据此给出本地网络权限/防火墙指引。
fn discovery_status_json() -> serde_json::Value {
    serde_json::json!({
        "started": DISCOVERY_STARTED.load(Ordering::SeqCst),
        "joined": DISCOVERY_JOINED.load(Ordering::SeqCst),
        "ipv6": DISCOVERY_V6_JOINED.load(Ordering::SeqCst),
        "error": discovery_error().lock().unwrap_or_else(|e| e.into_inner()).clone(),
        "port": discovery_active_port(),
        "portRange": [DISCOVERY_PORT, DISCOVERY_PORT_LAST],
    })
}

/// 本机当前应当广播的发现报文。返回 None = 服务未在局域网监听, 不应对外宣告。
fn announce_packet_bytes() -> Option<Vec<u8>> {
    let port = PORT.load(Ordering::Relaxed);
    // 看"实际绑定"而不是偏好: 用户刚拨开开关但没重启服务时监听仍是 127.0.0.1,
    // 此时广播出去只会让别的设备看到一台连不上的机器。
    if !broadcast_allowed(is_running(), LAN_ACTIVE.load(Ordering::Relaxed), port) {
        return None;
    }
    let busy = active_uploads()
        .lock()
        .map(|m| m.values().any(|v| *v > 0))
        .unwrap_or(false);
    let packet = serde_json::json!({
        "app": DISCOVERY_APP,
        "id": discovery_id(),
        "tok": discovery_token(),
        "name": local_name(),
        "port": port,
        "platform": std::env::consts::OS,
        "busy": busy,
        "dport": discovery_active_port(),
        "ts": now_f64(),
    });
    Some(packet.to_string().into_bytes())
}

/// 主动探测包: 不登记为设备, 只让收到包的对端立即单播回自己的宣告。
fn probe_packet_bytes() -> Option<Vec<u8>> {
    if !broadcast_allowed(is_running(), LAN_ACTIVE.load(Ordering::Relaxed), PORT.load(Ordering::Relaxed)) {
        return None;
    }
    Some(
        serde_json::json!({
            "app": DISCOVERY_APP,
            "t": "probe",
            "id": discovery_id(),
            "dport": discovery_active_port(),
        })
        .to_string()
        .into_bytes(),
    )
}

/// 正常退出时宣告离线; 对端立即移除, 不必等 TTL。
fn bye_packet_bytes() -> Option<Vec<u8>> {
    if !LAN_ACTIVE.load(Ordering::Relaxed) || PORT.load(Ordering::Relaxed) == 0 {
        return None;
    }
    Some(
        serde_json::json!({
            "app": DISCOVERY_APP,
            "t": "bye",
            "id": discovery_id(),
            "tok": discovery_token(),
            "dport": discovery_active_port(),
        })
        .to_string()
        .into_bytes(),
    )
}

fn reply_port(v: &serde_json::Value) -> u16 {
    v.get("dport")
        .and_then(|x| x.as_u64())
        // 只允许回到发现端口范围, 不能被伪造包引去打任意 UDP 端口
        .filter(|p| (DISCOVERY_PORT as u64..=DISCOVERY_PORT_LAST as u64).contains(p))
        .unwrap_or(DISCOVERY_PORT as u64) as u16
}

fn send_discovery_packet(packet: &[u8]) {
    for port in DISCOVERY_PORT..=DISCOVERY_PORT_LAST {
        let per_iface = send_announcement_on_all_interfaces(packet, port);
        let _v6 = send_announcement_on_all_interfaces_v6(packet, port);
        // v4 多播成功的场景仍补一次全局广播, 覆盖 Android/路由器过滤组播的接收端
        if per_iface {
            if let Ok(s) = UdpSocket::bind(("0.0.0.0", 0)) {
                let _ = s.set_broadcast(true);
                let _ = s.send_to(packet, (Ipv4Addr::new(255, 255, 255, 255), port));
            }
        }
    }
}

fn send_discovery_announce() {
    if let Some(packet) = announce_packet_bytes() {
        send_discovery_packet(&packet);
    }
}

fn send_discovery_probe() {
    if let Some(packet) = probe_packet_bytes() {
        send_discovery_packet(&packet);
    }
    send_unicast_probes();
}

fn send_discovery_bye() {
    if let Some(packet) = bye_packet_bytes() {
        send_discovery_packet(&packet);
    }
}

/// 向指定局域网 IP 发单播 probe(手动探测): 多播/广播被交换机或防火墙拦截时,
/// 只要目标放行 UDP 53318-53321 入站, 单播仍可触发对方回宣告。
fn send_probe_to(ip: std::net::IpAddr) -> bool {
    let Some(packet) = probe_packet_bytes() else { return false; };
    match ip {
        std::net::IpAddr::V4(v4) => {
            let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) else { return false };
            for port in DISCOVERY_PORT..=DISCOVERY_PORT_LAST {
                let _ = sock.send_to(&packet, (v4, port));
            }
            true
        }
        std::net::IpAddr::V6(v6) => {
            let Ok(sock) = UdpSocket::bind(("::", 0)) else { return false };
            for port in DISCOVERY_PORT..=DISCOVERY_PORT_LAST {
                let _ = sock.send_to(&packet, (v6, port));
            }
            true
        }
    }
}

/// 已发现 peer 的单播心跳：有些 Android/企业 Wi-Fi 会单向过滤多播/广播，
/// 双方首次能发现，但后续 announce 收不到，设备状态就会在在线/离线间跳。
/// 已知 peer IP 时每 3s 直接单播一份 announce，不再依赖组播通路。
fn send_unicast_announcements() {
    let Some(packet) = announce_packet_bytes() else { return };
    let Ok(v4) = UdpSocket::bind(("0.0.0.0", 0)) else { return };
    let mut v6: Option<UdpSocket> = None;
    for raw in cached_peer_ips() {
        let (host, zone) = raw.split_once('%').unwrap_or((raw.as_str(), ""));
        if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
            if ip.is_loopback() { continue; }
            for port in DISCOVERY_PORT..=DISCOVERY_PORT_LAST {
                let _ = v4.send_to(&packet, (ip, port));
            }
        } else if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
            if ip.is_loopback() { continue; }
            let idx = if zone.is_empty() { 0 } else { iface_index(zone).unwrap_or(0) };
            if ip.is_unicast_link_local() && idx == 0 { continue; }
            if v6.is_none() { v6 = UdpSocket::bind(("::", 0)).ok(); }
            if let Some(sock) = v6.as_ref() {
                for port in DISCOVERY_PORT..=DISCOVERY_PORT_LAST {
                    let dst = std::net::SocketAddr::V6(std::net::SocketAddrV6::new(ip, port, 0, idx));
                    let _ = sock.send_to(&packet, dst);
                }
            }
        }
    }
}

/// 多播/广播都被交换机或系统过滤时的兜底: 向本机所在 IPv4 /24 逐个主机
/// 发 unicast probe。两台新装设备还没有 peer-cache, 只靠多播+广播会互相等不到;
/// 单播探测不依赖组播组，能穿过常见的企业 Wi-Fi 组播过滤。只扫私网/链路本地，
/// 每个网卡最多 1024 个地址，避免误扫公网或大网段。
fn send_subnet_probes() {
    let Some(packet) = probe_packet_bytes() else { return };
    let Ok(ifaces) = if_addrs::get_if_addrs() else { return };
    let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) else { return };
    let mut seen = std::collections::HashSet::new();
    for it in ifaces {
        if discovery_iface_skipped(&it.name) {
            continue;
        }
        let if_addrs::IfAddr::V4(v4) = it.addr else { continue };
        if v4.ip.is_loopback() || !is_lan_peer(std::net::IpAddr::V4(v4.ip)) {
            continue;
        }
        let ip = u32::from(v4.ip);
        let mask = u32::from(v4.netmask);
        if mask == 0 {
            continue;
        }
        let prefix = mask.count_ones();
        // 大网段只扫自己所在 /24；/24 及更小网段按真实 host 范围扫。
        let (start, end) = if prefix < 24 {
            let base = ip & 0xffff_ff00;
            (base + 1, base + 254)
        } else {
            let network = ip & mask;
            let broadcast = network | !mask;
            (network.saturating_add(1), broadcast.saturating_sub(1))
        };
        if end < start {
            continue;
        }
        let count = (end - start + 1).min(1024);
        for off in 0..count {
            let host = Ipv4Addr::from(start.saturating_add(off));
            if host == v4.ip || !seen.insert(host) {
                continue;
            }
            for port in DISCOVERY_PORT..=DISCOVERY_PORT_LAST {
                let _ = sock.send_to(&packet, (host, port));
            }
        }
    }
}

/// 向历史设备地址发单播 probe: 发送到整个发现端口范围, 兼容对方端口顺延。
fn send_unicast_probes() {
    let Some(packet) = probe_packet_bytes() else { return };
    for raw in cached_peer_ips() {
        let (host, zone) = raw.split_once('%').unwrap_or((raw.as_str(), ""));
        if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
            if v4.is_loopback() { continue; }
            if let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) {
                for port in DISCOVERY_PORT..=DISCOVERY_PORT_LAST {
                    let _ = sock.send_to(&packet, (v4, port));
                }
            }
        } else if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
            if v6.is_loopback() { continue; }
            let idx = if zone.is_empty() { 0 } else { iface_index(zone).unwrap_or(0) };
            // link-local 没有 zone 时无法路由, 跳过而不是发一个必然失败的包
            if v6.is_unicast_link_local() && idx == 0 { continue; }
            if let Ok(sock) = UdpSocket::bind(("::", 0)) {
                for port in DISCOVERY_PORT..=DISCOVERY_PORT_LAST {
                    let dst = std::net::SocketAddr::V6(std::net::SocketAddrV6::new(v6, port, 0, idx));
                    let _ = sock.send_to(&packet, dst);
                }
            }
        }
    }
}

/// 发现报文不走的接口: VPN/隧道/虚拟网卡; bridge 热点保留。
fn discovery_iface_skipped(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("utun")
        || name.starts_with("tun")
        || name.starts_with("tap")
        || name.starts_with("ppp")
        || name.starts_with("ipsec")
        || name.starts_with("awdl")
        || name.starts_with("llw")
        || name.starts_with("docker")
        || name.starts_with("vboxnet")
        || name.starts_with("vmnet")
        || name.starts_with("tailscale")
        || name.contains("vpn")
}

/// 从每个真实网卡各发一份: 绑定到该网卡地址并显式设置 IP_MULTICAST_IF。
/// 单靠 0.0.0.0 的默认路由在"Wi-Fi + VPN/虚拟网卡"机器上会把多播/广播发到
/// 错误的接口 —— 表现为桌面端明明开着服务, 却永远发现不了同一 Wi-Fi 的手机。
/// 同时补发子网定向广播(如 10.1.2.255), 某些路由器/Android 过滤组播但放行广播。
fn send_announcement_on_all_interfaces(packet: &[u8], dest_port: u16) -> bool {
    let Ok(ifaces) = if_addrs::get_if_addrs() else { return false };
    let mut sent_iface = false;
    for it in ifaces {
        // 跳过 VPN / 虚拟网卡: 宣告只应留在真实局域网
        if discovery_iface_skipped(&it.name) {
            continue;
        }
        let if_addrs::IfAddr::V4(v4) = it.addr else { continue };
        if v4.ip.is_loopback() { continue; }
        let sk = match socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        ) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = sk.set_broadcast(true);
        let _ = sk.set_multicast_ttl_v4(1);
        // 关键: 显式把多播出口钉在这块网卡上
        let _ = sk.set_multicast_if_v4(&v4.ip);
        let _ = sk.bind(&std::net::SocketAddr::from((v4.ip, 0)).into());
        let group = socket2::SockAddr::from(std::net::SocketAddr::from((DISCOVERY_GROUP, dest_port)));
        if sk.send_to(packet, &group).is_ok() {
            sent_iface = true;
        }
        let bcast = v4.broadcast.unwrap_or_else(|| {
            Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask))
        });
        if bcast != v4.ip {
            let dst = socket2::SockAddr::from(std::net::SocketAddr::from((bcast, dest_port)));
            let _ = sk.send_to(packet, &dst);
        }
    }
    // 没有成功发出任何逐网卡多播(极端环境)时, 退回系统默认路由;
    // 正常情况由调用方只补一次 255.255.255.255 广播, 不再重复发多播。
    if !sent_iface {
        if let Ok(s) = UdpSocket::bind(("0.0.0.0", 0)) {
            let _ = s.set_broadcast(true);
            let _ = s.set_multicast_ttl_v4(1);
            let _ = s.send_to(packet, (DISCOVERY_GROUP, dest_port));
            let _ = s.send_to(packet, (Ipv4Addr::new(255, 255, 255, 255), dest_port));
        }
    }
    sent_iface
}

/// 网卡名 → 系统 interface index: IPv6 多播必须按 index 指定出口。
#[cfg(unix)]
fn iface_index(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    let idx = unsafe { libc::if_nametoindex(c.as_ptr()) };
    (idx > 0).then_some(idx)
}

#[cfg(windows)]
fn iface_index(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    let idx = unsafe {
        windows_sys::Win32::NetworkManagement::IpHelper::if_nametoindex(c.as_ptr() as *const u8)
    };
    (idx > 0).then_some(idx)
}

/// interface index → 网卡名: 链路本地 IPv6 的 URL 必须带 zone(%en0 / %25en0)
#[cfg(unix)]
fn iface_name(index: u32) -> Option<String> {
    // c_char 的平台类型不同: macOS/iOS 是 i8, Android(bionic) 是 u8。
    // 写死 [0i8; 128] 会让 Android 目标编译失败(expected *mut u8, found *mut i8)。
    let mut buf = [0 as libc::c_char; 128];
    let p = unsafe { libc::if_indextoname(index, buf.as_mut_ptr()) };
    if p.is_null() {
        return None;
    }
    let c = unsafe { std::ffi::CStr::from_ptr(p) };
    c.to_str().ok().map(|s| s.to_string())
}

#[cfg(windows)]
fn iface_name(index: u32) -> Option<String> {
    let mut buf = [0u8; 256];
    let p = unsafe {
        windows_sys::Win32::NetworkManagement::IpHelper::if_indextoname(
            index,
            buf.as_mut_ptr() as *mut u8,
        )
    };
    if p.is_null() {
        return None;
    }
    let c = unsafe { std::ffi::CStr::from_ptr(p as *const std::ffi::c_char) };
    c.to_str().ok().map(|s| s.to_string())
}

/// 设备 URL 用的地址: IPv6 link-local 带 `%zone`, 否则浏览器无法路由。
fn source_ip_string(src: std::net::SocketAddr) -> String {
    if let std::net::SocketAddr::V6(v6) = src {
        if v6.ip().is_unicast_link_local() && v6.scope_id() != 0 {
            if let Some(name) = iface_name(v6.scope_id()) {
                return format!("{}%{}", v6.ip(), name);
            }
        }
    }
    src.ip().to_string()
}

/// 从每个 IPv6 网卡各发一份到 link-local 多播组。
fn send_announcement_on_all_interfaces_v6(packet: &[u8], dest_port: u16) -> bool {
    let Ok(ifaces) = if_addrs::get_if_addrs() else { return false };
    let mut sent = false;
    for it in ifaces {
        let if_addrs::IfAddr::V6(v6) = it.addr else { continue };
        if v6.ip.is_loopback() {
            continue;
        }
        // VPN/虚拟网卡与 v4 保持同样的隔离策略
        if discovery_iface_skipped(&it.name) {
            continue;
        }
        let Some(idx) = iface_index(&it.name) else { continue };
        let Ok(sk) = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        ) else {
            continue;
        };
        let _ = sk.set_only_v6(true);
        let _ = sk.set_multicast_if_v6(idx);
        let _ = sk.set_multicast_loop_v6(true);
        if sk.bind(&std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)).into()).is_err() {
            continue;
        }
        let dst = std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
            DISCOVERY_GROUP_V6,
            dest_port,
            0,
            idx,
        ));
        if sk.send_to(packet, &socket2::SockAddr::from(dst)).is_ok() {
            sent = true;
        }
    }
    sent
}

/// IPv6 监听: 与 v4 同端口但 v6only, 加入 link-local 自定义多播组。
/// 没有 IPv6 / 端口被占用时静默降级, 不影响 v4 发现。
fn ensure_discovery_v6(port: u16) {
    let sk = match socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    ) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = sk.set_only_v6(true);
    let _ = sk.set_reuse_address(true);
    let _ = sk.set_multicast_loop_v6(true);
    if sk
        .bind(&std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())
        .is_err()
    {
        return;
    }
    let sock: UdpSocket = sk.into();
    let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));
    let mut joined = false;
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for it in ifaces {
            let if_addrs::IfAddr::V6(v6) = it.addr else { continue };
            if v6.ip.is_loopback() || discovery_iface_skipped(&it.name) {
                continue;
            }
            if let Some(idx) = iface_index(&it.name) {
                if sock.join_multicast_v6(&DISCOVERY_GROUP_V6, idx).is_ok() {
                    joined = true;
                }
            }
        }
    }
    DISCOVERY_V6_JOINED.store(joined, Ordering::SeqCst);
    let listener = match sock.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let reply_sock = sock.try_clone().ok();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        let mut last_rescan = std::time::Instant::now();
        let mut last_reply: HashMap<std::net::IpAddr, std::time::Instant> = HashMap::new();
        let mut last_probe_reply: HashMap<std::net::IpAddr, std::time::Instant> = HashMap::new();
        loop {
            if last_rescan.elapsed() >= Duration::from_secs(30) {
                last_rescan = std::time::Instant::now();
                let mut any = false;
                if let Ok(ifs) = if_addrs::get_if_addrs() {
                    for it in ifs {
                        let if_addrs::IfAddr::V6(v6) = it.addr else { continue };
                        if v6.ip.is_loopback() || discovery_iface_skipped(&it.name) {
                            continue;
                        }
                        if let Some(idx) = iface_index(&it.name) {
                            if listener.join_multicast_v6(&DISCOVERY_GROUP_V6, idx).is_ok() {
                                any = true;
                            }
                        }
                    }
                }
                if any {
                    DISCOVERY_V6_JOINED.store(true, Ordering::SeqCst);
                }
            }
            let Ok((n, src)) = listener.recv_from(&mut buf) else { continue };
            if src.ip().is_loopback() || !is_running() || !discovery_packet_admit(src.ip()) {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) else { continue };
            let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("announce");
            if t == "probe" {
                if is_lan_peer(src.ip())
                    && v.get("app").and_then(|x| x.as_str()) == Some(DISCOVERY_APP)
                    && !is_self_discovery_packet(v.get("id").and_then(|x| x.as_str()), src.ip())
                {
                    let due = last_probe_reply
                        .get(&src.ip())
                        .map(|at| at.elapsed() >= Duration::from_millis(500))
                        .unwrap_or(true);
                    if due {
                        if let (Some(s), Some(pkt)) = (reply_sock.as_ref(), announce_packet_bytes()) {
                            // 必须保留 src 的 scope id: fe80:: 目标没有 scope 时 send 直接失败
                            let mut dst = src;
                            dst.set_port(reply_port(&v));
                            let _ = s.send_to(&pkt, dst);
                            last_probe_reply.insert(src.ip(), std::time::Instant::now());
                            if last_probe_reply.len() > 128 {
                                last_probe_reply.clear();
                            }
                        }
                    }
                }
                continue;
            }
            if t == "bye" {
                let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
                if is_lan_peer(src.ip()) && !id.is_empty() && !is_self_discovery_packet(Some(id), src.ip()) {
                    let mut map = multicast_peers().lock().unwrap_or_else(|e| e.into_inner());
                    let tok = v.get("tok").and_then(|x| x.as_str()).unwrap_or("").trim();
                    let same = map.get(id).map(|d| {
                        d.ip == source_ip_string(src) && (d.token.is_empty() || d.token == tok)
                    }).unwrap_or(false);
                    if same && map.remove(id).is_some() {
                        drop(map);
                        broadcast_event("devices:changed");
                        broadcast_devices_presence();
                    }
                }
                continue;
            }
            let res = ingest_peer_packet_ex(&v, src);
            if res.new_device || res.changed {
                broadcast_event("devices:changed");
            }
            // 新设备/字段变化立即推；普通 valid 心跳进入 1s 合并器，尾包必达。
            if res.new_device || res.changed {
                broadcast_devices_presence();
            } else if res.heard {
                schedule_devices_presence();
            }
            if !is_lan_peer(src.ip()) {
                continue;
            }
            let due = last_reply
                .get(&src.ip())
                .map(|at| at.elapsed() >= Duration::from_secs(15))
                .unwrap_or(true);
            if due {
                if let (Some(s), Some(pkt)) = (reply_sock.as_ref(), announce_packet_bytes()) {
                    let mut dst = src;
                    dst.set_port(reply_port(&v));
                    let _ = s.send_to(&pkt, dst);
                    last_reply.insert(src.ip(), std::time::Instant::now());
                    if last_reply.len() > 128 {
                        last_reply.clear();
                    }
                }
            }
        }
    });
}

fn ensure_discovery() {
    if DISCOVERY_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // 端口顺延: 53318 被占用时依次尝试 53319..53321; 每个实例只绑一个接收端口,
    // 发送端向整个范围发, 因此同机多实例/端口冲突时仍能互相发现。
    let mut picked: Option<(UdpSocket, u16)> = None;
    let mut last_err = String::new();
    for port in DISCOVERY_PORT..=DISCOVERY_PORT_LAST {
        match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(s) => {
                picked = Some((s, port));
                break;
            }
            Err(e) => last_err = format!("{e}"),
        }
    }
    let Some((sock, active_port)) = picked else {
        DISCOVERY_STARTED.store(false, Ordering::SeqCst);
        set_discovery_error(format!(
            "发现端口 {DISCOVERY_PORT}-{DISCOVERY_PORT_LAST} 均被占用或不可用: {last_err}"
        ));
        push_log_warn(format!(
            "局域网多播发现不可用(端口 {DISCOVERY_PORT}-{DISCOVERY_PORT_LAST}): {last_err}"
        ));
        return;
    };
    DISCOVERY_ACTIVE_PORT.store(active_port, Ordering::Relaxed);
    push_log(format!("设备发现监听 UDP {active_port}（可用顺延范围 {DISCOVERY_PORT}-{DISCOVERY_PORT_LAST}）"));
    ensure_discovery_v6(active_port);
    {
        let _ = sock.set_multicast_loop_v4(true);   // 同机多实例测试; 自身包会按 id 忽略
        let _ = sock.set_multicast_ttl_v4(1);       // 只留在局域网
        let _ = sock.set_broadcast(true);           // Android 过滤组播时的广播兜底
        let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));

        let mut joined = false;
        if let Ok(ifaces) = if_addrs::get_if_addrs() {
            for it in ifaces {
                if let if_addrs::IfAddr::V4(v4) = it.addr {
                    if v4.ip.is_loopback() { continue; }
                    if sock.join_multicast_v4(&DISCOVERY_GROUP, &v4.ip).is_ok() {
                        joined = true;
                    }
                }
            }
        }
        if !joined {
            joined = sock.join_multicast_v4(&DISCOVERY_GROUP, &Ipv4Addr::UNSPECIFIED).is_ok();
        }
        DISCOVERY_JOINED.store(joined, Ordering::SeqCst);
        if !joined {
            set_discovery_error("加入局域网多播组失败，其他 CC Flash 设备可能无法自动出现");
            push_log_warn("局域网多播发现加入组播组失败，其他 CC Flash 设备可能无法自动出现".into());
        }

        let listener = match sock.try_clone() {
            Ok(s) => s,
            Err(e) => {
                DISCOVERY_STARTED.store(false, Ordering::SeqCst);
                set_discovery_error(format!("多播监听初始化失败: {e}"));
                push_log_warn(format!("局域网多播监听初始化失败: {e}"));
                return;
            }
        };
        // 单播回包用的发送端: 收到谁的报文就回谁一份自己的宣告。
        // 只靠多播时, 某些 AP 隔离/防火墙存在"对方能收到我、我收不到对方"的单向
        // 通路; 单播回包让双方都能在 5s 内补齐设备表。
        let reply_sock = sock.try_clone().ok();

        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let mut last_rescan = std::time::Instant::now();
            // 每个来源的单播回包节流: 同一设备最多 15s 回一次, 避免 N 台设备
            // 互发宣告形成无谓的 N² 流量。
            let mut last_reply: HashMap<std::net::IpAddr, std::time::Instant> = HashMap::new();
            let mut last_probe_reply: HashMap<std::net::IpAddr, std::time::Instant> = HashMap::new();
            loop {
                // 切 Wi-Fi / 插拔网卡后, 新接口需要重新 join 组播组, 否则只能发广播、
                // 收不到别人(设备列表 30s 后清空, 直到重启 App)。每 30s 幂等 rejoin。
                if last_rescan.elapsed() >= Duration::from_secs(30) {
                    last_rescan = std::time::Instant::now();
                    let mut any = false;
                    if let Ok(ifs) = if_addrs::get_if_addrs() {
                        for it in ifs {
                            if let if_addrs::IfAddr::V4(v4) = it.addr {
                                if !v4.ip.is_loopback()
                                    && listener.join_multicast_v4(&DISCOVERY_GROUP, &v4.ip).is_ok()
                                {
                                    any = true;
                                }
                            }
                        }
                    }
                    if any {
                        DISCOVERY_JOINED.store(true, Ordering::SeqCst);
                        set_discovery_error("");
                    }
                }
                let (n, src) = match listener.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if src.ip().is_loopback() { continue; }
                // 服务已停止时不再刷新设备表(广播线程同样会停)
                if !is_running() || !discovery_packet_admit(src.ip()) {
                    continue;
                }
                let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) else { continue };
                let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("announce");
                // 主动探测: 立即单播回自己的宣告(1s/来源限流), 不再等 15s 常规回包节流
                if t == "probe" {
                    if is_lan_peer(src.ip())
                        && v.get("app").and_then(|x| x.as_str()) == Some(DISCOVERY_APP)
                        && !is_self_discovery_packet(v.get("id").and_then(|x| x.as_str()), src.ip())
                    {
                        let due = last_probe_reply
                            .get(&src.ip())
                            .map(|at| at.elapsed() >= Duration::from_millis(500))
                            .unwrap_or(true);
                        if due {
                            if let (Some(s), Some(pkt)) = (reply_sock.as_ref(), announce_packet_bytes()) {
                                let mut dst = src;
                                dst.set_port(reply_port(&v));
                                let _ = s.send_to(&pkt, dst);
                                last_probe_reply.insert(src.ip(), std::time::Instant::now());
                                if last_probe_reply.len() > 128 {
                                    last_probe_reply.clear();
                                }
                            }
                        }
                    }
                    continue;
                }
                // 正常退出: 同 IP 同 id 才能移除, 防止伪造 bye 清掉别人
                if t == "bye" {
                    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
                    if is_lan_peer(src.ip()) && !id.is_empty() && !is_self_discovery_packet(Some(id), src.ip()) {
                        let mut map = multicast_peers().lock().unwrap_or_else(|e| e.into_inner());
                        let tok = v.get("tok").and_then(|x| x.as_str()).unwrap_or("").trim();
                        let same = map.get(id).map(|d| {
                            d.ip == source_ip_string(src) && (d.token.is_empty() || d.token == tok)
                        }).unwrap_or(false);
                        if same && map.remove(id).is_some() {
                            drop(map);
                            broadcast_event("devices:changed");
                            broadcast_devices_presence();
                        }
                    }
                    continue;
                }
                let res = ingest_peer_packet_ex(&v, src);
                if res.new_device || res.changed {
                    broadcast_event("devices:changed");
                }
                if res.new_device || res.changed {
                    broadcast_devices_presence();
                } else if res.heard {
                    schedule_devices_presence();
                }
                // 单播回包只发给局域网来源: 公网伪造包不回, 避免把本机信息发到外网。
                if !is_lan_peer(src.ip()) { continue; }
                // 单播回包: 新增来源立即回, 已知来源 15s 一次。reply_sock 绑定在
                // 0.0.0.0:DISCOVERY_PORT, 对端按来源端口 53318 接收。
                let due = last_reply
                    .get(&src.ip())
                    .map(|t| t.elapsed() >= Duration::from_secs(15))
                    .unwrap_or(true);
                if due {
                    if let (Some(s), Some(pkt)) = (reply_sock.as_ref(), announce_packet_bytes()) {
                        let _ = s.send_to(&pkt, (src.ip(), reply_port(&v)));
                        last_reply.insert(src.ip(), std::time::Instant::now());
                        if last_reply.len() > 128 {
                            last_reply.clear();   // 防止大量伪造源地址把表撑大
                        }
                    }
                }
            }
        });

        std::thread::spawn(move || {
            let mut last_unicast = std::time::Instant::now();
            loop {
            // 移动端默认安全模式(仅 127.0.0.1): 不广播本机 —— 否则其他设备会在
            // 列表里看到手机、点进去却连不上。用户开启局域网访问并重启服务后才广播。
            if let Some(bytes) = announce_packet_bytes() {
                for port in DISCOVERY_PORT..=DISCOVERY_PORT_LAST {
                    // 逐网卡显式指定出口, 避免 Wi-Fi + VPN/虚拟网卡时走错接口
                    let per_iface = send_announcement_on_all_interfaces(&bytes, port);
                    if !per_iface {
                        let _ = sock.send_to(&bytes, (DISCOVERY_GROUP, port));
                    }
                    // 广播兜底: 部分 Android 网络默认过滤组播, 广播在多数家用路由器仍可达
                    let _ = sock.send_to(&bytes, (Ipv4Addr::new(255, 255, 255, 255), port));
                }
            }
            // 已知 peer 直接单播心跳：绕过 Android/企业 Wi-Fi 的单向组播过滤。
            send_unicast_announcements();
            // 即使没有新报文，也每 3s 推一次权威在线态：保证设备静默 12s 后，
            // 前端不依赖 HTTP 轮询也能准时从 online 切成 offline。
            broadcast_devices_presence();
            // 单播 probe 兼容旧版对端：新 peer 直接收 announce，旧 peer 只有
            // probe 回包才会主动回 announce；5s 一次确保 last_seen 不会被 12s 宽限判离线。
            if last_unicast.elapsed() >= Duration::from_secs(5) {
                last_unicast = std::time::Instant::now();
                send_unicast_probes();
            }
            std::thread::sleep(Duration::from_secs(3));
            }
        });
    }
}

async fn device_hello(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    Json(body): Json<HelloIn>,
) -> Response {
    let ip = normalize_peer(addr.ip());
    let nickname = body
        .nickname
        .unwrap_or_default()
        .trim()
        .chars()
        .filter(|c| *c != '\0')
        .take(NICK_MAX_CHARS)
        .collect::<String>();
    let nickname = if nickname.is_empty() { "未命名设备".into() } else { nickname };
    // 只有本机页面(回环)的昵称用于 UDP 广播; 远程浏览器没有自己的监听端口,
    // 不再登记为"可发送目标"(旧实现把 ip:本服务端口 当成对方地址, 点击必然失败)。
    if ip.is_loopback() {
        set_local_name(&nickname);
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn device_list() -> Response {
    let now = now_f64();
    Json(serde_json::json!({
        "devices": device_list_json(now),
        "discovery": discovery_status_json(),
    }))
    .into_response()
}

/// 单来源发现包准入: 正常设备每 5s 一包, 50 包/秒足够; 挡住 UDP 洪泛占锁/刷列表。
const DISCOVERY_PACKETS_PER_SEC: u32 = 50;
fn discovery_packet_rate() -> &'static Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static RATE: OnceLock<Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    RATE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn discovery_packet_admit(peer: std::net::IpAddr) -> bool {
    let now = std::time::Instant::now();
    let mut m = discovery_packet_rate().lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 256 {
        m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 2);
    }
    let e = m.entry(rate_key(peer)).or_insert((0, now));
    if now.duration_since(e.1).as_millis() >= 1000 {
        *e = (0, now);
    }
    if e.0 >= DISCOVERY_PACKETS_PER_SEC {
        return false;
    }
    e.0 = e.0.saturating_add(1);
    true
}

/// 限流键: IPv6 按 /64 聚合 —— 同一网段内轮换接口地址(隐私扩展)不能刷新配额。
/// v4(mapped 已折回)无轮换诉求, 仍按单地址计。
fn rate_key(peer: std::net::IpAddr) -> std::net::IpAddr {
    pin_prefix6(peer).unwrap_or(peer)
}

const DISCOVERY_PROBE_PER_MIN: u32 = 30;
fn discovery_probe_rate() -> &'static Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static RATE: OnceLock<Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    RATE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn discovery_probe_allow(peer: std::net::IpAddr) -> bool {
    let now = std::time::Instant::now();
    let mut m = discovery_probe_rate().lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 256 {
        m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 60);
    }
    let e = m.entry(rate_key(peer)).or_insert((0, now));
    // 固定窗口: e.1 是窗口起点, 只有窗口过期才重置。旧实现每次成功都刷新 e.1,
    // 结果不是"N 次/分钟", 而是"突发 N 次后整窗封禁"(额度耗尽后要等最后一次成功满 60s)。
    if now.duration_since(e.1).as_secs() >= 60 {
        *e = (0, now);
    }
    if e.0 >= DISCOVERY_PROBE_PER_MIN {
        return false;
    }
    e.0 = e.0.saturating_add(1);
    true
}

#[derive(serde::Deserialize)]
struct ProbeIn {
    ip: Option<String>,
    /// 仅用户主动点击「扫描」时允许扫本机 /24；WS 重连/自动探测不扫。
    scan: Option<bool>,
}

async fn discovery_probe(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    body: Option<Json<ProbeIn>>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    if !discovery_probe_allow(peer) {
        return err(StatusCode::TOO_MANY_REQUESTS, "扫描过于频繁，请稍后再试");
    }
    // 手动探测: 指定局域网 IP 时只向该地址发单播 probe, 用于绕过交换机/防火墙
    // 对多播与广播的过滤; 不填则沿用全局多播+广播+历史地址探测。
    let body = body.map(|Json(b)| b);
    if let Some(raw) = body.as_ref().and_then(|b| b.ip.clone()) {
        let Ok(ip) = raw.trim().parse::<std::net::IpAddr>() else {
            return err(StatusCode::BAD_REQUEST, "IP 格式不正确");
        };
        if !is_lan_peer(ip) {
            return err(StatusCode::BAD_REQUEST, "只支持局域网 IP");
        }
        let sent = tokio::task::spawn_blocking(move || send_probe_to(ip))
            .await
            .unwrap_or(false);
        if !sent {
            return err(StatusCode::SERVICE_UNAVAILABLE, "探测包发送失败，请检查服务是否运行");
        }
        push_log(format!("手动单播探测 {ip} ← {peer}"));
        return Json(serde_json::json!({
            "ok": true, "port": discovery_active_port(), "target": ip.to_string()
        }))
        .into_response();
    }
    // 自动探测只走多播/广播/缓存 peer；/24 全扫仅由用户显式 scan:true 触发。
    tokio::task::spawn_blocking(send_discovery_probe)
        .await
        .ok();
    if body.as_ref().and_then(|b| b.scan).unwrap_or(false) {
        tokio::task::spawn_blocking(send_subnet_probes).await.ok();
    }
    Json(serde_json::json!({ "ok": true, "port": discovery_active_port() })).into_response()
}

/// 文件 → 上传者昵称(相对路径为键)。内存态: 服务重启后丢失, 列表回落为通用头像。
/// 用途: 其他设备在时间线里看到的文件卡片能显示上传者头像, 与聊天消息一致。
fn uploaders() -> &'static Mutex<HashMap<String, String>> {
    static UP: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    UP.get_or_init(|| Mutex::new(HashMap::new()))
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
    // 给 warn 预留 200 条: 任意设备可循环上传/下载/制造失败把普通日志刷满,
    // 预留区保证 PIN 锁定等安全事件不会被挤出环形队列(审计防洪水)。
    const WARN_RESERVE: usize = 200;
    let entry = LogEntry {
        ts: now_ts(),
        level: level.to_string(),
        loc: format!("{file}:{}", at.line()),
        msg: clean,
    };
    if level != "warn" {
        let routine = q.iter().filter(|e| e.level != "warn").count();
        if routine >= REQ_LOG_MAX.saturating_sub(WARN_RESERVE) {
            if let Some(pos) = q.iter().position(|e| e.level != "warn") {
                q.remove(pos);
            }
        }
    }
    q.push_back(entry);
    while q.len() > REQ_LOG_MAX {
        // 普通日志先淘汰; 只有全是 warn 时才动 warn
        if let Some(pos) = q.iter().position(|e| e.level != "warn") {
            q.remove(pos);
        } else {
            q.pop_front();
        }
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

/// 审计日志准入: 每个来源 IPv4/IPv6-/64 每分钟最多记 60 条失败请求。
/// 未认证的局域网设备可以用 2000+ 次 404 把 PIN 锁定、删除、上传完成等安全事件
/// 挤出环形队列(审计规避); 限流后只保留有代表性的失败行。安全事件(锁定告警等)
/// 走 push_log_warn 直接入队, 不经此准入。
const LOG_FAILS_PER_MIN: u32 = 60;
fn log_rate() -> &'static Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static RATE: OnceLock<Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    RATE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn log_admit(peer: std::net::IpAddr) -> bool {
    let now = std::time::Instant::now();
    let mut m = log_rate().lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 1024 {
        m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 60);
    }
    let e = m.entry(rate_key(peer)).or_insert((0, now));
    if now.duration_since(e.1).as_secs() >= 60 {
        *e = (0, now);
    }
    if e.0 >= LOG_FAILS_PER_MIN {
        return false;
    }
    e.0 = e.0.saturating_add(1);
    true
}

// ---- WebSocket 事件总线: 聊天/列表/状态变化即时推送, 前端轮询降级为兜底 ---- //

fn event_bus() -> &'static broadcast::Sender<String> {
    static TX: OnceLock<broadcast::Sender<String>> = OnceLock::new();
    TX.get_or_init(|| broadcast::channel(64).0)
}

/// WS 连接数上限: 每个连接占一个任务与缓冲, 局域网工具不需要数百条
fn ws_permits() -> &'static tokio::sync::Semaphore {
    static S: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    S.get_or_init(|| tokio::sync::Semaphore::new(64))
}

/// 上传并发上限: 每个在途上传持有成品 + marker 两个 fd, 无上限可被空闲连接耗尽
fn upload_permits() -> &'static tokio::sync::Semaphore {
    static S: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    S.get_or_init(|| tokio::sync::Semaphore::new(16))
}

/// 下载并发上限: 与上传/WS 对齐。每条下载固定持有约 CHUNK_HINT 的缓冲,
/// 不设闸门时局域网任意设备可并发数百条把内存与 fd 同时耗尽。
fn download_permits() -> &'static tokio::sync::Semaphore {
    static S: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    S.get_or_init(|| tokio::sync::Semaphore::new(32))
}

/// 目录列表并发闸: 遍历在 spawn_blocking 上跑, 无限制时空目录树可耗尽阻塞线程池。
fn files_permits() -> &'static tokio::sync::Semaphore {
    static S: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    S.get_or_init(|| tokio::sync::Semaphore::new(2))
}

/// 单来源并发槽位(IPv6 按 /64 聚合)。全局闸之外再做一层公平性限制:
/// 否则一台设备就能占满 16 个上传/32 个下载许可, 把所有其他设备挡在 429 外。
#[derive(Clone, Copy)]
enum PeerSlotKind { Upload, Download }
fn peer_slots(kind: PeerSlotKind) -> &'static Mutex<HashMap<std::net::IpAddr, usize>> {
    static UP: OnceLock<Mutex<HashMap<std::net::IpAddr, usize>>> = OnceLock::new();
    static DOWN: OnceLock<Mutex<HashMap<std::net::IpAddr, usize>>> = OnceLock::new();
    match kind {
        PeerSlotKind::Upload => UP.get_or_init(|| Mutex::new(HashMap::new())),
        PeerSlotKind::Download => DOWN.get_or_init(|| Mutex::new(HashMap::new())),
    }
}
fn peer_slot_try_acquire(peer: std::net::IpAddr, max: usize, kind: PeerSlotKind) -> Option<PeerSlot> {
    let key = rate_key(peer);
    let mut m = peer_slots(kind).lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 1024 {
        m.retain(|_, n| *n > 0);
    }
    let n = m.entry(key).or_insert(0);
    if *n >= max {
        return None;
    }
    *n += 1;
    Some(PeerSlot { key, kind })
}
/// 下载流的存活标记: 流被 drop(正常读完/客户端断开/看门狗 abort)时置位,
/// 让看门狗任务及时退出, 不必等到 45s 静默窗口。
struct DownloadAlive {
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}
impl Drop for DownloadAlive {
    fn drop(&mut self) {
        self.alive.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
}

struct PeerSlot {
    key: std::net::IpAddr,
    kind: PeerSlotKind,
}
impl Drop for PeerSlot {
    fn drop(&mut self) {
        let mut m = peer_slots(self.kind).lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.key);
            }
        }
    }
}

/// 广播事件给所有已连接的 WebSocket 客户端(失败静默: 无人在线属正常)
fn broadcast_event(kind: &str) {
    let _ = event_bus().send(format!("{{\"type\":\"{kind}\"}}"));
}

/// 本地时间 YYYY-MM-DD HH:MM:SS(精确到秒; 耗时类信息在消息体里自带毫秒)。
/// 不引入 chrono: 日志只用到本地时/分/秒, libc/GetLocalTime 足够, 可减小二进制。
fn now_ts() -> String {
    #[cfg(unix)]
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // localtime_r 是线程安全的; 失败时退回 UTC 秒级格式化
        if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
            return format!("{now}");
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec
        )
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::SYSTEMTIME;
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { GetLocalTime(&mut st) };
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("{secs}")
    }
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
/// 本次服务实际监听在局域网地址(而非仅 127.0.0.1)。与偏好 open_lan() 不同:
/// 用户拨动开关后要重启服务才生效, 二维码/分享/广播必须看"实际"而不是"意图"。
static LAN_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 共享目录选择持久化文件(家目录隐藏文件, 内容为一行路径)
fn share_pref_file() -> PathBuf {
    // Android: $HOME 无效, 偏好文件放 app 专属目录(setup 已注入), 否则手动选择的
    // 共享目录写入静默失败、重启回落默认
    #[cfg(any(target_os = "android", target_os = "ios"))]
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
            // 旧偏好可能含 .. / 符号链接: 回载时规范化, 后续文件操作与高风险判断一致
            return std::fs::canonicalize(saved).unwrap_or_else(|_| PathBuf::from(saved));
        }
    }
    // Android 应用进程 $HOME 无效(落到 "/" 或相对路径 → EACCES, 服务启动即失败):
    // 由 lib.rs setup 注入 app 专属目录(父); 共享目录为其下 CC-Flash 子目录,
    // 偏好文件平级置放(不进共享目录内部, 否则会被 HTTP 列表当普通文件展示)
    #[cfg(any(target_os = "android", target_os = "ios"))]
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
        match std::fs::rename(&old, &new) {
            Ok(()) => push_log(format!("已将共享目录 {} 迁移为 {}", old.display(), new.display())),
            Err(e) => {
                // 迁移失败绝不能静默用新空目录: 用户升级后会看到"共享文件全没了"。
                // 直接继续使用旧目录, 文件仍在列表里, 下次启动再尝试迁移。
                push_log_warn(format!(
                    "共享目录迁移失败({e})，继续使用旧目录 {}；请关闭占用该目录的程序后重启",
                    old.display()
                ));
                return old;
            }
        }
    }
    new
}

/// 移动端专属: setup 阶段注入的 app 专属目录(Android/iOS 的 $HOME 不可用)。
/// 桌面端无此机制, default_share_dir 走 $HOME 分支。
#[cfg(any(target_os = "android", target_os = "ios"))]
static PLATFORM_DEFAULT_DIR: OnceLock<String> = OnceLock::new();
#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn set_platform_default_dir(dir: String) {
    let _ = PLATFORM_DEFAULT_DIR.set(dir);
}
#[cfg(any(target_os = "android", target_os = "ios"))]
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
    // 原子写: 掉电落在 truncate 与写完之间会留下空文件 → 下次启动回落默认目录,
    // 用户会以为共享文件全部消失
    if let Err(e) = write_secret_atomic(&share_pref_file(), p.to_string_lossy().as_bytes()) {
        push_log_warn(format!("共享目录偏好保存失败(重启后可能回落默认目录): {e}"));
    }
    // 换目录后旧票据按同名可能解析到新目录的文件, 必须全部作废
    clear_share_tickets();
}


pub fn is_running() -> bool { running_slot().lock().unwrap_or_else(|e| e.into_inner()).is_some() }

/// 调系统默认程序打开文件/目录/URL(macOS: open / Windows: start / Linux: xdg-open)
pub fn open_with_system(target: &str) -> Result<(), String> {
    // iOS 沙箱禁止 spawn 外部进程: 旧 cfg 会落到 xdg-open 分支必然失败。
    // 明确报错, 前端据此走"复制链接, 请在浏览器中打开"的兜底路径
    #[cfg(target_os = "ios")]
    {
        let _ = target;
        return Err("iOS 壳不支持直接调起浏览器，请复制链接后在浏览器中打开".into());
    }
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
        let is_url = target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("ms-settings:");
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
    #[cfg(target_os = "android")]
    {
        // am 是短命启动器: 同步拿退出码, 失败时如实报错而不是"假成功"
        let st = cmd.status().map_err(|e| e.to_string())?;
        if !st.success() {
            return Err("系统浏览器启动失败，请复制链接后手动打开".into());
        }
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    {
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        // open/xdg-open 都是短命启动器: 立即后台 wait 回收, 否则每次打开文件
        // 都在托盘常驻进程下留一个僵尸子进程
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// 打开本地文件/目录(路径必须存在)
/// Windows canonicalize 返回 `\\?\` verbatim 形态, explorer 对它的兼容性差:
/// 只在交给系统程序/展示时剥掉, 内部安全校验仍用 canonical 路径。
fn strip_verbatim(path: &str) -> String {
    #[cfg(windows)]
    {
        if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = path.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
    }
    path.to_string()
}

pub fn open_path(path: &str) -> Result<(), String> {
    if !Path::new(path).exists() {
        return Err(format!("路径不存在: {path}"));
    }
    open_with_system(&strip_verbatim(path))
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
    // 隧道/虚拟网卡: 前缀 + 包含式关键词双保险。Windows 的适配器名是友好名
    // ("VirtualBox Host-Only Network"/"WireGuard Tunnel"/"ZeroTier One"), 只按前缀会漏,
    // 一旦枚举顺序靠前就会把二维码指向手机不可达的虚拟地址。
    const TUNNEL_PREFIXES: &[&str] = &[
        "utun", "tun", "tap", "wg", "ppp", "vethernet", "docker", "vmnet", "virbr",
        "tailscale", "zerotier", "zt", "wireguard", "vboxnet", "ham", "br-", "cni",
        "flannel", "kube", "orbstack", "colima", "lima", "multipass",
    ];
    const VIRTUAL_KEYWORDS: &[&str] = &[
        "virtualbox", "vmware", "hyper-v", "tailscale", "zerotier", "wireguard",
        "openvpn", "hamachi", "wsl", "docker", "veth", "virtual adapter",
    ];
    let is_tunnel = |name: &str| {
        let n = name.to_ascii_lowercase();
        TUNNEL_PREFIXES.iter().any(|p| n.starts_with(p))
            || VIRTUAL_KEYWORDS.iter().any(|k| n.contains(k))
    };
    // bridge*(macOS 互联网共享/热点)不硬过滤: 手机直连 Mac 热点时唯一可达地址就是它,
    // 但普通场景它只是冗余 —— 降为末位候选(真实网卡/探测都失效时才取)
    let is_bridge = |name: &str| name.to_ascii_lowercase().starts_with("bridge");
    let is_cgnat = |ip: &std::net::Ipv4Addr| {
        // 100.64.0.0/10: 运营商级 NAT / Tailscale 等, 不作为首选局域网地址
        let o = ip.octets();
        o[0] == 100 && (o[1] & 0xc0) == 0x40
    };
    let mut private: Vec<String> = Vec::new();
    let mut cgnat: Vec<String> = Vec::new();
    let mut other: Vec<String> = Vec::new();
    let mut linklocal: Vec<String> = Vec::new();
    let mut v6_ula: Vec<String> = Vec::new();
    let mut v6_global: Vec<String> = Vec::new();
    let mut bridge_last: Option<String> = None;
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if is_tunnel(&iface.name) {
                continue;
            }
            match iface.addr {
                if_addrs::IfAddr::V4(v4) => {
                    if v4.ip.is_loopback() {
                        continue;
                    }
                    let s = v4.ip.to_string();
                    if is_bridge(&iface.name) {
                        if bridge_last.is_none() {
                            bridge_last = Some(s);
                        }
                    } else if v4.ip.is_link_local() {
                        // 169.254/16: 只有在没有更好地址时才用(直连网线等)
                        if !linklocal.contains(&s) { linklocal.push(s); }
                    } else if v4.ip.is_private() {
                        if !private.contains(&s) { private.push(s); }
                    } else if is_cgnat(&v4.ip) {
                        if !cgnat.contains(&s) { cgnat.push(s); }
                    } else if !other.contains(&s) {
                        other.push(s);
                    }
                }
                if_addrs::IfAddr::V6(v6) => {
                    // 链路本地(fe80::/10)需要 %网卡 作用域, 跨设备 URL 不可达, 跳过;
                    // 优先 ULA(fc00::/7), 其次全局 IPv6。IPv6-only 网络至少能出可打开的二维码。
                    let ip = v6.ip;
                    if ip.is_loopback() || (ip.segments()[0] & 0xffc0) == 0xfe80 {
                        continue;
                    }
                    let s = ip.to_string();
                    if (ip.segments()[0] & 0xfe00) == 0xfc00 {
                        if !v6_ula.contains(&s) { v6_ula.push(s); }
                    } else if !v6_global.contains(&s) {
                        v6_global.push(s);
                    }
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
    if let Some(first) = cgnat.first() {
        return first.clone();
    }
    if let Some(first) = other.first() {
        return first.clone();
    }
    if let Some(b) = bridge_last {
        return b; // macOS 互联网共享场景: 手机直连热点时唯一可达地址
    }
    if let Some(first) = linklocal.first() {
        return first.clone();
    }
    if let Some(first) = v6_ula.first() {
        return first.clone();
    }
    if let Some(first) = v6_global.first() {
        return first.clone();
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
    #[cfg(any(target_os = "android", target_os = "ios"))]
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
    // 等在途 stop 收尾再拿锁(stop 入口先置 STOPPING 再抢同一把锁, 顺序天然互斥),
    // 拿到锁后复查一次 —— 关闭"检查通过后 stop 才开始"的残余空档, 地址不再漂移
    let _guard = loop {
        let stop_deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(3500);
        while STOPPING.load(Ordering::SeqCst) && std::time::Instant::now() < stop_deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let g = lifecycle_lock().lock().unwrap_or_else(|e| e.into_inner());
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
    if is_dangerous_share_dir(&share) {
        push_log_warn("警告: 当前共享目录是家目录/其上级或磁盘根，整个目录会对局域网开放".to_string());
    }
    tauri::async_runtime::block_on(async move {
        // 启动清理必须与上传的"占位→标记→登记"共用 file_op_lock:
        // 否则一个正在等待锁的旧 handler 会在清理之后才创建文件, 而 stop() 可能
        // 已把 STOPPING 复位; 紧接着的清理会删掉它正在写的成品(Unix 上 unlink
        // 仍打开的 inode, handler 继续写完并回 200, 文件却已消失)。
        {
            let _op = file_op_lock().lock().await;
            // 顺序敏感: 必须先清硬中断残留 —— 若先跑 clean_stale_parts(7天TTL按
            // is_part_artifact 删标记), 老标记被删而半截成品留下, 后者再也识别不出来。
            // 仍有在途上传时跳过破坏性清理: stop() 的 3s 等待不是硬保证。
            if active_uploads().lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                clean_incomplete_uploads(&share); // 标记在 → 成品必是半截(启动时无在途上传)
            } else {
                push_log_warn("仍有在途上传，本次启动跳过残留清理".into());
            }
            clean_stale_parts(&share); // 再清理过期续传断点(期内保留, 支持重启后续传)
        }
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
        // 本次启动的监听意图固定下来: 绑定期间用户拨动开关只影响下次启动。
        let want_lan = open_lan();
        for p in candidates {
            if !tried.insert(p) { continue; }
            if want_lan {
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
        LAN_ACTIVE.store(want_lan, Ordering::SeqCst);
        ensure_discovery();   // 局域网多播发现(整个进程只启动一次)
        // 绑定成功才落盘偏好, 且写**实际生效端口**(含被占顺延): 重启后回放的就是
        // 手机上还能打开的那个地址, 而不是曾被占用过的首选值
        if preferred.is_some() {
            // 原子写: 半截端口("16")会让下次启动回放错误端口, 已分享的地址静默失效
            let _ = write_secret_atomic(&port_pref_file(), port.to_string().as_bytes());
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
        // 启动即宣告 + 主动探测, 不等 5s 周期; 同网段已有设备会立即回包。
        send_discovery_announce();
        send_discovery_probe();
        Ok(port)
    })
}

/// 服务启动时清理**过期**的上传断点文件(*.ccpart 及其 .meta, 超过 UPLOAD_PART_TTL_SECS)。
/// 期内保留: 服务重启后客户端重选同一文件, 会话指纹命中即可续传
/// (Android 退后台被杀/电脑重启不再无声归零); 只有过期垃圾才回收磁盘。
/// 不跟随符号链接: 否则会删到共享目录之外(或把外部目录列进文件表)。
/// 启动清理预算: 共享目录可能很大/在网络盘上, setup 线程不能无限递归。
const CLEAN_MAX_ENTRIES: usize = 20_000;
const CLEAN_MAX_DEPTH: usize = 32;
const CLEAN_BUDGET_MS: u128 = 1_500;
struct CleanBudget {
    deadline: std::time::Instant,
    seen: usize,
    capped: bool,
}
impl CleanBudget {
    fn new() -> Self {
        Self {
            deadline: std::time::Instant::now()
                + std::time::Duration::from_millis(CLEAN_BUDGET_MS as u64),
            seen: 0,
            capped: false,
        }
    }
    fn tick(&mut self) -> bool {
        self.seen += 1;
        if self.seen > CLEAN_MAX_ENTRIES || std::time::Instant::now() > self.deadline {
            self.capped = true;
            return false;
        }
        true
    }
}

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
    fn walk(dir: &Path, n: &mut usize, depth: usize, budget: &mut CleanBudget) {
        if depth > CLEAN_MAX_DEPTH {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if !budget.tick() {
                return;
            }
            let p = e.path();
            let Ok(md) = e.metadata() else { continue }; // DirEntry::metadata 不跟随符号链接
            if md.is_dir() {
                walk(&p, n, depth + 1, budget);
            } else if md.is_file()
                && p
                    .file_name()
                    .and_then(|s| s.to_str())
                    // 只回收"带会话指纹的旧断点"和系统的原子写临时名; 普通用户文件
                    // 即使同名同后缀也保留。.ccup 由 clean_incomplete_uploads 单独处理。
                    .map_or(false, |n| {
                        let low = n.to_ascii_lowercase();
                        if low.ends_with(".ccpart.meta.tmp") {
                            return true;
                        }
                        is_session_part_name(&low)
                            && !low.ends_with(".ccup")
                            // 必须能对上 meta 里的会话 id, 否则可能是用户自己的同名文件
                            && session_part_meta_matches(&p, &low)
                    })
                && expired(&md)
                && std::fs::remove_file(&p).is_ok()
            {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    let mut budget = CleanBudget::new();
    walk(share, &mut n, 0, &mut budget);
    if n > 0 {
        push_log(format!("已清理 {n} 个过期的上传断点文件(超过 {}s)", UPLOAD_PART_TTL_SECS));
    }
    if budget.capped {
        push_log_warn("启动清理达到 1.5s/2 万条预算，剩余部分下次启动继续".into());
    }
}

/// 硬中断残留清理(#1 数据完整性): upload 在写入首个字节前先创建 `.{文件名}.ccup` 标记,
/// 成功后删除; 崩溃/停服超时 abort/进程被杀都不会走到错误清理路径 —— 启动时见标记即认定
/// 对应成品是半截文件, 连标记一并删除。仅启动执行(此刻必无在途上传)。
/// 残余窗口: "占位创建后、标记创建前"崩溃会留下 0 字节成品(无标记)——空文件肉眼可辨,
/// 不属"与完整文件无法区分"的静默损坏, 接受。
fn clean_incomplete_uploads(share: &Path) {
    fn walk(dir: &Path, n: &mut usize, depth: usize, budget: &mut CleanBudget) {
        if depth > CLEAN_MAX_DEPTH {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if !budget.tick() {
                return;
            }
            let p = e.path();
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                // 目录形态的 ".x.ccup" 是攻击残留(上传接口已拒绝, 但旧版本可创建):
                // 只清理空目录, 非空目录递归后保留, 绝不能删用户数据
                if let Some(fname) = p.file_name().and_then(|s| s.to_str()) {
                    let l = fname.to_ascii_lowercase();
                    if l.starts_with('.') && l.ends_with(".ccup") {
                        if std::fs::remove_dir(&p).is_ok() {
                            *n += 1;
                            push_log_warn(format!("清理目录形态的上传标记 {fname}"));
                        }
                        continue;
                    }
                }
                walk(&p, n, depth + 1, budget);
                continue;
            }
            let Some(fname) = p.file_name().and_then(|s| s.to_str()) else { continue };
            // 用 strip 而不是字节切片: 名为 ".ccup" 的文件会让 fname[1..0] 越界 panic
            let Some(final_name) = fname
                .strip_prefix('.')
                .and_then(|s| s.strip_suffix(".ccup"))
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let final_path = dir.join(final_name);
            // 标记格式: 首行会话令牌 / 第二行声明大小(可选); "ok" = 上传成功但标记删除失败。
            // 不是这两种形态的一律视为用户自己的 .ccup 文件, 绝不触碰(旧实现见名就删同名成品)。
            // 只读前 256 字节: token 26 + 声明大小 < 20, 足够; 避免用户大 .ccup 整份读入。
            let content = read_ccup_prefix(&p, 256).unwrap_or_default();
            let mut lines = content.lines();
            let token = lines.next().unwrap_or("").trim();
            let declared = lines.next().and_then(|s| s.trim().parse::<u64>().ok());
            let done = token == "ok";
            if !done && !is_upload_token(token) {
                // 空标记 = 崩溃在"建标记后、写令牌前": 成品通常还是 0 字节, 直接回收。
                // 非空内容一律视为用户自己的 .x.ccup, 保留并提示(不静默删用户文件)。
                if token.is_empty() {
                    if let Ok(md) = std::fs::metadata(&final_path) {
                        if md.is_file() && md.len() == 0 {
                            let _ = std::fs::remove_file(&final_path);
                            if std::fs::remove_file(&p).is_ok() {
                                *n += 1;
                                push_log_warn(format!("清理空上传标记与 0 字节残留 {final_name}"));
                            }
                        }
                    }
                    continue;
                }
                // v1.2.1 及更早的标记是裸 16 位 hex: 只做安全迁移 ——
                // 成品大小与声明吻合时清标记；不吻合时保留标记/成品，绝不自动删成品
                // (避免把用户自己的 .x.ccup 文件误判后删掉同名文件)。
                let old_style = token.len() == 16
                    && token.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
                if old_style && declared.is_some() {
                    let complete = declared.map_or(false, |d| {
                        d > 0
                            && std::fs::metadata(&final_path)
                                .map(|m| m.is_file() && m.len() == d)
                                .unwrap_or(false)
                    });
                    if complete && std::fs::remove_file(&p).is_ok() {
                        push_log_warn(format!("清理旧版完成标记(成品完好) {final_name}"));
                    } else {
                        push_log_warn(format!(
                            "发现旧版上传标记 .{final_name}.ccup，成品未自动删除；如为残留请手动清理"
                        ));
                    }
                }
                continue;
            }
            let mut removed = false;
            if done {
                removed |= std::fs::remove_file(&p).is_ok();
                if removed {
                    push_log_warn(format!("清理未删除的完成标记(成品完好) {final_name}"));
                }
                continue;
            }
            // 无声明大小(客户端未发 X-File-Size): 无法区分"半截"与"已完整落盘但标记未删"。
            // 绝不能按"判定不了就删"处理 —— 崩溃窗口前用户可能已把文件换成自己的完整文件。
            // 非破坏性做法: 把成品改名保留(.incomplete 后缀), 清标记让用户在列表里看到并自行判断。
            if declared.is_none() {
                match std::fs::metadata(&final_path) {
                    Ok(md) if md.is_file() => {
                        let mut alt = dir.join(format!("{final_name}.incomplete"));
                        let mut variant = 1u32;
                        while alt.exists() && variant < 1000 {
                            alt = dir.join(format!("{final_name}.incomplete-{variant}"));
                            variant += 1;
                        }
                        match std::fs::rename(&final_path, &alt) {
                            Ok(()) => {
                                let _ = std::fs::remove_file(&p);
                                push_log_warn(format!(
                                    "发现无大小声明的上传残留 {final_name}，已改名保留为 {}（请人工确认完整性）",
                                    alt.file_name().and_then(|s| s.to_str()).unwrap_or("*.incomplete")
                                ));
                            }
                            Err(e) => push_log_warn(format!(
                                "无大小声明的上传残留 {final_name} 改名失败({e})，保留原标记等待下次启动"
                            )),
                        }
                    }
                    _ => {
                        let _ = std::fs::remove_file(&p);
                    }
                }
                continue;
            }
            // 声明大小吻合 = 崩溃发生在"成品已完整落盘、标记尚未删除"的窗口:
            // 保留成品, 只清标记(旧实现会把完整文件当半截删掉)
            let complete = declared.map_or(false, |d| {
                d > 0
                    && std::fs::metadata(&final_path)
                        .map(|m| m.is_file() && m.len() == d)
                        .unwrap_or(false)
            });
            if complete {
                removed |= std::fs::remove_file(&p).is_ok();
                if removed {
                    push_log_warn(format!("清理完成标记(成品大小吻合, 已保留) {final_name}"));
                }
            } else {
                // 声明大小不吻合时也不能直接删成品: 硬杀进程后用户可能在重启前
                // 把该路径换成自己的完整文件。采用与"无声明大小"相同的非破坏性策略,
                // 改名保留 .incomplete, 只清标记, 让用户自行确认。
                match std::fs::metadata(&final_path) {
                    Ok(md) if md.is_file() => {
                        let mut alt = dir.join(format!("{final_name}.incomplete"));
                        let mut variant = 1u32;
                        while alt.exists() && variant < 1000 {
                            alt = dir.join(format!("{final_name}.incomplete-{variant}"));
                            variant += 1;
                        }
                        match std::fs::rename(&final_path, &alt) {
                            Ok(()) => {
                                let _ = std::fs::remove_file(&p);
                                removed = true;
                                push_log_warn(format!(
                                    "发现大小不符的上传残留 {final_name}，已改名保留为 {}（请人工确认完整性）",
                                    alt.file_name().and_then(|s| s.to_str()).unwrap_or("*.incomplete")
                                ));
                            }
                            Err(e) => push_log_warn(format!(
                                "大小不符的上传残留 {final_name} 改名失败({e})，保留原标记等待下次启动"
                            )),
                        }
                    }
                    _ => {
                        let _ = std::fs::remove_file(&p);
                        removed = true;
                    }
                }
            }
            if removed {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    let mut budget = CleanBudget::new();
    walk(share, &mut n, 0, &mut budget);
    if n > 0 {
        push_log(format!("已清理 {n} 个硬中断残留(半截成品及其标记)"));
    }
    if budget.capped {
        push_log_warn("启动清理达到 1.5s/2 万条预算，剩余部分下次启动继续".into());
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
/// stop() 在途标志: 优雅关闭在 running_slot 锁外进行(最长约 2.3s), 生命周期锁拦不住
/// 并发 start —— 旧 listener 未释放时 start 会顺延绑到下一个端口(地址/二维码漂移失效)。
/// start 在拿到同一把生命周期锁后等待此标志清除再绑定。
static STOPPING: AtomicBool = AtomicBool::new(false);
/// 已有一个 stop 在优雅关闭中: 第二个 stop 不能把 STOPPING 提前清掉
static STOP_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// start/stop 共用的生命周期锁(取代 start 内部局部 START_GUARD)
fn lifecycle_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

pub fn stop() -> bool {
    // 并发 stop: 第二个直接返回, 绝不把 STOPPING 置回 false —— 否则 start 会在
    // 第一个 stop 仍在关闭时拿到锁启动, 出现"已停止但仍运行/端口漂移"
    if STOP_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return false;
    }
    // 入口即置位: 必须先于抢生命周期锁 —— 否则 start 在"检查通过后、stop 开始前"
    // 的空档拿到锁直接 bind, 仍会与在途优雅关闭抢端口(地址漂移窗口没关死)
    STOPPING.store(true, Ordering::SeqCst);
    // 生命周期锁与 start 互斥; running_slot 锁只保护 handle 的接管。
    // start 会在持锁期间扫描共享目录, 网络盘/休眠外接盘上一次 read_dir 可阻塞很久 ——
    // 这里用带超时的 try_lock, 避免托盘/HTTP 控制面跟着永久挂起。
    let lifecycle = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match lifecycle_lock().try_lock() {
                Ok(g) => break g,
                Err(std::sync::TryLockError::Poisoned(p)) => break p.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        push_log_warn(
                            "停止服务超时：启动清理仍在等待共享目录响应（可能是已断开的网络盘），请稍后重试"
                                .into(),
                        );
                        STOPPING.store(false, Ordering::SeqCst);
                        STOP_IN_FLIGHT.store(false, Ordering::SeqCst);
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    };
    let mut lock = running_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = lock.take() {
        // 正常停止时发 bye, 对端立刻移除; 异常断电仍由 30s TTL 兜底。
        send_discovery_bye();
        push_log("服务已停止".into());
        broadcast_event("status:down");
        let _ = ws_kick().send(());
        let _ = h.shutdown.send(());
        let task = h.task;
        drop(lock);
        drop(lifecycle); // 别持有全局锁等待关闭
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
            // 等全部在途上传响应停服信号并完成自清理: axum 的连接任务是独立 spawn 的,
            // abort serve future 不会停掉它们; 不等待的话, 紧接着的 start() 会在
            // clean_incomplete_uploads 里把旧 handler 仍在写的"在途文件"删掉,
            // 而旧 handler 继续写完并回 200 —— "停止服务"的撤销语义失效。
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                let busy = !active_uploads()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty();
                if !busy || tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
        STOPPING.store(false, Ordering::SeqCst);
        STOP_IN_FLIGHT.store(false, Ordering::SeqCst);
        true
    } else {
        STOPPING.store(false, Ordering::SeqCst);
        STOP_IN_FLIGHT.store(false, Ordering::SeqCst);
        false
    }
}

/// 应用启动时自动拉起(失败仅打日志, 不阻断 GUI)。
/// 回放上次指定的端口: 重启应用后地址不漂移
pub fn autostart() -> Result<u16, String> {
    start(last_preferred_port())
}

/// 桌面端默认开放局域网(产品定位就是同 Wi-Fi 互传); 移动端默认安全模式(仅 127.0.0.1),
/// 由用户在 CC设置 里显式开启 —— 手机经常接入不可信的公共 Wi-Fi, 默认不能裸奔。
static OPEN_LAN: AtomicBool = AtomicBool::new(cfg!(not(any(
    target_os = "android",
    target_os = "ios"
))));
const OPEN_LAN_PREF_FILE: &str = "open_lan_pref";

fn open_lan_pref_file() -> PathBuf { port_pref_file().with_file_name(OPEN_LAN_PREF_FILE) }

fn open_lan() -> bool {
    // 桌面端与移动端都始终监听局域网(产品定位就是同一 Wi-Fi 互传), 不再提供开关;
    // 历史 open_lan_pref=0 不再生效, 避免"设置页没有入口却停在仅本机"的死角。
    // 移动端仍自动生成本机访问令牌, 同网段设备需配对码。
    true
}

pub fn set_open_lan(open: bool) {
    if !open {
        // 产品已取消局域网开关；保留命令兼容旧前端，但绝不能打印“已启用安全模式”
        // 让用户误以为关闭成功。
        OPEN_LAN.store(true, Ordering::Relaxed);
        push_log_warn("局域网访问始终开启，忽略关闭请求".into());
        return;
    }
    OPEN_LAN.store(true, Ordering::Relaxed);
    if let Err(e) = write_secret_atomic(open_lan_pref_file().as_path(), b"1") {
        push_log_warn(format!("局域网访问偏好保存失败(重启后可能回落默认值): {e}"));
    }
    push_log("已开放局域网访问（重启服务后生效）".into());
}

pub fn status() -> (bool, u16, String) {
    let running = running_slot().lock().unwrap_or_else(|e| e.into_inner()).is_some();
    (running, PORT.load(Ordering::Relaxed), lan_ip())
}

/// 托盘/外部打开网页用的地址: 安全模式只监听回环时不能给局域网 IP(必然打不开)。
pub fn web_url() -> String {
    let (running, port, _) = status();
    if !running || port == 0 {
        return String::new();
    }
    let host = if LAN_ACTIVE.load(Ordering::Relaxed) {
        lan_ip()
    } else {
        "127.0.0.1".to_string()
    };
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    format!("http://{host}:{port}")
}

// ---------------------------------------------------------------- //
// 路径安全 / 工具
// ---------------------------------------------------------------- //

/// 相对路径校验: 输入应为**已解码**字符串(axum Path/Query 提取器自带一次解码;
/// X-Filename 请求头由 upload 处理解码一次后再传入)。逐段检查(拒绝空/./.. 与非法字符),
/// 允许子目录(整目录上传), 段数上限与文件列表递归深度一致。
/// 返回以 "/" 连接的相对路径; 保证 join 后不越出共享目录。
fn safe_rel_path(raw: &str, for_upload: bool) -> Option<String> {
    // 只在 Windows 把 `\` 视作分隔符。Unix 的 `\` 是合法文件名字符: 全局归一化会让
    // 列表里的 `a\b.txt` 与真实存在的 `a/b.txt` 指向同一路径(删除/下载串文件)。
    let norm = if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw.to_string()
    };
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
                    // Windows 把上标数字 ¹²³ 也按设备号解析(COM¹ 等价 COM1)
                    | "COM¹" | "COM²" | "COM³"
                    | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
                    | "LPT¹" | "LPT²" | "LPT³"
            ) {
                return None;
            }
        }
        // Windows 读路径别名: Win32 会去掉段尾的点/空格, `:` 表示 ADS/盘符。
        // 真实共享文件不可能长这样; 放行只会让 DELETE/下载用别名绕过按名字
        // 建立的保护(active_uploads key、`.{名}.ccup` 完整性标记)。
        if cfg!(windows) && (seg.contains(':') || seg.ends_with('.') || seg.ends_with(' ')) {
            return None;
        }
        parts.push(seg.to_string());
    }
    if parts.is_empty() || parts.len() > UPLOAD_MAX_DEPTH {
        return None;
    }
    // 保留内部产物后缀: 仅上传入口拒绝(防止 ".victim.ccup/x" 创建同名目录)。
    // 读路径必须放行 —— 用户自己就有 `.x.ccup` 文件时, 若连删除都拒绝, 数据在
    // UI 里永久不可达; 下载/删除的真实安全由 marker 令牌与用户类型判断负责。
    if for_upload {
        for seg in &parts {
            let s = seg.to_ascii_lowercase();
            if s.ends_with(UPLOAD_PART_SUFFIX)
                || s.ends_with(UPLOAD_PART_META_SUFFIX)
                || s.ends_with(&format!("{}.tmp", UPLOAD_PART_META_SUFFIX))
                || s.ends_with(".ccup")
            {
                return None;
            }
        }
    }
    if let Some(last) = parts.last() {
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
fn session_hash(upload_id: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in upload_id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn session_part_path(share: &Path, name: &str, upload_id: &str) -> (PathBuf, PathBuf) {
    let part = share.join(format!("{name}.{}{UPLOAD_PART_SUFFIX}", session_hash(upload_id)));
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

/// 可安全 inline 预览的扩展名: 排除 html/svg/xml 等能在服务同源执行脚本的类型
fn is_inline_safe(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "avif" | "pdf"
            | "txt" | "md" | "log" | "csv"
            | "mp3" | "mp4" | "webm" | "mov" | "m4a" | "ogg" | "wav"
    )
}

/// 合法的旧版会话断点名: <name>.<16hex>.ccpart / <name>.<16hex>.ccpart.meta。
/// 用于 TTL 清理门槛 —— 不带会话指纹的同后缀普通文件可能是用户自己的数据, 不删。
fn is_session_part_name(fname: &str) -> bool {
    let rest = fname
        .strip_suffix(".ccpart.meta")
        .or_else(|| fname.strip_suffix(".ccpart"));
    let Some(pre) = rest else { return false };
    pre.rsplit('.')
        .next()
        .map(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
        .unwrap_or(false)
}

/// 会话断点是否真的属于内部产物: 除文件名带 16hex 指纹外, 还要求同名 .meta
/// 的首行会话 id 经 FNV 后与文件名指纹一致。用户自己恰好命名的
/// `xxx.0123456789abcdef.ccpart` 没有匹配 meta, 不会被隐藏/清理。
fn session_part_meta_matches(file: &Path, fname: &str) -> bool {
    let low = fname.to_ascii_lowercase();
    let Some(hash) = low
        .strip_suffix(UPLOAD_PART_META_SUFFIX)
        .or_else(|| low.strip_suffix(UPLOAD_PART_SUFFIX))
        .and_then(|pre| pre.rsplit('.').next())
        .filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    else {
        return false;
    };
    let meta = if low.ends_with(UPLOAD_PART_META_SUFFIX) {
        file.to_path_buf()
    } else {
        part_meta_path(file)
    };
    let Ok(content) = std::fs::read_to_string(meta) else { return false };
    let id = content.lines().next().unwrap_or("").trim();
    !id.is_empty() && session_hash(id) == hash
}

/// 共享目录遍历时跳过断点产物(中间文件与其 meta 及原子写临时名; 大小写不敏感——exFAT/U盘 拷来的变体同样跳)
fn is_part_artifact(path: &Path, fname: &str) -> bool {
    let f = fname.to_ascii_lowercase();
    (f.ends_with(UPLOAD_PART_SUFFIX) || f.ends_with(UPLOAD_PART_META_SUFFIX))
        && session_part_meta_matches(path, &f)
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

/// 上传/删除的"占位→标记→在途登记"临界区锁: 三段窗口内若 delete 恰好进来,
/// 文件会被删掉而上传方仍继续写入并回 200(Unix 写已删除 inode = 假成功)。
/// 该锁把上传的占位/标记/登记与删除的复查/删除串行化(锁内只做短文件操作)。
fn file_op_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 上传会话令牌(16 位十六进制): 写入完整性标记首行, 用于区分"本会话的标记/成品"
/// 与"同名新上传或用户自己的 .ccup 文件" —— abort 只清理仍属于本会话的产物。
const UPLOAD_TOKEN_PREFIX: &str = "ccflash-v1:";

fn fill_random(bytes: &mut [u8]) {
    if getrandom::getrandom(bytes).is_ok() {
        return;
    }
    // CSPRNG 不可用时不让 panic=abort 杀掉整个服务: 用时间/进程/原子序号
    // 经 splitmix64 扩散出可用的临时随机数, 仍保证 PIN/票据不可预测性远高于固定值。
    static FALLBACK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64)
        ^ FALLBACK_SEQ.fetch_add(1, Ordering::Relaxed);
    for chunk in bytes.chunks_mut(8) {
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = (z >> (i * 8)) as u8;
        }
    }
}

fn new_upload_token() -> String {
    let mut bytes = [0u8; 8];
    fill_random(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    // 固定前缀 = 标记签名: 用户自己的 .x.ccup 文件即使首行恰好是 16 位 hex,
    // 也不会被当成系统标记而删掉同名成品
    format!("{UPLOAD_TOKEN_PREFIX}{hex}")
}

/// 标记首行是否为系统生成的会话令牌(前缀 + 16 位小写十六进制)
fn is_upload_token(s: &str) -> bool {
    s.strip_prefix(UPLOAD_TOKEN_PREFIX)
        .map(|t| t.len() == 16 && t.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
        .unwrap_or(false)
}

/// `.x.ccup` 是否属于系统上传标记: 空文件(崩溃在写令牌前)、`ccflash-v1:<hex>`、
/// 或完成标记 `ok`。用户自建的同名文件内容任意, 不属于内部产物。
/// 读取 .ccup 前 max 字节并按行解析。标记只有"首行 token + 可选的声明大小",
/// 不需要整份读入内存 —— /api/files 每 5s 会对每个标记调用一次。
fn read_ccup_prefix(path: &Path, max: usize) -> Option<String> {
    use std::io::Read;
    if !std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false) {
        return None;
    }
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(max as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// 首行是否为系统标记(含 v1.2.1 及更早的裸 16 位 hex 形态)。
fn ccup_marker_token(s: &str) -> (&str, bool) {
    let t = s.lines().next().unwrap_or("").trim();
    let legacy = t.len() == 16
        && t.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    (t, legacy)
}

fn is_system_ccup(path: &Path) -> bool {
    let Some(s) = read_ccup_prefix(path, 128) else { return false; };
    let (t, legacy) = ccup_marker_token(&s);
    // 二进制/非 UTF8/空内容不再被当成系统标记: 用户自建的 .x.ccup(含 0 字节)
    // 不应把同名成品永久隐藏/锁成"上传中"(旧实现 Err/空 => true)。
    t == "ok" || is_upload_token(t) || legacy
}

/// 成品是否仍处于"上传中": 对应 .{名}.ccup 标记是系统标记且不是完成的 "ok"。
/// "ok" 是成功落盘但标记删除失败的状态, 成品必须可列出/可下载。
fn marker_is_incomplete(file: &Path) -> bool {
    let Some(dir) = file.parent() else { return false };
    let Some(base) = file.file_name().and_then(|s| s.to_str()) else { return false };
    let marker = dir.join(format!(".{base}.ccup"));
    let Some(s) = read_ccup_prefix(&marker, 128) else { return false };
    let (t, legacy) = ccup_marker_token(&s);
    let system = t == "ok" || is_upload_token(t) || legacy;
    if !system {
        return false; // 用户自建的同名 .ccup 不能把成品锁成"上传中"
    }
    t != "ok"
}

/// 上传 handler 被底层连接直接 drop(客户端 RST/连接任务取消)时的兜底:
/// 正常错误路径会显式 abort_upload; 但 hyper 可能直接丢弃 future, 导致
/// active_uploads 计数、marker、半截成品永久残留。Drop 里 spawn 一次同样的
/// 清理(abort_upload 会校验标记令牌, 正常成功路径调用也无害)。
struct UploadCleanup {
    path: PathBuf,
    marker: PathBuf,
    token: String,
    key: String,
    /// 显式清理/成功路径会先置 false: 否则 Drop 会再执行一次 abort_upload,
    /// 同名新上传已注册同一路径 key 时, 第二次递减会把新会话的计数误清掉
    armed: bool,
}
impl Drop for UploadCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (path, marker, token, key) = (
            self.path.clone(),
            self.marker.clone(),
            self.token.clone(),
            self.key.clone(),
        );
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { abort_upload(&path, &marker, &token, &key).await; });
        } else {
            drop_active(&self.key);
        }
    }
}

/// 上传失败统一清理: 仅在完整性标记仍属于本会话时删除半截成品与标记。
/// 若标记已被同名新上传替换/移除, 成品可能已属于别人 —— 绝不能按路径无条件删。
async fn abort_upload(path: &std::path::Path, marker: &std::path::Path, token: &str, key: &str) {
    // 与 DELETE 的复查/删除互斥, 避免"判定后删除"与清理互相踩踏
    let _op = file_op_lock().lock().await;
    let owned = tokio::fs::read(marker)
        .await
        .map(|b| String::from_utf8_lossy(&b).lines().next().unwrap_or("").trim() == token)
        .unwrap_or(false);
    if owned {
        // 只有成品确实消失(删除成功或本来就不存在)才移除标记: Windows 上杀软/
        // 索引器持有句柄时 remove 会失败, 先删标记会让半截文件以真实文件名出现在
        // 列表里并被当成完整文件下载(NFS 瞬时错误同理)。
        let gone = match tokio::fs::remove_file(path).await {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if gone {
            let _ = tokio::fs::remove_file(marker).await;
        } else {
            push_log_warn(format!(
                "半截文件删除失败(可能被占用)，已保留上传标记等待下次启动清理: {}",
                path.display()
            ));
        }
    }
    drop_active(key);
}

/// 一次性分享票据：授权“某个相对路径的单次下载”，永不授予上传/删除/API 能力。
/// 承载式(bearer)语义: 凭 ticket 字符串本身 + 文件名绑定 + TTL 生效, 不校验来源 IP ——
/// 创建方是桌面壳(127.0.0.1)、接收方必然是局域网另一台设备, 校验 owner 会让分享永远失败。
struct ShareTicket {
    name: String,
    /// 用单调时钟: 系统时间回拨不会把 10 分钟有效期的分享链接无限延长
    expires_at: std::time::Instant,
    /// 签发时的文件指纹: 文件被删除/同名重传后票据立即失效, 不能下载替换后的内容
    size: u64,
    etag: String,
}

fn ticket_file_matches(share: &Path, name: &str, size: u64, etag: &str) -> bool {
    safe_join_existing(share, name)
        .and_then(|p| p.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len() == size && file_etag(&m) == etag)
        .unwrap_or(false)
}

/// 票据文件指纹校验的 blocking 版本: middleware/下载链路不得直接做磁盘 IO
async fn ticket_file_matches_async(share: &Path, name: &str, size: u64, etag: &str) -> bool {
    let share = share.to_path_buf();
    let name = name.to_string();
    let etag = etag.to_string();
    tokio::task::spawn_blocking(move || ticket_file_matches(&share, &name, size, &etag))
        .await
        .unwrap_or(false)
}

fn share_tickets() -> &'static Mutex<HashMap<String, ShareTicket>> {
    static TICKETS: OnceLock<Mutex<HashMap<String, ShareTicket>>> = OnceLock::new();
    TICKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 缩略图/预览用的短时媒体会话: 不绑定具体文件, 但绑定来源 IP + 10 分钟 TTL,
/// 可重复用于 HEAD/GET/Range。避免给每个 <img> 都开一次性票据导致开票限流,
/// 同时长期 PIN 不进媒体 URL; 泄漏到另一台设备也因 IP 不匹配而不可用。
struct MediaSession {
    peer: std::net::IpAddr,
    expires_at: std::time::Instant,
}
const MEDIA_SESSION_TTL_SECS: u64 = 10 * 60;
fn media_sessions() -> &'static Mutex<HashMap<String, MediaSession>> {
    static M: OnceLock<Mutex<HashMap<String, MediaSession>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}
fn media_session_allows(token: &str, peer: std::net::IpAddr) -> bool {
    if token.is_empty() { return false; }
    let now = std::time::Instant::now();
    let mut m = media_sessions().lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, s| s.expires_at > now);
    m.get(token).map(|s| s.peer == peer).unwrap_or(false)
}

const SHARE_TICKET_TTL_SECS: u64 = 10 * 60;

fn random_token() -> String {
    // 12 位十六进制(48bit): 6 位(24bit)在 10 分钟 TTL 内可被局域网/多 IPv6 源猜测,
    // 且 2048 张票据时生日碰撞约 12%。48bit 下暴力猜中与碰撞都不现实; 票据仍是
    // 承载式、绑定单文件、下载即焚。
    let mut bytes = [0u8; 6];
    fill_random(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn prune_tickets_locked(tickets: &mut HashMap<String, ShareTicket>) {
    let now = std::time::Instant::now();
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
    if !ticket_issue_allow(peer) {
        return err(StatusCode::TOO_MANY_REQUESTS, "分享链接生成过于频繁，请稍后再试");
    }
    let Some(name) = safe_rel_path(&req.name, false) else {
        return err(StatusCode::BAD_REQUEST, "非法的文件名");
    };
    let share = share_dir();
    let Some((size, etag)) = file_fingerprint_async(&share, &name).await else {
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    let ticket = {
        let mut tickets = share_tickets().lock().unwrap_or_else(|e| e.into_inner());
        prune_tickets_locked(&mut tickets);
        // 满额不再随机驱逐 —— 同网段任意设备都能刷票, 驱逐会让合法分享链接
        // 静默失效。宁可拒绝新票, 也不牺牲已发出的链接。
        if tickets.len() >= 2048 {
            return err(StatusCode::SERVICE_UNAVAILABLE, "分享链接过多，请稍后再试");
        }
        // 即使碰撞概率极低也不覆盖已有票据(否则旧分享链接会静默失效)
        let generated = loop {
            let t = random_token();
            if !tickets.contains_key(&t) {
                break t;
            }
        };
        tickets.insert(
            generated.clone(),
            ShareTicket {
                name: name.clone(),
                expires_at: std::time::Instant::now() + std::time::Duration::from_secs(SHARE_TICKET_TTL_SECS),
                size,
                etag,
            },
        );
        generated
    };
    push_log(format!("生成分享票据 {name} ← {peer}"));
    Json(serde_json::json!({ "ticket": ticket, "ttl": SHARE_TICKET_TTL_SECS })).into_response()
}

/// 创建一个 10 分钟、绑定来源 IP 的媒体会话令牌。前端每个页面只取一次,
/// 缩略图/图片/视频/PDF 预览共用, 不再逐文件开一次性票据。
async fn media_session(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    if !ticket_issue_allow(peer) {
        return err(StatusCode::TOO_MANY_REQUESTS, "媒体会话创建过于频繁，请稍后再试");
    }
    let token = {
        let mut m = media_sessions().lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        m.retain(|_, s| s.expires_at > now);
        if m.len() >= 2048 {
            return err(StatusCode::SERVICE_UNAVAILABLE, "媒体会话过多，请稍后再试");
        }
        let token = loop {
            let t = random_token();
            if !m.contains_key(&t) { break t; }
        };
        m.insert(token.clone(), MediaSession {
            peer,
            expires_at: now + std::time::Duration::from_secs(MEDIA_SESSION_TTL_SECS),
        });
        token
    };
    Json(serde_json::json!({ "token": token, "ttl": MEDIA_SESSION_TTL_SECS })).into_response()
}

/// 校验票据但不移除(peek): 有效返回票据 id。
/// raw_path 是线上原始(百分号编码)形态: 逐段解码后再走安全校验 ——
/// 票据创建时存的是解码后的文件名, 不解码则中文名票据永远匹配不上;
/// 先解码后校验也保证 %2E%2E 等穿越形态仍被 safe_rel_path 拦下。
async fn valid_ticket(query: &str, raw_path: &str) -> Option<String> {
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
    // 先在锁内取票据身份, 文件指纹校验放在锁外(可能触发磁盘 IO)
    let (size, etag) = {
        let mut tickets = share_tickets().lock().unwrap_or_else(|e| e.into_inner());
        prune_tickets_locked(&mut tickets); // 过期票在此被移除
        let t = tickets.get(&ticket)?;
        if t.name != name {
            return None;
        }
        (t.size, t.etag.clone())
    };
    if ticket_file_matches_async(&share_dir(), &name, size, &etag).await { Some(ticket) } else { None }
}

/// 原子取出票据: 校验与移除在同一把锁内完成。
/// 旧实现"peek 校验 → handler await → 事后移除"存在 TOCTOU, 并发请求都能通过校验并各下载一次。
async fn take_ticket(query: &str, raw_path: &str) -> Option<(String, ShareTicket)> {
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
    let (size, etag) = {
        let mut tickets = share_tickets().lock().unwrap_or_else(|e| e.into_inner());
        prune_tickets_locked(&mut tickets);
        let t = tickets.get(&ticket)?;
        if t.name != name {
            return None;
        }
        (t.size, t.etag.clone())
    };
    // 文件已被删除或同名重传(内容变了) → 票据失效, 不再授权下载
    if !ticket_file_matches_async(&share_dir(), &name, size, &etag).await {
        return None;
    }
    share_tickets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&ticket)
        .map(|t| (ticket, t))
}

/// 下载失败时把已取出的票据放回(4xx/5xx/连接中断不烧票), 保持"成功下载才消费"的语义。
fn put_ticket(id: String, ticket: ShareTicket) {
    let mut tickets = share_tickets().lock().unwrap_or_else(|e| e.into_inner());
    prune_tickets_locked(&mut tickets);
    if tickets.len() < 2048 {
        tickets.insert(id, ticket);
    }
}

/// 从查询串提取并解码 ticket 参数(供已消费票据的 Range 复用校验)。
fn query_ticket_value(query: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("ticket="))
        .map(|v| percent_encoding::percent_decode_str(v).decode_utf8_lossy().to_string())
}

/// 把 `/api/download` 之后的原始路径解码成安全相对路径。
fn ticket_path_name(raw_path: &str) -> Option<String> {
    let decoded = raw_path
        .split('/')
        .map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/");
    safe_rel_path(decoded.trim_start_matches('/'), false)
}

/// 一次性票据被首个 GET 消费后, 同一客户端对同一文件在 TTL 内的 Range
/// 续传/分段预览仍可用(Safari 媒体元素/PDF 查看器会发多段 Range)。
/// 无 Range 的第二次整包下载依然 401, 保持"完整下载一次"的语义。
struct RecentTicket {
    name: String,
    size: u64,
    etag: String,
    peer: std::net::IpAddr,
    used_at: std::time::Instant,
    /// 不能超过原始一次性票据的过期时间；否则“下载一次”可被续到签发后 20 分钟。
    expires_at: std::time::Instant,
}

const RECENT_TICKET_TTL_SECS: u64 = 10 * 60;

fn recent_tickets() -> &'static Mutex<HashMap<String, RecentTicket>> {
    static RECENT: OnceLock<Mutex<HashMap<String, RecentTicket>>> = OnceLock::new();
    RECENT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_ticket(id: &str, t: &ShareTicket, peer: std::net::IpAddr) {
    let now = std::time::Instant::now();
    let mut m = recent_tickets().lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, r| {
        now < r.expires_at && now.duration_since(r.used_at).as_secs() < RECENT_TICKET_TTL_SECS
    });
    if m.len() >= 2048 {
        if let Some(k) = m.keys().next().cloned() {
            m.remove(&k);
        }
    }
    m.insert(
        id.to_string(),
        RecentTicket {
            name: t.name.clone(),
            size: t.size,
            etag: t.etag.clone(),
            peer,
            used_at: now,
            expires_at: t.expires_at,
        },
    );
}

/// 是否是 `bytes=0-...` 整包 Range：已消费票据不允许再用它重放整文件。
fn range_starts_at_zero(req: &axum::extract::Request) -> bool {
    let Some(v) = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(raw) = v.strip_prefix("bytes=") else { return false; };
    let first = raw.split(',').next().unwrap_or("").trim();
    let Some((a, _)) = first.split_once('-') else { return false; };
    !a.trim().is_empty() && a.trim().parse::<u64>() == Ok(0)
}

/// 已消费票据的 Range 复用校验: 同票据 + 同来源 IP + 同文件 + 文件内容未变。
async fn recent_ticket_allows(id: &str, name: &str, peer: std::net::IpAddr) -> bool {
    let now = std::time::Instant::now();
    // 锁守卫不能跨 await: 先取出指纹再释放锁, 否则中间件 future 不是 Send
    let (size, etag) = {
        let mut m = recent_tickets().lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, r| {
            now < r.expires_at && now.duration_since(r.used_at).as_secs() < RECENT_TICKET_TTL_SECS
        });
        let Some(r) = m.get(id) else { return false; };
        if r.peer != peer || r.name != name || now >= r.expires_at {
            return false;
        }
        (r.size, r.etag.clone())
    };
    ticket_file_matches_async(&share_dir(), name, size, &etag).await
}

/// 当前所有票据(轮换/关闭访问码时统一作废, 兑现日志里"旧分享链接随之失效"的承诺)
fn clear_share_tickets() {
    share_tickets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    recent_tickets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    media_sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// 无效票据猜码限流(按来源 IP): 24bit 空间虽小, 但不限流时局域网可全速穷举。
/// 返回 true = 允许本次尝试; 连续失败达到阈值后暂停受理。
fn ticket_fails() -> &'static Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static FAILS: OnceLock<Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    FAILS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ticket_admit(peer: std::net::IpAddr) -> bool {
    let now = std::time::Instant::now();
    let mut m = ticket_fails().lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 2048 {
        m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < TICKET_FAIL_WINDOW_SECS);
    }
    let e = m.entry(rate_key(peer)).or_insert((0, now));
    if now.duration_since(e.1).as_secs() >= TICKET_FAIL_WINDOW_SECS {
        *e = (0, now);   // 窗口过期: 重新计数
    }
    if e.0 >= TICKET_FAIL_FREE {
        return false;
    }
    e.0 = e.0.saturating_add(1);
    e.1 = now;
    true
}

/// 开票限流: 每 IP 每分钟最多 60 张。前端每页预取 8 张, 正常用户远低于阈值;
/// 防止单个 LAN 客户端(或本机页面)灌满 2048 张票据表, 让合法分享返回 503。
const TICKET_ISSUE_PER_MIN: u32 = 60;

fn ticket_issue_rate() -> &'static Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static RATE: OnceLock<Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    RATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ticket_issue_allow(peer: std::net::IpAddr) -> bool {
    let now = std::time::Instant::now();
    let mut m = ticket_issue_rate().lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 1024 {
        m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 60);
    }
    let e = m.entry(rate_key(peer)).or_insert((0, now));
    // 固定窗口(见 discovery_probe_allow 注释): 额度按窗口起点恢复, 不再被每次成功推后
    if now.duration_since(e.1).as_secs() >= 60 {
        *e = (0, now);
    }
    if e.0 >= TICKET_ISSUE_PER_MIN {
        return false;
    }
    e.0 = e.0.saturating_add(1);
    true
}

/// 聊天发送限流: 每 IP 60 条/分钟。防止局域网洪泛反复触发「锁内序列化整段历史
/// 并同步写盘」而阻塞 tokio worker。
const CHAT_PER_MIN: u32 = 60;

/// 聊天读取/写入并发闸: 历史最多 200 万字符，一次 GET 会构造数 MB JSON；
/// 不限制时局域网并发拉到 GB 级，panic=abort 下直接终止进程。
fn chat_read_permits() -> &'static tokio::sync::Semaphore {
    static S: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    S.get_or_init(|| tokio::sync::Semaphore::new(4))
}
fn chat_write_permits() -> &'static tokio::sync::Semaphore {
    static S: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    S.get_or_init(|| tokio::sync::Semaphore::new(8))
}

fn chat_rate() -> &'static Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static RATE: OnceLock<Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    RATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn chat_admit(peer: std::net::IpAddr) -> bool {
    let now = std::time::Instant::now();
    let mut m = chat_rate().lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 1024 {
        m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 60);
    }
    let e = m.entry(rate_key(peer)).or_insert((0, now));
    // 固定窗口(见 discovery_probe_allow 注释): 60 条/分钟是持续速率, 不是
    // "发满 60 条后静默禁言 60s" —— 后者在群聊里会误伤正常用户
    if now.duration_since(e.1).as_secs() >= 60 {
        *e = (0, now);
    }
    if e.0 >= CHAT_PER_MIN {
        return false;
    }
    e.0 = e.0.saturating_add(1);
    true
}

/// 删除消息限流: 每次删除都会重写整段历史, 不限制时单来源可用 DELETE 洪水
/// 持续占用磁盘 IO(移动端还有闪存磨损)。60 次/分钟对正常用户足够。
const CHAT_DEL_PER_MIN: u32 = 60;
fn chat_del_rate() -> &'static Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static RATE: OnceLock<Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    RATE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn chat_del_admit(peer: std::net::IpAddr) -> bool {
    let now = std::time::Instant::now();
    let mut m = chat_del_rate().lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 1024 {
        m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 60);
    }
    let e = m.entry(rate_key(peer)).or_insert((0, now));
    if now.duration_since(e.1).as_secs() >= 60 {
        *e = (0, now);
    }
    if e.0 >= CHAT_DEL_PER_MIN {
        return false;
    }
    e.0 = e.0.saturating_add(1);
    true
}

/// 共享剪贴板写入限流: 内容可达 2 万字且每次附磁盘落盘, 不限制时可被高频 POST
/// 拖慢磁盘/占满 worker。桌面/手机自动同步约 40 次/分钟, 240 次/分钟留足余量。
const CLIP_PER_MIN: u32 = 240;
fn clip_rate() -> &'static Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static RATE: OnceLock<Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>> =
        OnceLock::new();
    RATE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn clip_admit(peer: std::net::IpAddr) -> bool {
    let now = std::time::Instant::now();
    let mut m = clip_rate().lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 1024 {
        m.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 60);
    }
    let e = m.entry(rate_key(peer)).or_insert((0, now));
    if now.duration_since(e.1).as_secs() >= 60 {
        *e = (0, now);
    }
    if e.0 >= CLIP_PER_MIN {
        return false;
    }
    e.0 = e.0.saturating_add(1);
    true
}

/// 写敏感文件(PIN / 聊天历史): Unix 下权限 0600, 避免同机其他用户读取。
fn write_secret(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)?;
        // 落盘后再交给 rename: 掉电时避免"文件名已替换、内容仍在页缓存"变空文件
        // (聊天历史/PIN/偏好都走这里, 一次延迟换来崩溃后可读)
        f.sync_all()
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)
    }
}





/// 轻量 MIME 映射: 替代 mime_guess 的完整类型表(体积收益明显)。
/// 覆盖前端静态资源、下载预览白名单与常见文档/压缩包; 未知类型回退 octet-stream,
/// 下载行为不受影响(浏览器仍按 Content-Disposition 保存)。
fn mime_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "xml" => "application/xml",
        "txt" | "md" | "log" | "csv" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "zip" => "application/zip",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "apk" => "application/vnd.android.package-archive",
        "dmg" => "application/x-apple-diskimage",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

fn safe_join_existing(share: &Path, name: &str) -> Option<PathBuf> {
    let p = share.join(name);
    let canon = p.canonicalize().ok()?;
    let base = share.canonicalize().ok()?;
    canon.starts_with(base).then_some(canon)
}

/// 从 canonical 路径反推共享目录内的真实相对名(统一 `/`)。
/// Windows 的尾随点/空格、8.3 短名、ADS 等别名经 canonicalize 后会落回真实文件；
/// 删除/标记/在途计数必须用真实名，否则按别名算出的 key 与 marker 查不到目标。
fn canonical_rel_name(share: &Path, path: &Path) -> Option<String> {
    let base = share.canonicalize().ok()?;
    let rel = path.strip_prefix(&base).ok()?;
    let mut s = rel.to_string_lossy().to_string();
    if cfg!(windows) {
        s = s.replace('\\', "/");
    }
    (!s.is_empty()).then_some(s)
}

/// async handler 禁止直接 canonicalize/metadata/查盘: 慢盘/NFS 会阻塞 tokio worker
async fn safe_join_existing_async(share: &Path, name: &str) -> Option<PathBuf> {
    let share = share.to_path_buf();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || safe_join_existing(&share, &name))
        .await
        .ok()
        .flatten()
}

/// 文件指纹(大小 + ETag), blocking 包装: 开票/下载校验不在 worker 上做磁盘 IO
async fn file_fingerprint_async(share: &Path, name: &str) -> Option<(u64, String)> {
    let share = share.to_path_buf();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        safe_join_existing(&share, &name)
            .and_then(|p| p.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| (m.len(), file_etag(&m)))
    })
    .await
    .ok()
    .flatten()
}

async fn available_space_async(path: PathBuf) -> Option<u64> {
    tokio::task::spawn_blocking(move || fs2::available_space(&path).ok())
        .await
        .ok()
        .flatten()
}

async fn dedup_rel_name_async(dir: PathBuf, rel: String) -> String {
    let fallback = rel.clone();
    tokio::task::spawn_blocking(move || dedup_rel_name(&dir, &rel))
        .await
        .unwrap_or(fallback)
}

/// 强 ETag: 大小 + mtime(纳秒)。同名文件被删除后重新上传会得到不同 ETag,
/// 断点续传据此判断"是否还是同一份内容", 避免把旧前缀拼到新文件上。
fn file_etag(meta: &std::fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // FAT/exFAT/SMB 的 mtime 只有 1-2s 粒度: 同大小文件在粒度内被替换时,
    // 仅 size+mtime 的 ETag 不变。Unix 再混入 inode; 其他平台混入创建时间。
    #[cfg(unix)]
    let extra = {
        use std::os::unix::fs::MetadataExt;
        format!("{:x}", meta.ino())
    };
    #[cfg(not(unix))]
    let extra = meta
        .created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| format!("{:x}", d.as_nanos()))
        .unwrap_or_else(|| "0".into());
    format!("{:x}-{:x}-{}", meta.len(), mtime, extra)
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "error": msg.into() })),
    )
        .into_response()
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

/// 静态页面/资源的安全响应头: 网页端没有 Tauri 的 CSP 注入, 这里补第二道防线 ——
/// 防 MIME 嗅探、防被任意页面 iframe 点击劫持(浏览器地址栏/局域网 IP 场景)。
fn static_headers(mime: &str, cache: &'static str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap());
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("SAMEORIGIN"),
    );
    h.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    // 浏览器版没有 Tauri 注入的 CSP, 这里补齐脚本级防线:
    // - 脚本只允许同源构建产物(无 eval/无内联);
    // - 样式允许内联(Vue 动态 style / 主题变量);
    // - 图片/媒体允许 data:/blob:(预览缩略图与下载 blob);
    // - ws: 供同源 WebSocket 事件推送; 其余外连一律禁止(更新检查走 GitHub 的
    //   connect-src 白名单, 由 Tauri 壳内的 CSP 管; 浏览器版更新检查在 fetchLatestRelease
    //   里失败即静默, 不影响主功能)。
    h.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; \
             script-src 'self'; \
             style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: blob:; \
             media-src 'self' blob:; \
             font-src 'self' data:; \
             connect-src 'self' ws: wss:; \
             object-src 'none'; \
             base-uri 'self'; \
             frame-ancestors 'self'",
        ),
    );
    h
}

async fn static_handler(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // 未知 API 路径返回 404 JSON(与 Python 版一致), 不做 SPA 兜底
    if path == "api" || path.starts_with("api/") {
        return err(StatusCode::NOT_FOUND, "Not Found");
    }
    let path = if path.is_empty() { "index.html" } else { path };
    // index.html 必须 no-store: 否则手机浏览器缓存旧版页面, 新服务端的协议头会
    // 对旧页面整链 400。构建产物(/assets/ 下文件名带内容 hash)可以长期 immutable
    // 缓存 —— iOS 二次打开/切回页面不必重新下载 300KB+ 的 JS。
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    match Frontend::get(path) {
        Some(f) => (static_headers(mime_for_path(Path::new(path)), cache), f.data).into_response(),
        None => {
            // 资源文件(带扩展名或 /assets/)缺失必须 404, 不能 SPA 兜底成 index.html:
            // 升级后旧页面/Service Worker 请求已删除的 /assets/old-hash.js 会拿到
            // text/html, <script type="module"> 解析失败 → 整页白屏, 且动态 import
            // 会被永久缓存为 rejected。无扩展名的路径才回 SPA 首页。
            let last = path.rsplit('/').next().unwrap_or("");
            if path.starts_with("assets/") || last.contains('.') {
                return err(StatusCode::NOT_FOUND, "Not Found");
            }
            match Frontend::get("index.html") {
                Some(f) => (static_headers("text/html; charset=utf-8", "no-store"), f.data).into_response(),
                None => err(StatusCode::NOT_FOUND, "前端资源未构建(先执行 npm run build)"),
            }
        }
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
    /// 上传者昵称: 本次进程内上传过的文件才有; 重启后旧文件为 None
    #[serde(skip_serializing_if = "Option::is_none")]
    uploader: Option<String>,
}

/// 递归收集文件(深度上限 8, 跳过上传断点临时文件); name 为相对路径
fn collect_files(dir: &Path, base: &Path, depth: usize, out: &mut Vec<FileEntry>, visited: &mut usize) {
    // depth 为当前目录相对层级(根=0), 其内文件路径段数 = depth+1;
    // 用 >= 而非 >: 否则 8 层目录里的文件(9 段)会进列表, 但 safe_rel_path 拒收 9 段,
    // 出现"列得出、下载/删除全 400"的自相矛盾
    if depth >= UPLOAD_MAX_DEPTH || out.len() >= FILES_LIST_MAX || *visited >= FILES_LIST_MAX * 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if out.len() >= FILES_LIST_MAX || *visited >= FILES_LIST_MAX * 4 {
            return; // 文件/目录访问都有预算: 空目录树也不能无限拖垮阻塞线程池
        }
        *visited += 1;
        let p = e.path();
        // 先看自身类型再 follow: 指向目录的符号链接若先 metadata 会递归到共享目录之外
        // (并可形成环); 列表/下载/删除的越界校验虽会拒绝，也不能把界外文件名/metadata 列出来。
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            collect_files(&p, base, depth + 1, out, visited);
            continue;
        }
        // FIFO/设备节点等非普通文件取不回也删不了，不进列表
        if !md.is_file() {
            continue;
        }
        let fname = e.file_name().to_string_lossy().to_string();
        // 用户自建的同后缀文件(内容不是系统令牌)是合法数据, 必须出现在列表里
        let user_ccup = fname.to_ascii_lowercase().ends_with(".ccup") && !is_system_ccup(&p);
        if (is_part_artifact(&p, &fname) && !user_ccup) || is_junk_file(&fname) {
            continue; // 断点续传中间文件/元数据、系统垃圾文件(.DS_Store 等)不进列表
        }
        // 正在上传的最终名文件带 .{名}.ccup 标记: 不列进列表, 避免用户下载半截
        // ("ok" 标记是已完成但标记删除失败, 仍要列出/可下载)
        if marker_is_incomplete(&p) {
            continue;
        }
        let modified = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let mut rel = p
            .strip_prefix(base)
            .unwrap_or(&p)
            .to_string_lossy()
            .to_string();
        // 仅 Windows 归一化路径分隔符; Unix 反斜杠是文件名的一部分, 不能改写
        if cfg!(windows) {
            rel = rel.replace('\\', "/");
        }
        out.push(FileEntry {
            name: rel,
            size: md.len(),
            modified,
            uploader: None,
        });
    }
}

async fn files(State(share): State<PathBuf>) -> Response {
    let Ok(_permit) = files_permits().try_acquire() else {
        return err(StatusCode::TOO_MANY_REQUESTS, "文件列表刷新过快，请稍后重试");
    };
    // 同步递归遍历放进阻塞线程: 大目录时别把 tokio worker 卡住(饿死 WS 心跳等其他任务)
    let (out, visited) = tokio::task::spawn_blocking(move || {
        let mut out: Vec<FileEntry> = Vec::new();
        let mut visited = 0usize;
        collect_files(&share, &share, 0, &mut out, &mut visited);
        // 补上传者昵称: 遍历时不加锁, 这里一次性查映射
        {
            let mut up = uploaders().lock().unwrap_or_else(|e| e.into_inner());
            // 裁剪: 外部删除/改名后条目会永久残留, 无界增长会拖慢每次列表请求
            let names: std::collections::HashSet<&str> = out.iter().map(|f| f.name.as_str()).collect();
            up.retain(|k, _| names.contains(k.as_str()));
            if up.len() > 10_000 {
                up.clear();
            }
            for f in &mut out {
                if let Some(nick) = up.get(&f.name) {
                    f.uploader = Some(nick.clone());
                }
            }
        }
        out.sort_by(|a, b| b.modified.total_cmp(&a.modified));
        (out, visited)
    })
    .await
    .unwrap_or_default();
    let truncated = out.len() >= FILES_LIST_MAX || visited >= FILES_LIST_MAX * 4;
    Json(serde_json::json!({ "files": out, "truncated": truncated })).into_response()
}

async fn download(
    State(share): State<PathBuf>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
    headers: HeaderMap,
    AxPath(raw): AxPath<String>,
) -> Response {
    use tokio::io::AsyncSeekExt;
    let peer = normalize_peer(addr.ip());
    // 并发闸: 超限立即 429, 不排队(每条下载固定占用一块流式缓冲)
    let Ok(download_permit) = download_permits().try_acquire() else {
        return err(StatusCode::TOO_MANY_REQUESTS, "下载并发过多，请稍后重试");
    };
    // 单来源并发上限 8(总闸 32): 否则一台设备慢读 32 条即可让全员 429
    let Some(download_slot) = peer_slot_try_acquire(peer, 8, PeerSlotKind::Download) else {
        return err(StatusCode::TOO_MANY_REQUESTS, "该设备下载并发过多，请稍后再试");
    };
    // 通配路由捕获值带前导 "/", 先去掉再做相对路径校验(读路径不拦保留设备名, 否则既有文件取不回)
    let Some(name) = safe_rel_path(raw.trim_start_matches('/'), false) else {
        return err(StatusCode::BAD_REQUEST, "非法的文件名");
    };
    let Some(path) = safe_join_existing_async(&share, &name).await else {
        push_log_err(format!("下载失败 {name}: 文件不存在 ← {}", peer));
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    // 标记检查 / 元数据 / 打开文件都是同步 fs 调用: 共享目录在 NFS、休眠外接盘或深目录时
    // 单次 stat/open 可能挂起数百毫秒, 直接跑在 async handler 里会卡住 tokio worker,
    // 连带冻结 WS 心跳与所有其它请求。统一放进阻塞线程。
    let open_path = path.clone();
    let opened = tokio::task::spawn_blocking(move || {
        // 内部产物/系统垃圾文件不对外: 列表里看不到, 也不能靠猜名字直接下载
        // (如 .DS_Store 会泄露目录历史、.ccup 会泄露上传会话令牌)。
        let real_base = open_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if is_junk_file(real_base) {
            return Err((StatusCode::NOT_FOUND, "文件不存在".to_string()));
        }
        if is_part_artifact(&open_path, real_base) {
            let user_ccup = real_base.to_ascii_lowercase().ends_with(".ccup")
                && !is_system_ccup(&open_path);
            if !user_ccup {
                return Err((StatusCode::NOT_FOUND, "文件不存在".to_string()));
            }
        }
        // 上传中的文件带 .{名}.ccup 标记: 拒绝下载, 否则客户端会拿到 200 的半截文件
        if marker_is_incomplete(&open_path) {
            return Err((StatusCode::CONFLICT, "文件正在上传中，请稍后再下载".to_string()));
        }
        let file = std::fs::File::open(&open_path)
            .map_err(|_| (StatusCode::NOT_FOUND, "文件不存在".to_string()))?;
        // 用打开后的句柄取元数据: stat 与 open 之间文件被替换时, 避免
        // Content-Length/ETag 来自旧文件、字节来自新文件(静默截断/拼接)。
        let meta = file
            .metadata()
            .map_err(|_| (StatusCode::NOT_FOUND, "文件不存在".to_string()))?;
        // 目录必须拒绝: File::open 对目录在 Unix 上成功, ReaderStream 读到 EISDIR
        // 中途断流, 客户端拿到的是"半截/损坏的下载"而非明确错误
        if !meta.is_file() {
            return Err((StatusCode::BAD_REQUEST, "不是文件".to_string()));
        }
        Ok((meta, file))
    })
    .await;
    let (meta, std_file) = match opened {
        Ok(Ok(v)) => v,
        Ok(Err((code, msg))) => {
            push_log_err(format!("下载失败 {name}: {msg} ← {}", peer));
            return err(code, msg);
        }
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "打开文件失败"),
    };
    let size = meta.len();
    let etag = file_etag(&meta);
    // If-Range: 客户端续传时会带首次响应的 ETag。同名文件被删除后重新上传时
    // ETag 改变 —— 此时必须忽略 Range 回 200 全量; 否则客户端把旧前缀拼到新
    // 文件的后半段上, 得到一份"HTTP 层完全合法"的损坏文件。
    let if_range_ok = headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().trim_matches('"') == etag)
        .unwrap_or(true);
    let range_header = if if_range_ok {
        headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("bytes="))
    } else {
        None
    };
    // Range 支持(单区间): 浏览器断下重试会带 Range: bytes=N- , 不支持的话 10GB 传到
    // 8GB 断掉只能从零重来; 服务端回 206 + Content-Range, Accept-Ranges 声明能力
    let mut start: u64 = 0;
    let mut end_excl: u64 = size;
    let is_partial = if let Some(rv) = range_header {
        // 单区间解析: Ok(true)=有效区间(206) / Ok(false)=畸形, 忽略走整体 200 / Err=超出文件(416)
        let parsed: Result<bool, ()> = (|| {
            let (a, b) = rv.split_once('-').ok_or(())?;
            let s = a.trim();
            let e = b.trim();
            if s.is_empty() {
                // 后缀区间 bytes=-N(取末尾 N 字节): 浏览器/播放器读取 MP4 moov 等
                // 场景会用到; 不支持时会退化成 200 全量, 浪费带宽且拖慢起播。
                let Ok(suffix) = e.parse::<u64>() else { return Ok(false) };
                if suffix == 0 || size == 0 {
                    return Err(());
                }
                start = size.saturating_sub(suffix);
                end_excl = size;
                return Ok(true);
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
    // 全量也必须 take(size): 打开后文件仍可能被上传继续写大, 不截断会让响应体
    // 超过已声明的 Content-Length, 破坏 HTTP 分帧
    let bounded = tokio::io::AsyncReadExt::take(tokio_file, if is_partial { end_excl - start } else { size });
    let reader = tokio::io::BufReader::with_capacity(CHUNK_HINT, bounded);
    // 下载空闲/慢读看门狗: 上传侧有 120s 空闲闸, 下载侧旧实现完全没有 —— 慢读或
    // 干脆不读的客户端会让 hyper 停止轮询响应体, 许可与单来源槽位被永久占住。
    // 每块数据产出时累加进度; 独立任务每 5s 检查, 45s 无进展(或总时长超 2h)就
    // abort 流(AbortHandle 会唤醒并结束响应体), 随流 drop 归还 permit/槽位。
    use tokio::io::AsyncReadExt;
    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let alive_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let (abort_tx, abort_rx) = futures_util::future::AbortHandle::new_pair();
    let base = futures_util::stream::unfold(
        (reader, DownloadAlive { alive: alive.clone(), notify: alive_notify.clone() }, false),
        {
            let progress = progress.clone();
            move |(mut r, guard, done)| {
                let progress = progress.clone();
                async move {
                    if done {
                        return None;
                    }
                    let mut buf = vec![0u8; 64 * 1024];
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(45),
                        r.read(&mut buf),
                    )
                    .await
                    {
                        Ok(Ok(0)) => None,
                        Ok(Ok(n)) => {
                            progress.fetch_add(n as u64, Ordering::Relaxed);
                            buf.truncate(n);
                            Some((Ok(axum::body::Bytes::from(buf)), (r, guard, false)))
                        }
                        Ok(Err(e)) => Some((Err(e), (r, guard, true))),
                        Err(_) => Some((
                            Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "download idle timeout",
                            )),
                            (r, guard, true),
                        )),
                    }
                }
            }
        },
    );
    {
        let progress = progress.clone();
        let alive = alive.clone();
        let alive_notify = alive_notify.clone();
        // 许可所有权放在看门狗而不是响应流里: 客户端不读响应时 hyper 可能不再 poll body,
        // 流不会被 drop, 许可会永久泄漏。看门狗超时无条件 abort；即便流从未被 poll,
        // 这里 break 也会 drop permit/slot，恢复下载容量。
        tokio::spawn(async move {
            let _permit = download_permit;
            let _slot = download_slot;
            let started = std::time::Instant::now();
            let mut last_bytes = 0u64;
            let mut last_at = std::time::Instant::now();
            loop {
                tokio::select! {
                    _ = alive_notify.notified() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                }
                if alive.load(Ordering::Relaxed) {
                    break;
                }
                let cur = progress.load(Ordering::Relaxed);
                if cur != last_bytes {
                    last_bytes = cur;
                    last_at = std::time::Instant::now();
                } else if last_at.elapsed() >= std::time::Duration::from_secs(45)
                    || started.elapsed() >= std::time::Duration::from_secs(2 * 3600)
                {
                    abort_tx.abort();
                    break;
                }
            }
        });
    }
    let stream = futures_util::future::Abortable::new(base, abort_rx);
    if log_admit(peer) {
        push_log(format!(
            "下载 {name} ({}) ← {}{}",
            humans(if is_partial { end_excl - start } else { size }),
            peer,
            if is_partial { format!(" · 续传自 {start}") } else { String::new() }
        ));
    }
    let mime = mime_for_path(&path);
    // filename* 只能是文件名(不能带子目录), 且必须做完整百分号编码:
    // 旧实现用自定义 set 漏了 `/`、`'`, 浏览器会把 RFC 5987 值判为非法并忽略,
    // 分享链接直接下载时保存的文件名可能变成 URL 末段或乱码。
    let base_name = name.rsplit('/').next().unwrap_or(&name);
    let enc = percent_encoding::utf8_percent_encode(base_name, percent_encoding::NON_ALPHANUMERIC);
    let mut rh = HeaderMap::new();
    rh.insert(header::CONTENT_TYPE, mime.parse().unwrap());
    rh.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    rh.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{etag}\"")).unwrap(),
    );
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
    // inline=1 供浏览器预览(图片/PDF/文本/音视频)。只对安全类型放行:
    // HTML/SVG/XML 若以 inline 打开会运行在服务同源上, 可直接调用删除/控制接口
    let inline = q.get("inline").map(|v| v == "1").unwrap_or(false) && is_inline_safe(&name);
    let disp = if inline { "inline" } else { "attachment" };
    rh.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("{disp}; filename*=UTF-8''{enc}")).unwrap(),
    );
    rh.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    // 媒体 URL 带短时媒体会话令牌: 禁止通过 Referer 把完整 URL 带到外链
    rh.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
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
    // 并发闸: 超限立即 429, 不排队(排队会放大内存/连接占用)
    let Ok(_upload_permit) = upload_permits().try_acquire() else {
        return err(StatusCode::TOO_MANY_REQUESTS, "上传并发过多，请稍后重试");
    };
    // 单来源最多占 4 条(总闸 16): 一台设备不能凭慢速上传饿死整个局域网
    let Some(_peer_slot) = peer_slot_try_acquire(peer, 4, PeerSlotKind::Upload) else {
        return err(StatusCode::TOO_MANY_REQUESTS, "该设备上传并发过多，请稍后再试");
    };
    let raw = headers
        .get("X-Filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // 上传者昵称(可选头, URL 编码): 供其他端的时间线文件卡片显示头像
    let uploader = headers
        .get("X-Nickname")
        .and_then(|v| v.to_str().ok())
        .map(|s| percent_decode_str(s).decode_utf8_lossy().to_string())
        .unwrap_or_default()
        .trim()
        .chars()
        .filter(|c| *c != '\0')
        .take(NICK_MAX_CHARS)
        .collect::<String>();
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
    // 完整性判据必须来自一个可信的声明大小: X-File-Size(前端) 或 Content-Length。
    // 两者都没有时不能按“未知大小”接收 —— 干净结束的截断请求体会被当作完整文件回 200。
    let header_size = match headers.get("X-File-Size").and_then(|v| v.to_str().ok()) {
        Some(s) => match s.trim().parse::<u64>() {
            Ok(v) => Some(v),
            Err(_) => return err(StatusCode::BAD_REQUEST, "X-File-Size 格式不正确"),
        },
        None => None,
    };
    let content_length = match headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()) {
        Some(s) => match s.trim().parse::<u64>() {
            Ok(v) => Some(v),
            Err(_) => None,
        },
        None => None,
    };
    let size = match (header_size, content_length) {
        (Some(a), Some(b)) if a != b => {
            return err(StatusCode::BAD_REQUEST, "X-File-Size 与 Content-Length 不一致");
        }
        (Some(a), _) => a,
        (None, Some(b)) => b,
        (None, None) => {
            return err(
                StatusCode::LENGTH_REQUIRED,
                "缺少文件大小（需要 X-File-Size 或 Content-Length）",
            )
        }
    };
    if size > 0 {
        if let Some(free) = available_space_async(share.clone()).await {
            // 额外保留 256MB 安全余量: 不让一次上传把系统盘写到几近枯竭
            let need = size.saturating_add(UPLOAD_FREE_RESERVE_BYTES);
            if need > free {
                push_log_warn(format!(
                    "上传拒绝 {name}: 磁盘空间不足(需要 {} + 保留 {}，可用 {}) ← {peer}",
                    humans(size),
                    humans(UPLOAD_FREE_RESERVE_BYTES),
                    humans(free)
                ));
                return err(
                    StatusCode::BAD_REQUEST,
                    format!("磁盘空间不足，需要 {}，可用 {}", humans(need), humans(free)),
                );
            }
        }
    }
    let path = share.join(&name); // safe_rel_path 逐段已校验, 不会越界
    // 越界预检必须在建目录之前: create_dir_all 会跟随符号链接, 先建后查会把目录建到界外。
    // 对"最近存在的祖先"做 canonical 校验(其下若链向界外, 在此拦截, 且未创建任何东西)
    if let Some(parent) = path.parent() {
        let mut probe = parent.to_path_buf();
        // 必须走 tokio::fs: 同步 exists() 在 NFS/SMB/休眠外接盘上会卡住整个
        // tokio worker(16 个并发上传即可耗尽线程池), 这是可从 LAN 触发的 DoS。
        while tokio::fs::metadata(&probe).await.is_err() {
            match probe.parent() {
                Some(p) => probe = p.to_path_buf(),
                None => break,
            }
        }
        let (Ok(base), Ok(probe_canon)) = (
            tokio::fs::canonicalize(&share).await,
            tokio::fs::canonicalize(&probe).await,
        ) else {
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
        match (
            tokio::fs::canonicalize(parent).await,
            tokio::fs::canonicalize(&share).await,
        ) {
            (Ok(parent_canon), Ok(base)) if parent_canon.starts_with(&base) => {}
            (Ok(_), Ok(_)) => {
                let _ = tokio::fs::remove_dir(parent).await;
                return err(StatusCode::BAD_REQUEST, "路径越界");
            }
            _ => return err(StatusCode::INTERNAL_SERVER_ERROR, "路径校验失败"),
        }
    }
    // 停服进行中: 不再受理新上传(旧监听在优雅关闭窗口内仍可能收到请求)
    if STOPPING.load(Ordering::SeqCst) {
        return err(StatusCode::SERVICE_UNAVAILABLE, "服务正在停止，请稍后重试");
    }
    // 同名自动改名(1.1.5/1.1.6 策略恢复): 绝不覆盖已有文件 —— create_new 原子占位,
    // 并发同名被抢先则 dedup_rel_name 顺延 (1)(2)… 重试, 无 check-then-create 竞态
    let mut final_name = name.clone();
    let token = new_upload_token();
    // 占位 → 标记 → 在途登记 三段必须在同一临界区内完成(见 file_op_lock 注释):
    // 否则并发 DELETE 可在窗口内删掉成品, 上传方仍写完并回 200(假成功)
    let op_guard = file_op_lock().lock().await;
    // 等待锁期间服务可能已开始/完成停机, 且 stop() 可能已把 STOPPING 复位;
    // 这里再查一次, 避免在"启动清理已跑完"之后凭空建出一个没人清理的占位文件。
    if STOPPING.load(Ordering::SeqCst) {
        return err(StatusCode::SERVICE_UNAVAILABLE, "服务正在停止，请稍后重试");
    }
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
                let next = dedup_rel_name_async(share.clone(), name.clone()).await;
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
    // 凭标记识别半截成品并连标记一并删除(否则截断文件以真实文件名永久残留、与完整文件无法区分)。
    // 首行 = 会话令牌(abort 只清理自己的产物), 第二行 = 声明大小(崩溃时据此保留已完整落盘的成品)
    let base = final_name.rsplit('/').next().unwrap_or(final_name.as_str());
    // marker 比成品名多 6 个字符(".{名}.ccup"): 预检必须算进去, 否则接近 NAME_MAX
    // 的合法文件名会卡在 marker 创建失败, 跨平台永远传不上去。
    if base.len() + 6 > 249 {
        let _ = tokio::fs::remove_file(&path).await;
        push_log_warn(format!("上传拒绝 {final_name}: 文件名过长(含 .ccup 标记超出上限)"));
        return err(StatusCode::BAD_REQUEST, "文件名过长（含上传标记超出系统上限），请缩短文件名");
    }
    let Some(dir) = path.parent() else {
        let _ = tokio::fs::remove_file(&path).await;
        return err(StatusCode::INTERNAL_SERVER_ERROR, "路径异常");
    };
    let marker = dir.join(format!(".{base}.ccup"));
    // 同名 .ccup 若不是系统标记(用户自己的文件), 绝不能覆盖删除;
    // 空内容的标记视为崩溃残留, 允许回收后重建。
    // 读文件必须离开 async worker: 共享目录可能是 NFS/SMB, 慢盘会卡死整个服务。
    let marker_prefix = {
        let probe_marker = marker.clone();
        tokio::task::spawn_blocking(move || read_ccup_prefix(&probe_marker, 256))
            .await
            .ok()
            .flatten()
    };
    if let Some(c) = marker_prefix {
        let t = c.lines().next().unwrap_or("").trim().to_string();
        if t.is_empty() {
            let _ = tokio::fs::remove_file(&marker).await;
        } else if t != "ok" && !is_upload_token(&t) {
            let _ = tokio::fs::remove_file(&path).await;
            return err(StatusCode::CONFLICT, "同名 .ccup 文件已存在，请先清理或改名后重试");
        }
    }
    let marker_body = if size > 0 { format!("{token}\n{size}") } else { token.clone() };
    let marker_result = async {
        match tokio::fs::OpenOptions::new().create_new(true).write(true).open(&marker).await {
            Ok(mut mf) => {
                use tokio::io::AsyncWriteExt;
                mf.write_all(marker_body.as_bytes()).await
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // 陈旧标记(正常应被启动清理移除): 清掉重建, 保证本次上传受保护
                let _ = tokio::fs::remove_file(&marker).await;
                match tokio::fs::OpenOptions::new().create_new(true).write(true).open(&marker).await {
                    Ok(mut mf) => {
                        use tokio::io::AsyncWriteExt;
                        mf.write_all(marker_body.as_bytes()).await
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }
    .await;
    if let Err(e) = marker_result {
        let _ = tokio::fs::remove_file(&marker).await;
        let _ = tokio::fs::remove_file(&path).await; // 回收占位, 不留无标记空文件
        push_log_err(format!("上传 {final_name}: 标记创建失败({e}) ← {peer}"));
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
    }
    // 在途计数(#2 数据一致性): delete_file 见 key 或标记即 409 —— 消除"删在途文件 →
    // 上传方收 200 但字节已被移走"的假成功竞态
    let akey = active_key(&path);
    *active_uploads()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(akey.clone())
        .or_insert(0) += 1;
    drop(op_guard);
    // 正常成功/显式失败路径之外的“future 被 drop”兜底; 绑定必须活到函数结束
    let mut upload_cleanup = UploadCleanup {
        path: path.clone(),
        marker: marker.clone(),
        token: token.clone(),
        key: akey.clone(),
        armed: true,
    };
    if log_admit(peer) {
        push_log(format!(
            "上传开始 {name} (声明 {}) ← {peer}{}",
            if size > 0 { humans(size) } else { "未知大小".into() },
            if final_name != name {
                format!(" · 已有同名，将改存为 {final_name}")
            } else {
                String::new()
            }
        ));
    }
    let started = std::time::Instant::now();
    let mut kick = ws_kick().subscribe();   // 停服信号: 立刻中止在途上传并清理
    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;
    let mut last_check: u64 = 0;
    let mut rate_at = started;
    let mut rate_bytes: u64 = 0;
    loop {
        // 停服: 先于读循环判断, 保证 stop() 能在数秒内回收全部在途上传,
        // 否则 start() 的启动清理会在旧 handler 仍在写盘时删掉"在途文件"
        if STOPPING.load(Ordering::SeqCst) {
            drop(file);
            upload_cleanup.armed = false;
            abort_upload(&path, &marker, &token, &akey).await;
            push_log_warn(format!("上传中止 {name}: 服务停止，已清理半截文件 ← {peer}"));
            return err(StatusCode::SERVICE_UNAVAILABLE, "服务正在停止，上传已中止");
        }
        // 总时长上限: 空闲超时挡不住"每 119s 滴一个字节"的慢速滴流
        if started.elapsed().as_secs() > UPLOAD_MAX_DURATION_SECS {
            drop(file);
            upload_cleanup.armed = false;
            abort_upload(&path, &marker, &token, &akey).await;
            push_log_warn(format!(
                "上传中止 {name}: 超过总时长上限({}s)，已清理半截文件 ← {peer}",
                UPLOAD_MAX_DURATION_SECS
            ));
            return err(StatusCode::REQUEST_TIMEOUT, "上传超时，请重试");
        }
        // 空闲超时(审查回归): 半开连接(信号黑洞/锁屏切换无 RST)会让读循环永久悬挂 ——
        // 120s 无任何字节即中止并清理, 同时限定半截文件可见/可下载的窗口。
        // 与停服信号 select: 客户端不再发数据时也能被 stop() 立即打断
        let chunk = tokio::select! {
            _ = kick.recv() => {
                drop(file);
                upload_cleanup.armed = false;
                abort_upload(&path, &marker, &token, &akey).await;
                push_log_warn(format!("上传中止 {name}: 服务停止，已清理半截文件 ← {peer}"));
                return err(StatusCode::SERVICE_UNAVAILABLE, "服务正在停止，上传已中止");
            }
            r = tokio::time::timeout(
                std::time::Duration::from_secs(UPLOAD_IDLE_TIMEOUT_SECS),
                stream.next(),
            ) => match r {
                Ok(c) => c,
                Err(_) => {
                    drop(file);
                    upload_cleanup.armed = false;
                    abort_upload(&path, &marker, &token, &akey).await;
                    push_log_warn(format!(
                        "上传中止 {name}: 空闲超时({}s 无数据)，已清理半截文件 ← {peer}",
                        UPLOAD_IDLE_TIMEOUT_SECS
                    ));
                    return err(StatusCode::GATEWAY_TIMEOUT, "上传超时，请重试");
                }
            },
        };
        let Some(c) = chunk else {
            break; // body 正常结束
        };
        let Ok(c) = c else {
            drop(file);
            upload_cleanup.armed = false;
            abort_upload(&path, &marker, &token, &akey).await;
            push_log_err(format!("上传中断 {name}: 连接断开，已清理半截文件 ← {peer}"));
            return err(StatusCode::INTERNAL_SERVER_ERROR, "上传中断");
        };
        // 周期查盘(审查回归): size=0 与谎报 size 的请求体绕过了启动预检,
        // 每 64MB 复查一次, 低于安全余量即中止 —— 防灌满共享盘
        if written - last_check >= UPLOAD_FREE_CHECK_BYTES {
            if let Some(free) = available_space_async(share.clone()).await {
                if free < UPLOAD_FREE_RESERVE_BYTES {
                    drop(file);
                    upload_cleanup.armed = false;
                    abort_upload(&path, &marker, &token, &akey).await;
                    push_log_warn(format!(
                        "上传中止 {name}: 磁盘剩余不足，已写入 {}，已清理半截文件 ← {peer}",
                        humans(written)
                    ));
                    return err(StatusCode::INSUFFICIENT_STORAGE, "磁盘空间不足，上传已中止");
                }
            }
            last_check = written;
        }
        // 声明大小是硬上限: 客户端多送字节时必须立即中止，不能先把超出部分写盘、
        // 最后再以"声明/实际不符"拒绝 —— 否则谎报小 size 可绕过启动预检灌盘。
        if size > 0 && written.saturating_add(c.len() as u64) > size {
            drop(file);
            upload_cleanup.armed = false;
            abort_upload(&path, &marker, &token, &akey).await;
            push_log_warn(format!(
                "上传中止 {name}: 实际内容超过声明大小 {size} 字节，已清理半截文件 ← {peer}"
            ));
            return err(StatusCode::PAYLOAD_TOO_LARGE, "上传内容超过声明大小");
        }
        if let Err(e) = file.write_all(&c).await {
            drop(file);
            upload_cleanup.armed = false;
            abort_upload(&path, &marker, &token, &akey).await;
            push_log_err(format!("上传失败 {name}: 写入失败({e})，已清理半截文件 ← {peer}"));
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}"));
        }
        written += c.len() as u64;
        // 低速闸: 空闲超时挡不住"每 119s 滴一个字节"的连接, 16 个槽位可被占满数小时。
        // 连续 60s 增量不足 16KB 即视为占槽不传, 中止并清理。
        if rate_at.elapsed().as_secs() >= 60 {
            if written.saturating_sub(rate_bytes) < 16 * 1024 {
                drop(file);
                upload_cleanup.armed = false;
                abort_upload(&path, &marker, &token, &akey).await;
                push_log_warn(format!(
                    "上传中止 {name}: 速率过低(60s < 16KB)，已清理半截文件 ← {peer}"
                ));
                return err(StatusCode::REQUEST_TIMEOUT, "上传速率过低，请检查网络后重试");
            }
            rate_at = std::time::Instant::now();
            rate_bytes = written;
        }
    }
    // 刷盘失败不能回 200: 原实现吞掉错误, 断电/坏盘时调用方会以为文件已安全落盘
    if let Err(e) = file.sync_all().await {
        drop(file);
        upload_cleanup.armed = false;
        abort_upload(&path, &marker, &token, &akey).await;
        push_log_err(format!("上传失败 {name}: 落盘失败({e})，已清理半截文件 ← {peer}"));
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("落盘失败: {e}"));
    }
    drop(file);   // Windows 上先关句柄, 标记/成品才可能删除成功
    // 声明了大小就必须收满: 分片/提前"正常收尾"的截断体不能当成功 ——
    // 否则标记被当"已完成"移除, 落盘即静默损坏文件, 对端拿到 Content-Length 自洽的半截
    if size > 0 && written != size {
        upload_cleanup.armed = false;
        abort_upload(&path, &marker, &token, &akey).await;
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
    // 改写入 "ok", 启动清理见完成标记只删标记本身。
    // 必须先校验 marker 仍属于本会话: 成品被外部删除/改名后, 同名新上传可能已
    // 把 marker 换成 tokenB —— 无条件删除会拆掉新上传的保护, 让它失败时留下
    // 无标记的半截文件(列表/下载都当完整文件)。
    {
        let _op = file_op_lock().lock().await;
        let owned = tokio::fs::read(&marker)
            .await
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    == token
            })
            .unwrap_or(false);
        if owned {
            if tokio::fs::remove_file(&marker).await.is_err() {
                let _ = tokio::fs::write(&marker, b"ok").await;
            }
        }
    }
    upload_cleanup.armed = false;   // 成功路径已自行释放计数, 防止 Drop 二次递减
    drop_active(&akey);
    if log_admit(peer) {
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
    }
    if !uploader.is_empty() {
        let mut up = uploaders().lock().unwrap_or_else(|e| e.into_inner());
        if up.len() >= 10_000 {
            up.clear();   // 防无界增长: 极端批量上传时整体重置, 旧文件回落通用头像
        }
        up.insert(final_name.clone(), uploader.clone());
    }
    // 文件即消息: 持久化一条投递事件(独立上限, 不占文本聊天额度), 刷新/重启后
    // 时间线仍能看到发送过的文件; 内部会广播 chat:new 让所有端刷新。
    record_file_event(final_name.clone(), written, uploader.clone());
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
    let name_for_log = name.clone();
    // 先在 active 锁内做一次短判定; 文件 IO 不再持锁, 避免慢盘 metadata/remove
    // 把上传 handler 的 active 计数也卡在 std Mutex 上。
    let pending = {
        let m = active_uploads().lock().unwrap_or_else(|e| e.into_inner());
        if m.get(&key).copied().unwrap_or(0) > 0 {
            cancel_requests()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key);
            true
        } else {
            false
        }
    };
    if pending {
        return Json(serde_json::json!({ "ok": true, "removed": false, "pending": true }))
            .into_response();
    }
    let removed = tokio::task::spawn_blocking(move || {
        let mut removed = false;
        if std::fs::metadata(&part).is_ok() {
            removed |= std::fs::remove_file(&part).is_ok();
        }
        if std::fs::metadata(&meta).is_ok() {
            removed |= std::fs::remove_file(&meta).is_ok();
        }
        removed
    })
    .await
    .unwrap_or(false);
    if removed {
        push_log(format!("取消上传并清理断点 {name_for_log}"));
    }
    Json(serde_json::json!({ "ok": true, "removed": removed })).into_response()
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
    let Some(path) = safe_join_existing_async(&share, &name).await else {
        return err(StatusCode::NOT_FOUND, "文件不存在");
    };
    // 用 canonical 真实名做保护检查: Windows 的 8.3 短名等别名 canonicalize 后
    // 仍会落回真实路径, 按请求名算 key/marker 会漏检, 导致删掉在途文件/完整性标记。
    let resolved_name = {
        let share2 = share.clone();
        let path2 = path.clone();
        tokio::task::spawn_blocking(move || canonical_rel_name(&share2, &path2))
            .await
            .ok()
            .flatten()
    }
    .unwrap_or_else(|| name.clone());
    // 复查与删除必须与上传的"占位→标记→登记"临界区互斥: 否则检查通过后上传刚完成登记,
    // 删除仍会把在途文件删掉, 上传方收到 200 但字节已消失(TOCTOU 假成功)
    let op_guard = file_op_lock().lock().await;
    // 在途上传保护(#2): 直写最终名期间文件已存在, 无防护的删除会让上传方
    // "收到 200 但字节已全部消失"; key 与 upload 的占位路径同构(share.join(名))
    {
        let m = active_uploads().lock().unwrap_or_else(|e| e.into_inner());
        if m.get(&active_key(&share.join(&resolved_name))).copied().unwrap_or(0) > 0 {
            return err(StatusCode::CONFLICT, "该文件正在上传中，请等上传完成后再删除");
        }
    }
    // 标记检查涉及共享目录上的文件读取(可能是 NFS/休眠盘): 放进阻塞线程,
    // 否则一次 DELETE 就能卡住一个 tokio worker, 连带冻结 WS 心跳与所有 API。
    let (system_ccup, marker_token, marker_path) = {
        let share2 = share.clone();
        let name2 = resolved_name.clone();
        tokio::task::spawn_blocking(move || {
            let base = name2.rsplit('/').next().unwrap_or("").to_string();
            let is_ccup = base.to_ascii_lowercase().ends_with(".ccup");
            let system_ccup = is_ccup && is_system_ccup(&share2.join(&name2));
            let (token, path) = match share2.join(&name2).parent() {
                Some(parent) => {
                    let marker = parent.join(format!(".{base}.ccup"));
                    let tok = read_ccup_prefix(&marker, 256)
                        .and_then(|c| c.lines().next().map(|t| t.trim().to_string()));
                    (tok, Some(marker))
                }
                None => (None, None),
            };
            (system_ccup, token, path)
        })
        .await
        .unwrap_or((false, None, None))
    };
    // 系统上传标记不是用户数据: 删掉它会解除在途上传的保护, 半截成品随后会被
    // 当成完整文件列出/下载。用户自建的 .ccup(内容不是系统令牌)仍允许删除。
    if system_ccup {
        return err(StatusCode::CONFLICT, "上传完整性标记不可删除");
    }
    // 只有"系统上传令牌"才代表正在写入。上传成功但标记删除失败会留下 "ok",
    // 用户自建的同名 .ccup 也不属于上传中 —— 二者都不应把文件永久锁成 409。
    if let Some(t) = marker_token {
        if is_upload_token(&t) {
            return err(StatusCode::CONFLICT, "该文件正在上传中，请等上传完成后再删除");
        }
        if t == "ok" {
            if let Some(marker) = marker_path {
                let _ = tokio::fs::remove_file(marker).await;   // 完成标记残留: 顺手回收
            }
        }
    }
    // 符号链接只删链接本身: safe_join_existing 返回的是 canonical 目标路径,
    // 直接 remove 会删掉共享目录里的另一个真实文件。
    let raw_path = share.join(&name);
    let is_link = tokio::fs::symlink_metadata(&raw_path)
        .await
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    let victim = if is_link { &raw_path } else { &path };
    let res = match tokio::fs::remove_file(victim).await {
        Ok(()) => {
            // 已发出的分享票据立即作废: 文件没了, 链接不能继续悬空授权
            share_tickets()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|_, t| t.name != name && t.name != resolved_name);
            recent_tickets()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|_, r| r.name != name && r.name != resolved_name);
            if log_admit(peer) {
                push_log(format!(
                    "删除 {name}{} ← {}",
                    if resolved_name != name { format!("（实际 {resolved_name}）") } else { String::new() },
                    peer
                ));
            }
            uploaders().lock().unwrap_or_else(|e| e.into_inner()).remove(&name);
            if resolved_name != name {
                uploaders().lock().unwrap_or_else(|e| e.into_inner()).remove(&resolved_name);
            }
            // 文件删除后其时间线卡片一并消失: 清掉持久化的文件事件并通知各端重拉
            {
                let mut ev = file_events().lock().unwrap_or_else(|e| e.into_inner());
                let before = ev.len();
                ev.retain(|e| e.name != name && e.name != resolved_name);
                if ev.len() != before {
                    drop(ev);
                    schedule_file_events_save();
                    broadcast_event("chat:new");
                }
            }
            broadcast_event("files:changed");
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(e) => {
            push_log_err(format!("删除失败 {name}: {e} ← {}", peer));
            err(StatusCode::INTERNAL_SERVER_ERROR, format!("删除失败: {e}"))
        }
    };
    drop(op_guard);
    res
}

/// 聊天列表的借用视图: 直接序列化 `&ChatMsg`, 不再逐条 clone 成
/// `serde_json::Value`。历史上限 200 条 / 200 万字符, 旧实现会在锁内
/// 复制一遍全部字符串再构造 Value 树, 多客户端并发拉取时峰值内存翻倍。
#[derive(serde::Serialize)]
struct ChatList<'a> {
    messages: Vec<&'a ChatMsg>,
    /// 文件投递事件(独立上限 200 条): 与文本消息一起下发, 客户端渲染为文件卡片
    files: Vec<FileEvent>,
}

async fn chat_get() -> Response {
    let Ok(_permit) = chat_read_permits().try_acquire() else {
        return err(StatusCode::TOO_MANY_REQUESTS, "聊天历史读取并发过多，请稍后重试");
    };
    let q = chat_log().lock().unwrap_or_else(|e| e.into_inner());
    let files = file_events()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    // ip 仅进服务端日志, 不随接口下发 —— 局域网内没必要让所有客户端互相看到地址
    let body = match serde_json::to_vec(&ChatList { messages: q.iter().collect(), files }) {
        Ok(b) => b,
        Err(e) => {
            push_log_err(format!("聊天历史序列化失败: {e}"));
            return err(StatusCode::INTERNAL_SERVER_ERROR, "聊天历史序列化失败");
        }
    };
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

#[derive(serde::Deserialize)]
struct ChatIn {
    nickname: Option<String>,
    message: String,
    /// 引用回复: 客户端传被引用消息的快照; 字段全部可选, 旧客户端不传即普通消息
    #[serde(default)]
    quote: Option<ChatQuoteIn>,
}

#[derive(serde::Deserialize)]
struct ChatQuoteIn {
    id: Option<u64>,
    nickname: Option<String>,
    message: Option<String>,
}

async fn chat_post(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    Json(body): Json<ChatIn>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    let Ok(_permit) = chat_write_permits().try_acquire() else {
        return err(StatusCode::TOO_MANY_REQUESTS, "消息发送并发过多，请稍后重试");
    };
    if !chat_admit(peer) {
        return err(StatusCode::TOO_MANY_REQUESTS, "消息发送过于频繁，请稍后再试");
    }
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
    // 超限必须明确拒绝: 旧实现静默截断后仍回 ok, 发送方以为已发出、接收方只看到前半段。
    let text_len = text.chars().count();
    if text_len > MSG_MAX_CHARS {
        return err(
            StatusCode::BAD_REQUEST,
            format!(
                "消息超过 {MSG_MAX_CHARS} 字符上限（当前 {text_len}），请删减后再发送"
            ),
        );
    }
    let message = text;
    // 引用快照同样消毒/截断: 内容来自客户端, 不能信任其长度
    let quote = body.quote.and_then(|q| {
        let qtext = q.message.unwrap_or_default().trim().chars()
            .filter(|c| *c != '\0').take(MSG_MAX_CHARS).collect::<String>();
        if qtext.is_empty() { return None; }
        let qnick = q.nickname.unwrap_or_default().trim().chars()
            .filter(|c| *c != '\0').take(NICK_MAX_CHARS).collect::<String>();
        Some(ChatQuote {
            id: q.id.unwrap_or(0),
            nickname: if qnick.is_empty() { DEFAULT_NICK.into() } else { qnick },
            message: qtext,
        })
    });
    // id 分配与入队必须在同一临界区: 分两段加锁时并发发送会算出重复 id,
    // 前端 :key="m.id" 出现重复键导致列表渲染错乱。
    // 锁内只做内存序列化, 落盘交给 schedule_chat_save 的阻塞线程(见函数注释)。
    let (msg, save) = {
        let mut q = chat_log().lock().unwrap_or_else(|e| e.into_inner());
        let id = next_chat_id();
        let msg = ChatMsg {
            id,
            nickname,
            message,
            timestamp: now_f64(),
            ip: peer.to_string(),
            quote,
        };
        q.push_back(msg.clone());
        // 条数 + 总字符双上限: 20 万字 × 200 条可达数千万字符, 会拖垮内存、历史落盘
        // 与全量拉取。正常 2 千字消息 200 条约 40 万字符, 不受影响。
        let mut total = chat_history_chars(&q);
        while q.len() > CHAT_MAX || total > CHAT_TOTAL_MAX_CHARS {
            let Some(old) = q.pop_front() else { break };
            total = total.saturating_sub(chat_msg_chars(&old));
        }
        (msg, chat_history_json(&q))
    };
    schedule_chat_save(save);
    let id = msg.id;
    let preview: String = msg.message.chars().take(CHAT_PREVIEW_CHARS).collect();
    let dots = if msg.message.chars().count() > CHAT_PREVIEW_CHARS { "…" } else { "" };
    push_log(format!("聊天 {}: {preview}{dots} ← {}", msg.nickname, peer));
    broadcast_event("chat:new");
    Json(serde_json::json!({ "ok": true, "id": id })).into_response()
}

/// 删除单条聊天消息(局域网可信模型: 任何接入设备都可删)。
/// 广播 chat:changed 让所有在线端重拉; 轮询端下一次 loadChat 自然收敛。
async fn chat_delete(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    // 删除同样会重写整段历史: 不限流时可用 DELETE 洪水持续消耗磁盘 IO
    if !chat_del_admit(peer) {
        return err(StatusCode::TOO_MANY_REQUESTS, "删除操作过于频繁，请稍后再试");
    }
    let (removed, save) = {
        let mut q = chat_log().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = q.iter().position(|m| m.id == id) {
            q.remove(pos);
            (true, chat_history_json(&q))
        } else {
            (false, None)
        }
    };
    if removed {
        schedule_chat_save(save);
    }
    if !removed {
        return err(StatusCode::NOT_FOUND, "消息不存在");
    }
    push_log(format!("聊天消息已删除 id={id}"));
    broadcast_event("chat:changed");
    Json(serde_json::json!({ "ok": true })).into_response()
}

// ---------------------------------------------------------------- //
// 共享剪贴板: 同一服务下的所有设备共用一格文本
//
// 局域网页面通常走 http://IP (非安全上下文), 浏览器会禁用 navigator.clipboard,
// 因此不能做"自动同步系统剪贴板"; 这里提供的是显式共享板:
//   任一端把文本贴进面板 → 服务端保存并广播 → 其他端一键复制(execCommand 兜底)。
// ---------------------------------------------------------------- //

const CLIP_MAX_CHARS: usize = 20_000;

#[derive(Clone, Serialize, Deserialize, Default)]
struct SharedClip {
    text: String,
    updated_at: f64,
    from: String,
}

fn shared_clip_file() -> Option<PathBuf> {
    // 单测共享进程/真实用户目录: 测试不落盘
    if cfg!(test) { return None; }
    ui_pref_dir().map(|d| d.join("shared-clipboard.json"))
}

fn shared_clip() -> &'static Mutex<SharedClip> {
    static CLIP: OnceLock<Mutex<SharedClip>> = OnceLock::new();
    CLIP.get_or_init(|| {
        let saved = shared_clip_file()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<SharedClip>(&s).ok())
            .unwrap_or_default();
        Mutex::new(saved)
    })
}

/// 剪贴板落盘串行化(与聊天历史同思路): 锁内只序列化, 慢 IO 走阻塞线程;
/// 序号保证迟到的旧快照不会覆盖新内容。
static CLIP_SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CLIP_SAVED_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn clip_save_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}
fn schedule_clip_save(json: Option<Vec<u8>>) {
    let (Some(path), Some(json)) = (shared_clip_file(), json) else { return };
    let seq = CLIP_SAVE_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let task = move || {
        let _g = clip_save_lock().lock().unwrap_or_else(|e| e.into_inner());
        if seq <= CLIP_SAVED_SEQ.load(Ordering::SeqCst) {
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = write_secret_atomic(&path, &json);
        CLIP_SAVED_SEQ.store(seq, Ordering::SeqCst);
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let _ = handle.spawn_blocking(task);
    } else {
        task();
    }
}

async fn clipboard_get() -> Response {
    let c = shared_clip().lock().unwrap_or_else(|e| e.into_inner()).clone();
    Json(serde_json::json!({
        "text": c.text,
        "updatedAt": c.updated_at,
        "from": c.from,
        "maxChars": CLIP_MAX_CHARS,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct ClipIn {
    text: Option<String>,
    from: Option<String>,
}

async fn clipboard_post(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    Json(body): Json<ClipIn>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    // 剪贴板含最高 2 万字且每次落盘: 限流防止单设备高频 POST 拖慢磁盘/worker。
    // 正常自动同步约 40 次/分钟, 240 次/分钟不会误伤。
    if !clip_admit(peer) {
        return err(StatusCode::TOO_MANY_REQUESTS, "剪贴板更新过于频繁，请稍后再试");
    }
    // 与聊天一样剥 NUL 并截断: 内容来自客户端, 不能信任其长度/控制字符
    let text = body
        .text
        .unwrap_or_default()
        .chars()
        .filter(|c| *c != '\0')
        .take(CLIP_MAX_CHARS)
        .collect::<String>();
    if text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "剪贴板内容不能为空");
    }
    let from = body
        .from
        .unwrap_or_default()
        .trim()
        .chars()
        .filter(|c| *c != '\0' && !c.is_control())
        .take(NICK_MAX_CHARS)
        .collect::<String>();
    let (updated, save) = {
        let mut c = shared_clip().lock().unwrap_or_else(|e| e.into_inner());
        c.text = text.clone();
        c.updated_at = now_f64();
        c.from = if from.is_empty() { DEFAULT_NICK.into() } else { from };
        // 锁内只序列化; 落盘交给阻塞线程
        (c.updated_at, serde_json::to_vec(&*c).ok())
    };
    schedule_clip_save(save);
    // 日志只记长度与来源, 不落内容 —— 共享剪贴板可能含密码/验证码
    push_log(format!(
        "共享剪贴板更新 {} 字 ← {}",
        text.chars().count(),
        peer
    ));
    broadcast_event("clipboard:changed");
    Json(serde_json::json!({ "ok": true, "updatedAt": updated })).into_response()
}

async fn clipboard_delete(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    // 与 POST 共用限流: 每次清空都会落盘 + 广播, 不能无限循环调用
    if !clip_admit(peer) {
        return err(StatusCode::TOO_MANY_REQUESTS, "剪贴板更新过于频繁，请稍后再试");
    }
    let (had_content, save) = {
        let mut c = shared_clip().lock().unwrap_or_else(|e| e.into_inner());
        let had = !c.text.is_empty() || !c.from.is_empty();
        *c = SharedClip::default();
        (had, serde_json::to_vec(&*c).ok())
    };
    if !had_content {
        // 已经是空槽: 跳过磁盘写与广播, 高频空删除不再制造 I/O/广播风暴
        return Json(serde_json::json!({ "ok": true })).into_response();
    }
    schedule_clip_save(save);
    push_log("共享剪贴板已清空".into());
    broadcast_event("clipboard:changed");
    Json(serde_json::json!({ "ok": true })).into_response()
}

// ---------------------------------------------------------------- //
// WebSocket: 事件推送(聊天/列表/状态), 轮询为降级通道
// ---------------------------------------------------------------- //

/// WS 发送带超时: 慢客户端/半开连接不读数据时，send 会在内核缓冲上长时间挂起；
/// 不设超时会被单个连接长期占用 WS 许可与任务，拖垮其他客户端。
async fn ws_try_send(
    socket: &mut axum::extract::ws::WebSocket,
    msg: axum::extract::ws::Message,
) -> bool {
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), socket.send(msg)).await,
        Ok(Ok(()))
    )
}

async fn ws_handler(
    ws: axum::extract::ws::WebSocketUpgrade,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
) -> Response {
    let peer = normalize_peer(addr.ip());
    // 连接数上限: 超限直接拒绝, 避免大量空闲连接拖垮服务
    let Ok(ws_permit) = ws_permits().try_acquire() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "WS 连接数已达上限");
    };
    let mut rx = event_bus().subscribe();
    let mut kick = ws_kick().subscribe();
    push_log(format!("WS 连接建立 ← {peer}"));
    // 入站帧不需要数据: 默认 64MiB 消息上限 + panic=abort 意味着少数连接即可 OOM
    let ws = ws.max_message_size(64 * 1024).max_frame_size(16 * 1024);
    ws.on_upgrade(move |mut socket: axum::extract::ws::WebSocket| async move {
        let _ws_permit = ws_permit;
        // 应用层心跳: 半开连接(漫游/AP 休眠)靠它暴露, 客户端看门狗据此强制重连
        let mut hb = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            // 停服信号存在订阅竞态(广播不重放): 在 kick 发出后才 subscribe 的连接
            // 永远收不到, 会一直占着 ws 许可。每轮循环直接以运行状态兜底, 最迟
            // 15s(心跳)退出, 反复启停不会再累积泄漏。
            if !is_running() {
                break;
            }
            tokio::select! {
                _ = kick.recv() => break, // 停服: 主动断开, 别让优雅关闭等挂起的连接
                _ = hb.tick() => {
                    if !ws_try_send(
                        &mut socket,
                        axum::extract::ws::Message::Text("{\"type\":\"hb\"}".into()),
                    )
                    .await
                    {
                        break;
                    }
                }
                ev = rx.recv() => match ev {
                    Ok(msg) => {
                        if !ws_try_send(&mut socket, axum::extract::ws::Message::Text(msg)).await {
                            break;
                        }
                    }
                    // 慢消费者: 事件已丢, 明确通知客户端立即全量同步 —— WS 在线时
                    // 前端停掉了轮询, 静默跳过会让界面永久停在旧状态。
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if !ws_try_send(
                            &mut socket,
                            axum::extract::ws::Message::Text(
                                "{\"type\":\"sync:needed\"}".into(),
                            ),
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                incoming = socket.recv() => match incoming {
                    Some(Ok(axum::extract::ws::Message::Ping(p))) => {
                        if !ws_try_send(&mut socket, axum::extract::ws::Message::Pong(p)).await {
                            break;
                        }
                    }
                    Some(Ok(axum::extract::ws::Message::Close(_))) | None => break,
                    _ => {}
                },
            }
        }
        // 循环退出(停服踢出/发送失败/客户端主动关闭)只记一条断开。
        // 该日志必须在循环体之外: 放在体内会让每 15s 心跳 / 每条广播 / 每个 Ping
        // 都记一条"连接断开", 数小时即可刷爆 2000 条日志环并挤掉 pin 锁定等审计事件
        push_log(format!("WS 连接断开 ← {peer}"));
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

/// 桌面端 app_config_dir 的等价路径(与 lib.rs 的 pref_path 对齐)。
/// 浏览器/移动壳从 /api/info 读取这两个 pref, 让网页配色跟随桌面端选择。
#[allow(unreachable_code)]
fn ui_pref_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").ok()?;
        return Some(Path::new(&home).join("Library/Application Support/com.ccflash.app"));
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").ok()?;
        return Some(Path::new(&base).join("com.ccflash.app"));
    }
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| std::env::var("HOME").ok().map(|h| Path::new(&h).join(".config")))?;
        return Some(base.join("com.ccflash.app"));
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let base = platform_default_dir()?;
        return Some(Path::new(base).join("config"));
    }
    None
}

fn read_ui_pref(key: &str) -> Option<String> {
    let p = ui_pref_dir()?.join(format!("pref-{key}"));
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn info() -> Response {
    let (running, port, ip) = status();
    // pinEnabled 只暴露"是否开启", 不下发码值 —— 浏览器/移动壳据此清理本地陈旧配对码
    let pin_enabled = access_pin()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    Json(serde_json::json!({
        "ip": ip, "port": port, "running": running,
        "version": APP_VERSION,
        "config": frontend_config(),
        "pinEnabled": pin_enabled,
        "openLan": open_lan(),
        "lanActive": LAN_ACTIVE.load(Ordering::Relaxed),
        "uiTheme": read_ui_pref("ui-theme"),
        "uiMode": read_ui_pref("ui-mode"),
    }))
    .into_response()
}

async fn ctrl_status() -> Response {
    let (running, port, ip) = status();
    Json(serde_json::json!({
        "running": running, "port": port, "ip": ip,
        "version": APP_VERSION,
        "config": frontend_config(),
        "openLan": open_lan(),
        "lanActive": LAN_ACTIVE.load(Ordering::Relaxed),
        "uiTheme": read_ui_pref("ui-theme"),
        "uiMode": read_ui_pref("ui-mode"),
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
    // 单测共享进程: 不读写真实用户的 PIN 文件
    if cfg!(test) {
        return std::env::temp_dir().join(format!(".ccflash-pin-test-{}", std::process::id()));
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
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
/// 6 位数字配对码: 必须来自 CSPRNG。时间/pid 可被推算, 而移动端配对码是
/// 同机其他 App 与本机服务之间唯一的隔离手段。
fn new_pin() -> String {
    let mut bytes = [0u8; 4];
    fill_random(&mut bytes);
    let n = u32::from_le_bytes(bytes) % 900_000;
    format!("{:06}", 100_000 + n)
}

/// 访问码“是否开启”的独立标记(与码值分离)。老用户只有码值文件, 读取时按
/// 兼容路径视为已开启; 明确关闭后写 "0", 防止残留码值文件在重启后把保护复活。
fn pin_enabled_file() -> PathBuf {
    pin_pref_file().with_file_name(PIN_ENABLED_PREF_FILE)
}

fn write_pin_enabled(enabled: bool) {
    if let Err(e) = write_secret_atomic(&pin_enabled_file(), if enabled { b"1" } else { b"0" }) {
        push_log_warn(format!("访问令牌开关状态保存失败(重启后可能无法准确回载): {e}"));
    }
}

/// 安装新令牌: 先落码值, 再写 enabled 标记; 码值写失败必须让调用方知道。
fn install_pin(pin: &str) -> std::io::Result<()> {
    write_secret_atomic(&pin_pref_file(), pin.as_bytes())?;
    write_pin_enabled(true);
    Ok(())
}

/// 原子写敏感文件: 临时文件(0600) → rename, 掉电/截断不会留下半截 PIN
fn write_secret_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("secret");
    // 唯一临时名: 固定 `.name.tmp` 在并发写(PIN/聊天/偏好调试)时会互相覆盖。
    let tmp = path.with_file_name(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    write_secret(&tmp, data)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Windows 目标已存在时部分环境 rename 不替换: 极小窗口内先删旧文件再重试
        if cfg!(windows) && path.exists() {
            let _ = std::fs::remove_file(path);
            return std::fs::rename(&tmp, path);
        }
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn access_pin() -> &'static Mutex<Option<String>> {
    static PIN: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    PIN.get_or_init(|| {
        // 单测不读写真实用户的 PIN 文件
        if cfg!(test) {
            return Mutex::new(None);
        }
        let path = pin_pref_file();
        let mobile_auto = cfg!(any(target_os = "android", target_os = "ios"));
        // enabled 标记与码值分离: “没有码值文件”不再等同于“从未开启” ——
        // 网络 home 未挂载/文件被临时清理时必须 fail-closed，而不是静默裸跑。
        let (enabled, enabled_read_err) = match std::fs::read_to_string(pin_enabled_file()) {
            Ok(s) => match s.trim() {
                "1" => (Some(true), false),
                "0" => (Some(false), false),
                // 标记存在但内容损坏: 不能当作“从未开启”, 交给 fail-closed 分支
                _ => (None, true),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, false),
            Err(_) => (None, true),
        };
        let mint = |reason: &str| -> Option<String> {
            let pin = new_pin();
            match install_pin(&pin) {
                Ok(()) => push_log_warn(format!("{reason}；已生成新配对码，请在 CC设置 查看")),
                Err(e) => push_log_err(format!(
                    "{reason}；已生成内存令牌但无法落盘（重启会再次轮换）: {e}"
                )),
            }
            Some(pin)
        };
        let saved = match std::fs::read_to_string(&path) {
            Ok(s) => {
                let s = s.trim().to_string();
                if let Some(pin) = (!s.is_empty()).then_some(s) {
                    // 明确关闭后残留的码值文件不能把保护“复活”
                    if enabled == Some(false) {
                        let _ = std::fs::remove_file(&path);
                        None
                    } else {
                        Some(pin)
                    }
                } else if enabled == Some(false) {
                    None
                } else if enabled == Some(true) || enabled_read_err || mobile_auto {
                    mint("访问令牌文件为空")
                } else {
                    None
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if enabled == Some(false) {
                    None
                } else if enabled == Some(true) || enabled_read_err || mobile_auto {
                    // 移动端默认强制生成; 桌面端在 enabled 标记存在时也 fail-closed。
                    mint(if mobile_auto {
                        "移动端本机访问令牌缺失"
                    } else {
                        "访问令牌已开启但码值文件缺失"
                    })
                } else {
                    None
                }
            }
            Err(e) => {
                // 明确关闭时即使残留码值读不出来也不能复活保护
                if enabled == Some(false) {
                    let _ = std::fs::remove_file(&path);
                    None
                } else {
                    // 码值读取失败(权限/网络盘异常)时不能静默降级为无保护
                    mint(&format!("访问令牌文件读取失败({e})"))
                }
            }
        };
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
/// IPv6 /64 聚合锁窗上限: 单台设备可锁整个前缀, 上限取 10 分钟避免长期误伤同网段
const PIN6_MAX_SECS: u64 = 600;
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

/// 公网来源的全局失败预算: 单 IP 退避 + IPv6 /64 聚合仍挡不住持有 /48 的
/// 攻击者轮换前缀, 或分布式 IPv4 同时试码。公网来源累计失败达到阈值后整体
/// 暂停受理(私网/本机不受影响), 把 6 位码的在线爆破成本抬到不可行。
const PIN_GLOBAL_FREE: u32 = 60;
const PIN_GLOBAL_WINDOW_SECS: u64 = 300;

fn pin_global_fails() -> &'static Mutex<(u32, std::time::Instant)> {
    static G: OnceLock<Mutex<(u32, std::time::Instant)>> = OnceLock::new();
    G.get_or_init(|| Mutex::new((0, std::time::Instant::now())))
}

/// 公网全局预算是否已耗尽(窗口内)。窗口过期自动清零, 不做永久封禁。
fn pin_global_locked(now: std::time::Instant) -> bool {
    let g = pin_global_fails().lock().unwrap_or_else(|e| e.into_inner());
    g.0 >= PIN_GLOBAL_FREE && now.duration_since(g.1).as_secs() < PIN_GLOBAL_WINDOW_SECS
}

fn pin_global_fail(now: std::time::Instant) {
    let mut g = pin_global_fails().lock().unwrap_or_else(|e| e.into_inner());
    if now.duration_since(g.1).as_secs() >= PIN_GLOBAL_WINDOW_SECS {
        *g = (0, now);
    }
    g.0 = g.0.saturating_add(1);
    g.1 = now;
}

/// IPv6 聚合键: /64 前缀(同网段设备同前缀, 隐私扩展只换接口标识); v4 与已折叠的
/// v4-mapped 返回 None —— v4 无轮换诉求, 单 IP 层足够
fn pin_prefix6(ip: std::net::IpAddr) -> Option<std::net::IpAddr> {
    match ip {
        std::net::IpAddr::V6(v6) => {
            // 链路本地 fe80::/64 在同一个二层网络里被所有设备共用: 按 /64 聚合
            // 会让一台设备的发现/上传/配对失败把全体 IPv6 邻居一起限流。
            // link-local 接口标识天然稳定, 直接用完整地址做 key 更公平。
            if v6.is_unicast_link_local() {
                return None;
            }
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
    let is_public = !is_lan_peer(peer);
    // 公网来源: 全局预算耗尽后连正确码也拒(与单 IP 锁定同理, 不制造对/错 oracle)
    if is_public && pin_global_locked(now) {
        return false;
    }
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
        if pin_locked(&mut pm, p, now, PIN6_FAIL_FREE, PIN6_BASE_SECS, PIN6_MAX_SECS) {
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
            // 两层都未锁定才走到: 正确码清零两层计数并放行。
            // 同时清空公网全局失败预算 —— 合法用户成功一次即可从公网 DoS 中恢复。
            if is_public {
                *pin_global_fails().lock().unwrap_or_else(|e| e.into_inner()) = (0, now);
            }
            m.remove(&peer);
            drop(m);
            if let Some(p) = pfx {
                pin_prefix_fails().lock().unwrap_or_else(|e| e.into_inner()).remove(&p);
            }
            return true;
        }
        // 失败才计数(ok 已在上方返回): /64 聚合层与单 IP 层各记一次 ——
        // 锁序改为"判定先行"后前缀计数原在判定块内, 重排时必须补回, 否则前缀锁永不触发
        if is_public {
            pin_global_fail(now);
        }
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
    // 移动端同样允许关闭(用户在设置页二次确认风险后执行): 关闭后同 Wi-Fi 设备
    // 无需配对码即可读写本机共享; 回环地址的安全边界由用户显式选择承担。
    if !enable {
        *slot = None;
        // 先写关闭标记再撤码值: 即使删码失败, 标记 0 也能保证重启后不会复活保护
        write_pin_enabled(false);
        let _ = std::fs::remove_file(pin_pref_file());   // 同步撤盘, 重启后不再回载
        clear_share_tickets();                           // 关闭保护: 已签发的下载凭证一并作废
        let _ = ws_kick().send(());                       // 关闭/开启令牌都重新握手: 旧 WS 不得继续订阅
        push_log_warn("访问令牌已关闭 —— 局域网设备不再需要配对".into());
        return serde_json::json!({ "ok": true });
    }
    let pin = new_pin();
    let path = pin_pref_file();
    // 先原子落盘成功再改内存: 写失败必须报错 —— 否则 UI 显示"已开启"，
    // 重启后读不到文件，保护静默消失（比没有保护更危险）
    if let Err(e) = install_pin(&pin) {
        push_log_err(format!("访问令牌开启失败: 无法写入 {} ({e})", path.display()));
        return serde_json::json!({
            "ok": false,
            "error": format!("访问令牌保存失败（{}），保护未开启: {e}", path.display())
        });
    }
    *slot = Some(pin.clone());
    // 轮换配对码 = 旧凭证作废: 必须真的清掉票据, 否则日志宣称的"旧分享链接失效"是假的
    clear_share_tickets();
    let _ = ws_kick().send(());                           // 新码生效: 旧连接必须重新认证
    push_log_ok("访问令牌已开启（已生成新配对码，旧分享链接随之失效）".into());
    serde_json::json!({ "ok": true, "pin": pin })
}

#[tauri::command]
pub async fn control_status_cmd() -> serde_json::Value {
    // 网卡枚举/磁盘查询是同步阻塞调用, 每 2.5s 一次: 放工作线程, 不能卡 UI 主线程
    tauri::async_runtime::spawn_blocking(|| {
        let (running, port, ip) = status();
        let share = strip_verbatim(&share_dir().to_string_lossy());
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
            "lanActive": LAN_ACTIVE.load(Ordering::Relaxed),
            "pin": pin.unwrap_or_default(),
        })
    })
    .await
    .unwrap_or_else(|_| serde_json::json!({ "running": false, "port": 0, "ip": "--", "error": "状态读取失败" }))
}

/// 移动端不能"托管任意共享目录": 手机沙盒里没有用户可浏览的目录概念,
/// 允许改目录只会制造"文件存哪"的困惑与越权写入面。服务启停与局域网开关
/// 则允许(默认安全模式, 用户可自行开启局域网)。
fn mobile_client_only() -> bool {
    cfg!(any(target_os = "android", target_os = "ios"))
}

#[tauri::command]
pub async fn control_start_cmd(port: Option<u16>) -> serde_json::Value {
    // start 内含两轮各 1.5s 的残留清理扫描: 同步命令会卡住 UI 主线程
    tauri::async_runtime::spawn_blocking(move || match start(port) {
        Ok(port) => serde_json::json!({ "ok": true, "port": port }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    })
    .await
    .unwrap_or_else(|_| serde_json::json!({ "ok": false, "error": "启动失败" }))
}

#[tauri::command]
pub async fn control_stop_cmd() -> serde_json::Value {
    // stop 最多等待 2s 关闭 + 3s 在途上传: 必须离开主线程执行
    tauri::async_runtime::spawn_blocking(|| serde_json::json!({ "ok": stop() }))
        .await
        .unwrap_or_else(|_| serde_json::json!({ "ok": false, "error": "停止失败" }))
}

#[tauri::command]
pub fn control_clear_logs_cmd() -> serde_json::Value {
    clear_logs();
    serde_json::json!({ "ok": true })
}

#[tauri::command]
pub fn set_open_lan_cmd(open: bool) -> serde_json::Value {
    set_open_lan(open);
    // 回报实际保存值(监听地址需下次 start 才切换)
    serde_json::json!({ "ok": true, "openLan": open_lan() })
}

fn path_component_eq(a: &Path, b: &Path) -> bool {
    let mut ac = a.components();
    let mut bc = b.components();
    loop {
        match (ac.next(), bc.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                let eq = if cfg!(windows) {
                    x.as_os_str().to_string_lossy().eq_ignore_ascii_case(&y.as_os_str().to_string_lossy())
                } else {
                    x == y
                };
                if !eq {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// child 是否以 base 为前缀(按路径组件, Windows 忽略大小写)。
/// 不能用字符串 starts_with: C:\Users\foo 会错误匹配 C:\Users\foobar。
fn path_starts_with_components(child: &Path, base: &Path) -> bool {
    let cc: Vec<_> = child.components().collect();
    let bc: Vec<_> = base.components().collect();
    if cc.len() < bc.len() {
        return false;
    }
    cc[..bc.len()].iter().zip(bc.iter()).all(|(x, y)| {
        if cfg!(windows) {
            x.as_os_str().to_string_lossy().eq_ignore_ascii_case(&y.as_os_str().to_string_lossy())
        } else {
            x == y
        }
    })
}

/// 高风险共享目录: 家目录本身 / 家目录的祖先(/Users、C:\Users 等) / 磁盘根。
/// 共享这些位置等于把个人全部文件对整个 Wi-Fi 开放读写删, 前端须二次确认(force)后才放行
fn is_dangerous_share_dir(p: &Path) -> bool {
    // 必须先 canonicalize: 词法比较会被 `/Users/name/..`、指向家目录的符号链接、
    // Windows 大小写/8.3 短名绕过, 最终共享的仍是整个家目录或根
    let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    #[cfg(target_os = "windows")]
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var("HOME").unwrap_or_default();
    let home_path = if home.is_empty() {
        None
    } else {
        Some(std::fs::canonicalize(&home).unwrap_or_else(|_| PathBuf::from(&home)))
    };
    if let Some(home_path) = home_path {
        // 家目录本身, 或其任意祖先(覆盖 / 、/Users 、C:\ 、C:\Users)
        let same = path_component_eq(&canon, &home_path);
        let ancestor = path_starts_with_components(&home_path, &canon);
        if same || ancestor {
            return true;
        }
    }
    // 文件系统根(parent 为 None, 如 "/")
    canon.parent().is_none()
}

#[tauri::command]
pub fn set_share_dir_cmd(path: String, force: Option<bool>) -> serde_json::Value {
    if mobile_client_only() {
        return serde_json::json!({ "ok": false, "error": "移动端为客户端模式，不能托管共享目录" });
    }
    // 运行中拒绝: 服务持的是启动时的目录快照, 热改只会造成"UI 显示与实际共享不一致";
    // 也堵住"注入脚本趁服务运行改目录、下次启动静默共享别处"的路径(纵深防御)
    if is_running() {
        return serde_json::json!({ "ok": false, "error": "服务运行中不可更改共享目录，请先停止服务" });
    }
    // 规范化后再判断/落盘: `..`、符号链接、Windows 大小写都会在这里被展开,
    // 避免高风险目录确认被绕过
    let Ok(canon) = std::fs::canonicalize(&path) else {
        return serde_json::json!({ "ok": false, "error": "目录不存在或不可访问" });
    };
    if !canon.is_dir() {
        return serde_json::json!({ "ok": false, "error": "目录不存在" });
    }
    if !force.unwrap_or(false) && is_dangerous_share_dir(&canon) {
        return serde_json::json!({ "ok": false, "dangerous": true, "error": "高风险目录" });
    }
    let shown = canon.to_string_lossy().to_string();
    set_share_dir(canon);
    if force.unwrap_or(false) {
        push_log_warn(format!("共享目录已切换为 {shown}（高风险目录，用户已二次确认）"));
    } else {
        push_log(format!("共享目录已切换为 {shown}（下次启动生效）"));
    }
    serde_json::json!({ "ok": true })
}

#[tauri::command]
pub fn open_path_cmd(path: String) -> serde_json::Value {
    // 只允许打开共享目录内的路径: 否则命令本身就成了"任意程序执行"入口
    let share = share_dir();
    let Ok(canon) = std::fs::canonicalize(&path) else {
        return serde_json::json!({ "ok": false, "error": "路径不存在或不可访问" });
    };
    let share_canon = std::fs::canonicalize(&share).unwrap_or(share);
    if !canon.starts_with(&share_canon) {
        return serde_json::json!({ "ok": false, "error": "仅允许打开共享目录内的文件" });
    }
    match open_path(&canon.to_string_lossy()) {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    }
}

#[tauri::command]
pub fn open_url_cmd(url: String) -> serde_json::Value {
    // 诊断页需要直接打开系统网络/防火墙设置; 除此之外只放行 http(s)。
    // 不走 cmd/shell, 全部是固定参数直传。
    let allowed = url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("x-apple.systempreferences:")
        || url.starts_with("ms-settings:");
    match if allowed { open_with_system(&url) } else { Err("仅允许 http/https 或系统设置链接".into()) } {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    }
}

/// 复制文件本体到系统剪贴板(访达/资源管理器可直接粘贴)。
/// macOS: osascript POSIX file · Windows: PowerShell Set-Clipboard · Linux: 暂不支持
#[tauri::command]
pub async fn copy_file_cmd(name: String) -> serde_json::Value {
    // 复制会同步等待 osascript/PowerShell 输出, 不能阻塞 UI 主线程
    tauri::async_runtime::spawn_blocking(move || copy_file_blocking(name))
        .await
        .unwrap_or_else(|_| serde_json::json!({ "ok": false, "error": "复制失败" }))
}

fn copy_file_blocking(name: String) -> serde_json::Value {
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
    // 移动端没有系统剪贴板文件复制能力(下面直接返回 Unsupported), 仅消除未用警告
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let _ = &p;
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
/// 从 Host / Origin 的 host 部分提取 IP 字面量(支持 `[v6]:port` 形态)。
/// v4-mapped IPv6 统一折回 v4, 否则 `[::ffff:192.168.1.5]` 与 `192.168.1.5`
/// 会被当成两个不同地址(同源判断失败、私网判定失效)。
fn host_literal_ip(host: &str) -> Option<std::net::IpAddr> {
    let raw = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next()?
    } else {
        host.split(':').next()?
    };
    // 链路本地 IPv6 的 Host 可能是 [fe80::1%25en0]:1688; %25(URL) 或 %(系统 zone)
    // 都必须先剥掉再 parse，否则发现到的 fe80 设备所有 API 都会被判 Host 非法。
    let literal = raw.split('%').next().unwrap_or(raw);
    let ip = literal.parse().ok()?;
    Some(normalize_peer(ip))
}

/// 返回值: `Ok(cross_lan)` = 放行, 且是否为"局域网跨设备"来源
/// (true 时中间件不得因源地址是本机而豁免 PIN, 否则恶意页面可借同机浏览器打本机服务)。
fn browser_origin_guard(req: &axum::extract::Request) -> Result<bool, Response> {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // IPv6 字面量 Host("[::1]:1688")不能按 ':' 硬切(会截出 "[")——必须先判括号形态,
    // 从原始 host 上取 "]" 之前的 "[::1" 解析; 只有非括号 Host 才按 ':' 摘端口
    let host_ip = host_literal_ip(host);
    // 允许: IP / localhost。曾放行 *.local —— 但同 Wi-Fi 攻击者可自架 mDNS 应答:
    // 先让受害者从攻击者机器加载页面, 再把 evil.local 重绑到受害者 IP, 页面同源
    // 请求直连其本机服务(源地址是本机 → is_local_source 豁免 pin), 攻击者 JS 即可
    // 拿到无码的 files/upload/delete。二维码/地址栏给的都是 IP, 收紧不损失正常路径
    let host_ok = host_ip.is_some() || host.split(':').next().unwrap_or("") == "localhost";
    if !host_ok {
        return Err(err(StatusCode::BAD_REQUEST, "非法的 Host"));
    }
    if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        // 全部精确匹配: 子串匹配可被 tauri.localhost.evil.com 绕过(该域名合法注册即含此子串),
        // 叠加 CorsLayer::permissive 的 ACAO:* 后攻击者 JS 可读取全部响应
        let self_ok = origin == format!("http://{host}")
            || origin == "tauri://localhost"          // macOS/iOS WKWebView
            || origin == "http://tauri.localhost"     // Windows WebView2
            || origin == "https://tauri.localhost"    // Android WebView(https 源)
            // tauri dev: 窗口加载自 devServer(5173)。仅 debug 构建放行 ——
            // release 下放行"本机任意端口"会让本机任何网页跨源读数据/启停服务
            || (cfg!(debug_assertions)
                && (origin.starts_with("http://localhost:")
                    || origin.starts_with("http://127.0.0.1:")));
        let origin_ip = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
            .map(|r| r.split('/').next().unwrap_or(""))
            .and_then(host_literal_ip);
        let origin_lan = origin_ip
            .map(|ip| is_lan_peer(ip) || ip.is_loopback())
            .unwrap_or(false);
        let host_is_lan = host_ip
            .map(|ip| is_lan_peer(ip) || ip.is_loopback())
            .unwrap_or(false);
        // 局域网设备互发(附近设备)唯一放开面:
        // 目标必须是私网 IP 且不能是 loopback, 来源与目标也不能是同一 IP。
        // 这样恶意局域网页面既不能借浏览器打 127.0.0.1, 也不能伪装成"同机页面";
        // 并且返回 cross_lan 让中间件对这些请求不豁免 PIN。
        let cross_lan = !self_ok
            && origin_lan
            && host_is_lan
            && !host_ip.map(|ip| ip.is_loopback()).unwrap_or(false)
            && origin_ip != host_ip;
        if !self_ok && !cross_lan {
            return Err(err(StatusCode::FORBIDDEN, "跨源请求被拒绝"));
        }
        return Ok(cross_lan);
    }
    Ok(false)
}

/// 日志脱敏: 查询串里的访问码不落日志(日志可能被截图/复制分享, 码一泄即失效)
fn mask_pin(query: &str) -> String {
    query
        .split('&')
        .map(|kv| if kv.starts_with("pin=") { "pin=***" } else if kv.starts_with("ticket=") { "ticket=***" } else if kv.starts_with("media=") { "media=***" } else { kv })
        .collect::<Vec<_>>()
        .join("&")
}

/// 判定请求来源是否为本机: 回环地址, 或源地址是本机任一网卡的地址。
/// 同一台电脑的浏览器用局域网 IP 访问本机服务时(如 http://10.x.x.x:1688),
/// TCP 源地址是本机网卡 IP 而非 127.0.0.1 —— 只判回环会把"坐在电脑前的人"当成
/// 远程设备锁在访问码外面。伪造该源地址需完成 TCP 握手(收不到 SYN-ACK), 不可冒充
/// 是否私网/链路本地地址: 公网 IPv6(或被路由到公网的地址)默认不允许无配对码访问,
/// 避免主机有公网 IPv6 时把共享目录暴露到互联网。
fn is_lan_peer(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            // 100.64.0.0/10 是运营商级 NAT / Tailscale 使用的共享地址段,
            // 不属于 RFC1918; 不认它会把远程访问用户当成公网直接 403。
            let o = v4.octets();
            v4.is_private()
                || v4.is_link_local()
                || v4.is_loopback()
                || (o[0] == 100 && (o[1] & 0xc0) == 0x40)
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || (s[0] & 0xfe00) == 0xfc00   // fc00::/7 ULA
                || (s[0] & 0xffc0) == 0xfe80   // fe80::/10 link-local
        }
    }
}

fn is_local_source(ip: std::net::IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    // 精确匹配本机网卡地址集合。旧实现用"能否 bind 到该地址"判定,
    // 在 net.ipv4.ip_nonlocal_bind=1 等环境下远端地址也能 bind 成功,
    // 会把所有远端请求当本机从而整体绕过访问码。
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        if ifs.iter().any(|i| i.ip() == ip) {
            return true;
        }
    }
    false
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
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // Host/Origin 校验只作用于 API/WS: 静态页面仍允许用主机名访问, 但页面内的
    // 每个数据读写都走 API 的严格校验 —— DNS rebinding 能加载页面也拿不到数据。
    let is_api = path == "/api" || path.starts_with("/api/") || path == "/ws";
    let mut cross_lan_origin = false;
    if is_api {
        match browser_origin_guard(&req) {
            Ok(v) => cross_lan_origin = v,
            Err(resp) => return resp,
        }
    }
    // 无访问令牌时禁止跨设备 Origin: 否则恶意局域网页面可借受害者浏览器
    // (源地址=受害者本机, 默认豁免 PIN)绕过 AP 隔离读取/删除本机文件。
    // Tauri 壳(tauri://localhost)与同源页面不是 cross_lan, 不受影响。
    if cross_lan_origin
        && access_pin()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
    {
        // 无令牌时只放行“投递类”POST(附近设备发消息/文件)。读列表、下载、删除、
        // 生成分享票据等一律拒绝 —— 否则恶意局域网页面可借同机浏览器读取/删除/外带。
        let deliver = method.as_str() == "POST" && (path == "/api/upload" || path == "/api/chat");
        if !deliver {
            return err(StatusCode::FORBIDDEN, "跨源请求需要目标设备开启访问令牌");
        }
    }
    // 移动端控制接口只走应用内 Tauri 命令: 127.0.0.1 在 Android 上不按应用隔离,
    // 同机任意 App 都能调 HTTP 控制面(停服/读日志)。桌面端仍需 HTTP 日志面板。
    if path.starts_with("/api/control/") && mobile_client_only() {
        return err(StatusCode::FORBIDDEN, "移动端控制接口仅限应用内命令");
    }
    // 控制接口(启停/日志/清空)仅限本机: 桌面壳走 127.0.0.1 回环或 Tauri 命令,
    // 局域网任意设备可 curl 调用的话, 任何人能关停服务/读走全部活动日志(文件名+IP+聊天预览)
    if path.starts_with("/api/control/") && !peer.is_loopback() {
        return err(StatusCode::FORBIDDEN, "控制接口仅限本机访问");
    }
    // 控制接口有副作用。浏览器跨站请求(<a ping>、sendBeacon)可能不带 Origin,
    // 但会带 Sec-Fetch-Site —— 不拦的话恶意页面点一下链接就能停服/清空审计日志。
    if path.starts_with("/api/control/") {
        let sfs = req
            .headers()
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // 只拦"无 Origin 且跨站"的请求(<a ping> 等简单请求): 桌面壳的
        // tauri://localhost 跨源调用会带 Origin, 不能被误伤(日志面板/启停)。
        let has_origin = req.headers().contains_key(header::ORIGIN);
        if !has_origin && (sfs == "cross-site" || sfs == "same-site") {
            return err(StatusCode::FORBIDDEN, "控制接口拒绝跨站请求");
        }
    }
    let query = req.uri().query().unwrap_or("").to_string();
    // 访问码(可选, 设置页开启): 非本机来源的 API/WS 请求必须携带 ?pin= 或 X-Access-Pin。
    // 本机来源(回环 + 本机网卡地址, 覆盖"同机浏览器经局域网 IP 访问")豁免 ——
    // 在电脑前的人本就拥有完全控制权; 安卓/iOS 壳走 127.0.0.1 同样命中。
    // 例外: 局域网跨设备 Origin(附近设备互发)即使 TCP 源地址是本机(受害者浏览器
    // 代发), 也绝不豁免 PIN —— 否则恶意局域网页面可借同机浏览器无码操作服务。
    // 静态页面不走此中间件, 网页端先加载页面再由 401 引导输入访问码
    // 移动端开启访问令牌后, loopback 也不再豁免: 127.0.0.1 在 Android 上不按应用
    // 隔离, 同机其他 App 不能凭"本机来源"直接读写/停服。移动壳页面通过 Tauri 命令
    // 取得本机码并自动放进 X-Access-Pin; 浏览器访问时正常输入配对码。
    // 移动端只要启用了访问令牌，就对所有来源强制校验 —— 包括本机网卡 IP。
    // Android/iOS 的 127.0.0.1 不按应用隔离，同机其他 App 改连本机 Wi-Fi IP
    // 时源地址会命中 is_local_source；只锁 loopback 会留下完整绕过口。
    let mobile_pin_required = mobile_client_only()
        && access_pin()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
    if is_api && (cross_lan_origin || mobile_pin_required || !is_local_source(peer)) {
        // 公网地址(非私网/链路本地)默认必须带访问令牌: 主机有公网 IPv6 且防火墙
        // 未拦入站时, 旧行为会把共享目录直接暴露给互联网。私网/本机仍零配置直连。
        if !is_lan_peer(peer) && access_pin().lock().unwrap_or_else(|e| e.into_inner()).is_none() {
            return err(
                StatusCode::FORBIDDEN,
                "公网地址访问需先在电脑端「CC设置」开启访问令牌",
            );
        }
        // 分享票据先于访问码校验: 票据是一次性、绑定单文件的下载凭证 ——
        // 若放在 pin 之后, 开启访问码时仅带 ?ticket= 的分享链接会先吃 401,
        // 接收方被迫索要全站配对码, 与"链接仅授权下载该文件"的承诺矛盾
        if path.starts_with("/api/download/") {
            // 媒体会话: 缩略图/预览共用, IP 绑定 + 10min, 可重复 HEAD/GET/Range。
            // 只用于读取, 不授予上传/删除/API 能力; 无效则继续走票据/访问码。
            if let Some(raw) = query.split('&').find_map(|kv| kv.strip_prefix("media=")) {
                let tok = percent_encoding::percent_decode_str(raw)
                    .decode_utf8_lossy()
                    .to_string();
                if media_session_allows(&tok, peer) {
                    return next.run(req).await;
                }
            }
            // 单次剥离前缀(与 handler 的 AxPath 对齐): trim_start_matches 会循环剥,
            // /api/download/api/download/x 会被剥成 x 造成票据绑定错位(#3)
            let raw_path = path.strip_prefix("/api/download").unwrap_or("");
            // HEAD 只是预检, 不消费票据; GET 先原子取出票据再执行, 失败(4xx/5xx)放回。
            // 这样并发 GET 只有一个能拿到票据, 一次性承诺不再有 TOCTOU 窗口。
            if method == "HEAD" {
                if valid_ticket(&query, raw_path).await.is_some() {
                    ticket_fails()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&rate_key(peer));
                    return next.run(req).await;
                }
            } else if let Some((id, ticket)) = take_ticket(&query, raw_path).await {
                ticket_fails()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&rate_key(peer));
                let res = next.run(req).await;
                if !(method == "GET" && res.status().is_success()) {
                    put_ticket(id, ticket);
                } else {
                    // 记下"已消费": Safari 媒体/PDF 查看器还会发后续 Range 请求
                    remember_ticket(&id, &ticket, peer);
                }
                return res;
            }
            // 已消费票据的后续 Range 请求(分段预览/断点续传): 同票、同 IP、同文件、
            // 文件内容未变才放行; 无 Range 的第二次整包下载仍然 401。
            if method == "GET"
                && req.headers().get(header::RANGE).is_some()
                && !range_starts_at_zero(&req)
                && query.split('&').any(|kv| kv.starts_with("ticket="))
            {
                let reusable = match query_ticket_value(&query).zip(ticket_path_name(raw_path)) {
                    Some((id, name)) => recent_ticket_allows(&id, &name, peer).await,
                    None => false,
                };
                if reusable {
                    return next.run(req).await;
                }
            }
            // 无效票据 = 正在猜码: 按来源 IP 限流, 给 48bit 票据再加一层探测成本
            if query.split('&').any(|kv| kv.starts_with("ticket="))
                && access_pin().lock().unwrap_or_else(|e| e.into_inner()).is_some()
                && !ticket_admit(peer)
            {
                return err(StatusCode::TOO_MANY_REQUESTS, "分享链接尝试过于频繁，请稍后再试");
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
        // 静态资源/页面每开一次就十几条请求, 不进服务日志(只记 API/WS)
        || (!path.starts_with("/api") && path != "/ws")
        || path == "/favicon.ico"
        // 附近设备心跳: 每个客户端每 12s 一次 POST, 逐条记会刷爆日志环
        || (path == "/api/hello" && method == "POST")
        // 并行分片上传: 每个大文件几十个成功分片请求, 逐条记会把日志环冲爆;
        // 开始/完成/中断由 upload handler 自己记关键日志, 中间片仅失败时留痕
        || (path == "/api/upload" && method == "POST" && res.status().is_success())
        // 成功下载同样可被高频刷屏, 挤掉 PIN 锁定等安全事件; 只按来源采样记录
        || (path.starts_with("/api/download/") && res.status().is_success() && !log_admit(peer))
        // 审计防洪水: 4xx 失败请求按来源限流(每 IPv4/IPv6-/64 每分钟 60 条),
        // 否则未认证设备可用 2000+ 次 404 把 PIN 锁定/删除等安全事件挤出日志环
        || (res.status().is_client_error() && !log_admit(peer));
    if !noisy {
        let ms = started.elapsed().as_millis();
        let size = res
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(humans)
            .unwrap_or_else(|| "-".into());
        // 昵称完全由客户端自报, 只作展示不作身份: 标注"自称"避免被嫁祸到他人名下
        let who = if nick.is_empty() {
            device
        } else {
            format!("{nick}(自称)·{device}")
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

/// JSON/表单请求在进入 handler 前就会缓冲 body；限大小之外再加一层全局许可，
/// 避免大量小请求同时把 axum 的 body 缓冲堆到内存上限。
fn body_gate() -> &'static tokio::sync::Semaphore {
    static S: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    S.get_or_init(|| tokio::sync::Semaphore::new(32))
}
/// 单来源 body 许可: 只靠全局 32 个许可时, 一个 IP 开 32 条慢 body 就能
/// 把所有聊天/剪贴板/分享请求挤成 429。按来源再限 4 条, 并给 handler 30s
/// 超时, 超时/断连时许可随 Drop 释放。
const BODY_PEER_MAX: u32 = 4;
fn body_peer_gate() -> &'static Mutex<HashMap<std::net::IpAddr, u32>> {
    static M: OnceLock<Mutex<HashMap<std::net::IpAddr, u32>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}
struct BodyPeerGuard(Option<std::net::IpAddr>);
impl Drop for BodyPeerGuard {
    fn drop(&mut self) {
        let Some(p) = self.0 else { return };
        let mut m = body_peer_gate().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&p) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&p);
            }
        }
    }
}
async fn body_gate_mw(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path();
    let body_route = matches!(
        path,
        "/api/chat" | "/api/discovery/probe" | "/api/clipboard" | "/api/share-ticket"
            | "/api/media-session" | "/api/hello"
    ) && req.method() == axum::http::Method::POST;
    if body_route {
        let peer = req
            .extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|ci| normalize_peer(ci.0.ip()));
        if let Some(p) = peer {
            let mut m = body_peer_gate().lock().unwrap_or_else(|e| e.into_inner());
            if m.len() > 1024 {
                m.retain(|_, n| *n > 0);
            }
            let n = m.entry(p).or_insert(0);
            if *n >= BODY_PEER_MAX {
                return err(StatusCode::TOO_MANY_REQUESTS, "该设备请求过多，请稍后重试");
            }
            *n += 1;
        }
        let _peer_guard = BodyPeerGuard(peer);
        let Ok(permit) = body_gate().try_acquire() else {
            return err(StatusCode::TOO_MANY_REQUESTS, "请求过多，请稍后重试");
        };
        let res = match tokio::time::timeout(std::time::Duration::from_secs(30), next.run(req)).await {
            Ok(r) => r,
            Err(_) => err(StatusCode::REQUEST_TIMEOUT, "请求处理超时"),
        };
        drop(permit);
        return res;
    }
    next.run(req).await
}

fn router(share: PathBuf) -> Router {
    Router::new()
        .route("/api/files", get(files))
        .route("/api/download/*name", get(download))
        .route("/api/upload", post(upload))
        .route("/api/upload_progress", get(upload_progress))
        .route("/api/upload_cancel", post(upload_cancel))
        .route("/api/delete/*name", delete(delete_file))
        // 单条消息上限 20 万字(UTF-8 最多 800KB) + 引用快照/JSON 转义余量 → 4MB
        .route("/api/chat", get(chat_get).post(chat_post).layer(DefaultBodyLimit::max(256 * 1024)))
        .route("/api/chat/:id", delete(chat_delete))
        .route("/api/clipboard", get(clipboard_get).post(clipboard_post).delete(clipboard_delete).layer(DefaultBodyLimit::max(128 * 1024)))
        .route("/api/share-ticket", post(share_ticket).layer(DefaultBodyLimit::max(16 * 1024)))
        .route("/api/media-session", post(media_session).layer(DefaultBodyLimit::max(4 * 1024)))
        .route("/api/hello", post(device_hello).layer(DefaultBodyLimit::max(16 * 1024)))
        .route("/api/devices", get(device_list))
        .route("/api/discovery/probe", post(discovery_probe).layer(DefaultBodyLimit::max(4 * 1024)))
        .route("/ws", get(ws_handler))
        .route("/api/info", get(info))
        .route("/api/control/status", get(ctrl_status))
        .route("/api/control/start", post(ctrl_start))
        .route("/api/control/stop", post(ctrl_stop))
        .route("/api/control/logs", get(ctrl_logs))
        .route("/api/control/logs/clear", post(ctrl_logs_clear))
        .route("/favicon.ico", get(favicon_handler))
        // fallback 必须在 layer 之前注册: axum 的 Router::layer 不会包裹之后才
        // 替换的 fallback, 否则静态页面完全绕过 Host/Origin 校验与请求日志。
        .fallback(static_handler)
        // 鉴权(log_middleware)必须早于 body 闸门: 未通过 PIN/Origin 的
        // 慢请求不能占用全局 body 许可。
        .layer(axum::middleware::from_fn(body_gate_mw))
        .layer(axum::middleware::from_fn(log_middleware))
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

    /// 共享目录里的符号链接不能被递归进列表（否则可列出/遍历界外目录，甚至成环）。
    #[cfg(unix)]
    #[test]
    fn collect_files_skips_symlinked_dirs() {
        let root = std::env::temp_dir().join(format!("ccflash-symlink-test-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("ccflash-symlink-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("real.txt"), b"ok").unwrap();
        std::fs::write(outside.join("secret.txt"), b"no").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linked-dir")).unwrap();
        let mut out = Vec::new();
        let mut visited = 0usize;
        collect_files(&root, &root, 0, &mut out, &mut visited);
        let names: Vec<&str> = out.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"real.txt"), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("secret")), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("linked-dir")), "{names:?}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

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
            // 上传成功会先记一条文件消息(chat:new), 再广播列表变更(files:changed)
            let event: serde_json::Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
            assert_eq!(event, serde_json::json!({ "type": "chat:new" }));
            let event: serde_json::Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
            assert_eq!(event, serde_json::json!({ "type": "files:changed", "action": "uploaded" }));

            let response = delete_file(State(share.clone()), axum::extract::ConnectInfo(peer), AxPath("phone.txt".into())).await;
            assert_eq!(response.status(), StatusCode::OK);
            // 删除时文件事件一并清除, 同样先 chat:new 再 files:changed
            let event: serde_json::Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
            assert_eq!(event, serde_json::json!({ "type": "chat:new" }));
            let event: serde_json::Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
            assert_eq!(event, serde_json::json!({ "type": "files:changed" }));

            let saved = {
                let mut log = chat_log().lock().unwrap_or_else(|e| e.into_inner());
                std::mem::take(&mut *log)
            };
            for i in 0..=CHAT_MAX {
                // 本测试关注历史/ID, 不测限流: 每 50 条清一次配额
                if i % 50 == 0 {
                    chat_rate().lock().unwrap_or_else(|e| e.into_inner()).clear();
                }
                let response = chat_post(axum::extract::ConnectInfo(peer), Json(ChatIn {
                    nickname: Some("phone".into()), message: format!("message {i}"), quote: None,
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

    /// 超过单条消息上限必须明确拒绝(400), 不能静默截断后仍回 ok。
    #[test]
    fn chat_rejects_message_over_limit() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let peer = "192.168.1.51:43210".parse::<SocketAddr>().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            chat_rate().lock().unwrap_or_else(|e| e.into_inner()).clear();
            let too_long = "x".repeat(MSG_MAX_CHARS + 1);
            let response = chat_post(
                axum::extract::ConnectInfo(peer),
                Json(ChatIn { nickname: Some("test".into()), message: too_long, quote: None }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            chat_rate().lock().unwrap_or_else(|e| e.into_inner()).clear();
        });
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

    /// 回归: 共享目录里存在名为 ".ccup" 的文件时, 启动清理不得 panic, 也不得删它
    #[test]
    fn bare_ccup_name_does_not_panic_or_delete() {
        let tmp = std::env::temp_dir().join(format!("ccflash-bare-ccup-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let f = tmp.join(".ccup");
        std::fs::write(&f, b"user file").unwrap();
        clean_incomplete_uploads(&tmp);
        assert!(f.exists(), "名为 .ccup 的用户文件不得被删除");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 配对码/令牌基础性质: 6 位数字、常量时间比较、失败退避窗口
    #[test]
    fn pin_and_token_basics() {
        for _ in 0..200 {
            let pin = new_pin();
            assert_eq!(pin.len(), 6);
            assert!(pin.bytes().all(|b| b.is_ascii_digit()));
            let n: u32 = pin.parse().unwrap();
            assert!((100_000..=999_999).contains(&n));
        }
        assert!(ct_eq("123456", "123456"));
        assert!(!ct_eq("123456", "123457"));
        assert!(!ct_eq("123456", "1234567"));
        let token = random_token();
        assert_eq!(token.len(), 12);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));

        let peer: std::net::IpAddr = "192.168.9.9".parse().unwrap();
        let now = std::time::Instant::now();
        let mut map = HashMap::new();
        for _ in 0..PIN_FAIL_FREE {
            pin_count(&mut map, peer, now);
        }
        assert!(pin_locked(&mut map, peer, now, PIN_FAIL_FREE, PIN_FAIL_BASE_SECS, PIN_FAIL_MAX_SECS));
        let later = now + std::time::Duration::from_secs(PIN_FAIL_BASE_SECS + 1);
        assert!(!pin_locked(&mut map, peer, later, PIN_FAIL_FREE, PIN_FAIL_BASE_SECS, PIN_FAIL_MAX_SECS));
    }

    /// 媒体会话: 绑定来源 IP, 过期后失效, 不消费(可重复 GET/Range)
    #[test]
    fn media_session_is_ip_bound_and_expiring() {
        let peer: std::net::IpAddr = "192.168.1.50".parse().unwrap();
        let other: std::net::IpAddr = "192.168.1.51".parse().unwrap();
        {
            let mut m = media_sessions().lock().unwrap_or_else(|e| e.into_inner());
            m.clear();
            m.insert("abcdef123456".into(), MediaSession {
                peer,
                expires_at: std::time::Instant::now() + std::time::Duration::from_secs(60),
            });
        }
        assert!(media_session_allows("abcdef123456", peer));
        assert!(media_session_allows("abcdef123456", peer), "同 IP 可重复使用");
        assert!(!media_session_allows("abcdef123456", other), "换 IP 不可用");
        assert!(!media_session_allows("missing", peer));
        {
            let mut m = media_sessions().lock().unwrap_or_else(|e| e.into_inner());
            m.insert("expired123456".into(), MediaSession {
                peer,
                expires_at: std::time::Instant::now() - std::time::Duration::from_secs(1),
            });
        }
        assert!(!media_session_allows("expired123456", peer), "过期会话不可用");
        media_sessions().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 普通 .ccpart 是用户文件, 只有能对上 meta 会话指纹的断点才隐藏/清理
    #[test]
    fn part_artifact_requires_matching_session_meta() {
        let tmp = std::env::temp_dir().join(format!("ccflash-part-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let user = tmp.join("report.ccpart");
        std::fs::write(&user, b"user data").unwrap();
        assert!(!is_part_artifact(&user, "report.ccpart"), "普通用户 .ccpart 不应被隐藏");

        let (part, meta) = session_part_path(&tmp, "demo.bin", "session-22");
        std::fs::write(&part, b"chunk").unwrap();
        std::fs::write(&meta, b"session-22\ndemo.bin\n{}").unwrap();
        let fname = part.file_name().unwrap().to_string_lossy().to_string();
        assert!(is_part_artifact(&part, &fname), "带匹配 meta 的断点应被隐藏");
        std::fs::remove_file(&meta).unwrap();
        assert!(!is_part_artifact(&part, &fname), "meta 不匹配/缺失时按用户文件处理");
        let _ = std::fs::remove_dir_all(&tmp);
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

        // 6b) 后缀区间 bytes=-3 → 206 末尾 3 字节(媒体/播放器读取尾部常用)
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/download/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=-3\r\nConnection: close\r\n\r\n",
                enc(name)
            ),
        );
        assert_eq!(st, 206);
        assert_eq!(body, b"AAA", "Range bytes=-3 应返回末尾 3 字节");

        // 6c) 媒体会话令牌: 创建 + 带 media 下载
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "POST /api/media-session HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(st, 200, "媒体会话创建应成功: {}", String::from_utf8_lossy(&body));
        let media: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let token = media["token"].as_str().unwrap_or("");
        assert!(!token.is_empty(), "媒体会话应返回 token: {media}");
        let (st, _h, body) = http_raw(
            port,
            &format!(
                "GET /api/download/{}?media={} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(name), token
            ),
        );
        assert_eq!(st, 200, "媒体会话下载应成功");
        assert_eq!(body, b"AAAAAA");

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

    fn udp_packet(id: &str, port: u64, name: &str) -> serde_json::Value {
        serde_json::json!({
            "app": DISCOVERY_APP,
            "id": id,
            "name": name,
            "port": port,
            "platform": "macos",
            "busy": false
        })
    }

    /// 首次登记/重复包/忙碌变化的状态标记: 只有新增或字段变化才推 devices:changed。
    #[test]
    fn udp_ingest_reports_new_and_changed() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
        let src: std::net::SocketAddr = "192.168.80.20:5000".parse().unwrap();
        let mut p = udp_packet("state-device", 1688, "State");
        let a = ingest_peer_packet_ex(&p, src);
        assert!(a.new_device && a.changed, "首次登记应报告 new/changed");
        let b = ingest_peer_packet_ex(&p, src);
        assert!(!b.new_device && !b.changed, "重复同状态包不应推事件");
        p["busy"] = serde_json::json!(true);
        let c = ingest_peer_packet_ex(&p, src);
        assert!(!c.new_device && c.changed, "忙碌变化应报告 changed");
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 同一设备先经 IPv6 link-local、后经 IPv4 宣告: 应迁移到 IPv4。
    /// 否则设备一直显示在线，但直发使用 `fe80::%zone` URL 在 WebView 里不可用，
    /// 表现为"能给手机发消息却一直失败"。
    #[test]
    fn udp_ingest_prefers_ipv4_over_link_local_v6() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
        let v6: std::net::SocketAddr = "[fe80::1]:5000".parse().unwrap();
        let v4: std::net::SocketAddr = "192.168.60.21:5000".parse().unwrap();
        let mut p = udp_packet("dual-stack-device", 1688, "Phone");
        p["tok"] = serde_json::json!("abc123");
        assert!(ingest_peer_packet_ex(&p, v6).new_device);
        let r = ingest_peer_packet_ex(&p, v4);
        assert!(r.changed, "IPv4 宣告应触发地址迁移");
        let ip = multicast_peers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get("dual-stack-device")
            .map(|d| d.ip.clone())
            .unwrap_or_default();
        assert_eq!(ip, "192.168.60.21", "应优先保留 IPv4 地址");
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// UDP 发现报文: 合法包登记, 自身/异常 app/系统端口/超界端口全部拒绝,
    /// 昵称里的控制字符与 RTL 覆写符被剥离。
    #[test]
    fn udp_ingest_filters_invalid_packets() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
        let ip: std::net::IpAddr = "192.168.50.10".parse().unwrap();

        assert!(ingest_peer_packet(&udp_packet("peer-a", 1688, "MacBook"), ip));
        assert!(
            !ingest_peer_packet(
                &serde_json::json!({"app":"evil","id":"peer-x","port":1688}),
                "192.168.50.12".parse().unwrap()
            ),
            "非 ccflash 报文必须丢弃"
        );
        assert!(
            !ingest_peer_packet(
                &udp_packet(discovery_id(), 1688, "self"),
                "127.0.0.1".parse().unwrap()
            ),
            "本机回环同 id 必须丢弃"
        );
        assert!(
            ingest_peer_packet(
                &udp_packet(discovery_id(), 1688, "cloned"),
                "192.168.50.15".parse().unwrap()
            ),
            "另一台同 id 设备(克隆配置)必须可被发现"
        );
        for bad_port in [0u64, 80, 1023, 70000] {
            assert!(
                !ingest_peer_packet(&udp_packet("peer-b", bad_port, "bad"), "192.168.50.13".parse().unwrap()),
                "非法端口 {bad_port} 必须丢弃"
            );
        }
        assert!(
            ingest_peer_packet(
                &udp_packet("peer-c", 1688, "a\u{1}b\u{202E}c"),
                "192.168.50.14".parse().unwrap()
            )
        );
        let guard = multicast_peers().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(guard.get("peer-a").map(|d| d.port), Some(1688));
        assert!(guard.contains_key("peer-c"));
        assert_eq!(guard.get("peer-c").unwrap().nickname, "abc");
        assert!(!guard.contains_key("peer-b"));
        assert_eq!(discovery_id().len(), 16, "发现 id 固定 16 位十六进制");
        drop(guard);
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 同一源 IP 只保留最新 id(重启换 id 不堆重复项); 不同 IP 的条目受 64 条上限约束。
    #[test]
    fn udp_ingest_dedups_same_ip_and_caps_peers() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
        let ip: std::net::IpAddr = "192.168.60.5".parse().unwrap();
        assert!(ingest_peer_packet(&udp_packet("old-id", 1688, "old"), ip));
        assert!(!ingest_peer_packet(&udp_packet("new-id", 1689, "new"), ip), "同 IP 新 id 不得替换真实设备");

        {
            let guard = multicast_peers().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(guard.values().filter(|d| d.ip == "192.168.60.5").count(), 1);
            assert!(guard.contains_key("old-id"));
            assert!(!guard.contains_key("new-id"));
        }

        for i in 0..70u8 {
            let src = std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, i + 1));
            ingest_peer_packet(&udp_packet(&format!("flood-{i}"), 1688, "n"), src);
        }
        let len = multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(len, DISCOVERY_PEERS_MAX, "peers 应恰好填满上限");

        // 满额时同 IP 的新 id 也不能驱逐原条目
        let same_ip: std::net::IpAddr = "10.0.0.1".parse().unwrap();
        assert!(!ingest_peer_packet(&udp_packet("flood-0-new", 1688, "n"), same_ip));
        {
            let guard = multicast_peers().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(guard.len(), DISCOVERY_PEERS_MAX);
            assert!(guard.contains_key("flood-0"));
            assert!(!guard.contains_key("flood-0-new"));
        }
        // 满额时新 IP 仍拒绝
        assert!(
            !ingest_peer_packet(
                &udp_packet("flood-new-ip", 1688, "n"),
                "10.0.0.71".parse().unwrap()
            ),
            "满额后新 IP 应被拒绝"
        );
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 设备快照: 15s 未更新的条目被剔除; 同 ip:port 只保留最后活跃的一条。
    #[test]
    fn udp_device_snapshot_prunes_and_dedups_endpoint() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
        let now = now_f64();
        {
            let mut map = multicast_peers().lock().unwrap_or_else(|e| e.into_inner());
            map.insert("a".into(), DeviceInfo {
                id: "a".into(), ip: "192.168.70.1".into(), port: 1688,
                nickname: "A".into(), platform: "macos".into(), busy: false,
                last_seen: now, token: String::new(),
            });
            map.insert("b".into(), DeviceInfo {
                id: "b".into(), ip: "192.168.70.1".into(), port: 1688,
                nickname: "B".into(), platform: "macos".into(), busy: false,
                last_seen: now + 0.5, token: String::new(),
            });
            map.insert("old".into(), DeviceInfo {
                id: "old".into(), ip: "192.168.70.2".into(), port: 1688,
                nickname: "O".into(), platform: "macos".into(), busy: false,
                last_seen: now - 1000.0, token: String::new(),
            });
        }
        let list = device_snapshot(now);
        assert_eq!(list.len(), 1, "过期条目剔除 + 同端点去重后应只剩一条: {list:?}");
        assert_eq!(list[0].id, "b", "同一端点保留最后活跃的条目");
        assert!(multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).get("old").is_none());
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 安全模式(仅回环)/服务停止/未绑定端口时不得广播本机。
    #[test]
    fn udp_broadcast_requires_running_and_lan_access() {
        assert!(!broadcast_allowed(true, false, 1688), "安全模式不得广播");
        assert!(!broadcast_allowed(false, true, 1688), "服务停止不得广播");
        assert!(!broadcast_allowed(true, true, 0), "未绑定端口不得广播");
        assert!(broadcast_allowed(true, true, 1688));
    }

    /// 端到端(协议 → 登记 → HTTP): 用临时回环端口收一个真实 UDP 报文,
    /// 走 `ingest_peer_packet` 登记后从 `/api/devices` 查回来。
    /// 不能用 53319: 发现广播会向 53318-53321 整个范围发送, 服务自己的
    /// announce 可能先到, recv_from 会拿到它而不是测试报文(时序性失败)。
    #[test]
    fn udp_ingest_to_http_end_to_end() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(rx) = UdpSocket::bind(("127.0.0.1", 0)) else {
            eprintln!("skip: 无法绑定临时回环 UDP 端口");
            return;
        };
        let rx_addr = rx.local_addr().expect("rx local addr");
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let pref = port_pref_file();
        let orig_pref = std::fs::read_to_string(&pref).ok();
        let port = start(Some(17714)).expect("start server on 17714");

        let tx = UdpSocket::bind(("127.0.0.1", 0)).expect("bind tx");
        tx.send_to(
            udp_packet("peer-e2e-udp", 17715, "E2E Peer").to_string().as_bytes(),
            rx_addr,
        )
        .expect("send udp");
        let mut buf = [0u8; 2048];
        let (n, _src) = rx.recv_from(&mut buf).expect("rx udp");
        let v: serde_json::Value = serde_json::from_slice(&buf[..n]).expect("json");
        assert!(
            ingest_peer_packet(&v, "192.168.99.99".parse().unwrap()),
            "合法报文应登记"
        );

        let (_st, _h, body) = http_raw(
            port,
            &format!("GET /api/devices HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        );
        let q: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let found = q["devices"]
            .as_array()
            .map(|a| a.iter().any(|d| d["id"] == "peer-e2e-udp" && d["port"] == 17715))
            .unwrap_or(false);
        assert!(found, "UDP 报文应出现在 /api/devices: {q}");

        stop();
        match orig_pref {
            Some(s) => { let _ = std::fs::write(&pref, s); }
            None => { let _ = std::fs::remove_file(&pref); }
        }
        multicast_peers().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 前端所有下载/预览都先发 HEAD 预检; axum 的 get() 必须自动接住 HEAD,
    /// 否则浏览器端"下载失败 HTTP 405"。这里做真实 HTTP 断言。
    #[test]
    fn download_head_is_supported() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let pref = port_pref_file();
        let orig_pref = std::fs::read_to_string(&pref).ok();
        let port = start(Some(17716)).expect("start server on 17716");
        let share = share_dir();
        let name = "__e2e_head.txt";
        let path = share.join(name);
        std::fs::write(&path, b"hello").unwrap();

        let (st, head, body) = http_raw(
            port,
            &format!(
                "HEAD /api/download/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
                enc(name)
            ),
        );
        assert_eq!(st, 200, "HEAD 下载预检应为 200(否则前端下载全坏): {head}");
        assert!(
            head.to_ascii_lowercase().contains("content-length: 5"),
            "HEAD 应带 Content-Length: {head}"
        );
        assert!(body.is_empty(), "HEAD 不应有响应体");

        let _ = std::fs::remove_file(&path);
        stop();
        match orig_pref {
            Some(s) => { let _ = std::fs::write(&pref, s); }
            None => { let _ = std::fs::remove_file(&pref); }
        }
    }

    /// 构建产物(/assets/ 带内容 hash)长缓存, index.html 仍 no-store。
    #[test]
    fn static_assets_are_immutable_cached() {
        let path = Frontend::iter()
            .map(|p| p.to_string())
            .find(|p| p.starts_with("assets/"))
            .expect("dist/assets 应存在(先 npm run build)");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let uri: axum::http::Uri = format!("/{path}").parse().unwrap();
        let resp = rt.block_on(static_handler(uri));
        let cc = resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(cc.contains("immutable"), "assets 应长缓存: {cc}");

        let index: axum::http::Uri = "/".parse().unwrap();
        let resp2 = rt.block_on(static_handler(index));
        assert_eq!(
            resp2.headers().get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "index.html 必须 no-store"
        );
    }

    fn origin_req(host: &str, origin: Option<&str>) -> axum::extract::Request {
        let mut b = axum::http::Request::builder()
            .method("GET")
            .uri("/api/files")
            .header("host", host);
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        b.body(axum::body::Body::empty()).unwrap()
    }

    /// 安全回归自测: 私网 Origin 只允许"跨设备"互发, 不能借浏览器打 loopback,
    /// 且跨设备请求必须由中间件按 PIN 校验(不享受本机豁免)。
    #[test]
    fn origin_guard_blocks_lan_page_from_local_service() {
        // 自身页面: 放行且不是 cross_lan(享受本机 PIN 豁免)
        assert_eq!(
            browser_origin_guard(&origin_req("192.168.1.5:1688", Some("http://192.168.1.5:1688"))).ok(),
            Some(false)
        );
        // 另一台设备的私网页面 → 目标私网 IP: 放行, 但标记 cross_lan(true)
        assert_eq!(
            browser_origin_guard(&origin_req("192.168.1.5:1688", Some("http://192.168.1.99:8080"))).ok(),
            Some(true)
        );
        // 恶意私网页面打 127.0.0.1: 直接拒绝
        assert!(
            browser_origin_guard(&origin_req("127.0.0.1:1688", Some("http://192.168.1.99:8080"))).is_err(),
            "私网 Origin 不得访问 loopback"
        );
        // 公网域名 Origin: 拒绝
        assert!(
            browser_origin_guard(&origin_req("192.168.1.5:1688", Some("http://evil.com"))).is_err()
        );
        // Tauri WebView: 放行且非 cross_lan
        assert_eq!(
            browser_origin_guard(&origin_req("127.0.0.1:1688", Some("tauri://localhost"))).ok(),
            Some(false)
        );
        // 无 Origin 的非浏览器客户端: 放行
        assert_eq!(
            browser_origin_guard(&origin_req("192.168.1.5:1688", None)).ok(),
            Some(false)
        );
    }

    /// 聊天发送限流: 每 IP 60 条/分钟, 其他 IP 不受影响。
    #[test]
    fn chat_rate_limits_per_ip() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        chat_rate().lock().unwrap_or_else(|e| e.into_inner()).clear();
        let ip: std::net::IpAddr = "192.168.77.10".parse().unwrap();
        for _ in 0..CHAT_PER_MIN {
            assert!(chat_admit(ip));
        }
        assert!(!chat_admit(ip), "超过每分钟配额应拒绝");
        assert!(
            chat_admit("192.168.77.11".parse().unwrap()),
            "其他 IP 不受影响"
        );
        chat_rate().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 票据开票限流: 每 IP 60 张/分钟, 其他 IP 不受影响。
    #[test]
    fn ticket_issue_rate_limits_per_ip() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        ticket_issue_rate().lock().unwrap_or_else(|e| e.into_inner()).clear();
        let ip: std::net::IpAddr = "192.168.88.10".parse().unwrap();
        for _ in 0..TICKET_ISSUE_PER_MIN {
            assert!(ticket_issue_allow(ip));
        }
        assert!(!ticket_issue_allow(ip), "超过每分钟配额应拒绝");
        assert!(
            ticket_issue_allow("192.168.88.11".parse().unwrap()),
            "其他 IP 不受影响"
        );
        ticket_issue_rate().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// .ccup 标记识别: 新旧系统标记都隐藏成品, 用户自建/二进制文件不误判。
    #[test]
    fn ccup_markers_recognize_legacy_and_user_files() {
        let dir = std::env::temp_dir().join(format!("ccflash-ccup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let final_file = dir.join("x.txt");
        std::fs::write(&final_file, b"hello").unwrap();
        let marker = dir.join(".x.txt.ccup");

        std::fs::write(&marker, b"ccflash-v1:0123456789abcdef\n5").unwrap();
        assert!(is_system_ccup(&marker), "v1.2.2 token marker 应识别");
        assert!(marker_is_incomplete(&final_file), "进行中的标记应隐藏成品");

        std::fs::write(&marker, b"0123456789abcdef\n5").unwrap();
        assert!(is_system_ccup(&marker), "旧版裸 16hex 标记应识别");
        assert!(marker_is_incomplete(&final_file), "旧版进行中标记应隐藏成品");

        std::fs::write(&marker, b"ok").unwrap();
        assert!(!marker_is_incomplete(&final_file), "完成标记不应隐藏成品");

        std::fs::write(&marker, b"hello user data").unwrap();
        assert!(!is_system_ccup(&marker), "用户自建 .ccup 不应算系统标记");
        assert!(!marker_is_incomplete(&final_file), "用户 .ccup 不应锁住成品");

        std::fs::write(&marker, [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
        assert!(!is_system_ccup(&marker), "二进制 .ccup 不应算系统标记");

        std::fs::write(&marker, b"").unwrap();
        assert!(!is_system_ccup(&marker), "0 字节 .ccup 不应算系统标记");
        assert!(!marker_is_incomplete(&final_file), "0 字节 .ccup 不应锁住成品");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 共享剪贴板: POST 后 GET 能读回; 空内容 400; DELETE 清空。
    #[test]
    fn shared_clipboard_roundtrip_e2e() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let pref = port_pref_file();
        let orig_pref = std::fs::read_to_string(&pref).ok();
        let port = start(Some(17730)).expect("start server on 17730");
        let text = "链接 https://example.com\n验证码 123456";
        let body = serde_json::json!({ "text": text, "from": "测试设备" }).to_string();
        let req = format!(
            "POST /api/clipboard HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.as_bytes().len(),
            body
        );
        let (st, _h, resp) = http_raw(port, &req);
        assert_eq!(st, 200, "发布共享剪贴板应成功: {}", String::from_utf8_lossy(&resp));

        let (st, _h, resp) = http_raw(
            port,
            &format!("GET /api/clipboard HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(st, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp).expect("JSON");
        assert_eq!(v["text"], serde_json::json!(text));
        assert_eq!(v["from"], serde_json::json!("测试设备"));

        // 空文本拒绝(避免把别人的共享板清成空)
        let empty = serde_json::json!({ "text": "   " }).to_string();
        let req = format!(
            "POST /api/clipboard HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            empty.as_bytes().len(),
            empty
        );
        let (st, _h, _) = http_raw(port, &req);
        assert_eq!(st, 400);

        let (st, _h, _) = http_raw(
            port,
            &format!("DELETE /api/clipboard HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(st, 200);
        let (st, _h, resp) = http_raw(
            port,
            &format!("GET /api/clipboard HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(st, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp).expect("JSON");
        assert_eq!(v["text"], serde_json::json!(""));
        let _ = stop();
        // 恢复用户真实端口偏好: 测试端口不能落盘成下次启动的地址
        match orig_pref {
            Some(s) => { let _ = std::fs::write(&pref, s); }
            None => { let _ = std::fs::remove_file(&pref); }
        }
    }
}
