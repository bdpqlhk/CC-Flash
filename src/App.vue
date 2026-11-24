<script setup>
import { ref, computed, watch, onMounted, onUnmounted, nextTick } from "vue";
// 二维码库只在需要出码时动态加载: 手机浏览器/聊天首屏不必下载 qrcode 整包,
// 主 bundle 更小、首屏更快; 真正点二维码/桌面悬浮码时才拉取该 chunk。
let qrLibPromise = null;
function loadQrLib() {
  if (!qrLibPromise) {
    qrLibPromise = import("qrcode")
      .then((m) => m.toDataURL || (m.default && m.default.toDataURL) || m.default)
      .catch((e) => {
        // 弱网/升级后旧 chunk 404 时不能永久缓存 rejected Promise,
        // 否则本会话所有二维码都失败; 置空让下一次 syncQr 重试。
        qrLibPromise = null;
        throw e;
      });
  }
  return qrLibPromise;
}
// 失联提示图: 构建期内联为 base64, 服务停止后无需再发请求。
// 用 JPEG(59KB)而非 PNG(133KB): 该图无透明像素, 内联后直接减轻 JS/二进制体积。
import noSignalImg from "./assets/no-signal.webp";
// 可调参数集中管理(轮询/阈值/路径/文案等)
import {
  API, APP_VERSION_FALLBACK, AVATAR_POOL, BLOB_REVOKE_MS, COPIED_RESET_MS, CONTROL_RETRY_MS,
  CONTROL_RETRY_TIMES, DEFAULT_NICK, DEFAULT_PORT, FOOTER_TEXT, ICON_MAP, IMG_EXTS, LS, MSG_MAX,
  GITHUB_REPO, NARROW_MAX_PX, NICK_MAX, OFFLINE_FAILS, PAGE_SIZE, POLL_CHAT_MS,
  PEER_ONLINE_GRACE_SECS, POLL_FILES_MS, POLL_LOGS_MS, POLL_STATUS_MS, REFRESH_SPIN_MS, RELEASES_PAGE,
  THEME_MIGRATE, TIP_COPY_URL, TIP_QR, TIP_SHARE, TOAST_MS, UPDATE_CHECK_DELAY_MS,
  UPDATE_CHECK_INTERVAL_MS, UPLOAD_DONE_KEEP_MS, WS_HB_TIMEOUT_MS, WS_WATCHDOG_MS,
} from "./config";
import { fetchLatestRelease, isNewer } from "./update";
import { createPetActivityTracker } from "./pet-activity";
import { readText as readClipboardText, readImage as readClipboardImage, writeText as writeClipboardText } from "@tauri-apps/plugin-clipboard-manager";
// 模块化 API 取代 window.__TAURI__ 全局暴露(withGlobalTauri=false): XSS 即便发生也拿不到
// 覆盖全部自定义命令的 invoke 面 —— isDesktop 判定走 __TAURI_INTERNALS__(IPC 内部注入, 不受此开关影响)
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { availableMonitors, cursorPosition, currentMonitor, primaryMonitor } from "@tauri-apps/api/window";
import { LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize } from "@tauri-apps/api/dpi";
import { emit, listen } from "@tauri-apps/api/event";

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
  const next = {
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
  // 状态轮询每 2.5s 调一次: 值没变就不替换 cfg 引用。
  // 旧实现无条件赋新对象 → 依赖 cfg 的整棵 App 模板每 2.5s 重渲染,
  // 在 WKWebView 里表现为输入时卡顿、光标/输入法状态被反复打断。
  if (JSON.stringify(next) === JSON.stringify(cfg.value)) return;
  const oldPoll = JSON.stringify(cfg.value.poll);
  cfg.value = next;
  if (JSON.stringify(next.poll) !== oldPoll) startPolling();   // 间隔有变 → 重排定时器
}

/* ---------- 环境 ---------- */
// 诊断落盘默认关闭: 需要排查时执行 localStorage.setItem("wft-diag","1") 并重启
const DIAG_ENABLED = (() => { try { return localStorage.getItem("wft-diag") === "1"; } catch (e) { return false; } })();
const isDesktop = "__TAURI_INTERNALS__" in window;
// 仅开发态: ?mobilePreview=1 在浏览器里模拟手机 App 壳(390/360 宽度做设计审查)。
// 生产构建 import.meta.env.DEV 为 false, 这段分支会被 tree-shake。
const _previewQ = (() => { try { return new URLSearchParams(location.search); } catch (e) { return new URLSearchParams(); } })();
const PREVIEW_MOBILE = import.meta.env.DEV && _previewQ.has("mobilePreview");
// 真实桌面 OS: isDesktop 在 Android/iOS 的 Tauri WebView 里同样为 true, 但 autostart/托盘
// 等桌面插件按 #[cfg(desktop)] 未注册 —— 依赖这些插件的 UI 必须用 isRealDesktop 门控
// iPadOS 13+ 伪装桌面 UA("Macintosh"), 单看 UA 会把 iPad 网页误判为真实桌面:
// 用 MacIntel + 多点触控补判(Apple 官方推荐手法)
const isIOSFamily = /iPhone|iPad|iPod/.test(navigator.userAgent)
  || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
const isRealDesktop = isDesktop && !isIOSFamily && !/Android/.test(navigator.userAgent);
const mobileShell = PREVIEW_MOBILE || (isDesktop && !isRealDesktop);
/* PWA: 仅浏览器端显示安装入口; 桌面/移动壳已有原生 App。
   http://局域网IP 不是安全上下文, 浏览器内核直接关闭 Service Worker 与安装能力;
   https / localhost 下才是完整 PWA。这里把两种路径都做成明确可操作的状态,
   不再出现"按钮点了没反应"。 */
let deferredInstallPrompt = null;
const pwaPromptReady = ref(false);
const pwaStandalone = (() => {
  try {
    return window.matchMedia("(display-mode: standalone)").matches || navigator.standalone === true;
  } catch (e) { return false; }
})();
window.addEventListener("beforeinstallprompt", (e) => {
  e.preventDefault();
  deferredInstallPrompt = e;
  pwaPromptReady.value = true;
});
window.addEventListener("appinstalled", () => {
  deferredInstallPrompt = null;
  pwaPromptReady.value = false;
  showToast("已安装到主屏幕");
});
function installPwa() {
  if (isDesktop || isQrPetWindow) return;
  // 局域网 http:// 不是安全上下文: Chrome 不提供 beforeinstallprompt/SW,
  // 直接说清楚能做什么, 不要让用户以为点了会装出一个离线应用。
  if (!window.isSecureContext) {
    showToast(isIOSFamily
      ? "Safari：点「分享」→「添加到主屏幕」创建快捷方式（HTTP 下不缓存离线）"
      : "当前为 HTTP 访问，浏览器不提供安装；请用浏览器菜单「添加到主屏幕」");
    return;
  }
  if (deferredInstallPrompt) {
    deferredInstallPrompt.prompt();
    pwaPromptReady.value = false;
    deferredInstallPrompt = null;
    return;
  }
  if (pwaStandalone) { showToast("已经作为独立应用运行"); return; }
  if (isIOSFamily) showToast("Safari：点「分享」→「添加到主屏幕」");
  else showToast("浏览器菜单 →「安装应用 / 添加到主屏幕」");
}
const isQrPetWindow = isDesktop && new URLSearchParams(location.search).get("view") === "qr-pet";
const isIOS = isIOSFamily;   // Safari 不支持 webkitdirectory, 文件夹入口对 iOS/iPadOS 隐藏
const isWeChat = /MicroMessenger/i.test(navigator.userAgent);   // 微信内置浏览器: 下载被拦需引导到系统浏览器
// 桌面模式:窗口页面不在服务端口上,API 走回环地址;网页模式:相对路径即可;
// 移动壳只作为独立本机节点(连 127.0.0.1), 不再提供"连接电脑"入口;
// 未就绪时 apiBase 为空 → 启动门遮罩
const serverAddr = ref("");
const apiBase = computed(() => {
  if (!isDesktop) return "";
  if (mobileShell) return serverAddr.value ? `http://${serverAddr.value}` : "";
  return `http://127.0.0.1:${port.value || DEFAULT_PORT}`;
});
/* 移动 App: 本机服务启动后自动连回环地址 */
const mobileBootFailed = ref(false);
function isLocalAddr(addr) { return /^127\.0\.0\.1:\d+$/.test(String(addr || "")); }
async function initMobileLocal() {
  if (!mobileShell) return;
  mobileBootFailed.value = false;
  serverAddr.value = "";
  // 旧版本可能存过"连接电脑"地址; 现在移动壳只跑本机节点, 启动时清掉遗留值。
  try { localStorage.removeItem("wft-server-addr"); } catch (e) {}
  saveTextPref("server-addr", "");
  for (let i = 0; i < 30; i++) {
    try {
      const d = await tauriInvoke("control_status_cmd");
      if (d && d.running && d.port) {
        const addr = `127.0.0.1:${d.port}`;
        serverAddr.value = addr;
        // 移动端开启访问令牌后 loopback 也校验: 本机壳通过 Tauri 拿码并自动附加,
        // 同机其他 App 没有这条通道(拿不到码)。
        if (typeof d.pin === "string" && d.pin) accessPin.value = d.pin;
        mobileBootFailed.value = false;
        refreshStatus(); loadFiles(); loadChat(); connectWs();
        return;
      }
    } catch (e) {}
    await new Promise((r) => setTimeout(r, 500));
  }
  mobileBootFailed.value = true;
  showToast("本地服务启动失败，请点「重新启动本机服务」", "error");
}
/* 启动门重试: 先显式拉起本机服务(移动端命令已放开), 再轮询就绪状态 */
async function retryMobileBoot() {
  if (!mobileShell) return;
  mobileBootFailed.value = false;
  try { await tauriInvoke("control_start_cmd"); } catch (e) {}
  await initMobileLocal();
}
function retryMobileLocal() {
  if (!mobileShell) return;
  resetSessionState();
  serverAddr.value = "";
  retryMobileBoot();
}
/* 重启本机服务(局域网开关/端口变更后需要重绑监听) */
async function restartMobileService() {
  if (!mobileShell) return;
  showToast("正在重启本机服务…");
  try { await tauriInvoke("control_stop_cmd"); } catch (e) {}
  try { await tauriInvoke("control_start_cmd"); } catch (e) {}
  await initMobileLocal();
}
/* 会话级缓存全部重置: 本机服务重启/断线重连后不得把旧会话数据带进新连接 */
let sessionEpoch = 0;   // 每次会话重置/断线重连自增: 旧请求的迟到响应直接丢弃
function resetSessionState() {
  sessionEpoch++;
  loadingFiles = false;
  loadingChat = false;
  statusRefreshing = false;
  // 切换电脑地址前中止在途上传并释放 Blob/句柄, 否则旧连接的上传会继续跑、
  // 屏幕常亮/保活也不会释放。必须明确告知用户 —— 静默清空会被误认为"已传完"。
  const abortedUploads = uploads.value.filter(u => u.status === "uploading" || u.status === "waiting").length;
  for (const u of uploads.value) {
    try { u.xhr?.abort?.(); } catch (e) {}
    u.file = null; u.xhr = null;
  }
  if (abortedUploads) showToast(`连接已切换，${abortedUploads} 个未完成的上传已取消`, "error");
  closePreview();
  uploads.value = [];
  updateWakeLock().catch(() => {});   // 清空上传后释放屏幕常亮/静音保活
  remoteFiles.value = [];
  fileEvents.value = [];
  peers.value = [];
  sendTarget.value = null;
  directReceipts.value = [];
  selected.value = new Set();
  knownFileNames = null;
  chatMsgs.value = [];
  sharedClip.value = { text: "", updatedAt: 0, from: "" };
  chatHtmlCache.clear();   // 切换电脑/服务后消息 id 可能从头计数, 不能复用旧 HTML 缓存
  chatCount.value = 0;
  seenChatId.value = 0;
  jumpCount.value = 0;
  loadFiles._sig = null;
  loadChat._sig = null;
  loadChat._lastSig = null;
  loadChat._filesSig = null;
  loadChat._init = false;
  clearedTs.value = 0;
  timelineStart.value = 0;
  // 引用快照来自旧服务端的消息, 切会话后必须作废 —— 否则切换后发送会把
  // 电脑 A 的消息内容带进电脑 B 的聊天记录并广播给 B 的所有设备
  quoting.value = null;
  pinSnoozeUntil = 0;
  clearShareTicketCache();
  accessPin.value = "";
  try { localStorage.removeItem(LS.pin); } catch (e) {}
  saveTextPref("access-pin", "");
}
function tauriInvoke(cmd, args) { return invoke(cmd, args || {}); }

/* ---------- 访问码(可选; 电脑端 CC设置 开启后, 局域网网页端需输入一次) ---------- */
const accessPin = ref((() => { try { return localStorage.getItem(LS.pin) || ""; } catch (e) { return ""; } })());
const needPin = ref(false);      // 401 → 弹出输入框; 验证通过后全量重载
const pinInput = ref("");
const pinEnabled = ref(false);   // 桌面设置页展示用(仅 Tauri 命令下发, HTTP /api/info 不携带码值)
const pinCode = ref("");
// "稍后再输入": 点关闭后 60s 内不再被自动请求重新弹框(否则 2.5s 状态轮询立刻弹回)
let pinSnoozeUntil = 0;
function snoozePin() {
  needPin.value = false;
  pinSnoozeUntil = Date.now() + 60 * 1000;
  showToast("已隐藏配对框；60 秒内不再自动弹出");
}
/* 移动壳的 WKWebView localStorage 有"只驻内存不落盘"的历史问题: 地址/配对码
   双写 Rust pref 文件, 启动时 localStorage 为空则回读 —— 否则重启后连接门重来 */
function saveTextPref(key, value) {
  if (!isDesktop) return;
  tauriInvoke("save_text_pref_cmd", { key, value: value || "" }).catch(() => {});
}
function withPin(url, pin = accessPin.value) {
  return pin ? url + (url.includes("?") ? "&" : "?") + "pin=" + encodeURIComponent(pin) : url;
}
/* 服务端已轮换/关闭配对码: 清掉本机缓存, 否则每个自动请求/排队上传都继续
   带着错码计数, 会把本机 IP 推进退避锁定(用户"越输越锁") */
function dropAccessPin() {
  if (!accessPin.value) return;
  accessPin.value = "";
  try { localStorage.removeItem(LS.pin); } catch (e) {}
  saveTextPref("access-pin", "");
  clearShareTicketCache();
}
/* 状态接口带回配对码开关/码值: 变化(轮换、关闭、重新开启)时旧分享票据全部失效 */
function syncPinState(d) {
  if (typeof d.pinEnabled !== "boolean") return;
  const changed = pinEnabled.value !== d.pinEnabled
    || (typeof d.pin === "string" && pinCode.value !== d.pin);
  pinEnabled.value = d.pinEnabled;
  pinCode.value = d.pin || "";
  if (changed) clearShareTicketCache();
  clearAccessPinIfOff(pinEnabled.value);
}
/* 统一请求入口: 令牌走 X-Access-Pin 头(不再拼进 URL, 减少查询串泄露面);
   401/429 时置 needPin 配对弹框(网页端输入一次后全流程透明), 并清掉本机缓存的旧码 ——
   服务端轮换/关闭配对码后, 旧码会让每个自动请求都计入失败, 越输越锁(自锁死循环)。
   429 = 配对码连续错进退避锁定 —— 若不当配对信号处理, 页面会静默退化成"已停止/无文件" */
async function apiFetch(url, opts, timeoutMs = 30000) {
  const o = opts ? { ...opts } : {};
  const headers = new Headers(o.headers || {});
  const usedPin = accessPin.value;   // 本次请求实际使用的码: 401 时据此判断是否该清除
  if (accessPin.value) headers.set("X-Access-Pin", accessPin.value);
  o.headers = headers;
  // 统一超时: 分享/删除/发消息不走 loadFiles/loadChat 的专用计时器, 半开 Wi-Fi
  // 会让 fetch 永久挂起。调用方已有的 signal 通过监听合并, 谁先触发都中止。
  const ctl = new AbortController();
  const outer = o.signal;
  const timer = setTimeout(() => ctl.abort(), timeoutMs);
  const onAbort = () => ctl.abort();
  if (outer) {
    if (outer.aborted) ctl.abort();
    else outer.addEventListener("abort", onAbort, { once: true });
  }
  o.signal = ctl.signal;
  // API 数据不允许浏览器缓存: 删除/重命名后旧列表可能被直接复用
  if (!o.method || String(o.method).toUpperCase() === "GET") o.cache = o.cache || "no-store";
  let cleared = false;
  const clearReq = () => {
    if (cleared) return;
    cleared = true;
    clearTimeout(timer);
    if (outer) outer.removeEventListener("abort", onAbort);
  };
  try {
    const r = await fetch(url, o);
    if (r.status === 401 || r.status === 429) {
      // 429 不一定是配对锁定: 聊天/共享剪贴板限流也返回 429。只有服务端明确
      // 标记 pinRequired 时才清码弹配对框, 否则会把正确令牌误清、并给出错误提示。
      let pinRequired = r.status === 401;
      if (r.status === 429) {
        try {
          const d = await r.clone().json();
          pinRequired = d && d.pinRequired === true;
        } catch (_) {}
      }
      // 只清除"这次请求所用的码"仍是当前码的情况: submitPin 成功后, 旧的在途请求
      // 迟到 401 不能把刚保存的新码清掉，否则配对成功后立刻又弹配对框
      if (pinRequired && accessPin.value === usedPin && Date.now() >= pinSnoozeUntil) {
        dropAccessPin();
        needPin.value = true;
      }
      const err = new Error(
        pinRequired
          ? (r.status === 429 ? "配对尝试过于频繁" : "需要配对码")
          : "请求过于频繁，请稍后再试"
      );
      err.status = r.status;   // 供 refreshStatus 区分"服务在但要配对"与"真断网"
      err.pinRequired = pinRequired;
      clearReq();
      throw err;
    }
    // 超时必须覆盖"响应体读取"：只等响应头时，半开 Wi-Fi 下 r.json() 会永久挂起。
    // 给 json/text/blob/arrayBuffer 套一层，读完才清计时器；调用方不读 body 时
    // 30s 后由 AbortController 兜底释放（调用方可传更短超时）。
    for (const method of ["json", "text", "blob", "arrayBuffer"]) {
      const orig = r[method];
      if (typeof orig !== "function") continue;
      r[method] = (...args) => Promise.resolve(orig.apply(r, args)).finally(clearReq);
    }
    return r;
  } catch (e) {
    clearReq();
    throw e;
  }
}
/* 下载/图片/分享链接统一经此: 分享链接带上令牌, 接收方打开即验证 */
function downloadUrl(name) { return withPin(apiBase.value + API.download(name)); }
async function submitPin() {
  const p = pinInput.value.trim();
  if (!p) return;
  // 弱网/半开连接下裸 fetch 会永久挂起: 15s 超时并如实提示, 不留下无法关闭的弹框
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), 15000);
  try {
    const r = await fetch(apiBase.value + API.info + "?pin=" + encodeURIComponent(p), { signal: ctl.signal, cache: "no-store" });
    if (r.ok) {
      accessPin.value = p;
      try { localStorage.setItem(LS.pin, p); } catch (e) {}
      saveTextPref("access-pin", p);
      needPin.value = false;
      pinSnoozeUntil = 0;
      statusFail = 0;
      offline.value = false;
      showToast("配对成功");
      refreshStatus(); loadFiles(); loadChat(); connectWs();
    } else if (r.status === 429) showToast("尝试过于频繁，请稍后再试", "error");
    else showToast("配对码不正确", "error");
  } catch (e) {
    showToast(e?.name === "AbortError" ? "配对超时，请重试" : "验证失败，请检查网络", "error");
  } finally {
    clearTimeout(timer);
  }
}

/* 分享链接只携带短期一次性票据，绝不携带全站 PIN。 */
async function createShareTicket(name) {
  try {
    const r = await apiFetch(apiBase.value + "/api/share-ticket", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name }),
    });
    if (!r.ok) return null;
    const d = await r.json();
    return typeof d.ticket === "string" ? d.ticket : null;
  } catch (e) { return null; }
}
/* 令牌关闭时必须同步清掉本地缓存的 accessPin —— shareFile 的 pinOn 判定是
   accessPin || pinCode, 只清 pinCode 的话旧码还躺在 localStorage 里,
   分享会继续误请求票据(链接仍带 ?ticket=) */
function clearAccessPinIfOff(enabled) {
  if (enabled || !accessPin.value) return;
  accessPin.value = "";
  try { localStorage.removeItem(LS.pin); } catch (e) {}
  saveTextPref("access-pin", "");
  clearShareTicketCache();
}
/* 统一的应用内确认框: 替代系统原生 alert/confirm, 与 CC Flash 主题一致 */
const confirmState = ref(null);
function appConfirm(opts) {
  return new Promise((resolve) => {
    confirmState.value = {
      title: "请确认", message: "", confirmText: "确定", cancelText: "取消",
      danger: false, icon: "", ...(opts || {}), resolve,
    };
  });
}
function resolveConfirm(ok) {
  const st = confirmState.value;
  if (!st) return;
  confirmState.value = null;
  try { st.resolve?.(!!ok); } catch (e) {}
}

async function toggleAccessPin() {
  try {
    // 移动端允许关闭令牌, 但必须二次确认: 关闭后同 Wi-Fi 设备无需配对码即可读写
    // 本机共享, 且 Android 的回环地址不按应用隔离, 同机其他 App 也可直接访问。
    if (mobileShell && pinEnabled.value) {
      const yes = await appConfirm({
        title: "关闭访问令牌保护",
        message: "关闭后同一 Wi-Fi 下的设备无需配对码即可读写本机共享文件与聊天；Android 的本机回环地址也不按应用隔离。确定关闭吗？",
        confirmText: "关闭保护", danger: true, icon: "shield",
      });
      if (!yes) return;
    }
    const d = await tauriInvoke("set_access_pin_cmd", { enable: !pinEnabled.value });
    if (d.ok) {
      pinEnabled.value = !pinEnabled.value;
      pinCode.value = d.pin || "";
      // 移动端 loopback 也强制校验 PIN；开启后必须立刻把码放进 accessPin，
      // 否则 clearShareTicketCache 触发的媒体/列表请求会先 401 弹出配对框。
      if (mobileShell) accessPin.value = pinEnabled.value ? (d.pin || "") : "";
      clearShareTicketCache();   // 服务端已轮换/清空票据
      clearAccessPinIfOff(pinEnabled.value);
      showToast(pinEnabled.value ? `访问令牌已开启：${d.pin}` : "访问令牌已关闭");
    } else showToast(d.error || "设置失败", "error");
  } catch (e) { showToast("设置失败，请重试", "error");
  }
}
/* 与服务端同口径的昵称截断: 服务端按 Unicode 字符截 20, 前端用完整昵称比较
   会把"我"的消息误判成别人(isMine), 也会让输入框显示与实际落库不一致。 */
function clampNick(s) {
  return Array.from(String(s || "")).slice(0, cfg.value.nickMax).join("");
}
/* 当前设备昵称(供日志/请求头使用) */
function deviceNick() { return clampNick(nickInput.value.trim() || autoNick.value); }

/* ---------- 视图切换 ---------- */
/* 视图策略(v1.2.3):
   - 真实桌面: 侧栏 消息发送(页内左侧设备栏) / 服务中心 / CC设置
   - 手机 App: 四页 发送 / 设备 / 文件 / 设置, 底部四标签切换
   - 手机/电脑浏览器: 不再有底部标签/发现/设置, 固定回到原聊天室单页 */
const _bootView = (() => {
  // 浏览器: 只保留原聊天室, 不恢复 discover/settings/PWA tab 参数
  if (!isDesktop) return "transfer";
  // 手机 App: 四页 发送/设备/文件/设置; 设备选择在发送页顶部
  if (mobileShell) {
    try {
      const t = new URLSearchParams(location.search).get("tab");
      if (t === "transfer" || t === "settings") return t;
    } catch (e) {}
    try {
      const saved = localStorage.getItem("wft-mobile-view");
      if (saved === "transfer" || saved === "settings") return saved;
    } catch (e) {}
    return "transfer";
  }
  return "transfer";   // 真实桌面
})();
const view = ref(isQrPetWindow ? "pet" : _bootView);   // discover | transfer | service | settings | pet
const VIEW_TITLES = { discover: "发现", transfer: "消息发送", service: "服务中心", settings: "CC设置" };
const discoverTitle = computed(() => (isRealDesktop ? "周边设备" : "发现"));
function setView(v) {
  // 兼容旧入口：files/chat 都归到合并后的「传输」
  if (v === "files" || v === "chat") v = "transfer";
  // 设备/目标已并入消息页: 旧 discover 视图不再存在
  if (v === "discover") v = "transfer";
  // 浏览器没有发现/设置/服务中心: 任何入口都回原聊天室
  if (!isDesktop && !PREVIEW_MOBILE && (v === "settings" || v === "service")) v = "transfer";
  view.value = v;
  if (!isQrPetWindow) {
    try { localStorage.setItem("wft-mobile-view", v); } catch (e) {}
  }
  moreOpen.value = false;
  plusOpen.value = false;
  if (v === "transfer") {
    markChatSeen();
    // 输入框随内容长高的高度是内联 style, v-if 卸载后重建会回到 rows=1;
    // 切回发送页时按 chatInput 重新量一次, 否则多行草稿看起来"缩回单行"。
    nextTick(() => { scrollChats(); autoGrowInput(); });
  } else if (isNarrow.value || mobileTabbed.value) {
    fileSheetOpen.value = false;
    deviceInlineOpen.value = false;
  }
}
/* 四页标签栏只属于手机 App; 浏览器回到原聊天室单页, 不再渲染任何底部标签。 */
const mobileTabbed = computed(() => mobileShell);
/* 视图切换后 textarea 是重建的: 已有多行草稿需要重新量高度, 否则回到单行样式 */
watch(view, (v) => {
  nextTick(() => {
    if (v === "transfer") autoGrowInput();
  });
});

/* ---------- 合并传输页(微信式)面板状态 ---------- */
const drawerCollapsed = ref(true);    // 桌面右侧共享文件抽屉: 默认收起, 由头部按钮/引导打开
const fileSheetOpen = ref(false);     // 窄屏共享文件全屏面板
const plusOpen = ref(false);          // 输入栏 ＋ 面板
const moreOpen = ref(false);          // 房间头更多菜单
/* 手机 App 四页导航(SEND / DEVICES / FILES / CONFIG): 直接复用现有面板状态,
   不新增一套页面状态, 避免和窄窗桌面/浏览器抢同一套交互。 */
const mobileFileSelect = ref(false);
const mobileTab = computed(() => {
  if (!mobileTabbed.value) return "send";
  if (view.value === "settings") return "config";
  if (deviceInlineOpen.value) return "devices";
  if (fileSheetOpen.value) return "files";
  return "send";
});
function setMobileTab(tab) {
  if (!mobileTabbed.value) return;
  plusOpen.value = false;
  moreOpen.value = false;
  if (tab !== "files") mobileFileSelect.value = false;
  if (tab === "config") {
    deviceInlineOpen.value = false;
    fileSheetOpen.value = false;
    setView("settings");
    return;
  }
  // 先落覆盖层状态再 setView("transfer"): setView 内会调 markChatSeen,
  // 反过来写会让 DEVICES/FILES 页打开瞬间把新消息误标已读。
  deviceInlineOpen.value = tab === "devices";
  fileSheetOpen.value = tab === "files";
  setView("transfer");
}
const nickOpen = ref(false);          // 昵称设置弹框
const nickDraft = ref("");            // 弹框草稿: 取消时丢弃, 不污染全局昵称
const fileMenuName = ref("");         // 移动端文件行动作面板
const locatedName = ref("");          // “定位”高亮的文件
const fileDot = ref(false);           // 文件抽屉关闭时收到新文件: 文件夹按钮绿点
const expandedGroups = ref({});       // 文件组展开状态
const atBottom = ref(true);           // 时间线是否贴底
const jumpCount = ref(0);             // 上翻期间的新消息数
/* ---------- 消息工作台: 目标设备(本机共享 / 直发) ---------- */
const sendTargets = ref([]);          // [] = 本机共享; 单选或多选设备(直发)
const multiSelectMode = ref(false);   // 附近设备多选模式: 点卡片只切换选中, 不弹发送面板
const sendTarget = computed({         // 兼容既有单目标逻辑: 返回第一个选中设备
  get: () => sendTargets.value[0] || null,
  set: (p) => { sendTargets.value = p ? [p] : []; },
});
function isPeerSelected(p) { return !!p && sendTargets.value.some((t) => t.id === p.id); }
function pickPeer(p) {
  if (!p) { selectSendTarget(null); return; }
  if (!multiSelectMode.value) { selectSendTarget(p); return; }
  // 多选: 已选则取消, 未选则追加; 不关闭设备面板, 便于连续勾选
  const exists = isPeerSelected(p);
  const next = exists
    ? sendTargets.value.filter((t) => t.id !== p.id)
    : [...sendTargets.value, p];
  sendTargets.value = next;
}
function toggleMultiSelect() {
  multiSelectMode.value = !multiSelectMode.value;
  if (!multiSelectMode.value && sendTargets.value.length > 1) {
    // 退出多选时只保留第一个目标, 输入栏回到常规"发送到单台设备"
    sendTargets.value = sendTargets.value.slice(0, 1);
  }
}
const historyOpen = ref(false);        // 传输历史弹框(入口在本机身份卡右侧)
// 窄屏设备面板: 复用宽屏设备栏的同一套内容, 竖屏整页展开(不是弹框)
const deviceInlineOpen = ref(false);
const directReceipts = ref([]);       // 直发回执只留本机时间线
const TRANSFER_HISTORY_KEY = "wft-transfer-history";
const transferHistory = ref((() => {
  try {
    const saved = JSON.parse(localStorage.getItem(TRANSFER_HISTORY_KEY) || "[]");
    return Array.isArray(saved) ? saved.slice(0, 120) : [];
  } catch (e) { return []; }
})());
function saveTransferHistory() {
  try { localStorage.setItem(TRANSFER_HISTORY_KEY, JSON.stringify(transferHistory.value)); } catch (e) {}
}
const historyQuery = ref("");
const filteredHistory = computed(() => {
  const q = historyQuery.value.trim().toLowerCase();
  if (!q) return transferHistory.value;
  return transferHistory.value.filter((h) => {
    const type = h.kind === "file" ? "文件" : "消息";
    return `${h.targetName} ${h.name} ${type}`.toLowerCase().includes(q);
  });
});
async function clearTransferHistory() {
  if (!transferHistory.value.length) return;
  const yes = await appConfirm({
    title: "清空传输历史",
    message: "将删除本机保存的全部直发记录（不影响对方设备），且无法恢复。",
    confirmText: "清空", danger: true, icon: "trash",
  });
  if (!yes) return;
  transferHistory.value = [];
  historyQuery.value = "";
  saveTransferHistory();
  showToast("传输历史已清空");
}
// 关闭弹框时清掉搜索词: 下次打开默认看全量, 避免"只看到几条"的错觉
watch(historyOpen, (v) => { if (!v) historyQuery.value = ""; });
function recordTransfer(p, kind, name) {
  if (!p) return;
  transferHistory.value = [{
    id: Date.now() + "-" + Math.random().toString(36).slice(2, 6),
    targetId: p.id || p.ip || "",
    targetName: p.nickname || p.ip || "设备",
    kind,
    name: String(name || "").slice(0, 80),
    at: Date.now() / 1000,
  }, ...transferHistory.value].slice(0, 120);
  saveTransferHistory();
}
let directReceiptSeq = 0;
const directSending = ref(false);
const canTargetDevices = computed(() => (isDesktop || PREVIEW_MOBILE) && !isQrPetWindow);
const DEVICE_RAIL_KEY = "wft-device-rail-collapsed";
const deviceRailCollapsed = ref((() => {
  try { return localStorage.getItem(DEVICE_RAIL_KEY) === "1"; } catch (e) { return false; }
})());
function toggleDeviceRail() {
  deviceRailCollapsed.value = !deviceRailCollapsed.value;
  try { localStorage.setItem(DEVICE_RAIL_KEY, deviceRailCollapsed.value ? "1" : "0"); } catch (e) {}
}
// 竖屏设备面板(窄窗桌面 / 手机 App): 与宽屏设备栏共用同一段模板, 展开为整页竖排;
// 不弹窗、不遮罩。宽屏未收起时仍用左侧 200px 设备栏。
const devicePanelInline = computed(() => deviceInlineOpen.value && (isNarrow.value || !isRealDesktop));
const showDeviceRail = computed(() => {
  if (!canTargetDevices.value) return false;
  if (deviceInlineOpen.value) return true;
  return isRealDesktop && !isNarrow.value && !deviceRailCollapsed.value;
});
const targetLabel = computed(() => {
  const ts = sendTargets.value;
  if (!ts.length) return "本机共享";
  if (ts.length === 1) return ts[0].nickname || "设备";
  return `${ts[0].nickname || "设备"} 等 ${ts.length} 台`;
});
function selectSendTarget(p) {
  if (!p) {
    sendTargets.value = [];
    deviceInlineOpen.value = false;
    return;
  }
  if (!isLanHost(p.ip) || !(Number(p.port) > 0 && Number(p.port) <= 65535)) {
    showToast("设备地址不可用，请刷新设备列表", "error");
    return;
  }
  sendTargets.value = [p];
  deviceInlineOpen.value = false;   // 选完收起面板
  // 选中即生效: 输入栏/拖文件直接直发, 不再自动弹出「发送到设备」面板;
  // 面板只保留给顶部/更多菜单里的「手动发送」入口。
}
function clearSendTarget() { sendTargets.value = []; }
function addReceipt(text, ok = true) {
  directReceipts.value.push({ id: ++directReceiptSeq, ts: Date.now() / 1000, text, ok });
  if (directReceipts.value.length > 40) directReceipts.value.splice(0, directReceipts.value.length - 40);
  atBottom.value = true;
  jumpCount.value = 0;
  nextTick(scrollChats);
}
/* 手机 App 的四页布局始终把共享文件作为整页 sheet, 不依赖 980px 断点:
   否则平板/折叠屏横屏点 FILES 会只高亮标签、页面空白。 */
function filePanelIsSheet() {
  return isNarrow.value || mobileTabbed.value;
}
function filePanelVisible() {
  // 注意: 脚本里 isNarrow 是 ref, 必须 .value —— 漏写会恒为 truthy,
  // 宽屏下永远走窄屏分支, 表现为「抽屉收起后再也打不开」
  return filePanelIsSheet() ? fileSheetOpen.value : !drawerCollapsed.value;
}
/* 手机上打开文件页/面板前先收键盘: 否则软键盘会把底部内容顶掉一半 */
function blurMobileKeyboard() {
  if (isNarrow.value && document.activeElement && typeof document.activeElement.blur === "function") {
    document.activeElement.blur();
  }
}
/* ---------- 桌面端首次使用: 共享文件入口气泡引导 ---------- */
const FILE_HINT_KEY = "wft-file-hint-seen";
const fileHintShow = ref(false);
function markFileHintSeen() {
  fileHintShow.value = false;
  try { localStorage.setItem(FILE_HINT_KEY, "1"); } catch (e) {}
}
function maybeShowFileHint() {
  if (!isRealDesktop) return;   // 气泡引导只在桌面端出现, 浏览器/移动壳不打扰
  try { if (localStorage.getItem(FILE_HINT_KEY) === "1") return; } catch (e) {}
  if (isQrPetWindow || view.value !== "transfer" || filePanelVisible()) return;
  fileHintShow.value = true;
  // 提示卡不长期占用聊天区: 12 秒后自动收起, 用户也可以点右上角关闭。
  window.setTimeout(() => { if (fileHintShow.value) markFileHintSeen(); }, 12000);
}
function toggleFilePanel() {
  markFileHintSeen();
  moreOpen.value = false;
  plusOpen.value = false;
  if (filePanelIsSheet()) {
    const next = !fileSheetOpen.value;
    fileSheetOpen.value = next;
    if (next) deviceInlineOpen.value = false;
  } else drawerCollapsed.value = !drawerCollapsed.value;
  if (filePanelVisible()) { fileDot.value = false; blurMobileKeyboard(); }
}
function openFilePanel() {
  markFileHintSeen();
  moreOpen.value = false;
  plusOpen.value = false;
  if (filePanelIsSheet()) {
    deviceInlineOpen.value = false;
    fileSheetOpen.value = true;
  } else drawerCollapsed.value = false;
  fileDot.value = false;
  blurMobileKeyboard();
}
/* 设备入口/文件页互斥: 两个都是整页覆盖时不能同时展开 */
function toggleDeviceInline() {
  const next = !deviceInlineOpen.value;
  deviceInlineOpen.value = next;
  if (next) fileSheetOpen.value = false;
}
/* 聚焦输入框(尤其手机弹键盘)时收起浮层: 否则加号面板+键盘把聊天区挤没 */
function onInputFocus() {
  plusOpen.value = false;
  moreOpen.value = false;
}
function togglePlus() {
  plusOpen.value = !plusOpen.value;
  moreOpen.value = false;
  if (plusOpen.value) blurMobileKeyboard();
}
function toggleMore() {
  moreOpen.value = !moreOpen.value;
  plusOpen.value = false;
  if (moreOpen.value) blurMobileKeyboard();
}
function openNick() {
  nickDraft.value = nickInput.value;
  nickOpen.value = true;
  moreOpen.value = false;
}
function saveNickAndClose() {
  nickDraft.value = clampNick(nickDraft.value);
  nickInput.value = nickDraft.value;
  saveNick();
  nickOpen.value = false;
}
function closeFileMenu() { fileMenuName.value = ""; }
function toggleGroup(batch) {
  expandedGroups.value = { ...expandedGroups.value, [batch]: !expandedGroups.value[batch] };
}

/* ---------- 侧边栏展开/收起(仅桌面壳; 状态记忆, 窄屏顶栏布局不受影响) ---------- */
const sidebarCollapsed = ref((() => {
  try { return localStorage.getItem("wft-side-collapsed") === "1"; } catch (e) { return false; }
})());
function toggleSidebar() {
  sidebarCollapsed.value = !sidebarCollapsed.value;
  try { localStorage.setItem("wft-side-collapsed", sidebarCollapsed.value ? "1" : "0"); } catch (e) {}
}

/* ---------- 外观主题 ---------- */
/* 6 套低饱和配色(信号台/蓝/灰/绿/紫/琥珀): 去掉微信绿与相近的石墨玫瑰,
   长时间阅读不刺眼; 被移除的选择在 main.js 启动时迁移(见 THEME_MIGRATE) */
const THEMES = [
    { id: "signal",   name: "信号台",   preview: "linear-gradient(135deg,#0b0e0d,#f2b441)" },
    { id: "default",  name: "深空蓝",   preview: "linear-gradient(135deg,#0d1017,#1a2233)" },
    { id: "rose",     name: "石墨玫瑰", preview: "linear-gradient(135deg,#16151a,#4a2531)" },
    { id: "mint",     name: "森林薄荷", preview: "linear-gradient(135deg,#0f1712,#1d4634)" },
    { id: "purple",   name: "暗夜紫",   preview: "linear-gradient(135deg,#15121d,#33265a)" },
    { id: "amber",    name: "墨色琥珀", preview: "linear-gradient(135deg,#141109,#4a3512)" },
];
const theme = ref("signal");   // 信号台：石墨黑 + 琥珀雷达盘；老默认主题首次升级时迁移一次
let mobileSignalLock = false;  // 手机 App 四页改版首次进入时锁定信号台, 避免被服务端旧配色覆盖
function applyTheme(t) {
    if (t && t !== "default") document.documentElement.dataset.theme = t;
    else delete document.documentElement.dataset.theme;
}
function setTheme(id) {
    mobileSignalLock = false;
    theme.value = id;
    applyTheme(id);
    try { localStorage.setItem(LS.theme, id); } catch (e) {}
    saveTextPref("ui-theme", id);   // 桌面端选择: /api/info 下发给浏览器/移动壳跟随
}
try {
  // 读取时同样应用迁移: 本模块先于 main.js 本体求值, 不迁移的话升级后
  // 首次启动选择器高亮会停留在已被移除的旧 id 上(网格无高亮)
  let t = localStorage.getItem(LS.theme);
  if (!isRealDesktop) {
    // 浏览器/移动壳首帧先用上次服务端下发的配色缓存, 避免默认蓝闪一下再变
    const sv = localStorage.getItem("wft-ui-theme");
    if (sv) t = sv;
  }
  if (!t) {
    // 首装统一默认「信号台」；已有用户的显式选择保留
    t = "signal";
    try { localStorage.setItem(LS.theme, t); } catch (e) {}
  }
  if (THEME_MIGRATE[t]) {
    t = THEME_MIGRATE[t];
    try { localStorage.setItem(LS.theme, t); } catch (e) {}
  }
  // 一次性迁移：这次是整体视觉改造，旧皮肤首次升级统一进入「信号台」；
  // 迁移只做一次，之后用户在 CC设置 里仍可自由切回其他配色。
  try {
    if (localStorage.getItem("wft-signal-theme-v1") !== "1") {
      t = "signal";
      localStorage.setItem("wft-signal-theme-v1", "1");
      localStorage.setItem(LS.theme, t);
    }
  } catch (e) {}
  // 手机 App 四页改版: 首次进入统一信号台, 并把选择写回本机服务偏好;
  // 之后用户在 CONFIG 里仍可自由切换。
  try {
    if (mobileShell && localStorage.getItem("wft-mobile-signal-v2") !== "1") {
      t = "signal";
      mobileSignalLock = true;
      localStorage.setItem(LS.theme, t);
      localStorage.setItem("wft-mobile-signal-v2", "1");
      saveTextPref("ui-theme", t);
    }
  } catch (e) {}
  theme.value = t;
  applyTheme(t);   // 模块自身先应用一次, 不依赖 main.js 的读取顺序
} catch (e) {}
/* 配色行的"当前"标注: 色板芯片不显示名字(走 tooltip), 选中名在此回显 */
const currentThemeName = computed(() => THEMES.find(t => t.id === theme.value)?.name || "");
/* 主题模式(亮/暗/跟随系统)与配色正交: 配色(data-theme)定色相, 模式(data-mode)定明暗 */
const THEME_MODES = [
    { id: "light",  name: "亮色" },
    { id: "dark",   name: "暗色" },
    { id: "system", name: "跟随系统" },
];
if (PREVIEW_MOBILE) {
  const _pt = _previewQ.get("theme");
  if (_pt) setTheme(_pt);
}
const systemDarkMq = window.matchMedia("(prefers-color-scheme: dark)");
const themeMode = ref((() => {
    try {
      if (!isRealDesktop) {
        const sv = localStorage.getItem("wft-ui-mode");
        if (sv) return sv;
      }
      return localStorage.getItem(LS.mode) || "dark";
    } catch (e) { return "dark"; }
})());
const themeModeName = computed(() => THEME_MODES.find(m => m.id === themeMode.value)?.name || "");
function applyThemeMode() {
    // 显式写 data-mode("dark" 无对应 CSS 规则 = 默认深色), system 按当前系统外观解析
    const dark = themeMode.value === "system" ? systemDarkMq.matches : themeMode.value === "dark";
    document.documentElement.dataset.mode = dark ? "dark" : "light";
    // 同步原生控件(滚动条/自动填充/表单)的明暗, 否则系统深色 + 应用浅色时
    // 会画出突兀的深色原生控件(index.html 的 meta 只是 JS 运行前的兜底)
    document.documentElement.style.colorScheme = dark ? "dark" : "light";
}
function setThemeMode(m) {
    themeMode.value = m;
    applyThemeMode();
    try { localStorage.setItem(LS.mode, m); } catch (e) {}
    saveTextPref("ui-mode", m);   // 同步给浏览器/移动壳
}
/* 浏览器/移动壳跟随桌面端配色: 服务端 /api/info 下发 pref */
function applyServerUiTheme(d) {
    if (isRealDesktop || !d || mobileSignalLock) return;
    const t = typeof d.uiTheme === "string" ? d.uiTheme : "";
    const m = typeof d.uiMode === "string" ? d.uiMode : "";
    if (t && THEMES.some((x) => x.id === t) && t !== theme.value) {
      theme.value = t;
      applyTheme(t);
      try { localStorage.setItem("wft-ui-theme", t); } catch (e) {}
    }
    if (m && ["light", "dark", "system"].includes(m) && m !== themeMode.value) {
      themeMode.value = m;
      applyThemeMode();
      try { localStorage.setItem("wft-ui-mode", m); } catch (e) {}
    }
}
if (typeof systemDarkMq.addEventListener === "function") systemDarkMq.addEventListener("change", applyThemeMode);
else if (typeof systemDarkMq.addListener === "function") systemDarkMq.addListener(applyThemeMode);
onUnmounted(() => {
    if (typeof systemDarkMq.removeEventListener === "function") systemDarkMq.removeEventListener("change", applyThemeMode);
    else if (typeof systemDarkMq.removeListener === "function") systemDarkMq.removeListener(applyThemeMode);
});
applyThemeMode();   // 初始落地(与 main.js 挂载前兜底双保险, 并覆盖系统外观在启动间隙的变化)

/* ---------- 手机端(窄屏): 悬浮球 + 底部聊天面板 ---------- */
const mq = window.matchMedia(`(max-width: ${NARROW_MAX_PX}px)`);
const isNarrow = ref(mq.matches);
// 纯触屏设备: 回车换行、点发送按钮提交(与手机微信一致); 桌面键盘回车发送
const touchOnly = window.matchMedia("(hover: none)").matches;
function onMqChange(e) {
  isNarrow.value = e.matches;
  if (!e.matches) {
    // 宽屏桌面由左侧设备栏/抽屉接管; 手机 App 的四页布局必须保留当前页,
    // 否则横竖屏切换会把 DEVICES/FILES 直接弹回 SEND。
    if (!mobileTabbed.value) {
      deviceInlineOpen.value = false;
      fileSheetOpen.value = false;
    }
    plusOpen.value = false;
    moreOpen.value = false;
  }
}
if (typeof mq.addEventListener === "function") mq.addEventListener("change", onMqChange);
else if (typeof mq.addListener === "function") mq.addListener(onMqChange);
function scrollChats() {
  const el = chatBox.value;
  if (!el) return;
  // 已在底部就不要每轮都赋一次 scrollTop: 无谓的滚动/重绘会打断
  // WKWebView 里输入框的光标绘制(表现为光标闪几秒后消失)
  const top = el.scrollHeight - el.clientHeight;
  if (Math.abs(el.scrollTop - top) > 1) el.scrollTop = top;
}

/* ---------- 服务控制(桌面) ---------- */
const running = ref(false);
// 局域网访问状态(服务端 /api/info 下发; 两端始终开放)。定义提前到二维码 watch 之前。
const openLanEnabled = ref(true);
// 本次服务实际监听在局域网(与偏好 openLan 不同: 拨开关后需重启服务才生效)
const lanActive = ref(true);
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
      // 只有真实桌面走本机 invoke; 移动壳与浏览器都经 HTTP 拉电脑的状态
      // (壳内 invoke 拿到的是手机本机状态, 恒"已停止"——客户端模式的关键修正)
      const d = isRealDesktop
        ? await tauriInvoke("control_status_cmd")
        : await (await apiFetch(apiBase.value + API.info)).json();
      running.value = d.running;
      port.value = d.port;
      lanIp.value = d.ip;
      if (typeof d.version === "string") appVersion.value = d.version;
      syncPinState(d);
      applyServerConfig(d.config);
      applyServerUiTheme(d);
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
/* 应用共享目录(含高危目录二次确认): 家目录/其祖先/磁盘根一旦共享,
   个人全部文件对整个 Wi-Fi 开放读写删, 必须让用户明确知情后才放行 */
/* 端口参数解析(启动与目录切换重启共用): 空=默认(null); 非法返回 NaN 由调用方提示 */
function portArgOrNaN() {
  const raw = portInput.value.trim();
  if (!raw) return null;
  const p = Number(raw);
  return Number.isInteger(p) && p >= 1 && p <= 65535 ? p : NaN;
}
/* 启动服务并回写状态(与 startServer 的启动段一致); 失败自行 toast 并返回 false */
async function _restartForDirSwitch() {
  try {
    const portArg = portArgOrNaN();
    if (Number.isNaN(portArg)) { showToast("端口需为 1-65535 的整数，服务未自动重启", "error"); return false; }
    const st = await tauriInvoke("control_start_cmd", { port: portArg });
    if (st.ok) {
      running.value = true;
      port.value = st.port;
      portInput.value = String(st.port);
      portDirty.value = false;
      return true;
    }
    showToast(st.error || "服务重启失败，请手动点「启动服务」", "error");
    return false;
  } catch (e) {
    showToast("服务重启失败，请手动点「启动服务」", "error");
    return false;
  }
}
async function applyShareDir(path) {
  // 运行中选目录: 自动"停止 → 切换 → 重启"一键完成, 不再要求用户手动三步
  // (后端"运行中拒绝"保留为安全兜底; 高危目录确认与失败回滚逻辑不变)
  let wasRunning = false;
  if (running.value) {
    const yes = await appConfirm({
      title: "切换共享目录",
      message: "服务正在运行。修改共享目录将自动执行：停止服务 → 切换目录 → 重启服务（手机端需刷新页面）。继续吗？",
      confirmText: "继续",
    });
    if (!yes) return { ok: false, cancelled: true };
    try {
      await tauriInvoke("control_stop_cmd");
    } catch (e) {
      return { ok: false, error: "停止服务失败，共享目录未修改" };
    }
    running.value = false;
    wasRunning = true;
  }
  const finish = (res) => {
    // 切换被取消/失败且已停过服务 → 把服务恢复原状, 不留半停状态
    if (wasRunning && !res.ok) {
      if (res.cancelled) {
        _restartForDirSwitch();
        return { ok: false, cancelled: true };
      }
      _restartForDirSwitch();
      return res;
    }
    return res;
  };
  const d = await tauriInvoke("set_share_dir_cmd", { path });
  if (!d.dangerous) {
    if (!d.ok) return finish(d);
    clearShareTicketCache();   // 服务端已清空旧目录票据
    if (wasRunning) {
      const restarted = await _restartForDirSwitch();
      if (restarted) showToast("共享目录已切换，服务已重启");
      return { ok: true, restarted: restarted };
    }
    return d;
  }
  const yes = await appConfirm({
    title: "高风险共享目录",
    message: `「${path}」是家目录、其上级或磁盘根目录。\n共享后同一 Wi-Fi 内的任何设备都能读取、覆盖、删除其中的全部文件。确定继续吗？`,
    confirmText: "仍然共享", danger: true,
  });
  if (!yes) return finish({ ok: false, cancelled: true });
  const d2 = await tauriInvoke("set_share_dir_cmd", { path, force: true });
  if (!d2.ok) return finish(d2);
  clearShareTicketCache();
  if (wasRunning) {
    const restarted = await _restartForDirSwitch();
    if (restarted) showToast("共享目录已切换，服务已重启");
    return { ok: true, restarted: restarted };
  }
  return d2;
}
async function startServer() {
  if (!isRealDesktop) { showToast("服务托管仅支持桌面端；移动壳保持客户端模式", "error"); return; }
  try {
    // 先应用共享目录(含持久化), 再启动 —— 否则输入框里的目录从未生效过
    const s = await applyShareDir(shareInput.value);
    if (!s.ok) { if (!s.cancelled) showToast(s.error || "共享目录无效", "error"); return; }
    // 端口校验前置: "99999"/非数字不再落到 invoke 反序列化失败的笼统报错
    const portArg = portArgOrNaN();
    if (Number.isNaN(portArg)) {
      showToast("端口需为 1-65535 的整数", "error");
      return;
    }
    const d = await tauriInvoke("control_start_cmd", { port: portArg });
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
  if (!isRealDesktop) return;
  try {
    // 目录选择只在真实桌面用: 动态加载, 移动端/浏览器不下载该模块
    const { open: openDialog } = await import("@tauri-apps/plugin-dialog");
    const sel = await openDialog({ directory: true, title: "选择共享目录" });
    if (!sel) return;   // 用户取消
    // 立即落盘: 后端更新后轮询回写的才是新目录, 否则 2.5s 内会被旧值冲掉
    const d = await applyShareDir(sel);
    if (d.ok) {
      shareInput.value = sel;
      // 运行中切换已由 applyShareDir 重启并提示, 这里只补静默停服场景的文案
      if (!d.restarted) showToast("共享目录已更新，下次启动服务生效");
    } else if (!d.cancelled) {
      showToast(d.error || "设置失败", "error");
    }
  } catch (e) {
    // Android/iOS 的 dialog 插件未实现目录选择(直接返回错误) → 明确提示;
    // 真实桌面的失败(IPC 竞态等)如实报错, 不误报"平台不支持"
    showToast(isRealDesktop ? ("设置失败：" + (e?.message || e)) : "当前平台不支持目录选择，请手动输入路径", "error");
  }
}
const VIEWED_FILES_KEY = "wft-viewed-files";
const viewedFiles = ref((() => {
  try {
    const saved = JSON.parse(localStorage.getItem(VIEWED_FILES_KEY) || "[]");
    return new Set(Array.isArray(saved) ? saved.slice(-2000) : []);
  } catch (e) { return new Set(); }
})());
function saveViewedFiles() {
  try { localStorage.setItem(VIEWED_FILES_KEY, JSON.stringify([...viewedFiles.value].slice(-2000))); } catch (e) {}
}
function markFileViewed(name) {
  if (!name || viewedFiles.value.has(name)) return;
  const next = new Set(viewedFiles.value);
  next.add(name);
  if (next.size > 2000) {
    const arr = [...next].slice(-2000);
    viewedFiles.value = new Set(arr);
  } else viewedFiles.value = next;
  saveViewedFiles();
}
function isFileViewed(name) { return !!name && viewedFiles.value.has(name); }
function openFile(name) {
  if (!isRealDesktop) return;   // Android/iOS 无系统程序打开能力, 走下载路径
  markFileViewed(name);
  const dir = shareInput.value.replace(/\/+$/, "");
  tauriInvoke("open_path_cmd", { path: dir + "/" + name })
    .then(d => { if (!d.ok) showToast(d.error || "打开失败", "error"); })
    .catch(() => showToast("打开失败", "error"));
}
function copyFileToClipboard(name) {
  markFileViewed(name);
  tauriInvoke("copy_file_cmd", { name })
    .then(d => d.ok ? showToast("已复制到剪贴板，可在访达/资源管理器中粘贴") : showToast(d.error || "复制失败", "error"))
    .catch(() => showToast("复制失败", "error"));
}
async function stopServer() {
  if (!isRealDesktop) return;
  try {
    await tauriInvoke("control_stop_cmd");
    running.value = false;
    showToast("服务已停止");
  } catch (e) {
    console.error("停止服务异常:", e);
    showToast("停止服务异常", "error");
  }
}
/* 行双击打开文件; 但按钮/勾选框上的双击必须忽略 —— 否则"复制"按钮连点两下
   第一下复制、第二下冒泡到行触发打开文件 */
function rowDblClick(e, f) {
  if (e.target.closest("button, label, .tx-thumb")) return;
  if (isRealDesktop) openFile(f.name);
}
function openBrowser() {
  if (!running.value) { showToast("服务未启动", "error"); return; }
  // 安全模式只监听回环: 用局域网 IP 打开自己的网页会连不上
  const base = lanActive.value ? shareUrl.value : `http://127.0.0.1:${port.value}`;
  openExternal(base);
}

/* ---------- 二维码 ---------- */
// 单一数据源: watch [shareUrl, running] 驱动生成, 三处展示(服务中心/弹窗/介绍页)共用一份
const qrData = ref("");
const qrShow = ref(false);
const qrFloatEnabled = ref((() => {
  try { return localStorage.getItem("wft-qr-float-enabled") !== "0"; } catch (e) { return true; }
})());
/* ---------- Pet: 贴边收拢的悬浮码 ----------
   收拢 = 呼吸圆点贴边"漏一半"(窗口中心压边缘线, 64px 圆点半隐半露, 四周 16px 透明缓冲
   即"鼠标靠近"命中区); 指针停留 1.5s = 滑出展开 136×190 扫码卡(纯预览, 离开 250ms 内收拢);
   拖动松手吸附最近边(在哪块屏就吸哪块屏的边, 支持扩展屏)。
   dock 记忆带显示器锚(mon={name,x,y}), 重启恢复到同一块屏; 记忆屏离线时
   落当前屏且不改锚, 插回后由 heal 巡检自动归位。首次落位完成前不 show,
   防止先在 Rust 默认位(主屏左缘)闪现。
   窗口尺寸与位置全部由本窗 setSize/setPosition 同步(逻辑px):
   收拢 96×96(= Rust PET 窗初始 inner_size) / 展开 136×190。
   startDragging 是异步原生拖拽(macOS/Linux 发起即返回), 不能当"拖完"信号 ——
   吸附挂在 onMoved 静默 140ms 的防抖上, 拖拽/系统移窗停顿后统一贴边。 */
const PET_ORB_W = 96, PET_ORB_H = 96, PET_CARD_W = 136, PET_CARD_H = 190;
const PET_DOCK_KEY = "wft-qr-pet-dock";
const qrPetExpanded = ref(false);   // 几何态: 决定窗口尺寸与半隐/贴边(petApply 执行时读它)
const qrPetShown = ref(false);      // 展示态: 模板 expanded class —— 与几何态解耦, 过渡零挤压帧
const petEdge = ref("left");        // 当前吸附边(模板按它放状态点/面板滑入方向)
const petNotify = ref(false);       // 外圈绿点: 新文件/新消息，直到回到主窗查看
const petMainActive = ref(false);   // 可见且未最小化且获得焦点，才视为正在查看
const petActivity = createPetActivityTracker();
let petMainStateVersion = 0;
let petMainStateQuery = null;
let petDock = null;                 // { edge, cross(沿边中心, 相对工作区逻辑px), mon?:{name,x,y}(显示器锚) }
let petPlaced = null;               // 首次成功落位 Promise: show 前 await, 防主屏默认位闪现
let petPlacedResolve = null;
let petWaFallback = false;          // 最近一次落位用了兜底屏(记忆显示器离线), 目标屏回归后 heal 归位
let petPress = null;
let petResizing = false;            // 程序化改窗口期间忽略 onMoved 吸附, 防缩放-悬停抖动回环
let petOpDepth = 0;                 // 几何操作并发深度: 定时器清 petResizing 前复查, 防后继任务中途被放行
let petSnapTimer = null;
let unlistenPetMoved = null;
let unlistenMainVis = null;
let petPollTimer = null;
let petHealTimer = null;    // 显示器拓扑变化(睡眠/改分辨率/拔屏)后窗口落屏外的自愈巡检
let petHoverTimer = null;   // 靠近后须停留 1.5s 才展开(路过不弹, 意图明确才出码)
let petCloseTimer = null;   // 离开后 250ms 宽限再收起(指针在边界抖动不闪断)
const petClamp = (v, a, b) => Math.min(Math.max(v, a), b);
/* Windows 混合 DPI: tao 把 LogicalPosition/LogicalSize 按**窗口当前所在屏**的缩放
   换算成物理像素。Pet 的目标屏(记忆屏/扩展屏)与窗口当前屏缩放不同时, 落位会算错并
   永久出屏(heal 每 30s 用同一错误公式重算 → 死循环)。Windows 下直接给物理坐标/
   物理尺寸, 由目标屏的 scaleFactor 自行换算, 绕开 tao 的"当前屏"假设。
   macOS/Linux 的全局坐标系是逻辑点, 保持 Logical 才不会跨屏漂移。 */
const isWindows = /Windows/i.test(navigator.userAgent);
function petPosArg(x, y, sf) {
  return isWindows
    ? new PhysicalPosition(Math.round(x * sf), Math.round(y * sf))
    : new LogicalPosition(x, y);
}
function petSizeArg(w, h, sf) {
  return isWindows
    ? new PhysicalSize(Math.round(w * sf), Math.round(h * sf))
    : new LogicalSize(w, h);
}

/* 显示器身份: {name, x, y}(物理原点) —— dock 记忆靠它跨重启定位到同一块屏。
   同名多屏(双联同款)用原点就近; 系统改名但位置没动 → 退化按原点匹配。 */
function petMonIdentity(mon) {
  return { name: mon.name || "", x: mon.position.x, y: mon.position.y };
}
async function petFindMonitor(id) {
  if (!id || typeof id.name !== "string" || !Number.isFinite(id.x) || !Number.isFinite(id.y)) return null;
  let list = [];
  try { list = await availableMonitors(); } catch (e) { return null; }
  const dist = (m) => Math.abs(m.position.x - id.x) + Math.abs(m.position.y - id.y);
  const byName = list.filter((m) => (m.name || "") === id.name);
  if (byName.length) { byName.sort((a, b) => dist(a) - dist(b)); return byName[0]; }
  return list.find((m) => m.position.x === id.x && m.position.y === id.y) || null;
}
function petMonWa(mon, viaFallback) {
  const sf = mon.scaleFactor || 1;
  const wa = mon.workArea;
  return {
    x: wa.position.x / sf, y: wa.position.y / sf,
    w: wa.size.width / sf, h: wa.size.height / sf,
    sf, id: petMonIdentity(mon), viaFallback: !!viaFallback,
  };
}
/* 工作区(避开任务栏/Dock, 逻辑px) + 缩放比 + 显示器身份(id); 拿不到则 null。
   preferDock=true: 先找 dock 记忆的显示器(启动恢复/展开落位/自愈 —— 不这样会
   按窗口当前所在的主屏解析, 重启后从扩展屏跳回主屏); 记忆屏离线则退当前屏并
   标 viaFallback(不改锚, 插回后 heal 归位)。
   preferDock=false: 就近当前屏 —— 拖拽吸附专用, 松手在哪块屏就吸哪块屏的边。 */
async function petWorkArea(preferDock) {
  try {
    if (preferDock && petDock && petDock.mon) {
      const m = await petFindMonitor(petDock.mon);
      if (m) return petMonWa(m, false);
      // 记忆屏不在线: 退当前屏, viaFallback 提醒调用方别回写 cross/mon
      const cur = await petWorkArea(false);
      return cur ? { ...cur, viaFallback: true } : null;
    }
    // 必须用模块级 currentMonitor(): WebviewWindow 实例上没有该方法
    // (win.currentMonitor() 会 TypeError 被静默吞掉, 从 v1.1.9 起一直走
    //  availableMonitors()[0] 兜底 = 永远主屏 —— 球拖到扩展屏会被吸回主屏的根因)。
    // 模块级版本经 plugin:window|current_monitor 注入调用方窗口, 返回球所在屏。
    let mon = null;
    try { mon = await currentMonitor(); } catch (e) {}
    if (!mon) {
      // currentMonitor 拿不到时按几何选屏(全部换算到逻辑点比较, 跨屏混 DPI 安全):
      // 窗口中心落在哪块屏就用哪块, 落空再退 [0]
      try {
        const win = getCurrentWebviewWindow();
        const [lp, ls, winSf] = await Promise.all([
          win.outerPosition(), win.outerSize(), win.scaleFactor(),
        ]);
        // Windows 全物理空间比较; 其他平台统一到全局逻辑点(混合 DPI 安全)
        const cx = isWindows ? lp.x + ls.width / 2 : lp.x / winSf + ls.width / winSf / 2;
        const cy = isWindows ? lp.y + ls.height / 2 : lp.y / winSf + ls.height / winSf / 2;
        const list = await availableMonitors();
        mon = list.find((m) => {
          const sf = m.scaleFactor || 1;
          const x = isWindows ? m.position.x : m.position.x / sf;
          const y = isWindows ? m.position.y : m.position.y / sf;
          const w = isWindows ? m.size.width : m.size.width / sf;
          const h = isWindows ? m.size.height : m.size.height / sf;
          return cx >= x && cx < x + w && cy >= y && cy < y + h;
        }) || list[0] || null;
      } catch (e) {}
    }
    if (!mon) return null;
    return petMonWa(mon, false);
  } catch (e) { return null; }
}
/* 吸附位姿: 边缘 + 沿边中心 → 窗口左上角(逻辑px), 跨轴夹紧在工作区内。
   collapsed=收拢: 窗口中心压边缘线(圆点漏一半在屏内); expanded=展开: 完整贴边不越界。 */
function petDockPos(edge, cross, w, h, wa, collapsed) {
  if (!collapsed) {
    // 展开: 完整贴边, 面板中心对齐 cross
    if (edge === "left" || edge === "right") {
      return {
        x: edge === "left" ? wa.x : wa.x + wa.w - w,
        y: wa.y + petClamp(cross - h / 2, 0, Math.max(0, wa.h - h)),
      };
    }
    return {
      x: wa.x + petClamp(cross - w / 2, 0, Math.max(0, wa.w - w)),
      y: edge === "top" ? wa.y : wa.y + wa.h - h,
    };
  }
  // 收拢: **球心**压边缘线(漏一半), cross 语义 = 球心(与历史 dock 数据兼容); 球在 96×96 窗内居中
  const orbYo = h / 2;
  if (edge === "left" || edge === "right") {
    return {
      x: edge === "left" ? wa.x - w / 2 : wa.x + wa.w - w / 2,
      y: wa.y + petClamp(cross - orbYo, 0, Math.max(0, wa.h - h)),
    };
  }
  return {
    x: wa.x + petClamp(cross - w / 2, 0, Math.max(0, wa.w - w)),
    y: edge === "top" ? wa.y - orbYo : wa.y + wa.h - orbYo,
  };
}
/* 同步吸附边(模板 edge-* class: 状态点贴可见侧 / 面板滑入方向) */
function setPetDock(d) {
  petDock = d;
  if (d) petEdge.value = d.edge;
}
/* 最近边吸附: 距离并列时按 left→right→top→bottom 优先(默认停左侧) */
function petNearestDock(rect, wa) {
  const cands = [
    ["left", rect.x - wa.x],
    ["right", (wa.x + wa.w) - (rect.x + rect.w)],
    ["top", rect.y - wa.y],
    ["bottom", (wa.y + wa.h) - (rect.y + rect.h)],
  ];
  cands.sort((a, b) => a[1] - b[1]);   // ES2019 起稳定排序, 等距保持声明顺序
  const edge = cands[0][0];
  return {
    edge,
    cross: (edge === "left" || edge === "right")
      ? rect.y + rect.h / 2 - wa.y
      : rect.x + rect.w / 2 - wa.x,
  };
}
/* 保存吸附记忆: Rust 文件为权威来源(同步落盘, 重启不丢); localStorage 仅作历史兜底缓存。
   invoke 串行化 —— 快速连续保存(展开/收拢/吸附连发)时防止后写覆盖新值。 */
let petSaveChain = Promise.resolve();
function petSaveDock() {
  if (!petDock) return;
  const s = JSON.stringify(petDock);
  try { localStorage.setItem(PET_DOCK_KEY, s); } catch (e) {}
  if (isDesktop) {
    petSaveChain = petSaveChain
      .then(() => tauriInvoke("save_pet_dock_cmd", { dock: s }))
      .catch(() => {});
  }
}
/* 诊断落盘: 每类事件独立 pref 文件(单文件 last-write-wins 会把 enter/fire 盖掉),
   载荷带时间戳 —— 读一组 pet-dbg-* 即可还原完整时序 */
function petDiag(o) {
  if (!isDesktop || !DIAG_ENABLED) return;
  try {
    const key = "pet-dbg-" + String(o.fn || "x").replace(/[^a-z0-9-]/g, "").slice(0, 16);
    tauriInvoke("save_text_pref_cmd", {
      key,
      value: JSON.stringify({ t: Date.now(), ...o }).slice(0, 480),
    }).catch(() => {});
  } catch (e) {}
}
/* isVisible 包含后台和部分平台的最小化窗口，不能当作“用户已读”。
   新内容到达时复查原生窗口状态；焦点事件与定时查询一起兜底。 */
async function markPetNotify() {
  if (!isQrPetWindow) return;
  await petSyncMainState();
  if (!petMainActive.value) petNotify.value = true;
}
function petApplyMainState(state) {
  petMainActive.value = state?.active === true;
  if (petMainActive.value) petNotify.value = false;
}
function petSyncMainState() {
  if (petMainStateQuery) return petMainStateQuery;
  const version = petMainStateVersion;
  petMainStateQuery = tauriInvoke("main_visible_cmd")
    .then((r) => {
      // 查询在途时的新焦点事件优先，迟到的旧快照不能覆盖它。
      if (r?.ok && version === petMainStateVersion) petApplyMainState(r);
    })
    .catch(() => {})
    .finally(() => { petMainStateQuery = null; });
  return petMainStateQuery;
}
function petParseDock(s) {
  try {
    const d = JSON.parse(s || "null");
    if (d && ["left", "right", "top", "bottom"].includes(d.edge) && Number.isFinite(d.cross)) {
      // mon(显示器锚)可选: 旧格式没有 —— 剥掉脏数据, 首次落位按当前屏补锚
      const m = d.mon;
      if (m && typeof m === "object" && typeof m.name === "string"
          && Number.isFinite(m.x) && Number.isFinite(m.y)) {
        d.mon = { name: m.name, x: m.x, y: m.y };
      } else delete d.mon;
      return d;
    }
  } catch (e) {}
  return null;
}
function petLoadDockLocal() {
  try { return petParseDock(localStorage.getItem(PET_DOCK_KEY)); } catch (e) { return null; }
}
/* Rust 文件优先(可靠), localStorage 兜底(迁移旧记忆); 都没有 → null 用默认左侧中部 */
async function petLoadDock() {
  try {
    const r = await tauriInvoke("load_pet_dock_cmd");
    const d = r?.ok ? petParseDock(r.dock) : null;
    if (d) return d;
  } catch (e) {}
  return petLoadDockLocal();
}
/* 几何操作单飞串行: 展开/收拢/吸附/锁屏重试落位可能并发, 交错 setSize/setPosition
   会产生"卡片配收拢位姿"错配 —— 诊断 pref-pet-last 实锤过(136×190 卡片被按半隐
   算到 y=-70, 二维码出血一半)。所有几何变更排队执行, 永不交叠。 */
let petOpChain = Promise.resolve();
function petSerialize(fn) {
  const run = petOpChain.then(fn, fn);   // 前序失败不阻塞后续
  petOpChain = run.then(() => {}, () => {});
  return run;
}
/* 改窗口尺寸并按吸附记忆落位; 尺寸/位姿在"执行时"从 ref 推导(同源), 返回是否成功 */
async function petApply() {
  return petSerialize(_petApply);
}
async function _petApply() {
  petResizing = true;
  petOpDepth++;
  try {
    const win = getCurrentWebviewWindow();
    let wa = await petWorkArea(true);   // 记忆屏优先: 启动恢复不能按当前(主屏)屏解析
    if (!wa) {   // currentMonitor 偶发拿不到: 等 300ms 重试一次再判失败
      await new Promise((r) => setTimeout(r, 300));
      wa = await petWorkArea(true);
    }
    if (!wa) { petDiag({ fn: "apply-fail", why: "wa" }); return false; }
    if (!petDock) { petDiag({ fn: "apply-fail", why: "dock" }); return false; }
    petWaFallback = !!wa.viaFallback;   // 记忆屏离线中: heal 见它回归后主动归位
    const expanded = qrPetExpanded.value;   // 执行时刻的状态: 尺寸与半隐/贴边都用它
    const w = expanded ? PET_CARD_W : PET_ORB_W;
    const h = expanded ? PET_CARD_H : PET_ORB_H;
    const pos = petDockPos(petDock.edge, petDock.cross, w, h, wa, !expanded);
    // 顺序按状态走, 过渡期间永不露出屏幕外:
    // 展开 = 先就位(完整贴边)再放大(原点固定向屏内生长); 收拢 = 先缩小再滑入半隐
    if (expanded) {
      await win.setPosition(petPosArg(pos.x, pos.y, wa.sf));
      await win.setSize(petSizeArg(w, h, wa.sf));
    } else {
      await win.setSize(petSizeArg(w, h, wa.sf));
      await win.setPosition(petPosArg(pos.x, pos.y, wa.sf));
    }
    if (!wa.viaFallback) {
      // 跨轴被夹紧时锚点会偏移 → 回写实际锚点; 收拢态锚点=球心(A布局纵向偏移48)
      petDock.cross = (petDock.edge === "left" || petDock.edge === "right")
        ? pos.y + h / 2 - wa.y
        : pos.x + w / 2 - wa.x;
      // 老记忆(无 mon)首次在某块屏落位 → 补显示器锚, 之后重启才能回到这块屏
      if (!petDock.mon && wa.id) petDock.mon = wa.id;
    }
    // viaFallback(记忆屏离线)时 cross/mon 都不回写: 保持原锚, 插回后按原记忆归位
    petSaveDock();
    petDiag({ fn: "apply", expanded, w, h, pos, dock: petDock, wa });
    return true;
  } catch (e) {
    console.warn("悬浮码落位失败:", e);
    petDiag({ fn: "apply-fail", why: String(e).slice(0, 100) });
    return false;
  } finally {
    petOpDepth = Math.max(0, petOpDepth - 1);
    setTimeout(() => { if (petOpDepth <= 0) petResizing = false; }, 160);
  }
}
/* 松手/系统移窗静默后: 找最近边缘贴上去并持久化。
   移窗静默 = 拖拽真正结束的信号(startDragging 是异步原生拖拽, 拿不到结束回调)。 */
async function petSnap() {
  return petSerialize(_petSnap);
}
async function _petSnap() {
  petResizing = true;
  petOpDepth++;
  try {
    const win = getCurrentWebviewWindow();
    const wa = await petWorkArea(false);   // 吸附按"当前所在屏"算 —— 拖到扩展屏就吸扩展屏的边
    if (!wa || !petDock) return;
    const p = await win.outerPosition();
    const s = await win.outerSize();
    // Windows: 物理矩形先对齐目标屏物理原点, 再换算成与 wa 同一逻辑空间的矩形
    const rect = isWindows
      ? {
          x: wa.x + (p.x - wa.x * wa.sf) / wa.sf,
          y: wa.y + (p.y - wa.y * wa.sf) / wa.sf,
          w: s.width / wa.sf,
          h: s.height / wa.sf,
        }
      : { x: p.x / wa.sf, y: p.y / wa.sf, w: s.width / wa.sf, h: s.height / wa.sf };
    // 位姿按"窗口当前实际尺寸"推导(96→收拢半隐 / 136→展开贴边), 不信 ref ——
    // 过渡期 ref 与窗口尺寸会短暂错配, 信 ref 会把卡片按半隐算出去(出血一半)
    const collapsed = rect.w < (PET_ORB_W + PET_CARD_W) / 2;
    setPetDock(petNearestDock(rect, wa));
    const pos = petDockPos(petDock.edge, petDock.cross, rect.w, rect.h, wa, collapsed);
    await win.setPosition(petPosArg(pos.x, pos.y, wa.sf));
    petDock.cross = (petDock.edge === "left" || petDock.edge === "right")
      ? rect.y + rect.h / 2 - wa.y
      : rect.x + rect.w / 2 - wa.x;
    if (wa.id) petDock.mon = wa.id;   // 拖到哪块屏就把锚迁到哪块屏(重启回这块屏)
    petWaFallback = false;            // 用户主动落位 = 已在实屏上, 不再等 heal 归位
    petSaveDock();
    petDiag({ fn: "snap", collapsed, rect, dock: petDock, pos, wa });
  } catch (e) {
    console.warn("悬浮码吸附失败:", e);
  } finally {
    petPress = null;   // 拖拽结束(移窗静默=松手), 清掉按压残留
    petOpDepth = Math.max(0, petOpDepth - 1);
    setTimeout(() => { if (petOpDepth <= 0) petResizing = false; }, 160);
  }
}
/* 展开/收起: 先切状态让 CSS 就位, 再改窗口尺寸(顺序反了会先撑出一个巨型呼吸球帧) */
async function setPetExpanded(v) {
  if (!isQrPetWindow) return;
  // 快路径要求"几何+展示"双 ref 都一致才跳过 —— 只比 geo 的话, geo 卡 true 而
  // shown/window 未收敛的半途态会被当成"已展开"永久跳过, 悬停再也唤不醒(自愈入口)
  if (qrPetExpanded.value === v && qrPetShown.value === v) return;
  // 整段(意图翻转+落位+展示切换)进串行队列: 快速"展开→收起"连点时, 若只排队 petApply,
  // 完成回调拿着入队时的旧 v 会把 shown 设回 true 而窗口已按收拢态缩到 96 —— 出现
  // "96 小窗里显示扫码卡"的错配。队内完成态一律以执行时的几何态为准。
  return petSerialize(async () => {
    if (qrPetExpanded.value === v && qrPetShown.value === v) return;   // 排队期间已被覆盖
    qrPetExpanded.value = v;
    // 展示态切换时机 = 过渡零挤压的关键:
    // 收拢先切回球(球在大窗里居中不难看)再缩窗; 展开先放大窗(球随窗居中)再切卡片
    if (!v) qrPetShown.value = false;
    const ok = await _petApply();
    if (ok) {
      qrPetShown.value = qrPetExpanded.value;   // 跟随执行时几何态, 不用入队时的 v
    } else {
      qrPetExpanded.value = !v;                 // 回滚意图
      qrPetShown.value = qrPetExpanded.value;
      petSerialize(_petApply).catch(() => {});  // 队尾补一次, 把窗口尺寸拉回回滚态
    }
  });
}
function petClearHoverTimers() {
  clearTimeout(petHoverTimer); petHoverTimer = null;
  clearTimeout(petCloseTimer); petCloseTimer = null;
}
/* 悬停触发 = 纯存在性判断: 指针在圆点上停满 1.5s 即展开, 离开即取消。
   在场判定的权威来源是 petPollPresence(轮询光标 vs 窗口矩形) ——
   mouseenter/leave 是"边界过渡事件", 窗口被程序化移动/页面重载后 WebKit 会丢失
   进出配对(认为指针一直在窗内 → enter 永不再发), 实测"点击正常、悬停永不触发"。
   mouseenter/leave 仅作加速通道, 与轮询结果经 petSetOver 去重汇合。 */
let petOverOrb = false;          // 轮询/边界事件共用的"在场"状态(只在穿越边界时动作)
function petSetOver(over) {
  if (over === petOverOrb) return;
  petOverOrb = over;
  if (over) {
    clearTimeout(petCloseTimer); petCloseTimer = null;   // 回到球上: 取消待收起
    if (qrPetShown.value) return;                        // 已展开无需再计时
    if (petHoverTimer) return;
    petDiag({ fn: "enter", geo: qrPetExpanded.value, shown: qrPetShown.value });
    petHoverTimer = setTimeout(() => {
      petHoverTimer = null;
      petDiag({ fn: "expand-fire", geo: qrPetExpanded.value, shown: qrPetShown.value });
      setPetExpanded(true);   // 停留满 1.5s 才滑出二维码
    }, 1500);
  } else {
    clearTimeout(petHoverTimer); petHoverTimer = null;   // 没满 1.5s 就走 = 路过, 不弹
    // 必须在"几何态或展示态任一为展开"时排收起: 展开落位尚未完成时 qrPetShown 还是
    // false, 只看它会让离开事件被吞掉 —— 窗口随后展开却永远不再收到 leave(卡展开)
    if (qrPetShown.value || qrPetExpanded.value) {
      petDiag({ fn: "leave", geo: qrPetExpanded.value, shown: qrPetShown.value });
      petCloseTimer = setTimeout(() => {
        petCloseTimer = null;
        if (!petOverOrb) setPetExpanded(false);   // 250ms 内又回到球上则取消收起
      }, 250);
    }
  }
}
function petEnter() { petSetOver(true); }    // 加速通道(可能因边界状态丢失而不发, 不作依赖)
function petLeave() { petSetOver(false); }
/* 在场轮询: 每 250ms 查光标是否落在本窗矩形内(物理像素) —— 命中测试级可靠,
   与窗口移动/页面重载/边界事件状态完全无关 */
let petPollOkLogged = false;
let petPollBusy = false;   // 上一拍未落定不叠新拍: native 拖拽/锁屏期 invoke 挂起时,
                           // 250ms 间隔会堆叠批次, 唤醒后集中 settle 造成突发风暴
async function petPollPresence() {
  if (petResizing || petPollBusy) return;   // 落位期间窗口在动, 等下一拍
  petPollBusy = true;
  try {
    const win = getCurrentWebviewWindow();
    // 跨屏坐标空间换算(混 DPI 必须统一到全局逻辑点再比较):
    // - cursorPosition raw = 全局逻辑点 × 主屏 sf(tao util::cursor_position 固定
    //   乘 primary_monitor().scale_factor) —— 球在扩展屏(sf1)而主屏 sf2 时,
    //   不换算则光标值翻倍, 悬停永远判不中(扩展屏上悬停失效的根因);
    // - outerPosition raw = 逻辑点 × 窗口所在屏 sf, 除以 win.scaleFactor() 即逻辑点
    const [p, pos, size, winSf, pri, mons] = await Promise.all([
      cursorPosition(), win.outerPosition(), win.outerSize(),
      win.scaleFactor(), primaryMonitor().catch(() => null), availableMonitors().catch(() => []),
    ]);
    // 主屏 sf: primaryMonitor 权限缺失/拿不到时退化为"原点 (0,0) 的屏"
    // (macOS 主屏恒在全局原点), 再退化 1 —— 保证光标换算永远有基准
    const priSf = (pri && pri.scaleFactor)
      || ((mons.find((m) => m.position.x === 0 && m.position.y === 0) || {}).scaleFactor)
      || 1;
    // Windows: cursorPosition / outerPosition / outerSize 都是真实物理像素,
    // 同空间直接比较;"÷主屏 sf"只在 macOS 成立(主屏 sf≠扩展屏 sf 时在 Windows 必挂)。
    const cx = isWindows ? p.x : p.x / priSf, cy = isWindows ? p.y : p.y / priSf;
    const rx = isWindows ? pos.x : pos.x / winSf;
    const ry = isWindows ? pos.y : pos.y / winSf;
    const rw = isWindows ? size.width : size.width / winSf;
    const rh = isWindows ? size.height : size.height / winSf;
    if (!petPollOkLogged) {
      petPollOkLogged = true;
      petDiag({ fn: "poll-ok", cursor: { x: cx, y: cy }, rect: { x: rx, y: ry, w: rw, h: rh }, priSf, winSf });
    }
    const over = cx >= rx && cx < rx + rw && cy >= ry && cy < ry + rh;
    petSetOver(over);
  } catch (e) {
    if (!petPollOkLogged) { petPollOkLogged = true; petDiag({ fn: "poll-fail", why: String(e).slice(0, 90) }); }
  } finally {
    petPollBusy = false;
  }
}
/* 屏幕拓扑自愈: 启动落位重试只覆盖前 ~3 分钟; 睡眠/改分辨率/拔屏后窗口若整体
   移出工作区(悬停够不到、onMoved 不触发), 30s 巡检发现即按停靠记忆重新落位。
   另覆盖扩展屏"离线兜底 → 插回归位": 上次落位用了兜底屏而这次记忆屏找得到 → 搬回。 */
function petHealTick() {
  if (petResizing) return;
  (async () => {
    try {
      const win = getCurrentWebviewWindow();
      const wa = await petWorkArea(true);
      if (!wa) return;
      let fresh = false;
      if (!petDock) {
        setPetDock(await petLoadDock() || { edge: "left", cross: wa.h / 2 });
        fresh = true;
      }
      const monBack = petWaFallback && !wa.viaFallback;
      const p = await win.outerPosition();
      const s = await win.outerSize();
      // Windows 统一在物理像素空间比较(混合 DPI 下逻辑换算会跨屏失真)
      const cx = isWindows ? p.x + s.width / 2 : p.x / wa.sf + (s.width / wa.sf) / 2;
      const cy = isWindows ? p.y + s.height / 2 : p.y / wa.sf + (s.height / wa.sf) / 2;
      const pad = isWindows ? 16 * wa.sf : 16;
      const mx = isWindows ? wa.x * wa.sf : wa.x;
      const my = isWindows ? wa.y * wa.sf : wa.y;
      const mw = isWindows ? wa.w * wa.sf : wa.w;
      const mh = isWindows ? wa.h * wa.sf : wa.h;
      const outside = cx < mx - pad || cx > mx + mw + pad || cy < my - pad || cy > my + mh + pad;
      if (fresh || monBack || outside) {
        petDiag({ fn: "heal", fresh, monBack, cx: Math.round(cx), cy: Math.round(cy) });
        await petApply();
      }
    } catch (e) { /* 瞬时异常, 下一巡再试 */ }
  })();
}
function petStartTimers() {
  if (petPollTimer) return;
  petPollPresence();
  // 400ms: 悬停展开本身有 1.5s 停留门槛, 提高轮询间隔几乎无感, 但能把常驻 IPC 降三分之一
  petPollTimer = setInterval(petPollPresence, 400);
  petHealTimer = setInterval(petHealTick, 30000);
}
function petStopTimers() {
  clearInterval(petPollTimer); petPollTimer = null;
  clearInterval(petHealTimer); petHealTimer = null;
}

/* Pet 交互: 按下仅记录原点, 移动超阈值才 startDragging —— pointerdown 里立刻调会提前进入
   macOS 原生拖拽循环(吞掉后续 click, 且拿不到真实拖拽事件导致窗口不动)。未拖动松手即点按:
   打开主窗口; 拖动过则拦掉残余 click 防误触 */
function startPetShell(event) {
  if (event.button !== 0 || !isQrPetWindow) return;
  petClearHoverTimers();   // 按下 = 点击/拖拽意图, 取消悬停展开与待收起
  // 捕获指针: 96px 收拢窗口上斜着一拉指针很快出窗, 不捕获就收不到后续 move, 12px 拖拽阈值永远凑不满
  try { event.target.setPointerCapture?.(event.pointerId); } catch (e) {}
  petPress = { x: event.clientX, y: event.clientY, dragged: false, pid: event.pointerId };
}
function petShellMove(event) {
  if (!petPress || petPress.dragged || !event.buttons) return; // 无按键的移动不判拖(触控板轻点常带几px抖动)
  const dx = Math.abs(event.clientX - petPress.x), dy = Math.abs(event.clientY - petPress.y);
  if (dx > 12 || dy > 12) {
    petPress.dragged = true;
    // 交棒给系统原生拖拽前先释放捕获: 原生拖拽会吞掉 pointerup, capture 残留会一直压制
    // mouseenter/mouseleave, 拖完悬停展开就再也不触发了
    try { event.target.releasePointerCapture?.(petPress.pid); } catch (e) {}
    getCurrentWebviewWindow().startDragging().catch(() => {});
  }
}
function petShellUp(event) {
  try { event.target.releasePointerCapture?.(event.pointerId); } catch (e) {}
}
function petClick(event) {
  if (!isQrPetWindow) return;
  const pressed = petPress;
  petPress = null;
  if (pressed && pressed.dragged) return; // 拖动后的残余 click
  // 点击(圆点或预览面板) = 打开主窗口, 并收起面板回到圆点 —— 预览面板不提供任何点击功能
  petClearHoverTimers();
  setPetExpanded(false);
  tauriInvoke("show_main_window_cmd").catch(() => {});
}

/* 悬浮码开关是主窗/Pet 窗共享状态: 两窗各自一份 Vue ref, 互不可见 —— 若一端只改本窗,
   主窗 CC设置开关不联动, 且对端状态轮询会把 Pet 重新拉起, 关闭形同失效。
   故任一端改动都写持久层 + 广播, 对端监听回写。关闭入口只保留 CC设置(圆点上不再放 ✕)。
   持久层 = Rust pref 文件(localStorage 不落盘只作会话缓存), 满足"重启后仍保持关闭"。 */
function setQrFloatEnabled(v) {
  qrFloatEnabled.value = v;
  try { localStorage.setItem("wft-qr-float-enabled", v ? "1" : "0"); } catch (e) {}
  if (isDesktop) {
    tauriInvoke("save_text_pref_cmd", { key: "qr-float-enabled", value: v ? "1" : "0" }).catch(() => {});
    emit("qr-float://changed", { enabled: v }).catch(() => {});
  }
}

function toggleQrFloatEnabled() {
  setQrFloatEnabled(!qrFloatEnabled.value);
  if (isRealDesktop) {
    tauriInvoke("show_qr_pet_cmd", { show: qrFloatEnabled.value })
      .then((d) => {
        // 启动 2.5s 内 Pet 尚未完成落位时的拒绝属预期(它会自己按门控显示), 不弹错误
        if (!d?.ok && !/尚未/.test(d?.error || "")) showToast(d?.error || "悬浮二维码状态同步失败", "error");
      })
      .catch((e) => showToast(`悬浮二维码状态同步失败：${e?.message || e}`, "error"));
  }
}

function openQr() {
  // running 翻 false 的同一微任务里 watch 已清空 qrData, 不存在"瞬断但二维码仍在"的窗口期, 直接按状态拦截
  if (!running.value) { showToast("服务未启动，无法显示二维码", "error"); return; }
  qrShow.value = true;
}

/* ---------- 复制(降级兼容) ---------- */
/* [临时诊断] 复制失败链路定位: 阶段/安全上下文/剪贴板API/执行结果 落盘 pref-dbg-copy */
function copyDiag(stage, extra) {
  if (!DIAG_ENABLED) return;
  try {
    tauriInvoke("save_text_pref_cmd", {
      key: "dbg-copy",
      value: JSON.stringify({ t: Date.now(), stage, secure: window.isSecureContext, clip: !!navigator.clipboard, ...(extra || {}) }).slice(0, 480),
    }).catch(() => {});
  } catch (e) {}
}
/* 复制彻底失败(无剪贴板权限/激活已过期)时的兜底: 把内容放进可选中的弹框,
   而不是只弹一句"请手动复制"却无处可复制(移动壳冷路径尤甚) */
const copyFallbackShow = ref(false);
const copyFallbackText = ref("");
const copiedMsgKey = ref("");
let copiedMsgTimer = 0;
function fallbackCopy(text, okMsg) {
  const ta = document.createElement("textarea");
  ta.value = text; ta.style.position = "fixed"; ta.style.opacity = "0";
  document.body.appendChild(ta); ta.select();
  let ok = false, err = "";
  try { ok = document.execCommand("copy"); } catch (e) { err = String(e && e.message || e); }
  ta.remove();
  copyDiag("exec", { ok, err });
  if (ok) {
    showToast(okMsg);
    return true;
  }
  copyFallbackText.value = text;
  copyFallbackShow.value = true;
  return false;
}
function selectCopyFallback(e) {
  const ta = e.target.closest(".dialog-box")?.querySelector("textarea");
  if (ta) { ta.focus(); ta.select(); }
}
function copyText(text, okMsg) {
  const value = String(text == null ? "" : text);
  const message = okMsg || "已复制";
  return new Promise((resolve) => {
    const ok = () => { copyDiag("ok"); showToast(message); resolve(true); };
    const fail = (stage, e) => {
      copyDiag(stage, { err: String((e && e.message) || e) });
      resolve(fallbackCopy(value, message));
    };
    copyDiag("enter", {});
    // Tauri 壳(WKWebView/Android WebView)优先走原生剪贴板插件:
    // navigator.clipboard 在自定义协议/WebView 里可能既无权限也不报错, 表现为"点了没反应"。
    if (isDesktop) {
      writeClipboardText(value).then(ok).catch((e) => fail("plugin-reject", e));
      return;
    }
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(value).then(ok).catch((e) => fail("clip-reject", e));
    } else {
      copyDiag("no-clip-api");
      resolve(fallbackCopy(value, message));
    }
  });
}
/* IPv6 字面量在 URL 中必须加方括号: http://[fd00::1]:1688 */
function formatHost(h) {
  const s = String(h || "");
  return s.includes(":") && !s.startsWith("[") ? `[${s}]` : s;
}
const shareUrl = computed(() => `http://${formatHost(lanIp.value)}:${port.value}`);
/* 地址栏宽度贴合内容: 宽屏不留大片空白; 窄屏(换行后)由 CSS 改为撑满整行 */
// 地址(ip:端口)或运行状态一变就重生成二维码(覆盖 IP 变化/端口顺延/手动改端口/启停全场景);
// 停服清空避免残留旧码; await 结果落地前校验地址未再变, 丢弃乱序的过期结果
async function syncQr() {
  // 安全模式(仅 127.0.0.1)下二维码指向局域网 IP 但没人能打开, 不生成误导性二维码
  if (!running.value || !lanActive.value) { qrData.value = ""; return; }
  const url = shareUrl.value;
  try {
    const qrToDataURL = await loadQrLib();
    const data = await qrToDataURL(url, { margin: 1, width: 220 });
    if (shareUrl.value === url && running.value) qrData.value = data;
  } catch (e) { /* 生成失败由 refreshStatus 轮询兜底重试 */ }
}
watch([shareUrl, running, lanActive], syncQr);

const copied = ref(false);
function copyUrl() {
  if (serviceDown()) { showToast("服务未启动", "error"); return; }
  // 安全模式只监听回环: 复制出的局域网 IP 别人打不开
  if (!lanActive.value) {
    showToast("当前为仅本机模式，请重启电脑端服务", "error");
    return;
  }
  // 浏览器首刷前 lanIp 还是占位 "--": 此刻复制出来的是 http://--:port 无效地址
  // (shareFile 对此有回落保护, copyUrl 之前漏了)
  if (!isDesktop && lanIp.value === "--") { showToast("地址还没同步好，稍等两秒再复制", "error"); return; }
  const text = `📡 局域网访问地址\n${shareUrl.value}${TIP_COPY_URL}`;
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
/* 浏览器可内联预览的类型(与服务端 is_inline_safe 对齐; 不含 svg/html/xml 防同源脚本) */
const PREVIEW_EXTS = ["jpg","jpeg","png","gif","webp","bmp","avif","pdf","txt","md","log","csv","mp3","mp4","webm","mov","m4a","ogg","wav"];
function isPreviewable(n) { return PREVIEW_EXTS.includes(extFor(n)); }
function iconFor(n) { return ICON_MAP[extFor(n)] || "FILE"; }
/* 按 Unicode 字符(而非 UTF-16 码元)截断: 与服务端 chars().take(n) 语义一致,
   避免把 emoji 的代理对切成半个字符。 */
function clampChars(s, n) {
  const arr = Array.from(String(s ?? ""));
  return arr.length > n ? arr.slice(0, n).join("") : arr.join("");
}
/* 大小格式化 [同步: server.rs humans] —— B 不带小数, 其余一位小数, 单位表两端一致 */
function fmtSize(b) {
  if (b === 0) return "0 B";
  const u = ["B","KB","MB","GB","TB"];
  let i = 0, v = b;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return v.toFixed(i === 0 ? 0 : 1) + " " + u[i];
}
/* 具体时间(月-日 时:分, 跨年补年份), 聊天与文件共用同一格式 —— 各端展示统一;
   不再用"刚刚/几分钟前": 相对时间在多端同时查看时容易误读, 也无法直接定位消息 */
function fmtTime(ts) {
  const d = new Date(ts * 1000);
  const p = (n) => String(n).padStart(2, "0");
  const md = `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
  return d.getFullYear() === new Date().getFullYear() ? md : `${d.getFullYear()}-${md}`;
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
  const epoch = sessionEpoch;   // 旧电脑的迟到响应不得写入新会话
  const reqAt = Date.now();   // 列表请求发起时刻: 判定"删除"时排除请求后才完成的上传
  const spinStart = Date.now();
  if (manual) refreshing.value = true;
  // 超时保护: 与 loadChat 同款 —— 无 AbortController 时单个挂死请求会让 loadingFiles
  // 恒 true, 之后轮询/WS/删除补拉/手动刷新全部只排队不执行, 列表冻结到浏览器底层超时
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), 10000);
  try {
    // 昵称走 URL 参数: 避免 X-Nickname 自定义头触发 CORS 预检, 刷新只需一次往返
    const r = await apiFetch(apiBase.value + API.files + "?nick=" + encodeURIComponent(deviceNick()), { signal: ctl.signal });
    if (!r.ok) throw new Error(`文件列表加载失败 (${r.status})`);
    const d = await r.json();
    if (epoch !== sessionEpoch) {
      loadingFiles = false;
      if (reloadQueued) { reloadQueued = false; loadFiles(false); }
      return;
    }
    // 内容没变就不替换 files: 每 5s 轮询都换新数组会让抽屉(乃至整页模板)重渲染,
    // 打断输入框光标的绘制 —— 静默期应零重绘
    const nextFiles = d.files || [];
    const truncated = !!d.truncated;   // 服务端只返回前 FILES_LIST_MAX 条: 不能据此判定"已删除"
    const filesSig = nextFiles.map(f => `${f.name}:${f.size}:${f.modified}`).join("|");
    if (filesSig !== loadFiles._sig) {
      loadFiles._sig = filesSig;
      allFiles.value = nextFiles;
      // 当前页票据预取(仅开 PIN 时有意义): 解决触屏/冷路径 await 丢点击激活
      nextTick(prefetchPageTickets);
    }
    // 远端文件 → 时间线(文件即消息): 只认上一次快照里没有、且不是本机上传的新文件;
    // 首次拉取只建基线, 否则打开页面时全部历史文件会刷成一串"新消息"
    const names = new Set(nextFiles.map(f => f.name));
    if (knownFileNames) {
      const selfNames = new Set(uploads.value.map(u => uploadName(u)).filter(Boolean));
      const added = nextFiles.filter(f => !knownFileNames.has(f.name) && !selfNames.has(f.name));
      if (added.length) {
        // 上传响应可能晚于 WS 广播: 同名自动改名的 final 名这时还没回写本机队列,
        // 1.5s 后按最新队列复核, 避免自己的文件在时间线里再出现一张"远端"卡片
        setTimeout(() => {
          const self = new Set(uploads.value.map(u => uploadName(u)).filter(Boolean));
          // 复核时以"当前列表"为准: 1.5s 内文件可能已被删除, 不能再补一张 404 卡片
          const current = new Set(allFiles.value.map(f => f.name));
          const fresh = added.filter(f => !self.has(f.name) && current.has(f.name));
          if (!fresh.length) return;
          const ts = Date.now() / 1000;
          remoteFiles.value = remoteFiles.value
            .concat(fresh.map(f => ({ key: "rf-" + f.name, name: f.name, size: f.size, modified: f.modified, uploader: f.uploader || "", ts })))
            .slice(-REMOTE_CARDS_MAX);
          // 文件消息也进"新消息"胶囊: 用户上翻时新文件不该被静默塞进列表
          if (!atBottom.value) jumpCount.value += fresh.length;
          else nextTick(scrollChats);
        }, 1500);
      }
      // 文件被删除: 对应卡片一并消失。列表被截断(>5000 个文件)时不能做
      // "上一轮有、这一轮没有"的判定 —— 未返回的既有文件会被误当成已删除。
      if (!truncated) {
        remoteFiles.value = remoteFiles.value.filter(c => names.has(c.name));
        // 其他设备删除本机上传的文件: 时间线里的上传卡片同样清掉。
        // 只认"上一轮列表里有、这一轮消失"的名字 —— 刚上传完成的文件上一轮还没进
        // 列表, 不能用"不在 names 就删"去判定, 否则会把在途完成的上传卡片误清。
        const vanished = new Set();
        for (const n of knownFileNames) if (!names.has(n)) vanished.add(n);
        if (vanished.size) {
          const keepUploads = uploads.value.filter(u => {
            if (u.status !== "done" || !vanished.has(uploadName(u))) return true;
            return u.doneAt && u.doneAt >= reqAt;   // 本次拉取发起后才落盘: 列表可能还没看到
          });
          if (keepUploads.length !== uploads.value.length) uploads.value = keepUploads;
        }
      }
    }
    knownFileNames = names;
    // 断线补拉/轮询兜底：新增或同名更新提醒，纯删除不提醒；首拉仅建立基线。
    if (isQrPetWindow && petActivity.observeFiles(allFiles.value)) markPetNotify();
    // 列表缩水(删除本页等)后回收越界页码: 否则页码条无高亮、‹ 可点到更越界的页
    if (currentPage.value > totalPages.value) currentPage.value = totalPages.value;
    // 清理已不存在的选中项
    if (selected.value.size) {
      const names = new Set(allFiles.value.map(f => f.name));
      const keep = [...selected.value].filter(n => names.has(n));
      if (keep.length !== selected.value.size) selected.value = new Set(keep);
    }
  } catch (e) {
    console.error(e);
    if (manual) showToast("文件列表加载失败，请检查电脑端服务", "error");
    // WS 在线时列表轮询被停掉，事件触发的失败必须自己补拉。
    setTimeout(() => { if (ws && ws.readyState === WebSocket.OPEN) loadFiles(false); }, 2000);
  }
  clearTimeout(timer);
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
  if (serviceDown()) { showToast("服务已停止，无法分享", "error"); return; }
  if (!lanActive.value) {
    showToast("当前为仅本机模式，请重启本机服务", "error");
    return;
  }   // 停服后链接打不开, 先拦截
  markFileViewed(name);
  // 浏览器端优先 shareUrl(与二维码同源), 但首次状态刷新前 lanIp 还是占位 "--" ——
  // 此时必须回落 location.origin, 否则复制出 http://--:0 无效地址
  const base = isDesktop
    ? `http://${formatHost(lanIp.value)}:${port.value}`
    : (lanIp.value === "--" ? location.origin : shareUrl.value);
  // 未开 pin → 票据无用, 完全同步复制(点击手势内, 激活不丢)。
  // 开 pin → 优先吃 pointerover 预取的缓存(同样同步); 冷点才 await 兜底
  // (激活可能过期导致复制失败, 但预取通常已在悬停期间完成, 实际很少走到)。
  let ticket = null;
  const epoch = shareTicketEpoch;   // 本次分享代际: 期间清过缓存就不再把旧票写回
  const pinOn = !!(accessPin.value || pinCode.value || pinEnabled.value);
  if (pinOn) {
    const c = shareTicketCache.get(name);
    if (c && Date.now() - c.at < SHARE_TICKET_UI_TTL_MS) {
      ticket = c.ticket;
      // 票据一次性: 取用即从缓存移除。连点两次时第二次必须走冷路径重新开票,
      // 否则两次分享拿到同一张票, 第一个收件人下载后第二个就是死链
      shareTicketCache.delete(name);
    } else {
      const t = await createShareTicket(name);
      if (t) {
        ticket = t;
        if (epoch === shareTicketEpoch) shareTicketCache.set(name, { ticket: t, at: Date.now() });
      }
    }
    if (!ticket) { showToast("生成分享链接失败", "error"); return; }
    // 用掉缓存的同时后台换新票: 收件人下载会销票, 下次分享不能拿到死链
    createShareTicket(name).then((t) => {
      if (t && epoch === shareTicketEpoch) shareTicketCache.set(name, { ticket: t, at: Date.now() });
    }).catch(() => {});
  }
  const url = ticket
    ? `${base}${API.download(name)}?ticket=${encodeURIComponent(ticket)}`
    : `${base}${API.download(name)}`;
  const displayName = name.split("/").pop() || name;
  const pinNote = pinOn ? "\n· 链接仅授权下载该文件，请仅发给可信对象" : "";
  copyText(`📄 文件分享\n${displayName}\n${url}${TIP_SHARE}${pinNote}`, "分享链接已复制：" + name);
}

/* 分享票据预取: 指针滑到「分享」按钮上就开票, 点击时命中缓存 → 复制在点击手势内
   同步完成 —— 排到 await 之后会丢掉用户激活, WKWebView 里剪贴板/execCommand 双双被拒
   (v1.1.9 分享"复制失败"的根因)。缓存 TTL 对齐服务端 10min(留 1min 余量);
   每次分享后后台换新票, 防收件人下载销票后再次分享拿到死链。 */
const shareTicketCache = new Map();   // name -> { ticket, at }
const SHARE_TICKET_UI_TTL_MS = 9 * 60 * 1000;
let shareTicketEpoch = 0;   // 清缓存代际: 在途开票的迟到响应不得写回已作废的缓存
/* 缩略图/预览 URL: 开 PIN 时用短时媒体会话令牌, 绝不把长期 PIN 拼进 <img>/<iframe> URL。
   ticket 被消费/过期后靠 @error 重新开票。 */
const thumbSrcs = ref({});
const thumbRetryAt = {};
/* 缩略图/预览共用一个 10min、IP 绑定的媒体会话令牌: 不把长期 PIN 拼进 URL,
   也不为每张图片开一次性票据(大量图片会触发开票限流)。 */
const mediaToken = ref("");
let mediaTokenEpoch = 0;
let mediaTokenPromise = null;
let mediaTokenExpiresAt = 0;
function clearMediaToken() {
  mediaToken.value = "";
  mediaTokenExpiresAt = 0;
  mediaTokenEpoch++;
  mediaTokenPromise = null;
}
function clearShareTicketCache() {
  shareTicketEpoch++;
  shareTicketCache.clear();
  thumbSrcs.value = {};
  clearMediaToken();
  // 服务重启/PIN 轮换后媒体会话失效: 当前可见图片重新取令牌, 否则缩略图会一直空白
  nextTick(() => { for (const n of thumbNames.value) loadThumb(n); });
}
function pinProtected() { return !!(accessPin.value || pinCode.value || pinEnabled.value); }
function plainFileUrl(name) { return `${apiBase.value}${API.download(name)}?inline=1`; }
async function ensureMediaToken() {
  if (!pinProtected()) return "";
  if (mediaToken.value && Date.now() < mediaTokenExpiresAt) return mediaToken.value;
  if (mediaToken.value) clearMediaToken();
  if (mediaTokenPromise) return mediaTokenPromise;
  const epoch = mediaTokenEpoch;
  const p = (async () => {
    try {
      const r = await apiFetch(apiBase.value + "/api/media-session", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "{}",
      });
      if (!r.ok) return "";
      const d = await r.json();
      const t = typeof d.token === "string" ? d.token : "";
      if (epoch !== mediaTokenEpoch) return "";   // 期间 PIN/会话已轮换
      if (t) {
        mediaToken.value = t;
        mediaTokenExpiresAt = Date.now() + Math.max(30, Number(d.ttl) || 600) * 1000 - 30000;
      }
      return t;
    } catch (e) { return ""; }
  })();
  mediaTokenPromise = p;
  try { return await p; } finally { if (mediaTokenPromise === p) mediaTokenPromise = null; }
}
async function mediaUrl(name, inline = true) {
  if (!pinProtected()) return `${apiBase.value}${API.download(name)}${inline ? "?inline=1" : ""}`;
  const t = await ensureMediaToken();
  if (!t) return "";
  return `${apiBase.value}${API.download(name)}?${inline ? "inline=1&" : ""}media=${encodeURIComponent(t)}`;
}
async function loadThumb(name) {
  if (!name || !isImage(name) || thumbSrcs.value[name]) return;
  // 移动壳本机服务尚未就绪时 apiBase 为空, 此时生成的相对 URL 必然 404
  if (isDesktop && !apiBase.value) return;
  const url = await mediaUrl(name, true);
  if (!url) return;
  thumbSrcs.value = { ...thumbSrcs.value, [name]: url };
}
function retryThumb(name) {
  if (!name) return;
  const now = Date.now();
  if (thumbRetryAt[name] && now - thumbRetryAt[name] < 2500) return;
  thumbRetryAt[name] = now;
  if (thumbSrcs.value[name]) {
    const next = { ...thumbSrcs.value };
    delete next[name];
    thumbSrcs.value = next;
  }
  // ensureMediaToken 会在令牌过期时自动换新; 这里不逐张清 token,
  // 否则一批图片同时 401 会连续开很多媒体会话。
  loadThumb(name);
}
function prefetchShareTicket(name) {
  if (!(accessPin.value || pinCode.value || pinEnabled.value)) return;   // 未开 pin 不需要票据
  if (shareTicketCache.has(name)) return;
  const epoch = shareTicketEpoch;
  createShareTicket(name).then((t) => {
    if (t && epoch === shareTicketEpoch) shareTicketCache.set(name, { ticket: t, at: Date.now() });
  }).catch(() => {});
}
function onDocShareHover(e) {
  // 合并页 DOM: 分享按钮/文件名在 .tx-frow[data-file] 内; 旧 .fb-share/.file-row 已不存在
  const btn = e.target && e.target.closest && e.target.closest(".tx-share, .tx-fname2");
  if (!btn) return;
  const row = btn.closest(".tx-frow");
  const name = row && row.getAttribute("data-file");
  if (name) prefetchShareTicket(name);
}
/* 当前页票据批量预取: 触屏没有 hover, 冷路径 await 会丢点击激活; 面板/列表加载时
   预先开好前 8 张票, 分享点击才能同步复制 */
function prefetchPageTickets() {
  if (!(accessPin.value || pinCode.value || pinEnabled.value)) return;
  for (const f of pageFiles.value.slice(0, 8)) prefetchShareTicket(f.name);
}

/* 浏览器旧页面/图片预览仍使用 PIN；壳内走回环豁免。 */

/* ---------- 下载(浏览器端): 手机小文件 fetch 回来再触发保存(有"下载完成"时机);
   桌面浏览器一律 HEAD 预检 + 原生流式下载, 见 downloadFileWeb 内注释 ---------- */
/* 浏览器端原生下载: 锚点必须挂进 DOM 再 click —— iOS Safari/部分国产浏览器(夸克等)
   对"游离锚点"的 click 静默忽略; 也不用 location.href(会把当前页面导航走) */
function nativeDownload(url, filename) {
  if (!url || url === "null") return false;
  const a = document.createElement("a");
  a.href = url;
  if (filename) a.download = filename;
  a.rel = "noopener";
  document.body.appendChild(a);
  a.click();
  a.remove();
  return true;
}
/* 下载优先用一次性 ticket, 不让长期配对码出现在浏览器历史/系统浏览器。
   开票失败必须返回 null 让调用方提示重试 —— 回退到带 ?pin= 的 URL 会把全站
   凭据写进系统浏览器历史和下载记录, 与分享路径的安全承诺矛盾。 */
async function authedDownloadUrl(name) {
  if (!(accessPin.value || pinCode.value || pinEnabled.value)) return downloadUrl(name);
  const t = await createShareTicket(name);
  return t
    ? `${apiBase.value}${API.download(name)}?ticket=${encodeURIComponent(t)}`
    : null;
}
/* 浏览器预览: inline=1 让服务端用 Content-Disposition: inline; 开访问码时同样走
   一次性 ticket, 不把长期 PIN 写进浏览器历史。开票失败返回 null, 不回退裸 PIN。 */
async function authedPreviewUrl(name) {
  const base = `${apiBase.value}${API.download(name)}?inline=1`;
  if (!pinProtected()) return base;
  const media = await mediaUrl(name, true);
  if (media) return media;
  // 兼容旧服务端/媒体会话失败: 回退一次性票据; 仍不回退裸 PIN
  const t = await createShareTicket(name);
  return t ? `${base}&ticket=${encodeURIComponent(t)}` : null;
}
/* ---------- 应用内预览 ----------
   旧实现 window.open 新标签: 浏览器原生查看器没有"关闭回到聊天"的入口, 手机
   Safari 还会离开当前会话。改为应用内全屏层, 右上角「关闭」直接回到聊天页。
   预览地址统一用短时媒体会话令牌（10min、IP 绑定、可重复 Range）:
   视频/音频分段不会因首段 206 烧票而 401; 长期 PIN 不进入 <img>/<iframe> URL。 */
const previewName = ref("");
const previewKind = ref("");
const previewUrl = ref("");
const previewText = ref("");
const previewErr = ref("");
const previewLoading = ref(false);
let previewSeq = 0;              // 预览代际: 迟到的旧文件文本响应不得覆盖新预览
let previewAbort = null;
let previewRetryAt = 0;
function previewKindOf(n) {
  const e = extFor(n);
  if (["jpg","jpeg","png","gif","webp","bmp","avif"].includes(e)) return "image";
  if (["mp4","webm","mov"].includes(e)) return "video";
  if (["mp3","m4a","ogg","wav"].includes(e)) return "audio";
  if (e === "pdf") return "pdf";
  return "text";
}
function closePreview() {
  previewSeq++;
  previewAbort?.abort();
  previewAbort = null;
  previewName.value = "";
  previewUrl.value = "";
  previewText.value = "";
  previewErr.value = "";
  previewLoading.value = false;
  previewRetryAt = 0;
}
async function previewFileWeb(name) {
  if (serviceDown()) { showToast("服务已停止，无法预览", "error"); return; }
  markFileViewed(name);
  const kind = previewKindOf(name);
  // 移动壳(Android/iOS WebView)没有 PDF 渲染器: 交系统浏览器; 桌面壳的
  // WKWebView/WebView2 有内建 PDF 查看器, 统一走应用内 iframe。
  if (mobileShell && kind === "pdf") {
    const url = await authedPreviewUrl(name);
    if (!url) {
      showToast("生成预览链接失败，请下载后查看", "error");
      return;
    }
    tauriInvoke("open_url_cmd", { url })
      .then((d) => { if (!d || !d.ok) copyText(url, "已复制一次性预览链接，请在浏览器中打开"); })
      .catch(() => copyText(url, "已复制一次性预览链接，请在浏览器中打开"));
    return;
  }
  const meta = allFiles.value.find(f => f.name === name);
  // 文本整段读进内存展示: 先看列表大小; 列表里没有(刚上传/列表截断)就 HEAD 预检,
  // 不能因为查不到 meta 就把任意大的日志整段拉进手机内存。
  if (kind === "text" && meta && meta.size > 2 * 1024 * 1024) {
    showToast("文本超过 2MB，请下载后查看", "error");
    return;
  }
  const seq = ++previewSeq;
  previewAbort?.abort();
  previewAbort = null;
  // 预览/缩略图一律用媒体会话令牌, 不把长期 PIN 拼进 <img>/<video>/<iframe> URL。
  const url = await authedPreviewUrl(name);
  if (seq !== previewSeq) return;
  if (!url) {
    showToast("生成预览链接失败，请重试", "error");
    return;
  }
  if (kind === "text") {
    const hctl = new AbortController();
    const htimer = setTimeout(() => hctl.abort(), 10000);
    try {
      const h = await fetch(url, { method: "HEAD", signal: hctl.signal });
      if (seq !== previewSeq) return;
      if (!h.ok) { showToast(`预览失败：HTTP ${h.status}`, "error"); return; }
      const size = Number(h.headers.get("Content-Length") || 0);
      if (size > 2 * 1024 * 1024) {
        showToast("文本超过 2MB，请下载后查看", "error");
        return;
      }
    } catch (e) {
      if (seq === previewSeq) showToast("预览失败：请确认电脑端服务仍在运行", "error");
      return;
    } finally {
      clearTimeout(htimer);
    }
  }
  if (seq !== previewSeq) return;
  document.activeElement?.blur?.();   // 先收软键盘, 否则 100dvh 层会被顶掉一截
  previewName.value = name;
  previewKind.value = kind;
  previewUrl.value = url;
  previewText.value = "";
  previewErr.value = "";
  if (kind !== "text") return;
  previewLoading.value = true;
  const ctl = new AbortController();
  previewAbort = ctl;
  const timer = setTimeout(() => ctl.abort(), 15000);
  try {
    const r = await fetch(previewUrl.value, { signal: ctl.signal });
    if (seq !== previewSeq) return;   // 期间已切到别的文件/关闭预览
    if (!r.ok) previewErr.value = `加载失败 (HTTP ${r.status})`;
    else previewText.value = await r.text();
  } catch (e) {
    if (seq === previewSeq) previewErr.value = "加载失败：请确认电脑端服务仍在运行";
  } finally {
    clearTimeout(timer);
    if (seq === previewSeq) previewLoading.value = false;
  }
}
async function retryPreview() {
  if (!previewName.value) return;
  const now = Date.now();
  if (now - previewRetryAt < 2500) return;
  previewRetryAt = now;
  const seq = previewSeq;
  const name = previewName.value;
  const url = await authedPreviewUrl(name);
  // 期间切到别的文件/关闭预览: 迟到的旧地址不得写进新预览
  if (seq !== previewSeq || name !== previewName.value) return;
  if (url) { previewUrl.value = url; previewErr.value = ""; }
  else previewErr.value = "加载失败";
}
async function downloadFileWeb(name) {
  const url = await authedDownloadUrl(name);
  if (!url) { showToast("下载凭证生成失败，请重试", "error"); return; }
  const base = name.split("/").pop() || name;   // download 属性不带子目录路径(部分移动端拒收)
  // 桌面浏览器: HEAD 预检后直接交浏览器原生流式下载。
  // 旧逻辑 ≤1GB 会把整个文件 fetch 进内存(blob)才触发保存 —— 500MB 在 Wi-Fi 上要
  // 等几十秒且期间零进度零提示, 用户感知就是"点了没反应"; 等 blob 期间浏览器瞬态
  // 用户激活早已过期, 程序化 click 可能被静默拦截。现在文件本体由浏览器下载管理器
  // 流式落盘(自带进度条/断点续传, 零内存占用), fetch 只拿状态码 —— 响应头毫秒级
  // 到达, 激活仍有效; 404/401 如实报错而不是把 JSON 错误体当文件下载。
  // 桌面/移动壳(isDesktop)同样走原生下载: WebView 的锚点下载由 on_download
  // 处理器接管落盘(壳内没有浏览器下载管理器, blob 路径在壳内不可靠)
  if (mobileShell) {
    // 移动壳: WebView 没有 DownloadListener/on_download, 锚点下载不会落任何文件 ——
    // 交给系统浏览器下载。url 已优先使用一次性 ticket, 不把长期配对码写进
    // 系统浏览器历史/外部 App。
    tauriInvoke("open_url_cmd", { url })
      .then((d) => {
        if (d && d.ok) showToast("已在系统浏览器打开下载");
        else copyText(url, "已复制下载链接，请在浏览器中打开");
      })
      .catch(() => copyText(url, "已复制下载链接，请在浏览器中打开"));
    return;
  }
  if (isRealDesktop || (!isIOSFamily && !/Android/.test(navigator.userAgent))) {
    try {
      // 预检必须带超时: 服务失联时 fetch 会无限挂起, 表现恰恰是"点了没反应"
      const ctl = new AbortController();
      const timer = setTimeout(() => ctl.abort(), 8000);
      const r = await fetch(url, { method: "HEAD", signal: ctl.signal });
      clearTimeout(timer);
      if (!r.ok) {
        showToast(`下载失败：HTTP ${r.status}`, "error");
        return;
      }
    } catch (e) {
      showToast("下载失败：连接超时，请确认电脑端服务已启动", "error");
      return;
    }
    nativeDownload(url, base);
    showToast(isDesktop ? "已开始下载，完成后提示保存位置" : "已开始下载，进度见浏览器下载列表");
    return;
  }
  // 手机浏览器: 预检必须用 HEAD —— 一次性 ticket 只在真正的 GET 时消费。
  // 旧实现先 GET 预检, 服务端立刻销票; 大文件分支再拿同一张票原生下载必然 401。
  let len = 0;
  try {
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), 10000);
    const h = await fetch(url, { method: "HEAD", signal: ctl.signal });
    clearTimeout(timer);
    if (!h.ok) {
      showToast(`下载失败：HTTP ${h.status}`, "error");
      return;
    }
    len = Number(h.headers.get("Content-Length") || 0);
  } catch (e) {
    showToast("下载失败：连接超时，请确认电脑端服务已启动", "error");
    return;
  }
  // 手机浏览器/WebView 的 blob 内存配额远低于桌面(iOS Safari 数百 MB 即杀标签页且无报错),
  // 阈值 50MB, 超出一律走原生流式下载(这一次 GET 才消费 ticket)
  if (len > 50 * 1024 * 1024) {
    nativeDownload(url, base);
    showToast("大文件较大，已交由浏览器直接下载");
    return;
  }
  try {
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), 120000);   // 半开 Wi-Fi 下不得永久挂起
    try {
      const r = await fetch(url, { signal: ctl.signal });   // 唯一一次 GET, 消费 ticket
      if (!r.ok) {
        showToast(`下载失败：HTTP ${r.status}`, "error");
        return;
      }
      const blob = await r.blob();
      const objUrl = URL.createObjectURL(blob);
      if (!nativeDownload(objUrl, base)) {
        showToast("下载失败，请重试", "error");
        return;
      }
      setTimeout(() => URL.revokeObjectURL(objUrl), BLOB_REVOKE_MS);
      showToast(`已下载：${name}${len ? " (" + fmtSize(len) + ")" : ""}`);
    } finally {
      clearTimeout(timer);
    }
  } catch (e) {
    // blob 阶段失败(内存/内核限制/超时): 原 ticket 已被上面的 GET 消费, 必须重新开票再交给浏览器
    const retryUrl = await authedDownloadUrl(name);
    if (!nativeDownload(retryUrl, base)) {
      showToast("下载凭证生成失败，请重试", "error");
      return;
    }
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
  let ok = 0, fail = 0, gone = 0;
  const removed = new Set();
  for (const n of targets) {
    try {
      const r = await apiFetch(apiBase.value + API.del(n), { method: "DELETE" });
      // 404 = 文件已不存在(列表过期/他端已删/同名改名的旧名): 视为已处理而非失败
      if (r.status === 404) { gone++; removed.add(n); continue; }
      if (r.ok) { ok++; removed.add(n); } else fail++;
    } catch (e) { fail++; }
  }
  // 删除后聊天里的文件卡片同步消失: remote 卡片由 loadFiles 过滤, 本机已上传
  // 卡片是本地队列, 不主动清理会一直停在时间线里(用户以为没删掉)
  if (removed.size) {
    uploads.value = uploads.value.filter(u => !removed.has(uploadName(u)));
    remoteFiles.value = remoteFiles.value.filter(c => !removed.has(c.name));
  }
  if (fail) showToast(`删除完成：成功 ${ok} 个，失败 ${fail} 个`, "error");
  else if (gone) showToast(targets.length > 1 ? `已清理 ${gone} 个失效条目，列表已刷新` : "文件已不存在，列表已刷新");
  else showToast(targets.length > 1 ? `已删除 ${ok} 个文件` : "已删除 「" + targets[0] + "」");
  selected.value = new Set();
  loadFiles(true);   // 手动刷新: 带旋转反馈; 与轮询在途竞争时由补拉机制保证不丢
}

/* ---------- 上传(单请求整文件直传 + 整目录) ---------- */
const uploads = ref([]);
let upSeq = 0;
let lastSelfUploadAt = 0;   // 自己上传完成的 files:changed 不点亮“新文件”绿点
/* 其他设备上传的文件: 作为"文件消息"补进时间线。
   本机上传走 uploads 卡片; 远端文件只能从服务端列表里"新出现"来识别 ——
   首次拉取只建立基线, 避免历史文件在打开页面时全部刷成新消息。 */
const remoteFiles = ref([]);
/* 服务端持久化的文件投递事件(独立 200 条上限): 刷新/重启后时间线仍能看到
   发送过的文件; 与 remoteFiles(临时新文件卡)和 uploads(本机在途卡)按文件名去重。 */
const fileEvents = ref([]);
let knownFileNames = null;   // 上一次文件列表快照; null = 尚未建立基线
const REMOTE_CARDS_MAX = 100;
let openBatch = null;       // 60s 内连续上传归入同一文件组
let openBatchAt = 0;
let openBatchSize = 0;
/* 上传并发闸: 同时最多 UPLOAD_CONCURRENCY 个文件在传, 其余真正停在"等待中"。
   WebKit 每 host 仅 6 条 HTTP/1.1 连接, WS/轮询/下载要留预算 —— 一次选数百文件
   全并发时会把所有 XHR 压上 6 条连接(整文件版无看门狗, 不会像旧版那样集体误报
   "上传中断", 但手机端明显变慢且失败无掩护)。整文件版一个文件=一条连接,
   3 并发用掉一半预算; 纯内存排队, 完成即出队, 无任何持久状态 */
const UPLOAD_CONCURRENCY = 3;
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
let pickLocalOnly = false;   // 文件页/共享文件抽屉的上传只进本机共享, 不沿用直发目标
function pickFiles(localOnly = false) {
  pickLocalOnly = !!localOnly;
  document.getElementById("fileInput").click();
}
// 移除上传条目(失败行不自动清理, 否则只能刷新页面才能清掉)
function removeUpload(id) { uploads.value = uploads.value.filter(u => u.id !== id); }
function pickDir() {
  // iOS Safari/WebView 不支持 webkitdirectory: 按钮保留可见(与其它端一致), 点击给出明确说明
  if (isIOS) { showToast("iOS 浏览器不支持选择文件夹，请用「文件」多选", "error"); return; }
  document.getElementById("dirInput").click();
}
function handleIncomingFiles(list) {
  // 服务端 safe_rel_path 最多 8 段(相对路径), 更深的选择在此提前过滤并明确告知,
  // 避免"部分文件静默没传"。
  let tooDeep = 0;
  const arr = Array.from(list || []).filter((f) => {
    const rel = (f && (f.webkitRelativePath || f.name)) || "";
    if (rel && rel.split("/").length > 8) { tooDeep++; return false; }
    return true;
  });
  if (tooDeep) showToast(`已跳过 ${tooDeep} 个超出 8 层目录限制的文件`, "error");
  if (!arr.length) return;
  if (sendTargets.value.length) {
    // 多选: 依次发往每台设备; manual=true 让每批不被"目标切换"中断
    const targets = sendTargets.value.slice();
    (async () => {
      for (const p of targets) {
        const r = await directSendFiles(p, arr, { manual: true });
        if (!r.allOk) break;   // 失败中断, 避免连续弹出多个配对框/长时间挂起
      }
    })();
    return;
  }
  handleFiles(arr);
}
function onPick(e) {
  const files = Array.from(e.target.files || []);
  const localOnly = pickLocalOnly;
  pickLocalOnly = false;
  e.target.value = "";
  if (files.length) {
    if (localOnly) handleFiles(files); else handleIncomingFiles(files);
    return;
  }
  // 长按/快捷键粘贴文本: 回传共享剪贴板, 电脑端自动写入系统剪贴板
  const text = e.clipboardData && e.clipboardData.getData ? e.clipboardData.getData("text/plain") : "";
  if (text && text.trim()) publishPastedText(text, e.target);
}
function onPickDir(e) {
  // 选空文件夹与拖入空文件夹一致给反馈(原静默 return 像软件坏了)
  if (!e.target.files || !e.target.files.length) { showToast("文件夹为空或不含可上传文件", "error"); e.target.value = ""; return; }
  const files = Array.from(e.target.files);
  e.target.value = "";
  if (filesPageActive()) handleFiles(files); else handleIncomingFiles(files);
}
function handleFiles(list) {
  if (!list || !list.length) return;
  if (serviceDown()) {
    // 服务未启动时不要把整批文件入队逐个失败: 一个提示, 不产生 N 个 toast/请求
    showToast(running.value ? "服务正在重连，请稍后重试" : "服务未启动，无法上传", "error");
    return;
  }
  const arr = Array.from(list);
  const now = Date.now();
  // 同一批选择 / 拖入 / 粘贴共享一个 batch；60s 内的连续上传继续归入同一批。
  // 总数 >3 时时间线合并成一张文件组卡，避免连续小批上传刷屏。
  let batch, batchSize;
  if (openBatch && now - openBatchAt < 60000) {
    batch = openBatch;
    openBatchSize += arr.length;
    batchSize = openBatchSize;
    for (const u of uploads.value) if (u.batch === batch) u.batchSize = batchSize;
  } else {
    batch = "b" + now + "-" + (upSeq++);
    openBatch = batch;
    openBatchSize = arr.length;
    batchSize = openBatchSize;
  }
  openBatchAt = now;
  const t0 = now / 1000;
  for (const f of arr) uploadFile(f, { batch, batchSize, t0 });
}
/* 全局粘贴上传(Ctrl+V/Cmd+V): mounted 挂载监听 —— 此函数曾在上传重构的
   块删除中被越界移除, 导致挂载钩子 ReferenceError 整页黑屏, 已恢复 */
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
  if (files.length) {
    if (filesPageActive()) handleFiles(files); else handleIncomingFiles(files);
    return;
  }
  // 长按/快捷键粘贴文本: 回传共享剪贴板, 电脑端自动写入系统剪贴板
  const text = e.clipboardData && e.clipboardData.getData ? e.clipboardData.getData("text/plain") : "";
  if (text && text.trim()) publishPastedText(text, e.target);
}
/* 拖拽收集: 支持文件夹(webkitGetAsEntry 递归)与混合拖入, 目录结构经 webkitRelativePath 保留 */
let collectSkipped = 0;   // 目录过深/条目读取失败被丢弃的数量: 收完必须提示用户
async function collectDroppedFiles(dt) {
  const out = [];
  const entries = [];
  collectSkipped = 0;
  if (dt && dt.items) {
    for (const it of dt.items) {
      const en = it.webkitGetAsEntry && it.webkitGetAsEntry();
      if (en) entries.push(en);
      else { const f = it.getAsFile && it.getAsFile(); if (f) out.push(f); }
    }
  }
  if (!entries.length) return dt && dt.files ? Array.from(dt.files) : out;
  // 深度上限=8(对齐服务端 UPLOAD_MAX_DEPTH): 超深树收集了也会被服务端逐个 400;
  // 同时是符号链接环的兜底 —— 环会让递归永不返回, collecting 卡死后所有拖入被静默丢弃
  const walk = async (entry, prefix, depth) => {
    // 深度上限与服务端 UPLOAD_MAX_DEPTH=8 对齐: 文件路径段数 = depth+1,
    // depth>7 的文件(≥9段)服务端 safe_rel_path 必拒 —— 提前丢弃, 不进队列也不误报
    if (depth > 7) { collectSkipped++; return; }
    if (entry.isFile) {
      const f = await new Promise((res) => entry.file(res, () => res(null)));
      if (f) {
        try { Object.defineProperty(f, "webkitRelativePath", { value: prefix + entry.name }); } catch (_) {}
        out.push(f);
      } else collectSkipped++;
    } else if (entry.isDirectory) {
      const rd = entry.createReader();
      for (;;) {
        const batch = await new Promise((res) => rd.readEntries(res, () => res([])));
        if (!batch.length) break;
        for (const en of batch) await walk(en, prefix + entry.name + "/", depth + 1);
      }
    }
  };
  for (const en of entries) await walk(en, "", 0);
  return out;
}
const collecting = ref(false);   // 递归收集拖入条目期间置位: 超大目录树收集耗时秒级, 得让用户知道在干活
async function onPageDragLeave(e) {
  // 子元素之间移动也会冒泡 dragleave: 只有真正离开页面才收起投放层
  if (!e.currentTarget || !e.relatedTarget || !e.currentTarget.contains(e.relatedTarget)) {
    dragOver.value = false;
  }
}
async function onDrop(e) {
  dragOver.value = false;
  if (collecting.value) return;
  collecting.value = true;
  try {
    const list = await collectDroppedFiles(e.dataTransfer);
    if (collectSkipped) {
      showToast(`已跳过 ${collectSkipped} 个文件（目录超过 8 层或无法读取）`, "error");
    }
    // 空文件夹拖入零反馈会让用户反复拖、以为软件坏了
    if (!list || !list.length) {
      if (!collectSkipped) showToast("文件夹为空或不含可上传文件", "error");
      return;
    }
    if (filesPageActive()) handleFiles(list); else handleIncomingFiles(list);
  } finally { collecting.value = false; }
}
/* 上传名: 文件夹拖入/选择时为相对路径(如 目录/子目录/文件名), 普通文件为原名 */
function uploadRelName(file) { return file.webkitRelativePath || file.name; }
/* 手机 App 的文件页: 上传应加入本机共享, 不能因为之前选过直发目标就发给别人 */
function filesPageActive() { return fileSheetOpen.value; }

/* 唤醒锁: 有上传进行中时申请屏幕常亮 —— iOS 锁屏/切后台会冻结上传连接(失败主因) */
let wakeLock = null;
/* iOS 屏幕保活(视频通路): Wake Lock API 在 HTTP 非安全上下文不存在(手机浏览器恰好是),
   改用 2px 静音内联视频循环播放 —— 媒体播放通路不受 HTTP 限制, 可阻止 iOS 锁屏熄屏 */
let keepAliveVideo = null, keepAliveStream = null;
function setScreenKeepAlive(on) {
  if (on) {
    if (!keepAliveVideo) {
      // **先在局部变量上构造, 全部成功才落全局**: captureStream 在微信 WebView/
      // 部分 iOS 上会抛错, 半初始化对象一旦占了全局槽, 后续 `!keepAliveVideo`
      // 永假 → 整个会话保活静默失效
      try {
        const v = document.createElement("video");
        v.muted = true;
        v.setAttribute("playsinline", "");
        v.loop = true;
        const c = document.createElement("canvas");
        c.width = c.height = 2;
        c.getContext("2d").fillRect(0, 0, 2, 2);
        const s = c.captureStream(2);
        v.srcObject = s;
        v.style.cssText = "position:fixed;left:-20px;top:-20px;width:2px;height:2px;opacity:0.01;pointer-events:none";
        document.body.appendChild(v);
        keepAliveVideo = v;
        keepAliveStream = s;
      } catch (e) {
        keepAliveVideo = null;
        keepAliveStream = null;
        return;   // 创建失败: 保持 null, 下次调用重试
      }
    }
    try { keepAliveVideo.play().catch(() => {}); } catch (e) {}
  } else if (keepAliveVideo) {
    try { keepAliveVideo.pause(); } catch (e) {}
  }
}
let wakeLockPending = null;
async function updateWakeLock() {
  const want = uploads.value.some(u => u.status === "uploading" || u.status === "waiting");
  setScreenKeepAlive(want);
  try {
    if (!want) {
      // 在途请求先等它落地再释放: 否则 request 迟到把 wakeLock 重新点亮
      if (wakeLockPending) {
        const s = await wakeLockPending.catch(() => null);
        try { if (s) await s.release(); } catch (e) {}
        wakeLockPending = null;
        wakeLock = null;
        return;
      }
      if (wakeLock) {
        try { await wakeLock.release(); } catch (e) {}
        wakeLock = null;
      }
      return;
    }
    // 同一时刻只允许一个 request: 多文件并发启动时不会创建多个 sentinel
    if (wakeLock || wakeLockPending || !navigator.wakeLock) return;
    wakeLockPending = navigator.wakeLock
      .request("screen")
      .then((s) => {
        wakeLock = s;
        s.addEventListener("release", () => { if (wakeLock === s) wakeLock = null; });
        return s;
      })
      .catch(() => null)
      .finally(() => { wakeLockPending = null; });
  } catch (e) {}
}
/* 时间线卡片上限: 成功/失败/取消都走同一裁剪, 否则"一次选几百个文件且全部
   失败"会一直保留 File 句柄与 XHR, 移动端 WebView 内存持续增长。 */
function trimUploads() {
  if (uploads.value.length <= 200) return;
  const live = uploads.value.filter(u => u.status === "uploading" || u.status === "waiting");
  const history = uploads.value.filter(u => u.status !== "uploading" && u.status !== "waiting");
  const keep = Math.max(0, 200 - live.length);
  const kept = keep > 0 ? history.slice(-keep) : [];
  history.slice(0, Math.max(0, history.length - keep))
    .forEach(u => { u.file = null; u.xhr = null; });
  uploads.value = [...live, ...kept];
}
function uploadFile(file, meta = {}) {
  // 单请求整文件直传(1.1.2 传输) + 保留功能: 同名自动改名、取消✕、锁屏防中断
  const id = "up-" + Date.now() + "-" + upSeq++;
  const name = uploadRelName(file);   // 文件夹入口保留: 相对路径经 X-Filename 下传
  uploads.value.push({
    id, name, size: file.size || 0, pct: 0, speed: "", status: "waiting",
    t0: meta.t0 || Date.now() / 1000,
    batch: meta.batch || ("b" + id),
    batchSize: meta.batchSize || 1,
    file,   // 失败重试复用原 File 对象
  });   // waiting = 真实排队
  const stat = () => uploads.value.find(u => u.id === id);
  upQueue.push(() => startUpload(file, id, name, stat));
  pumpUploadQueue();
}

/* 链体(可独立测试): 返回 Promise, 槽位在传输结束(成功/失败/取消)后经 resolve 释放 ——
   xhr.send() 立即返回, 绝不能在 send 时就放槽, 否则并发闸形同虚设 */
function startUpload(file, id, name, stat) {
  return new Promise((resolve) => {
    if (!stat()) return resolve();   // 排队期间条目被用户移除: 直接让出槽位
    try {
      const xhr = new XMLHttpRequest();
      // 移动壳/浏览器端 XHR 不能带自定义鉴权头跨源时会触发预检, 但同源(网页端)与
      // 桌面壳回环均允许; 令牌走请求头, 不再拼进 URL
      xhr.open("POST", apiBase.value + API.upload, true);
      const sentPin = accessPin.value;   // 本次请求实际携带的码: 迟到 401 不能清掉新码
      if (accessPin.value) { try { xhr.setRequestHeader("X-Access-Pin", accessPin.value); } catch (e) {} }
      xhr.setRequestHeader("X-Filename", encodeURIComponent(name));
      xhr.setRequestHeader("X-File-Size", String(file.size));
      // 上传者昵称: 其他端的时间线文件卡片据此显示头像(服务端内存态, 重启后回落通用头像)
      xhr.setRequestHeader("X-Nickname", encodeURIComponent(deviceNick()));
      xhr.setRequestHeader("Content-Type", "application/octet-stream");
      const t0 = Date.now();
      let lastTick = Date.now();
      // 无进展看门狗: 服务端 120s 空闲超时之前, 客户端先放弃(125s)。半开连接会让
      // XHR 既不 error 也不 load —— 没有它, 3 个悬挂连接会永久占满并发闸, 队列全部堵死
      let stallTimer = null;
      let finished = false;
      const finish = () => {
        if (finished) return;
        finished = true;
        if (stallTimer) clearInterval(stallTimer);
        resolve();
      };
      stallTimer = setInterval(() => {
        if (Date.now() - lastTick <= 125000) return;
        const it = stat();
        if (it) { it.status = "error"; showToast("上传超时：" + name, "error"); }
        try { xhr.abort(); } catch (e) {}
        finish();
        updateWakeLock();   // abort 走 onabort 不会经过 onload/onerror 的释放
      }, 5000);
      xhr.upload.onprogress = (e) => {
        lastTick = Date.now();
        const it = stat(); if (!it) return;
        it.status = "uploading";
        if (e.lengthComputable) {
          it.pct = Math.round((e.loaded / e.total) * 100);
          const sp = e.loaded / Math.max((Date.now() - t0) / 1000, 0.1);
          it.speed = fmtSize(sp) + "/s";
        }
      };
      xhr.onload = () => {
        const it = stat(); if (!it) return finish();
        if (xhr.status === 200) {
          it.pct = 100; it.status = "done"; it.doneAt = Date.now();
          it.file = null; it.xhr = null;   // 完成卡片不再需要 Blob/句柄, 及时释放内存
          // 同名自动改名: 服务端把实际落盘名放 final 字段, 变了才提示"已存为"
          let d = {};
          try { d = JSON.parse(xhr.responseText); } catch (_) {}
          const base = name.split("/").pop();
          const finalRel = typeof d.final === "string" ? d.final : "";
          const finalBase = finalRel.split("/").pop();
          if (finalRel && finalBase && finalBase !== base) {
            // 保存**完整相对路径**: 文件夹上传触发改名时, 只存 basename 会让卡片
            // 的打开/分享/删除指向共享根目录下的另一个同名文件
            it.final = finalRel;
            // 服务端先广播再回响应: 弱网下可能已把改名后的文件当成"远端新增"加了卡片,
            // 响应落地后按 final 名移除, 避免时间线出现本机+远端重复卡
            remoteFiles.value = remoteFiles.value.filter((c) => c.name !== finalRel);
            showToast("目录里已有同名文件，已改存为 「" + finalBase + "」");
          }
          updateWakeLock();
          lastSelfUploadAt = Date.now();
          // 发送文件后立即定位到最新: 卡片先出现在时间线, 再补两次滚动覆盖渲染/布局变化
          atBottom.value = true;
          jumpCount.value = 0;
          nextTick(scrollChats);
          setTimeout(scrollChats, 150);
          loadFiles();
          // 完成卡片留在时间线里（这是“文件即消息”的核心）；只裁剪过长的历史。
          trimUploads();
        } else {
          // 配对码是独立保留功能: 401/429 唤起弹框并清掉旧码(否则队列里每个文件
          // 都会继续带错码计数, 把本机 IP 越锁越死); 失败文案与 1.1.2 逐字一致。
          // 例外: 服务端上传并发闸门也返回 429, 不能把合法 PIN 当成错码清掉。
          const busy = xhr.status === 429 && /并发/.test(xhr.responseText || "");
          // 与 apiFetch 的代际保护一致: 配对成功后, 旧的在途请求迟到 401
          // 不能把刚保存的新码清掉(否则立刻又弹配对框并自锁)
          if ((xhr.status === 401 || (xhr.status === 429 && !busy)) && accessPin.value === sentPin) {
            dropAccessPin();
            needPin.value = true;
          }
          it.status = "error";
          updateWakeLock();
          showToast(busy ? "上传并发过多，请稍后重试" : "上传失败: " + name, "error");
        }
        trimUploads();
        finish();
      };
      xhr.onerror = () => {
        const it = stat(); if (!it) return finish();
        it.status = "error";
        updateWakeLock();
        showToast("上传失败: " + name, "error");
        trimUploads();
        finish();
      };
      xhr.onabort = finish;   // 取消✕ 走 abort → onabort(不触发 onerror/onload), 也要放槽
      const row = stat();
      row.status = "uploading";
      row.xhr = xhr;          // 取消✕ 的中止句柄
      updateWakeLock();       // 上传开始 → 申请屏幕常亮/保活
      xhr.send(file);
    } catch (e) {
      console.error("上传异常:", e);
      const it = stat(); if (it) it.status = "error";
      updateWakeLock();
      showToast("上传出错: " + (e && e.message ? e.message : "未知错误"), "error");
      resolve();
    }
  });
}
/* 取消上传: 中止在途 XHR —— 服务端连接断开走"上传中断,已清理半截文件"路径,
   无需额外请求; 行立即移除 */
function cancelUpload(u) {
  if (u.xhr) { try { u.xhr.abort(); } catch (e) {} }
  removeUpload(u.id);
  updateWakeLock();
}
/* ---------- 时间线：消息 + 文件卡片 + 文件组 ---------- */
function uploadName(u) { return u.final || u.name; }
function uploadDisplayName(u) { return uploadName(u).split("/").pop(); }
/* 当前可见的图片文件: 页面列表 + 时间线远端卡 + 本机上传完成卡 */
const thumbNames = computed(() => {
  const names = new Set();
  for (const f of pageFiles.value) if (isImage(f.name)) names.add(f.name);
  for (const r of remoteFiles.value) if (isImage(r.name)) names.add(r.name);
  for (const u of uploads.value) {
    if (u.status === "done" && isImage(uploadName(u))) names.add(uploadName(u));
  }
  return [...names];
});
watch(thumbNames, (names) => {
  // 裁掉已不在当前列表里的旧 URL, 避免长期运行后 map 无界增长
  const keep = new Set(names);
  let pruned = null;
  for (const k of Object.keys(thumbSrcs.value)) {
    if (!keep.has(k)) { pruned = pruned || { ...thumbSrcs.value }; delete pruned[k]; }
  }
  if (pruned) thumbSrcs.value = pruned;
  for (const n of names) loadThumb(n);
}, { immediate: true });
watch([pinEnabled, accessPin, pinCode], () => {
  // 保护开关/码值变化: 旧的无 PIN / 旧票 / 旧媒体令牌一律作废
  thumbSrcs.value = {};
  clearMediaToken();
  nextTick(() => { for (const n of thumbNames.value) loadThumb(n); });
});
watch([serverAddr, port], () => {
  // 本机服务地址/端口就绪或变化: 旧相对 URL 与媒体会话都作废, 重建缩略图
  thumbSrcs.value = {};
  clearMediaToken();
  nextTick(() => { for (const n of thumbNames.value) loadThumb(n); });
});
function uploadStatusText(u) {
  if (u.status === "waiting") return "等待中";
  if (u.status === "uploading") return `${u.pct}%${u.speed ? " · " + u.speed : ""}`;
  if (u.status === "done") return u.final ? `已存为 ${uploadDisplayName(u)}` : "已上传";
  return "上传失败";
}
function fileTone(name) {
  const e = extFor(name);
  if (e === "pdf") return "pdf";
  if (IMG_EXTS.includes(e)) return "img";
  if (["zip", "rar", "7z", "tar", "gz"].includes(e)) return "zip";
  if (["mp4", "mov", "avi", "mkv", "webm"].includes(e)) return "vid";
  if (["doc", "docx"].includes(e)) return "doc";
  if (["xls", "xlsx", "csv"].includes(e)) return "xls";
  if (["ppt", "pptx"].includes(e)) return "ppt";
  if (["md", "txt"].includes(e)) return "txt";
  if (["jks", "keystore", "pem", "key"].includes(e)) return "key";
  return "file";
}
function retryUpload(u) {
  const f = u.file;
  if (!f) return;
  const meta = { batch: u.batch, batchSize: u.batchSize, t0: Date.now() / 1000 };
  removeUpload(u.id);
  uploadFile(f, meta);
}
function openUpload(u) {
  const n = uploadName(u);
  if (!n) return;
  markFileViewed(n);
  if (isRealDesktop) openFile(n);
  else downloadFileWeb(n);
}
function openRemote(r) {
  if (!r || !r.name) return;
  markFileViewed(r.name);
  if (isRealDesktop) openFile(r.name);
  else downloadFileWeb(r.name);
}
/* 缩略图点击: 桌面交给系统程序打开; 浏览器端能内联预览就预览, 不能则下载。
   400ms 内同名连点只响应一次 —— 双击缩略图会连发 click, 否则会开两次预览/下载 */
let thumbLast = { name: "", t: 0 };
function openThumb(name) {
  if (!name) return;
  markFileViewed(name);
  const now = Date.now();
  if (thumbLast.name === name && now - thumbLast.t < 400) return;
  thumbLast.name = name;
  thumbLast.t = now;
  if (isRealDesktop) openFile(name);
  else if (isPreviewable(name)) previewFileWeb(name);
  else downloadFileWeb(name);
}
function shareUpload(u) {
  const n = uploadName(u);
  if (n) shareFile(n);
}
function locateFile(name) {
  if (!name) return;
  const idx = sortedFiles.value.findIndex(f => f.name === name);
  if (idx >= 0) currentPage.value = Math.floor(idx / PAGE_SIZE) + 1;
  openFilePanel();
  locatedName.value = name;
  nextTick(() => {
    const el = [...document.querySelectorAll("[data-file]")].find(e => e.dataset.file === name);
    if (el) el.scrollIntoView({ block: "nearest" });
  });
  setTimeout(() => { if (locatedName.value === name) locatedName.value = ""; }, 2600);
}
function locateUpload(u) { locateFile(uploadName(u)); }
function openFileMenu(name) { blurMobileKeyboard(); fileMenuName.value = name; }
function isMine(m) { return (m.nickname || "") === curNick.value; }
const uploadCount = computed(() => uploads.value.filter(u => u.status === "waiting" || u.status === "uploading").length);
const uploadAvgPct = computed(() => {
  const active = uploads.value.filter(u => u.status === "waiting" || u.status === "uploading");
  if (!active.length) return 0;
  return Math.round(active.reduce((s, u) => s + (u.pct || 0), 0) / active.length);
});
function groupSize(items) { return (items || []).reduce((s, u) => s + (u.size || 0), 0); }
function groupDone(items) { return (items || []).filter(u => u.status === "done").length; }
function groupErr(items) { return (items || []).filter(u => u.status === "error").length; }
function groupStatusText(items) {
  const err = groupErr(items), done = groupDone(items);
  if (err) return `${err} 个失败`;
  if (done >= (items || []).length) return "已上传";
  return `上传中 ${done}/${(items || []).length}`;
}
/* 消息 HTML 按 id 缓存: 消息 id 单调且不复用, 内容不可编辑。没有缓存时
   每来一条新消息/每轮轮询都会把最近 200 条全部重新跑一遍 Markdown,
   iOS 上会明显卡顿(尤其滚动时同时触发布局)。 */
const chatHtmlCache = new Map();
function chatHtml(m) {
  // id + timestamp 双键: 服务端重启后即使 id 从头计数, 也不会命中上一代的旧 HTML
  const key = `${m.id}:${m.timestamp}`;
  let h = chatHtmlCache.get(key);
  if (h === undefined) {
    h = renderQuote(m.quote) + mdChat(m.message);
    if (chatHtmlCache.size > 400) chatHtmlCache.clear();
    chatHtmlCache.set(key, h);
  }
  return h;
}
const timelineItems = computed(() => {
  const items = [];
  for (const m of chatMsgs.value) {
    if (m.timestamp <= clearedTs.value) continue;
    items.push({ kind: "msg", key: "m" + m.id, ts: m.timestamp, msg: m, html: chatHtml(m) });
  }
  // 文件投递事件(持久化): 同名只保留最新一条; 本机在途/完成的上传卡已展示的跳过,
  // 避免同一文件出现"本机上传卡 + 远端文件卡"两张。
  const eventByName = new Map();
  for (const f of fileEvents.value) {
    if (f.timestamp <= clearedTs.value) continue;
    const cur = eventByName.get(f.name);
    if (!cur || (f.timestamp || 0) >= (cur.timestamp || 0)) eventByName.set(f.name, f);
  }
  const localUploadNames = new Set(uploads.value.map((u) => uploadName(u)).filter(Boolean));
  for (const f of eventByName.values()) {
    if (localUploadNames.has(f.name)) continue;
    items.push({
      kind: "remote", key: "fe-" + f.name, ts: f.timestamp || 0,
      r: { name: f.name, size: f.size, modified: f.timestamp, uploader: f.uploader || "", ts: f.timestamp || 0 },
    });
  }
  for (const r of remoteFiles.value) {
    if (eventByName.has(r.name)) continue;   // 已有持久化事件, 不再重复一张临时卡
    items.push({ kind: "remote", key: r.key, ts: r.ts, r });
  }
  const groups = new Map();
  for (const u of uploads.value) {
    if ((u.batchSize || 1) > 3) {
      let g = groups.get(u.batch);
      if (!g) {
        g = { kind: "group", key: "g" + u.batch, batch: u.batch, ts: u.t0 || 0, items: [] };
        groups.set(u.batch, g);
      }
      g.items.push(u);
      g.ts = Math.min(g.ts, u.t0 || 0);
    } else {
      items.push({ kind: "file", key: "u" + u.id, ts: u.t0 || 0, u });
    }
  }
  for (const g of groups.values()) {
    g.items.sort((a, b) => (a.t0 || 0) - (b.t0 || 0));
    items.push(g);
  }
  for (const r of directReceipts.value) {
    items.push({ kind: "receipt", key: "r" + r.id, ts: r.ts, r });
  }
  items.sort((a, b) => (a.ts || 0) - (b.ts || 0));
  return items;
});
/* 手机/iOS 只渲染最近 140 条: 聊天 200 + 远端卡片 100 + 上传卡片 200 叠起来
   可达 500 条复杂 DOM, Safari 滚动会明显掉帧。数据本身不裁剪, 桌面端仍是全量。 */
const MOBILE_TIMELINE_MAX = 140;
/* 移动端窗口起点(不是固定条数): 用户上翻时新消息只在底部增加, 顶部不裁切,
   可见内容不会因新消息到达而跳动; 回到底部后才重新收敛到最近 140 条。
   顶部提供「加载更早」按钮, 不静默丢历史。 */
const timelineStart = ref(0);
const visibleTimeline = computed(() => {
  const arr = timelineItems.value;
  if (!isNarrow.value) return arr;
  return arr.slice(timelineStart.value);
});
function loadEarlierTimeline() {
  const el = chatBox.value;
  const before = el ? el.scrollHeight : 0;
  timelineStart.value = Math.max(0, timelineStart.value - MOBILE_TIMELINE_MAX);
  nextTick(() => {
    // 顶部插入内容后保持用户当前可见位置不跳
    if (el) el.scrollTop += el.scrollHeight - before;
  });
}
/* 日期分隔: 按消息实际日期显示「月-日」, 不再用相对"今天" */
function fmtDay(ts) {
  const d = new Date(ts * 1000);
  const p = (n) => String(n).padStart(2, "0");
  const md = `${p(d.getMonth() + 1)}-${p(d.getDate())}`;
  return d.getFullYear() === new Date().getFullYear() ? md : `${d.getFullYear()}-${md}`;
}
function dayKey(ts) {
  const d = new Date(ts * 1000);
  return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
}
function showDayDivider(i) {
  const arr = visibleTimeline.value;
  return i === 0 || dayKey(arr[i].ts) !== dayKey(arr[i - 1].ts);
}
/* 滚动事件用 rAF 合并: iOS 上每次 scroll 都读 scrollHeight/写响应式状态会
   触发布局抖动, 一帧只处理一次即可。 */
let timelineScrollRaf = 0;
function onTimelineScroll(e) {
  const el = e.target;
  if (!el || timelineScrollRaf) return;
  timelineScrollRaf = requestAnimationFrame(() => {
    timelineScrollRaf = 0;
    const bottom = el.scrollHeight - el.scrollTop - el.clientHeight < 90;
    if (bottom !== atBottom.value) atBottom.value = bottom;
    // 滚回底部 = 已看到最新消息: 立即追平未读, 否则 WS 在线时角标会一直挂着
    if (bottom) { jumpCount.value = 0; markChatSeen(); }
  });
}
function jumpToLatest() {
  jumpCount.value = 0;
  atBottom.value = true;
  nextTick(() => { scrollChats(); markChatSeen(); });
}

/* ---------- 聊天 ---------- */
const chatMsgs = ref([]);
const chatInput = ref("");
const chatInputEl = ref(null);
// 与服务端一致的 Unicode 字符计数(不能直接用 .length: 浏览器按 UTF-16 码元算)
const chatCharCount = computed(() => Array.from(chatInput.value || "").length);
let chatOverWarned = false;
function onChatInput(e) {
  autoGrowInput();
  const n = Array.from((e && e.target && e.target.value) || "").length;
  const over = n > cfg.value.msgMax;
  if (over && !chatOverWarned) {
    chatOverWarned = true;
    showToast(`消息超过 ${cfg.value.msgMax} 字符上限，请删减后再发送`, "error");
  }
  if (!over) chatOverWarned = false;
}
// 多行输入: textarea 随内容长高(最高 160px 后内部滚动), 空内容/发送后回落单行
let inputGrowRaf = 0;
function autoGrowInput() {
  // iOS 每次按键都同步读 scrollHeight/getComputedStyle 会强制重排; 一帧合并一次。
  if (inputGrowRaf) return;
  inputGrowRaf = requestAnimationFrame(() => {
    inputGrowRaf = 0;
    const el = chatInputEl.value;
    if (!el) return;
    el.style.height = "auto";
    // 按 CSS 的实际 max-height 计算(移动端 140 / 桌面 160), 未超出时隐藏滚动条 ——
    // 空输入框或单行内容不应出现右侧滑块。
    const max = parseFloat(getComputedStyle(el).maxHeight) || 160;
    el.style.height = Math.min(el.scrollHeight, max) + "px";
    el.style.overflowY = el.scrollHeight > max ? "auto" : "hidden";
  });
}
watch(chatInput, autoGrowInput);
// 设备面板展开/收起会改变聊天区宽度: WebKit 的 textarea scrollHeight 会把
// 换行后的 placeholder 也算进去, 面板覆盖时量出的大高度会在收起后残留。
// 布局变化后下一帧重算, 避免输入框突然变成半屏高。
watch(deviceInlineOpen, () => nextTick(autoGrowInput));
const nickInput = ref("");
const seenChatId = ref(0);     // 最后一条已读消息 id: 满 200 条后条数恒定, 不能用条数算未读
const chatCount = ref(0);
const chatBox = ref(null);

/* ---------- 附近设备(最小实现: 复用 HTTP, 不引入 UDP) ---------- */
const PEER_FAVS_KEY = "wft-peer-favs";
const peers = ref([]);
watch(peers, (list) => {
  if (!sendTargets.value.length) return;
  const byId = new Map((list || []).map((p) => [p.id, p]));
  const fresh = sendTargets.value.map((t) => byId.get(t.id)).filter(Boolean);
  const gone = sendTargets.value.length - fresh.length;
  if (gone > 0) {
    showToast(fresh.length ? `有 ${gone} 台目标设备已离线，已移除` : "目标设备已离线，已切回本机共享", "error");
  }
  const changed = fresh.length !== sendTargets.value.length
    || fresh.some((p, i) => p !== sendTargets.value[i]);
  if (changed) sendTargets.value = fresh;   // 刷新 IP/端口/忙碌状态
});
const peerFavs = ref((() => {
  try { return new Set(JSON.parse(localStorage.getItem(PEER_FAVS_KEY) || "[]")); } catch (e) { return new Set(); }
})());
const peersSorted = computed(() => {
  const arr = [...peers.value];
  arr.sort((a, b) =>
    (peerFavs.value.has(b.id) ? 1 : 0) - (peerFavs.value.has(a.id) ? 1 : 0) ||
    (b.last_seen || 0) - (a.last_seen || 0)
  );
  return arr;
});
function isFavPeer(p) { return !!(p && peerFavs.value.has(p.id)); }
/* Snapdrop 风格雷达: 最多 6 个点, 位置固定且稳定, 不随刷新乱跳 */
const RADAR_SLOTS = [[50, 10], [78, 24], [88, 52], [74, 78], [46, 88], [18, 72]];
const radarPeers = computed(() => peersSorted.value.slice(0, RADAR_SLOTS.length));
function radarStyle(i) {
  const [x, y] = RADAR_SLOTS[i % RADAR_SLOTS.length];
  return { left: x + "%", top: y + "%" };
}
/* dir-2 设备行的信号格: 在线 4 格 / 忙碌 2 格 / 离线 1 格 */
function peerRssi(p) {
  const st = peerState(p);
  return st === "online" ? 4 : st === "busy" ? 2 : 1;
}
function peerState(p) {
  if (!p) return "away";
  if (p.busy) return "busy";
  // 服务端已经按自己的单调接收时间给出 online，前端不再用浏览器时钟推算。
  if (typeof p.online === "boolean") return p.online ? "online" : "away";
  const ago = Math.max(0, Date.now() / 1000 - (Number(p.last_seen) || 0));
  return ago < PEER_ONLINE_GRACE_SECS ? "online" : "away";
}
/* 雷达中心跟随当前终端: 手机/平板显示手机图标, 电脑显示显示器图标 */
const selfDeviceKind = computed(() => {
  const platform = currentPlatform();
  return platform === "iOS" || platform === "Android" ? "phone" : "computer";
});
const isMacDesktop = computed(() => isRealDesktop && /Macintosh|Mac OS X/.test(navigator.userAgent));
const isWinDesktop = computed(() => isRealDesktop && /Windows/.test(navigator.userAgent));
const showNetworkSettingsButton = computed(() => isMacDesktop.value || isWinDesktop.value);
const diagnosticsOpen = ref(false);
const probeIp = ref("");
const probeSending = ref(false);
function openDiagnostics() { diagnosticsOpen.value = true; }
function closeDiagnostics() { diagnosticsOpen.value = false; }
function diagGoService() { diagnosticsOpen.value = false; setView(isRealDesktop ? "service" : "settings"); }
function diagOpenQr() { diagnosticsOpen.value = false; openQr(); }
function diagManualSend() { diagnosticsOpen.value = false; openManualSend(); }
/* 手动单播探测: 多播/广播被交换机或防火墙拦截时的兜底 */
async function probePeerByIp() {
  const ip = String(probeIp.value || "").trim().replace(/^\[|\]$/g, "");
  if (!ip || probeSending.value) return;
  if (!isLanHost(ip)) { showToast("请输入局域网 IP（如 192.168.1.8）", "error"); return; }
  probeSending.value = true;
  try {
    const r = await apiFetch(apiBase.value + "/api/discovery/probe", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ ip }),
    });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) { showToast(d.error || "探测失败", "error"); return; }
    showToast(`已向 ${ip} 发送探测，等待对方回包`);
    setTimeout(() => refreshPeers(), 900);
  } catch (e) {
    showToast("探测失败，请检查网络", "error");
  } finally {
    probeSending.value = false;
  }
}
async function openNetworkSettings() {
  if (!showNetworkSettingsButton.value) return;
  const url = isMacDesktop.value
    ? "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork"
    : "ms-settings:windowsdefender";
  try {
    const d = await tauriInvoke("open_url_cmd", { url });
    if (!d || d.ok === false) showToast("请手动打开系统设置中的网络/防火墙权限", "error");
  } catch (e) {
    showToast("请手动打开系统设置中的网络/防火墙权限", "error");
  }
}
function toggleFavPeer(e, p) {
  e.stopPropagation();
  if (!p || !p.id) return;
  const s = new Set(peerFavs.value);
  if (s.has(p.id)) s.delete(p.id); else s.add(p.id);
  peerFavs.value = s;
  try { localStorage.setItem(PEER_FAVS_KEY, JSON.stringify([...s])); } catch (e) {}
  showToast(s.has(p.id) ? "已收藏设备" : "已取消收藏");
}
/* 附近设备只可能是局域网地址: 拒绝把 UDP 伪造包指向公网(文件外带) */
function isLanHost(ip) {
  const s = String(ip || "").trim();
  if (/^\d{1,3}(\.\d{1,3}){3}$/.test(s)) {
    const p = s.split(".").map(Number);
    if (p.some(n => !Number.isInteger(n) || n < 0 || n > 255)) return false;
    return p[0] === 10 || p[0] === 127
      || (p[0] === 172 && p[1] >= 16 && p[1] <= 31)
      || (p[0] === 192 && p[1] === 168)
      || (p[0] === 100 && p[1] >= 64 && p[1] <= 127)
      || (p[0] === 169 && p[1] === 254);
  }
  if (!s.includes(":")) return false;
  // IPv6 必须真的是地址: 旧实现只 startsWith(fe80/fc/fd), fd.example.com 也能过。
  // 去掉 link-local zone(%25en0 / %en0)后交给 URL 解析器做结构校验。
  const core = s.split("%")[0].toLowerCase();
  try { new URL(`http://[${core}]`); } catch (e) { return false; }
  return core === "::1" || core.startsWith("fe80:") || /^f[cd][0-9a-f]{2}:/.test(core);
}
function peerBase(p) {
  const host = String(p.ip || "").replace(/^\[|\]$/g, "");
  // IPv6 link-local 的 zone(%en0)在 URL 里必须写成 %25en0; 否则浏览器不会带出接口
  const urlHost = host.includes(":") ? host.replace(/%/g, "%25") : host;
  const h = urlHost.includes(":") ? `[${urlHost}]` : urlHost;
  return `http://${h}:${p.port || port.value}`;
}
/* LocalSend 式发送面板: 文件 / 文本 / 剪贴板 三种来源 */
// 浏览器/手机浏览器跨设备投递时, 对方可能开了访问令牌: 按 endpoint 记住对方配对码,
// 请求带 X-Access-Pin; 401 时提示用户填写。
const PEER_PINS_KEY = "wft-peer-pins";
const peerPins = ref((() => {
  try { return JSON.parse(localStorage.getItem(PEER_PINS_KEY) || "{}") || {}; } catch (e) { return {}; }
})());
const pinDialogDevice = ref(null);
const pinDialogValue = ref("");
function promptPeerPin(p) {
  pinDialogDevice.value = p;
  pinDialogValue.value = peerPins.value[peerKey(p)] || "";
}
async function savePeerPinDialog() {
  const p = pinDialogDevice.value;
  if (!p || pinChecking.value) return;
  const pin = pinDialogValue.value.trim();
  if (!pin) {
    savePeerPin(p, "");
    pinDialogDevice.value = null;
    showToast("已清除配对码");
    return;
  }
  pinChecking.value = true;
  const res = await verifyPeerPin(p, pin);
  pinChecking.value = false;
  if (res === "invalid") { showToast("配对码不正确，请重新输入", "error"); return; }
  if (res === "locked") { showToast("尝试过于频繁，请稍后再试", "error"); return; }
  savePeerPin(p, pin);
  pinDialogDevice.value = null;
  showToast(res === "ok" ? "配对码已验证并保存" : "对方暂不可达，配对码已保存");
}
function peerKey(p) { return p && p.id ? `id:${p.id}` : `${p?.ip}:${p?.port}`; }
function savePeerPin(p, pin) {
  if (!p) return;
  const k = peerKey(p);
  const next = { ...peerPins.value };
  if (pin) next[k] = pin; else delete next[k];
  peerPins.value = next;
  try { localStorage.setItem(PEER_PINS_KEY, JSON.stringify(next)); } catch (e) {}
}
const pinChecking = ref(false);
// 保存前先向对方验证配对码: 直达设备的 API 都走访问码中间件, /api/devices 最轻量。
// 错误/限流都立即反馈且不落盘; 对方暂时不可达时仍保存(标注未验证), 不阻塞使用。
async function verifyPeerPin(p, pin) {
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), 8000);
  try {
    const r = await fetch(peerBase(p) + "/api/devices", {
      headers: { "X-Access-Pin": pin },
      signal: ctl.signal,
    });
    if (r.status === 401) return "invalid";
    if (r.status === 429) return "locked";
    return r.ok ? "ok" : "unreachable";
  } catch (e) {
    return "unreachable";
  } finally { clearTimeout(timer); }
}
function currentPlatform() {
  if (isIOSFamily) return "iOS";
  if (/Android/.test(navigator.userAgent)) return "Android";
  if (isRealDesktop) return "电脑";
  return "浏览器";
}
/* ---------- 直发到目标设备(不弹窗, 时间线只留本机回执) ---------- */
function peerPinOf(p) { return (peerPins.value[peerKey(p)] || "").trim(); }
function receiptPreview(text) {
  const s = String(text || "");
  return s.length > 32 ? s.slice(0, 32) + "…" : s;
}
async function directSendText(p, text, opts = {}) {
  if (!p || !text) return false;
  const chars = Array.from(String(text)).length;
  if (chars > cfg.value.msgMax) {
    showToast(`消息超过 ${cfg.value.msgMax} 字符上限（当前 ${chars}），请删减后再发送`, "error");
    return false;
  }
  if (!isLanHost(p.ip) || !(Number(p.port) > 0 && Number(p.port) <= 65535)) {
    showToast("设备地址不可用，请刷新设备列表", "error");
    return false;
  }
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), 30000);
  if (opts.signal) {
    if (opts.signal.aborted) ctl.abort();
    else opts.signal.addEventListener("abort", () => ctl.abort(), { once: true });
  }
  const pin = peerPinOf(p);
  try {
    const headers = { "Content-Type": "application/json" };
    if (pin) headers["X-Access-Pin"] = pin;
    const r = await fetch(peerBase(p) + "/api/chat?nick=" + encodeURIComponent(curNick.value), {
      method: "POST",
      headers,
      body: JSON.stringify({ nickname: curNick.value, message: text }),
      signal: ctl.signal,
    });
    if (r.status === 401) {
      // 缓存的配对码被拒: 立即清除并重新弹框(留空), 否则后续发送会反复带同一个
      // 错码, 累计失败触发服务端指数限流(429), 表现为“一直失败且不再提示配对”。
      savePeerPin(p, "");
      promptPeerPin(p);
      throw new Error("peer-pin");
    }
    if (r.status === 403) throw new Error("peer-forbidden");
    // 限流窗内正确码也会返回 429(服务端刻意不区分对错): 单独提示, 不误报离线
    if (r.status === 429) throw new Error("peer-pin-locked");
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    recordTransfer(p, "text", receiptPreview(text));
    addReceipt(`已向 ${p.nickname || "设备"} 发送消息：${receiptPreview(text)}`);
    return true;
  } catch (e) {
    if (e?.name === "AbortError") {
      // 用户点「取消发送」也会触发 AbortError: 只有真正的 30s 超时才提示离线,
      // 主动取消不该再弹一个"发送超时"误导用户。
      if (!opts.signal?.aborted) showToast("发送超时：对方可能已离线", "error");
    }
    else if (e?.message === "peer-pin") showToast("配对码不正确，请重新输入后发送", "error");
    else if (e?.message === "peer-pin-locked") showToast("配对码错误次数过多，已被对方临时限流，请稍后再试", "error");
    else if (e?.message === "peer-forbidden") showToast("浏览器端跨设备发送需对方开启访问令牌；可使用桌面/手机 App", "error");
    else if (e?.name === "TypeError") {
      // 把目标地址带进提示: IPv6 link-local(fe80::%zone)在 WebView 里往往不可用,
      // 与"手机熄屏被冻结"表现相同, 直接看地址即可区分。
      const where = p && p.ip ? `${p.ip}:${p.port || port.value}` : "目标设备";
      const linkLocalV6 = p && /^fe80:/i.test(String(p.ip || ""));
      showToast(linkLocalV6
        ? `对方仅以 IPv6 链路本地地址发现（${where}），请点「重新扫描」刷新设备后重试`
        : `无法连接设备 ${where}：请确认手机 App 未被系统冻结（可在手机设置中关闭电池优化）`, "error");
    }
    else showToast("发送失败：对方可能已离线或开启访问令牌", "error");
    return false;
  } finally { clearTimeout(timer); }
}
async function directSendFiles(p, files, opts = {}) {
  const list = Array.from(files || []);
  const sent = new Set();
  const fail = () => ({ allOk: false, sent });
  if (!p || !list.length) return fail();
  if (!isLanHost(p.ip) || !(Number(p.port) > 0 && Number(p.port) <= 65535)) {
    showToast("设备地址不可用，请刷新设备列表", "error");
    return fail();
  }
  if (directSending.value) {
    // 排队等待当前批次结束: 直接丢弃会让用户以为第二批已发出。
    showToast(`正在发送，已排队 ${list.length} 个文件`, "success");
    const waitStart = Date.now();
    while (directSending.value && Date.now() - waitStart < 15 * 60 * 1000) {
      await new Promise((r) => setTimeout(r, 250));
    }
    if (directSending.value) {
      showToast("等待前一批发送超时，请稍后重试", "error");
      return fail();
    }
  }
  directSending.value = true;
  const pin = peerPinOf(p);
  let ok = 0, forbidden = 0, unauthorized = 0, rateLimited = 0;
  showToast(`正在发送 ${list.length} 个文件到 ${p.nickname || "设备"}…`);
  try {
    for (const f of list) {
      if (!opts.manual && sendTarget.value && sendTarget.value.id !== p.id) break;   // 目标已切换; 手动发送不绑定目标
      const rel = f.webkitRelativePath || f.name;
      try {
        await new Promise((resolve, reject) => {
          const xhr = new XMLHttpRequest();
          xhr.open("POST", peerBase(p) + API.upload, true);
          xhr.setRequestHeader("X-Filename", encodeURIComponent(rel));
          xhr.setRequestHeader("X-File-Size", String(f.size));
          xhr.setRequestHeader("X-Nickname", encodeURIComponent(curNick.value));
          if (pin) xhr.setRequestHeader("X-Access-Pin", pin);
          xhr.timeout = 10 * 60 * 1000;
          // 半开连接时 XHR 既不 error 也不 timeout: 125s 无上传进展就中止,
          // 否则 directSending 会锁死 10 分钟并阻塞后续直发。
          let lastTick = Date.now();
          let settled = false;
          const done = (err) => {
            if (settled) return;
            settled = true;
            clearInterval(stall);
            err ? reject(err) : resolve();
          };
          const stall = setInterval(() => {
            if (Date.now() - lastTick <= 125000) return;
            try { xhr.abort(); } catch (_) {}
            done(new Error("timeout"));
          }, 5000);
          xhr.upload.onprogress = () => { lastTick = Date.now(); };
          xhr.onload = () => done(xhr.status === 200 ? null : new Error(String(xhr.status)));
          xhr.onerror = () => done(new Error("network"));
          xhr.ontimeout = () => done(new Error("timeout"));
          const abort = () => {
            try { xhr.abort(); } catch (_) {}
            done(new Error("aborted"));
          };
          if (opts.signal) {
            if (opts.signal.aborted) abort();
            else opts.signal.addEventListener("abort", abort, { once: true });
          }
          xhr.send(f);
        });
        ok++;
        sent.add(f);
        recordTransfer(p, "file", rel);
      } catch (err) {
        const status = String(err?.message);
        // 401/429 说明"继续用同一凭据重试只会把对方 IP 越锁越死": 必须立即中止整批,
        // 让用户先更新配对码或等限流窗口结束, 而不是把剩余文件全打一遍。
        // 401 = 缓存的配对码被拒: 清除后再弹框, 避免一直复用错码把自己打进 429
        if (status === "401") { unauthorized++; savePeerPin(p, ""); break; }
        if (status === "429") { rateLimited++; break; }
        if (status === "aborted" || status === "timeout" || status === "network") break;
        if (status === "403") forbidden++;
      }
    }
  } finally { directSending.value = false; }
  if (unauthorized) promptPeerPin(p);
  if (ok) addReceipt(`已向 ${p.nickname || "设备"} 发送 ${ok} 个文件`);
  // 用户主动取消: 已发出去的文件照常记回执, 但不再弹"发送失败"吓人。
  const userAborted = !!(opts.signal && opts.signal.aborted);
  if (userAborted) {
    if (ok) showToast(`已取消，已发送 ${ok} 个文件`);
    return fail();
  }
  if (!ok) {
    showToast(unauthorized ? "配对码不正确，请重新输入后重试"
      : rateLimited ? "对方拒绝了本次发送（尝试过于频繁），请稍后再试"
      : forbidden ? "浏览器端跨设备发送需对方开启访问令牌；可使用桌面/手机 App"
      : `发送失败：无法连接设备 ${p.ip}:${p.port || port.value}，请确认手机 App 未被系统冻结（可关闭电池优化）`, "error");
    return fail();
  }
  if (forbidden || unauthorized || rateLimited) {
    const rest = rateLimited ? "其余因尝试过于频繁被限流"
      : unauthorized ? "其余配对码不正确"
      : "其余被拒绝";
    showToast(`已发送 ${ok} 个，${rest}`, "error");
    return fail();
  }
  return { allOk: ok === list.length, sent };
}
const discoveryInfo = ref({ started: false, joined: false, error: "" });
const peersRefreshing = ref(false);
let peersSeq = 0;
async function refreshPeers() {
  // 设备发现只属于桌面/手机 App; 浏览器不请求 /api/devices(设计预览除外)
  if (!isDesktop && !PREVIEW_MOBILE) { peers.value = []; return; }
  // 浏览器端 apiBase 为空表示相对路径; 移动壳服务未就绪时才跳过
  if (serviceDown() || (isDesktop && !apiBase.value)) { peers.value = []; return; }
  const epoch = sessionEpoch;   // 切换电脑后旧服务的迟到列表不得写回新会话
  const seq = ++peersSeq;
  peersRefreshing.value = true;
  try {
    const r = await apiFetch(apiBase.value + "/api/devices");
    if (!r.ok || seq !== peersSeq) return;
    const d = await r.json();
    if (epoch !== sessionEpoch || seq !== peersSeq) return;
    peers.value = Array.isArray(d.devices) ? d.devices : [];
    if (d.discovery && typeof d.discovery === "object") discoveryInfo.value = d.discovery;
  } catch (e) {
  } finally {
    if (seq === peersSeq) peersRefreshing.value = false;
  }
}
/* 主动探测: 让同网段设备立即单播回宣告, 不再等对方下一次 5s 广播 */
async function mobileProbe() {
  const ok = await sendDiscoveryProbe(true);
  showToast(ok ? "已发送单播探测" : "探测失败：请确认服务与网络", ok ? "success" : "error");
}
async function sendDiscoveryProbe(scan = false) {
  if (!isDesktop || serviceDown() || (isDesktop && !apiBase.value)) return false;
  try {
    const r = await apiFetch(apiBase.value + "/api/discovery/probe", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ scan: !!scan }),
    });
    return r.ok;
  } catch (e) {
    return false;
  }
}
const radarScanning = ref(false);
/* ---------- 桌面 WebView 自定义右键菜单(替代 Reload 菜单) ---------- */
const contextMenu = ref(null);
function closeContextMenu() { contextMenu.value = null; }
function onContextMenu(e) {
  if (!isDesktop) return;   // 浏览器保留原生菜单
  const t = e.target;
  if (t && t.closest && t.closest("input,textarea,[contenteditable='true']")) return;
  // 弹层打开时不叠加右键菜单
  if (diagnosticsOpen.value || manualSendOpen.value || qrShow.value || previewName.value || needPin.value || showDel.value || introShow.value) return;
  e.preventDefault();
  const sel = String(window.getSelection()?.toString() || "").trim();
  const fileEl = t && t.closest ? t.closest("[data-file]") : null;
  const file = fileEl ? (fileEl.getAttribute("data-file") || "") : "";
  if (file) prefetchShareTicket(file);   // 右键文件即预取票据，点“分享链接”时同步复制不吃 await 丢激活
  contextMenu.value = {
    x: Math.min(e.clientX, Math.max(8, window.innerWidth - 180)),
    y: Math.min(e.clientY, Math.max(8, window.innerHeight - 390)),
    hasSel: !!sel,
    sel,
    file,
  };
}
function ctxCopy() {
  const page = shareUrl.value && !shareUrl.value.includes("--") ? shareUrl.value : "";
  const text = contextMenu.value?.sel || page;
  if (!text) { showToast("没有可复制的内容", "error"); return; }
  copyText(text, contextMenu.value?.hasSel ? "已复制选中内容" : "已复制页面地址");
  closeContextMenu();
}
function ctxScan() {
  closeContextMenu();
  refreshDiscover();
}
function ctxDiagnose() {
  closeContextMenu();
  openDiagnostics();
}
function ctxPasteInput() {
  closeContextMenu();
  if (view.value !== "transfer") { showToast("请先回到消息发送页", "error"); return; }
  readLocalClipboardText().then((text) => {
    if (text && text.trim()) {
      _fillChatText(text, "已粘贴剪贴板文本");
      publishSharedClip(text, true);
    } else showToast("剪贴板为空或当前页面无法读取", "error");
  });
}
function ctxManualSend() { closeContextMenu(); openManualSend(); }
function ctxFilePreview() { const n = contextMenu.value?.file; closeContextMenu(); if (n) previewFileWeb(n); }
function ctxFileOpen() { const n = contextMenu.value?.file; closeContextMenu(); if (!n) return; if (isRealDesktop) openFile(n); else downloadFileWeb(n); }
function ctxFileCopy() { const n = contextMenu.value?.file; closeContextMenu(); if (n) copyFileToClipboard(n); }
function ctxFileShare() { const n = contextMenu.value?.file; closeContextMenu(); if (n) shareFile(n); }
function ctxFileLocate() { const n = contextMenu.value?.file; closeContextMenu(); if (n) locateFile(n); }
/* ---------- 手动发送（LocalSend 式：不依赖发现，直接填 IP:端口） ---------- */
const manualSendOpen = ref(false);
const manualAddr = ref("");
const manualText = ref("");
const manualFiles = ref([]);
const manualSending = ref(false);
let manualAbortCtrl = null;
const sendDialogTarget = ref(null);   // null = 手动 IP:端口
const sendTab = ref("files");         // files | folder | text | clipboard
const sendDialogName = computed(() => {
  const p = sendDialogTarget.value || parseManualTarget(manualAddr.value);
  return p ? (p.nickname || p.ip || "设备") : "手动目标";
});
const sendDialogStatus = computed(() => {
  if (manualSending.value) return "发送中…";
  if (manualFiles.value.length) return `${manualFiles.value.length} 个文件已选择`;
  if (manualText.value.trim()) return "文本已就绪";
  return "等待添加文件";
});
const sendDialogReady = computed(() => {
  if (manualSending.value) return false;
  const p = sendDialogTarget.value || parseManualTarget(manualAddr.value);
  return !!(p && (manualFiles.value.length || manualText.value.trim()));
});
function pickManualFiles() { document.getElementById("manualFileInput")?.click(); }
function pickManualFolder() {
  if (isIOS) { showToast("iOS 不支持选择文件夹，请改用「文件」多选", "error"); return; }
  document.getElementById("manualDirInput")?.click();
}
function pickManualPhotos() { document.getElementById("manualPhotoInput")?.click(); }
function onSendDrop(e) {
  const files = Array.from((e.dataTransfer && e.dataTransfer.files) || []);
  if (files.length) manualFiles.value = manualFiles.value.concat(files);
}
function setSendTab(t) {
  sendTab.value = t;
  if (t === "clipboard" && !manualText.value.trim()) loadClipboardForSend();
}
async function loadClipboardForSend() {
  try {
    const text = await readLocalClipboardText();
    if (text && text.trim()) manualText.value = text;
    else showToast("剪贴板为空或当前页面无法读取", "error");
  } catch (e) {
    showToast("剪贴板为空或当前页面无法读取", "error");
  }
}
function openManualSend() {
  // 浏览器跨源直发会先被 connect-src 'self' 拦截；不放开 CSP，改为明确提示，
  // 浏览器保留聊天室/共享文件，跨设备直发走桌面端或手机 App。
  if (!isDesktop && !PREVIEW_MOBILE) {
    showToast("浏览器端不支持跨设备直发，请使用桌面端或手机 App", "error");
    return;
  }
  sendDialogTarget.value = null;
  sendTab.value = "files";
  manualSendOpen.value = true;
  // 保留上次取消/失败时的地址、文本与已选文件, 方便直接重试。
  // 成功发送后 submitManualSend 会清空文本/文件(地址保留)。
}
function closeManualSend() {
  // 发送中关闭/取消: 必须中止批次，不能把整个 App 锁在弹框里。
  if (manualSending.value && manualAbortCtrl) {
    try { manualAbortCtrl.abort(); } catch (_) {}
    manualSending.value = false;
  }
  manualSendOpen.value = false;
  sendDialogTarget.value = null;
}
function parseManualTarget(raw) {
  let s = String(raw || "").trim();
  if (!s) return null;
  s = s.replace(/^https?:\/\//i, "").split("/")[0];
  let host = s;
  let port = 1688;
  if (s.startsWith("[")) {
    const end = s.indexOf("]");
    if (end < 0) return null;
    host = s.slice(1, end);
    if (s[end + 1] === ":") port = Number(s.slice(end + 2));
  } else if ((s.match(/:/g) || []).length === 1) {
    const i = s.lastIndexOf(":");
    host = s.slice(0, i);
    port = Number(s.slice(i + 1));
  }
  host = host.replace(/%25/gi, "%").trim();
  if (!host || !isLanHost(host) || !(port > 0 && port <= 65535)) return null;
  return { id: `manual-${host}:${port}`, nickname: `手动 ${host}`, ip: host, port, platform: "manual" };
}
function onManualFiles(e) {
  const files = Array.from((e.target && e.target.files) || []);
  if (e.target) e.target.value = "";
  if (files.length) manualFiles.value = manualFiles.value.concat(files);
}
function removeManualFile(i) { manualFiles.value.splice(i, 1); }
async function submitManualSend() {
  if (manualSending.value) return;
  const p = sendDialogTarget.value || parseManualTarget(manualAddr.value);
  if (!p) { showToast("请输入 IP 或 IP:端口，例如 10.161.118.123", "error"); return; }
  const text = manualText.value.trim();
  const files = manualFiles.value.slice();
  if (!text && !files.length) { showToast("请输入文本或选择文件", "error"); return; }
  manualSending.value = true;
  const ctl = new AbortController();
  manualAbortCtrl = ctl;
  try {
    let allOk = true;
    if (text) {
      const tOk = await directSendText(p, text, { signal: ctl.signal });
      if (tOk) manualText.value = "";   // 成功的部分先清掉, 失败的部分留作重试
      allOk = tOk && allOk;
    }
    if (files.length && !ctl.signal.aborted) {
      const fRes = await directSendFiles(p, files, { manual: true, signal: ctl.signal });
      // 部分成功时只保留失败项: 原实现把已成功文件也留下, 重试会重复发送
      if (fRes.sent.size) {
        manualFiles.value = manualFiles.value.filter(f => !fRes.sent.has(f));
      }
      allOk = fRes.allOk && allOk;
    }
    if (ctl.signal.aborted || !manualSendOpen.value) return;
    // 只有全部成功才关闭弹框；失败/超时保留未成功的文本与文件供重试。
    if (allOk) { manualSendOpen.value = false; sendDialogTarget.value = null; }
  } finally {
    manualSending.value = false;
    if (manualAbortCtrl === ctl) manualAbortCtrl = null;
  }
}
/* 周边设备手动刷新: 主动 probe + 拉设备表 + 本机心跳; probe 回包通常 <300ms */
async function refreshDiscover() {
  if (!running.value) {
    showToast("服务未启动，请先到「服务中心」启动服务", "error");
    return;
  }
  radarScanning.value = true;
  const started = Date.now();
  try {
    await sendDiscoveryProbe(true);
    await new Promise((r) => setTimeout(r, 200));
    await Promise.all([sendHello(), refreshPeers()]);
    showToast(peers.value.length ? `已发现 ${peers.value.length} 台设备` : "暂未发现其他设备，保持本页会自动刷新");
  } finally {
    // 让雷达至少转一圈, 否则本地探测太快完成, 点击像没反应
    const wait = Math.max(0, 600 - (Date.now() - started));
    setTimeout(() => { radarScanning.value = false; }, wait);
  }
}
/* ---------- 周边设备展示 ---------- */
function peerPlatformKind(p) {
  const s = String((p && p.platform) || "").toLowerCase();
  if (s.includes("android") || s.includes("ios")) return "phone";
  if (s.includes("mac") || s.includes("win") || s.includes("linux") || s.includes("电脑")) return "laptop";
  return "desktop";
}
function peerStatusText(p) {
  if (!p) return "";
  if (p.busy) return "忙碌中";
  if (p.online === true) return "在线 · 可发送";
  const ago = Math.max(0, Date.now() / 1000 - (Number(p.last_seen) || 0));
  if (ago < 60) return `${Math.max(1, Math.floor(ago))} 秒前活跃`;
  return `${Math.floor(ago / 60)} 分钟前活跃`;
}
const selfRoleLabel = computed(() =>
  mobileShell ? "手机本机节点" : (isDesktop ? "电脑" : "浏览器页面"));
/* 用"实际监听状态"而不是偏好开关判断: 打开开关但没重启服务时 lanActive 仍为 false,
   反过来关闭后没重启时它仍为 true。只看 openLanEnabled 会把两种状态都说反。 */
const discoverLanHint = computed(() => {
  if (!(mobileShell || isRealDesktop)) return false;
  return running.value && !lanActive.value;
});
/* 偏好已开、等待重启服务生效 */
const discoverLanPending = computed(() => discoverLanHint.value && openLanEnabled.value);
async function restartServiceForLan() {
  if (mobileShell) { await restartMobileService(); return; }
  if (!isRealDesktop) return;
  try {
    if (running.value) await tauriInvoke("control_stop_cmd");
    running.value = false;
    await _restartForDirSwitch();
    refreshStatus();
  } catch (e) {
    showToast("重启服务失败，请到「服务中心」手动重启", "error");
  }
}
async function enableLanFromDiscover() {
  if (!(mobileShell || isRealDesktop)) return;
  try {
    const d = await tauriInvoke("set_open_lan_cmd", { open: true });
    openLanEnabled.value = !!(d && d.openLan);
    if (running.value) {
      await restartServiceForLan();
      showToast("已开启局域网访问，服务已重启");
    } else {
      showToast("已开启局域网访问，启动服务后生效");
    }
  } catch (e) {
    showToast("开启失败，请到设置页重试", "error");
  }
}
async function handleLanDiscoverAction() {
  if (discoverLanPending.value) {
    await restartServiceForLan();
    showToast("服务已重启，局域网访问已生效");
    return;
  }
  await enableLanFromDiscover();
}
/* 手机 App 首次运行: 安全模式(仅回环)下桌面端永远发现不了手机。
   用一次性弹框让用户明确选择, 而不是把"设备列表为空"留给用户自己猜。 */
const LAN_ASK_KEY = "wft-lan-asked";
const lanAskOpen = ref(false);
const lanAskPin = ref(true);   // 公共 Wi-Fi 下默认同时加访问令牌
let lanAskSeen = false;
function maybeAskLan() {
  if (!mobileShell || lanAskSeen || lanAskOpen.value) return;
  if (serverAddr.value && !isLocalAddr(serverAddr.value)) return;   // 非本机地址不触发局域网询问
  if (!running.value || openLanEnabled.value) return;
  try { if (localStorage.getItem(LAN_ASK_KEY) === "1") return; } catch (e) {}
  lanAskSeen = true;
  lanAskOpen.value = true;
}
function markLanAsked() {
  lanAskSeen = true;
  try { localStorage.setItem(LAN_ASK_KEY, "1"); } catch (e) {}
  // WKWebView localStorage 可能不落盘: 同步一份 Rust pref, 重启不再重复首装询问
  saveTextPref("lan-asked", "1");
}
async function acceptLanAsk() {
  markLanAsked();
  lanAskOpen.value = false;
  try {
    const d = await tauriInvoke("set_open_lan_cmd", { open: true });
    openLanEnabled.value = !!(d && d.openLan);
    // 先落令牌再重启服务: 重启后本机请求(WS/轮询)立即带上新码, 不会先吃一轮 401
    if (lanAskPin.value && !pinEnabled.value) {
      const p = await tauriInvoke("set_access_pin_cmd", { enable: true });
      if (p && p.ok) {
        pinEnabled.value = true;
        pinCode.value = p.pin || "";
        accessPin.value = p.pin || "";
        try { localStorage.setItem(LS.pin, accessPin.value); } catch (e) {}
        saveTextPref("access-pin", accessPin.value);
      }
    }
    await restartMobileService();
    showToast(pinEnabled.value
      ? `已开启局域网访问，配对码 ${pinCode.value}`
      : "已开启局域网访问，其他设备可以发现本机了");
  } catch (e) {
    showToast("开启失败，请到设置页重试", "error");
  }
}
function dismissLanAsk() {
  markLanAsked();
  lanAskOpen.value = false;
}
watch([openLanEnabled, running, serverAddr], maybeAskLan);

/* ---------- 共享剪贴板 ---------- */
/* 系统剪贴板自动同步（本机服务内）是可选能力, 默认关闭:
   它会把用户复制的密码/验证码等持续上传到服务端, 默认开启等于默认外带敏感内容。
   浏览器端仍保留"在聊天输入框粘贴时回传"的显式行为(见 onPaste/publishPastedText)。 */
const CLIP_SYNC_KEY = "wft-clip-sync";
const clipSyncEnabled = ref((() => {
  try { return localStorage.getItem(CLIP_SYNC_KEY) === "1"; } catch (e) { return false; }
})());
function setClipSyncEnabled(v) {
  clipSyncEnabled.value = !!v;
  try { localStorage.setItem(CLIP_SYNC_KEY, v ? "1" : "0"); } catch (e) {}
  if (isDesktop) {
    tauriInvoke("save_text_pref_cmd", { key: "clip-sync", value: v ? "1" : "0" }).catch(() => {});
  }
  if (v) { if (running.value) initClipboardSync(); }
  else { stopClipboardSync(); }
}
function toggleClipSync() {
  setClipSyncEnabled(!clipSyncEnabled.value);
  showToast(clipSyncEnabled.value
    ? "已开启剪贴板同步：与连接本机服务的设备共享文本；电脑 App 与手机 App 之间请用发送面板「剪贴板」投递"
    : "已关闭系统剪贴板自动同步");
}
const sharedClip = ref({ text: "", updatedAt: 0, from: "" });
const sharedClipLoading = ref(false);
const sharedClipSaving = ref(false);
let sharedClipLoadSeq = 0;
async function loadSharedClip() {
  if ((isDesktop && !apiBase.value) || serviceDown()) return;
  const epoch = sessionEpoch;   // 切换电脑后旧服务的剪贴板不得落到新会话
  const seq = ++sharedClipLoadSeq;
  sharedClipLoading.value = true;
  try {
    const r = await apiFetch(apiBase.value + API.clipboard);
    if (!r.ok) return;
    const d = await r.json();
    if (seq !== sharedClipLoadSeq || epoch !== sessionEpoch) return;
    const updatedAt = Number(d.updatedAt) || 0;
    // 两次 WS/4.5s 兜底并发时，旧响应可能后到；updatedAt 更旧的内容不能覆盖新剪贴板。
    if (updatedAt && updatedAt < (Number(sharedClip.value.updatedAt) || 0)) return;
    sharedClip.value = {
      text: typeof d.text === "string" ? d.text : "",
      updatedAt,
      from: typeof d.from === "string" ? d.from : "",
    };
  } catch (e) {
  } finally {
    if (seq === sharedClipLoadSeq) sharedClipLoading.value = false;
  }
}
/* 只同步文本: 本机服务共享槽 <-> 本机系统剪贴板; 电脑 App 与手机 App 之间不自动转发。
   App 壳的自动同步由 CC设置 的开关控制(默认关闭); 浏览器只在聊天输入框粘贴时回传。 */
async function publishSharedClip(text, silent = false) {
  const val = clampChars(String(typeof text === "string" ? text : ""), 20000).trim();
  if (!val) { if (!silent) showToast("剪贴板为空", "error"); return false; }
  if (sharedClipSaving.value) return false;
  sharedClipSaving.value = true;
  try {
    const r = await apiFetch(apiBase.value + API.clipboard, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: val, from: curNick.value }),
    });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(d.error || "同步失败");
    sharedClip.value = { text: val, updatedAt: Number(d.updatedAt) || Date.now() / 1000, from: curNick.value };
    return true;
  } catch (e) {
    if (!silent) showToast(e.message || "同步失败", "error");
    return false;
  } finally {
    sharedClipSaving.value = false;
  }
}
/* 粘贴事件: 浏览器/其他端长按粘贴时, 顺手把文本写回共享剪贴板(电脑可立即粘贴)。
   文件/图片仍走上传, 不参与文本同步。 */
function publishPastedText(text, target) {
  // 自动同步关闭时, 粘贴只填充输入框, 不把用户未发送的内容写进局域网共享槽
  if (!clipSyncEnabled.value) return;
  const t = clampChars(String(text || ""), 20000).trim();
  if (!t || !running.value || isQrPetWindow) return;
  // 只回传"在聊天输入框里粘贴"的文本 —— 全局 paste 监听会把配对码、昵称、
  // 日志搜索等输入框的粘贴也外发, 属于越权收集敏感输入。
  if (!target || !target.closest || !target.closest(".tx-input")) return;
  lastSentClip = t;   // 抑制自己的回声
  publishSharedClip(t, true);
}
/* 读取本机系统剪贴板: 桌面/移动壳走原生插件; 浏览器仅安全上下文可用;
   HTTP 局域网页面读不到时返回空串, 由调用方引导长按粘贴。 */
async function readLocalClipboardText() {
  if (isDesktop) {
    try { const t = await readClipboardText(); if (t && t.trim()) return t; } catch (_) {}
  }
  if (navigator.clipboard && window.isSecureContext) {
    try { const t = await navigator.clipboard.readText(); if (t && t.trim()) return t; } catch (_) {}
  }
  return "";
}


/* ---------- 本机服务内剪贴板同步(仅桌面/手机 App 壳; 浏览器粘贴事件回传) ----------
   本机剪贴板变化 -> POST /api/clipboard；服务端变化 -> 写回本机系统剪贴板。
   开关在 CC设置(默认关闭)；用 lastSent/lastApplied + 哈希抑制回声，避免两端互相覆盖。 */
let clipSyncTimer = null;
let clipSyncReady = false;
let clipSyncBusy = false;
let lastSentClip = "";
let lastAppliedClip = "";
let clipRemoteTick = 0;
let pendingRemoteApply = false;

function stopClipboardSync() {
  if (clipSyncTimer) clearInterval(clipSyncTimer);
  clipSyncTimer = null;
  clipSyncReady = false;
  clipSyncBusy = false;
  clipRemoteTick = 0;
  pendingRemoteApply = false;
}
async function initClipboardSync() {
  if (!isDesktop || isQrPetWindow || clipSyncTimer || !clipSyncEnabled.value) return;
  await loadSharedClip();
  const local = (await readLocalClipboardText()).trim();
  // 首次连接只记基线: 不把服务端旧内容覆盖到本机, 也不把本机旧内容强行发布
  lastSentClip = local;
  lastAppliedClip = (sharedClip.value.text || "").trim();
  clipSyncReady = true;
  clipSyncTimer = setInterval(syncClipboardTick, 1500);
}
async function restartClipboardSync() {
  stopClipboardSync();
  lastSentClip = "";
  lastAppliedClip = "";
  if (clipSyncEnabled.value) await initClipboardSync();
}
async function applyRemoteClipboard() {
  if (!isDesktop || !clipSyncReady) return;
  // 本地发布在途时不要套用远端快照: 否则刚发出去的新内容可能被旧响应覆盖。
  if (sharedClipSaving.value) return;
  // 页面隐藏(收进托盘/最小化/切后台)时不写系统剪贴板: 防止后台被远端静默注入。
  // 重新可见时由 visibilitychange 补一次, 用户在场并收到来源提示。
  if (document.visibilityState !== "visible") return;
  // 正在读/写本机剪贴板: 记住这次远端变化, 等当前操作结束后补写 ——
  // 直接 return 会丢掉 WS 事件, 表现为"手机发了但电脑剪贴板没变"。
  if (clipSyncBusy) { pendingRemoteApply = true; return; }
  const remote = (sharedClip.value.text || "").trim();
  if (!remote || remote === lastAppliedClip || remote === lastSentClip) return;
  clipSyncBusy = true;
  try {
    await writeClipboardText(remote);
    lastAppliedClip = remote;
    showToast(`已从 ${sharedClip.value.from || "另一台设备"} 同步剪贴板`, "success");
  } catch (e) {
    // 写权限/系统拒绝: 静默, 下一次 WS/轮询再试
  } finally {
    clipSyncBusy = false;
    if (pendingRemoteApply) {
      pendingRemoteApply = false;
      // 让出一帧再补写, 避免和刚落定的本机读取/发布打架
      setTimeout(() => { applyRemoteClipboard(); }, 0);
    }
  }
}
async function syncClipboardTick() {
  if (!isDesktop || !clipSyncReady || clipSyncBusy) return;
  // 页面不可见时不读取/发布本机剪贴板: 避免托盘/最小化期间复制的敏感内容被自动共享
  if (document.visibilityState !== "visible") return;
  // 每 ~4.5s 兜底拉一次服务端: WS 断线时仍能收到另一端的变化
  if (++clipRemoteTick >= 3) {
    clipRemoteTick = 0;
    await loadSharedClip();
    await applyRemoteClipboard();
  }
  const local = (await readLocalClipboardText()).trim();
  if (!local || local === lastSentClip || local === lastAppliedClip || local === (sharedClip.value.text || "").trim()) return;
  const prev = lastSentClip;
  lastSentClip = local;   // 先置位: 自己的 POST 回声不能被当成远端变化
  clipSyncBusy = true;
  try {
    const ok = await publishSharedClip(local, true);
    if (!ok) lastSentClip = prev;
  } finally {
    clipSyncBusy = false;
  }
}
async function sendHello() {
  // 浏览器不参与发现, 也不上报 /api/hello
  if (!isDesktop) return;
  if (serviceDown() || (isDesktop && !apiBase.value)) return;
  try {
    await apiFetch(apiBase.value + "/api/hello", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        nickname: curNick.value,
        platform: currentPlatform(),
        busy: uploads.value.some(u => u.status === "uploading" || u.status === "waiting")
      })
    });
  } catch (e) {}
}
const unread = computed(() => chatMsgs.value.filter(m => m.id > seenChatId.value).length);
function setSeenToLatest() {
  const last = chatMsgs.value.length ? chatMsgs.value[chatMsgs.value.length - 1] : null;
  seenChatId.value = last ? last.id : 0;
}
const curNick = computed(() => clampNick(nickInput.value.trim() || autoNick.value));
/* 合并后只有「传输」一个页面: 页面在前台且窗口有焦点时, 消息可见即已读。
   失焦/切后台保留未读; 浏览器标签标题同步未读数(桌面壳由侧栏角标承担)。 */
const introShow = ref(false);
/* 聊天是否被弹层/设备面板/文件页遮住: 遮住时收到新消息不能算已读,
   否则 SEND 标签红点永远不出现, 用户会漏消息。 */
const chatCovered = computed(() => {
  if (view.value !== "transfer") return true;
  return deviceInlineOpen.value
    || fileSheetOpen.value
    || manualSendOpen.value
    || pinDialogDevice.value
    || previewName.value
    || needPin.value
    || showDel.value
    || historyOpen.value
    || diagnosticsOpen.value
    || fileMenuName.value
    || contextMenu.value
    || qrShow.value
    || nickOpen.value
    || lanAskOpen.value
    || copyFallbackShow.value
    || introShow.value;
});
function markChatSeen() {
  if (chatCovered.value) return;
  if (document.visibilityState !== "visible" || !document.hasFocus()) return;
  setSeenToLatest();
}
watch([unread, view], () => {
  if (isQrPetWindow) return;
  const base = "CC Flash · " + (view.value === "discover" ? discoverTitle.value : (VIEW_TITLES[view.value] || "消息发送"));
  document.title = unread.value > 0 ? `(${unread.value}) ${base}` : base;
}, { immediate: true });
function avatarFor(nick) {
  const pool = AVATAR_POOL;
  let h = 0;
  const s = nick || cfg.value.defaultNick;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  return pool[Math.abs(h) % pool.length];
}
/* 自动设备名: Android 尽量从 UA / UA-CH 读取具体型号; iOS Safari 不暴露型号,
   只能显示 iPhone/iPad。Chrome 的 UA 缩减会把 Android 型号变成 "K", 因此
   userAgentData.getHighEntropyValues(["model"]) 是更可靠的来源(需安全上下文)。 */
function cleanDeviceModel(raw) {
  let m = String(raw || "").replace(/\s+Build\/.*$/i, "").replace(/\s+wv$/i, "").trim();
  if (!m || /^(K|wv|Mobile|Tablet|Android|Linux)$/i.test(m)) return "";
  // 服务端昵称上限 20 字; 给 "-1234" 后缀留 5 字, 否则对端只看到被截断的型号
  return m.slice(0, 15);
}
function uaAndroidModel() {
  const ua = navigator.userAgent;
  if (!/Android/i.test(ua)) return "";
  const hit = ua.match(/Android\s+[^;)]*;\s*([^;)]+?)(?:\s+Build\/[^;)]*)?(?:\)|;)/i);
  return cleanDeviceModel(hit && hit[1]);
}
/* Android 壳在 WebView 创建时注入 window.CCDevice: 优先读系统里显示的设备名称
   (用户自定义名 / 厂商型号), 比 UA 里的机型代码更接近用户认知。 */
function nativeDeviceName() {
  try {
    const n = window.CCDevice && window.CCDevice.getDeviceName ? window.CCDevice.getDeviceName() : "";
    return cleanDeviceModel(n);
  } catch (e) { return ""; }
}
function autoNickBase(model) {
  if (model) return model;
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
  return `${os}${br}`;
}
const autoNick = ref((() => {
  try {
    const nativeName = nativeDeviceName();
    return localStorage.getItem(LS.nickAuto) || `${autoNickBase(nativeName || uaAndroidModel())}-${Math.floor(1000 + Math.random() * 9000)}`;
  } catch (e) { return cfg.value.defaultNick; }
})());
try {
  nickInput.value = localStorage.getItem(LS.nick) || autoNick.value;
  if (!localStorage.getItem(LS.nickAuto)) localStorage.setItem(LS.nickAuto, autoNick.value);
} catch (e) {}
/* UA-CH 异步补型号: 用户没改过昵称时同步更新输入框与本地缓存, 改过则不动用户设置。 */
(function refineAutoNick() {
  const apply = (model) => {
    const m = cleanDeviceModel(model);
    if (!m) return;
    const old = autoNick.value;
    const suffix = (old.match(/-(\d{4})$/) || [])[1] || String(Math.floor(1000 + Math.random() * 9000));
    const next = `${m}-${suffix}`;
    if (next === old) return;
    autoNick.value = next;
    if (!nickInput.value.trim() || nickInput.value === old) nickInput.value = next;
    try { localStorage.setItem(LS.nickAuto, next); } catch (e) {}
  };
  // 壳内设备名最准确; 没有再退到 UA-CH / UA。
  const nativeName = nativeDeviceName();
  if (nativeName) {
    apply(nativeName);
    return;
  }
  try {
    const ua = navigator.userAgentData;
    if (ua && typeof ua.getHighEntropyValues === "function") {
      ua.getHighEntropyValues(["model"]).then((d) => apply(d && d.model)).catch(() => {});
      return;
    }
  } catch (e) {}
  apply(uaAndroidModel());
})();
function saveNick() {
  try { localStorage.setItem(LS.nick, clampNick(nickInput.value.trim())); } catch (e) {}
}
/* 聊天文本链接化: 先转义防 XSS, 再把 URL 包成可点击链接(桌面走系统浏览器) */
function escHtml(s) { return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;"); }
/* 轻量 Markdown(行内): 行内代码 / [文字](链接) / 裸链接 / 粗体 / 斜体 / 删除线。
   输入已整体转义, 这里只把匹配片段换成自己生成的标签, 内容无法注入 HTML。 */
function mdInline(s) {
  const stash = [];
  const keep = (html) => { stash.push(html); return `\u0000${stash.length - 1}\u0000`; };
  s = s.replace(/`([^`\n]+)`/g, (m0, c) => keep(`<code class="chat-inline">${c}</code>`));
  s = s.replace(/\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/g,
    (m0, t, u) => keep(`<span class="chat-link" data-url="${u}" title="点击跳转到浏览器">${t}</span>`));
  s = s.replace(/(https?:\/\/[^\s<>\u0000]+)/gi,
    (u) => `<span class="chat-link" data-url="${u}" title="点击跳转到浏览器">${u}</span>`);
  s = s.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/(^|[^*\w])\*([^*\n]+)\*/g, "$1<em>$2</em>");
  s = s.replace(/~~([^~\n]+)~~/g, "<del>$1</del>");
  return s.replace(/\u0000(\d+)\u0000/g, (m0, i) => stash[Number(i)]);
}
/* 轻量 Markdown(块级): 围栏代码块 / 标题 / 无序·有序列表 / 引用 / 分割线。
   块级占位符用 \u0001 包裹, 避免与 mdInline 的 \u0000 占位符互相覆盖。 */
function mdChat(raw) {
  let s = escHtml(String(raw));
  const stash = [];
  const keep = (html) => { stash.push(html); return `\u0001${stash.length - 1}\u0001`; };
  // 围栏代码块先摘出来: 内容里的 * # - 等不能被后续规则误伤
  s = s.replace(/```[\w+-]*\r?\n?([\s\S]*?)```/g,
    (m0, code) => keep(`<pre class="chat-code"><code>${code.replace(/\n+$/, "")}</code></pre>`));

  const out = [];
  let para = [];
  let list = null;
  let quote = null;
  const flushPara = () => {
    if (!para.length) return;
    out.push(`<div class="chat-p">${para.map(mdInline).join("<br>")}</div>`);
    para = [];
  };
  const flushList = () => {
    if (!list) return;
    out.push(`<${list.tag} class="chat-list">${list.items.map(i => `<li>${mdInline(i)}</li>`).join("")}</${list.tag}>`);
    list = null;
  };
  const flushQuote = () => {
    if (!quote) return;
    out.push(`<blockquote class="chat-md-quote">${quote.map(mdInline).join("<br>")}</blockquote>`);
    quote = null;
  };
  const flushAll = () => { flushPara(); flushList(); flushQuote(); };

  for (const line of s.split(/\r?\n/)) {
    if (/^\u0001\d+\u0001$/.test(line.trim())) { flushAll(); out.push(line.trim()); continue; }
    const h = line.match(/^\s*(#{1,6})\s+(.*)$/);
    if (h) { flushAll(); out.push(`<div class="chat-h${h[1].length}">${mdInline(h[2])}</div>`); continue; }
    if (/^\s*([-*_])\s*(?:\1\s*){2,}$/.test(line)) { flushAll(); out.push('<hr class="chat-hr">'); continue; }
    const q = line.match(/^\s*(?:>|&gt;)\s?(.*)$/);   // escHtml 已把 > 转成 &gt;
    if (q) { flushPara(); flushList(); if (!quote) quote = []; quote.push(q[1]); continue; }
    const ul = line.match(/^\s*[-*+]\s+(.*)$/);
    if (ul) {
      flushPara(); flushQuote();
      if (list && list.tag !== "ul") flushList();
      if (!list) list = { tag: "ul", items: [] };
      list.items.push(ul[1]);
      continue;
    }
    const ol = line.match(/^\s*\d+[.)]\s+(.*)$/);
    if (ol) {
      flushPara(); flushQuote();
      if (list && list.tag !== "ol") flushList();
      if (!list) list = { tag: "ol", items: [] };
      list.items.push(ol[1]);
      continue;
    }
    if (!line.trim()) { flushAll(); continue; }
    flushList(); flushQuote(); para.push(line);
  }
  flushAll();
  return out.join("").replace(/\u0001(\d+)\u0001/g, (m0, i) => stash[Number(i)]);
}
/* 引用块与正文一起进 v-html: 引用内容同样转义, 只保留一行, 防止长引用撑爆气泡 */
function renderQuote(q) {
  if (!q || !q.message) return "";
  const nick = escHtml(String(q.nickname || ""));
  const text = escHtml(String(q.message)).replace(/\s*\n\s*/g, " ");
  return `<div class="tx-quote"><b>${nick}</b><span>${text}</span></div>`;
}
/* 聊天内链接点击: 注册在组件生命周期内(挂载/卸载成对), 避免热重载时重复监听 */
function onDocChatClick(e) {
  const link = e.target.closest(".chat-link");
  if (!link) return;
  const url = link.dataset.url;
  if (!url) return;
  openExternal(url);
}
/* 清空显示: 仅清本窗口(与 PyQt 镜像清空语义一致); 新消息到达后恢复显示(只显示新消息) */
const clearedTs = ref(0);   // 清空水位: 消息时间戳(服务端封顶 200 条, 用条数做水位在满 200 时永不解除)
function clearChatDisplay() {
  // 水位取最后一条可见消息的时间戳而非条数: 条数 ≤200 恒成立, "length > 水位条数" 永假 → 满 200 条时
  // 点清空后本机永久空白; 时间戳在服务端重启(消息清零)后依然单调可比。
  // 已处于清空态(可见列表为空)时不得把水位覆盖成 0, 否则重复点清空丢水位、历史整段回归
  // 文件投递事件也参与水位: 否则清空显示后文件卡片仍留在时间线
  const lastChat = chatMsgs.value.length ? chatMsgs.value[chatMsgs.value.length - 1].timestamp : 0;
  const lastFile = fileEvents.value.reduce((mx, f) => Math.max(mx, f.timestamp || 0), 0);
  clearedTs.value = Math.max(clearedTs.value, lastChat, lastFile);
  // 必须立即清显示列表: WS 在线时 loadChat 只由"新消息事件"触发, 安静的聊天室里
  // 清空后 loadChat 永远不来, 只设标记会表现为"点了清空没反应"(轮询时代的回归)
  chatMsgs.value = [];
  directReceipts.value = [];
  showToast("已清空数据（不影响其他设备与服务端记录）");
}
let loadingChat = false;
let reloadChatQueued = false;
let chatRecoveredOffline = false;   // 断网恢复后首拉: 历史消息不应全部计为未读
async function loadChat() {
  // 在途互斥 + 补拉(与 loadFiles 对齐): 刷屏时每个 chat:new 都触发全量拉取,
  // 后发先至的旧响应会覆盖新数据 → 消息回闪
  if (loadingChat) { reloadChatQueued = true; return; }
  loadingChat = true;
  const epoch = sessionEpoch;
  // 超时保护: 无 AbortController 时单个挂死请求会让 loadingChat 恒 true,
  // 互斥把影响面从"单次"放大为"聊天停更直至浏览器默认超时(~5min)"
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), 10000);
  try {
    const r = await apiFetch(apiBase.value + API.chat + "?nick=" + encodeURIComponent(deviceNick()), { signal: ctl.signal });
    if (!r.ok) throw new Error(`消息加载失败 (${r.status})`);
    // 超时必须覆盖响应体读取: 半开 Wi-Fi 下 r.json() 会挂到浏览器底层超时,
    // 期间 loadingChat 恒 true, WS 触发的补拉全部只排队不执行(聊天停更)。
    const d = await r.json();
    if (epoch !== sessionEpoch) {
      loadingChat = false;
      if (reloadChatQueued) { reloadChatQueued = false; loadChat(); }
      return;
    }
    const msgs = d.messages || [];
    // 水位过滤是"已清空历史不再复现"的唯一保证: 旧实现用 chatCleared 布尔开关, 新消息一到
    // 开关即解除, 全量 msgs 覆盖显示 → 之前清掉的历史整段回来。过滤后新消息正常出现,
    // 水位之下(已清空)的永远不再进入本窗口; clearedTs 初始为 0, 未清空时全员放行
    // 内容没变就不替换数组: 轮询/WS 重复拉取同一批消息时,
    // 旧实现每次都重建 list → 整条时间线重渲染, 输入时光标被反复打断。
    // 签名必须覆盖完整 id 列表: 只比"条数+末条 id"时, 服务端删除末条后
    // 新消息复用同一 id, "删除+新增"在一次拉取内完成会被误判为没变化。
    const prevLastId = chatMsgs.value.length ? chatMsgs.value[chatMsgs.value.length - 1].id : 0;
    const visible = msgs.filter((m) => m.timestamp > clearedTs.value);
    const sig = visible.map((m) => m.id).join(",") + "|" + clearedTs.value;
    if (sig !== loadChat._sig) {
      loadChat._sig = sig;
      chatMsgs.value = visible;
    }
    // 文件投递事件(持久化): 内容不变就不替换数组, 避免 3s 轮询反复重渲染时间线
    const files = Array.isArray(d.files) ? d.files : [];
    const filesSig = files.map((f) => `${f.name}:${f.timestamp}:${f.size}`).join("|");
    if (filesSig !== loadChat._filesSig) {
      loadChat._filesSig = filesSig;
      fileEvents.value = files;
    }
    chatCount.value = msgs.length;
    const firstLoad = !loadChat._init;
    if (isQrPetWindow && petActivity.observeMessages(msgs)) markPetNotify();
    // 首次拉取把已有历史视为已读: 后台自启/刚打开页面时不应顶着 99+ 未读
    if (firstLoad) {
      markChatSeen();   // 被设备/文件页或弹层遮住时不算已读
    // 断网恢复: 失联期间离线清空了本地计数, 恢复首拉若按全量算未读会瞬间冲到 99+ ——
    // 把恢复瞬间的历史视为已读, 之后的新消息照常计数(仍受遮罩判断约束)
    } else if (chatRecoveredOffline) {
      chatRecoveredOffline = false;
      markChatSeen();
    } else if (atBottom.value) {
      markChatSeen();
    } else {
      // 用户上翻看历史: 新消息不能被静默标已读, 计入胶囊与未读角标
      const fresh = msgs.filter((m) => m.id > prevLastId).length;
      if (fresh > 0) jumpCount.value += fresh;
    }
    // 滚动判据用"最后一条消息 id"而非条数: 服务端封顶 200 条后条数恒定,
    // 旧判据 (msgs.length !== old) 永假 —— 聊天满 200 条后新消息不再自动滚到底
    const lastSig = msgs.length ? msgs[msgs.length - 1].id : 0;
    if (lastSig !== loadChat._lastSig) {
      loadChat._lastSig = lastSig;
      // 用户上翻看历史时不能被强制拽到底: 只有贴底或首拉才跟随;
      // 未读胶囊已在上面按实际新增条数累加, 这里不再重复计数
      if (firstLoad || atBottom.value) nextTick(scrollChats);
    } else if (!loadChat._init) {
      nextTick(scrollChats);
    }
    loadChat._init = true;
  } catch (e) {
    console.error(e);
    // WS 在线时轮询已停：一次事件触发的加载失败不能永久留下旧时间线。
    setTimeout(() => { if (ws && ws.readyState === WebSocket.OPEN) loadChat(); }, 2000);
  }
  finally { clearTimeout(timer); }
  loadingChat = false;
  if (reloadChatQueued) { reloadChatQueued = false; loadChat(); }
}
/* 回车发送需豁免输入法组字: 中文 IME 组字期间的 Enter 是"确认候选词",
   isComposing/keyCode 229 时不得触发发送 —— 否则把未上屏的拼音串当消息发出去 */
function onChatEnter(e) {
  if (e.isComposing || e.keyCode === 229) return;
  // 触屏: 回车换行(点发送按钮提交); 桌面: 回车发送, Shift+回车换行
  if (touchOnly || e.shiftKey) return;
  e.preventDefault();
  sendChat();
}
const sending = ref(false);
const quoting = ref(null);   // 引用中的消息快照: { id, nickname, message } | null
const activeMsgKey = ref("");   // 浏览器/触屏: 点击气泡展开操作按钮(桌面壳走 hover)
function onMsgClick(it) {
  if (isRealDesktop) return;   // 桌面壳用悬停, 点击不锁操作条
  activeMsgKey.value = activeMsgKey.value === it.key ? "" : it.key;
}
function startQuote(m) {
  if (!m) return;
  quoting.value = {
    id: m.id,
    nickname: m.nickname || "",
    message: String(m.message || "").slice(0, 200),
  };
  nextTick(() => { const el = document.querySelector(".tx-input"); if (el) el.focus(); });
}
function copyMessage(m) {
  if (!m) return;
  const key = `m${m.id}`;
  copyText(String(m.message || ""), "消息已复制").then((ok) => {
    if (!ok) return;
    copiedMsgKey.value = key;
    clearTimeout(copiedMsgTimer);
    copiedMsgTimer = setTimeout(() => {
      if (copiedMsgKey.value === key) copiedMsgKey.value = "";
    }, 1500);
  });
}
async function deleteMessage(m) {
  if (!m || offlineGuard()) return;
  const yes = await appConfirm({
    title: "删除消息",
    message: "删除这条消息？所有设备都将看不到它。",
    confirmText: "删除", danger: true, icon: "trash",
  });
  if (!yes) return;
  try {
    const r = await apiFetch(apiBase.value + API.chatDelete(m.id), { method: "DELETE" });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(d.error || "删除失败");
    loadChat();
    showToast("消息已删除");
  } catch (e) {
    showToast(e.message || "删除失败", "error");
  }
}
function _fillChatText(text, msg = "已粘贴剪贴板文本") {
  chatInput.value = clampChars(text, cfg.value.msgMax);
  showToast(msg);
  nextTick(() => { const el = document.querySelector(".tx-input"); if (el) el.focus(); });
}
/* 原生剪贴板图片 → PNG File: 插件给的是 RGBA 原始像素, 经 canvas 编码后才能上传 */
async function _pasteNativeImage() {
  const img = await readClipboardImage();
  if (!img) return false;
  const size = await img.size();
  if (!size || !size.width || !size.height) return false;
  const rgba = await img.rgba();
  const canvas = document.createElement("canvas");
  canvas.width = size.width;
  canvas.height = size.height;
  const ctx = canvas.getContext("2d");
  if (!ctx) return false;
  ctx.putImageData(new ImageData(new Uint8ClampedArray(rgba), size.width, size.height), 0, 0);
  const blob = await new Promise((res) => canvas.toBlob(res, "image/png"));
  if (!blob) return false;
  handleIncomingFiles([new File([blob], `粘贴图片-${Date.now()}.png`, { type: "image/png" })]);
  showToast("已粘贴剪贴板图片");
  return true;
}
/* 点「粘贴」: 先看剪贴板有没有文本, 再看有没有图片/文件; 都没有才提示。
   桌面/移动壳走原生剪贴板插件 —— 自定义协议下 WebView 的 Clipboard API 常被拒绝;
   浏览器仅在安全上下文(https/localhost)才有 Clipboard API, http 局域网地址读不到。 */
/* 唯一「粘贴」入口: 本机剪贴板优先; 本机读不到(HTTP 局域网页)时
   直接回填另一端共享的文本, 不弹面板。 */
async function pasteSharedClipToInput() {
  if (serviceDown()) return false;
  await loadSharedClip();
  const t = (sharedClip.value.text || "").trim();
  if (!t) return false;
  _fillChatText(t, "已粘贴电脑剪贴板");
  return true;
}
async function pasteFromClipboard() {
  plusOpen.value = false;
  if (isDesktop) {
    try {
      const text = await readClipboardText();
      if (text && text.trim()) {
        _fillChatText(text);
        if (clipSyncEnabled.value) publishSharedClip(text, true);   // 仅在自动同步开启时回传
        return;
      }
    } catch (_) {}
    try { if (await _pasteNativeImage()) return; } catch (_) {}
    if (!(await pasteSharedClipToInput())) showToast("剪贴板为空，先复制一段文字", "error");
    return;
  }
  const local = await readLocalClipboardText();
  if (local.trim()) {
    _fillChatText(local);
    publishSharedClip(local, true);
    return;
  }
  // 浏览器 HTTP 下读不到系统剪贴板: 直接回填电脑端共享文本
  if (!(await pasteSharedClipToInput())) showToast("剪贴板为空，请长按输入框选择「粘贴」", "error");
}
async function sendChat() {
  if (sending.value) return;   // 连按回车/连点发送只产生一条消息
  const text = chatInput.value.trim();
  if (!text) return;
  // 前端拦一次(保留草稿); 服务端还会按同一上限再校验并返回 400
  const chars = Array.from(text).length;
  if (chars > cfg.value.msgMax) {
    showToast(`消息超过 ${cfg.value.msgMax} 字符上限（当前 ${chars}），请删减后再发送`, "error");
    return;
  }
  // 目标设备: 直发, 不进入房间时间线, 只追加本机回执; 多选时依次发往每台
  if (sendTargets.value.length) {
    saveNick();
    sending.value = true;
    try {
      const targets = sendTargets.value.slice();
      let ok = 0;
      for (const p of targets) {
        // 失败即停: 多台设备同时缺配对码时, 继续发下一台会覆盖正在输入的
        // 配对码弹框; 先让用户处理失败设备, 再重试剩余目标。
        if (!(await directSendText(p, text))) break;
        ok++;
      }
      if (ok) {
        chatInput.value = "";
        quoting.value = null;
        atBottom.value = true;
        jumpCount.value = 0;
        nextTick(scrollChats);
        setTimeout(scrollChats, 150);
      }
      if (ok < targets.length) {
        showToast(`已发送到 ${ok}/${targets.length} 台设备`, "error");
      }
    } finally {
      sending.value = false;
    }
    return;
  }
  const nickname = curNick.value;
  const quote = quoting.value;
  saveNick();
  sending.value = true;
  chatInput.value = "";        // 立即清空: 响应在途期间不允许重复提交
  try {
    const r = await apiFetch(apiBase.value + API.chat + "?nick=" + encodeURIComponent(nickname), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ nickname, message: text, ...(quote ? { quote } : {}) }),
    });
    const d = await r.json();
    if (d.ok) {
      quoting.value = null;
      atBottom.value = true;
      jumpCount.value = 0;
      loadChat();
      // 发送后立即钉到最新一条: 消息渲染/WS 回拉/移动端键盘动画都会改变高度, 多补两次
      nextTick(scrollChats);
      setTimeout(scrollChats, 150);
      setTimeout(scrollChats, 450);
    } else {
      if (!chatInput.value) chatInput.value = text;   // 失败回填
      showToast(d.error || "发送失败", "error");
    }
  } catch (e) {
    if (!chatInput.value) chatInput.value = text;
    showToast("发送失败", "error");
  } finally {
    sending.value = false;
  }
}

/* ---------- 提示 ---------- */
const toasts = ref([]);
let toastSeq = 0;
function showToast(msg, type = "success", action = null) {
  const id = ++toastSeq;
  toasts.value.push({ id, msg, type, action });
  if (toasts.value.length > 5) toasts.value.splice(0, toasts.value.length - 5);
  // 带操作(如"撤销")的提示停留更久, 给用户反应时间
  setTimeout(() => { toasts.value = toasts.value.filter(t => t.id !== id); }, action ? 4000 : TOAST_MS);
}
function runToastAction(t) {
  try { t.action && t.action.fn(); } catch (e) {}
  toasts.value = toasts.value.filter(x => x.id !== t.id);
}

/* ---------- 软件介绍页(桌面端全屏 overlay, 设计稿见 docs/软件介绍页设计稿.html) ---------- */
const introSec = ref("feat");   // scroll spy 当前所在区块
const INTRO_FAQS = [
  { q: "手机需要安装 App 吗？", a: "不用。同一 Wi-Fi 下扫码或输入电脑地址，浏览器即可上传、下载、聊天。Android 也可安装 APK，作为独立本机节点；安装包已修复 versionCode/versionName，可正常覆盖升级。" },
  { q: "文件会上传到外网吗？", a: "不会。服务只监听你的电脑/手机，所有传输都在局域网内，不经过第三方服务器；断外网也能用。" },
  { q: "关闭窗口后服务会停吗？", a: "不会。关窗只是收进托盘，服务继续运行；真正退出请走托盘菜单。Android 用 connectedDevice 前台服务 + Wi-Fi/CPU 锁保活，并在首次启动申请忽略电池优化。" },
  { q: "支持哪些平台？", a: "macOS、Windows、Android App，以及电脑/手机浏览器。Windows 安装包内置 WebView2 离线安装器，离线也能装；iOS 暂无原生包，用 Safari 打开电脑地址即可。" },
  { q: "能同时发给多台设备吗？", a: "可以。设备页/设备栏点「多选」，勾选多台后发送文本或拖入文件，会依次投递到每台设备；某台失败会停下并提示，避免配对框互相覆盖。" },
  { q: "文件消息刷新后会丢吗？", a: "不会。上传成功的文件会记录为本机服务的文件消息（最近 200 条），刷新浏览器或重启 App 后聊天时间线仍保留；删除文件时卡片同步消失。" },
  { q: "文件列表在哪里？", a: "桌面点房间头文件夹图标，手机 App 点底部「文件」，手机浏览器点顶部文件夹。上传的文件也会同时出现在聊天里。" },
  { q: "能直接预览文件吗？", a: "图片、PDF、文本和音视频可在应用内预览；Office、压缩包等需下载后查看。html/svg/xml 为安全起见不以网页打开。" },
  { q: "设备发现不到怎么办？", a: "确认两台设备在同一 Wi-Fi/子网；macOS 允许「本地网络」，Windows 防火墙放行 UDP 53318-53321；手机熄屏后要能被发现，请允许忽略电池优化并保持 App 后台运行；仍不行可在故障诊断里按 IP 单播探测，或直接手动发送。" },
  { q: "共享剪贴板怎么用？", a: "默认关闭；开启后才与打开本机地址的页面/设备共享文本，聊天框粘贴才会回传。关闭时粘贴只填充输入框，不会把未发送内容写进共享槽。电脑 App 与手机 App 独立，跨 App 请用发送面板「剪贴板」投递。" },
  { q: "访问令牌可以关闭吗？", a: "可以。手机设置页可随时关闭/重新开启访问令牌，关闭前会二次确认风险；关闭后同 Wi-Fi 设备无需配对码即可读写，公共网络不建议关闭。" },
  { q: "更新检查会提示未签名版本吗？", a: "未签名/未公证的 Release 会公开发布但标记为 prerelease，应用内更新检查会跳过，避免把 Gatekeeper/SmartScreen 拦截的包当正式更新推送。" },
  { q: "访问令牌和分享链接安全吗？", a: "令牌是 6 位 CSPRNG 配对码，失败按来源 IP 退避；分享链接一次性、10 分钟有效，文件删除或重传后立即失效。缩略图/预览使用短时媒体令牌。" },
  { q: "一条消息最多多长？", a: "2 万字符（按 Unicode 字符计）。聊天保留最近 200 条，超出时从最旧消息开始淘汰。" },
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
function goTransferFromIntro() { closeIntro(); setView("transfer"); }
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
  if (mobileShell) {
    // 移动壳 WebView 无多窗/无下载通道: 交系统浏览器打开(安卓 am / iOS 复制兜底)
    tauriInvoke("open_url_cmd", { url })
      .then((d) => {
        if (d && d.ok) { if (!silent) showToast("已在系统浏览器打开"); }
        else copyText(url, "已复制链接，请在浏览器中打开");
      })
      .catch(() => copyText(url, "已复制链接，请在浏览器中打开"));
    return;
  }
  if (!isRealDesktop) {
    window.open(url, "_blank", "noopener,noreferrer");
    return;
  }
  tauriInvoke("open_url_cmd", { url })
    .then((d) => {
      if (d.ok) { if (!silent) showToast("已在系统浏览器打开"); }
      else {
        if (!silent) showToast(d.error || "打开失败，尝试备用方式", "error");
        window.open(url, "_blank", "noopener,noreferrer");
      }
    })
    .catch(() => { window.open(url, "_blank", "noopener,noreferrer"); });
}

/* ---------- WebSocket 推送(聊天/列表/状态), 轮询降级为兜底通道 ---------- */
let ws = null;
let wsUrl_ = null;
let wsRetryTimer = null;
let wsRetryDelay = 1000;
let wsWatchdogTimer = null;
let lastWsMsg = 0;   // 最近一次收到 WS 消息(含服务端 15s 心跳)的时间戳
function wsEndpoint() {
  // 移动壳: 连电脑的 WS(地址同 apiBase); withPin: 访问码开启后非回环握手同样要校验
  if (mobileShell) return serverAddr.value ? withPin(`ws://${serverAddr.value}${API.ws}`) : null;
  if (isDesktop) return port.value ? `ws://127.0.0.1:${port.value}${API.ws}` : null;
  return withPin((location.protocol === "https:" ? "wss://" : "ws://") + location.host + API.ws);
}
function scheduleWsRetry() {
  clearTimeout(wsRetryTimer);
  // 桌面端服务已停时停止重试, 等 running 变化再连; 网页端持续退避重试(服务回来自动恢复)
  if (isDesktop && !running.value) return;
  // 配对弹框挂起时停止重试: 陈旧/错误配对码的每次握手都会被服务端记一次失败,
  // 10s 退避重试几分钟就把该 IP 烧进锁定 —— submitPin 成功路径会主动 connectWs
  if (needPin.value) return;
  wsRetryTimer = setTimeout(() => {
    wsRetryDelay = Math.min(wsRetryDelay * 2, 10000);
    connectWs();
  }, wsRetryDelay);
}
function closeWs(applyMode = true) {
  clearTimeout(wsRetryTimer);
  wsRetryTimer = null;
  // 注意: 这里不能重置 wsRetryDelay —— connectWs() 每次都会先 closeWs(), 在这里
  // 重置会把退避永久摁回 1s, 断网时变成每秒重连一次的"重连风暴"。
  // 退避只在连接成功(onopen)时复位。
  if (ws) {
    const w = ws;
    ws = null;
    w.onopen = w.onmessage = w.onclose = w.onerror = null;
    try { w.close(); } catch (_) {}
  }
  wsUrl_ = null;
  if (applyMode) applyWsMode();
}
function connectWs() {
  const url = wsEndpoint();
  if (!url) return;
  if (ws && wsUrl_ === url) {
    // OPEN/CONNECTING 才允许复用; CLOSING/CLOSED 的旧 socket 必须替换,
    // 否则半开看门狗 close() 后 connectWs 会永远直接 return。
    if (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING) return;
    closeWs(false);
  } else {
    closeWs();                        // 地址变化(换端口)先清场
  }
  wsUrl_ = url;
  try { ws = new WebSocket(url); } catch (_) { scheduleWsRetry(); return; }
  ws.onopen = () => {
    wsRetryDelay = 1000;
    lastWsMsg = Date.now();
    applyWsMode();
    // 服务端重启会清空内存票据: 重连即作废本地缓存, 避免分享出死链
    clearShareTicketCache();
    loadChat();     // 连接即全量同步一次, 补上断线窗口期
    loadFiles();
    loadSharedClip();
    sendDiscoveryProbe();   // 重连后补一次主动探测, 覆盖断线期间新上线的设备
  };
  ws.onmessage = (e) => {
    lastWsMsg = Date.now();   // 任何消息(含 hb 心跳)都证明连接活着
    let ev;
    try { ev = JSON.parse(e.data); } catch (_) { return; }
    if (ev.type === "chat:new") {
      markPetNotify();
      loadChat();
    } else if (ev.type === "chat:changed") {
      // 删除消息: 只重拉, 不点新消息提醒
      loadChat();
    } else if (ev.type === "devices:presence") {
      // 服务端在每条有效 UDP 宣告后直接推送完整列表；不再等 HTTP 轮询。
      if (isDesktop && Array.isArray(ev.devices)) peers.value = ev.devices;
    } else if (ev.type === "devices:changed") {
      if (isDesktop) refreshPeers();
    } else if (ev.type === "clipboard:changed") {
      loadSharedClip().then(() => {
        // App 壳: 本机服务的共享槽变化后写回本机系统剪贴板
        applyRemoteClipboard();
        // 打开着剪贴板面板时同步草稿, 但不打断正在编辑的内容
      });
    } else if (ev.type === "sync:needed") {
      // 服务端广播积压导致事件被丢弃: WS 在线时轮询是停的, 必须立即补一次全量
      loadChat(); loadFiles(); refreshPeers();
      loadSharedClip().then(applyRemoteClipboard);
    } else if (ev.type === "files:changed") {
      // 上传完成直接点亮，不依赖列表基线/HTTP 拉取；删除也发 files:changed，需区分。
      if (ev.action === "uploaded") {
        markPetNotify();
        // 别人上传且抽屉/面板关着时，在文件夹按钮上点绿点；自己的上传不点
        if (!filePanelVisible() && Date.now() - lastSelfUploadAt > 3000) fileDot.value = true;
      }
      loadFiles();
    }
    else if (ev.type === "status:up" || ev.type === "status:down") refreshStatus();
  };
  ws.onclose = () => { ws = null; wsUrl_ = null; applyWsMode(); scheduleWsRetry(); };
  ws.onerror = () => { try { ws && ws.close(); } catch (_) {} };
}
// WS 在线时停掉聊天/列表轮询(省电省请求); 断开时恢复轮询兜底
let wsLiveState = null;   // null = 尚未初始化; 只有状态真正翻转才重排计时器
function applyWsMode() {
  const live = !!(ws && ws.readyState === WebSocket.OPEN);
  // 断线重连会按退避不断 closeWs→applyWsMode; 旧实现每次都 clear+setInterval,
  // 1s/2s/4s/8s 的重连节奏永远等不到 5s 的文件轮询 —— WS 连不上时列表/聊天停更。
  // 状态没翻转就不动计时器, 让既有间隔正常跑满。
  if (live === wsLiveState) return;
  wsLiveState = live;
  clearInterval(chatTimer); chatTimer = null;
  clearInterval(filesTimer); filesTimer = null;
  if (!live) {
    chatTimer = setInterval(loadChat, cfg.value.poll.chat);
    filesTimer = setInterval(() => loadFiles(false), cfg.value.poll.files);
  }
}
// 服务启停/端口变化时管理连接
watch([running, port, serverAddr], () => {
  // 悬浮码窗已关闭(隐藏)时完全不建立 WS: 隐藏的 96px WebView 不该持续拉聊天/文件
  if (isQrPetWindow && !qrFloatEnabled.value) { closeWs(false); stopClipboardSync(); return; }
  if (running.value && port.value) {
    connectWs();
    // 桌面壳的 apiBase 依赖实际端口: 启动瞬间 port 还是默认值/0, 首拉可能失败;
    // 状态接口拿到真实端口后补拉一次, 不必等 5s 轮询或 WS 建连
    if (isDesktop && view.value === "transfer") { loadFiles(false); loadChat(); }
    // App 壳双向剪贴板: 服务启停/换端口/换电脑后重建基线
    if (isDesktop && !isQrPetWindow) restartClipboardSync();
  } else { closeWs(); stopClipboardSync(); }
});
// Pet 显隐只由开关决定(服务停止也常驻圆点, 用状态点表达运行/停止)。
// petPrefsReady: 开关权威值从 Rust pref 文件异步加载, 加载完才放行显隐 —— 否则启动时
// 会先按 localStorage 陈旧缓存闪一下错误状态(如上次关了但缓存还是开)。
let petPrefsReady = !isQrPetWindow;   // 主窗不负责 Pet 显隐, 无需等待
async function syncPetVisibility() {
  if (!isQrPetWindow || !petPrefsReady) return;
  // 首次落位 Promise 只在 onMounted 的 Pet 初始化块里创建: setup 阶段的 immediate
  // watch 若先跑, petPlaced 还是 null, 旧逻辑会直接 show() 到 Rust 默认位。等挂载后
  // 的同步调用(预制加载完成处)再显示
  if (qrFloatEnabled.value && !petPlaced) return;
  const win = getCurrentWebviewWindow();
  if (qrFloatEnabled.value) {
    // 隐藏→显示且上次停在展开态: 先收回圆点 —— 指针并不在窗内, 不会自己触发 leave 收起
    try {
      if (!(await win.isVisible()) && qrPetExpanded.value) await setPetExpanded(false);
    } catch (e) {}
    // 等首次落位完成再显: Rust 建窗时落在主屏默认位, 先 show 会从主屏闪一下
    // 再跳到记忆屏(扩展屏场景尤其明显)。petPlaced 有 2.5s 兜底, 不会永远不显示。
    if (petPlaced) await petPlaced;
    // 异步等待期间用户可能已关闭悬浮码；迟到的 show 不能再把窗口拉回来。
    if (!qrFloatEnabled.value) { win.hide(); return; }
    win.show();
  } else win.hide();
}
watch([running, qrData, qrFloatEnabled], () => { syncPetVisibility(); }, { immediate: true });
// 悬浮码关闭 = Pet 窗隐藏: 停止 WS/聊天/文件轮询, 否则一个看不见的小窗仍持续耗电耗连接。
// 重新开启时恢复(地址/配对态不变, 直接重连即可)
if (isQrPetWindow) {
  watch(qrFloatEnabled, (v) => {
    if (!petPrefsReady) return;
    if (v) { petStartTimers(); startPolling(); refreshStatus(); connectWs(); }
    else { petStopTimers(); stopPolling(); closeWs(false); }
  });
}

watch(running, (value) => {
  if (!isQrPetWindow) return;
  syncQr();
});

/* ---------- 生命周期 ---------- */
let chatTimer = null;
let logTimer = null;
let filesTimer = null;
let statusTimer = null;
let unlistenDl = null;   // 壳内下载完成事件的反注册
let unlistenQrFloat = null;   // 悬浮码开关跨窗同步的反注册
let setupDisposed = false;   // 卸载后落地的 .then(unlisten) 立即反注册, 防热重载监听累积
/* 在 onMounted 内注册的清理函数: onUnmounted 只能在 setup 同步阶段注册,
   在 onMounted 回调里调用会被 Vue 丢弃(无活动实例), HMR 后监听叠加 */
const mountedCleanups = [];
function onMountedCleanup(fn) { mountedCleanups.push(fn); }
/* 轮询定时器统一启停: 服务端下发间隔变化时重排; 聊天/列表由 applyWsMode 按 WS 状态增减 */
function startPolling() {
  stopPolling();
  statusTimer = setInterval(refreshStatus, cfg.value.poll.status);   // 失联判定 ≈ OFFLINE_FAILS × 间隔
  if (isRealDesktop) logTimer = setInterval(refreshLogs, cfg.value.poll.logs);
  applyWsMode();
}
function stopPolling() {
  clearInterval(chatTimer);
  clearInterval(logTimer);
  clearInterval(statusTimer);
  clearInterval(filesTimer);
  wsLiveState = null;   // 计时器已被清空: 下次 applyWsMode 必须重新排程
}
onMounted(() => {
  if (PREVIEW_MOBILE) {
    const tab = _previewQ.get("tab");
    if (tab) setMobileTab(tab);
    const mode = _previewQ.get("mode");
    if (mode) setThemeMode(mode);
    if (_previewQ.get("intro") === "1") openIntro();
  }
  // 移动端时间线窗口起点: 必须在挂载后注册 —— setup 阶段 watch 会立即求值
  // timelineItems, 而 chatMsgs/clearedTs 在后面才定义(TDZ 会导致整页白屏)。
  watch([timelineItems, atBottom, isNarrow], () => {
    if (!isNarrow.value) { timelineStart.value = 0; return; }
    const maxStart = Math.max(0, timelineItems.value.length - MOBILE_TIMELINE_MAX);
    if (atBottom.value || timelineStart.value > maxStart) timelineStart.value = maxStart;
  }, { immediate: true });
  // 主窗首帧就绪信号: Rust 收到 main://ready 才 show —— 消灭「空壳先出、内容后蹦」
  // (mounted 时 DOM 已插入, show 后下一帧即绘制; webview 加载本不依赖 show)。
  // pet 窗有自己的 petPlaced 门控; 自启动 --hidden 模式 Rust 侧不监听, 收到也无害。
  if (isDesktop && !isQrPetWindow) {
    try { emit("main://ready").catch(() => {}); } catch (e) {}
  }
  document.addEventListener("paste", onPaste);
  document.addEventListener("contextmenu", onContextMenu);
  document.addEventListener("click", closeContextMenu);
  window.addEventListener("scroll", closeContextMenu, true);
  window.addEventListener("resize", closeContextMenu);
  document.addEventListener("click", onDocChatClick);
  // 点击 ＋ 面板以外的空白区域也收起(面板内按钮与 ＋ 自身除外)
  const onDocClosePlus = (e) => {
    if (!plusOpen.value) return;
    const t = e.target;
    if (t && t.closest && t.closest(".tx-plus-panel, .tx-plus")) return;
    plusOpen.value = false;
  };
  document.addEventListener("click", onDocClosePlus);
  onMountedCleanup(() => document.removeEventListener("click", onDocClosePlus));
  // 合并页未读：回到窗口/标签页时，若传输页可见则立即追平已读
  const onSeenSignal = () => {
    markChatSeen();
    if (document.visibilityState !== "visible") return;
    // 手机浏览器切后台会冻结定时器/留下半开 WS: 回到前台立即补拉一次,
    // 不等 3s 轮询或重连退避 —— 否则"桌面发的消息手机没收到"要等很久
    if (!isRealDesktop) { loadChat(); loadFiles(false); }
    if (!ws || ws.readyState !== WebSocket.OPEN) connectWs();
    // 系统会在页面隐藏时释放 Wake Lock/暂停保活视频: 回到前台且仍有上传时重新申请
    updateWakeLock().catch(() => {});
  };
  window.addEventListener("focus", onSeenSignal);
  document.addEventListener("visibilitychange", onSeenSignal);
  onMountedCleanup(() => {
    window.removeEventListener("focus", onSeenSignal);
    document.removeEventListener("visibilitychange", onSeenSignal);
  });
  // 视口变化(软键盘收放/手机地址栏折叠/横竖屏): 贴底时重新钉到最新消息
  // iOS Safari: 键盘/底部工具栏会改变 visualViewport 的高度和偏移。
  // fixed 布局必须同时跟随 offsetTop, 否则键盘弹出时输入框会被顶出可视区(输入框消失)。
  // 注意: iOS 地址栏伸缩/键盘动画会连续触发 resize+scroll, 这里用 rAF 合并,
  // 且只在高度/偏移真正变化时写 CSS 变量 —— 每帧写变量会让整页重排, 滚动发涩。
  let vvRaf = 0, lastVh = -1, lastVvTop = -1;
  const applyViewportVars = () => {
    const vv = window.visualViewport;
    const h = Math.round((vv && vv.height) || window.innerHeight);
    // 手机浏览器(尤其 vivo/微信)在地址栏伸缩时会给出非零 visualViewport.offsetTop；
    // 键盘关闭时若把它当作 app 的 top，会在页头之上留下一整条空白。
    // 只有可视高度明显缩小(键盘)才采用 offsetTop。
    const keyboardOpen = !!(vv && vv.height < window.innerHeight * 0.75);
    const top = keyboardOpen ? Math.round(vv.offsetTop || 0) : 0;
    if (h !== lastVh) {
      lastVh = h;
      document.documentElement.style.setProperty("--app-vh", h + "px");
    }
    if (top !== lastVvTop) {
      lastVvTop = top;
      document.documentElement.style.setProperty("--app-vv-top", top + "px");
    }
  };
  const syncAppViewportHeight = () => {
    // 移动壳(isDesktop=true, isRealDesktop=false)同样需要: 其 WebView 与手机 Safari
    // 一样可能是"键盘覆盖视口而不 resize", 跳过会导致输入栏被键盘遮住。
    // 桌面窄窗也同步一次 --app-vh, 供发送面板等 fixed 层使用。
    if (isRealDesktop) { applyViewportVars(); return; }
    if (vvRaf) return;
    vvRaf = requestAnimationFrame(() => {
      vvRaf = 0;
      applyViewportVars();
      // 视口变化后如仍在底部, 重新贴底(键盘弹出时输入栏上移)
      if (atBottom.value) scrollChats();
    });
  };
  const onResize = () => {
    // 软键盘弹出(可视高度骤减): 收起加号/更多浮层, 把空间还给聊天区
    const vv = window.visualViewport;
    if (isNarrow.value && vv && vv.height < window.innerHeight * 0.75) {
      plusOpen.value = false;
      moreOpen.value = false;
    }
    syncAppViewportHeight();
  };
  applyViewportVars();   // 首帧同步落地, 避免先按默认值画一帧
  window.addEventListener("resize", onResize);
  if (window.visualViewport) {
    window.visualViewport.addEventListener("resize", onResize);
    window.visualViewport.addEventListener("scroll", onResize);
  }
  onMountedCleanup(() => {
    window.removeEventListener("resize", onResize);
    if (window.visualViewport) {
      window.visualViewport.removeEventListener("resize", onResize);
      window.visualViewport.removeEventListener("scroll", onResize);
    }
  });
  // 壳内下载完成通知(Rust on_download 落盘后 emit): 提示保存位置
  if (isDesktop) {
    listen("download://done", (e) => {
      const p = e.payload || {};
      p.success ? showToast("已下载到 " + (p.path || "下载目录")) : showToast("下载失败", "error");
    }).then((un) => { if (setupDisposed) un(); else unlistenDl = un; });
  }
  // WS 半开连接看门狗: 超时收不到任何消息(含服务端心跳) → 强制关闭,
  // onclose 恢复轮询兜底并退避重连 —— 桌面端状态轮询走 invoke 永远成功, 无此看门狗会永久卡死
  wsWatchdogTimer = setInterval(() => {
    if (ws && ws.readyState === WebSocket.OPEN && Date.now() - lastWsMsg > WS_HB_TIMEOUT_MS) {
      // 半开连接 often 停在 CLOSING 且永远不触发 onclose: 必须主动清掉旧 socket、
      // 恢复轮询并排下一次退避重连, 不能只调用 ws.close()。
      closeWs(false);
      applyWsMode();
      scheduleWsRetry();
    }
  }, WS_WATCHDOG_MS);
  // F5 刷新文件列表(输入框聚焦时除外)—— 仅桌面壳劫持; 浏览器端用户按 F5 期望整页重载
  const f5 = (e) => {
    if (isDesktop && e.key === "F5" && !/INPUT|TEXTAREA/.test(document.activeElement?.tagName || "")) {
      e.preventDefault(); loadFiles(true);
    }
  };
  document.addEventListener("keydown", f5);
  onMountedCleanup(() => document.removeEventListener("keydown", f5));
  // 分享票据预取: 悬停到分享按钮即开票(见 prefetchShareTicket)
  document.addEventListener("pointerover", onDocShareHover, true);
  onMountedCleanup(() => document.removeEventListener("pointerover", onDocShareHover, true));
  // Esc 关闭最上层弹层: 预览 > 附近发送 > 配对 > 二维码 > 删除 > 昵称 > 介绍
  const escLayers = (e) => {
    if (e.key !== "Escape") return;
    if (contextMenu.value) { closeContextMenu(); return; }
    if (diagnosticsOpen.value) { closeDiagnostics(); return; }
    if (manualSendOpen.value) { closeManualSend(); return; }
    if (previewName.value) { closePreview(); return; }
    if (moreOpen.value) { moreOpen.value = false; return; }
    if (isNarrow.value ? fileSheetOpen.value : !drawerCollapsed.value) {
      if (isNarrow.value) fileSheetOpen.value = false; else drawerCollapsed.value = true;
      return;
    }
    if (needPin.value) { needPin.value = false; return; }
    if (qrShow.value) { qrShow.value = false; return; }
    if (showDel.value) { closeDel(); return; }
    if (nickOpen.value) { nickOpen.value = false; return; }
    if (introShow.value) closeIntro();
  };
  document.addEventListener("keydown", escLayers);
  onMountedCleanup(() => document.removeEventListener("keydown", escLayers));
  // 气泡引导不响应 Esc: 只有用户点开共享文件入口才收起并记录
  // 拖拽兜底: 文件拖到 dropzone 之外(页面边缘/侧栏)松手时, 浏览器默认会导航到该文件,
  // 整个页面被替换且无法后退。只在携带 Files 时拦默认行为, 不影响页面内文本拖选
  const preventFileDragNav = (e) => {
    if (e.dataTransfer && Array.from(e.dataTransfer.types || []).includes("Files")) e.preventDefault();
  };
  window.addEventListener("dragover", preventFileDragNav);
  window.addEventListener("drop", preventFileDragNav);
  onMountedCleanup(() => {
    window.removeEventListener("dragover", preventFileDragNav);
    window.removeEventListener("drop", preventFileDragNav);
  });
  // 附近设备心跳: 仅桌面/手机 App; 浏览器不做发现
  if (!isQrPetWindow && isDesktop) {
    setTimeout(async () => {
      await sendDiscoveryProbe();
      sendHello();
      setTimeout(refreshPeers, 200);   // 给 probe 回包一点时间
    }, 500);
    const peerPoll = setInterval(() => { sendHello(); refreshPeers(); }, 3000);
    onMountedCleanup(() => clearInterval(peerPoll));
  }
  // App 壳双向剪贴板: 状态就绪后启动; 卸载/隐藏时清理
  if (!isQrPetWindow && isDesktop) {
    setTimeout(() => { if (running.value) initClipboardSync(); }, 1200);
    const onClipVisibility = () => {
      if (document.visibilityState !== "visible") return;
      // 隐藏期间复制的内容不补发(安全基线), 只同步可见期间的新变化;
      // 同时补做一次远端写入, 让手机端在桌面 App 重新可见后立即生效。
      readLocalClipboardText().then((t) => {
        lastSentClip = (t || "").trim();
        applyRemoteClipboard();
      }).catch(() => {});
    };
    document.addEventListener("visibilitychange", onClipVisibility);
    onMountedCleanup(() => {
      document.removeEventListener("visibilitychange", onClipVisibility);
      stopClipboardSync();
    });
  }
  // PWA 新版本就绪(SW installed 且有旧 controller): 给一次可点的刷新提示
  if (!isQrPetWindow) {
    const onSwUpdate = () => showToast("界面已更新，刷新后生效", "success", {
      label: "刷新",
      fn: () => { try { location.reload(); } catch (e) {} },
    });
    window.addEventListener("ccflash:sw-update", onSwUpdate);
    onMountedCleanup(() => window.removeEventListener("ccflash:sw-update", onSwUpdate));
  }
  // 窗口尺寸/横竖屏变化后重新量输入框高度: 手机键盘收起/旋转会改变可用宽度
  if (!isQrPetWindow) {
    const onGrow = () => autoGrowInput();
    window.addEventListener("resize", onGrow);
    window.addEventListener("orientationchange", onGrow);
    onMountedCleanup(() => {
      window.removeEventListener("resize", onGrow);
      window.removeEventListener("orientationchange", onGrow);
    });
  }
  if (isDesktop) {
    // 移动壳: localStorage 在 WKWebView 可能不落盘 → 启动回读 Rust pref 里的配对码/开关。
    // 移动壳只作为独立本机节点, 不再恢复旧版本保存的"连接电脑"地址。
    if (mobileShell) {
      (async () => {
        try {
          // 首装局域网询问同样落 Rust pref: WKWebView localStorage 可能重启丢失
          const ra = await tauriInvoke("load_text_pref_cmd", { key: "lan-asked" });
          if (ra?.ok && ra.value === "1") lanAskSeen = true;
          if (!accessPin.value) {
            const rp = await tauriInvoke("load_text_pref_cmd", { key: "access-pin" });
            if (rp?.ok && typeof rp.value === "string" && rp.value) accessPin.value = rp.value;
          }
          // 系统剪贴板同步开关: WKWebView localStorage 可能不落盘, 以 Rust pref 为准
          const rc = await tauriInvoke("load_text_pref_cmd", { key: "clip-sync" });
          if (rc?.ok && (rc.value === "0" || rc.value === "1")) clipSyncEnabled.value = rc.value === "1";
        } catch (e) {}
        initMobileLocal();
      })();
    }
    // 悬浮码开关跨窗同步: Pet ✕ 关闭 / 主窗 CC设置切换 / 恢复入口, 三处改同一状态
    // 经 qr-float://changed 广播(emit 含自发自收, 同值回写幂等无副作用)
    listen("qr-float://changed", (e) => {
      const v = !!e.payload?.enabled;
      qrFloatEnabled.value = v;
      try { localStorage.setItem("wft-qr-float-enabled", v ? "1" : "0"); } catch (err) {}
    }).then((un) => { if (setupDisposed) un(); else unlistenQrFloat = un; });
    // 开关状态权威值从 Rust pref 文件加载(localStorage 可能是没落盘的陈旧缓存);
    // 加载完放行 Pet 显隐(petPrefsReady), 保证"重启后仍保持关闭"且启动不闪错态
    (async () => {
      try {
        const r = await tauriInvoke("load_text_pref_cmd", { key: "qr-float-enabled" });
        if (r?.ok && (r.value === "0" || r.value === "1")) {
          qrFloatEnabled.value = r.value === "1";
          try { localStorage.setItem("wft-qr-float-enabled", r.value); } catch (err) {}
        }
      } catch (e) {}
      if (isQrPetWindow) { petPrefsReady = true; syncPetVisibility(); }
    })();
    // Pet 窗: 恢复吸附记忆(无记忆=默认屏幕左侧中部) + 监听移窗做最近边吸附。
    // onMoved 在拖拽期间持续触发, 防抖 140ms 静默后才真正贴边 —— 拖到哪都会吸回边缘。
    if (isQrPetWindow) {
      // :has() 兜底: 老内核(Chrome<105/macOS≤12.2)不识别 body:has(.qr-pet) 的透明
      // 背景规则 → 整窗被 body 底色刷成深色方块; 类选择器全内核可用
      document.body.classList.add("pet-bg-clear");
      // [临时探针] boot = 页面启动+写通道存活; doc-over = 事件是否进了页面。
      // 与 petEnter 的 enter/expand-fire 串联可精确定位悬停断链环节
      petDiag({ fn: "boot", t: Date.now() });
      // 在场轮询 + 30s 拓扑自愈; 悬浮码一开始就是关闭态时不启动
      // (watcher 只在值变化时触发, 初值即 false 会让隐藏小窗永久轮询)
      if (qrFloatEnabled.value) petStartTimers();
      // 先监听后查询，覆盖启动竞态；后台/最小化/隐藏均恢复提醒。
      listen("main-vis://changed", (e) => {
        petMainStateVersion++;
        petApplyMainState(e.payload);
      }).then((un) => { if (setupDisposed) { un(); return; } unlistenMainVis = un; return petSyncMainState(); });
      getCurrentWebviewWindow().onMoved(() => {
        if (petResizing) return;   // 自己 setSize/setPosition 触发的移动不重复吸附
        clearTimeout(petSnapTimer);
        petSnapTimer = setTimeout(() => { petSnapTimer = null; petSnap().catch(() => {}); }, 140);
      }).then((un) => { if (setupDisposed) un(); else unlistenPetMoved = un; });
      // 首次落位信号: syncPetVisibility 等它再 show(防主屏默认位闪现); 2.5s 兜底
      // 放行 —— 落位迟迟不成功时宁可在默认位显示, 也不能永远不显示悬浮球
      petPlaced = new Promise((resolve) => { petPlacedResolve = resolve; });
      // 后台落位完成/2.5s 兜底后告知 Rust: 主窗 2.5s 状态轮询里的强制显示从此才放行
      petPlaced.then(() => tauriInvoke("pet_ready_cmd").catch(() => {}));
      setTimeout(() => { if (petPlacedResolve) petPlacedResolve(); }, 2500);
      (async () => {
        try {
          // 记忆从 Rust 文件加载(同步落盘可靠); 没有则默认屏幕左侧中部收拢。
          // 先挂记忆(含 mon 显示器锚)再解析工作区, petWorkArea(true) 才找得到目标屏
          const loaded = await petLoadDock();
          if (!petDock && loaded) setPetDock(loaded);
          // 落位带重试: 锁屏/显示器休眠期间 currentMonitor 可能拿不到 → petApply 失败,
          // 窗口会停在错误坐标; 唤醒后自动重试落位自愈(最多 ~3 分钟, 成功即停)
          for (let i = 0; i < 90; i++) {
            const wa = await petWorkArea(true);
            if (wa) {
              if (!petDock) setPetDock({ edge: "left", cross: wa.h / 2 });
              const ok = await petApply();   // 尺寸在执行时从状态推导, 排队不会与悬停展开交错
              if (ok) break;
            }
            await new Promise((r) => setTimeout(r, 2000));
          }
        } finally {
          if (petPlacedResolve) { petPlacedResolve(); petPlacedResolve = null; }
        }
      })();
    }
    refreshControl();
    refreshAutostart();
    // 桌面端把当前配色/明暗写入 pref, 供 /api/info 下发给浏览器/移动壳跟随
    saveTextPref("ui-theme", theme.value);
    saveTextPref("ui-mode", themeMode.value);
    // 首次进入提示共享文件入口(仅桌面端, 不自动收起)
    setTimeout(maybeShowFileHint, 1500);
    // 启动静默检查更新: 延迟触发(等 appVersion 下发到位) + 24h 频控, 失败/无更新均无感;
    // 悬浮码窗(pet)不检查 —— 否则更新弹窗会把 96×96 的小窗整个盖死
    setTimeout(() => {
      if (!isQrPetWindow && Date.now() - lastCheckedAt() >= UPDATE_CHECK_INTERVAL_MS) checkUpdate(false);
    }, UPDATE_CHECK_DELAY_MS);
  }
  // 悬浮码窗在"关闭"状态下完全不拉数据/不轮询: 它只是个隐藏的 96px 小窗,
  // 之前无条件启动会让它持续 250ms IPC + WS + 3s/5s 轮询, 白耗电
  if (!(isQrPetWindow && !qrFloatEnabled.value)) {
    loadFiles();
    loadChat();
    refreshStatus();
    startPolling();
  }
  // 首帧后补滚一次: 图片/字体落地会改变列表高度, 首次 nextTick 时可能还没贴底
  setTimeout(scrollChats, 300);
});
onUnmounted(() => {
  setupDisposed = true;
  for (const fn of mountedCleanups.splice(0)) { try { fn(); } catch (e) {} }
  document.removeEventListener("paste", onPaste);
  document.removeEventListener("click", onDocChatClick);
  // onMounted 注册的右键/收起/滚动/尺寸监听必须成对移除, 否则热重载会叠加
  document.removeEventListener("contextmenu", onContextMenu);
  document.removeEventListener("click", closeContextMenu);
  window.removeEventListener("scroll", closeContextMenu, true);
  window.removeEventListener("resize", closeContextMenu);
  if (unlistenDl) unlistenDl();
  if (unlistenQrFloat) unlistenQrFloat();
  if (unlistenPetMoved) unlistenPetMoved();
  if (unlistenMainVis) unlistenMainVis();
  clearTimeout(petSnapTimer);
  clearTimeout(petHoverTimer);
  clearTimeout(petCloseTimer);
  petStopTimers();
  // 窄屏断点监听: 此前从不反注册, 热重载后回调叠加
  if (typeof mq.removeEventListener === "function") mq.removeEventListener("change", onMqChange);
  else if (typeof mq.removeListener === "function") mq.removeListener(onMqChange);
  clearInterval(wsWatchdogTimer);
  clearTimeout(filesThrottleTimer);
  document.body.style.overflow = "";
  stopIntroSpy();
  closeWs(false);
  stopPolling();
});

const logsBox = ref(null);
const logQuery = ref("");
const filteredLogs = computed(() => {
  const terms = logQuery.value.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (!terms.length) return logs.value;
  return logs.value.filter((e) => {
    const hay = `${e.ts} ${lvlName(e.level)} ${e.loc} ${e.msg}`.toLowerCase();
    return terms.every((t) => hay.includes(t));
  });
});
/* 只渲染最近 300 行: 2000 行 × 每 2s 全量 v-html 重排会让服务中心页持续占满 CPU */
const visibleLogs = computed(() => {
  const arr = filteredLogs.value;
  return arr.length > 300 ? arr.slice(-300) : arr;
});
watch(logQuery, () => {
  nextTick(() => {
    const b = logsBox.value;
    if (b) b.scrollTop = b.scrollHeight;   // 过滤后定位到最新匹配
  });
});
async function refreshLogs() {
  // 只在服务中心页在台前时拉取: 日志面板仅此页展示, 其余视图每 2s 空拉+全量重排是纯浪费
  if (view.value !== "service") return;
  try {
    // 贴底跟随: 更新前沿在底部附近才自动滚到最新; 用户上翻看历史时不打扰
    const box = logsBox.value;
    const stick = !box || box.scrollHeight - box.scrollTop - box.clientHeight < 48;
    const r = await apiFetch(apiBase.value + API.logs);
    const d = await r.json();
    logs.value = d.logs || [];
    if (stick) nextTick(() => { const b = logsBox.value; if (b) b.scrollTop = b.scrollHeight; });
  } catch (e) {}
}
/* 切入服务中心立即补一拉, 不等下一个 2s 轮询(期间日志可能已滚动很多条) */
watch(() => view.value, (v) => { if (v === "service" && isRealDesktop) refreshLogs(); });
/* 日志行富化: 服务端已消毒(剥 CR/LF/截断), 前端先转义再上色 —— method 词高亮、
   "→ NNN" 状态码按首位着色(2xx/3xx 绿、4xx/5xx 红)。v-html 前必须转义 */
function escLog(s) {
  return String(s).replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
function logHtml(m) {
  let s = escLog(m);
  s = s.replace(/(^|·\s)((?:GET|POST|PUT|DELETE)\b)/g, '$1<span class="log-method">$2</span>');
  s = s.replace(/→\s?(\d{3})/g, (m0, code) => {
    const cls = code[0] === "4" || code[0] === "5" ? "st-bad" : code[0] === "2" || code[0] === "3" ? "st-ok" : "";
    return cls ? `→ <span class="log-st ${cls}">${code}</span>` : m0;
  });
  return s;
}
/* 级别名映射(loguru 命名): info→INFO / ok→SUCCESS / warn→WARNING / err→ERROR */
const LVL_NAMES = { info: "INFO", ok: "SUCCESS", warn: "WARNING", err: "ERROR" };
function lvlName(l) {
  return LVL_NAMES[l] || String(l || "?").toUpperCase();
}

async function clearLogs() {
  try {
    if (isDesktop) await tauriInvoke("control_clear_logs_cmd");
    else await apiFetch(apiBase.value + API.logsClear, { method: "POST" });
    logs.value = [];
    logQuery.value = "";
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
let statusRefreshing = false;   // 2.5s 定时器不等待上一次: 慢网下会叠 5 个在途并乱序覆盖
let mobileReprobing = false;
const offline = ref(false);   // 浏览器端与服务失联(服务停止/地址变更)
async function refreshStatus() {
  if (statusRefreshing) return;
  statusRefreshing = true;
  if (isQrPetWindow) petSyncMainState();
  const epoch = sessionEpoch;
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), 10000);
  try {
    // 移动壳本机节点走 Tauri 命令: HTTP /api/info 出于安全不下发 PIN 码值,
    // 走 HTTP 会导致刚生成的令牌在下一轮轮询被清空、无法复制。
    const d = (isRealDesktop || (mobileShell && isLocalAddr(serverAddr.value)))
      ? await tauriInvoke("control_status_cmd")
      : await (await apiFetch(apiBase.value + API.info, { signal: ctl.signal })).json();
    if (epoch !== sessionEpoch) return;   // 旧电脑的状态不得覆盖新连接
    // 移动端本机节点: 本机码随状态下发(Tauri 可信通道), 自动用于后续请求;
    // 令牌轮换/服务重启后下一轮轮询即可恢复, 不会弹配对框。
    if (mobileShell && isLocalAddr(serverAddr.value) && typeof d.pin === "string" && d.pin) {
      accessPin.value = d.pin;
    }
    syncPinState(d);
    // 轮询成功(=电脑端已关闭访问码, 约束真实解除)才收起输入框, 且给提示让"自动消失"可理解;
    // 访问码仍开启时所有请求 401, 永远走不到这里 —— 弹框不会被静默清掉。
    // 刻意不设"取消"按钮: 码仍生效时, 底下页面每次请求都会 401 再度弹回
    if (needPin.value) {
      needPin.value = false;
      // 来路有二: ① 电脑端真关了开关(本机未存码) ② submitPin 刚成功、在途旧轮询迟到 401 又被收起
      showToast(accessPin.value ? "已恢复与服务的连接" : "配对保护已由电脑端关闭");
      loadFiles();
      loadChat();
      connectWs();   // 解锁瞬间补拉断档数据, 与 submitPin 成功路径一致
    }
    statusFail = 0;
    const wasOffline = offline.value;
    offline.value = false;
    if (wasOffline) {
      // 断网自愈: 恢复的第一时间提示 + 立即全量重载, 不等下一轮轮询/WS 事件
      showToast("已恢复与服务的连接");
      chatRecoveredOffline = true;   // 恢复首拉的历史不计未读(见 loadChat)
      // 服务可能已重启: 旧分享票据/媒体会话全部失效
      clearShareTicketCache();
      loadFiles();
      loadChat();
      connectWs();
    }
    running.value = d.running;
    // 移动壳本机服务停止时 control_status_cmd 仍正常返回 running:false,
    // 不会进入 catch; 必须显式进入失联层, 否则没有"重新启动本机服务"入口。
    if (mobileShell && isLocalAddr(serverAddr.value) && !d.running) {
      offline.value = true;
    }
    port.value = d.port;
    if (typeof d.openLan === "boolean") openLanEnabled.value = d.openLan;
    if (typeof d.lanActive === "boolean") lanActive.value = d.lanActive;
    // 移动壳连本机时: 服务重启可能顺延端口, apiBase 必须跟着走
    if (mobileShell && d.port && isLocalAddr(serverAddr.value)) {
      const next = `127.0.0.1:${d.port}`;
      if (serverAddr.value !== next) {
        serverAddr.value = next;
        connectWs();
      }
    }
    // 开关开着就保持常驻(服务停止也显示圆点); 轮询兜底重显, 防窗口被外部误隐藏后回不来
    if (isRealDesktop && qrFloatEnabled.value) {
      tauriInvoke("show_qr_pet_cmd", { show: true }).catch(() => {});
    }
    syncPortInput(running.value, d.port);
    if (typeof d.version === "string") appVersion.value = d.version;
    applyServerConfig(d.config);
    applyServerUiTheme(d);
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
    // 401/429(带 status)已弹访问码输入框: 服务其实"可达"(只是要配对), 顺带清 offline ——
    // 否则先失联后开令牌时无信号页(z1000)永远盖住配对框(z998)。
    // 网络类错误(TypeError, 无 status)不走此路: 配对框开着时真断网仍要累计进失联,
    // 否则失联态被配对框吞掉, 页面永远停在"请输入配对码"
    if (e.status === 401 || e.status === 429) {
      // 服务可达(只是要配对/被限流): 不能累计失联, 否则无信号遮罩会盖住配对框。
      // 与 needPin 是否可见无关 —— 用户点"稍后再输入"后服务仍然是可达的。
      offline.value = false;
      return;
    }
    // 浏览器端连续失败 → 判定与服务失联: 状态回落为"已停止", 共享列表/聊天数据一并清空不再展示
    // 移动壳(isRealDesktop=false, isDesktop=true)同样累计失联: 否则电脑停机/断网后
    // 壳内永远显示最后一次的"运行中+陈旧列表", 无任何失联信号
    if (!isRealDesktop && ++statusFail >= OFFLINE_FAILS) {
      offline.value = true;
      running.value = false;
      allFiles.value = [];
      selected.value = new Set();
      chatMsgs.value = [];
      chatHtmlCache.clear();   // 失联期间服务可能已重启, 恢复后 id 可能从头计数
      chatCount.value = 0;
      seenChatId.value = 0;
      // 清空数组的同时必须让"内容签名"失效: 否则恢复后服务端内容与断网前一致,
      // loadFiles/loadChat 会因签名相同跳过赋值, 界面永久停在空列表
      loadFiles._sig = null;
      loadChat._sig = null;
      loadChat._lastSig = null;
      loadChat._init = false;
      knownFileNames = null;
      remoteFiles.value = [];
      // clearedTs 不重置: 本机清空意图不应被一次网络抖动(离线→恢复)作废,
      // 否则短暂失联重连后已清空的历史又会全量回来
    }
    // 移动壳连本机: 服务重启换端口后旧 apiBase 会一直失败, 主动重新探测一次
    if (mobileShell && isLocalAddr(serverAddr.value) && !mobileReprobing) {
      mobileReprobing = true;
      initMobileLocal().finally(() => { mobileReprobing = false; });
    }
  } finally {
    clearTimeout(timer);
    statusRefreshing = false;
  }
}
</script>

<template>
  <!-- 移动 App 启动门: 本机服务就绪前短暂等待; 失败时必须给出可点的重试出路 -->
  <div v-if="mobileShell && !serverAddr && !PREVIEW_MOBILE" class="connect-gate">
    <div class="connect-card">
      <h3>{{ mobileBootFailed ? "本机服务启动失败" : "正在启动本机服务…" }}</h3>
      <p v-if="!mobileBootFailed">手机作为独立节点运行本地文件服务与聊天，附近设备会自动发现，无需输入电脑地址。</p>
      <p v-else>服务未能就绪，请点下方重试；连续失败请检查系统是否限制了应用的后台网络。</p>
      <div class="cg-row">
        <span v-if="!mobileBootFailed" class="cg-loading">●</span>
        <button v-else class="btn btn-primary" @click="retryMobileBoot">重新启动本机服务</button>
      </div>
      <template v-if="mobileBootFailed">
        <p class="cg-err">请确认系统未限制应用后台网络；连续失败可尝试重启 App。</p>
      </template>
    </div>
  </div>
  <div v-if="!isQrPetWindow" class="shell" :class="{ 'transfer-active': view === 'transfer' || mobileTabbed, 'has-wx-banner': isWeChat, 'real-desktop': isRealDesktop, 'has-mobile-tabs': mobileTabbed }">
    <!-- 微信内置浏览器拦非微信域名下载: 顶部常驻引导(右上角 ⋯ → 在浏览器打开) -->
    <div v-if="isWeChat" class="wx-banner"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 11v6"/><circle cx="12" cy="7.5" r="1" fill="currentColor" stroke="none"/></svg> 当前在微信内打开，文件下载可能被拦截 —— 点右上角 <b>⋯</b> 选择「<b>在浏览器打开</b>」即可正常传输</div>
    <!-- ===== 侧边栏(仅桌面壳) ===== -->
    <aside v-if="isDesktop && !mobileShell" class="sidebar" :class="{ collapsed: sidebarCollapsed }">
      <div class="brand">
        <div class="logo"><img class="logo-img" src="/logo.png" alt="logo" /></div>
        <div class="t">CC Flash</div>
        <button v-if="isRealDesktop" class="intro-entry" title="软件介绍" aria-label="软件介绍" @click="openIntro">
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>
          介绍
        </button>
      </div>
      <nav class="nav">
        <button class="nav-item" :class="{ active: view === 'transfer' }" title="消息发送" @click="setView('transfer')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 2L11 13"/><path d="M22 2l-7 20-4-9-9-4z"/></svg></span><span class="nav-label">消息发送</span>
          <span v-if="unread > 0" class="nav-badge">{{ unread > 99 ? "99+" : unread }}</span>
        </button>
        <button v-if="isRealDesktop" class="nav-item svc" :class="{ active: view === 'service' }" title="服务中心" @click="setView('service')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg></span><span class="nav-label">服务中心</span>
        </button>
        <button v-if="isRealDesktop || mobileShell" class="nav-item" :class="{ active: view === 'settings' }" title="CC设置" @click="setView('settings')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 01-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z"/></svg></span><span class="nav-label">CC设置</span>
        </button>
      </nav>
      <div class="side-foot">
        <div class="side-ver">{{ appVersion ? "v" + appVersion + " · Tauri" : "Tauri" }}</div>
        <button class="collapse-btn" :class="{ open: sidebarCollapsed }"
                :title="sidebarCollapsed ? '展开侧边栏' : '收起侧边栏'"
                :aria-label="sidebarCollapsed ? '展开侧边栏' : '收起侧边栏'" @click="toggleSidebar">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><polyline points="15 18 9 12 15 6"/></svg>
          <span v-if="!sidebarCollapsed" class="collapse-txt">收起</span>
        </button>
      </div>
    </aside>

    <!-- ===== 主区 ===== -->
    <main class="main" :class="{ 'transfer-main': view === 'transfer', 'discover-main': view === 'discover', 'web': !isDesktop }">
      <!-- 顶部栏(桌面壳): 各页统一展示标题与地址; 传输页额外用房间头承载对话操作 -->
      <div v-if="isRealDesktop" class="topbar">
        <div class="topbar-head">
          <h2>{{ view === "discover" ? discoverTitle : (VIEW_TITLES[view] || VIEW_TITLES.transfer) }}</h2>
          <div class="sub">{{ running ? (lanActive ? "同一 Wi-Fi 下的设备扫码或复制地址即可访问" : "仅本机模式 · 其他设备无法访问（到「CC设置」开启局域网）") : "服务未启动 —— 到「服务中心」页启动" }}</div>
        </div>
        <div class="topbar-url">
          <div class="url-box">
            <span class="url-state" :class="running ? 'run' : 'stop'" :title="running ? '服务运行中' : '服务已停止'">
              <span class="dots" aria-hidden="true"><i></i><i></i><i></i></span><b>{{ running ? "运行中" : "已停止" }}</b>
            </span>
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

      <!-- ===== 传输(合并入口, 微信式) ===== -->
      <template v-if="view === 'transfer'">
        <div class="transfer-page" :class="{ 'is-mobile': isNarrow, 'is-web': !isDesktop, 'm-devices-page': mobileTab === 'devices' }"
             @dragover.prevent="dragOver = true" @dragleave.prevent="onPageDragLeave"
             @drop.prevent="onDrop">
          <!-- 桌面工作台: 左侧设备栏 = Snapdrop 风格身份卡 + 雷达 + 设备列表 -->
          <aside v-if="showDeviceRail" class="tx-devices" :class="{ 'is-inline': devicePanelInline }">
            <!-- 手机 App · DEVICES 页头（dir-2 信号台） -->
            <div v-if="mobileTabbed" class="m-pagehead m-dev-head">
              <div>
                <div class="m-kicker">UDP / 53318</div>
                <h1>设备</h1>
                <div class="m-sub">{{ peers.length + 1 }} DEVICES · SAME SUBNET</div>
              </div>
              <button class="m-head-act" @click="historyOpen = true">HISTORY</button>
            </div>
            <div class="tx-devices-head">
              <b>周边设备</b><span>{{ peers.length }}</span>
              <button class="tx-devices-manual" title="手动发送（输入 IP:端口）" aria-label="手动发送" @click="openManualSend">
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="M5.8 18.1a8.8 8.8 0 1 1 12.4 0"/><path d="M9.2 15.9a4.7 4.7 0 1 1 5.6 0"/><circle cx="12" cy="12" r="1.8" fill="currentColor" stroke="none"/></svg>
              </button>
              <button class="tx-devices-collapse" :title="devicePanelInline ? '收起设备面板' : '收起设备栏'"
                      :aria-label="devicePanelInline ? '收起设备面板' : '收起设备栏'"
                      @click="devicePanelInline ? (deviceInlineOpen = false) : toggleDeviceRail()">
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="15 18 9 12 15 6"/></svg>
              </button>
            </div>
            <div class="tx-idcard" :class="{ on: !sendTarget, off: !running }" role="button" tabindex="0"
                 :title="'共享到本机房间'" @click="selectSendTarget(null)"
                 @keydown.enter.self.prevent="selectSendTarget(null)" @keydown.space.self.prevent="selectSendTarget(null)">
              <span class="tx-idcard-ava"><svg v-if="selfDeviceKind === 'phone'" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="7" y="2.5" width="10" height="19" rx="2.2"/><path d="M10.5 5h3"/><circle cx="12" cy="18.2" r=".8" fill="currentColor" stroke="none"/></svg><svg v-else width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="4" width="18" height="12" rx="2"/><path d="M2 20h20"/></svg><i class="tx-state-dot" :class="running ? 'on' : 'off'"></i></span>
              <span class="tx-idcard-main"><b>{{ curNick }}</b><small>{{ running ? (lanActive ? "这是你 · 可被发现" : "这是你 · 仅本机") : "服务未启动" }}</small></span>
              <button class="tx-idcard-history" title="传输历史" aria-label="传输历史" @click.stop="historyOpen = true">
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <path d="M3.5 12a8.5 8.5 0 1 0 2.7-6.2"/><path d="M3 4v5h5"/><path d="M12 7.5V12l3 2"/>
                </svg>
              </button>
            </div>
            <div class="tx-radar-wrap">
            <div class="tx-radar" :class="{ scanning: running, probing: radarScanning, disabled: !running }"
                 role="button" tabindex="0" :aria-disabled="!running"
                 :title="running ? '点击主动扫描周边设备' : '服务未启动'"
                 aria-label="主动扫描周边设备" @click="refreshDiscover"
                 @keydown.enter.prevent="refreshDiscover" @keydown.space.prevent="refreshDiscover">
              <i class="tx-radar-ring r1"></i><i class="tx-radar-ring r2"></i><i class="tx-radar-sweep"></i>
<span class="tx-radar-self" :class="selfDeviceKind" role="img"
                    :aria-label="selfDeviceKind === 'phone' ? '本机 · 手机' : '本机 · 电脑'"
                    :title="selfDeviceKind === 'phone' ? '本机 · 手机' : '本机 · 电脑'">
                <svg v-if="selfDeviceKind === 'phone'" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="7" y="2.5" width="10" height="19" rx="2.2"/><path d="M10.5 5h3"/><circle cx="12" cy="18.2" r=".8" fill="currentColor" stroke="none"/></svg>
                <svg v-else width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="4" width="18" height="12" rx="2"/><path d="M2 20h20"/></svg>
              </span>
              <button v-for="(p, i) in radarPeers" :key="'r' + p.id" class="tx-radar-dot"
                      :class="[peerState(p), { on: isPeerSelected(p) }]"
                      :style="radarStyle(i)" :title="p.nickname || '设备'"
                      :aria-label="'发送到 ' + (p.nickname || '设备')"
                      :aria-pressed="isPeerSelected(p)" @click.stop="pickPeer(p)"></button>
            </div>
            <div class="tx-dev-legend" aria-label="设备状态图例">
              <span><i class="online"></i>在线</span>
              <span><i class="busy"></i>忙碌</span>
              <span><i class="away"></i>离线</span>
            </div>
            </div>
            <div v-if="mobileTabbed" class="m-dev-stats">
              <div><small>DEVICES</small><b>{{ peers.length + 1 }}</b></div>
              <div><small>PORT</small><b>{{ port }}</b></div>
              <div><small>LINK</small><b>{{ running ? "ON" : "OFF" }}</b></div>
            </div>
            <div v-if="mobileTabbed" class="m-dev-actions">
              <button :disabled="!running" @click="mobileProbe">单播探测</button>
              <button class="primary" @click="openManualSend">手动发送</button>
            </div>
            <div class="tx-devices-sub"><span>附近设备</span>
              <button class="tx-devices-multi" :class="{ on: multiSelectMode }"
                      :title="multiSelectMode ? '完成多选' : '多选附近设备，同时发送'"
                      @click="toggleMultiSelect">{{ multiSelectMode ? "完成" : "多选" }}</button>
              <button class="tx-devices-scan" :disabled="!running" :title="running ? '主动扫描（发送 UDP probe）' : '服务未启动'" aria-label="重新扫描" @click="refreshDiscover">
                <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 0 0-15-6.7L3 8"/><path d="M3 3v5h5"/><path d="M3 12a9 9 0 0 0 15 6.7L21 16"/><path d="M21 21v-5h-5"/></svg>
              </button>
            </div>
            <div class="tx-devices-list">
              <button v-if="mobileTabbed" type="button" class="tx-device tx-device-self" :class="{ on: !sendTargets.length }"
                      :aria-pressed="!sendTargets.length" @click="selectSendTarget(null)">
                <span class="m-dev-st online"></span>
                <span class="m-dev-nm"><b>{{ curNick }}</b><small>THIS DEVICE · SELF</small></span>
                <span class="m-rssi" aria-hidden="true"><i class="on" style="height:4px"></i><i class="on" style="height:7px"></i><i class="on" style="height:9px"></i><i class="on" style="height:12px"></i></span>
              </button>
              <div v-for="p in peersSorted" :key="p.id" class="tx-device" :class="{ on: isPeerSelected(p), busy: p.busy, multi: multiSelectMode }">
                <button class="tx-device-pick" :title="'发送到 ' + (p.nickname || '设备')"
                        :aria-pressed="multiSelectMode ? isPeerSelected(p) : undefined" @click="pickPeer(p)">
                  <template v-if="mobileTabbed">
                    <span class="m-dev-st" :class="peerState(p)"></span>
                    <span class="m-dev-nm"><b>{{ p.nickname || "未命名设备" }}</b><small>{{ peerStatusText(p) }} · {{ p.ip }}</small></span>
                    <span class="m-rssi" aria-hidden="true">
                      <i :class="{ on: peerRssi(p) >= 1 }" style="height:4px"></i><i :class="{ on: peerRssi(p) >= 2 }" style="height:7px"></i><i :class="{ on: peerRssi(p) >= 3 }" style="height:9px"></i><i :class="{ on: peerRssi(p) >= 4 }" style="height:12px"></i>
                    </span>
                  </template>
                  <template v-else>
                    <span class="tx-device-ava"><svg v-if="peerPlatformKind(p) === 'phone'" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="7" y="2.5" width="10" height="19" rx="2.2"/><path d="M10.5 5h3"/><circle cx="12" cy="18.2" r=".8" fill="currentColor" stroke="none"/></svg><svg v-else-if="peerPlatformKind(p) === 'laptop'" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="4" width="18" height="12" rx="2"/><path d="M2 20h20"/></svg><svg v-else width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="2.5" y="3.5" width="19" height="13" rx="2"/><path d="M8 20.5h8"/><path d="M12 16.5v4"/></svg><i class="tx-device-dot" :class="peerState(p)"></i></span>
                    <span class="tx-device-info"><b>{{ p.nickname || "未命名设备" }}</b><small>{{ peerStatusText(p) }}</small></span>
                  </template>
                  <span v-if="multiSelectMode" class="tx-device-check" :class="{ on: isPeerSelected(p) }" aria-hidden="true">
                    <svg v-if="isPeerSelected(p)" width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>
                  </span>
                </button>
                <div class="tx-device-actions">
                  <button class="tx-device-fav" :class="{ on: isFavPeer(p) }"
                          :title="isFavPeer(p) ? '取消收藏' : '收藏设备'" :aria-label="isFavPeer(p) ? '取消收藏' : '收藏设备'"
                          @click.stop="toggleFavPeer($event, p)">
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3.5l2.6 5.3 5.9.9-4.3 4.2 1 5.9-5.2-2.8-5.2 2.8 1-5.9-4.3-4.2 5.9-.9z"/></svg>
                  </button>
                  <button class="tx-device-history" title="传输历史" aria-label="传输历史" @click.stop="historyOpen = true">
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3.5 12a8.5 8.5 0 1 0 2.7-6.2"/><path d="M3 4v5h5"/><path d="M12 7.5V12l3 2"/></svg>
                  </button>
                </div>
              </div>
              <div v-if="!peers.length" class="tx-devices-empty">
                <span aria-hidden="true"><svg width="34" height="34" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4.9 9.9a10 10 0 0 1 14.2 0"/><path d="M7.8 13a6 6 0 0 1 8.4 0"/><path d="M10.6 16a2.2 2.2 0 0 1 2.8 0"/><circle cx="12" cy="19" r="1" fill="currentColor" stroke="none"/></svg></span>
                <p>未发现周边设备</p>
                <button class="tx-diag-entry" @click="openDiagnostics">故障诊断</button>
              </div>
            </div>
          </aside>
          <section class="tx-chat">
            <div v-if="isRealDesktop && !running" class="tx-service-bar">
              <span><svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="4" y="10" width="16" height="11" rx="2"/><path d="M8 10V7a4 4 0 0 1 8 0v3"/></svg> 服务未启动，手机暂时无法访问</span>
              <button @click="setView('service')">去启动</button>
            </div>
            <header class="tx-head" :class="{ slim: isRealDesktop }">
              <!-- 窄窗/手机: 设备入口靠左，与右侧共享文件同一行；点击展开竖屏设备面板 -->
              <button v-if="canTargetDevices && (isNarrow || !isRealDesktop)"
                      class="tx-hbtn tx-dev-btn tx-dev-btn-left" :class="{ on: deviceInlineOpen }"
                      :title="deviceInlineOpen ? '收起周边设备' : '展开周边设备'"
                      :aria-expanded="deviceInlineOpen ? 'true' : 'false'"
                      @click.stop="toggleDeviceInline">
                <svg class="tx-dev-ico" width="10" height="10" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                  <circle cx="12" cy="12" r="5.2" fill="currentColor"/>
                  <circle cx="12" cy="12" r="8.1" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-dasharray="3 3.36"/>
                </svg>
                <span>设备</span>
              </button>
              <div v-if="isRealDesktop" class="tx-room-brief">
                <button v-if="canTargetDevices && deviceRailCollapsed" class="tx-rail-open" title="展开周边设备" @click="toggleDeviceRail">
                  <svg class="tx-dev-ico" width="11" height="11" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                    <circle cx="12" cy="12" r="5.2" fill="currentColor"/>
                    <circle cx="12" cy="12" r="8.1" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-dasharray="3 3.36"/>
                  </svg>
                  设备
                </button>
                <span>{{ running ? (lanActive ? "同一 Wi-Fi · 局域网实时互通" : "仅本机模式") : "服务未启动" }}</span>
              </div>
              <span v-if="!isRealDesktop && !mobileShell" class="tx-room">CC</span>
              <div v-if="!isRealDesktop" class="tx-room-title">
                <template v-if="mobileTabbed">
                  <div class="m-kicker">TARGET / {{ sendTargets.length ? targetLabel : "LOCAL ROOM" }}</div>
                  <h1>发送</h1>
                  <p>{{ sendTargets.length ? `${sendTarget.ip}:${sendTarget.port} · LOCKED` : (running ? "LOCAL ROOM · 本机共享" : "服务未启动") }}</p>
                </template>
                <template v-else>
                  <div class="tx-room-head">
                    <h2>消息发送</h2>
                    <span class="chip" :class="running ? 'run' : 'stop'">
                      <i class="live-dot" aria-hidden="true"></i><span class="chip-label">{{ running ? "实时" : "已停止" }}</span>
                    </span>
                  </div>
                  <p>{{ running ? (lanActive ? "同一 Wi-Fi · 局域网实时互通" : "仅本机模式") : "服务未启动" }}</p>
                </template>
              </div>
              <button v-if="mobileTabbed" class="m-head-act m-head-dots" title="更多" aria-label="更多"
                      :aria-expanded="moreOpen ? 'true' : 'false'" @click.stop="toggleMore">●●●</button>
              <div class="tx-acts">
                <button class="tx-hbtn tx-clear-btn" title="清空本窗口的消息显示（不影响其他设备）" @click="clearChatDisplay(); moreOpen = false">清空消息</button>
                <div class="tx-file-entry">
                  <button class="tx-hbtn" :class="{ 'hint-pulse': fileHintShow }" title="共享文件" @click="toggleFilePanel">
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z"/></svg>
                    <b>{{ stats.count }}</b>
                    <i v-if="fileDot" class="tx-dot" role="status" title="有新文件"></i>
                  </button>
                  <div v-if="fileHintShow && !deviceInlineOpen" class="tx-hint-card" @click.stop>
                    <div class="tx-hint-head">
                      <span class="tx-hint-ico" aria-hidden="true"><svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg></span>
                      <b>共享文件都在这里</b>
                      <button class="tx-hint-x" title="关闭提示" aria-label="关闭提示" @click.stop="markFileHintSeen">
                        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><path d="M6 6l12 12M18 6L6 18"/></svg>
                      </button>
                    </div>
                    <p>预览、下载、分享、删除都在抽屉里，聊天里的文件卡片也会同步。</p>
                    <div class="tx-hint-acts">
                      <button class="tx-hint-ok" @click.stop="openFilePanel">打开共享文件</button>
                    </div>
                  </div>
                </div>
                <button v-if="!isNarrow && !isRealDesktop" class="tx-hbtn icon" title="复制地址" @click="copyUrl(); moreOpen = false">
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1"/></svg>
                </button>
                <button v-if="!isNarrow && !isRealDesktop" class="tx-hbtn icon" title="显示二维码" @click="openQr(); moreOpen = false">
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><line x1="14" y1="14" x2="14" y2="17"/><line x1="17" y1="14" x2="21" y2="14"/><line x1="14" y1="17" x2="14" y2="21"/><line x1="17" y1="21" x2="21" y2="21"/><line x1="21" y1="17" x2="21" y2="21"/></svg>
                </button>
                <!-- 桌面壳宽屏的菜单已无剩余项, 不渲染「⋯」; 窄屏仍有「服务中心」入口 -->
                <button v-if="!isRealDesktop || isNarrow" class="tx-hbtn icon" title="更多" aria-haspopup="true" :aria-expanded="moreOpen ? 'true' : 'false'" @click.stop="toggleMore">⋯</button>
                <span class="tx-me" :title="'昵称：' + curNick" @click="openNick">{{ avatarFor(curNick) }}</span>
              </div>
              <div v-if="moreOpen" class="tx-more" @click.stop>
                <button v-if="mobileShell" @click="openNick(); moreOpen = false">修改昵称</button>
                <button v-if="isNarrow" @click="clearChatDisplay(); moreOpen = false">清空消息</button>
                <button v-if="!isRealDesktop" @click="copyUrl(); moreOpen = false">复制地址</button>
                <button v-if="!isRealDesktop" @click="openQr(); moreOpen = false">显示二维码</button>
                <button v-if="mobileShell || (isRealDesktop && isNarrow)" @click="openManualSend(); moreOpen = false">手动发送</button>
                <button v-if="!isDesktop" @click="installPwa(); moreOpen = false">安装到主屏幕</button>
                <button v-if="isRealDesktop && isNarrow" @click="setView('service'); moreOpen = false">服务中心</button>
              </div>
            </header>
            <div v-if="mobileTabbed" class="m-target-bar">
              <span class="m-sig" aria-hidden="true">
                <i :class="{ off: !running }"></i><i :class="{ off: !running }"></i><i :class="{ off: !running }"></i><i :class="{ off: !running }"></i><i class="off"></i>
              </span>
              <span class="m-tb-name">发送到 <b>{{ targetLabel }}</b></span>
              <span class="m-ip">{{ sendTargets.length > 1 ? `${sendTargets.length} 台` : (sendTargets.length ? `${sendTarget.ip}:${sendTarget.port}` : `本机 :${port}`) }}</span>
              <button v-if="sendTargets.length" class="m-tb-clear" title="切回本机共享" aria-label="切回本机共享" @click="clearSendTarget">✕</button>
            </div>
            <div v-if="moreOpen" class="tx-menu-backdrop" @click="moreOpen = false"></div>

            <div class="tx-list" ref="chatBox" @scroll="onTimelineScroll" @click.self="activeMsgKey = ''">
              <div v-if="!timelineItems.length" class="tx-date">{{ running ? fmtDay(Date.now() / 1000) : "服务已停止" }}</div>
              <div v-if="!timelineItems.length" class="tx-empty">
                <div class="tx-empty-ic" aria-hidden="true">
                  <svg width="40" height="40" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
                </div>
                <h3>还没有内容</h3>
                <p>{{ mobileTabbed ? "上传文件，或说句话。" : "拖个文件进来，或说句话。" }}</p>
                <div class="tx-empty-actions">
                  <button class="btn btn-primary" @click="pickFiles(true)">上传文件</button>
                  <button class="btn btn-ghost" @click="openFilePanel">查看共享文件</button>
                </div>
              </div>

              <div v-if="timelineStart > 0" class="tx-more-hint">
                <button class="tx-more-btn" @click="loadEarlierTimeline">加载更早的消息</button>
              </div>
              <template v-for="(it, i) in visibleTimeline" :key="it.key">
                <div v-if="showDayDivider(i)" class="tx-date">{{ fmtDay(it.ts) }}</div>
                <div v-if="it.kind === 'msg'" class="tx-msg" :class="{ out: isMine(it.msg), active: activeMsgKey === it.key }" @click="onMsgClick(it)">
                  <span class="tx-ava">{{ avatarFor(it.msg.nickname) }}</span>
                  <div class="tx-col">
                    <div class="tx-name"><span class="nm">{{ it.msg.nickname }}</span><span class="tm">{{ fmtTime(it.msg.timestamp) }}</span></div>
                    <div class="tx-bubble" v-html="it.html"></div>
                    <div class="tx-foot">
                      <div class="tx-msg-acts" @click.stop>
                        <button class="danger" title="删除" aria-label="删除消息" @click="deleteMessage(it.msg)">
                          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a2 2 0 012-2h4a2 2 0 012 2v2"/><line x1="10" y1="11" x2="10" y2="17"/><line x1="14" y1="11" x2="14" y2="17"/></svg>
                        </button>
                        <button title="引用" aria-label="引用消息" @click="startQuote(it.msg)">
                          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M10.7 8.4c-1.8.6-2.9 1.9-2.9 3.5 0 1.3.9 2.2 2.1 2.2 1.1 0 2-.9 2-2 0-1-.6-1.7-1.5-1.9.2-.4.6-.8 1.1-1z" fill="currentColor" stroke="none"/><path d="M16.6 8.4c-1.8.6-2.9 1.9-2.9 3.5 0 1.3.9 2.2 2.1 2.2 1.1 0 2-.9 2-2 0-1-.6-1.7-1.5-1.9.2-.4.6-.8 1.1-1z" fill="currentColor" stroke="none"/></svg>
                        </button>
                        <button :class="{ copied: copiedMsgKey === it.key }" :title="copiedMsgKey === it.key ? '已复制' : '复制'"
                                :aria-label="copiedMsgKey === it.key ? '已复制' : '复制消息'" @click="copyMessage(it.msg)">
                          <svg v-if="copiedMsgKey === it.key" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.3" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>
                          <svg v-else width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z"/><polyline points="14 2 14 8 20 8"/></svg>
                        </button>
                      </div>
                    </div>
                  </div>
                </div>

                <div v-else-if="it.kind === 'remote'" class="tx-file"
                     :class="{ out: it.r.uploader && it.r.uploader === curNick }">
                  <span class="tx-ava" :title="it.r.uploader ? '上传者：' + it.r.uploader : '来自其他设备'">{{ avatarFor(it.r.uploader || '共享文件') }}</span>
                  <div class="tx-file-col">
                    <div class="tx-name"><span class="nm">{{ it.r.uploader || "其他设备" }}</span><span class="tm">{{ fmtTime(it.r.modified || it.r.ts) }}</span></div>
                    <div class="tx-filecard" :data-file="it.r.name" :class="{ viewed: isFileViewed(it.r.name) }">
                      <div class="tx-file-main">
                        <span class="tx-ficon clickable" :class="'tone-' + fileTone(it.r.name)"
                              :title="isRealDesktop ? '打开' : (isPreviewable(it.r.name) ? '预览' : '下载')"
                              @click.stop="openThumb(it.r.name)">
                          <img v-if="isImage(it.r.name) && thumbSrcs[it.r.name]" :src="thumbSrcs[it.r.name]" loading="lazy" decoding="async" alt="" @load="$event.target.style.display = ''" @error="$event.target.style.display = 'none'; retryThumb(it.r.name)" />
                          <template v-else>{{ iconFor(it.r.name) }}</template>
                        </span>
                        <span class="tx-finfo">
                          <span class="tx-fname" :title="it.r.name">{{ it.r.name }}</span>
                          <span class="tx-fmeta">共享文件 · {{ fmtSize(it.r.size) }}</span>
                        </span>
                      </div>
                      <div class="tx-file-actions">
                        <button v-if="isRealDesktop" @click="openRemote(it.r)">打开</button>
                        <button v-else-if="isPreviewable(it.r.name)" @click="previewFileWeb(it.r.name)">预览</button>
                        <button v-else @click="openRemote(it.r)">下载</button>
                        <button v-if="isRealDesktop" @click="copyFileToClipboard(it.r.name)">复制</button>
                        <button @pointerenter="prefetchShareTicket(it.r.name)" @click="shareFile(it.r.name)">分享</button>
                        <button @click="locateFile(it.r.name)">定位</button>
                        <button class="danger" @click="confirmDelete(it.r.name)">删除</button>
                      </div>
                    </div>
                  </div>
                </div>

                <div v-else-if="it.kind === 'file'" class="tx-file out">
                  <div class="tx-file-col">
                    <div class="tx-name"><span class="nm">{{ curNick }}</span><span class="tm">{{ fmtTime(it.u.doneAt ? it.u.doneAt / 1000 : it.u.t0) }}</span></div>
                    <div class="tx-filecard" :data-file="uploadName(it.u)" :class="{ viewed: it.u.status === 'done' && isFileViewed(uploadName(it.u)) }">
                      <div class="tx-file-main">
                        <span class="tx-ficon" :class="['tone-' + fileTone(it.u.name), { clickable: it.u.status === 'done' }]"
                              :title="it.u.status === 'done' ? (isRealDesktop ? '打开' : (isPreviewable(uploadName(it.u)) ? '预览' : '下载')) : ''"
                              @click.stop="it.u.status === 'done' && openThumb(uploadName(it.u))">
                          <img v-if="it.u.status === 'done' && isImage(it.u.name) && thumbSrcs[uploadName(it.u)]" :src="thumbSrcs[uploadName(it.u)]" loading="lazy" decoding="async" alt="" @load="$event.target.style.display = ''" @error="$event.target.style.display = 'none'; retryThumb(uploadName(it.u))" />
                          <template v-else>{{ iconFor(it.u.name) }}</template>
                        </span>
                        <span class="tx-finfo">
                          <span class="tx-fname" :title="uploadName(it.u)">{{ uploadDisplayName(it.u) }}</span>
                          <span class="tx-fmeta" :class="{ doing: it.u.status === 'uploading' || it.u.status === 'waiting', err: it.u.status === 'error' }">{{ uploadStatusText(it.u) }}</span>
                        </span>
                        <button v-if="it.u.status === 'uploading' || it.u.status === 'waiting'" class="tx-fx" title="取消上传" @click="cancelUpload(it.u)">✕</button>
                        <button v-else-if="it.u.status === 'error'" class="tx-fx tx-retry" @click="retryUpload(it.u)">重试</button>
                      </div>
                      <div v-if="it.u.status === 'uploading' || it.u.status === 'waiting'" class="tx-progress"><i :style="{ width: (it.u.pct || 0) + '%' }"></i></div>
                      <div v-if="it.u.status === 'done'" class="tx-file-actions">
                        <button v-if="isRealDesktop" @click="openUpload(it.u)">打开</button>
                        <button v-else-if="isPreviewable(uploadName(it.u))" @click="previewFileWeb(uploadName(it.u))">预览</button>
                        <button v-else @click="openUpload(it.u)">下载</button>
                        <button v-if="isRealDesktop" @click="copyFileToClipboard(uploadName(it.u))">复制</button>
                        <button @pointerenter="prefetchShareTicket(uploadName(it.u))" @click="shareUpload(it.u)">分享</button>
                        <button @click="locateUpload(it.u)">定位</button>
                        <button class="danger" @click="confirmDelete(uploadName(it.u))">删除</button>
                      </div>
                      <div v-else-if="it.u.status === 'error'" class="tx-file-actions">
                        <button @click="retryUpload(it.u)">重试</button>
                        <button @click="cancelUpload(it.u)">取消</button>
                      </div>
                    </div>
                  </div>
                  <span class="tx-ava">{{ avatarFor(curNick) }}</span>
                </div>

                <div v-else-if="it.kind === 'group'" class="tx-file out">
                  <div class="tx-file-col">
                    <div class="tx-name"><span class="nm">{{ curNick }}</span><span class="tm">{{ fmtTime(it.ts) }}</span></div>
                    <div class="tx-groupcard">
                      <div class="tx-group-head" @click="toggleGroup(it.batch)">
                        <span class="tx-gicon" :class="'tone-' + fileTone(it.items[0] && it.items[0].name)">{{ iconFor(it.items[0] && it.items[0].name) }}</span>
                        <span class="tx-gtitle">
                          <b>{{ it.items.length }} 个文件</b>
                          <small>{{ fmtSize(groupSize(it.items)) }} · {{ groupStatusText(it.items) }}</small>
                        </span>
                        <span class="tx-gtoggle">{{ expandedGroups[it.batch] ? "收起" : "展开" }} ›</span>
                      </div>
                      <div class="tx-gicons">
                        <span v-for="u in it.items.slice(0, 6)" :key="u.id" class="tx-gicon small" :class="'tone-' + fileTone(u.name)">{{ iconFor(u.name) }}</span>
                        <span v-if="it.items.length > 6" class="tx-gmore">+{{ it.items.length - 6 }}</span>
                      </div>
                      <div v-if="expandedGroups[it.batch]" class="tx-glist">
                        <div v-for="u in it.items" :key="u.id" class="tx-grow">
                          <span class="tx-gname" :title="uploadName(u)">{{ uploadDisplayName(u) }}</span>
                          <span class="tx-gmeta" :class="{ err: u.status === 'error' }">{{ uploadStatusText(u) }}</span>
                          <button v-if="u.status === 'uploading' || u.status === 'waiting'" @click="cancelUpload(u)">✕</button>
                          <button v-else-if="u.status === 'error'" @click="retryUpload(u)">重试</button>
                          <button v-else @click="locateUpload(u)">定位</button>
                        </div>
                      </div>
                    </div>
                  </div>
                  <span class="tx-ava">{{ avatarFor(curNick) }}</span>
                </div>

                <div v-else-if="it.kind === 'receipt'" class="tx-receipt-block">
                  <time class="tx-receipt-time">{{ fmtTime(it.r.ts) }}</time>
                  <div class="tx-receipt" :class="{ err: !it.r.ok }">{{ it.r.text }}</div>
                </div>
              </template>

            </div>
            <button v-if="jumpCount" class="tx-jump" title="回到底部" aria-label="回到底部"
                    :style="{ bottom: `calc(${uploadCount ? 100 : 70}px + env(safe-area-inset-bottom, 0px))` }"
                    @click="jumpToLatest">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 4v15"/><path d="M6 13l6 6 6-6"/></svg>
              <span>{{ jumpCount }} 条新消息</span>
            </button>

            <div v-if="uploadCount" class="tx-upload-strip">
              <span>正在上传 {{ uploadCount }} 个</span>
              <span class="bar"><i :style="{ width: uploadAvgPct + '%' }"></i></span>
              <span>{{ uploadAvgPct }}%</span>
            </div>

            <div v-if="plusOpen" class="tx-plus-panel" @click.stop>
              <button class="tx-pa" @click="pickFiles(); plusOpen = false"><span class="ic"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/><path d="M8 13h8M8 17h5"/></svg></span>文件</button>
              <button class="tx-pa" @click="pickDir(); plusOpen = false"><span class="ic"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg></span>文件夹</button>
              <button class="tx-pa" @click="pasteFromClipboard"><span class="ic"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="8" y="2.5" width="8" height="4" rx="1.2"/><path d="M16 4.5h2a2 2 0 0 1 2 2v13a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2v-13a2 2 0 0 1 2-2h2"/><path d="M12 11v6M9.5 14.5 12 17l2.5-2.5"/></svg></span>粘贴</button>
              <button class="tx-pa" @click="openFilePanel(); plusOpen = false"><span class="ic"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/><path d="M9 13h6M9 17h4"/></svg></span>共享文件</button>
            </div>

            <div v-if="quoting" class="tx-quoting">
              <span class="tx-quoting-txt">引用 {{ quoting.nickname }}：{{ quoting.message }}</span>
              <button title="取消引用" aria-label="取消引用" @click="quoting = null">✕</button>
            </div>

            <div v-if="sendTargets.length" class="tx-target-note">
              <span class="tx-target-note-txt">发送到 <b>{{ targetLabel }}</b>{{ directSending ? " · 正在发送…" : "" }}</span>
              <button @click="clearSendTarget">切回本机共享</button>
            </div>

            <footer class="tx-composer" :class="{ 'has-quote': quoting }">
              <button class="tx-plus" :class="{ open: plusOpen }" title="上传文件 / 文件夹" @click="togglePlus">＋</button>
              <textarea ref="chatInputEl" class="tx-input" v-model="chatInput" rows="1"
                        :placeholder="running ? (sendTarget ? ((touchOnly || !isRealDesktop) ? `发送到 ${targetLabel}…` : `发送到 ${targetLabel}…（Enter 直发）`) : ((touchOnly || !isRealDesktop) ? '说点什么…' : '说点什么…（Enter 发送）')) : '服务未启动'"
                        :title="touchOnly ? '点右侧按钮发送' : 'Enter 发送，Shift+Enter 换行'"
                        @focus="onInputFocus" @keydown.enter="onChatEnter" @input="onChatInput"></textarea>
              <span v-if="chatInput" class="tx-char-count" :class="{ over: chatCharCount > cfg.msgMax }">{{ chatCharCount }} / {{ cfg.msgMax }}</span>
              <button class="tx-send" :disabled="!chatInput.trim() || sending || chatCharCount > cfg.msgMax" @click="sendChat">{{ sending ? "发送中…" : (mobileTabbed ? "发送" : (sendTarget ? "直发" : "发送")) }}</button>
            </footer>

            <input type="file" id="fileInput" multiple style="display:none" @change="onPick" />
            <input type="file" id="dirInput" webkitdirectory multiple style="display:none" @change="onPickDir" />

            <div v-if="dragOver" class="tx-drag">
              <div class="tx-drag-ic" aria-hidden="true"><svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V5"/><path d="m5 12 7-7 7 7"/></svg></div>
              <b>松手传到电脑</b>
              <small>{{ collecting ? "正在读取拖入的文件…" : "文件 / 文件夹 · 保持目录结构" }}</small>
            </div>
          </section>

          <div v-if="isDesktop && !isNarrow && !drawerCollapsed" class="tx-drawer-backdrop" @click="drawerCollapsed = true"></div>
          <aside class="tx-files" :class="{ collapsed: !isNarrow && !mobileTabbed && drawerCollapsed, sheet: isNarrow || mobileTabbed, open: fileSheetOpen, 'm-select': mobileFileSelect }">
            <!-- 手机 App · FILES 页头 -->
            <div v-if="mobileTabbed" class="m-pagehead m-files-head">
              <div>
                <div class="m-kicker">MANIFEST / ROOM</div>
                <h1>文件</h1>
                <div class="m-sub">{{ stats.count }} ITEMS · {{ stats.size }}</div>
              </div>
              <div class="m-files-acts">
                <span class="m-count">{{ String(stats.count).padStart(2, "0") }}</span>
                <button v-if="stats.count" class="m-head-act" @click="mobileFileSelect = !mobileFileSelect">{{ mobileFileSelect ? "完成" : "选择" }}</button>
              </div>
            </div>
            <div class="tx-grab" @click="fileSheetOpen = false"><i></i></div>
            <div class="tx-files-head">
              <b>共享文件</b><span class="tx-count">{{ stats.count }}</span>
              <span class="tx-files-head-acts">
                <button v-if="isRealDesktop" class="tx-files-open" title="打开共享目录" aria-label="打开共享目录" @click="openShareDir">
                  <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 19V6a2 2 0 012-2h4l2 2h7a2 2 0 012 2v2"/><path d="M4 19h14.5a2 2 0 001.94-1.5L22 9H7.5a2 2 0 00-1.94 1.5z"/></svg>
                </button>
                <button class="tx-files-close" :title="isNarrow ? '关闭' : '收起'" @click="isNarrow ? fileSheetOpen = false : drawerCollapsed = true">{{ isNarrow ? "完成" : "›" }}</button>
              </span>
            </div>
            <div class="tx-files-tools">
              <div class="tx-sort">
                <button :class="{ on: sortBy === 'time' }" @click="setSort('time')">最新</button>
                <button :class="{ on: sortBy === 'name' }" @click="setSort('name')">名称</button>
                <button :class="{ on: sortBy === 'size' }" @click="setSort('size')">大小</button>
              </div>
              <label v-if="pageFiles.length" class="tx-selall" title="全选/取消本页">
                <input type="checkbox" :checked="pageAllSelected" @change="toggleAll" />全选
              </label>
              <button class="tx-mini" :class="{ spinning: refreshing }" title="刷新文件列表" @click="loadFiles(true)">↻</button>
            </div>
            <div class="tx-files-list">
              <div v-if="!pageFiles.length" class="tx-files-empty">
                <div aria-hidden="true"><svg width="36" height="36" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M21 8 12 3 3 8l9 5 9-5Z"/><path d="M3 8v8l9 5 9-5V8"/><path d="M12 13v8"/></svg></div>
                <p>还没有文件</p>
                <button class="btn btn-primary" @click="pickFiles(true)">上传文件</button>
              </div>
              <div v-for="(f, idx) in pageFiles" :key="f.name" class="tx-frow" :class="{ located: locatedName === f.name }" :data-file="f.name" @dblclick="rowDblClick($event, f)">
                <span v-if="mobileTabbed" class="m-file-no">{{ String(idx + 1).padStart(2, "0") }}</span>
                <label class="tx-check" @click.stop>
                  <input type="checkbox" :checked="selected.has(f.name)" @change="toggleSel(f.name)" />
                </label>
                <div class="tx-thumb clickable" :class="'tone-' + fileTone(f.name)"
                     :title="isRealDesktop ? '打开' : (isPreviewable(f.name) ? '预览' : '下载')"
                     @click="openThumb(f.name)">
                  <span>{{ iconFor(f.name) }}</span>
                  <img v-if="isImage(f.name) && thumbSrcs[f.name]" :src="thumbSrcs[f.name]" loading="lazy" decoding="async" alt="" @load="$event.target.style.display = ''" @error="$event.target.style.display = 'none'; retryThumb(f.name)" />
                </div>
                <div class="tx-finfo2" :class="{ 'mobile-tap': isNarrow }"
                     :role="isNarrow ? 'button' : undefined" :tabindex="isNarrow ? 0 : undefined"
                     @click="isNarrow && openFileMenu(f.name)"
                     @keydown.enter.prevent="isNarrow && openFileMenu(f.name)"
                     @keydown.space.prevent="isNarrow && openFileMenu(f.name)">
                  <div class="tx-fname2" :title="f.name">{{ f.name }}</div>
                  <div class="tx-fmeta2">{{ fmtSize(f.size) }} · {{ fmtTime(f.modified) }}</div>
                  <div v-if="mobileTabbed" class="m-pbar" aria-hidden="true"><i style="width:100%"></i></div>
                </div>
                <div class="tx-frow-acts">
                  <button v-if="isNarrow" title="更多" :aria-label="'更多操作：' + f.name" @click="openFileMenu(f.name)">⋯</button>
                  <template v-else>
                    <template v-if="isRealDesktop">
                      <button title="打开" aria-label="打开" @click="openFile(f.name)">
                        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M2 12s3.5-6 10-6 10 6 10 6-3.5 6-10 6S2 12 2 12z"/><circle cx="12" cy="12" r="2.5"/></svg>
                      </button>
                      <button title="复制到剪贴板" aria-label="复制到剪贴板" @click="copyFileToClipboard(f.name)">
                        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1"/></svg>
                      </button>
                    </template>
                    <template v-else>
                      <button v-if="isPreviewable(f.name)" title="预览" aria-label="预览" @click="previewFileWeb(f.name)">
                        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/><circle cx="12" cy="12" r="3"/></svg>
                      </button>
                      <button title="下载" aria-label="下载" @click="downloadFileWeb(f.name)">
                        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>
                      </button>
                    </template>
                    <button class="tx-share" title="分享" aria-label="分享" @click="shareFile(f.name)">
                      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><line x1="8.59" y1="13.51" x2="15.42" y2="17.49"/><line x1="15.41" y1="6.51" x2="8.59" y2="10.49"/></svg>
                    </button>
                    <button class="danger" title="删除" aria-label="删除" @click="confirmDelete(f.name)">
                      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a2 2 0 012-2h4a2 2 0 012 2v2"/><line x1="10" y1="11" x2="10" y2="17"/><line x1="14" y1="11" x2="14" y2="17"/></svg>
                    </button>
                  </template>
                </div>
              </div>
              <button v-if="mobileTabbed" class="m-drop" @click="pickFiles(true)"><b>＋ DROP FILES</b><span>上传文件 / 文件夹</span></button>
            </div>
            <div class="tx-files-foot">
              <button v-if="selectedCount" class="tx-del" @click="confirmDeleteSelected">
                <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><line x1="10" y1="11" x2="10" y2="17"/><line x1="14" y1="11" x2="14" y2="17"/></svg>
                删除选中 ({{ selectedCount }})
              </button>
              <span v-else-if="isNarrow && pageFiles.length" class="tx-files-tip">{{ mobileTabbed ? "点文件打开操作" : "点文件或 ⋯ 操作" }}</span>
              <div class="tx-pg" v-if="totalPages > 1">
                <button :disabled="currentPage === 1" @click="goPage(currentPage - 1)">‹</button>
                <template v-for="(it, i) in pageItems" :key="i">
                  <span v-if="it === '…'">…</span>
                  <button v-else :class="{ on: it === currentPage }" @click="goPage(it)">{{ it }}</button>
                </template>
                <button :disabled="currentPage === totalPages" @click="goPage(currentPage + 1)">›</button>
              </div>
            </div>
          </aside>
          <div v-if="isNarrow && fileSheetOpen && !mobileTabbed" class="tx-sheet-backdrop" @click="fileSheetOpen = false"></div>
        </div>

        <div v-if="fileMenuName" class="tx-action-overlay" @click.self="closeFileMenu">
          <div class="tx-action-sheet">
            <div class="tx-action-title">{{ fileMenuName }}</div>
            <template v-if="isRealDesktop">
              <button @click="openFile(fileMenuName); closeFileMenu()">打开</button>
              <button @click="copyFileToClipboard(fileMenuName); closeFileMenu()">复制文件</button>
            </template>
            <template v-else>
              <button v-if="isPreviewable(fileMenuName)" @click="previewFileWeb(fileMenuName); closeFileMenu()">预览</button>
              <button @click="downloadFileWeb(fileMenuName); closeFileMenu()">下载</button>
            </template>
            <button @click="shareFile(fileMenuName); closeFileMenu()">分享</button>
            <button class="danger" @click="confirmDelete(fileMenuName); closeFileMenu()">删除</button>
            <button @click="closeFileMenu">取消</button>
          </div>
        </div>
      </template>

      <!-- 设置页头: 有移动标签时由标签导航; 宽屏浏览器给返回箭头 -->
      <div v-if="!isRealDesktop && view === 'settings'" class="m-pagehead" :class="{ 'm-settings-head': mobileTabbed }">
        <button v-if="!mobileTabbed" class="m-back" title="返回发送" aria-label="返回发送" @click="setView('transfer')">‹</button>
        <div>
          <div v-if="mobileTabbed" class="m-settings-kicker">LOCAL / CONFIG</div>
          <component :is="mobileTabbed ? 'h1' : 'h2'">设置</component>
          <p>{{ mobileTabbed ? "ACCESS · SYNC · THEME" : "本机服务、外观与关于" }}</p>
        </div>
      </div>

      <!-- 服务中心: 控制卡通栏 + 日志卡吃满剩余高度; 扫码走顶栏「显示二维码」弹窗(信息更全), 页面不再放冗余扫码卡 -->
      <template v-if="isRealDesktop && view === 'service'">
        <div class="card svc-ctrl">
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
        <!-- 服务日志: 吃满剩余高度 -->
        <div class="card svc-log-card">
          <div class="list-head">
            <div class="lh-left">
              <h3>服务日志</h3>
              <span class="badge">{{ logQuery ? filteredLogs.length + " / " + logs.length : logs.length }} 条</span>
            </div>
            <div class="log-search" :class="{ on: logQuery }">
              <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="11" cy="11" r="7"/><line x1="16.5" y1="16.5" x2="21" y2="21"/></svg>
              <input v-model="logQuery" placeholder="搜索日志" spellcheck="false" @keydown.esc="logQuery = ''" />
              <button v-if="logQuery" class="log-search-clear" title="清除搜索" aria-label="清除搜索" @click="logQuery = ''">✕</button>
            </div>
            <button class="btn btn-ghost" style="height:28px" title="清空服务日志显示（不影响服务运行）"
                    @click="clearLogs">清空日志</button>
          </div>
          <div class="svc-box" style="flex:1;display:flex;flex-direction:column;margin-bottom:0;min-height:0">
            <div class="logs" ref="logsBox" style="flex:1;height:auto">
              <div v-for="(e, i) in visibleLogs" :key="i" class="log-line">
                <span class="log-ts">{{ e.ts }}</span>
                <span class="log-sep">|</span>
                <span class="log-lvl" :class="'lvl-' + e.level">{{ lvlName(e.level) }}</span>
                <span class="log-sep">|</span>
                <span class="log-loc">{{ e.loc }}</span>
                <span class="log-sep">-</span>
                <span class="log-msg" v-html="logHtml(e.msg)"></span>
              </div>
              <div v-if="!filteredLogs.length" class="log-empty">{{ logQuery ? "没有匹配的日志" : "HTTP 请求与服务事件…" }}</div>
            </div>
          </div>
        </div>
      </template>

      <!-- CC设置: 仅真实桌面/移动壳; 浏览器没有设置页 -->
      <template v-if="(isRealDesktop || mobileShell) && view === 'settings'">
        <div class="card" v-if="isRealDesktop">
          <h3>通用</h3>
          <div class="setting-row">
            <div class="setting-info">
              <div class="setting-title">开机自动启动</div>
              <div class="setting-desc">开启后登录电脑时 CC Flash 将在后台自动启动（默认关闭，仅在你打开开关时注册）</div>
            </div>
            <button class="switch" :class="{ on: autostartOn }" role="switch" :aria-checked="autostartOn"
                    :title="autostartOn ? '点击关闭' : '点击开启'" @click="toggleAutostart">
              <span class="knob"></span>
            </button>
          </div>
          <div class="setting-row">
            <div class="setting-info">
              <div class="setting-title">访问令牌保护</div>
              <div class="setting-desc">
                开启后同一 Wi-Fi 下的设备需输入配对码才能使用（本机不受影响）
                <template v-if="pinEnabled"> · 当前令牌 <b>{{ pinCode }}</b></template>
              </div>
            </div>
            <div style="display:flex;align-items:center;gap:8px">
              <button v-if="pinEnabled" class="btn btn-ghost" style="height:28px"
                      @click="copyText(pinCode, '令牌已复制')">复制令牌</button>
              <button class="switch" :class="{ on: pinEnabled }" role="switch" :aria-checked="pinEnabled"
                      :aria-label="pinEnabled ? '关闭访问令牌保护' : '开启访问令牌保护'"
                      :title="pinEnabled ? '点击关闭' : '点击开启'" @click="toggleAccessPin">
                <span class="knob"></span>
              </button>
            </div>
          </div>
          <div class="setting-row">
            <div class="setting-info">
              <div class="setting-title">系统剪贴板自动同步</div>
              <div class="setting-desc">默认关闭；开启后与打开本机地址的浏览器/设备共享文本；电脑 App 与手机 App 之间请用发送面板「剪贴板」投递</div>
            </div>
            <button class="switch" :class="{ on: clipSyncEnabled }" role="switch" :aria-checked="clipSyncEnabled"
                    :aria-label="clipSyncEnabled ? '关闭系统剪贴板自动同步' : '开启系统剪贴板自动同步'"
                    :title="clipSyncEnabled ? '点击关闭' : '点击开启'" @click="toggleClipSync">
              <span class="knob"></span>
            </button>
          </div>
          <div class="setting-row">
            <div class="setting-info">
              <div class="setting-title">悬浮二维码</div>
              <div class="setting-desc">开启后在电脑桌面界面常驻展示，可拖动、隐藏或关闭</div>
            </div>
            <button class="switch" :class="{ on: qrFloatEnabled }" role="switch" :aria-checked="qrFloatEnabled"
                    aria-label="悬浮二维码" :title="qrFloatEnabled ? '点击关闭' : '点击开启'" @click="toggleQrFloatEnabled">
              <span class="knob"></span>
            </button>
          </div>
        </div>
        <!-- Android/iOS App: 按 dir-5 设计稿（LOCAL / CONFIG）收口 -->
        <div v-else-if="mobileShell" class="m-settings">
          <div class="m-setting-row">
            <div class="m-setting-main">
              <b>访问令牌保护</b>
              <small>同 Wi-Fi 设备需配对码</small>
            </div>
            <button class="switch" :class="{ on: pinEnabled }" role="switch" :aria-checked="pinEnabled"
                    :aria-label="pinEnabled ? '关闭访问令牌保护' : '开启访问令牌保护'"
                    :title="pinEnabled ? '点击关闭' : '点击开启'" @click="toggleAccessPin">
              <span class="knob"></span>
            </button>
          </div>
          <div class="m-setting-row">
            <div class="m-setting-main">
              <b>系统剪贴板自动同步</b>
              <small>电脑 ↔ 手机复制文本</small>
            </div>
            <button class="switch" :class="{ on: clipSyncEnabled }" role="switch" :aria-checked="clipSyncEnabled"
                    :aria-label="clipSyncEnabled ? '关闭系统剪贴板自动同步' : '开启系统剪贴板自动同步'"
                    :title="clipSyncEnabled ? '点击关闭' : '点击开启'" @click="toggleClipSync">
              <span class="knob"></span>
            </button>
          </div>
          <div class="m-setting-row">
            <div class="m-setting-main">
              <b>本机访问令牌</b>
              <small>当前令牌</small>
              <button v-if="pinEnabled && pinCode" class="m-token" title="点击复制令牌"
                      :aria-label="'复制令牌 ' + pinCode" @click="copyText(pinCode, '令牌已复制')">{{ pinCode }}</button>
              <span v-else class="m-token m-token-off">{{ pinEnabled ? "读取中…" : "未开启" }}</span>
            </div>
          </div>
          <div class="m-setting-row m-setting-appearance">
            <div class="m-setting-main">
              <b>外观</b>
              <small>{{ themeModeName }} · {{ currentThemeName }}</small>
              <div class="m-theme-row" role="group" aria-label="配色">
                <button v-for="t in THEMES" :key="t.id" class="theme-sw" :class="{ active: theme === t.id }"
                        :style="{ background: t.preview }" :title="t.name" :aria-label="t.name"
                        :aria-pressed="theme === t.id" @click="setTheme(t.id)"></button>
              </div>
              <div class="m-mode-row" role="group" aria-label="主题模式">
                <button v-for="m in THEME_MODES" :key="m.id" class="m-mode-btn" :class="{ on: themeMode === m.id }"
                        :aria-pressed="themeMode === m.id" @click="setThemeMode(m.id)">{{ m.name }}</button>
              </div>
            </div>
          </div>
          <div class="m-version">
            <span>CC FLASH / LOCAL NODE</span>
            <b>v{{ curVersion() }}</b>
          </div>
        </div>
        <template v-if="isRealDesktop">
        <!-- 皮肤设置: 两行"标签+控件"(外观/配色), 色板芯片即按钮本体 -->
        <div class="card svc-skin">
          <h3>皮肤设置</h3>
          <div class="skin-row">
            <span class="skin-label">外观</span>
            <div class="mode-row" role="group" aria-label="主题模式">
              <button v-for="m in THEME_MODES" :key="m.id" class="mode-btn" :class="{ active: themeMode === m.id }"
                      :aria-pressed="themeMode === m.id" @click="setThemeMode(m.id)">
                <svg v-if="m.id === 'light'" class="mode-ico" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/></svg>
                <svg v-else-if="m.id === 'dark'" class="mode-ico" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20.5 14.5A8.5 8.5 0 0 1 9.5 3.5a8.5 8.5 0 1 0 11 11Z"/></svg>
                <svg v-else class="mode-ico" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="4" width="18" height="12" rx="2"/><path d="M2 20h20"/><path d="M9 8h6"/></svg>
                <span>{{ m.name }}</span>
              </button>
            </div>
          </div>
          <div class="skin-row">
            <span class="skin-label">配色</span>
            <div class="theme-row">
              <button v-for="t in THEMES" :key="t.id" class="theme-sw" :class="{ active: theme === t.id }"
                      :style="{ background: t.preview }" :title="t.name" :aria-label="t.name"
                      :aria-pressed="theme === t.id" @click="setTheme(t.id)"></button>
              <span class="skin-cur">当前 · {{ currentThemeName }}</span>
            </div>
          </div>
          <div class="sub-hint">跟随系统随系统外观自动切换；即时生效并自动记忆</div>
        </div>
        <!-- 关于 -->
        <div class="card svc-about">
          <h3>关于</h3>
          <p class="about-text">CC Flash（曹操是快😂）—— 局域网文件互传工具：电脑启动服务，手机/平板/电脑浏览器打开地址即可上传/下载/分享/聊天。</p>
          <div class="about-ver">
            <span class="ver-chip">当前版本 v{{ curVersion() }}</span>
            <button class="btn btn-ghost" style="height:28px" :disabled="checkingUpdate" @click="checkUpdate(true)">
              {{ checkingUpdate ? "检查中…" : "检查更新" }}
            </button>
          </div>
        </div>
        </template>
      </template>

      <!-- 页脚: 版权(内容不满一屏时贴底)。浏览器/移动壳所有页面都显示(含传输页),
          桌面端仅 CC设置页保留 -->
      <!-- 手机浏览器不占底部版权条: 把高度全部留给输入栏与聊天区 -->
      <footer v-if="(!isDesktop && !isNarrow) || (isRealDesktop && view === 'settings')" class="app-foot">{{ FOOTER_TEXT }}</footer>
    </main>

    <!-- ===== 手机 App 四页导航: SEND / DEVICES / FILES / CONFIG ===== -->
    <nav v-if="mobileTabbed && !isQrPetWindow" class="mobile-tabs" role="tablist" aria-label="主导航">
      <button class="mtab" :class="{ on: mobileTab === 'send' }" role="tab" title="发送"
              :aria-selected="mobileTab === 'send'" @click="setMobileTab('send')">
        <span class="mtab-label">发送</span>
        <i v-if="unread > 0" class="mtab-dot unread">{{ unread > 99 ? "99+" : unread }}</i>
      </button>
      <button class="mtab" :class="{ on: mobileTab === 'devices' }" role="tab" title="设备"
              :aria-selected="mobileTab === 'devices'" @click="setMobileTab('devices')">
        <span class="mtab-label">设备</span>
      </button>
      <button class="mtab" :class="{ on: mobileTab === 'files' }" role="tab" title="文件"
              :aria-selected="mobileTab === 'files'" @click="setMobileTab('files')">
        <span class="mtab-label">文件</span>
      </button>
      <button class="mtab" :class="{ on: mobileTab === 'config' }" role="tab" title="设置"
              :aria-selected="mobileTab === 'config'" @click="setMobileTab('config')">
        <span class="mtab-label">设置</span>
      </button>
    </nav>
  </div>

  <!-- 独立桌面 Pet 窗：贴边收拢的呼吸圆点，悬停预览扫码卡；显隐只由 CC设置开关控制 -->
  <template v-if="isQrPetWindow">
    <div class="qr-pet"
         :class="[{ down: !running || !qrData, expanded: qrPetShown }, 'edge-' + petEdge]"
         role="button" tabindex="0"
         :title="qrPetShown ? '' : (petNotify ? '有新文件或消息 · 点按查看' : '')"
         :aria-label="(petNotify ? '有新文件或消息。' : '') + 'CC Flash 悬浮二维码：指针停留 1.5 秒预览，拖动调整位置，点按打开主窗口'"
         @pointerdown="startPetShell" @pointermove="petShellMove" @pointerup="petShellUp" @click="petClick"
         @mouseenter="petEnter" @mouseleave="petLeave"
         @keydown.enter.prevent="petClick" @keydown.space.prevent="petClick">
      <div class="qr-pet-shell">
        <!-- 单圈珊瑚表环 + 绿点同组呼吸: 缩放作用于整组, 绿点随环一起缩放(一个整体);
             pet-ring-rot 按吸附边把绿点旋到屏幕可见的半侧(圆对称, 旋转本身不可见) -->
        <svg class="qr-pet-cc" viewBox="0 0 64 64" fill="none" aria-hidden="true">
          <g class="pet-ring-rot">
            <g class="pet-ring-breath">
              <circle class="qr-pet-c-outer" cx="32" cy="32" r="30.2" />
              <circle v-if="petNotify" class="pet-ring-dot" cx="53.3" cy="10.7" r="3.8"
                      fill="#3ddc97" stroke="rgba(11,14,19,.95)" stroke-width="2" />
            </g>
          </g>
        </svg>
        <!-- 取景框标记(四角+中心镜): 比字体 ◎ 更精密, 一眼读出"扫码" -->
        <span class="qr-pet-avatar" aria-hidden="true">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"
               stroke-linecap="round" stroke-linejoin="round">
            <path d="M4 9.5V6.5A2.5 2.5 0 0 1 6.5 4H9.5" />
            <path d="M14.5 4h3A2.5 2.5 0 0 1 20 6.5v3" />
            <path d="M20 14.5v3a2.5 2.5 0 0 1-2.5 2.5h-3" />
            <path d="M9.5 20h-3A2.5 2.5 0 0 1 4 17.5v-3" />
            <!-- CC 品牌字标居中(替代中心镜): 取景框=扫码语义, CC=品牌, 融为一枚徽章 -->
            <text x="12" y="12" text-anchor="middle" dominant-baseline="central"
                  font-size="7.5" font-weight="800" letter-spacing="-0.4"
                  fill="currentColor" stroke="none">CC</text>
          </svg>
        </span>
      </div>
      <div class="qr-pet-panel">
        <div class="qr-pet-head"><span class="qr-pet-panel-dot" v-if="petNotify" aria-hidden="true"></span>{{ running ? (qrData ? "浏览器扫一扫" : "安全模式 · 仅本机") : "服务已停止" }}</div>
        <div class="qr-pet-view">
          <img v-if="running && qrData" class="qr-pet-img" :src="qrData" alt="扫码打开" />
          <span v-if="running && qrData" class="qr-pet-scan" aria-hidden="true"></span>
          <!-- 停止态空占位: 暗底虚线框 + 静默取景框(替换刺眼的白块◎) -->
          <span v-else class="qr-pet-empty" aria-hidden="true">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"
                 stroke-linecap="round" stroke-linejoin="round">
              <path d="M4 9.5V6.5A2.5 2.5 0 0 1 6.5 4H9.5" />
              <path d="M14.5 4h3A2.5 2.5 0 0 1 20 6.5v3" />
              <path d="M20 14.5v3a2.5 2.5 0 0 1-2.5 2.5h-3" />
              <path d="M9.5 20h-3A2.5 2.5 0 0 1 4 17.5v-3" />
              <circle cx="12" cy="12" r="3.1" />
            </svg>
          </span>
        </div>
        <div class="qr-pet-cap">{{ running && qrData ? "同一 WI-FI 下 扫码打开" : "打开主窗口启动服务" }}</div>
      </div>
    </div>
  </template>

  <!-- 软件介绍页(桌面端全屏 overlay, 按 docs/软件介绍页设计稿.html 实现); v-if 延迟到首次打开才挂载 -->
  <Transition name="intro">
    <div v-if="(isDesktop || PREVIEW_MOBILE) && introShow" id="introOverlay" class="intro-overlay" role="dialog" aria-label="CC Flash 软件介绍">
    <!-- 粘性顶栏 -->
    <header class="ihead">
      <div class="ihead-in">
        <span class="ihead-brand" title="回到顶部" @click="introScrollTop">
          <img class="ib-lg" src="/logo.png" alt="logo" /> CC Flash
        </span>
        <nav class="inav">
          <a href="#sec-feat" :class="{ active: introSec === 'feat' }" @click.prevent="introScrollTo('feat')">功能简介</a>
          <a href="#sec-howto" :class="{ active: introSec === 'howto' }" @click.prevent="introScrollTo('howto')">如何使用</a>
          <a href="#sec-safe" :class="{ active: introSec === 'safe' }" @click.prevent="introScrollTo('safe')">技术实现</a>
          <a href="#sec-faq" :class="{ active: introSec === 'faq' }" @click.prevent="introScrollTo('faq')">常见问题</a>
        </nav>
        <div class="ihead-act">
          <button class="btn btn-primary" @click="goServiceFromIntro">
            {{ running ? "打开服务中心" : "去启动服务" }}
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
        <h1>文件传过去，<br /><span class="hl">同一个 Wi-Fi</span> 就够了。</h1>
        <p class="lead">
          电脑启动服务，手机、平板、同事的电脑扫码即用。
          数据只在局域网流动，不上云、无账号，断外网也能传。
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
          <span class="feat-chip"><span class="tick">✓</span>多台设备同时发送</span>
          <span class="feat-chip"><span class="tick">✓</span>文件消息刷新不丢</span>
          <span class="feat-chip"><span class="tick">✓</span>可选访问令牌</span>
        </div>
      </div>
      <!-- 应用窗口示意(二维码为真实地址, 服务未启动时占位) -->
      <div class="hero-visual" aria-hidden="true">
        <div class="hv-bar">
          <i></i><i></i><i></i>
          <span class="hv-url">{{ running ? shareUrl : "http://192.168.1.5:1688" }}</span>
          <span class="hv-run"><i></i>{{ running ? "运行中" : "示例" }}</span>
        </div>
        <div class="hv-body">
          <div class="hv-qr">
            <img v-if="qrData" class="hv-qr-img" :src="qrData" alt="二维码，扫码访问本机" />
            <div v-else class="hv-qr-ph"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="7" y="2.5" width="10" height="19" rx="2.2"/><path d="M10.5 5h3"/><circle cx="12" cy="18.2" r=".8" fill="currentColor" stroke="none"/></svg>扫码打开</div>
          </div>
          <div class="hv-files">
            <div class="hv-file">
              <span class="fi" style="background:linear-gradient(135deg,#c73d3d,#a82d2d)">PDF</span>
              <div style="min-width:0"><div class="fn">需求评审纪要.pdf</div><div class="fm">2.4 MB · 电脑 → 手机</div></div>
              <span class="fp">100%</span>
            </div>
            <div class="hv-file">
              <span class="fi" style="background:linear-gradient(135deg,#3468c9,#244f9e)">JPG</span>
              <div style="min-width:0"><div class="fn">现场照片_01.jpg</div><div class="fm">5.1 MB · 手机 → 电脑</div></div>
              <span class="fp">100%</span>
            </div>
            <div class="hv-file">
              <span class="fi" style="background:linear-gradient(135deg,#1e8449,#196f3d)">ZIP</span>
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
      <h2>同一 Wi-Fi，一个入口。</h2>
      <p class="lead">文件、消息、剪贴板，一个页面完成；手机浏览器打开就能用。</p>
      <div class="cards">
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 01-2 2H7l-4 4V5a2 2 0 012-2h14a2 2 0 012 2z"/></svg></div>
          <h3>文件与消息</h3>
          <p>文件和文字在同一条对话里：拖入即上传、自动生成文件卡片，支持 Markdown 与图片/PDF/音视频预览。文件消息会持久化，刷新浏览器或重启 App 后仍保留；共享文件统一收进文件页，排序、分页、批量删除都在。</p>
          <div class="tags"><span class="tag">文件即消息</span><span class="tag">持久化历史</span><span class="tag">Markdown</span><span class="tag">应用内预览</span><span class="tag">共享文件</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg></div>
          <h3>服务中心</h3>
          <p>启停服务、端口自动顺延、共享目录与磁盘余量集中管理。桌面常驻/悬浮二维码随时扫码，实时日志与六套配色一键切换。</p>
          <div class="tags"><span class="tag">服务控制</span><span class="tag">悬浮扫码</span><span class="tag">多主题配色</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 01-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z"/></svg></div>
          <h3>CC设置</h3>
          <p>开机自启、托盘常驻、一键检查更新；访问令牌可随时开启/关闭，手机设置页用 dir-5 信号台排版，令牌大号展示、点按即复制。关窗不关服务，手机端始终可用。</p>
          <div class="tags"><span class="tag">开机自启</span><span class="tag">令牌开关</span><span class="tag">dir-5 设置</span><span class="tag">检查更新</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="4" width="20" height="12" rx="2"/><line x1="8" y1="20" x2="16" y2="20"/><line x1="12" y1="16" x2="12" y2="20"/></svg></div>
          <h3>消息工作台</h3>
          <p>桌面左侧是周边设备栏，手机 App 四页切换。点设备只选中目标，不再自动弹面板；支持「多选」同时发给多台设备，文本与拖入文件依次投递。状态实时同步，窄屏自动收起，浏览器保持简洁聊天室。</p>
          <div class="tags"><span class="tag">设备即目标</span><span class="tag">多选直发</span><span class="tag">直发回执</span><span class="tag">浏览器零打扰</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="7" y="2.5" width="10" height="19" rx="2.2"/><path d="M10.5 5h3"/><circle cx="12" cy="18.2" r=".8" fill="currentColor" stroke="none"/></svg></div>
          <h3>手机 App · 信号台</h3>
          <p>发送 / 设备 / 文件三页按 dir-2 信号台重排：等宽读数、刻度雷达盘、RSSI 设备行、manifest 序号与进度条。Wi-Fi/CPU 锁 + 忽略电池优化，熄屏后仍可被找到。</p>
          <div class="tags"><span class="tag">dir-2 三页</span><span class="tag">多选发送</span><span class="tag">熄屏保活</span><span class="tag">本机节点</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="8" y="2" width="8" height="4" rx="1"/><path d="M16 4h2a2 2 0 012 2v14a2 2 0 01-2 2H6a2 2 0 01-2-2V6a2 2 0 012-2h2"/><path d="M9 12h6"/><path d="M9 16h4"/></svg></div>
          <h3>共享剪贴板</h3>
          <p><b>默认关闭</b>；开启后才与打开本机地址的浏览器/设备共享文本，聊天框粘贴才会回传。关闭时粘贴只填充输入框，不会把未发送内容写进共享槽。电脑 App 与手机 App 独立，跨 App 请用发送面板「剪贴板」投递。</p>
          <div class="tags"><span class="tag">默认关闭</span><span class="tag">开启后才回传</span><span class="tag">跨 App 用发送面板</span><span class="tag">浏览器兜底</span></div>
        </div>
      </div>
    </section>

    <!-- 如何使用 -->
    <section class="sec" id="sec-howto" data-sec="howto">
      <h2>三步，把内容交给对方。</h2>
      <p class="lead">无需配置网络、无需注册账号，装好即用。</p>
      <div class="steps">
        <div class="step">
          <div class="no">01</div>
          <h3>电脑启动服务</h3>
          <p>打开 CC Flash，点「启动服务」。默认端口 <code>1688</code>，被占用自动顺延。</p>
        </div>
        <div class="step">
          <div class="no">02</div>
          <h3>扫码或输地址</h3>
          <p>手机连同一 Wi-Fi，扫码或输入 <code>http://电脑IP:端口</code>，立即进入共享页。</p>
        </div>
        <div class="step">
          <div class="no">03</div>
          <h3>直接开始互传</h3>
          <p>拖文件或发消息；在设备栏/设备页选中目标即可直发，支持多选同时发多台。文件会同时进入对方共享目录与聊天并持久化，电脑关窗/手机熄屏服务也不停。</p>
        </div>
      </div>
    </section>

    <!-- 技术实现 / 可靠性 -->
    <section class="sec" id="sec-safe" data-sec="safe">
      <h2>传得快，也要传得安心。</h2>
      <p class="lead">传输边界清清楚楚：数据只在你的路由器内部流动，电脑在哪里，数据就到哪里。</p>
      <div class="points">
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg></div>
          <h3>数据只在局域网</h3>
          <p>服务只在局域网直达，不上云、不注册，断外网可用。移动端自动生成本机令牌，桌面可开启 6 位访问令牌；分享链接一次性有效，预览使用短时媒体令牌，长期 PIN 不进入 URL。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg></div>
          <h3>大文件不占内存</h3>
          <p>上传下载全程流式处理，大文件不占内存；失败/超时自动回滚半截文件。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="7" width="20" height="14" rx="2"/><path d="M16 21V5a2 2 0 00-2-2h-4a2 2 0 00-2 2v16"/></svg></div>
          <h3>服务常驻不断</h3>
          <p>关窗进托盘，服务继续运行；Android 使用 connectedDevice 前台服务 + Wi-Fi/CPU 锁，熄屏后仍可被发现；开机自启可手动开启。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="2" width="14" height="20" rx="2"/><line x1="12" y1="18" x2="12.01" y2="18"/></svg></div>
          <h3>多端覆盖</h3>
          <p>macOS、Windows、Android App，外加电脑与手机浏览器。Windows 安装包内置 WebView2 离线安装器；iOS 没有原生包，用 Safari 打开电脑地址即可。</p>
        </div>
      </div>
      <div class="codes">
        <div class="code-card">
          <h3>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>
            桌面端安装
          </h3>
          <p>前往 GitHub Release 下载对应平台安装包，安装即用 —— 前端已嵌入二进制，无需额外部署。未签名版本会标记为 prerelease，应用内更新检查会跳过，建议签名版发布后再升级。</p>
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
      <h2>常见问题，一次说清。</h2>
      <div class="faq">
        <div v-for="(f, i) in INTRO_FAQS" :key="i" class="faq-item" :class="{ open: faqOpen === i }">
          <button class="faq-q" @click="toggleFaq(i)">{{ f.q }}<span class="pm">+</span></button>
          <div class="faq-a" :style="{ maxHeight: faqOpen === i ? '900px' : '0' }"><p>{{ f.a }}</p></div>
        </div>
      </div>
    </section>

    <!-- 页脚 -->
    <footer class="ifoot">
      <div class="ifoot-in">
        <div class="ifoot-brand">
          <img class="ib-lg" src="/logo.png" alt="logo" /> CC Flash
        </div>
        <div class="ifoot-links">
          <a :href="'https://github.com/' + GITHUB_REPO" @click.prevent="openExternal('https://github.com/' + GITHUB_REPO)">GitHub</a>
          <a :href="'https://github.com/' + GITHUB_REPO + '/issues'" @click.prevent="openExternal('https://github.com/' + GITHUB_REPO + '/issues')">反馈问题</a>
          <a :href="RELEASES_PAGE" @click.prevent="openExternal(RELEASES_PAGE)">下载安装</a>
        </div>
        <div class="ifoot-copy">CC Flash（曹操是快😂）—— 局域网文件互传工具 · {{ FOOTER_TEXT }}</div>
      </div>
    </footer>
    </div>
  </Transition>

  <!-- 应用内文件预览: 全屏层 + 关闭按钮, 关闭即回到聊天页 -->
  <div v-if="previewName" class="pv-overlay" @click.self="closePreview">
    <div class="pv-head">
      <span class="pv-name" :title="previewName">{{ previewName.split("/").pop() }}</span>
      <button class="pv-close" aria-label="关闭预览" @click="closePreview">
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
        <span>关闭</span>
      </button>
    </div>
    <div class="pv-body">
      <img v-if="previewKind === 'image'" :src="previewUrl" alt="" @error="retryPreview" />
      <video v-else-if="previewKind === 'video'" :src="previewUrl" controls playsinline preload="metadata" @error="retryPreview"></video>
      <audio v-else-if="previewKind === 'audio'" :src="previewUrl" controls preload="metadata" @error="retryPreview"></audio>
      <iframe v-else-if="previewKind === 'pdf'" :src="previewUrl" title="PDF 预览"></iframe>
      <template v-else>
        <div v-if="previewLoading" class="pv-tip">加载中…</div>
        <div v-else-if="previewErr" class="pv-tip err">{{ previewErr }}</div>
        <pre v-else class="pv-text">{{ previewText }}</pre>
      </template>
      <div v-if="previewErr && previewKind !== 'text'" class="pv-tip err">{{ previewErr }}</div>
    </div>
  </div>

  <!-- 手机 App 首次运行: 是否允许同 Wi-Fi 设备发现本机 -->
  <div v-if="lanAskOpen" class="dialog-overlay" @click.self="dismissLanAsk">
    <div class="dialog-box lan-ask-box">
      <div class="lan-ask-ic" aria-hidden="true"><svg width="34" height="34" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4.9 9.9a10 10 0 0 1 14.2 0"/><path d="M7.8 13a6 6 0 0 1 8.4 0"/><path d="M10.6 16a2.2 2.2 0 0 1 2.8 0"/><circle cx="12" cy="19" r="1" fill="currentColor" stroke="none"/></svg></div>
      <h4>让别人发现这台手机？</h4>
      <p class="lan-ask-sub">开启后，同一 Wi-Fi 下的电脑/手机会在「周边设备」里看到本机，可以直接发文件或消息。手机本机服务默认强制启用访问令牌，其他设备需输入配对码。</p>
      <label v-if="!pinEnabled" class="lan-ask-pin">
        <input type="checkbox" v-model="lanAskPin" />
        <span>同时开启访问令牌保护（公共 Wi-Fi 推荐）</span>
      </label>
      <div class="dialog-actions lan-ask-acts">
        <button class="btn btn-ghost" @click="dismissLanAsk">仅本机使用</button>
        <button class="btn btn-primary" @click="acceptLanAsk">开启（推荐）</button>
      </div>
    </div>
  </div>

  <!-- 桌面 WebView 自定义右键菜单 -->
  <div v-if="contextMenu" class="ctx-menu" :style="{ left: contextMenu.x + 'px', top: contextMenu.y + 'px' }"
       role="menu" @click.stop @contextmenu.prevent>
    <template v-if="contextMenu.file">
      <button v-if="isPreviewable(contextMenu.file)" role="menuitem" @click="ctxFilePreview">预览文件</button>
      <button role="menuitem" @click="ctxFileOpen">{{ isRealDesktop ? "打开文件" : "下载文件" }}</button>
      <button v-if="isRealDesktop" role="menuitem" @click="ctxFileCopy">复制文件</button>
      <button role="menuitem" @click="ctxFileShare">分享链接</button>
      <button role="menuitem" @click="ctxFileLocate">定位文件</button>
      <span class="ctx-sep"></span>
    </template>
    <button role="menuitem" @click="ctxCopy">{{ contextMenu.hasSel ? "复制选中内容" : "复制页面地址" }}</button>
    <button v-if="view === 'transfer'" role="menuitem" @click="ctxPasteInput">粘贴到输入框</button>
    <button role="menuitem" @click="ctxManualSend">手动发送</button>
    <button role="menuitem" :disabled="!running" @click="ctxScan">重新扫描设备</button>
    <button role="menuitem" @click="ctxDiagnose">设备故障诊断</button>
  </div>

  <!-- 设备发现故障诊断: 按原因分类给出可执行方案 -->
  <div v-if="diagnosticsOpen" class="dialog-overlay" @click.self="closeDiagnostics">
    <div class="dialog-box diag-box">
      <header class="diag-head">
        <div><h4>设备发现故障诊断</h4><p>按原因逐项排查，通常 1 分钟内可以定位。</p></div>
        <button class="diag-x" title="关闭" aria-label="关闭" @click="closeDiagnostics">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
        </button>
      </header>
      <div class="diag-status">
        <span :class="running ? 'ok' : 'bad'">{{ running ? "服务运行中" : "服务未启动" }}</span>
        <span :class="lanActive ? 'ok' : 'bad'">{{ lanActive ? "局域网可见" : "仅本机" }}</span>
        <span v-if="discoveryInfo.port">UDP {{ discoveryInfo.port }}</span>
        <span v-if="discoveryInfo.ipv6">IPv6</span>
        <span v-if="lanIp && lanIp !== '--'">本机 {{ lanIp }}</span>
        <span>已发现 {{ peers.length }}</span>
      </div>
      <div class="diag-list">
        <div class="diag-item">
          <span class="diag-no">1</span>
          <div class="diag-body">
            <b>系统网络权限 / 防火墙</b>
            <p>没发现？请允许「本地网络」权限（macOS：系统设置 → 隐私与安全性 → 本地网络）；Windows 请允许专用网络，并放行 UDP 53318-53321。</p>
            <button v-if="showNetworkSettingsButton" class="diag-act" @click="openNetworkSettings">打开网络设置</button>
          </div>
        </div>
        <div class="diag-item">
          <span class="diag-no">2</span>
          <div class="diag-body">
            <b>不在同一 Wi-Fi / 子网 / AP 隔离</b>
            <p>确认设备连的是同一个 Wi-Fi 且在同一网段（如都是 192.168.1.x）。公司、酒店或访客网络可能禁止设备互访，或把有线/无线分在不同子网；此时扫码打开电脑地址，用电脑中转文件/消息。</p>
            <button class="diag-act" @click="diagOpenQr">显示二维码</button>
          </div>
        </div>
        <div class="diag-item">
          <span class="diag-no">3</span>
          <div class="diag-body">
            <b>企业 Wi-Fi 过滤多播</b>
            <p>如网络禁止多播/广播，可对对方 IP 发一次单播探测；仍不行时直接用 IP:端口手动发送。</p>
            <div class="diag-probe">
              <input v-model="probeIp" placeholder="对方 IP，如 192.168.1.8" inputmode="decimal"
                     @keydown.enter.prevent="probePeerByIp" />
              <button class="diag-act" :disabled="probeSending || !running" @click="probePeerByIp">{{ probeSending ? "探测中…" : "单播探测" }}</button>
            </div>
            <button class="diag-act" @click="diagManualSend">手动发送</button>
          </div>
        </div>
        <div class="diag-item">
          <span class="diag-no">4</span>
          <div class="diag-body">
            <b>服务未启动 / 仅本机模式</b>
            <p>手机 App 始终开放局域网，并自动生成本机访问令牌；同网段设备需配对码。电脑端请先启动服务。</p>
            <button class="diag-act" @click="diagGoService">{{ isRealDesktop ? "去服务中心" : "去设置" }}</button>
          </div>
        </div>
        <div class="diag-item" v-if="discoveryInfo.error">
          <span class="diag-no warn">!</span>
          <div class="diag-body">
            <b>发现端口 / 多播初始化异常</b>
            <p>{{ discoveryInfo.error }}</p>
          </div>
        </div>
        <div class="diag-item">
          <span class="diag-no">5</span>
          <div class="diag-body">
            <b>对方尚未允许被发现</b>
            <p>确认另一台设备打开了 CC Flash（桌面端或手机 App），并且没有停在本机安全模式。</p>
          </div>
        </div>
      </div>
      <div class="diag-actions">
        <button class="btn btn-ghost" @click="closeDiagnostics">关闭</button>
        <button class="btn btn-primary" :disabled="!running" @click="refreshDiscover(); closeDiagnostics()">重新扫描</button>
      </div>
    </div>
  </div>

  <!-- 发送到设备 / 手动发送: 与设计稿一致的 LocalSend 式发送面板 -->
  <div v-if="manualSendOpen" class="send-dialog-overlay" @click.self="closeManualSend">
    <section class="send-dialog" role="dialog" aria-modal="true" :aria-label="'发送到 ' + sendDialogName">
      <header class="send-dialog-head">
        <div class="send-dialog-title">
          <h4>发送到 <b>{{ sendDialogName }}</b></h4>
          <p>发送后进入对方共享目录与聊天</p>
        </div>
        <button class="send-dialog-x" title="关闭" aria-label="关闭" @click="closeManualSend">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
        </button>
      </header>
      <div v-if="!sendDialogTarget" class="send-dialog-addr">
        <input class="send-dialog-addr-input" v-model="manualAddr" placeholder="IP 或 IP:端口，例如 10.161.118.123"
               autocomplete="off" spellcheck="false" @keydown.enter.prevent="submitManualSend" />
      </div>
      <div class="send-tabs" role="tablist" aria-label="发送内容">
        <button :class="{ on: sendTab === 'files' }" role="tab" :aria-selected="sendTab === 'files'" @click="setSendTab('files')">文件</button>
        <button :class="{ on: sendTab === 'folder' }" role="tab" :aria-selected="sendTab === 'folder'" @click="setSendTab('folder')">文件夹</button>
        <button :class="{ on: sendTab === 'text' }" role="tab" :aria-selected="sendTab === 'text'" @click="setSendTab('text')">文本</button>
        <button :class="{ on: sendTab === 'clipboard' }" role="tab" :aria-selected="sendTab === 'clipboard'" @click="setSendTab('clipboard')">剪贴板</button>
      </div>
      <div class="send-dialog-body">
        <template v-if="sendTab === 'files' || sendTab === 'folder'">
          <div class="send-drop" @click="sendTab === 'folder' ? pickManualFolder() : pickManualFiles()"
               @dragover.prevent @drop.prevent="onSendDrop">
            <div class="send-drop-ic" aria-hidden="true">
              <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 16V4"/><polyline points="6 10 12 4 18 10"/><path d="M4 20h16"/></svg>
            </div>
            <b>拖入文件，或点击选择</b>
            <span>支持多文件 / 文件夹</span>
            <span>内容会进入对方共享目录与聊天，同网段可见</span>
            <div class="send-quick" @click.stop>
              <button @click.stop="pickManualFiles">文件</button>
              <button @click.stop="setSendTab('clipboard')">粘贴</button>
              <button @click.stop="pickManualPhotos">相册</button>
              <button @click.stop="pickManualFolder">文件夹</button>
            </div>
          </div>
          <div v-if="manualFiles.length" class="send-file-list">
            <div v-for="(f, i) in manualFiles" :key="i" class="send-file">
              <span class="send-file-ic">{{ (String(f.name).split('.').pop() || 'FILE').slice(0, 4).toUpperCase() }}</span>
              <span class="send-file-name" :title="f.name">{{ f.name }}</span>
              <span class="send-file-size">{{ fmtSize(f.size) }}</span>
              <button title="移除" aria-label="移除" @click.stop="removeManualFile(i)">✕</button>
            </div>
          </div>
        </template>
        <template v-else>
          <textarea class="send-dialog-text" v-model="manualText" :maxlength="cfg.msgMax" rows="6"
                    :placeholder="sendTab === 'clipboard' ? '剪贴板内容…' : '输入要发送的文本…'"></textarea>
          <button v-if="sendTab === 'clipboard'" class="send-clip-load" @click="loadClipboardForSend">读取当前剪贴板</button>
        </template>
      </div>
      <footer class="send-dialog-foot">
        <span class="send-dialog-status">已选择 <b>{{ sendDialogName }}</b> · {{ sendDialogStatus }}</span>
        <button class="send-dialog-send" :disabled="!sendDialogReady" @click="submitManualSend">
          {{ manualSending ? "发送中…" : "发送" }}
        </button>
      </footer>
      <input type="file" id="manualFileInput" multiple hidden @change="onManualFiles" />
      <input type="file" id="manualDirInput" webkitdirectory multiple hidden @change="onManualFiles" />
      <input type="file" id="manualPhotoInput" accept="image/*" multiple hidden @change="onManualFiles" />
    </section>
  </div>

  <!-- 直发设备开启访问令牌: 输入对方配对码 -->
  <div v-if="pinDialogDevice" class="dialog-overlay" @click.self="pinDialogDevice = null">
    <div class="dialog-box pin-box">
      <div class="dialog-ic" aria-hidden="true">
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="10" width="16" height="11" rx="2.5"/><path d="M8 10V7a4 4 0 018 0v3"/><circle cx="12" cy="15.5" r="1.3" fill="currentColor" stroke="none"/><path d="M12 16.8v1.6"/></svg>
      </div>
      <h4>需要对方配对码</h4>
      <p class="pin-sub">「{{ pinDialogDevice.nickname || "设备" }}」开启了访问令牌</p>
      <input class="svc-input pin-input" v-model="pinDialogValue" maxlength="12" placeholder="配对码"
             autocomplete="off" inputmode="numeric" @keydown.enter.prevent="savePeerPinDialog" />
      <button class="btn btn-primary pin-btn" :disabled="pinChecking" @click="savePeerPinDialog">{{ pinChecking ? "验证中…" : "保 存" }}</button>
      <button class="pin-cancel" @click="pinDialogDevice = null">取消</button>
    </div>
  </div>

  <!-- 二维码弹窗 -->
  <div v-if="qrShow" class="dialog-overlay" @click.self="qrShow = false">
    <div class="dialog-box">
      <h4>扫描二维码在手机上打开</h4>
      <img v-if="qrData" class="qr-img" :src="qrData" alt="二维码，扫码访问本机" />
      <div v-else class="qr-empty" style="margin:10px auto">{{ running ? (lanActive ? "二维码生成中…" : "仅本机模式：请重启服务") : "服务已停止" }}</div>
      <p style="word-break:break-all">{{ shareUrl }}</p>
      <p class="qr-tip">{{ TIP_QR }}</p>
      <div class="dialog-actions"><button class="btn btn-primary" @click="qrShow = false">关闭</button></div>
    </div>
  </div>

  <!-- 昵称设置(合并页: 从房间头头像进入, 保留原聊天页昵称能力) -->
  <div v-if="nickOpen" class="dialog-overlay" @click.self="nickOpen = false">
    <div class="dialog-box">
      <h4>设置昵称</h4>
      <p style="font-size:12.5px;color:var(--text2)">昵称会显示在消息与文件卡片旁，自动记住。</p>
      <input class="svc-input" v-model="nickDraft" :placeholder="cfg.defaultNick" :maxlength="cfg.nickMax"
             style="width:100%;box-sizing:border-box;text-align:center" @keydown.enter.prevent="saveNickAndClose" />
      <div class="dialog-actions" style="margin-top:14px">
        <button class="btn btn-ghost" @click="nickOpen = false">取消</button>
        <button class="btn btn-primary" @click="saveNickAndClose">保存</button>
      </div>
    </div>
  </div>

  <!-- 删除确认: 与统一确认框同款结构, 图标/间距/按钮布局一致 -->
  <div v-if="showDel" class="dialog-overlay app-confirm-overlay" @click.self="showDel = false">
    <div class="dialog-box app-confirm danger" role="alertdialog" aria-modal="true">
      <div class="app-confirm-ic" aria-hidden="true">
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16"/><path d="M9 7V5h6v2"/><path d="M7 7l1 13h8l1-13"/><line x1="10" y1="11" x2="10" y2="16"/><line x1="14" y1="11" x2="14" y2="16"/></svg>
      </div>
      <h4>删除文件</h4>
      <p class="app-confirm-msg">{{ delTargets.length > 1
        ? `确定删除选中的 ${delTargets.length} 个文件吗？此操作不可撤销。`
        : `确定删除 ${delTargets[0]} 吗？此操作不可撤销。` }}</p>
      <div class="dialog-actions">
        <button class="btn btn-ghost" @click="showDel = false">取消</button>
        <button class="btn btn-danger" @click="doDelete">删除</button>
      </div>
    </div>
  </div>

  <!-- 统一应用内确认框(替代系统原生 ask/confirm) -->
  <div v-if="confirmState" class="dialog-overlay app-confirm-overlay" @click.self="resolveConfirm(false)">
    <div class="dialog-box app-confirm" :class="{ danger: confirmState.danger }" role="alertdialog" aria-modal="true">
      <div class="app-confirm-ic" aria-hidden="true">
        <svg v-if="confirmState.icon === 'trash'" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16"/><path d="M9 7V5h6v2"/><path d="M7 7l1 13h8l1-13"/><line x1="10" y1="11" x2="10" y2="16"/><line x1="14" y1="11" x2="14" y2="16"/></svg>
        <svg v-else-if="confirmState.icon === 'shield'" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3l7 3v5c0 4.6-2.8 7.9-7 10-4.2-2.1-7-5.4-7-10V6z"/><path d="M9.4 9.4l5.2 5.2M14.6 9.4l-5.2 5.2"/></svg>
        <svg v-else width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3.5l9 16H3z"/><line x1="12" y1="9.5" x2="12" y2="14.5"/><circle cx="12" cy="17.2" r=".9" fill="currentColor" stroke="none"/></svg>
      </div>
      <h4>{{ confirmState.title }}</h4>
      <p class="app-confirm-msg">{{ confirmState.message }}</p>
      <div class="dialog-actions">
        <button class="btn btn-ghost" @click="resolveConfirm(false)">{{ confirmState.cancelText }}</button>
        <button class="btn" :class="confirmState.danger ? 'btn-danger' : 'btn-primary'" @click="resolveConfirm(true)">{{ confirmState.confirmText }}</button>
      </div>
    </div>
  </div>

  <!-- 传输历史: 入口在本机身份卡右侧, 不再占用设备栏底部空间 -->
  <div v-if="historyOpen" class="dialog-overlay history-overlay" :class="{ 'is-mobile': mobileShell }" @click.self="historyOpen = false">
    <div class="dialog-box history-box">
      <div class="history-head">
        <div>
          <h4>传输历史</h4>
          <p class="history-sub">
            {{ historyQuery.trim() ? `匹配 ${filteredHistory.length} 条` : `最近 ${transferHistory.length} 条直发记录` }}
          </p>
        </div>
        <button class="history-close" title="关闭" aria-label="关闭" @click="historyOpen = false">
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><path d="M6 6l12 12M18 6L6 18"/></svg>
        </button>
      </div>
      <div class="history-tools">
        <input class="history-search" v-model="historyQuery" placeholder="搜索设备、内容或类型" aria-label="搜索传输历史"
               autocomplete="off" spellcheck="false" @keydown.esc="historyQuery = ''" />
        <button class="history-clear" :disabled="!transferHistory.length" @click="clearTransferHistory">清空</button>
      </div>
      <div v-if="!transferHistory.length" class="history-empty">
        还没有传输记录
        <small>给附近设备发消息或文件后会记录在这里</small>
      </div>
      <div v-else-if="!filteredHistory.length" class="history-empty">
        没有匹配的记录
        <small>换个关键词试试</small>
      </div>
      <div v-else class="history-list">
        <div v-for="h in filteredHistory" :key="h.id" class="history-row">
          <span class="history-ico">
            <svg v-if="h.kind === 'file'" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 11.5l-8.5 8.5a5 5 0 01-7.1-7.1l8.5-8.5a3.5 3.5 0 015 5L10.4 18a2 2 0 01-2.8-2.8l7.8-7.8"/></svg>
            <svg v-else width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 11.5a8.4 8.4 0 01-8.5 8.5 9 9 0 01-3.8-.8L3 21l1.8-5.1A8.4 8.4 0 013.5 12 8.5 8.5 0 0112 3.5 8.4 8.4 0 0121 11.5z"/></svg>
          </span>
          <span class="history-main">
            <b>{{ h.targetName }}</b>
            <small>{{ h.kind === "file" ? "文件" : "消息" }} · {{ h.name }}</small>
          </span>
          <time class="history-time">{{ fmtTime(h.at) }}</time>
        </div>
      </div>
    </div>
  </div>

  <!-- 配对弹框(网页端): 电脑端开启访问令牌保护后, 401 引导到此输入一次 -->
  <div v-if="needPin" class="dialog-overlay">
    <div class="dialog-box pin-box">
      <div class="dialog-ic" aria-hidden="true">
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="10" width="16" height="11" rx="2.5"/><path d="M8 10V7a4 4 0 018 0v3"/><circle cx="12" cy="15.5" r="1.3" fill="currentColor" stroke="none"/><path d="M12 16.8v1.6"/></svg>
      </div>
      <h4>需要配对</h4>
      <p class="pin-sub">输入电脑「访问令牌」完成配对</p>
      <input class="svc-input pin-input" v-model="pinInput" maxlength="12" placeholder="配对码"
             autocomplete="off" inputmode="numeric"
             @keydown.enter.prevent="submitPin" />
      <button class="btn btn-primary pin-btn" @click="submitPin">配 对</button>
      <button class="btn btn-ghost" style="margin-top:8px;width:100%" title="60 秒内不再自动弹出" @click="snoozePin">稍后再输入</button>
    </div>
  </div>

  <!-- 失联全屏(网页端): 电视"无信号"测试卡样式, 电脑/手机浏览器表现一致 -->
  <div v-if="!isRealDesktop && offline" class="nosignal-overlay">
    <img class="ns-img" :src="noSignalImg" alt="无信号测试卡" />
    <h3><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4.9 9.9a10 10 0 0 1 14.2 0"/><path d="M7.8 13a6 6 0 0 1 8.4 0"/><path d="M10.6 16a2.2 2.2 0 0 1 2.8 0"/><circle cx="12" cy="19" r="1" fill="currentColor" stroke="none"/></svg> 无信号</h3>
    <p class="ns-line">{{ mobileShell ? "与本机服务的连接已断开" : "与服务的连接已断开 —— 服务可能已停止，或电脑地址已变更" }}</p>
    <p class="ns-sub">{{ mobileShell ? "本地服务可能正在重启，请稍候或点下方重试" : "请在电脑上查看最新地址后重新打开本页" }}</p>
    <button v-if="mobileShell" class="btn btn-primary" style="margin-top:18px" @click="retryMobileLocal">重新启动本机服务</button>
  </div>

  <!-- 版本更新弹框(桌面端) -->
  <div v-if="showUpdate && !isQrPetWindow" class="dialog-overlay" @click.self="showUpdate = false">
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
      <h4>CC Flash 已开启开机启动</h4>
      <p>登录电脑后，CC Flash 将在后台自动启动（不弹出主窗口）。请从「应用程序」目录中的正式安装版开启 —— 若当前从 DMG 或开发目录运行，登录自启会指向该临时路径而失效。可随时在「CC设置」中关闭。</p>
      <div class="dialog-actions">
        <button class="btn btn-primary" @click="showAutostartTip = false">知道了</button>
      </div>
    </div>
  </div>

  <!-- 复制失败兜底: 内容可选中, 用户手动复制(分享链接冷路径不再"无路可走") -->
  <div v-if="copyFallbackShow" class="dialog-overlay" @click.self="copyFallbackShow = false">
    <div class="dialog-box">
      <h4>复制失败</h4>
      <p style="font-size:12.5px;color:var(--text2)">请手动选择下面的内容复制（点「全选」后按 Ctrl/Cmd+C）：</p>
      <textarea class="svc-input" readonly :value="copyFallbackText"
                style="width:100%;box-sizing:border-box;height:120px;resize:none;margin-top:6px;font-size:12px"></textarea>
      <div class="dialog-actions">
        <button class="btn btn-ghost" @click="selectCopyFallback">全选</button>
        <button class="btn btn-primary" @click="copyFallbackShow = false">关闭</button>
      </div>
    </div>
  </div>

  <!-- Toast -->
  <div class="toast-container" role="status" aria-live="polite" aria-atomic="false">
    <div v-for="t in toasts" :key="t.id" class="toast-msg" :class="'toast-' + t.type">
      <span>{{ t.msg }}</span>
      <button v-if="t.action" type="button" class="toast-act" @click="runToastAction(t)">{{ t.action.label }}</button>
    </div>
  </div>
</template>
