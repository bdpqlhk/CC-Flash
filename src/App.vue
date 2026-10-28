<script setup>
import { ref, computed, watch, onMounted, onUnmounted, nextTick } from "vue";
import QRCode from "qrcode";
// 失联提示图: 构建期内联为 base64, 服务停止后无需再发请求
import noSignalImg from "./assets/no-signal.png";
// 可调参数集中管理(轮询/阈值/路径/文案等)
import {
  API, APP_VERSION_FALLBACK, AVATAR_POOL, BLOB_REVOKE_MS, COPIED_RESET_MS, CONTROL_RETRY_MS,
  CONTROL_RETRY_TIMES, DEFAULT_NICK, DEFAULT_PORT, FOOTER_TEXT, ICON_MAP, IMG_EXTS, LS, MSG_MAX,
  GITHUB_REPO, NARROW_MAX_PX, NICK_MAX, NATIVE_DOWNLOAD_OVER, OFFLINE_FAILS, PAGE_SIZE, POLL_CHAT_MS,
  POLL_FILES_MS, POLL_LOGS_MS, POLL_STATUS_MS, REFRESH_SPIN_MS, RELEASES_PAGE,
  REL_TIME_MS, TIP_COPY_URL, TIP_QR, TIP_SHARE, TOAST_MS, UPDATE_CHECK_DELAY_MS,
  UPDATE_CHECK_INTERVAL_MS, UPLOAD_CHUNK, UPLOAD_CHUNK_TIMEOUT_MS, UPLOAD_DONE_KEEP_MS,
  UPLOAD_RETRY_MS, UPLOAD_RETRY_TIMES, WS_HB_TIMEOUT_MS, WS_WATCHDOG_MS,
} from "./config";
import { fetchLatestRelease, isNewer } from "./update";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

/* ---------- 运行参数: 下面是启动兜底值, 服务端(/api/info、control_status_cmd)下发后以其为准 ---------- */
const appVersion = ref("");   // 应用版本, 唯一来源 Cargo.toml, 随状态接口下发
const cfg = ref({
  nickMax: NICK_MAX,
  msgMax: MSG_MAX,
  defaultNick: DEFAULT_NICK,
  poll: { chat: POLL_CHAT_MS, files: POLL_FILES_MS, status: POLL_STATUS_MS, logs: POLL_LOGS_MS },
});
const _posNum = (v, fb) => (Number.isFinite(v) && v > 0 ? v : fb);
function applyServerConfig(c) {
  if (!c || typeof c !== "object") return;
  const p = c.poll && typeof c.poll === "object" ? c.poll : {};
  const oldPoll = JSON.stringify(cfg.value.poll);
  cfg.value = {
    nickMax: _posNum(c.nickMax, cfg.value.nickMax),
    msgMax: _posNum(c.msgMax, cfg.value.msgMax),
    defaultNick: (typeof c.defaultNick === "string" && c.defaultNick) || cfg.value.defaultNick,
    poll: {
      chat: _posNum(p.chat, cfg.value.poll.chat),
      files: _posNum(p.files, cfg.value.poll.files),
      status: _posNum(p.status, cfg.value.poll.status),
      logs: _posNum(p.logs, cfg.value.poll.logs),
    },
  };
  if (JSON.stringify(cfg.value.poll) !== oldPoll) startPolling();   // 间隔有变 → 重排定时器
}

/* ---------- 环境 ---------- */
const isDesktop = "__TAURI_INTERNALS__" in window;
// 真实桌面 OS: isDesktop 在 Android/iOS 的 Tauri WebView 里同样为 true, 但 autostart/托盘
// 等桌面插件按 #[cfg(desktop)] 未注册 —— 依赖这些插件的 UI 必须用 isRealDesktop 门控
// iPadOS 13+ 伪装桌面 UA("Macintosh"), 单看 UA 会把 iPad 网页误判为真实桌面:
// 用 MacIntel + 多点触控补判(Apple 官方推荐手法)
const isIOSFamily = /iPhone|iPad|iPod/.test(navigator.userAgent)
  || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
const isRealDesktop = isDesktop && !isIOSFamily && !/Android/.test(navigator.userAgent);
const isIOS = isIOSFamily;   // Safari 不支持 webkitdirectory, 文件夹入口对 iOS/iPadOS 隐藏
const isWeChat = /MicroMessenger/i.test(navigator.userAgent);   // 微信内置浏览器: 下载被拦需引导到系统浏览器
// 桌面模式:窗口页面不在服务端口上,API 走回环地址;网页模式:相对路径即可
const apiBase = computed(() => (isDesktop ? `http://127.0.0.1:${port.value || DEFAULT_PORT}` : ""));
function tauriInvoke(cmd, args) { return window.__TAURI__.core.invoke(cmd, args || {}); }
/* 当前设备昵称(供日志/请求头使用) */
function deviceNick() { return nickInput.value.trim() || autoNick; }

/* ---------- 视图切换 ---------- */
const view = ref("files");   // files | chat | service | settings
const VIEW_TITLES = { files: "文件传输", chat: "文本消息", service: "服务中心", settings: "CC设置" };
function setView(v) { view.value = v; }

/* ---------- 外观主题 ---------- */
const THEMES = [
    { id: "default",  name: "深空蓝",   preview: "linear-gradient(135deg,#0d1017,#1a2233)" },
    { id: "graphite", name: "石墨灰",   preview: "linear-gradient(135deg,#15171b,#2a2f38)" },
    { id: "aurora",   name: "极光蓝",   preview: "linear-gradient(135deg,#0e1520,#12365e)" },
    { id: "eye",      name: "护眼暖黑", preview: "linear-gradient(135deg,#191611,#3a3222)" },
    { id: "purple",   name: "暗夜紫",   preview: "linear-gradient(135deg,#15121d,#33265a)" },
];
const theme = ref("default");
function applyTheme(t) {
    if (t && t !== "default") document.documentElement.dataset.theme = t;
    else delete document.documentElement.dataset.theme;
}
function setTheme(id) {
    theme.value = id;
    applyTheme(id);
    try { localStorage.setItem(LS.theme, id); } catch (e) {}
}
try { theme.value = localStorage.getItem(LS.theme) || "default"; } catch (e) {}

/* ---------- 手机端(窄屏): 悬浮球 + 底部聊天面板 ---------- */
const mq = window.matchMedia(`(max-width: ${NARROW_MAX_PX}px)`);
const isNarrow = ref(mq.matches);
const chatOpen = ref(false);
// 手机聊天面板打开时锁定背后页面, 防止滑动穿透带动共享文件页滚动
watch(chatOpen, v => { document.body.style.overflow = v ? "hidden" : ""; });
function onMqChange(e) {
  isNarrow.value = e.matches;
  if (!e.matches) closeChat();   // 回到宽屏时自动收起手机聊天面板
}
if (typeof mq.addEventListener === "function") mq.addEventListener("change", onMqChange);
else if (typeof mq.addListener === "function") mq.addListener(onMqChange);
function openChat() {
  chatOpen.value = true;
  seenChat.value = chatCount.value;
  nextTick(scrollChats);
}
function closeChat() { chatOpen.value = false; }
function scrollChats() {
  // 桌面聊天视图容器只有 ref(chatBox); #chatMessagesSheet 是常驻 DOM 的手机面板
  // (桌面端 display:none)—— 滚它等于没滚, 必须把 chatBox 一并滚到底
  for (const id of ["chatMessages", "chatMessagesSheet"]) {
    const c = document.getElementById(id);
    if (c) c.scrollTop = c.scrollHeight;
  }
  if (chatBox.value) chatBox.value.scrollTop = chatBox.value.scrollHeight;
}

/* ---------- 服务控制(桌面) ---------- */
const running = ref(false);
const port = ref(0);
const lanIp = ref("--");
const logs = ref([]);
const portInput = ref(DEFAULT_PORT);
const portDirty = ref(false);   // 用户手动改过端口: 停止状态下轮询不再回写, 防"打完字点开处被旧值冲掉"
const shareInput = ref("");
const freeSpace = ref("--");
async function refreshControl() {
  // 窗口就绪竞态下 invoke 可能短暂失败, 自动重试
  for (let i = 0; i < CONTROL_RETRY_TIMES; i++) {
    try {
      const d = isDesktop
        ? await tauriInvoke("control_status_cmd")
        : await (await fetch(API.info)).json();
      running.value = d.running;
      port.value = d.port;
      lanIp.value = d.ip;
      if (typeof d.version === "string") appVersion.value = d.version;
      applyServerConfig(d.config);
      if (d.share) shareInput.value = d.share;
      if (d.free) freeSpace.value = `${d.free} / ${d.total}`;
      syncPortInput(running.value, d.port);
      return;
    } catch (e) {
      await new Promise(r => setTimeout(r, CONTROL_RETRY_MS));
    }
  }
}
/* 端口输入框与服务实际端口同步: 运行中展示实际生效端口(输入框为禁用态, 安全,
   修复"改端口启动后输入框仍显示默认 1688"); 停止时仅在"用户没改过且未聚焦"时同步 ——
   否则用户输入新端口后点开处, 2.5s 轮询会用上次运行的旧端口把输入冲掉 */
function syncPortInput(isRunning, port) {
  if (isRunning) portInput.value = String(port);
  else if (!portDirty.value && port && document.activeElement?.tagName !== "INPUT") {
    portInput.value = String(port);
  }
}
async function startServer() {
  if (!isDesktop) { showToast("请在桌面端控制服务", "error"); return; }
  try {
    // 先应用共享目录(含持久化), 再启动 —— 否则输入框里的目录从未生效过
    const s = await tauriInvoke("set_share_dir_cmd", { path: shareInput.value });
    if (!s.ok) { showToast(s.error || "共享目录无效", "error"); return; }
    const d = await tauriInvoke("control_start_cmd", { port: Number(portInput.value) || null });
    if (d.ok) {
      running.value = true;
      port.value = d.port;
      portInput.value = String(d.port);   // 立即回显实际生效端口(含被占用顺延的情况)
      portDirty.value = false;
      showToast("服务已启动");
    }   // 二维码由 watch [shareUrl,running] 自动生成
    else showToast(d.error || "启动失败", "error");
  } catch (e) {
    console.error("启动服务异常:", e);
    showToast("启动服务异常: " + (e.message || e), "error");
  }
}
function openShareDir() {
  if (!isRealDesktop) { showToast("本机文件管理器不可见共享目录（应用专属存储），文件经 HTTP 提供", "error"); return; }
  tauriInvoke("open_path_cmd", { path: shareInput.value })
    .then(d => { if (!d.ok) showToast(d.error || "打开失败", "error"); })
    .catch(() => showToast("打开失败", "error"));
}
async function pickShareDir() {
  try {
    const sel = await openDialog({ directory: true, title: "选择共享目录" });
    if (!sel) return;   // 用户取消
    // 立即落盘: 后端更新后轮询回写的才是新目录, 否则 2.5s 内会被旧值冲掉
    const d = await tauriInvoke("set_share_dir_cmd", { path: sel });
    if (d.ok) {
      shareInput.value = sel;
      showToast("共享目录已更新，下次启动服务生效");
    } else {
      showToast(d.error || "设置失败", "error");
    }
  } catch (e) {
    // Android/iOS 的 dialog 插件未实现目录选择(直接返回错误) → 明确提示;
    // 真实桌面的失败(IPC 竞态等)如实报错, 不误报"平台不支持"
    showToast(isRealDesktop ? ("设置失败：" + (e?.message || e)) : "当前平台不支持目录选择，请手动输入路径", "error");
  }
}
function openFile(name) {
  if (!isRealDesktop) return;   // Android/iOS 无系统程序打开能力, 走下载路径
  const dir = shareInput.value.replace(/\/+$/, "");
  tauriInvoke("open_path_cmd", { path: dir + "/" + name })
    .then(d => { if (!d.ok) showToast(d.error || "打开失败", "error"); })
    .catch(() => showToast("打开失败", "error"));
}
function copyFileToClipboard(name) {
  tauriInvoke("copy_file_cmd", { name })
    .then(d => d.ok ? showToast("已复制到剪贴板，可在访达/资源管理器中粘贴") : showToast(d.error || "复制失败", "error"))
    .catch(() => showToast("复制失败", "error"));
}
async function stopServer() {
  if (!isDesktop) { running.value = false; return; }
  try {
    await tauriInvoke("control_stop_cmd");
    running.value = false;
    showToast("服务已停止");
  } catch (e) {
    console.error("停止服务异常:", e);
    showToast("停止服务异常", "error");
  }
}
function openBrowser() {
  if (!running.value) { showToast("服务未启动", "error"); return; }
  openExternal(shareUrl.value);
}

/* ---------- 二维码 ---------- */
// 单一数据源: watch [shareUrl, running] 驱动生成, 三处展示(服务中心/弹窗/介绍页)共用一份
const qrData = ref("");
const qrShow = ref(false);
function openQr() {
  // running 翻 false 的同一微任务里 watch 已清空 qrData, 不存在"瞬断但二维码仍在"的窗口期, 直接按状态拦截
  if (!running.value) { showToast("服务未启动，无法显示二维码", "error"); return; }
  qrShow.value = true;
}

/* ---------- 复制(降级兼容) ---------- */
function fallbackCopy(text, okMsg) {
  const ta = document.createElement("textarea");
  ta.value = text; ta.style.position = "fixed"; ta.style.opacity = "0";
  document.body.appendChild(ta); ta.select();
  let ok = false;
  try { ok = document.execCommand("copy"); } catch (e) {}
  ta.remove();
  showToast(ok ? okMsg : "复制失败，请手动复制", ok ? "success" : "error");
}
function copyText(text, okMsg) {
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text).then(() => showToast(okMsg)).catch(() => fallbackCopy(text, okMsg));
  } else fallbackCopy(text, okMsg);
}
const shareUrl = computed(() => `http://${lanIp.value}:${port.value}`);
// 地址(ip:端口)或运行状态一变就重生成二维码(覆盖 IP 变化/端口顺延/手动改端口/启停全场景);
// 停服清空避免残留旧码; await 结果落地前校验地址未再变, 丢弃乱序的过期结果
async function syncQr() {
  if (!running.value) { qrData.value = ""; return; }
  const url = shareUrl.value;
  try {
    const data = await QRCode.toDataURL(url, { margin: 1, width: 220 });
    if (shareUrl.value === url && running.value) qrData.value = data;
  } catch (e) { /* 生成失败由 refreshStatus 轮询兜底重试 */ }
}
watch([shareUrl, running], syncQr);
const copied = ref(false);
function copyUrl() {
  if (serviceDown()) { showToast("服务未启动", "error"); return; }
  const text = shareUrl.value + TIP_COPY_URL;
  copyText(text, "链接和提示已复制，发给同一局域网的小伙伴即可打开");
  copied.value = true;
  setTimeout(() => { copied.value = false; }, COPIED_RESET_MS);
}

/* ---------- 文件列表 ---------- */
const allFiles = ref([]);
const sortBy = ref("time");
const currentPage = ref(1);
const refreshing = ref(false);

function extFor(n) { const p = n.lastIndexOf("."); return p >= 0 ? n.slice(p + 1).toLowerCase() : ""; }
function isImage(n) { return IMG_EXTS.includes(extFor(n)); }
function iconFor(n) { return ICON_MAP[extFor(n)] || "FILE"; }
/* 大小格式化 [同步: server.rs humans] —— B 不带小数, 其余一位小数, 单位表两端一致 */
function fmtSize(b) {
  if (b === 0) return "0 B";
  const u = ["B","KB","MB","GB","TB"];
  let i = 0, v = b;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return v.toFixed(i === 0 ? 0 : 1) + " " + u[i];
}
function fmtTime(ts) {
  const d = new Date(ts * 1000), now = new Date(), diff = now - d;
  if (diff < REL_TIME_MS.justNow) return "刚刚";
  if (diff < REL_TIME_MS.minutes) return Math.floor(diff / REL_TIME_MS.justNow) + " 分钟前";
  if (diff < REL_TIME_MS.hours) return Math.floor(diff / REL_TIME_MS.minutes) + " 小时前";
  return d.toLocaleDateString("zh-CN") + " " + d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
}

const sortedFiles = computed(() => {
  const arr = [...allFiles.value];
  if (sortBy.value === "time") arr.sort((a, b) => b.modified - a.modified);
  else if (sortBy.value === "name") arr.sort((a, b) => a.name.localeCompare(b.name, "zh-CN"));
  else if (sortBy.value === "size") arr.sort((a, b) => b.size - a.size);
  return arr;
});
const totalPages = computed(() => Math.max(1, Math.ceil(sortedFiles.value.length / PAGE_SIZE)));
const pageFiles = computed(() => {
  const cur = Math.min(Math.max(1, currentPage.value), totalPages.value);
  const s = (cur - 1) * PAGE_SIZE;
  return sortedFiles.value.slice(s, s + PAGE_SIZE);
});
const pageItems = computed(() => {
  const t = totalPages.value, cur = currentPage.value, items = [];
  if (t <= 7) { for (let i = 1; i <= t; i++) items.push(i); return items; }
  items.push(1);
  if (cur > 3) items.push("…");
  for (let i = Math.max(2, cur - 1); i <= Math.min(t - 1, cur + 1); i++) items.push(i);
  if (cur < t - 2) items.push("…");
  items.push(t);
  return items;
});
function goPage(p) { currentPage.value = p; }
function setSort(s) { sortBy.value = s; currentPage.value = 1; }

const stats = computed(() => ({
  count: allFiles.value.length,
  size: fmtSize(allFiles.value.reduce((s, f) => s + (f.size || 0), 0)),
}));

/* ---------- 批量选择 ---------- */
const selected = ref(new Set());
const selectedCount = computed(() => selected.value.size);
const pageAllSelected = computed(() =>
  pageFiles.value.length > 0 && pageFiles.value.every(f => selected.value.has(f.name))
);
function toggleSel(name) {
  const s = new Set(selected.value);
  s.has(name) ? s.delete(name) : s.add(name);
  selected.value = s;
}
function toggleAll() {
  const page = pageFiles.value.map(f => f.name);
  const allOn = page.length > 0 && page.every(n => selected.value.has(n));
  const s = new Set(selected.value);
  page.forEach(n => allOn ? s.delete(n) : s.add(n));
  selected.value = s;
}
function clearSel() { selected.value = new Set(); }

let loadingFiles = false;
let filesThrottleTimer = null;
let lastFilesLoadAt = 0;
let reloadQueued = false;   // 在途时又收到刷新请求(如删除后) → 本轮结束后补拉一次, 不丢弃
async function loadFiles(manual = false) {
  if (loadingFiles) { reloadQueued = true; return; }
  // 非手动刷新做 800ms 合并节流: 批量上传完成时每个文件都调 loadFiles + 服务端各广播
  // 一次 files:changed, 500 个文件即数百次串行全量拉取(每个在线设备都拉)
  if (!manual && Date.now() - lastFilesLoadAt < 800) {
    if (!filesThrottleTimer) {
      filesThrottleTimer = setTimeout(() => { filesThrottleTimer = null; loadFiles(false); }, 800);
    }
    return;
  }
  loadingFiles = true;
  lastFilesLoadAt = Date.now();
  const spinStart = Date.now();
  if (manual) refreshing.value = true;
  try {
    // 昵称走 URL 参数: 避免 X-Nickname 自定义头触发 CORS 预检, 刷新只需一次往返
    const r = await fetch(apiBase.value + API.files + "?nick=" + encodeURIComponent(deviceNick()));
    const d = await r.json();
    allFiles.value = d.files || [];
    // 列表缩水(删除本页等)后回收越界页码: 否则页码条无高亮、‹ 可点到更越界的页
    if (currentPage.value > totalPages.value) currentPage.value = totalPages.value;
    // 清理已不存在的选中项
    if (selected.value.size) {
      const names = new Set(allFiles.value.map(f => f.name));
      const keep = [...selected.value].filter(n => names.has(n));
      if (keep.length !== selected.value.size) selected.value = new Set(keep);
    }
  } catch (e) { console.error(e); }
  loadingFiles = false;
  // 强制动画至少转一圈: 本地请求毫秒级完成, 否则"看起来没刷新"
  if (manual) {
    const wait = Math.max(0, REFRESH_SPIN_MS - (Date.now() - spinStart));
    setTimeout(() => { refreshing.value = false; }, wait);
  }
  if (reloadQueued) { reloadQueued = false; loadFiles(false); }
}

/* ---------- 分享: 全平台统一复制链接(带温馨提示话术) ---------- */
async function shareFile(name) {
  if (serviceDown()) { showToast("服务已停止，无法分享", "error"); return; }   // 停服后链接打不开, 先拦截
  const base = isDesktop ? `http://${lanIp.value}:${port.value}` : location.origin;
  const url = base + API.download(name);
  copyText(url + TIP_SHARE, "分享链接已复制：" + name);
}

/* ---------- 下载(浏览器端): fetch 回来再触发保存, 才有"下载完成"时机 ---------- */
/* 浏览器端原生下载: 锚点必须挂进 DOM 再 click —— iOS Safari/部分国产浏览器(夸克等)
   对"游离锚点"的 click 静默忽略; 也不用 location.href(会把当前页面导航走) */
function nativeDownload(url, filename) {
  const a = document.createElement("a");
  a.href = url;
  if (filename) a.download = filename;
  a.rel = "noopener";
  document.body.appendChild(a);
  a.click();
  a.remove();
}
async function downloadFileWeb(name) {
  const url = apiBase.value + API.download(name);
  const base = name.split("/").pop() || name;   // download 属性不带子目录路径(部分移动端拒收)
  try {
    const r = await fetch(url);
    if (!r.ok) {
      // HTTP 错误(404/400 等)如实报错, 不走原生回退 —— 否则会把 JSON 错误体当文件下载
      showToast(`下载失败：HTTP ${r.status}`, "error");
      return;
    }
    const len = Number(r.headers.get("Content-Length") || 0);
    // 手机浏览器/WebView 的 blob 内存配额远低于桌面(iOS Safari 数百 MB 即杀标签页且无报错),
    // 阈值降到 50MB, 超出一律走原生流式下载
    const over = (isIOSFamily || /Android/.test(navigator.userAgent))
      ? 50 * 1024 * 1024 : NATIVE_DOWNLOAD_OVER;
    if (len > over) {
      // 超大文件交给浏览器原生下载(流式省内存, 但无完成回调)
      try { r.body?.cancel(); } catch (e) {}
      nativeDownload(url, base);
      showToast("大文件较大，已交由浏览器直接下载");
      return;
    }
    const blob = await r.blob();
    const objUrl = URL.createObjectURL(blob);
    nativeDownload(objUrl, base);
    setTimeout(() => URL.revokeObjectURL(objUrl), BLOB_REVOKE_MS);
    showToast(`已下载：${name}${len ? " (" + fmtSize(len) + ")" : ""}`);
  } catch (e) {
    // blob 路径失败(内存/内核限制等) → 退回原生下载, 交给浏览器下载管理器
    nativeDownload(url, name.split("/").pop() || name);
    showToast("已交由浏览器下载");
  }
}

/* ---------- 删除(单个 / 批量) ---------- */
const delTargets = ref([]);
const showDel = ref(false);
// 服务是否不可用: 桌面看 running(按钮直控), 网页看 offline(失联判定)
function serviceDown() { return isDesktop ? !running.value : offline.value; }
// 服务不可用时先拦截删除, 给出友好提示, 而不是请求失败后报一串失败计数
function offlineGuard() {
  if (!serviceDown()) return false;
  showToast("服务已停止，无法删除", "error");
  return true;
}
function confirmDelete(name) {
  if (offlineGuard()) return;
  delTargets.value = [name]; showDel.value = true;
}
function confirmDeleteSelected() {
  if (!selected.value.size || offlineGuard()) return;
  delTargets.value = [...selected.value];
  showDel.value = true;
}
function closeDel() { showDel.value = false; delTargets.value = []; }
async function doDelete() {
  const targets = delTargets.value;
  showDel.value = false;
  if (offlineGuard()) return;   // 弹窗期间服务停掉的兜底
  let ok = 0, fail = 0;
  for (const n of targets) {
    try {
      const r = await fetch(apiBase.value + API.del(n), { method: "DELETE" });
      r.ok ? ok++ : fail++;
    } catch (e) { fail++; }
  }
  if (fail) showToast(`删除完成：成功 ${ok} 个，失败 ${fail} 个`, "error");
  else showToast(targets.length > 1 ? `已删除 ${ok} 个文件` : "已删除 「" + targets[0] + "」");
  selected.value = new Set();
  loadFiles(true);   // 手动刷新: 带旋转反馈; 与轮询在途竞争时由补拉机制保证不丢
}

/* ---------- 上传(分片 + 断点续传 + 整目录) ---------- */
const uploads = ref([]);
let upSeq = 0;
/* 上传并发闸: 同时最多 UPLOAD_CONCURRENCY 条上传链, 其余真正停在"等待中" */
const UPLOAD_CONCURRENCY = 5;
let upActive = 0;
const upQueue = [];
function pumpUploadQueue() {
  while (upActive < UPLOAD_CONCURRENCY && upQueue.length) {
    const fn = upQueue.shift();
    upActive++;
    Promise.resolve()
      .then(fn)
      .catch((e) => console.error("上传任务异常:", e))
      .finally(() => { upActive--; pumpUploadQueue(); });
  }
}
const dragOver = ref(false);
function pickFiles() { document.getElementById("fileInput").click(); }
// 移除上传条目(失败行不自动清理, 否则只能刷新页面才能清掉)
function removeUpload(id) { uploads.value = uploads.value.filter(u => u.id !== id); }
function pickDir() { document.getElementById("dirInput").click(); }
function onPick(e) { handleFiles(e.target.files); e.target.value = ""; }
function onPickDir(e) {
  // 选空文件夹与拖入空文件夹一致给反馈(原静默 return 像软件坏了)
  if (!e.target.files || !e.target.files.length) { showToast("文件夹为空或不含可上传文件", "error"); e.target.value = ""; return; }
  handleFiles(e.target.files); e.target.value = "";
}
function handleFiles(list) {
  if (!list || !list.length) return;
  for (const f of Array.from(list)) uploadFile(f);
}
/* 拖拽收集: 支持文件夹(webkitGetAsEntry 递归)与混合拖入, 目录结构经 webkitRelativePath 保留 */
async function collectDroppedFiles(dt) {
  const out = [];
  const entries = [];
  if (dt && dt.items) {
    for (const it of dt.items) {
      const en = it.webkitGetAsEntry && it.webkitGetAsEntry();
      if (en) entries.push(en);
      else { const f = it.getAsFile && it.getAsFile(); if (f) out.push(f); }
    }
  }
  if (!entries.length) return dt && dt.files ? Array.from(dt.files) : out;
  const walk = async (entry, prefix) => {
    if (entry.isFile) {
      const f = await new Promise((res) => entry.file(res, () => res(null)));
      if (f) {
        try { Object.defineProperty(f, "webkitRelativePath", { value: prefix + entry.name }); } catch (_) {}
        out.push(f);
      }
    } else if (entry.isDirectory) {
      const rd = entry.createReader();
      for (;;) {
        const batch = await new Promise((res) => rd.readEntries(res, () => res([])));
        if (!batch.length) break;
        for (const en of batch) await walk(en, prefix + entry.name + "/");
      }
    }
  };
  for (const en of entries) await walk(en, "");
  return out;
}
async function onDrop(e) {
  dragOver.value = false;
  const list = await collectDroppedFiles(e.dataTransfer);
  // 空文件夹拖入零反馈会让用户反复拖、以为软件坏了
  if (!list || !list.length) { showToast("文件夹为空或不含可上传文件", "error"); return; }
  handleFiles(list);
}
/* 上传名: 文件夹拖入/选择时为相对路径(如 目录/子目录/文件名), 普通文件为原名 */
function uploadRelName(file) { return file.webkitRelativePath || file.name; }
async function fetchServerReceived(name, upId) {
  // 返回 null 表示查询失败(网络抖动) —— 与"服务端确认为 0"严格区分:
  // 失败时调用方保持当前 offset 不动(下一片若与服务端断点错位, 服务端 409 会带
  // 真实断点兜底重排); 若把失败当 0, 重试链会 offset=0 → 下一片 truncate
  // 掉数 GB 服务端断点从头重传(AP 漫游/休眠双重失败即触发)
  try {
    const r = await fetch(apiBase.value + API.uploadProgress(name, upId));
    if (!r.ok) return null;
    const d = await r.json();
    return Number.isFinite(d.received) ? d.received : null;
  } catch (_) { return null; }
}
async function sendChunk(name, upId, totalSize, offset, chunk) {
  // 超时保护: 半死连接(无 RST 的断网/AP 休眠)靠它触发续传, 而不是挂到浏览器 TCP 超时
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), UPLOAD_CHUNK_TIMEOUT_MS);
  try {
    const r = await fetch(apiBase.value + API.upload, {
      method: "POST",
      headers: {
        "X-Filename": encodeURIComponent(name),
        "X-File-Size": String(totalSize),
        "X-Upload-Offset": String(offset),
        "X-Upload-Id": upId,
        "Content-Type": "application/octet-stream",
      },
      body: chunk,
      signal: ctrl.signal,
    });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) {
      const err = new Error(d.error || `HTTP ${r.status}`);
      err.received = d.received;
      err.server = !!d.error;   // 服务端业务错误(如"正被其他设备上传"), 重试文案应如实展示
      // 4xx 业务错误重试无意义(路径非法/超长等确定性失败), 快速失败;
      // 408/429/409 例外: 超时/限流/续传重排值得重试
      err.fatal = r.status >= 400 && r.status < 500
        && ![408, 409, 429].includes(r.status);
      throw err;
    }
    return d;
  } finally { clearTimeout(timer); }
}
function uploadFile(file) {
  // 停服拦截(与删除/分享一致): 否则每个文件先探断点再 8×1.5s 空转重试才报错,
  // 粘贴大量文件即大量并发失败链 + toast 刷屏
  if (serviceDown()) { showToast("服务未启动，无法上传", "error"); return; }
  const id = "up-" + Date.now() + "-" + upSeq++;
  const name = uploadRelName(file);
  const total = file.size;
  // 会话 id = 文件稳定指纹(名+大小+修改时间): 页面重载/锁屏后重选同一文件仍命中
  // 服务端已有断点(自动续传真正生效); 内容变化(大小/修改时间变)则视为新会话覆盖旧断点。
  // name 必须 encodeURIComponent: 请求头值是 ByteString, 原始中文/emoji 在所有浏览器/WebView
  // 里 fetch 直接抛 TypeError(中文文件名上传全线失败); 服务端原样存 meta 原样比对, 自洽
  const upId = `${encodeURIComponent(name)}|${file.size}|${file.lastModified}`;
  // 同一文件已在队列中(等待/上传中)则跳过: 快速双击/多选重复会以相同 upId 并发推进,
  // 两条链互相截断断点导致整体重传数次、UI 出现重复进度行
  if (uploads.value.some(u => u.upId === upId && (u.status === "waiting" || u.status === "uploading"))) {
    return;
  }
  uploads.value.push({ id, upId, name, pct: total ? 0 : 100, speed: "", status: "waiting" });
  const stat = () => uploads.value.find(u => u.id === id);
  // 并发闸: 浏览器(尤其 Safari/WebKit)每 host 仅 6 条 HTTP/1.1 连接, 且 WS/轮询/缩略图
  // 已占槽 —— 一次选几百个文件全并发时, 排队请求会被单片 30s 超时(从 fetch 调用即计时)
  // 误杀, 集体报"上传中断"。入队后真正排队等待, 超时只计真实请求时间
  upQueue.push(async () => {
    // 断点续传: 服务端已有本次会话的断点则从其长度继续(覆盖 锁屏/断网 中断的场景);
    // 断点比本地文件还长 → 内容必然不一致, 强制从头重传
    let serverReceived = await fetchServerReceived(name, upId);
    if (serverReceived === null) {
      // 查询失败重试一次再放弃: 误把失败当 0 会让首片 truncate 掉服务端数 GB 断点
      await new Promise((r) => setTimeout(r, UPLOAD_RETRY_MS));
      serverReceived = (await fetchServerReceived(name, upId)) ?? 0;
    }
    if (serverReceived > total) serverReceived = 0;
    let offset = serverReceived;
    let base = offset;
    let t0 = Date.now();
    let attempts = 0;
    for (;;) {
      const it = stat(); if (!it) return;
      it.status = "uploading";
      try {
        // do-while: 0 字节文件也要发出至少一次请求(空 body, 服务端直接转正)
        do {
          const chunk = file.slice(offset, Math.min(offset + UPLOAD_CHUNK, total));
          const d = await sendChunk(name, upId, total, offset, chunk);
          offset = Math.min(
            Number.isFinite(d.received) ? d.received : offset + chunk.size,
            total
          );
          attempts = 0; // 成功一片清零连续失败计数(重试额度是连续性的, 不是全文件配额)
          const it2 = stat(); if (!it2) return;
          it2.pct = total ? Math.round((offset / total) * 100) : 100;
          it2.speed = fmtSize((offset - base) / Math.max((Date.now() - t0) / 1000, 0.1)) + "/s";
        } while (offset < total);
        const it3 = stat(); if (it3) { it3.pct = 100; it3.status = "done"; it3.speed = ""; }
        loadFiles();   // 不再强跳第 1 页: 批量上传会把正在第 5 页浏览/勾选的用户每秒拽回去, 页码收缩由 loadFiles 内回收
        setTimeout(() => { uploads.value = uploads.value.filter(u => u.id !== id); }, UPLOAD_DONE_KEEP_MS);
        return;
      } catch (e) {
        const it4 = stat(); if (!it4) return;
        if (e && e.fatal) {
          // 确定性业务失败(如"路径过长"): 直接报错, 不做 8×1.5s 无意义空转
          it4.status = "error";
          it4.speed = "";
          showToast("上传失败: " + name + " — " + (e.message || "请求被拒绝"), "error");
          return;
        }
        attempts += 1;
        if (attempts > UPLOAD_RETRY_TIMES) {
          it4.status = "error";
          it4.speed = "";   // 清掉"自动续传中"残留文案: 否则失败条目永远显示它, 用户以为还在重试
          showToast("上传中断: " + name, "error");
          return;
        }
        // 服务端业务错误(如"该文件正被其他设备上传")如实展示, 仅网络类异常才提示自动续传
        it4.speed = e && e.server ? String(e.message).slice(0, 24) : "网络中断，自动续传中…";
        await new Promise((r) => setTimeout(r, UPLOAD_RETRY_MS));
        // 以服务端断点为准重排; 查询失败(null)保持当前 offset 不动 ——
        // 服务端重启清了断点则下一片 409 会带回真实进度, 网络抖动误清零数 GB 断点
        const recv = await fetchServerReceived(name, upId);
        if (recv !== null) {
          offset = recv > total ? 0 : recv;
          base = offset; t0 = Date.now();
        }
      }
    }
  });
  pumpUploadQueue();
}
function onPaste(e) {
  const items = e.clipboardData && e.clipboardData.items;
  if (!items) return;
  const files = [];
  // getAsFile() 在富文本剪贴板场景可返回 null, 不过滤会在 uploadFile 里抛 TypeError
  for (const item of items) {
    if (item.kind === "file") {
      const f = item.getAsFile();
      if (f) files.push(f);
    }
  }
  if (files.length) handleFiles(files);
}

/* ---------- 聊天 ---------- */
const chatMsgs = ref([]);
const chatInput = ref("");
const nickInput = ref("");
const seenChat = ref(0);
const chatCount = ref(0);
const chatBox = ref(null);
const unread = computed(() => Math.max(0, chatCount.value - seenChat.value));
const curNick = computed(() => nickInput.value.trim() || autoNick);
function avatarFor(nick) {
  const pool = AVATAR_POOL;
  let h = 0;
  const s = nick || cfg.value.defaultNick;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  return pool[Math.abs(h) % pool.length];
}
const autoNick = (() => {
  try {
    let n = localStorage.getItem(LS.nickAuto) || "";
    if (!n) {
      const ua = navigator.userAgent;
      let os = "设备";
      if (/iPhone/i.test(ua)) os = "iPhone";
      else if (/iPad/i.test(ua)) os = "iPad";
      else if (/Android/i.test(ua)) os = "Android";
      else if (/Macintosh|Mac OS X/i.test(ua)) os = "Mac";
      else if (/Windows/i.test(ua)) os = "Windows";
      else if (/Linux/i.test(ua)) os = "Linux";
      let br = "";
      if (/Edg\//.test(ua)) br = " Edge";
      else if (/Chrome\//.test(ua)) br = " Chrome";
      else if (/Firefox\//.test(ua)) br = " Firefox";
      else if (/Safari\//.test(ua) && !/Chrome/.test(ua)) br = " Safari";
      n = `${os}${br}-${Math.floor(1000 + Math.random() * 9000)}`;
      localStorage.setItem(LS.nickAuto, n);
    }
    return n;
  } catch (e) { return cfg.value.defaultNick; }
})();
try { nickInput.value = localStorage.getItem(LS.nick) || autoNick; } catch (e) {}
function saveNick() {
  try { localStorage.setItem(LS.nick, nickInput.value.trim()); } catch (e) {}
}
/* 聊天文本链接化: 先转义防 XSS, 再把 URL 包成可点击链接(桌面走系统浏览器) */
function escHtml(s) { return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;"); }
/* 轻量 Markdown: 围栏代码块 / 行内代码 / 粗体 / 斜体 / 删除线 / 链接 */
function mdChat(raw) {
  let s = escHtml(String(raw));
  const stash = [];
  const keep = (html) => { stash.push(html); return `\u0000${stash.length - 1}\u0000`; };
  s = s.replace(/```[\w+-]*\r?\n?([\s\S]*?)```/g,
    (m0, code) => keep(`<pre class="chat-code"><code>${code.replace(/\n+$/, "")}</code></pre>`));
  s = s.replace(/`([^`\n]+)`/g, (m0, c) => keep(`<code class="chat-inline">${c}</code>`));
  s = s.replace(/(https?:\/\/[^\s<>\u0000]+)/gi,
    (u) => `<span class="chat-link" data-url="${u}" title="点击跳转到浏览器">${u}</span>`);
  s = s.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/(^|[^*\w])\*([^*\n]+)\*/g, "$1<em>$2</em>");
  s = s.replace(/~~([^~\n]+)~~/g, "<del>$1</del>");
  s = s.replace(/\n/g, "<br>");
  return s.replace(/\u0000(\d+)\u0000/g, (m0, i) => stash[Number(i)]);
}
const displayMsgs = computed(() => chatMsgs.value.map(m => ({ ...m, html: mdChat(m.message) })));
/* 聊天内链接点击: 注册在组件生命周期内(挂载/卸载成对), 避免热重载时重复监听 */
function onDocChatClick(e) {
  const link = e.target.closest(".chat-link");
  if (!link) return;
  const url = link.dataset.url;
  if (!url) return;
  openExternal(url);
}
/* 清空显示: 仅清本窗口(与 PyQt 镜像清空语义一致); 新消息到达后恢复显示 */
const chatCleared = ref(false);
const clearedTs = ref(0);   // 清空水位: 消息时间戳(服务端封顶 200 条, 用条数做水位在满 200 时永不解除)
function clearChatDisplay() {
  chatCleared.value = true;
  // 水位取最后一条消息的时间戳而非条数: 条数 ≤200 恒成立, "length > 水位条数" 永假 → 满 200 条时
  // 点清空后本机永久空白; 时间戳在服务端重启(消息清零)后依然单调可比
  clearedTs.value = chatMsgs.value.length ? chatMsgs.value[chatMsgs.value.length - 1].timestamp : 0;   // 无消息时任意新消息即解除
  showToast("已清空数据（不影响其他设备与服务端记录）");
}
let loadingChat = false;
let reloadChatQueued = false;
async function loadChat() {
  // 在途互斥 + 补拉(与 loadFiles 对齐): 刷屏时每个 chat:new 都触发全量拉取,
  // 后发先至的旧响应会覆盖新数据 → 消息回闪
  if (loadingChat) { reloadChatQueued = true; return; }
  loadingChat = true;
  try {
    // 超时保护: 无 AbortController 时单个挂死请求会让 loadingChat 恒 true,
    // 互斥把影响面从"单次"放大为"聊天停更直至浏览器默认超时(~5min)"
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), 10000);
    const r = await fetch(apiBase.value + API.chat + "?nick=" + encodeURIComponent(deviceNick()), { signal: ctl.signal });
    clearTimeout(timer);
    const d = await r.json();
    const msgs = d.messages || [];
    const old = chatCount.value;
    if (chatCleared.value && msgs.some(m => m.timestamp > clearedTs.value)) chatCleared.value = false;
    chatMsgs.value = chatCleared.value ? [] : msgs;
    chatCount.value = msgs.length;
    if (view.value === "chat" || chatOpen.value || (!isDesktop && !isNarrow)) seenChat.value = chatCount.value;
    if (msgs.length !== old || !loadChat._init) {
      loadChat._init = true;
      nextTick(scrollChats);
    }
  } catch (e) { console.error(e); }
  loadingChat = false;
  if (reloadChatQueued) { reloadChatQueued = false; loadChat(); }
}
async function sendChat() {
  const text = chatInput.value.trim();
  if (!text) return;
  const nickname = curNick.value;
  saveNick();
  try {
    const r = await fetch(apiBase.value + API.chat + "?nick=" + encodeURIComponent(nickname), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ nickname, message: text }),
    });
    const d = await r.json();
    if (d.ok) { chatInput.value = ""; loadChat(); }
    else showToast(d.error || "发送失败", "error");
  } catch (e) { showToast("发送失败", "error"); }
}

/* ---------- 提示 ---------- */
const toasts = ref([]);
let toastSeq = 0;
function showToast(msg, type = "success") {
  const id = ++toastSeq;
  toasts.value.push({ id, msg, type });
  setTimeout(() => { toasts.value = toasts.value.filter(t => t.id !== id); }, TOAST_MS);
}

/* ---------- 软件介绍页(桌面端全屏 overlay, 设计稿见 docs/软件介绍页设计稿.html) ---------- */
const introShow = ref(false);
const introSec = ref("feat");   // scroll spy 当前所在区块
const INTRO_FAQS = [
  { q: "手机需要安装 App 吗？", a: "不需要。手机、平板在同一 Wi-Fi 下用浏览器扫码或输入地址，即可使用全部功能（上传、下载、聊天、删除）。Android 用户如果想要原生体验，也可以直接安装我们提供的 APK。" },
  { q: "文件会上传到外网或云服务器吗？", a: "不会。HTTP 服务只监听你的电脑，所有传输都发生在局域网内部，不经过任何第三方服务器。拔掉外网网线，功能一切照常。" },
  { q: "端口被占用了怎么办？", a: "默认端口 1688，被占用时自动向后顺延探测（最多 20 个），当前生效地址会实时显示在顶栏与二维码中；也可以在「服务中心」手动指定端口。" },
  { q: "关闭窗口后服务会停止吗？", a: "不会。关窗只是把窗口收进系统托盘，服务继续保持运行，手机端不受影响；开机自启默认关闭，可在「CC设置」中手动开启。真正退出请点托盘菜单中的「退出」。" },
  { q: "支持哪些平台？", a: "桌面端提供 macOS 与 Windows 安装包（Linux 可从源码构建）；手机端浏览器免安装即可使用，Android 另有可直接安装的 APK。所有设备需处于同一局域网（同一 Wi-Fi）。" },
];
const faqOpen = ref(-1);
function toggleFaq(i) { faqOpen.value = faqOpen.value === i ? -1 : i; }
let introSpy = null;
function openIntro() {
  introShow.value = true;
  introSec.value = "feat";   // 重置导航高亮: 上次停在 FAQ 等区块时, 顶部没有区块触发 spy 自纠
  nextTick(() => {
    const el = document.getElementById("introOverlay");
    if (el) el.scrollTop = 0;
    startIntroSpy();
  });
}
function closeIntro() { introShow.value = false; stopIntroSpy(); }   // 停掉 scroll spy, 避免常驻壳上观察器泄漏
watch(introShow, (v) => { document.body.style.overflow = v ? "hidden" : ""; });
function goServiceFromIntro() { closeIntro(); setView("service"); }
/* Hero「立即开始传输」→ 关介绍页后进文件传输(顶栏「去启动服务」才去服务中心) */
function goTransferFromIntro() { closeIntro(); setView("files"); }
function introScrollTo(sec) {
  const root = document.getElementById("introOverlay");
  const el = document.getElementById("sec-" + sec);
  if (root && el) root.scrollTo({ top: el.offsetTop - 56, behavior: "smooth" });   // 56px ≈ 粘性顶栏 64px 留边
}
/* 顶栏品牌「回到顶部」: 滚回 Hero 顶部(而非功能区), 导航高亮回落到「功能」 */
function introScrollTop() {
  const root = document.getElementById("introOverlay");
  if (root) root.scrollTo({ top: 0, behavior: "smooth" });
  introSec.value = "feat";
}
function startIntroSpy() {
  stopIntroSpy();
  const root = document.getElementById("introOverlay");
  if (!root || typeof IntersectionObserver === "undefined") return;
  introSpy = new IntersectionObserver((entries) => {
    for (const en of entries) if (en.isIntersecting) introSec.value = en.target.dataset.sec;
  }, { root, rootMargin: "-40% 0px -55% 0px" });
  root.querySelectorAll(".sec[data-sec]").forEach((s) => introSpy.observe(s));
}
function stopIntroSpy() { if (introSpy) { introSpy.disconnect(); introSpy = null; } }
/* 外链打开的唯一入口: 桌面走系统浏览器(open_url_cmd), 失败退回 window.open;
   silent=true 不弹成功/失败提示(更新弹框等自带上下文的场景用)。
   openBrowser / goDownload / 聊天链接 / 介绍页链接统一经此, 避免多份拷贝各自漂移 */
function openExternal(url, silent = false) {
  if (!isDesktop) { window.open(url, "_blank"); return; }
  tauriInvoke("open_url_cmd", { url })
    .then((d) => {
      if (d.ok) { if (!silent) showToast("已在系统浏览器打开"); }
      else {
        if (!silent) showToast(d.error || "打开失败，尝试备用方式", "error");
        window.open(url, "_blank");
      }
    })
    .catch(() => { window.open(url, "_blank"); });
}

/* ---------- WebSocket 推送(聊天/列表/状态), 轮询降级为兜底通道 ---------- */
let ws = null;
let wsUrl_ = null;
let wsRetryTimer = null;
let wsRetryDelay = 1000;
let wsWatchdogTimer = null;
let lastWsMsg = 0;   // 最近一次收到 WS 消息(含服务端 15s 心跳)的时间戳
function wsEndpoint() {
  if (isDesktop) return port.value ? `ws://127.0.0.1:${port.value}${API.ws}` : null;
  return (location.protocol === "https:" ? "wss://" : "ws://") + location.host + API.ws;
}
function scheduleWsRetry() {
  clearTimeout(wsRetryTimer);
  // 桌面端服务已停时停止重试, 等 running 变化再连; 网页端持续退避重试(服务回来自动恢复)
  if (isDesktop && !running.value) return;
  wsRetryTimer = setTimeout(() => {
    wsRetryDelay = Math.min(wsRetryDelay * 2, 10000);
    connectWs();
  }, wsRetryDelay);
}
function closeWs() {
  clearTimeout(wsRetryTimer);
  wsRetryTimer = null;
  wsRetryDelay = 1000;
  if (ws) {
    const w = ws;
    ws = null;
    w.onopen = w.onmessage = w.onclose = w.onerror = null;
    try { w.close(); } catch (_) {}
  }
  wsUrl_ = null;
  applyWsMode();
}
function connectWs() {
  const url = wsEndpoint();
  if (!url) return;
  if (ws && wsUrl_ === url) return;   // 已连或将连同一地址
  closeWs();                          // 地址变化(换端口)先清场
  wsUrl_ = url;
  try { ws = new WebSocket(url); } catch (_) { scheduleWsRetry(); return; }
  ws.onopen = () => {
    wsRetryDelay = 1000;
    lastWsMsg = Date.now();
    applyWsMode();
    loadChat();     // 连接即全量同步一次, 补上断线窗口期
    loadFiles();
  };
  ws.onmessage = (e) => {
    lastWsMsg = Date.now();   // 任何消息(含 hb 心跳)都证明连接活着
    let ev;
    try { ev = JSON.parse(e.data); } catch (_) { return; }
    if (ev.type === "chat:new") loadChat();
    else if (ev.type === "files:changed") loadFiles();
    else if (ev.type === "status:up" || ev.type === "status:down") refreshStatus();
  };
  ws.onclose = () => { ws = null; wsUrl_ = null; applyWsMode(); scheduleWsRetry(); };
  ws.onerror = () => { try { ws && ws.close(); } catch (_) {} };
}
// WS 在线时停掉聊天/列表轮询(省电省请求); 断开时恢复轮询兜底
function applyWsMode() {
  const live = ws && ws.readyState === WebSocket.OPEN;
  clearInterval(chatTimer); chatTimer = null;
  clearInterval(filesTimer); filesTimer = null;
  if (!live) {
    chatTimer = setInterval(loadChat, cfg.value.poll.chat);
    filesTimer = setInterval(() => loadFiles(false), cfg.value.poll.files);
  }
}
// 服务启停/端口变化时管理连接
watch([running, port], () => {
  if (running.value && port.value) connectWs();
  else { closeWs(); }
});

/* ---------- 生命周期 ---------- */
let chatTimer = null;
let logTimer = null;
let filesTimer = null;
let statusTimer = null;
/* 轮询定时器统一启停: 服务端下发间隔变化时重排; 聊天/列表由 applyWsMode 按 WS 状态增减 */
function startPolling() {
  stopPolling();
  statusTimer = setInterval(refreshStatus, cfg.value.poll.status);   // 失联判定 ≈ OFFLINE_FAILS × 间隔
  if (isDesktop) logTimer = setInterval(refreshLogs, cfg.value.poll.logs);
  applyWsMode();
}
function stopPolling() {
  clearInterval(chatTimer);
  clearInterval(logTimer);
  clearInterval(statusTimer);
  clearInterval(filesTimer);
}
onMounted(() => {
  document.addEventListener("paste", onPaste);
  document.addEventListener("click", onDocChatClick);
  // WS 半开连接看门狗: 超时收不到任何消息(含服务端心跳) → 强制关闭,
  // onclose 恢复轮询兜底并退避重连 —— 桌面端状态轮询走 invoke 永远成功, 无此看门狗会永久卡死
  wsWatchdogTimer = setInterval(() => {
    if (ws && ws.readyState === WebSocket.OPEN && Date.now() - lastWsMsg > WS_HB_TIMEOUT_MS) {
      try { ws.close(); } catch (_) {}
    }
  }, WS_WATCHDOG_MS);
  // F5 刷新文件列表(输入框聚焦时除外)
  const f5 = (e) => {
    if (e.key === "F5" && !/INPUT|TEXTAREA/.test(document.activeElement?.tagName || "")) {
      e.preventDefault(); loadFiles(true);
    }
  };
  document.addEventListener("keydown", f5);
  onUnmounted(() => document.removeEventListener("keydown", f5));
  // Esc 关闭介绍页
  const escIntro = (e) => { if (e.key === "Escape" && introShow.value) closeIntro(); };
  document.addEventListener("keydown", escIntro);
  onUnmounted(() => document.removeEventListener("keydown", escIntro));
  if (isDesktop) {
    refreshControl();
    refreshAutostart();
    // 启动静默检查更新: 延迟触发(等 appVersion 下发到位) + 24h 频控, 失败/无更新均无感
    setTimeout(() => {
      if (Date.now() - lastCheckedAt() >= UPDATE_CHECK_INTERVAL_MS) checkUpdate(false);
    }, UPDATE_CHECK_DELAY_MS);
  }
  loadFiles();
  loadChat();
  refreshStatus();
  startPolling();
});
onUnmounted(() => {
  document.removeEventListener("paste", onPaste);
  document.removeEventListener("click", onDocChatClick);
  clearInterval(wsWatchdogTimer);
  clearTimeout(filesThrottleTimer);
  document.body.style.overflow = "";
  stopIntroSpy();
  closeWs();
  stopPolling();
});

async function refreshLogs() {
  try {
    const r = await fetch(apiBase.value + API.logs);
    const d = await r.json();
    logs.value = d.logs || [];
  } catch (e) {}
}

async function clearLogs() {
  try {
    if (isDesktop) await tauriInvoke("control_clear_logs_cmd");
    else await fetch(apiBase.value + API.logsClear, { method: "POST" });
    logs.value = [];
    showToast("日志已清空");
  } catch (e) {
    showToast("清空日志失败", "error");
  }
}

/* ---------- 版本更新(桌面端): 检查 → 弹框 → 浏览器下载 ---------- */
const showUpdate = ref(false);
const updateInfo = ref(null);      // { latest, notes, url }
const checkingUpdate = ref(false);
function markChecked() {
  try { localStorage.setItem(LS.updateCheck, String(Date.now())); } catch (e) {}
}
function lastCheckedAt() {
  try { return Number(localStorage.getItem(LS.updateCheck)) || 0; } catch (e) { return 0; }
}
function curVersion() { return appVersion.value || APP_VERSION_FALLBACK; }
async function checkUpdate(manual = false) {
  // 仅真实桌面检查更新: Android 的「前往下载」走 xdg-open 必失败, 弹框是死路;
  // 手动点击时给反馈, 不做哑巴按钮
  if (!isRealDesktop) {
    if (manual) showToast("请在电脑端检查更新（手机端请到 Releases 页按机型下载 APK）", "error");
    return;
  }
  if (checkingUpdate.value) return;
  checkingUpdate.value = true;
  const rel = await fetchLatestRelease();
  checkingUpdate.value = false;
  if (!rel) {
    // 失败不写 24h 频控: 开机瞬间 Wi-Fi 未就绪的一次抖动不应导致一整天不再检查
    if (manual) showToast("检查更新失败，需要能访问 GitHub，请稍后再试", "error");
    return;
  }
  markChecked();
  if (!isNewer(rel.latest, curVersion())) {
    if (manual) showToast(`当前已是最新版本 v${curVersion()}`);
    return;
  }
  updateInfo.value = rel;
  showUpdate.value = true;   // 弹出即记频控: 24h 内"稍后再说"不再打扰
}
function goDownload() {
  const url = updateInfo.value?.url || RELEASES_PAGE;
  openExternal(url, true);   // 更新弹框自身有完整上下文, 不再叠加打开提示
  showUpdate.value = false;
}

/* ---------- 开机自动启动(桌面端, tauri-plugin-autostart) ---------- */
// 产品决策: 纯手动开启 —— 首次运行不注册登录项, 用户在 CC设置 打开开关才写 LaunchAgent/注册表。
// (旧版首启静默注册会绑定当时执行的二进制路径: dev/DMG/移动应用位置后自启项即失效且无法自纠)
const autostartOn = ref(false);
const showAutostartTip = ref(false);   // 手动开启成功后的说明弹框(TCode 风格)
async function refreshAutostart() {
  if (!isRealDesktop) return;
  try {
    // 只读系统真实状态, 不做任何写入
    const d = await tauriInvoke("plugin:autostart|is_enabled");
    autostartOn.value = !!d;
  } catch (e) { /* 桌面端竞态/插件未就绪: 静默 */ }
}
async function toggleAutostart() {
  if (!isRealDesktop) { showToast("开机自启仅桌面版支持", "error"); return; }
  const next = !autostartOn.value;
  try {
    if (next) {
      await tauriInvoke("plugin:autostart|enable");
      autostartOn.value = true;
      showAutostartTip.value = true;   // 手动开启也弹说明, 与 TCode 交互一致
    } else {
      await tauriInvoke("plugin:autostart|disable");
      autostartOn.value = false;
      showToast("已关闭开机自动启动");
    }
  } catch (e) {
    showToast("设置失败，请重试", "error");
  }
}

/* ---------- 运行状态/IP 定期刷新: DHCP 换 IP 后地址栏/二维码/统计保持最新 ---------- */
let lastLanIp = "";
let statusFail = 0;
const offline = ref(false);   // 浏览器端与服务失联(服务停止/地址变更)
async function refreshStatus() {
  try {
    const d = isDesktop
      ? await tauriInvoke("control_status_cmd")
      : await (await fetch(API.info)).json();
    statusFail = 0;
    const wasOffline = offline.value;
    offline.value = false;
    if (wasOffline) {
      // 断网自愈: 恢复的第一时间提示 + 立即全量重载, 不等下一轮轮询/WS 事件
      showToast("已恢复与服务的连接");
      loadFiles();
      loadChat();
      connectWs();
    }
    running.value = d.running;
    port.value = d.port;
    syncPortInput(running.value, d.port);
    if (typeof d.version === "string") appVersion.value = d.version;
    applyServerConfig(d.config);
    if (d.share && document.activeElement?.tagName !== "INPUT") shareInput.value = d.share;
    if (d.free) freeSpace.value = `${d.free} / ${d.total}`;
    const ip = d.ip || "";
    if (ip && ip !== lanIp.value) {
      const first = !lastLanIp;
      const changed = lastLanIp && lastLanIp !== "127.0.0.1";
      lanIp.value = ip;
      lastLanIp = ip;
      if (changed && !first && running.value) {
        showToast(`本机 IP 已变为 ${ip}，手机请改用新地址访问`, "error");
      }
    } else if (ip) {
      lastLanIp = ip;
    }
    // 二维码主驱动是 watch [shareUrl, running]; 这里只做自愈兜底 ——
    // 生成失败/仍在途导致 qrData 为空时, 下一轮状态轮询(2.5s)重试
    if (running.value && !qrData.value) syncQr();
  } catch (e) {
    // 浏览器端连续失败 → 判定与服务失联: 状态回落为"已停止", 共享列表/聊天数据一并清空不再展示
    if (!isDesktop && ++statusFail >= OFFLINE_FAILS) {
      offline.value = true;
      running.value = false;
      allFiles.value = [];
      selected.value = new Set();
      chatMsgs.value = [];
      chatCount.value = 0;
      seenChat.value = 0;
      chatCleared.value = false;
      clearedTs.value = 0;
    }
  }
}
</script>

<template>
  <div class="shell">
    <!-- 微信内置浏览器拦非微信域名下载: 顶部常驻引导(右上角 ⋯ → 在浏览器打开) -->
    <div v-if="isWeChat" class="wx-banner">ⓘ 当前在微信内打开，文件下载可能被拦截 —— 点右上角 <b>⋯</b> 选择「<b>在浏览器打开</b>」即可正常传输</div>
    <!-- ===== 侧边栏(仅桌面壳) ===== -->
    <aside v-if="isDesktop" class="sidebar">
      <div class="brand">
        <div class="logo"><img class="logo-img" src="/logo.png" alt="logo" /></div>
        <div class="t">CC-Flash</div>
        <button class="intro-entry" title="软件介绍" aria-label="软件介绍" @click="openIntro">
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>
          介绍
        </button>
      </div>
      <nav class="nav">
        <button class="nav-item" :class="{ active: view === 'files' }" @click="setView('files')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z"/></svg></span>文件传输
        </button>
        <button v-if="isDesktop || !isNarrow" class="nav-item" :class="{ active: view === 'chat' }" @click="setView('chat')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 01-2 2H7l-4 4V5a2 2 0 012-2h14a2 2 0 012 2z"/></svg></span>文本消息
          <span v-if="unread > 0 && view !== 'chat'" class="nav-badge">{{ unread > 99 ? "99+" : unread }}</span>
        </button>
        <button v-if="isDesktop || !isNarrow" class="nav-item svc" :class="{ active: view === 'service' }" @click="setView('service')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg></span>服务中心
        </button>
        <button v-if="isDesktop || !isNarrow" class="nav-item" :class="{ active: view === 'settings' }" @click="setView('settings')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 01-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z"/></svg></span>CC设置
        </button>
      </nav>
      <div class="side-foot">
        <div class="side-status" :class="{ on: running }">
          <span class="d"></span>{{ running ? `运行中 :${port}` : "已停止" }}
        </div>
        <div class="side-ver">{{ appVersion ? "v" + appVersion + " · Tauri" : "Tauri" }}</div>
      </div>
    </aside>

    <!-- ===== 主区 ===== -->
    <main class="main" :class="{ 'chat-full': isDesktop && view === 'chat', 'web': !isDesktop }">
      <!-- 顶部栏(桌面壳):标题一行 + 地址栏独占一行(保证地址完整展示) -->
      <div v-if="isDesktop" class="topbar">
        <div class="topbar-head">
          <h2>{{ VIEW_TITLES[view] || VIEW_TITLES.files }}</h2>
          <div class="sub">{{ running ? "同一 Wi-Fi 下的设备可访问下方地址" : "服务未启动 —— 到「服务中心」页启动" }}</div>
        </div>
        <div class="topbar-url">
          <span class="chip" :class="running ? 'run' : 'stop'">
            <span class="dots" aria-hidden="true"><i></i><i></i><i></i></span>{{ running ? "运行中" : "○ 已停止" }}
          </span>
          <div class="url-box">
            <span class="protocol">HTTP</span>
            <input class="url-text" :value="running ? shareUrl : '服务未启动'" readonly @click="$event.target.select()" />
          </div>
          <button class="icon-btn" :class="{ 'icon-ok': copied }" :title="copied ? '已复制' : '复制链接'"
                  aria-label="复制链接" @click="copied || copyUrl()">
            <svg v-if="!copied" width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1"/></svg>
            <svg v-else width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>
          </button>
          <button class="icon-btn" title="显示二维码" aria-label="显示二维码" @click="openQr">
            <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><line x1="14" y1="14" x2="14" y2="17"/><line x1="17" y1="14" x2="21" y2="14"/><line x1="14" y1="17" x2="14" y2="21"/><line x1="17" y1="21" x2="21" y2="21"/><line x1="21" y1="17" x2="21" y2="21"/></svg>
          </button>
          <button v-if="isDesktop" class="icon-btn" title="在系统浏览器打开网页端" aria-label="在浏览器打开网页端" @click="openBrowser">
            <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 014 10 15.3 15.3 0 01-4 10 15.3 15.3 0 01-4-10 15.3 15.3 0 014-10z"/></svg>
          </button>
        </div>
      </div>

      <!-- 网页端头部(PyQt 风格): 标题 + 地址栏 + 状态统计 -->
      <template v-else>
        <header class="web-head">
          <div class="web-logo"><img class="logo-img" src="/logo.png" alt="logo" /></div>
          <div>
            <h2>CC-Flash</h2>
            <div class="sub">局域网高速文件共享</div>
          </div>
        </header>
        <div class="card web-top">
          <div class="web-urlrow">
          <span class="chip" :class="running ? 'run' : 'stop'">
            <span class="dots" aria-hidden="true"><i></i><i></i><i></i></span>{{ running ? "运行中" : "○ 已停止" }}
          </span>
          <div class="url-box">
            <span class="protocol">HTTP</span>
            <input class="url-text" :value="running ? shareUrl : '服务未启动'" readonly @click="$event.target.select()" />
          </div>
          <button class="icon-btn" :class="{ 'icon-ok': copied }" :title="copied ? '已复制' : '复制链接'"
                  aria-label="复制链接" @click="copied || copyUrl()">
            <svg v-if="!copied" width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1"/></svg>
            <svg v-else width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>
          </button>
          <button class="icon-btn" title="显示二维码" aria-label="显示二维码" @click="openQr">
            <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><line x1="14" y1="14" x2="14" y2="17"/><line x1="17" y1="14" x2="21" y2="14"/><line x1="14" y1="17" x2="14" y2="21"/><line x1="17" y1="21" x2="21" y2="21"/><line x1="21" y1="17" x2="21" y2="21"/></svg>
          </button>
        </div>
        <div class="web-stats">
          <div class="stat-cell"><div class="s-label">本机 IP</div><div class="s-val ip">{{ lanIp }}</div></div>
          <div class="stat-cell"><div class="s-label">端口</div><div class="s-val">{{ port || "--" }}</div></div>
          <div class="stat-cell"><div class="s-label">文件数</div><div class="s-val">{{ stats.count }}</div></div>
          <div class="stat-cell"><div class="s-label">总大小</div><div class="s-val">{{ stats.size }}</div></div>
        </div>
        </div>
      </template>

      <!-- 文件共享 -->
      <template v-if="isDesktop && view === 'files'">
        <div v-if="isDesktop && !running" class="warn-banner">
          <span>🔒 服务未启动 —— 手机暂时无法访问</span>
          <button class="btn btn-primary" @click="setView('service')">去启动</button>
        </div>
        <div class="card">
          <h3>上传文件</h3>
          <div class="dropzone" :class="{ 'drag-over': dragOver }" @click="pickFiles"
               @dragover.prevent="dragOver = true" @dragleave.prevent="dragOver = false"
               @dragend.prevent="dragOver = false" @drop.prevent="onDrop">
            <div class="ic">⬆️</div>
            <h5>点击选择文件 或 拖拽到此处</h5>
            <p>支持多选与<b>整个文件夹</b>（保持目录结构） · 可直接 Ctrl+V / Cmd+V 粘贴文件
              <span v-if="!isIOS"> · <a class="dir-pick" @click.stop="pickDir" title="选择文件夹上传">选择文件夹</a></span>
              · 大文件断网/锁屏后自动续传</p>
            <input type="file" id="fileInput" multiple style="display:none" @change="onPick" />
            <input type="file" id="dirInput" webkitdirectory multiple style="display:none" @change="onPickDir" />
          </div>
          <div v-for="u in uploads" :key="u.id" class="upload-item">
            <div class="up-line">
              <span class="up-name">{{ u.name }}</span>
              <span class="up-status" :class="u.status">
                {{ u.status === "uploading" ? u.pct + "% " + u.speed
                 : u.status === "waiting" ? "等待中"
                 : u.status === "done" ? "完成" : u.speed || "失败" }}
              </span>
              <button v-if="u.status === 'error'" class="icon-btn" title="移除"
                      style="margin-left:6px" @click="removeUpload(u.id)">✕</button>
            </div>
            <div class="up-bar-wrap"><div class="up-bar-fill" :class="u.status" :style="{ width: u.pct + '%' }"></div></div>
          </div>
        </div>

        <div class="card">
          <div class="list-head">
            <div class="lh-left">
              <h3>共享文件</h3>
              <span class="badge">{{ stats.count }} 个文件</span>
            </div>
            <div class="lh-right">
              <label v-if="pageFiles.length" class="sel-all" title="全选/取消本页">
                <input type="checkbox" :checked="pageAllSelected" @change="toggleAll" />全选
              </label>
              <div class="sort-group">
                <button class="sort-btn" :class="{ active: sortBy === 'time' }" @click="setSort('time')">最新</button>
                <button class="sort-btn" :class="{ active: sortBy === 'name' }" @click="setSort('name')">名称</button>
                <button class="sort-btn" :class="{ active: sortBy === 'size' }" @click="setSort('size')">大小</button>
              </div>
              <button v-if="selectedCount > 0" class="btn btn-del" style="height:28px;padding:0 12px"
                      @click="confirmDeleteSelected" title="删除选中的文件">
                🗑 删除选中 ({{ selectedCount }})
              </button>
              <button v-if="selectedCount > 0" class="refresh-btn" title="取消选择" @click="clearSel">✕</button>
              <button class="refresh-btn" :class="{ spinning: refreshing }" title="刷新文件列表" @click="loadFiles(true)">↻</button>
            </div>
          </div>
          <div v-if="pageFiles.length === 0" class="empty-state">
            <div class="es-icon">📦</div>
            <h4>还没有文件</h4>
            <p>拖拽文件到上方区域开始传输</p>
            <button class="btn btn-primary" @click="pickFiles">⬆ 立即上传</button>
          </div>
          <div v-else>
            <div v-for="f in pageFiles" :key="f.name" class="file-row" @dblclick="isRealDesktop && openFile(f.name)">
              <label class="sel-box" @click.stop>
                <input type="checkbox" :checked="selected.has(f.name)" @change="toggleSel(f.name)" />
              </label>
              <div class="file-thumb">
                <img v-if="isImage(f.name)" :src="apiBase + API.download(f.name)" loading="lazy"
                     @error="$event.target.style.display = 'none'" />
                <span class="icon-overlay">{{ iconFor(f.name) }}</span>
              </div>
              <div class="file-info">
                <div class="fname" :title="f.name" @click="shareFile(f.name)">{{ f.name }}</div>
                <div class="fmeta">
                  <span>{{ fmtSize(f.size) }}</span><span>{{ fmtTime(f.modified) }}</span>
                  <span v-if="isImage(f.name)" style="color:var(--warning);font-size:10px">预览</span>
                </div>
              </div>
              <div class="file-btns">
                <!-- 真实桌面: 打开 + 复制; Android/iOS 壳: 下载(打开/复制在移动端是死路) -->
                <template v-if="isRealDesktop">
                  <button class="btn-act fb-open" title="打开文件（系统默认程序）" @click="openFile(f.name)"><svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z"/></svg></button>
                  <button class="btn-act fb-copy" title="复制文件到剪贴板（可在访达/资源管理器粘贴）" @click="copyFileToClipboard(f.name)"><svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1"/></svg></button>
                </template>
                <button v-else class="btn-act fb-dl" title="下载" @click="downloadFileWeb(f.name)"><svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg></button>
                <button class="btn-act fb-del" title="删除" @click="confirmDelete(f.name)"><svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a2 2 0 012-2h4a2 2 0 012 2v2"/></svg></button>
                <button class="btn-act fb-share" title="分享（复制链接）" @click="shareFile(f.name)"><svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><line x1="8.59" y1="13.51" x2="15.42" y2="17.49"/><line x1="15.41" y1="6.51" x2="8.59" y2="10.49"/></svg></button>
              </div>
            </div>
            <div class="pagination" v-if="totalPages > 1">
              <button class="page-btn" :disabled="currentPage === 1" @click="goPage(currentPage - 1)">‹</button>
              <template v-for="(it, i) in pageItems" :key="i">
                <span v-if="it === '…'" class="page-ellipsis">…</span>
                <button v-else class="page-btn" :class="{ active: it === currentPage }" @click="goPage(it)">{{ it }}</button>
              </template>
              <button class="page-btn" :disabled="currentPage === totalPages" @click="goPage(currentPage + 1)">›</button>
            </div>
          </div>
        </div>

        <div class="hint-card">
          <h4>使用说明</h4>
          <ol>
            <li>电脑点「启动服务」（服务中心页），手机扫码或输入地址打开本页</li>
            <li>上传：拖拽 / 点击选择 / Ctrl+V 粘贴截图，均支持批量</li>
            <li>下载：点蓝色按钮；分享：点绿色按钮（手机拉起系统分享，电脑复制链接）</li>
            <li>聊天在左侧「文本消息」，手机/电脑消息实时互通</li>
          </ol>
        </div>
      </template>

      <!-- 聊天室 -->
      <template v-if="isDesktop && view === 'chat'">
        <div class="chat-card">
          <div class="chat-header">
            <h3>💬 文本消息 · 局域网内实时互通</h3>
            <div class="chat-head-right">
              <button class="btn btn-ghost" style="height:28px" title="仅清空本窗口的显示，不影响其他设备与服务端记录"
                      @click="clearChatDisplay">清空数据</button>
              <span class="avatar-chip" title="头像由昵称自动生成">{{ avatarFor(curNick) }}</span>
              <div class="chat-nickname">
                <span style="font-size:11px;color:var(--text3)">昵称:</span>
                <input v-model="nickInput" :placeholder="cfg.defaultNick" :maxlength="cfg.nickMax" title="输入昵称，会自动记住" @change="saveNick" />
              </div>
            </div>
          </div>
          <div class="chat-messages" ref="chatBox">
            <div v-if="displayMsgs.length === 0" class="chat-empty">👋 还没有消息，发一条打个招呼吧</div>
            <div v-for="m in displayMsgs" :key="m.id" class="chat-message">
              <span class="msg-avatar">{{ avatarFor(m.nickname) }}</span>
              <div class="msg-body">
                <div class="chat-nick">{{ m.nickname }}</div>
                <div class="chat-text" v-html="m.html"></div>
                <div class="chat-time">{{ fmtTime(m.timestamp) }}</div>
              </div>
            </div>
          </div>
          <div class="chat-input-area">
            <input class="chat-input" v-model="chatInput" placeholder="说点什么…（回车发送）" :maxlength="cfg.msgMax"
                   @keydown.enter.prevent="sendChat" />
            <button class="btn btn-primary send-btn" @click="sendChat">发送</button>
          </div>
        </div>
      </template>

      <!-- 服务中心 -->
      <template v-if="isDesktop && view === 'service'">
        <div class="svc-top">
          <!-- 服务控制 -->
          <div class="card svc-ctrl">
            <div class="svc-head">
              <div class="svc-status-row">
                <span class="chip" :class="running ? 'run' : 'stop'">
                  <span class="dots" aria-hidden="true"><i></i><i></i><i></i></span>{{ running ? "运行中" : "○ 已停止" }}
                </span>
                <span v-if="running" class="svc-url">{{ shareUrl }}</span>
                <button v-if="running" class="icon-btn svc-copy-btn" :class="{ 'icon-ok': copied }"
                        :title="copied ? '已复制' : '复制地址'" aria-label="复制地址" @click="copied || copyUrl()">
                  <svg v-if="!copied" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1"/></svg>
                  <svg v-else width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>
                </button>
              </div>
              <div class="svc-hint">同一 Wi-Fi 下的设备用浏览器打开即可使用</div>
            </div>
            <div class="svc-fields">
              <div class="svc-field">
                <label>端口</label>
                <input class="svc-input" v-model="portInput" :disabled="running" maxlength="5"
                       @input="portDirty = true" />
                <div class="field-hint">被占用自动顺延</div>
              </div>
              <div class="svc-field grow">
                <label>共享目录</label>
                <div class="dir-row">
                  <input class="svc-input" style="flex:1;min-width:0" v-model="shareInput" :disabled="running" :title="shareInput" />
                  <button class="btn btn-ghost" :disabled="running" @click="pickShareDir">选择</button>
                  <button class="btn btn-ghost" :disabled="running" @click="openShareDir">打开</button>
                </div>
                <div class="field-hint">💾 可用 {{ freeSpace }} · 修改目录需重启服务生效</div>
              </div>
            </div>
            <div class="svc-actions">
              <button class="btn btn-primary" :disabled="running" @click="startServer">启动服务</button>
              <button class="btn btn-danger" :disabled="!running" @click="stopServer">停止服务</button>
              <button class="btn btn-ghost" :disabled="!running" @click="openShareDir">打开共享目录</button>
            </div>
          </div>
          <!-- 手机扫码直达 -->
          <div class="card svc-qr">
            <h3>手机扫码直达</h3>
            <img v-if="qrData" class="qr-img" :src="qrData" alt="QR" />
            <div v-else class="qr-empty">启动服务后<br />显示二维码</div>
            <div class="qr-caption">手机 / 平板同一 Wi-Fi 下扫码打开</div>
          </div>
        </div>
        <!-- 服务日志: 独占整行 -->
        <div class="card svc-log-card">
          <div class="list-head">
            <h3>服务日志</h3>
            <button class="btn btn-ghost" style="height:28px" title="清空服务日志显示（不影响服务运行）"
                    @click="clearLogs">清空日志</button>
          </div>
          <div class="svc-box" style="flex:1;display:flex;flex-direction:column;margin-bottom:0;min-height:0">
            <div class="logs" style="flex:1;height:auto">{{ logs.length ? logs.join("\n") : "HTTP 请求与服务事件…" }}</div>
          </div>
        </div>
        <!-- 皮肤设置 -->
        <div class="card svc-skin">
          <h3>皮肤设置</h3>
          <div class="theme-row">
            <button v-for="t in THEMES" :key="t.id" class="theme-sw" :class="{ active: theme === t.id }"
                    :title="t.name" @click="setTheme(t.id)">
              <span class="sw" :style="{ background: t.preview }"></span>
              <span>{{ t.name }}</span>
            </button>
          </div>
          <div class="sub-hint">主题即时生效并自动记忆，所有页面同步换色</div>
        </div>
        <!-- 关于 -->
        <div class="card svc-about">
          <h3>关于</h3>
          <p class="about-text">CC-Flash（曹操是快😂）—— 局域网文件互传工具：电脑启动服务，手机/平板/电脑浏览器打开地址即可上传/下载/分享/聊天。</p>
          <div class="about-ver">
            <span class="ver-chip">当前版本 v{{ curVersion() }}</span>
            <button class="btn btn-ghost" style="height:28px" :disabled="checkingUpdate" @click="checkUpdate(true)">
              {{ checkingUpdate ? "检查中…" : "检查更新" }}
            </button>
          </div>
        </div>
      </template>

      <!-- CC设置 -->
      <template v-if="isDesktop && view === 'settings'">
        <div class="card" v-if="isRealDesktop">
          <h3>通用</h3>
          <div class="setting-row">
            <div class="setting-info">
              <div class="setting-title">开机自动启动</div>
              <div class="setting-desc">开启后登录电脑时 CC-Flash 将在后台自动启动（默认关闭，仅在你打开开关时注册）</div>
            </div>
            <button class="switch" :class="{ on: autostartOn }" role="switch" :aria-checked="autostartOn"
                    :title="autostartOn ? '点击关闭' : '点击开启'" @click="toggleAutostart">
              <span class="knob"></span>
            </button>
          </div>
        </div>
        <!-- Android/iOS 的 Tauri WebView 也走桌面壳(isDesktop), 但桌面插件未注册, 给出说明而非坏开关 -->
        <div class="card" v-else>
          <h3>通用</h3>
          <p class="about-text">当前平台暂无可配置项 —— 开机自动启动等桌面功能仅在 macOS / Windows 版提供。</p>
        </div>
      </template>

      <!-- 网页端(PyQt 风格): 左列上传+文件, 右列聊天室 -->
      <div v-if="!isDesktop" class="web-cols">
        <div class="web-lcol">
          <div class="card">
            <h3>上传文件</h3>
            <div class="dropzone" :class="{ 'drag-over': dragOver }" @click="pickFiles"
                 @dragover.prevent="dragOver = true" @dragleave.prevent="dragOver = false"
                 @dragend.prevent="dragOver = false" @drop.prevent="onDrop">
              <div class="ic">⬆️</div>
              <h5>点击选择文件 或 拖拽到此处</h5>
              <p>支持多选与<b>整个文件夹</b>（保持目录结构） · 可直接 Ctrl+V / Cmd+V 粘贴文件
                <span v-if="!isIOS"> · <a class="dir-pick" @click.stop="pickDir" title="选择文件夹上传">选择文件夹</a></span>
                · 大文件断网/锁屏后自动续传</p>
              <input type="file" id="fileInput" multiple style="display:none" @change="onPick" />
              <input type="file" id="dirInput" webkitdirectory multiple style="display:none" @change="onPickDir" />
            </div>
            <div v-for="u in uploads" :key="u.id" class="upload-item">
              <div class="up-line">
                <span class="up-name">{{ u.name }}</span>
                <span class="up-status" :class="u.status">
                  {{ u.status === "uploading" ? u.pct + "% " + u.speed
                   : u.status === "waiting" ? "等待中"
                   : u.status === "done" ? "完成" : u.speed || "失败" }}
                </span>
                <button v-if="u.status === 'error'" class="icon-btn" title="移除"
                        style="margin-left:6px" @click="removeUpload(u.id)">✕</button>
              </div>
              <div class="up-bar-wrap"><div class="up-bar-fill" :class="u.status" :style="{ width: u.pct + '%' }"></div></div>
            </div>
          </div>

          <div class="card">
            <div class="list-head">
              <div class="lh-left">
                <h3>共享文件</h3>
                <span class="badge">{{ stats.count }} 个文件</span>
              </div>
              <div class="lh-right">
                <label v-if="pageFiles.length" class="sel-all" title="全选/取消本页">
                  <input type="checkbox" :checked="pageAllSelected" @change="toggleAll" />全选
                </label>
                <div class="sort-group">
                  <button class="sort-btn" :class="{ active: sortBy === 'time' }" @click="setSort('time')">最新</button>
                  <button class="sort-btn" :class="{ active: sortBy === 'name' }" @click="setSort('name')">名称</button>
                  <button class="sort-btn" :class="{ active: sortBy === 'size' }" @click="setSort('size')">大小</button>
                </div>
                <button v-if="selectedCount > 0" class="btn btn-del" style="height:28px;padding:0 12px"
                        @click="confirmDeleteSelected" title="删除选中的文件">
                  🗑 删除选中 ({{ selectedCount }})
                </button>
                <button v-if="selectedCount > 0" class="refresh-btn" title="取消选择" @click="clearSel">✕</button>
                <button class="refresh-btn" :class="{ spinning: refreshing }" title="刷新文件列表" @click="loadFiles(true)">↻</button>
              </div>
            </div>
            <div v-if="pageFiles.length === 0" class="empty-state">
              <div class="es-icon">📦</div>
              <h4>还没有文件</h4>
              <p>拖拽文件到上方区域开始传输</p>
              <button class="btn btn-primary" @click="pickFiles">⬆ 立即上传</button>
            </div>
            <div v-else>
              <div v-for="f in pageFiles" :key="f.name" class="file-row">
                <label class="sel-box" @click.stop>
                  <input type="checkbox" :checked="selected.has(f.name)" @change="toggleSel(f.name)" />
                </label>
                <div class="file-thumb">
                  <img v-if="isImage(f.name)" :src="apiBase + API.download(f.name)" loading="lazy"
                       @error="$event.target.style.display = 'none'" />
                  <span class="icon-overlay">{{ iconFor(f.name) }}</span>
                </div>
                <div class="file-info">
                  <div class="fname" :title="f.name" @click="shareFile(f.name)">{{ f.name }}</div>
                  <div class="fmeta">
                    <span>{{ fmtSize(f.size) }}</span><span>{{ fmtTime(f.modified) }}</span>
                    <span v-if="isImage(f.name)" style="color:var(--warning);font-size:10px">预览</span>
                  </div>
                </div>
                <div class="file-btns">
                  <!-- 浏览器端: 下载 + 删除 + 分享 -->
                  <button class="btn-act fb-dl" title="下载" @click="downloadFileWeb(f.name)"><svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg></button>
                  <button class="btn-act fb-del" title="删除" @click="confirmDelete(f.name)"><svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a2 2 0 012-2h4a2 2 0 012 2v2"/></svg></button>
                  <button class="btn-act fb-share" title="分享（复制链接）" @click="shareFile(f.name)"><svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><line x1="8.59" y1="13.51" x2="15.42" y2="17.49"/><line x1="15.41" y1="6.51" x2="8.59" y2="10.49"/></svg></button>
                </div>
              </div>
              <div class="pagination" v-if="totalPages > 1">
                <button class="page-btn" :disabled="currentPage === 1" @click="goPage(currentPage - 1)">‹</button>
                <template v-for="(it, i) in pageItems" :key="i">
                  <span v-if="it === '…'" class="page-ellipsis">…</span>
                  <button v-else class="page-btn" :class="{ active: it === currentPage }" @click="goPage(it)">{{ it }}</button>
                </template>
                <button class="page-btn" :disabled="currentPage === totalPages" @click="goPage(currentPage + 1)">›</button>
              </div>
            </div>
          </div>
        </div>

        <div class="web-rcol">
          <div class="chat-card">
            <div class="chat-header">
              <h3>💬 聊天室</h3>
              <div class="chat-head-right">
                <button class="btn btn-ghost" style="height:28px" title="仅清空本窗口的显示，不影响其他设备与服务端记录"
                        @click="clearChatDisplay">清空数据</button>
                <span class="avatar-chip" title="头像由昵称自动生成">{{ avatarFor(curNick) }}</span>
                <div class="chat-nickname">
                  <span style="font-size:11px;color:var(--text3)">昵称:</span>
                  <input v-model="nickInput" :placeholder="cfg.defaultNick" :maxlength="cfg.nickMax" title="输入昵称，会自动记住" @change="saveNick" />
                </div>
              </div>
            </div>
            <div class="chat-messages" id="chatMessages">
              <div v-if="displayMsgs.length === 0" class="chat-empty">👋 还没有消息，发一条打个招呼吧</div>
              <div v-for="m in displayMsgs" :key="m.id" class="chat-message">
                <span class="msg-avatar">{{ avatarFor(m.nickname) }}</span>
                <div class="msg-body">
                  <div class="chat-nick">{{ m.nickname }}</div>
                  <div class="chat-text" v-html="m.html"></div>
                  <div class="chat-time">{{ fmtTime(m.timestamp) }}</div>
                </div>
              </div>
            </div>
            <div class="chat-input-area">
              <input class="chat-input" v-model="chatInput" placeholder="说点什么…（回车发送）" :maxlength="cfg.msgMax"
                     @keydown.enter.prevent="sendChat" />
              <button class="btn btn-primary send-btn" @click="sendChat">发送</button>
            </div>
          </div>
        </div>
      </div>

      <!-- 页脚: 版权(桌面/网页端共用; 内容不满一屏时贴底) -->
      <footer class="app-foot">{{ FOOTER_TEXT }}</footer>
    </main>
  </div>

  <!-- 悬浮球(手机端) -->
  <button v-if="isNarrow && !chatOpen" class="chat-fab" title="聊天室" @click="openChat">
    💬<span v-show="unread > 0" class="fab-badge">{{ unread > 99 ? "99+" : unread }}</span>
  </button>

  <!-- 底部聊天面板(手机端) -->
  <div class="chat-sheet" :class="{ open: chatOpen }">
    <div class="sheet-backdrop" @click="closeChat"></div>
    <div class="sheet-panel">
      <div class="sheet-handle" @click="closeChat"><div class="handle-bar"></div></div>
      <div id="sheet-chat-host">
        <div class="chat-card in-sheet">
          <div class="chat-header">
            <h2>📝 聊天室</h2>
            <div class="chat-head-right">
              <button class="btn btn-ghost" style="height:28px" title="仅清空本窗口的显示，不影响其他设备与服务端记录"
                      @click="clearChatDisplay">清空数据</button>
              <span class="avatar-chip">{{ avatarFor(curNick) }}</span>
              <div class="chat-nickname">
                <span style="font-size:11px;color:var(--text3)">昵称:</span>
                <input v-model="nickInput" :placeholder="cfg.defaultNick" :maxlength="cfg.nickMax" title="输入昵称，会自动记住" @change="saveNick" />
              </div>
              <button class="icon-btn sheet-only" title="收起" aria-label="收起聊天面板" @click="closeChat">
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
              </button>
            </div>
          </div>
          <div class="chat-messages" id="chatMessagesSheet">
            <div v-if="displayMsgs.length === 0" class="chat-empty">👋 还没有消息，发一条打个招呼吧</div>
            <div v-for="m in displayMsgs" :key="m.id" class="chat-message">
              <span class="msg-avatar">{{ avatarFor(m.nickname) }}</span>
              <div class="msg-body">
                <div class="chat-nick">{{ m.nickname }}</div>
                <div class="chat-text" v-html="m.html"></div>
                <div class="chat-time">{{ fmtTime(m.timestamp) }}</div>
              </div>
            </div>
          </div>
          <div class="chat-input-area">
            <input class="chat-input" v-model="chatInput" placeholder="说点什么…（回车发送）" :maxlength="cfg.msgMax"
                   @keydown.enter.prevent="sendChat" />
            <button class="btn btn-primary send-btn" @click="sendChat">发送</button>
          </div>
        </div>
      </div>
    </div>
  </div>

  <!-- 软件介绍页(桌面端全屏 overlay, 按 docs/软件介绍页设计稿.html 实现); v-if 延迟到首次打开才挂载 -->
  <Transition name="intro">
    <div v-if="isDesktop && introShow" id="introOverlay" class="intro-overlay" role="dialog" aria-label="CC-Flash 软件介绍">
    <!-- 粘性顶栏 -->
    <header class="ihead">
      <div class="ihead-in">
        <span class="ihead-brand" title="回到顶部" @click="introScrollTop">
          <img class="ib-lg" src="/logo.png" alt="logo" /> CC-Flash
        </span>
        <nav class="inav">
          <a href="#sec-feat" :class="{ active: introSec === 'feat' }" @click.prevent="introScrollTo('feat')">功能</a>
          <a href="#sec-howto" :class="{ active: introSec === 'howto' }" @click.prevent="introScrollTo('howto')">如何使用</a>
          <a href="#sec-safe" :class="{ active: introSec === 'safe' }" @click.prevent="introScrollTo('safe')">技术实现</a>
          <a href="#sec-faq" :class="{ active: introSec === 'faq' }" @click.prevent="introScrollTo('faq')">常见问题</a>
        </nav>
        <div class="ihead-act">
          <button class="btn btn-primary" @click="goServiceFromIntro">
            去启动服务
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><line x1="7" y1="17" x2="17" y2="7"/><polyline points="7 7 17 7 17 17"/></svg>
          </button>
          <button class="icon-x" title="关闭 (Esc)" aria-label="关闭" @click="closeIntro">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
          </button>
        </div>
      </div>
    </header>

    <!-- Hero -->
    <section class="hero">
      <div>
        <span class="hero-chip">局域网直传 · 手机免装 · 开源免费</span>
        <h1>文件传过去，<br /><span class="hl">同一个 Wi-Fi</span> 就够了。</h1>
        <p class="lead">
          电脑启动服务，手机、平板、同事的电脑打开浏览器扫码即可上传、下载、发消息。
          数据不出局域网 —— 不上云、无账号、不经过任何第三方服务器，断外网照样用。
        </p>
        <div class="hero-cta">
          <button class="btn btn-primary" @click="goTransferFromIntro">
            立即开始传输
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><line x1="5" y1="12" x2="19" y2="12"/><polyline points="12 5 19 12 12 19"/></svg>
          </button>
          <button class="btn btn-ghost" title="查看使用说明" @click="introScrollTo('howto')">
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" style="opacity:.85"><path d="M4 19.5A2.5 2.5 0 016.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 014 19.5v-15A2.5 2.5 0 016.5 2z"/></svg>
            查看使用说明
          </button>
        </div>
        <div class="hero-feats">
          <span class="feat-chip"><span class="tick">✓</span>手机免装 App</span>
          <span class="feat-chip"><span class="tick">✓</span>拖拽 / 粘贴上传</span>
          <span class="feat-chip"><span class="tick">✓</span>大文件流式直传</span>
          <span class="feat-chip"><span class="tick">✓</span>开机自动启动</span>
        </div>
      </div>
      <!-- 应用窗口示意(二维码为真实地址, 服务未启动时占位) -->
      <div class="hero-visual">
        <div class="hv-bar">
          <i></i><i></i><i></i>
          <span class="hv-url">{{ running ? shareUrl : "http://192.168.1.5:1688" }}</span>
          <span class="hv-run"><i></i>{{ running ? "运行中" : "示例" }}</span>
        </div>
        <div class="hv-body">
          <div class="hv-qr">
            <img v-if="qrData" class="hv-qr-img" :src="qrData" alt="QR" />
            <div v-else class="hv-qr-ph"><span>📱</span>扫码打开</div>
          </div>
          <div class="hv-files">
            <div class="hv-file">
              <span class="fi" style="background:linear-gradient(135deg,#e74c3c,#c0392b)">PDF</span>
              <div style="min-width:0"><div class="fn">需求评审纪要.pdf</div><div class="fm">2.4 MB · 电脑 → 手机</div></div>
              <span class="fp">100%</span>
            </div>
            <div class="hv-file">
              <span class="fi" style="background:linear-gradient(135deg,#3498db,#2980b9)">JPG</span>
              <div style="min-width:0"><div class="fn">现场照片_01.jpg</div><div class="fm">5.1 MB · 手机 → 电脑</div></div>
              <span class="fp">100%</span>
            </div>
            <div class="hv-file">
              <span class="fi" style="background:linear-gradient(135deg,#2ecc71,#27ae60)">ZIP</span>
              <div style="min-width:0"><div class="fn">release-build.zip</div><div class="fm">128 MB · 传输中</div></div>
              <span class="fp" style="color:var(--accent-text)">76%</span>
            </div>
          </div>
        </div>
        <div class="hv-cap">手机扫码打开即用 · 与电脑端实时互通</div>
      </div>
    </section>

    <!-- 功能 -->
    <section class="sec" id="sec-feat" data-sec="feat">
      <span class="eyebrow">不止传文件</span>
      <h2>同一 Wi-Fi，一个入口。</h2>
      <p class="lead">从会议室到工位，日常的文件、消息、临时协作，一个页面全部解决。手机不用装任何 App。</p>
      <div class="cards">
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z"/></svg></div>
          <h3>文件传输</h3>
          <p>拖拽、点击、Ctrl+V 粘贴都能传，多文件带实时进度与速率。图片缩略预览、60+ 彩色文件图标，最新 / 名称 / 大小排序，批量选择删除，一键复制带温馨提示的分享链接。</p>
          <div class="tags"><span class="tag">多途径上传</span><span class="tag">批量管理</span><span class="tag">分享链接</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 01-2 2H7l-4 4V5a2 2 0 012-2h14a2 2 0 012 2z"/></svg></div>
          <h3>文本消息</h3>
          <p>电脑手机实时互通，约 3 秒同步一轮。自定义昵称与 24 款 emoji 头像，支持代码块、粗体、链接的轻量 Markdown；链接点击直达系统浏览器，未读角标不漏消息。</p>
          <div class="tags"><span class="tag">实时互通</span><span class="tag">Markdown</span><span class="tag">未读提醒</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg></div>
          <h3>服务中心</h3>
          <p>启停服务、改端口（被占自动顺延）、换共享目录，磁盘余量一目了然。二维码扫码直达手机，服务日志实时滚动，五套深色皮肤即点即换。</p>
          <div class="tags"><span class="tag">服务控制</span><span class="tag">二维码直达</span><span class="tag">五套皮肤</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 01-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z"/></svg></div>
          <h3>CC设置</h3>
          <p>开机自动启动可按需开启（在 CC设置 中打开开关），登录电脑服务就在；关窗收进托盘、服务保持运行。一键检查更新，新版本发布即刻知晓。</p>
          <div class="tags"><span class="tag">开机自启</span><span class="tag">托盘常驻</span><span class="tag">检查更新</span></div>
        </div>
      </div>
    </section>

    <!-- 如何使用 -->
    <section class="sec" id="sec-howto" data-sec="howto">
      <span class="eyebrow">如何使用</span>
      <h2>三步，把内容交给对方。</h2>
      <p class="lead">无需配置网络、无需注册账号，装好即用。</p>
      <div class="steps">
        <div class="step">
          <div class="no">01</div>
          <h3>电脑启动服务</h3>
          <p>打开 CC-Flash，进入「服务中心」点「启动服务」。默认端口 <code>1688</code>，被占用自动顺延，无需任何配置。</p>
        </div>
        <div class="step">
          <div class="no">02</div>
          <h3>扫码或输地址</h3>
          <p>手机连上同一个 Wi-Fi，扫二维码，或在浏览器输入 <code>http://电脑IP:端口</code>，立即进入共享页。</p>
        </div>
        <div class="step">
          <div class="no">03</div>
          <h3>直接开始互传</h3>
          <p>拖文件上传、点按钮下载、随时发消息。手机不用装 App；电脑关窗进托盘，服务也不停。</p>
        </div>
      </div>
    </section>

    <!-- 技术实现 / 可靠性 -->
    <section class="sec" id="sec-safe" data-sec="safe">
      <span class="eyebrow">数据不出局域网</span>
      <h2>传得快，也要传得安心。</h2>
      <p class="lead">传输边界清清楚楚：数据只在你的路由器内部流动，电脑在哪里，数据就到哪里。</p>
      <div class="points">
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg></div>
          <h3>数据只在局域网</h3>
          <p>HTTP 服务只监听你的电脑，文件与消息在局域网内直达 —— 不上云、无账号体系、不经过任何第三方服务器，断外网照样用。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg></div>
          <h3>大文件不占内存</h3>
          <p>后端流式读写，超大文件轻松传；下载超过 <code>1GB</code> 自动交给浏览器原生流式下载，稳定省资源。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="7" width="20" height="14" rx="2"/><path d="M16 21V5a2 2 0 00-2-2h-4a2 2 0 00-2 2v16"/></svg></div>
          <h3>服务常驻不断</h3>
          <p>关窗即收进托盘，服务保持运行，手机端不受影响；开机自启可在「CC设置」开启，登录电脑即可用，真正退出走托盘菜单。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="2" width="14" height="20" rx="2"/><line x1="12" y1="18" x2="12.01" y2="18"/></svg></div>
          <h3>三平台覆盖</h3>
          <p>桌面端支持 macOS 与 Windows（Linux 可源码构建）；手机端浏览器免安装即可用，Android 另提供可直接安装的 APK。</p>
        </div>
      </div>
      <div class="codes">
        <div class="code-card">
          <h3>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>
            桌面端安装
          </h3>
          <p>前往 GitHub Release 下载对应平台安装包，安装即用 —— 前端已嵌入二进制，无需任何额外部署。</p>
          <div class="code-box"><button class="code-copy" @click="copyText(RELEASES_PAGE, '已复制')">复制</button><span class="cm"># 打开最新版发布页</span><br />{{ RELEASES_PAGE }}</div>
        </div>
        <div class="code-card">
          <h3>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/></svg>
            从源码构建
          </h3>
          <p>Rust + Vue 3 + Tauri 技术栈，两条命令构建当前平台的安装包。</p>
          <div class="code-box"><button class="code-copy" @click="copyText(`npm install\nnpm run tauri build`, '已复制')">复制</button>npm install<br />npm run tauri build</div>
        </div>
      </div>
      <p class="code-note">升级前请保留原共享目录（默认 <code>~/CC-Flash</code>）。问题反馈与建议欢迎提交 <a :href="'https://github.com/' + GITHUB_REPO + '/issues'" @click.prevent="openExternal('https://github.com/' + GITHUB_REPO + '/issues')">GitHub Issue</a>。</p>
    </section>

    <!-- 常见问题 -->
    <section class="sec" id="sec-faq" data-sec="faq">
      <span class="eyebrow">常见问题</span>
      <h2>开始之前，了解这些。</h2>
      <div class="faq">
        <div v-for="(f, i) in INTRO_FAQS" :key="i" class="faq-item" :class="{ open: faqOpen === i }">
          <button class="faq-q" @click="toggleFaq(i)">{{ f.q }}<span class="pm">+</span></button>
          <div class="faq-a" :style="{ maxHeight: faqOpen === i ? '260px' : '0' }"><p>{{ f.a }}</p></div>
        </div>
      </div>
    </section>

    <!-- 页脚 -->
    <footer class="ifoot">
      <div class="ifoot-in">
        <div class="ifoot-brand">
          <img class="ib-lg" src="/logo.png" alt="logo" /> CC-Flash
        </div>
        <div class="ifoot-links">
          <a :href="'https://github.com/' + GITHUB_REPO" @click.prevent="openExternal('https://github.com/' + GITHUB_REPO)">GitHub</a>
          <a :href="'https://github.com/' + GITHUB_REPO + '/issues'" @click.prevent="openExternal('https://github.com/' + GITHUB_REPO + '/issues')">反馈问题</a>
          <a :href="RELEASES_PAGE" @click.prevent="openExternal(RELEASES_PAGE)">下载安装</a>
        </div>
        <div class="ifoot-copy">CC-Flash（曹操是快😂）—— 局域网文件互传工具 · {{ FOOTER_TEXT }}</div>
      </div>
    </footer>
    </div>
  </Transition>

  <!-- 二维码弹窗 -->
  <div v-if="qrShow" class="dialog-overlay" @click.self="qrShow = false">
    <div class="dialog-box">
      <h4>扫描二维码在手机上打开</h4>
      <img v-if="qrData" class="qr-img" :src="qrData" alt="QR" />
      <div v-else class="qr-empty" style="margin:10px auto">{{ running ? "二维码生成中…" : "服务已停止" }}</div>
      <p style="word-break:break-all">{{ shareUrl }}</p>
      <p style="font-size:12px;color:var(--text2);margin:0">{{ TIP_QR }}</p>
      <div class="dialog-actions"><button class="btn btn-primary" @click="qrShow = false">关闭</button></div>
    </div>
  </div>

  <!-- 删除确认 -->
  <div v-if="showDel" class="dialog-overlay" @click.self="showDel = false">
    <div class="dialog-box">
      <h4>删除文件</h4>
      <p>{{ delTargets.length > 1
        ? `确定删除选中的 ${delTargets.length} 个文件吗？此操作不可撤销。`
        : `确定删除 ${delTargets[0]} 吗？此操作不可撤销。` }}</p>
      <div class="dialog-actions">
        <button class="btn btn-ghost" @click="showDel = false">取消</button>
        <button class="btn btn-danger" @click="doDelete">确认</button>
      </div>
    </div>
  </div>

  <!-- 失联全屏(网页端): 电视"无信号"测试卡样式, 电脑/手机浏览器表现一致 -->
  <div v-if="!isDesktop && offline" class="nosignal-overlay">
    <img class="ns-img" :src="noSignalImg" alt="无信号测试卡" />
    <h3>📡 无信号</h3>
    <p class="ns-line">与服务的连接已断开 —— 服务可能已停止，或电脑地址已变更</p>
    <p class="ns-sub">请在电脑上查看最新地址后重新打开本页</p>
  </div>

  <!-- 版本更新弹框(桌面端) -->
  <div v-if="showUpdate" class="dialog-overlay" @click.self="showUpdate = false">
    <div class="dialog-box update-box">
      <h4>🚀 发现新版本</h4>
      <div class="upd-rows">
        <div class="upd-row"><span class="upd-label">当前版本</span><span class="ver-chip">v{{ curVersion() }}</span></div>
        <div class="upd-row"><span class="upd-label">最新版本</span><span class="ver-chip new">{{ updateInfo.latest }}</span></div>
      </div>
      <pre v-if="updateInfo.notes" class="upd-notes">{{ updateInfo.notes }}</pre>
      <div class="dialog-actions">
        <button class="btn btn-ghost" @click="showUpdate = false">稍后再说</button>
        <button class="btn btn-primary" @click="goDownload">前往下载</button>
      </div>
    </div>
  </div>

  <!-- 开机自启说明弹框(参考 TCode): 开启成功后弹一次, 点「知道了」关闭 -->
  <div v-if="showAutostartTip" class="dialog-overlay" @click.self="showAutostartTip = false">
    <div class="dialog-box">
      <h4>CC-Flash 已开启开机启动</h4>
      <p>登录电脑后，CC-Flash 将在后台自动启动（不弹出主窗口）。请从「应用程序」目录中的正式安装版开启 —— 若当前从 DMG 或开发目录运行，登录自启会指向该临时路径而失效。可随时在「CC设置」中关闭。</p>
      <div class="dialog-actions">
        <button class="btn btn-primary" @click="showAutostartTip = false">知道了</button>
      </div>
    </div>
  </div>

  <!-- Toast -->
  <div class="toast-container">
    <div v-for="t in toasts" :key="t.id" class="toast-msg" :class="'toast-' + t.type">{{ t.msg }}</div>
  </div>
</template>
