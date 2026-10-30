//! 可调参数集中管理。
//! 与前端 src/config.js 存在约定关系的项, 注释中以 [同步: config.js xxx] 标注, 两端需一起改。

// ---- 应用 ---- //
/// 应用显示名(托盘 tooltip / 系统通知标题)
pub const APP_NAME: &str = "CC-Flash";
/// 应用版本号。唯一来源是 Cargo.toml 的 [package] version:
/// tauri.conf.json 已省略 version 字段(构建时自动回退读 Cargo.toml),
/// 这里编译期取同值, 随 /api/info 下发给前端展示。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---- 建议前端轮询间隔(ms) [同步: config.js POLL_*] ---- //
/// 仅作下发值: 前端以 /api/info 到达后的服务端值为准, config.js 里是启动兜底
pub const POLL_CHAT_MS: u64 = 3000;
pub const POLL_FILES_MS: u64 = 5000;
pub const POLL_STATUS_MS: u64 = 2500;
pub const POLL_LOGS_MS: u64 = 2000;

// ---- 服务端口 ---- //
/// 默认端口 [同步: config.js DEFAULT_PORT]
pub const DEFAULT_PORT: u16 = 1688;
/// 首选端口被占用时向后顺延探测个数
pub const PORT_PROBE_EXTRA: u16 = 20;
/// 兜底扫描范围上界(不含), 即 DEFAULT_PORT..PORT_RANGE_END
pub const PORT_RANGE_END: u16 = 1708;

// ---- 聊天 ---- //
/// 服务端保留的聊天条数
pub const CHAT_MAX: usize = 200;
/// 昵称长度上限 [同步: config.js NICK_MAX] —— 随 /api/info 下发, 前端运行时以服务端为准
pub const NICK_MAX_CHARS: usize = 20;
/// 消息长度上限 [同步: config.js MSG_MAX] —— 随 /api/info 下发, 前端运行时以服务端为准
pub const MSG_MAX_CHARS: usize = 2000;
/// 服务日志中聊天内容预览截断长度
pub const CHAT_PREVIEW_CHARS: usize = 40;
/// 未填昵称时的默认昵称 [同步: config.js DEFAULT_NICK] —— 随 /api/info 下发, 前端运行时以服务端为准
pub const DEFAULT_NICK: &str = "匿名用户";

// ---- 日志 ---- //
/// 服务日志环形队列上限
pub const REQ_LOG_MAX: usize = 1000;
/// 单条日志行硬截断: hyper 允许数百 KB 的 URL, 不截断可被超长路径灌爆内存(1000 条 × 数百 KB)
pub const LOG_LINE_MAX: usize = 400;
/// 高频轮询端点(不写请求日志)。与前端 config.js 的 POLL_* 轮询路径一一对应,
/// 前端新增/更名轮询接口时必须同步此清单, 否则服务日志会被轮询刷屏。
pub const NOISY_GET_PATHS: &[&str] = &[
    "/api/chat",
    "/api/info",
    "/api/files",
    "/api/control/logs",
    "/api/upload_progress",
];

// ---- 传输 ---- //
/// 下载流式读取缓冲
pub const CHUNK_HINT: usize = 1024 * 1024;
/// 断点续传临时文件后缀(隐藏于列表; 服务启动时清理残留)。
/// 分片大小是客户端关注点(见 config.js UPLOAD_CHUNK), 服务端只按 X-Upload-Offset 追加写。
pub const UPLOAD_PART_SUFFIX: &str = ".ccpart";
/// 断点旁路元数据(会话 id)后缀, 与 .ccpart 一同视为内部产物
pub const UPLOAD_PART_META_SUFFIX: &str = ".ccpart.meta";
/// 上传相对路径的最大段数(与文件列表递归深度上限一致, 超深目录列表不可见)
pub const UPLOAD_MAX_DEPTH: usize = 8;
/// 上传请求体空闲超时(秒): 半开连接的残留 handler 靠它释放断点文件的写锁
pub const UPLOAD_IDLE_TIMEOUT_SECS: u64 = 120;
/// 未知大小上传(size=0, 无法预检)每写入该字节数复查一次磁盘剩余,
/// 剩余不足一个流缓冲时中止——防"声明 0 大小无限灌盘"打满系统盘
pub const UPLOAD_FREE_CHECK_BYTES: u64 = 64 * 1024 * 1024;
/// 断点保留期(秒): 启动清理只删超过该期限的残留 .ccpart。
/// 立即全清会毁掉"进程/系统重启后续传"(Android 退后台被杀、电脑重启后
/// 重选同一文件本可靠会话指纹命中服务端断点继续)
pub const UPLOAD_PART_TTL_SECS: u64 = 7 * 24 * 3600;
/// 新上传会话(offset=0)接管保护窗(秒): 断点属于其他会话且该时间内仍在写入时拒绝接管。
/// 防两台设备传同名文件陷入"互截断断点→互从零重启"的无限拉锯; 写入停止超时则放行覆盖
pub const UPLOAD_PART_BUSY_SECS: u64 = 30;

// ---- 共享目录 ---- //
/// 默认共享目录名(家目录下)
pub const SHARE_DIR_NAME: &str = "CC-Flash";
/// 品牌更名前的旧目录名(启动时自动迁移)
pub const SHARE_DIR_NAME_LEGACY: &str = "Wlan-Flash-Transfer";
/// 共享目录选择持久化文件(家目录隐藏文件, 内容为一行路径)
pub const SHARE_PREF_FILE: &str = ".ccflash-share";
/// 端口偏好持久化文件(与目录偏好同目录): 跨进程重启回放, 兑现"重启后地址不漂移"
pub const PORT_PREF_FILE: &str = ".ccflash-port";
/// 访问码持久化文件(内容为码值; 文件存在即开启): 静默失效比没有保护更危险 ——
/// 用户开了访问码, 重启后若保护自动消失, 局域网在无感知下裸奔
pub const PIN_PREF_FILE: &str = ".ccflash-pin";
