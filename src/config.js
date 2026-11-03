/* ============================================================
 * 前端可调参数集中管理
 * 标注 [同步: config.rs xxx] 的项与 src-tauri/src/config.rs 成对存在, 改一处需同步另一处
 * ============================================================ */

/* ---------- 轮询与时序(毫秒) ---------- */
/* POLL_* 仅是启动兜底值: /api(info|control/status) 携带服务端下发值(config.rs 为唯一事实), 到达后覆盖 */
export const POLL_STATUS_MS = 2500;       // 状态轮询; 失联判定 ≈ OFFLINE_FAILS × 此值
export const OFFLINE_FAILS = 2;           // 连续失败这么多次判定与服务失联(纯前端行为, 不下发)
export const POLL_CHAT_MS = 3000;         // 聊天轮询
export const POLL_FILES_MS = 5000;        // 文件列表自动同步
export const POLL_LOGS_MS = 2000;         // 桌面端服务日志
export const CONTROL_RETRY_TIMES = 10;    // 桌面窗口就绪竞态: invoke 失败重试次数
export const CONTROL_RETRY_MS = 500;
export const TOAST_MS = 2400;             // 提示停留时长
export const COPIED_RESET_MS = 1500;      // "已复制"图标复位
export const UPLOAD_DONE_KEEP_MS = 1000;  // 上传完成条目残留时长
export const BLOB_REVOKE_MS = 10_000;     // 下载 blob: URL 回收延时
export const REFRESH_SPIN_MS = 600;       // 手动刷新最短旋转时长(本地请求太快, 不兜底动画会一闪而过)

/* ---------- 列表 / 传输 ---------- */
export const PAGE_SIZE = 8;                     // 文件分页大小
/* 上传 = 单请求整文件直传(回归 1.1.2 语义): 整文件就是"最大的分片", 无分片/账本/
   断点状态, 因此没有可调的分片常数。服务端 /api/upload_progress、/api/upload_cancel
   仅保留用于 v1.1.5-1.1.7 旧版 .ccpart 断点的查询与清理(常量在 config.rs UPLOAD_PART_*);
   历史并行分片参数(UPLOAD_CELL/UPLOAD_WORKERS/UPLOAD_STALL_MS 等)已随上传链回滚删除 */
export const WS_HB_TIMEOUT_MS = 45000;          // WS 心跳超时(服务端 15s 一发; 超时未收到消息→强制重连)
export const WS_WATCHDOG_MS = 10000;            // WS 看门狗巡检间隔

/* ---------- 界面 ---------- */
export const NARROW_MAX_PX = 980;   // 手机窄屏断点
export const NICK_MAX = 20;         // 启动兜底; /api/info 下发 nickMax 覆盖(config.rs NICK_MAX_CHARS)
export const MSG_MAX = 2000;        // 启动兜底; /api/info 下发 msgMax 覆盖(config.rs MSG_MAX_CHARS)

/* ---------- 服务 ---------- */
export const DEFAULT_PORT = 1688;   // [同步: config.rs DEFAULT_PORT]

/* ---------- API 路径 ---------- */
/* 相对路径(整目录上传会产生子目录)按段编码: 保留真实 "/", 服务端为通配路由 */
const encPath = (name) => String(name).split("/").map(encodeURIComponent).join("/");
export const API = {
  files: "/api/files",
  download: (name) => `/api/download/${encPath(name)}`,
  upload: "/api/upload",
  uploadProgress: (name, id) =>
    `/api/upload_progress?name=${encodeURIComponent(name)}&id=${encodeURIComponent(id || "")}`,
  uploadCancel: (id, name) =>
    `/api/upload_cancel?id=${encodeURIComponent(id)}&name=${encodeURIComponent(name)}`,
  del: (name) => `/api/delete/${encPath(name)}`,
  chat: "/api/chat",
  info: "/api/info",
  logs: "/api/control/logs",
  logsClear: "/api/control/logs/clear",
  ws: "/ws",
};

/* ---------- localStorage 键 ---------- */
export const LS = { theme: "wft-theme", mode: "wft-mode", nick: "lft-nick", nickAuto: "lft-nick-auto", updateCheck: "wft-update-check", pin: "wft-access-pin" };

/* ---------- 展示 ---------- */
export const REL_TIME_MS = { justNow: 60_000, minutes: 3_600_000, hours: 86_400_000 }; // 相对时间阈值
export const IMG_EXTS = ["jpg", "jpeg", "png", "gif", "svg", "webp", "bmp", "ico"];
export const ICON_MAP = {
  pdf: "PDF", doc: "DOC", docx: "DOC", xls: "XLS", xlsx: "XLS", ppt: "PPT", pptx: "PPT",
  exe: "EXE", msi: "MSI", dmg: "DMG", apk: "APK", mp4: "VID", mov: "VID", avi: "VID", mkv: "VID", webm: "VID",
  mp3: "AUD", wav: "AUD", flac: "AUD", aac: "AUD", ogg: "AUD", zip: "ZIP", rar: "RAR", "7z": "7Z", tar: "TAR", gz: "GZ",
  py: "PY", js: "JS", ts: "TS", java: "JV", go: "GO", rs: "RS", c: "C", cpp: "CPP", html: "HTM", css: "CSS",
  json: "JSN", xml: "XML", md: "MD", sql: "SQL", txt: "TXT", csv: "CSV", psd: "PSD", ai: "AI", sketch: "SK", fig: "FIG",
};
export const AVATAR_POOL = [
  "😀","😄","😁","🤠","😎","🤓","🥳","😴","🙃","😇","🦊","🐻","🐼","🐨","🐯","🦁","🐮","🐷","🐸","🐵","🐧","🦉","🦄","🐙",
];

/* ---------- 默认昵称 ---------- */
export const DEFAULT_NICK = "匿名用户";   // 启动兜底; /api/info 下发 defaultNick 覆盖(config.rs DEFAULT_NICK)

/* ---------- 话术模板 ---------- */
export const TIP_SHARE = "\n🔔 温馨提示：请在同一 Wi-Fi 下打开；文件请尽快下载到本地，共享文件不保证长期有效。";
export const TIP_COPY_URL = "\n\n🔔 温馨提示：请确保分享方和接收方在同一 Wi-Fi 下打开（不要使用魔法哦～）"
  + "\n👀 一定要看：文件请尽快下载到本地，共享文件不保证长期有效。";
export const TIP_QR = "🔔 请确保在同一 Wi-Fi 下打开；文件请尽快下载到本地";

/* ---------- 页脚 ---------- */
export const FOOTER_TEXT = "Copyright © 2026-Present 李航坤. All Rights Reserved.";

/* ---------- 主题迁移(v1.1.6: 皮肤 9 套精简为 6 套) ---------- */
/* main.js(挂载前预上色)与 App.vue(选择器高亮)都要用 —— 求值顺序是 App.vue 先于
   main.js 本体, 两端各自应用才能保证首启高亮与视觉一致 */
export const THEME_MIGRATE = { aurora: "default", mist: "graphite", eye: "mint" };

/* ---------- 版本更新(桌面端, GitHub Releases) ---------- */
export const GITHUB_REPO = "bdpqlhk/CC-Flash";   // 发布仓库(owner/name)
export const RELEASES_API = `https://api.github.com/repos/${GITHUB_REPO}/releases/latest`;
export const RELEASES_PAGE = `https://github.com/${GITHUB_REPO}/releases/latest`;
export const APP_VERSION_FALLBACK = "1.1.8";     // [同步: Cargo.toml version, release.sh 发版时自动改] 仅在服务端版本号未到达时展示
export const UPDATE_CHECK_DELAY_MS = 3000;               // 启动后延迟再静默检查(等 appVersion 到位)
export const UPDATE_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;   // 静默检查频控: 24h 一次
export const UPDATE_CHECK_TIMEOUT_MS = 8000;             // 请求超时; 任何失败一律静默
