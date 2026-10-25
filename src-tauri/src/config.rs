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
pub const DEFAULT_PORT: u16 = 8765;
/// 首选端口被占用时向后顺延探测个数
pub const PORT_PROBE_EXTRA: u16 = 20;
/// 兜底扫描范围上界(不含), 即 DEFAULT_PORT..PORT_RANGE_END
pub const PORT_RANGE_END: u16 = 8786;

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
/// 高频轮询端点(不写请求日志)。与前端 config.js 的 POLL_* 轮询路径一一对应,
/// 前端新增/更名轮询接口时必须同步此清单, 否则服务日志会被轮询刷屏。
pub const NOISY_GET_PATHS: &[&str] = &[
    "/api/chat",
    "/api/info",
    "/api/files",
    "/api/control/logs",
];

// ---- 传输 ---- //
/// 下载流式读取缓冲
pub const CHUNK_HINT: usize = 1024 * 1024;

// ---- 共享目录 ---- //
/// 默认共享目录名(家目录下)
pub const SHARE_DIR_NAME: &str = "CC-Flash";
/// 品牌更名前的旧目录名(启动时自动迁移)
pub const SHARE_DIR_NAME_LEGACY: &str = "Wlan-Flash-Transfer";
/// 共享目录选择持久化文件(家目录隐藏文件, 内容为一行路径)
pub const SHARE_PREF_FILE: &str = ".ccflash-share";
