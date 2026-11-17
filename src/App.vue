<script setup>
import { ref, computed, watch, onMounted, onUnmounted, nextTick } from "vue";
// 只用到 toDataURL: 命名导入让 Vite 有机会 tree-shake 掉 SVG/终端渲染器
import { toDataURL as qrToDataURL } from "qrcode";
// 失联提示图: 构建期内联为 base64, 服务停止后无需再发请求。
// 用 JPEG(59KB)而非 PNG(133KB): 该图无透明像素, 内联后直接减轻 JS/二进制体积。
import noSignalImg from "./assets/no-signal.jpg";
// 可调参数集中管理(轮询/阈值/路径/文案等)
import {
  API, APP_VERSION_FALLBACK, AVATAR_POOL, BLOB_REVOKE_MS, COPIED_RESET_MS, CONTROL_RETRY_MS,
  CONTROL_RETRY_TIMES, DEFAULT_NICK, DEFAULT_PORT, FOOTER_TEXT, ICON_MAP, IMG_EXTS, LS, MSG_MAX,
  GITHUB_REPO, NARROW_MAX_PX, NICK_MAX, OFFLINE_FAILS, PAGE_SIZE, POLL_CHAT_MS,
  POLL_FILES_MS, POLL_LOGS_MS, POLL_STATUS_MS, REFRESH_SPIN_MS, RELEASES_PAGE,
  THEME_MIGRATE, TIP_COPY_URL, TIP_QR, TIP_SHARE, TOAST_MS, UPDATE_CHECK_DELAY_MS,
  UPDATE_CHECK_INTERVAL_MS, UPLOAD_DONE_KEEP_MS, WS_HB_TIMEOUT_MS, WS_WATCHDOG_MS,
} from "./config";
import { fetchLatestRelease, isNewer } from "./update";
import { createPetActivityTracker } from "./pet-activity";
import { open as openDialog, ask as askDialog } from "@tauri-apps/plugin-dialog";
import { readText as readClipboardText, readImage as readClipboardImage } from "@tauri-apps/plugin-clipboard-manager";
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
const isDesktop = "__TAURI_INTERNALS__" in window;
// 真实桌面 OS: isDesktop 在 Android/iOS 的 Tauri WebView 里同样为 true, 但 autostart/托盘
// 等桌面插件按 #[cfg(desktop)] 未注册 —— 依赖这些插件的 UI 必须用 isRealDesktop 门控
// iPadOS 13+ 伪装桌面 UA("Macintosh"), 单看 UA 会把 iPad 网页误判为真实桌面:
// 用 MacIntel + 多点触控补判(Apple 官方推荐手法)
const isIOSFamily = /iPhone|iPad|iPod/.test(navigator.userAgent)
  || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
const isRealDesktop = isDesktop && !isIOSFamily && !/Android/.test(navigator.userAgent);
const mobileShell = isDesktop && !isRealDesktop;
/* PWA: 仅浏览器端显示安装入口; 桌面/移动壳已有原生 App */
let deferredInstallPrompt = null;
window.addEventListener("beforeinstallprompt", (e) => {
  e.preventDefault();
  deferredInstallPrompt = e;
});
function installPwa() {
  if (isDesktop) return;
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
    deferredInstallPrompt = null;
    return;
  }
  if (isIOSFamily) showToast("Safari：点「分享」→「添加到主屏幕」");
  else showToast("浏览器菜单 →「安装应用 / 添加到主屏幕」");
}
const isQrPetWindow = isDesktop && new URLSearchParams(location.search).get("view") === "qr-pet";
const isIOS = isIOSFamily;   // Safari 不支持 webkitdirectory, 文件夹入口对 iOS/iPadOS 隐藏
const isWeChat = /MicroMessenger/i.test(navigator.userAgent);   // 微信内置浏览器: 下载被拦需引导到系统浏览器
// 桌面模式:窗口页面不在服务端口上,API 走回环地址;网页模式:相对路径即可;
// 移动壳默认是本机独立节点(连 127.0.0.1), 也可在 CC设置 改为"连接电脑"并持久化地址;
// 未就绪时 apiBase 为空 → 启动门遮罩
// 移动 App 默认作为独立节点连本机服务; 用户在设置页选过"连接电脑"时恢复该地址。
const serverAddr = ref((() => {
  try { return localStorage.getItem("wft-server-addr") || ""; } catch (e) { return ""; }
})());
function normalizeServerAddr(a) {
  let h = String(a || "").trim().replace(/^https?:\/\//i, "").replace(/\/+$/, "");
  if (h && !/:\d+$/.test(h)) h = `${h}:${DEFAULT_PORT}`;   // 只给了主机名/IP → 补默认端口
  return h;
}
const apiBase = computed(() => {
  if (!isDesktop) return "";
  if (mobileShell) return serverAddr.value ? `http://${serverAddr.value}` : "";
  return `http://127.0.0.1:${port.value || DEFAULT_PORT}`;
});
const addrInput = ref(serverAddr.value);
const addrErr = ref("");
/* 移动 App: 本机服务启动后自动连回环地址 */
const mobileBootFailed = ref(false);
const mobileConnectOpen = ref(false);   // 启动失败/无信号时改用“连接电脑”的地址输入
function isLocalAddr(addr) { return /^127\.0\.0\.1:\d+$/.test(String(addr || "")); }
async function initMobileLocal() {
  if (!mobileShell) return;
  mobileBootFailed.value = false;
  serverAddr.value = "";
  for (let i = 0; i < 30; i++) {
    try {
      const d = await tauriInvoke("control_status_cmd");
      if (d && d.running && d.port) {
        const addr = `127.0.0.1:${d.port}`;
        serverAddr.value = addr;
        // 移动端开启访问令牌后 loopback 也校验: 本机壳通过 Tauri 拿码并自动附加,
        // 同机其他 App 没有这条通道(拿不到码)。
        if (typeof d.pin === "string" && d.pin) accessPin.value = d.pin;
        try { localStorage.setItem("wft-server-addr", addr); } catch (e) {}
        saveTextPref("server-addr", addr);
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
  if (serverAddr.value && !isLocalAddr(serverAddr.value)) {
    // 连接电脑模式: 只重连当前电脑, 绝不把地址覆写成本机(否则永久丢掉原电脑地址)
    refreshStatus(); loadFiles(); loadChat(); connectWs();
    return;
  }
  resetSessionState();   // 切回本机: 清掉旧电脑的 PIN/上传/时间线/设备状态
  serverAddr.value = "";
  retryMobileBoot();
}
/* 从"连接电脑"切回本机独立节点 */
function useLocalNode() {
  if (!mobileShell) return;
  resetSessionState();
  serverAddr.value = "";
  addrInput.value = "";
  addrErr.value = "";
  try { localStorage.removeItem("wft-server-addr"); } catch (e) {}
  saveTextPref("server-addr", "");
  mobileConnectOpen.value = false;
  retryMobileBoot();   // 连接模式下手机本机服务可能已停, 这里顺带拉起
  showToast("已切回本机节点");
}
/* 重启本机服务(局域网开关/端口变更后需要重绑监听) */
async function restartMobileService() {
  if (!mobileShell) return;
  showToast("正在重启本机服务…");
  try { await tauriInvoke("control_stop_cmd"); } catch (e) {}
  try { await tauriInvoke("control_start_cmd"); } catch (e) {}
  await initMobileLocal();
}
/* 移动壳连接电脑: 探活(/api/info)通过即持久化地址并全量拉取; 401=服务在但开了访问码, 也算连上 */
async function saveServerAddr() {
  const host = normalizeServerAddr(addrInput.value);
  // 与服务端 browser_origin_guard 白名单严格对齐: 仅 IP / localhost / [ipv6]。
  // .local 已被服务端拒绝(同 Wi-Fi 攻击者可自架 mDNS 应答做 DNS 重绑定), 前端不得再放行
  const hostOk = /^(\d{1,3}(\.\d{1,3}){3}|localhost|\[[0-9a-fA-F:]+\])(:\d{1,5})?$/.test(host);
  if (!host) {
    addrErr.value = "格式如 192.168.1.8:1688";
    return;
  }
  if (!hostOk) {
    addrErr.value = "仅支持 IP / localhost（请填电脑局域网 IP）";
    return;
  }
  try {
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), 5000);
    const r = await fetch(`http://${host}` + API.info, { signal: ctl.signal });
    clearTimeout(timer);
    if (!r.ok && r.status !== 401) throw new Error(`HTTP ${r.status}`);
    const prevHost = serverAddr.value;
    serverAddr.value = host;
    // 换电脑/换地址: 旧机的上传卡、聊天、已读、文件基线、票据与配对码全部作废,
    // 否则新机已有文件会被判成"新增"刷屏, 旧 PIN 还会发给新主机
    if (prevHost !== host) resetSessionState();
    try { localStorage.setItem("wft-server-addr", host); } catch (e) {}
    saveTextPref("server-addr", host);
    addrInput.value = host;
    addrErr.value = "";
    mobileConnectOpen.value = false;
    needPin.value = false;
    if (mobileShell) {
      // 切到连接电脑模式后手机本机节点不再使用: 停服并收回局域网开放,
      // 避免之前开过的局域网共享在后台继续暴露(重启 App 也不会再自动开放)。
      try { await tauriInvoke("set_open_lan_cmd", { open: false }); } catch (e) {}
      try { await tauriInvoke("control_stop_cmd"); } catch (e) {}
    }
    statusFail = 0;
    offline.value = false;
    showToast("已连接电脑服务");
    startPolling();   // resetServerAddr 会停掉轮询, 重新连接后必须恢复
    refreshStatus();
    loadFiles();
    loadChat();
    connectWs();
  } catch (e) {
    addrErr.value = e && e.name === "AbortError"
      ? "连接超时：检查地址与 Wi-Fi"
      : /HTTP 400/.test(String(e && e.message))
        ? "服务端拒绝该 Host（只认 IP / localhost）"
        : "连不上：确认电脑服务已运行、地址正确、同一 Wi-Fi";
  }
}
/* 失联全屏遮罩下的唯一出路: 清空已保存地址 → 回到"连接电脑"引导门。
   否则换 IP / 换电脑后, 遮罩(z-index 1000)盖住设置页, 连接门又因地址非空不出现,
   用户只能清应用数据脱困 */
/* 会话级缓存全部重置: 切换电脑地址/断线重连后不得把旧机的数据带进新连接 */
let sessionEpoch = 0;   // 每次切换电脑/地址自增: 旧请求的迟到响应直接丢弃
function resetSessionState() {
  sessionEpoch++;
  loadingFiles = false;
  loadingChat = false;
  statusRefreshing = false;
  // 切换电脑地址前中止在途上传并释放 Blob/句柄, 否则旧连接的上传会继续跑、
  // 屏幕常亮/保活也不会释放
  for (const u of uploads.value) {
    try { u.xhr?.abort?.(); } catch (e) {}
    u.file = null; u.xhr = null;
  }
  closePreview();
  uploads.value = [];
  updateWakeLock().catch(() => {});   // 清空上传后释放屏幕常亮/静音保活
  remoteFiles.value = [];
  peers.value = [];
  selected.value = new Set();
  knownFileNames = null;
  chatMsgs.value = [];
  chatHtmlCache.clear();   // 切换电脑/服务后消息 id 可能从头计数, 不能复用旧 HTML 缓存
  chatCount.value = 0;
  seenChat.value = 0;
  seenChatId.value = 0;
  jumpCount.value = 0;
  loadFiles._sig = null;
  loadChat._sig = null;
  loadChat._lastSig = null;
  loadChat._init = false;
  clearedTs.value = 0;
  timelineStart.value = 0;
  clearShareTicketCache();
  accessPin.value = "";
  try { localStorage.removeItem(LS.pin); } catch (e) {}
  saveTextPref("access-pin", "");
}
function resetServerAddr() {
  resetSessionState();
  serverAddr.value = "";
  try { localStorage.removeItem("wft-server-addr"); } catch (e) {}
  saveTextPref("server-addr", "");
  addrInput.value = "";
  addrErr.value = "";
  offline.value = false;
  statusFail = 0;
  closeWs();
  stopPolling();
  showToast("请输入新的电脑地址");
}
function tauriInvoke(cmd, args) { return invoke(cmd, args || {}); }

/* ---------- 访问码(可选; 电脑端 CC设置 开启后, 局域网网页端需输入一次) ---------- */
const accessPin = ref((() => { try { return localStorage.getItem(LS.pin) || ""; } catch (e) { return ""; } })());
const needPin = ref(false);      // 401 → 弹出输入框; 验证通过后全量重载
const pinInput = ref("");
const pinEnabled = ref(false);   // 桌面设置页展示用(仅 Tauri 命令下发, HTTP /api/info 不携带码值)
const pinCode = ref("");
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
async function apiFetch(url, opts, timeoutMs = 12000) {
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
  try {
    const r = await fetch(url, o);
    if (r.status === 401 || r.status === 429) {
      // 只清除"这次请求所用的码"仍是当前码的情况: submitPin 成功后, 旧的在途请求
      // 迟到 401 不能把刚保存的新码清掉, 否则配对成功后立刻又弹配对框
      if (accessPin.value === usedPin) {
        dropAccessPin();
        needPin.value = true;
      }
      const err = new Error(r.status === 429 ? "配对尝试过于频繁" : "需要配对码");
      err.status = r.status;   // 供 refreshStatus 区分"服务在但要配对"与"真断网"
      throw err;
    }
    return r;
  } finally {
    clearTimeout(timer);
    if (outer) outer.removeEventListener("abort", onAbort);
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
    const r = await fetch(apiBase.value + API.info + "?pin=" + encodeURIComponent(p), { signal: ctl.signal });
    if (r.ok) {
      accessPin.value = p;
      try { localStorage.setItem(LS.pin, p); } catch (e) {}
      saveTextPref("access-pin", p);
      needPin.value = false;
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
async function toggleAccessPin() {
  try {
    const d = await tauriInvoke("set_access_pin_cmd", { enable: !pinEnabled.value });
    if (d.ok) {
      pinEnabled.value = !pinEnabled.value;
      pinCode.value = d.pin || "";
      clearShareTicketCache();   // 服务端已轮换/清空票据
      clearAccessPinIfOff(pinEnabled.value);
      showToast(pinEnabled.value ? `访问令牌已开启：${d.pin}` : "访问令牌已关闭");
    } else showToast(d.error || "设置失败", "error");
  } catch (e) { showToast("设置失败，请重试", "error");
  }
}
/* 局域网访问开关: 桌面默认开; 移动端默认关(仅回环), 公共 Wi-Fi 下不会裸奔 */
async function toggleOpenLan() {
  try {
    const d = await tauriInvoke("set_open_lan_cmd", { open: !openLanEnabled.value });
    if (d && d.ok) {
      openLanEnabled.value = !!d.openLan;
      showToast(d.openLan
        ? "已开启局域网访问（重启服务后生效，同 Wi-Fi 设备可访问本机）"
        : "已关闭局域网访问（重启服务后生效，仅本机可访问）");
    } else showToast(d?.error || "设置失败", "error");
  } catch (e) { showToast("设置失败，请重试", "error"); }
}
/* 当前设备昵称(供日志/请求头使用) */
function deviceNick() { return nickInput.value.trim() || autoNick; }

/* ---------- 视图切换 ---------- */
const view = ref(isQrPetWindow ? "pet" : "transfer");   // transfer | service | settings | pet
const VIEW_TITLES = { transfer: "消息发送", service: "服务中心", settings: "CC设置" };
function setView(v) {
  // 兼容旧入口：files/chat 都归到合并后的「传输」
  if (v === "files" || v === "chat") v = "transfer";
  view.value = v;
  moreOpen.value = false;
  plusOpen.value = false;
  if (v === "transfer") {
    markChatSeen();
    nextTick(scrollChats);
  } else if (isNarrow) {
    fileSheetOpen.value = false;
  }
}

/* ---------- 合并传输页(微信式)面板状态 ---------- */
const drawerCollapsed = ref(true);    // 桌面右侧共享文件抽屉: 默认收起, 由头部按钮/引导打开
const fileSheetOpen = ref(false);     // 窄屏共享文件全屏面板
const plusOpen = ref(false);          // 输入栏 ＋ 面板
const moreOpen = ref(false);          // 房间头更多菜单
const nickOpen = ref(false);          // 昵称设置弹框
const nickDraft = ref("");            // 弹框草稿: 取消时丢弃, 不污染全局昵称
const fileMenuName = ref("");         // 移动端文件行动作面板
const locatedName = ref("");          // “定位”高亮的文件
const fileDot = ref(false);           // 文件抽屉关闭时收到新文件: 文件夹按钮绿点
const expandedGroups = ref({});       // 文件组展开状态
const atBottom = ref(true);           // 时间线是否贴底
const jumpCount = ref(0);             // 上翻期间的新消息数
function filePanelVisible() {
  // 注意: 脚本里 isNarrow 是 ref, 必须 .value —— 漏写会恒为 truthy,
  // 宽屏下永远走窄屏分支, 表现为「抽屉收起后再也打不开」
  return isNarrow.value ? fileSheetOpen.value : !drawerCollapsed.value;
}
/* 手机上打开面板前先收键盘: 否则软键盘会把底部面板顶掉一半 */
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
  // 不自动收起: 一直提示到用户真正点开共享文件入口为止
  fileHintShow.value = true;
}
function toggleFilePanel() {
  markFileHintSeen();
  moreOpen.value = false;
  plusOpen.value = false;
  if (isNarrow.value) fileSheetOpen.value = !fileSheetOpen.value;
  else drawerCollapsed.value = !drawerCollapsed.value;
  if (filePanelVisible()) { fileDot.value = false; blurMobileKeyboard(); }
}
function openFilePanel() {
  markFileHintSeen();
  moreOpen.value = false;
  plusOpen.value = false;
  if (isNarrow.value) fileSheetOpen.value = true;
  else drawerCollapsed.value = false;
  fileDot.value = false;
  blurMobileKeyboard();
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
/* 5 套低饱和配色(蓝/灰/绿/紫/琥珀): 去掉微信绿与相近的石墨玫瑰,
   长时间阅读不刺眼; 被移除的选择在 main.js 启动时迁移(见 THEME_MIGRATE) */
const THEMES = [
    { id: "default",  name: "深空蓝",   preview: "linear-gradient(135deg,#0d1017,#1a2233)" },
    { id: "rose",     name: "石墨玫瑰", preview: "linear-gradient(135deg,#16151a,#4a2531)" },
    { id: "mint",     name: "森林薄荷", preview: "linear-gradient(135deg,#0f1712,#1d4634)" },
    { id: "purple",   name: "暗夜紫",   preview: "linear-gradient(135deg,#15121d,#33265a)" },
    { id: "amber",    name: "墨色琥珀", preview: "linear-gradient(135deg,#141109,#4a3512)" },
];
const theme = ref(isRealDesktop ? "mint" : "default");   // 桌面端默认森林薄荷
function applyTheme(t) {
    if (t && t !== "default") document.documentElement.dataset.theme = t;
    else delete document.documentElement.dataset.theme;
}
function setTheme(id) {
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
    // 桌面端首次启动默认森林薄荷; 浏览器/移动端保持深空蓝
    t = isRealDesktop ? "mint" : "default";
    try { localStorage.setItem(LS.theme, t); } catch (e) {}
  }
  if (THEME_MIGRATE[t]) {
    t = THEME_MIGRATE[t];
    try { localStorage.setItem(LS.theme, t); } catch (e) {}
  }
  theme.value = t;
  applyTheme(t);   // 模块自身先应用一次, 不依赖 main.js 的读取顺序
} catch (e) {}
/* 配色行的"当前"标注: 色板芯片不显示名字(走 tooltip), 选中名在此回显 */
const currentThemeName = computed(() => THEMES.find(t => t.id === theme.value)?.name || "");
/* 主题模式(亮/暗/跟随系统)与配色正交: 配色(data-theme)定色相, 模式(data-mode)定明暗 */
const THEME_MODES = [
    { id: "light",  name: "亮色", icon: "☀️" },
    { id: "dark",   name: "暗色", icon: "🌙" },
    { id: "system", name: "跟随系统", icon: "🖥️" },
];
const systemDarkMq = window.matchMedia("(prefers-color-scheme: dark)");
const themeMode = ref((() => {
    try {
      if (!isRealDesktop) {
        const sv = localStorage.getItem("wft-ui-mode");
        if (sv) return sv;
      }
      return localStorage.getItem(LS.mode) || "light";
    } catch (e) { return "light"; }
})());
function applyThemeMode() {
    // 显式写 data-mode("dark" 无对应 CSS 规则 = 默认深色), system 按当前系统外观解析
    document.documentElement.dataset.mode =
        themeMode.value === "system" ? (systemDarkMq.matches ? "dark" : "light") : themeMode.value;
}
function setThemeMode(m) {
    themeMode.value = m;
    applyThemeMode();
    try { localStorage.setItem(LS.mode, m); } catch (e) {}
    saveTextPref("ui-mode", m);   // 同步给浏览器/移动壳
}
/* 浏览器/移动壳跟随桌面端配色: 服务端 /api/info 下发 pref */
function applyServerUiTheme(d) {
    if (isRealDesktop || !d) return;
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
const chatOpen = ref(false);
// 手机聊天面板打开时锁定背后页面, 防止滑动穿透带动共享文件页滚动
watch(chatOpen, v => { document.body.style.overflow = v ? "hidden" : ""; });
function onMqChange(e) {
  isNarrow.value = e.matches;
  if (!e.matches) {
    closeChat();                 // 回到宽屏时自动收起旧手机聊天面板(兼容残留)
    fileSheetOpen.value = false;
    plusOpen.value = false;
    moreOpen.value = false;
  }
}
if (typeof mq.addEventListener === "function") mq.addEventListener("change", onMqChange);
else if (typeof mq.addListener === "function") mq.addListener(onMqChange);
function openChat() {
  chatOpen.value = true;
  seenChat.value = chatCount.value;
  nextTick(scrollChats);
  // sheetUp 入场动画(0.3s)期间, 部分移动内核(WebKit 系尤其)会丢弃 display:none→block
  // 容器刚设置的滚动位置 → 打开面板停在顶部。动画结束后再钉一次, 确保必定位到最新一条
  setTimeout(scrollChats, 350);
}
function closeChat() { chatOpen.value = false; }
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
// 局域网访问开关(服务端 /api/info 下发; 移动端默认 false)。定义提前到二维码 watch 之前。
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
    const yes = await askDialog(
      `服务正在运行。修改共享目录将自动执行：\n停止服务 → 切换目录 → 重启服务（手机端需刷新页面）。继续吗？`,
      { title: "切换共享目录", kind: "warning" }
    );
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
  const yes = await askDialog(
    `「${path}」是高风险目录（家目录、其上级或磁盘根目录）。\n\n共享后同一 Wi-Fi 内的任何设备都能读取、覆盖、删除其中的全部文件。确定继续吗？`,
    { title: "高风险共享目录", kind: "warning" }
  );
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
  if (!isDesktop) return;
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
  petPollTimer = setInterval(petPollPresence, 250);
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
  } else {
    copyFallbackText.value = text;
    copyFallbackShow.value = true;
  }
}
function selectCopyFallback(e) {
  const ta = e.target.closest(".dialog-box")?.querySelector("textarea");
  if (ta) { ta.focus(); ta.select(); }
}
function copyText(text, okMsg) {
  copyDiag("enter", {});
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text)
      .then(() => { copyDiag("clip-ok"); showToast(okMsg); })
      .catch((e) => { copyDiag("clip-reject", { err: String((e && e.message) || e) }); fallbackCopy(text, okMsg); });
  } else {
    copyDiag("no-clip-api");
    fallbackCopy(text, okMsg);
  }
}
const shareUrl = computed(() => `http://${lanIp.value}:${port.value}`);
/* 地址栏宽度贴合内容: 宽屏不留大片空白; 窄屏(换行后)由 CSS 改为撑满整行 */
// 地址(ip:端口)或运行状态一变就重生成二维码(覆盖 IP 变化/端口顺延/手动改端口/启停全场景);
// 停服清空避免残留旧码; await 结果落地前校验地址未再变, 丢弃乱序的过期结果
async function syncQr() {
  // 安全模式(仅 127.0.0.1)下二维码指向局域网 IP 但没人能打开, 不生成误导性二维码
  if (!running.value || !lanActive.value) { qrData.value = ""; return; }
  const url = shareUrl.value;
  try {
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
    showToast("当前为安全模式（仅本机可访问），请先在 CC设置 开启局域网访问并重启服务", "error");
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
  } catch (e) { console.error(e); }
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
    showToast("当前为安全模式（仅本机可访问），请先在 CC设置 开启局域网访问并重启服务", "error");
    return;
  }   // 停服后链接打不开, 先拦截
  // 浏览器端优先 shareUrl(与二维码同源), 但首次状态刷新前 lanIp 还是占位 "--" ——
  // 此时必须回落 location.origin, 否则复制出 http://--:0 无效地址
  const base = isDesktop
    ? `http://${lanIp.value}:${port.value}`
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
function clearShareTicketCache() { shareTicketEpoch++; shareTicketCache.clear(); }
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
  for (const f of pageFiles.value) prefetchShareTicket(f.name);
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
  if (!(accessPin.value || pinCode.value || pinEnabled.value)) return base;
  const t = await createShareTicket(name);
  return t ? `${base}&ticket=${encodeURIComponent(t)}` : null;
}
/* ---------- 应用内预览 ----------
   旧实现 window.open 新标签: 浏览器原生查看器没有"关闭回到聊天"的入口, 手机
   Safari 还会离开当前会话。改为应用内全屏层, 右上角「关闭」直接回到聊天页。
   预览地址用长期 PIN 直连而非一次性 ticket: 视频/音频会发多次 Range 请求,
   首个 206 就把票据烧掉, 后续分段全部 401。应用内不导航, PIN 不会进浏览器历史。 */
const previewName = ref("");
const previewKind = ref("");
const previewUrl = ref("");
const previewText = ref("");
const previewErr = ref("");
const previewLoading = ref(false);
let previewSeq = 0;              // 预览代际: 迟到的旧文件文本响应不得覆盖新预览
let previewAbort = null;
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
}
async function previewFileWeb(name) {
  if (serviceDown()) { showToast("服务已停止，无法预览", "error"); return; }
  const kind = previewKindOf(name);
  // 移动壳(Android/iOS WebView)没有 PDF 渲染器: 交系统浏览器; 桌面壳的
  // WKWebView/WebView2 有内建 PDF 查看器, 统一走应用内 iframe —— 不再把长期
  // 访问令牌拼进系统浏览器历史与外部 App。移动壳是同一设备回环访问, 风险有限。
  if (mobileShell && kind === "pdf") {
    const url = withPin(`${apiBase.value}${API.download(name)}?inline=1`);
    tauriInvoke("open_url_cmd", { url })
      .then((d) => { if (!d || !d.ok) copyText(url, "已复制预览链接，请在浏览器中打开"); })
      .catch(() => copyText(url, "已复制预览链接，请在浏览器中打开"));
    return;
  }
  // 文本整段读进内存展示: 超大日志先拦住, 避免手机上把内存吃满
  if (kind === "text") {
    const meta = allFiles.value.find(f => f.name === name);
    if (meta && meta.size > 2 * 1024 * 1024) {
      showToast("文本超过 2MB，请下载后查看", "error");
      return;
    }
  }
  document.activeElement?.blur?.();   // 先收软键盘, 否则 100dvh 层会被顶掉一截
  const seq = ++previewSeq;
  previewAbort?.abort();
  previewAbort = null;
  previewName.value = name;
  previewKind.value = kind;
  previewUrl.value = withPin(`${apiBase.value}${API.download(name)}?inline=1`);
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
function pickFiles() { document.getElementById("fileInput").click(); }
// 移除上传条目(失败行不自动清理, 否则只能刷新页面才能清掉)
function removeUpload(id) { uploads.value = uploads.value.filter(u => u.id !== id); }
function pickDir() {
  // iOS Safari/WebView 不支持 webkitdirectory: 按钮保留可见(与其它端一致), 点击给出明确说明
  if (isIOS) { showToast("iOS 浏览器不支持选择文件夹，请用「文件」多选", "error"); return; }
  document.getElementById("dirInput").click();
}
function onPick(e) { handleFiles(e.target.files); e.target.value = ""; }
function onPickDir(e) {
  // 选空文件夹与拖入空文件夹一致给反馈(原静默 return 像软件坏了)
  if (!e.target.files || !e.target.files.length) { showToast("文件夹为空或不含可上传文件", "error"); e.target.value = ""; return; }
  handleFiles(e.target.files); e.target.value = "";
}
function handleFiles(list) {
  if (!list || !list.length) return;
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
  if (files.length) handleFiles(files);
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
  // 深度上限=8(对齐服务端 UPLOAD_MAX_DEPTH): 超深树收集了也会被服务端逐个 400;
  // 同时是符号链接环的兜底 —— 环会让递归永不返回, collecting 卡死后所有拖入被静默丢弃
  const walk = async (entry, prefix, depth) => {
    // 深度上限与服务端 UPLOAD_MAX_DEPTH=8 对齐: 文件路径段数 = depth+1,
    // depth>7 的文件(≥9段)服务端 safe_rel_path 必拒 —— 提前丢弃, 不进队列也不误报
    if (depth > 7) return;
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
    // 空文件夹拖入零反馈会让用户反复拖、以为软件坏了
    if (!list || !list.length) { showToast("文件夹为空或不含可上传文件", "error"); return; }
    handleFiles(list);
  } finally { collecting.value = false; }
}
/* 上传名: 文件夹拖入/选择时为相对路径(如 目录/子目录/文件名), 普通文件为原名 */
function uploadRelName(file) { return file.webkitRelativePath || file.name; }

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
  if (isRealDesktop) openFile(n);
  else downloadFileWeb(n);
}
function openRemote(r) {
  if (!r || !r.name) return;
  if (isRealDesktop) openFile(r.name);
  else downloadFileWeb(r.name);
}
/* 缩略图点击: 桌面交给系统程序打开; 浏览器端能内联预览就预览, 不能则下载。
   400ms 内同名连点只响应一次 —— 双击缩略图会连发 click, 否则会开两次预览/下载 */
let thumbLast = { name: "", t: 0 };
function openThumb(name) {
  if (!name) return;
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
  for (const r of remoteFiles.value) {
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
const nickInput = ref("");
const seenChat = ref(0);       // 兼容旧代码的已读条数(实际未读用 id 计算)
const seenChatId = ref(0);     // 最后一条已读消息 id: 满 200 条后条数恒定, 不能用条数算未读
const chatCount = ref(0);
const chatBox = ref(null);

/* ---------- 附近设备(最小实现: 复用 HTTP, 不引入 UDP) ---------- */
const PEER_FAVS_KEY = "wft-peer-favs";
const peers = ref([]);
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
  const s = String(ip || "");
  if (/^\d{1,3}(\.\d{1,3}){3}$/.test(s)) {
    const p = s.split(".").map(Number);
    return p[0] === 10 || p[0] === 127
      || (p[0] === 172 && p[1] >= 16 && p[1] <= 31)
      || (p[0] === 192 && p[1] === 168)
      || (p[0] === 100 && p[1] >= 64 && p[1] <= 127)
      || (p[0] === 169 && p[1] === 254);
  }
  const l = s.toLowerCase();
  return l === "::1" || l.startsWith("fe80") || l.startsWith("fc") || l.startsWith("fd");
}
function peerBase(p) {
  const host = String(p.ip || "").replace(/^\[|\]$/g, "");
  const h = host.includes(":") ? `[${host}]` : host;
  return `http://${h}:${p.port || port.value}`;
}
const peerTarget = ref(null);
const peerText = ref("");
const peerFileInput = ref(null);
const peerSending = ref(false);
// 浏览器/手机浏览器跨设备投递时, 对方可能开了访问令牌: 按 endpoint 记住对方配对码,
// 请求带 X-Access-Pin; 401 时提示用户填写。
const PEER_PINS_KEY = "wft-peer-pins";
const peerPins = ref((() => {
  try { return JSON.parse(localStorage.getItem(PEER_PINS_KEY) || "{}") || {}; } catch (e) { return {}; }
})());
const peerPin = ref("");
function peerKey(p) { return `${p.ip}:${p.port}`; }
function savePeerPin(p, pin) {
  if (!p) return;
  const k = peerKey(p);
  const next = { ...peerPins.value };
  if (pin) next[k] = pin; else delete next[k];
  peerPins.value = next;
  try { localStorage.setItem(PEER_PINS_KEY, JSON.stringify(next)); } catch (e) {}
}
function openPeerSend(p) {
  if (!p || !p.ip) return;
  if (!isLanHost(p.ip) || !(Number(p.port) > 0 && Number(p.port) <= 65535)) {
    showToast("设备地址不在局域网内，已拒绝", "error");
    return;
  }
  peerTarget.value = p;
  peerText.value = "";
  peerPin.value = peerPins.value[peerKey(p)] || "";
}
function copyPeerAddr() {
  if (!peerTarget.value) return;
  copyText(peerBase(peerTarget.value), "已复制设备地址");
}
async function recordPeerSend(text) {
  try {
    await apiFetch(apiBase.value + API.chat + "?nick=" + encodeURIComponent(curNick.value), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ nickname: curNick.value, message: text })
    });
    loadChat();
  } catch (e) {}
}
async function sendTextToPeer() {
  const p = peerTarget.value;
  const text = peerText.value.trim();
  if (!p || peerSending.value) return;
  if (!text) { showToast("请输入要发送的消息", "error"); return; }
  peerSending.value = true;
  try {
    // 半开 Wi-Fi 下裸 fetch 会永久挂起; 15s 超时并如实提示
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), 15000);
    const pin = peerPin.value.trim();
    if (pin) savePeerPin(p, pin);
    const headers = { "Content-Type": "application/json" };
    if (pin) headers["X-Access-Pin"] = pin;
    let r;
    try {
      r = await fetch(peerBase(p) + "/api/chat?nick=" + encodeURIComponent(curNick.value), {
        method: "POST",
        headers,
        body: JSON.stringify({ nickname: curNick.value, message: text }),
        signal: ctl.signal
      });
    } finally { clearTimeout(timer); }
    if (r.status === 401) throw new Error("peer-pin");
    if (r.status === 403) throw new Error("peer-forbidden");
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    // 本机回执只记"发了一条消息", 不把正文再广播进本机公共聊天室
    await recordPeerSend(`📤 已向 ${p.nickname || "设备"} 发送了一条消息`);
    showToast(`已发送给 ${p.nickname || "设备"}`);
    peerTarget.value = null;
  } catch (e) {
    showToast(
      e?.name === "AbortError" ? "发送超时：对方可能已离线"
        : e?.message === "peer-pin" ? "对方开启了访问令牌，请在上方填写对方配对码后重试"
        : e?.message === "peer-forbidden" ? "浏览器端跨设备发送需对方开启访问令牌；可使用桌面/手机 App"
        : "发送失败：对方可能已离线或开启访问令牌",
      "error"
    );
  } finally {
    peerSending.value = false;
  }
}
function pickPeerFiles() { if (!peerSending.value) peerFileInput.value && peerFileInput.value.click(); }
async function sendFilesToPeer(e) {
  const files = Array.from((e.target && e.target.files) || []);
  if (e.target) e.target.value = "";
  const p = peerTarget.value;
  if (!p || !files.length || peerSending.value) return;
  peerSending.value = true;
  const pin = peerPin.value.trim();
  if (pin) savePeerPin(p, pin);
  let ok = 0, forbidden = 0, unauthorized = 0;
  try {
    for (const f of files) {
      if (peerTarget.value !== p) break;   // 关闭弹窗 = 取消剩余文件
      try {
        await new Promise((resolve, reject) => {
          const xhr = new XMLHttpRequest();
          xhr.open("POST", peerBase(p) + API.upload, true);
          // HTTP 头只接受 ISO-8859-1: 中文文件名/昵称必须 URL 编码(与主上传链一致)
          xhr.setRequestHeader("X-Filename", encodeURIComponent(f.name));
          xhr.setRequestHeader("X-File-Size", String(f.size));
          xhr.setRequestHeader("X-Nickname", encodeURIComponent(curNick.value));
          if (pin) xhr.setRequestHeader("X-Access-Pin", pin);
          xhr.timeout = 10 * 60 * 1000;
          xhr.onload = () => (xhr.status === 200 ? resolve() : reject(new Error(String(xhr.status))));
          xhr.onerror = () => reject(new Error("network"));
          xhr.ontimeout = () => reject(new Error("timeout"));
          xhr.send(f);
        });
        ok++;
        await recordPeerSend(`📎 已发送文件 ${f.name} 到 ${p.nickname || "设备"}`);
      } catch (err) {
        if (String(err && err.message) === "403") forbidden++;
        else if (String(err && err.message) === "401") unauthorized++;
      }
    }
  } finally {
    peerSending.value = false;
  }
  showToast(
    ok ? `已发送 ${ok} 个文件给 ${p.nickname || "设备"}`
      : unauthorized ? "对方开启了访问令牌，请填写对方配对码后重试"
      : forbidden ? "浏览器端跨设备发送需对方开启访问令牌；可使用桌面/手机 App"
      : "发送失败：对方可能已离线或开启访问令牌",
    ok ? "success" : "error"
  );
  if (ok) peerTarget.value = null;
}
function currentPlatform() {
  if (isIOSFamily) return "iOS";
  if (/Android/.test(navigator.userAgent)) return "Android";
  if (isRealDesktop) return "电脑";
  return "浏览器";
}
async function refreshPeers() {
  // 浏览器端 apiBase 为空表示相对路径; 移动壳服务未就绪时才跳过
  if (serviceDown() || (isDesktop && !apiBase.value)) { peers.value = []; return; }
  try {
    const r = await apiFetch(apiBase.value + "/api/devices");
    if (!r.ok) return;
    const d = await r.json();
    peers.value = Array.isArray(d.devices) ? d.devices : [];
  } catch (e) {}
}
async function sendHello() {
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
  seenChat.value = chatCount.value;
}
const curNick = computed(() => nickInput.value.trim() || autoNick);
/* 合并后只有「传输」一个页面: 页面在前台且窗口有焦点时, 消息可见即已读。
   失焦/切后台保留未读; 浏览器标签标题同步未读数(桌面壳由侧栏角标承担)。 */
function markChatSeen() {
  if (view.value !== "transfer") return;
  if (document.visibilityState !== "visible" || !document.hasFocus()) return;
  setSeenToLatest();
}
watch([unread, view], () => {
  if (isQrPetWindow) return;
  const base = "CC-Flash · " + (VIEW_TITLES[view.value] || "消息发送");
  document.title = unread.value > 0 ? `(${unread.value}) ${base}` : base;
}, { immediate: true });
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
  if (chatMsgs.value.length) clearedTs.value = chatMsgs.value[chatMsgs.value.length - 1].timestamp;
  // 必须立即清显示列表: WS 在线时 loadChat 只由"新消息事件"触发, 安静的聊天室里
  // 清空后 loadChat 永远不来, 只设标记会表现为"点了清空没反应"(轮询时代的回归)
  chatMsgs.value = [];
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
  try {
    // 超时保护: 无 AbortController 时单个挂死请求会让 loadingChat 恒 true,
    // 互斥把影响面从"单次"放大为"聊天停更直至浏览器默认超时(~5min)"
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), 10000);
    const r = await apiFetch(apiBase.value + API.chat + "?nick=" + encodeURIComponent(deviceNick()), { signal: ctl.signal });
    if (!r.ok) { clearTimeout(timer); throw new Error(`消息加载失败 (${r.status})`); }
    // 超时必须覆盖响应体读取: 半开 Wi-Fi 下 r.json() 会挂到浏览器底层超时,
    // 期间 loadingChat 恒 true, WS 触发的补拉全部只排队不执行(聊天停更)。
    const d = await r.json();
    clearTimeout(timer);
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
    chatCount.value = msgs.length;
    const firstLoad = !loadChat._init;
    if (isQrPetWindow && petActivity.observeMessages(msgs)) markPetNotify();
    // 首次拉取把已有历史视为已读: 后台自启/刚打开页面时不应顶着 99+ 未读
    if (firstLoad) {
      setSeenToLatest();
    // 断网恢复: 失联期间离线清空了本地计数, 恢复首拉若按全量算未读会瞬间冲到 99+ ——
    // 把恢复瞬间的历史视为已读, 之后的新消息照常计数
    } else if (chatRecoveredOffline) {
      chatRecoveredOffline = false;
      setSeenToLatest();
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
  } catch (e) { console.error(e); }
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
  copyText(String(m.message || ""), "消息已复制");
}
async function deleteMessage(m) {
  if (!m || offlineGuard()) return;
  let yes = false;   // 默认不删: 确认框异常时绝不能静默执行跨端删除
  try {
    if (isRealDesktop) {
      yes = await askDialog("删除这条消息？所有设备都将看不到它。", { title: "删除消息", kind: "warning" });
    } else {
      yes = window.confirm("删除这条消息？所有设备都将看不到它。");
    }
  } catch (_) {
    showToast("无法弹出确认框，请重试", "error");
    return;
  }
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
function _fillChatText(text) {
  chatInput.value = String(text).slice(0, cfg.value.msgMax);
  showToast("已粘贴剪贴板文本");
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
  handleFiles([new File([blob], `粘贴图片-${Date.now()}.png`, { type: "image/png" })]);
  showToast("已粘贴剪贴板图片");
  return true;
}
/* 点「粘贴」: 先看剪贴板有没有文本, 再看有没有图片/文件; 都没有才提示。
   桌面/移动壳走原生剪贴板插件 —— 自定义协议下 WebView 的 Clipboard API 常被拒绝;
   浏览器仅在安全上下文(https/localhost)才有 Clipboard API, http 局域网地址读不到。 */
async function pasteFromClipboard() {
  plusOpen.value = false;
  if (isDesktop) {
    try {
      const text = await readClipboardText();
      if (text && text.trim()) { _fillChatText(text); return; }
    } catch (_) {}
    try { if (await _pasteNativeImage()) return; } catch (_) {}
    showToast("剪贴板里没有可粘贴的文本或图片（复制文件请直接 Cmd/Ctrl+V）", "error");
    return;
  }
  const focusInput = () => { const el = document.querySelector(".tx-input"); if (el) el.focus(); };
  if (!navigator.clipboard) {
    // http 局域网地址不是安全上下文: 浏览器不提供 Clipboard API, 只能引导用户长按粘贴
    focusInput();
    showToast("浏览器限制无法读取剪贴板，请长按输入框选择「粘贴」", "error");
    return;
  }
  let text = "";
  let denied = false;
  try { text = await navigator.clipboard.readText(); } catch (_) { denied = true; }
  if (text && text.trim()) { _fillChatText(text); return; }
  if (denied) {
    focusInput();
    showToast("无法读取剪贴板，请长按输入框选择「粘贴」", "error");
    return;
  }
  try {
    const items = await navigator.clipboard.read();
    const files = [];
    for (const item of items) {
      for (const type of item.types) {
        if (!type.startsWith("image/")) continue;
        const blob = await item.getType(type);
        const ext = type === "image/jpeg" ? "jpg" : (type.split("/")[1] || "png");
        files.push(new File([blob], `粘贴图片-${Date.now()}.${ext}`, { type }));
      }
    }
    if (files.length) { handleFiles(files); showToast(`已粘贴 ${files.length} 个文件`); return; }
  } catch (_) {}
  showToast("剪贴板里没有可粘贴的文件或文本", "error");
}
async function sendChat() {
  if (sending.value) return;   // 连按回车/连点发送只产生一条消息
  const text = chatInput.value.trim();
  if (!text) return;
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
  // 带操作(如"撤销")的提示停留更久, 给用户反应时间
  setTimeout(() => { toasts.value = toasts.value.filter(t => t.id !== id); }, action ? 4000 : TOAST_MS);
}
function runToastAction(t) {
  try { t.action && t.action.fn(); } catch (e) {}
  toasts.value = toasts.value.filter(x => x.id !== t.id);
}

/* ---------- 软件介绍页(桌面端全屏 overlay, 设计稿见 docs/软件介绍页设计稿.html) ---------- */
const introShow = ref(false);
const introSec = ref("feat");   // scroll spy 当前所在区块
const INTRO_FAQS = [
  { q: "手机需要安装 App 吗？", a: "不需要。手机、平板在同一 Wi-Fi 下用浏览器扫码或输入地址，即可使用全部功能（上传、下载、聊天、删除）。Android 用户也可以安装 APK：App 默认作为独立节点运行本机文件服务，并在「CC设置」里提供「连接电脑」输入框，可直接浏览电脑上的共享文件；两种模式随时切换。" },
  { q: "屏幕边缘的呼吸小球是什么？", a: "桌面端常驻的悬浮二维码：贴边只露一半，鼠标停留 1.5 秒展开「浏览器扫一扫」预览卡，移开自动收起；外圈呼吸=服务运行中、静止=已停止，环上绿点=有新文件或新消息。按住可拖到任意边缘，支持扩展屏——拖到哪块屏就吸哪块屏，位置连同屏幕一起记住、重启回原处，关闭入口在「CC设置 → 悬浮二维码」。" },
  { q: "文件列表去哪了？", a: "收进了「共享文件」抽屉 / 面板：桌面点房间头文件夹图标（或右侧抽屉），手机点顶部文件夹或＋面板里的「共享文件」。排序、分页、全选、批量删除都在里面；上传的文件同时以卡片出现在对话里。" },
  { q: "能直接预览文件吗？", a: "图片、PDF、纯文本（txt/md/log/csv）和音视频可以在应用内直接预览：点缩略图或「预览」打开全屏层，右上角「关闭」或按 Esc 即回到聊天。Office 文档、压缩包等浏览器无法渲染的类型仍需下载后查看；为安全起见 html/svg/xml 不会以网页形式打开。" },
  { q: "聊天支持 Markdown 吗？", a: "支持常用语法：标题、无序/有序列表、引用、分割线、围栏代码块，以及行内代码、粗体、斜体、删除线、[文字](链接) 和裸链接。输入框是多行的——桌面端 Enter 发送、Shift+Enter 换行；手机端 Enter 换行，点「发送」提交。" },
  { q: "文件会上传到外网或云服务器吗？", a: "不会。HTTP 服务只监听你的电脑，所有传输都发生在局域网内部，不经过任何第三方服务器。拔掉外网网线，功能一切照常。" },
  { q: "端口被占用了怎么办？", a: "默认端口 1688，被占用时自动向后顺延探测（最多 20 个），当前生效地址会实时显示在顶栏与二维码中；也可以在「服务中心」手动指定端口。" },
  { q: "关闭窗口后服务会停止吗？", a: "不会。关窗只是把窗口收进系统托盘，服务继续保持运行，手机端不受影响；开机自启默认关闭，可在「CC设置」中手动开启。真正退出请点托盘菜单中的「退出」。" },
  { q: "支持哪些平台？", a: "桌面端提供 macOS（11.0+）与 Windows 安装包（Linux 可从源码构建）；手机端浏览器免安装即可使用，Android 另有可直接安装的 APK（默认独立节点，可切换连接电脑）。iOS 暂无安装包，请直接用 Safari 打开电脑端地址使用网页版。所有设备需处于同一局域网（同一 Wi-Fi）。" },
  { q: "电脑换了 IP 或换了另一台电脑怎么办？", a: "手机浏览器直接打开电脑端显示的新地址即可。Android App：若当前处于「连接电脑」模式且地址失效，在「无信号」页面点「重新启动本机服务」会回到本机节点，或到「CC设置 → 连接电脑」重新填写地址；也可点「切回本机节点」。地址只支持 IP 或 localhost（为防 DNS 重绑定不接受 xxx.local 域名）。" },
  { q: "「附近设备」是什么？", a: "同一 Wi-Fi 下运行的 CC-Flash 节点（电脑端、移动 App）会通过 UDP 多播/广播互相发现，顶部出现的设备可直接投递消息或文件。注意：投递内容会进入对方的公共聊天与共享目录，同网段设备可见，请勿发送敏感内容。纯浏览器页面没有自己的监听端口，因此不会出现在可发送列表里。" },
  { q: "手机 App 会把我的文件暴露到 Wi-Fi 吗？", a: "默认不会：移动端首次启动是「安全模式」，只监听 127.0.0.1。需要手机被其他设备访问时，到「CC设置 → 本机服务」手动开启「局域网访问」，并建议同时开启访问令牌；开关修改后点「重启服务」生效。Android 会用前台服务通知保持传输不被系统冻结。" },
  { q: "访问令牌和分享链接安全吗？", a: "访问令牌是 6 位配对码，通过请求头校验，本机免输；连续输错会按来源 IP 指数退避，公网来源还受全局失败预算约束，锁定期间正确码也会被拒。开启令牌后分享按钮生成的链接只授权下载那一个文件、10 分钟有效、完整下载一次即失效；文件被删除或同名重传后旧链接立即失效；关闭令牌或轮换配对码时所有未使用链接同步作废；对无效链接的猜测会按来源 IP 限流。" },
  { q: "浏览器里能「安装」成应用吗？", a: "通过 https 或 localhost 访问时可以安装为 PWA 并离线缓存壳；通过 http://局域网IP 访问时浏览器不提供安装能力，只能「添加到主屏幕」创建快捷方式（不缓存离线）。桌面端与 Android 建议直接使用原生安装包。" },
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
    // 服务端重启会清空内存票据: 重连即作废本地缓存, 避免分享出死链
    clearShareTicketCache();
    loadChat();     // 连接即全量同步一次, 补上断线窗口期
    loadFiles();
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
    } else if (ev.type === "devices:changed") {
      refreshPeers();
    } else if (ev.type === "sync:needed") {
      // 服务端广播积压导致事件被丢弃: WS 在线时轮询是停的, 必须立即补一次全量
      loadChat(); loadFiles(); refreshPeers();
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
watch([running, port], () => {
  // 悬浮码窗已关闭(隐藏)时完全不建立 WS: 隐藏的 96px WebView 不该持续拉聊天/文件
  if (isQrPetWindow && !qrFloatEnabled.value) { closeWs(); return; }
  if (running.value && port.value) {
    connectWs();
    // 桌面壳的 apiBase 依赖实际端口: 启动瞬间 port 还是默认值/0, 首拉可能失败;
    // 状态接口拿到真实端口后补拉一次, 不必等 5s 轮询或 WS 建连
    if (isDesktop && view.value === "transfer") { loadFiles(false); loadChat(); }
  } else { closeWs(); }
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
    else { petStopTimers(); stopPolling(); closeWs(); }
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
    const top = Math.round((vv && vv.offsetTop) || 0);
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
    // 一样可能是"键盘覆盖视口而不 resize", 跳过会导致输入栏被键盘遮住
    if (isRealDesktop) return;
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
      try { ws.close(); } catch (_) {}
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
    if (previewName.value) { closePreview(); return; }
    if (peerTarget.value) { peerTarget.value = null; return; }
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
  // 附近设备心跳: 所有形态都上报/刷新(浏览器走相对路径, 手机走本机回环)
  if (!isQrPetWindow) {
    setTimeout(() => { sendHello(); refreshPeers(); }, 900);
    const peerPoll = setInterval(() => { sendHello(); refreshPeers(); }, 12000);
    onMountedCleanup(() => clearInterval(peerPoll));
  }
  if (isDesktop) {
    // 移动壳: localStorage 在 WKWebView 可能不落盘 → 启动回读 Rust pref 里的地址/配对码,
    // 避免"每次重启都要重新连接电脑"。localStorage 有值时优先它(更近的会话状态)
    if (mobileShell) {
      // 默认本机节点; 用户上次选择"连接电脑"时恢复该地址。
      // WKWebView 的 localStorage 可能只驻内存不落盘, 因此统一回读一次 Rust pref。
      // 配对码在两种模式下都要恢复, 否则连接电脑模式重启后又要重新输入。
      (async () => {
        try {
          if (!accessPin.value) {
            const rp = await tauriInvoke("load_text_pref_cmd", { key: "access-pin" });
            if (rp?.ok && typeof rp.value === "string" && rp.value) accessPin.value = rp.value;
          }
          if (serverAddr.value && !isLocalAddr(serverAddr.value)) {
            addrInput.value = serverAddr.value;
            refreshStatus(); loadFiles(); loadChat(); startPolling(); connectWs();
            return;
          }
          const r = await tauriInvoke("load_text_pref_cmd", { key: "server-addr" });
          if (r?.ok && typeof r.value === "string" && r.value && !isLocalAddr(r.value)) {
            serverAddr.value = normalizeServerAddr(r.value);
            addrInput.value = serverAddr.value;
            try { localStorage.setItem("wft-server-addr", serverAddr.value); } catch (e) {}
            refreshStatus(); loadFiles(); loadChat(); startPolling(); connectWs();
            return;
          }
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
      // [临时探针] JS 心跳: 每 2.5s 一次共 8 次 —— 区分"页面 JS 线程卡死"与"事件没投递"
      let hbN = 0;
      const hbTimer = setInterval(() => {
        hbN++;
        petDiag({ fn: "hb", n: hbN, shown: qrPetShown.value, geo: qrPetExpanded.value });
        if (hbN >= 8) clearInterval(hbTimer);
      }, 2500);
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
  if (unlistenDl) unlistenDl();
  if (unlistenQrFloat) unlistenQrFloat();
  if (unlistenPetMoved) unlistenPetMoved();
  if (unlistenMainVis) unlistenMainVis();
  clearTimeout(petSnapTimer);
  clearTimeout(petHoverTimer);
  clearTimeout(petCloseTimer);
  petStopTimers();
  // 窄屏断点监听: 此前从不反注册, 热重载后回调叠加(closeChat 被重复触发)
  if (typeof mq.removeEventListener === "function") mq.removeEventListener("change", onMqChange);
  else if (typeof mq.removeListener === "function") mq.removeListener(onMqChange);
  clearInterval(wsWatchdogTimer);
  clearTimeout(filesThrottleTimer);
  document.body.style.overflow = "";
  stopIntroSpy();
  closeWs();
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
  try {
    // 移动壳本机节点走 Tauri 命令: HTTP /api/info 出于安全不下发 PIN 码值,
    // 走 HTTP 会导致刚生成的令牌在下一轮轮询被清空、无法复制。
    const d = (isRealDesktop || (mobileShell && isLocalAddr(serverAddr.value)))
      ? await tauriInvoke("control_status_cmd")
      : await (await apiFetch(apiBase.value + API.info)).json();
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
      loadFiles();
      loadChat();
      connectWs();
    }
    running.value = d.running;
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
    if (needPin.value && (e.status === 401 || e.status === 429)) {
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
      seenChat.value = 0;
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
    statusRefreshing = false;
  }
}
</script>

<template>
  <!-- 移动 App 启动门: 本机服务就绪前短暂等待; 失败时必须给出可点的重试出路 -->
  <div v-if="mobileShell && !serverAddr" class="connect-gate">
    <div class="connect-card">
      <h3>{{ mobileBootFailed ? "本机服务启动失败" : "正在启动本机服务…" }}</h3>
      <p v-if="!mobileBootFailed">手机作为独立节点运行本地文件服务与聊天，附近设备会自动发现，无需输入电脑地址。</p>
      <p v-else>服务未能就绪，请点下方重试；连续失败请检查系统是否限制了应用的后台网络。</p>
      <div class="cg-row">
        <span v-if="!mobileBootFailed" class="cg-loading">●</span>
        <button v-else class="btn btn-primary" @click="retryMobileBoot">重新启动本机服务</button>
      </div>
      <template v-if="mobileBootFailed">
        <button class="btn btn-ghost" style="margin-top:10px;width:100%" @click="mobileConnectOpen = !mobileConnectOpen">
          {{ mobileConnectOpen ? "收起" : "改用连接电脑" }}
        </button>
        <div v-if="mobileConnectOpen" class="cg-row" style="margin-top:10px">
          <input v-model="addrInput" placeholder="192.168.1.8:1688" spellcheck="false" @keydown.enter="saveServerAddr" />
          <button class="btn btn-primary" @click="saveServerAddr">连接</button>
        </div>
        <p v-if="mobileConnectOpen && addrErr" class="cg-err">❌ {{ addrErr }}</p>
      </template>
    </div>
  </div>
  <div v-if="!isQrPetWindow" class="shell" :class="{ 'transfer-active': view === 'transfer', 'has-wx-banner': isWeChat, 'real-desktop': isRealDesktop }">
    <!-- 微信内置浏览器拦非微信域名下载: 顶部常驻引导(右上角 ⋯ → 在浏览器打开) -->
    <div v-if="isWeChat" class="wx-banner">ⓘ 当前在微信内打开，文件下载可能被拦截 —— 点右上角 <b>⋯</b> 选择「<b>在浏览器打开</b>」即可正常传输</div>
    <!-- ===== 侧边栏(仅桌面壳) ===== -->
    <aside v-if="isDesktop && !mobileShell" class="sidebar" :class="{ collapsed: sidebarCollapsed }">
      <div class="brand">
        <div class="logo"><img class="logo-img" src="/logo.png" alt="logo" /></div>
        <div class="t">CC-Flash</div>
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
    <main class="main" :class="{ 'transfer-main': view === 'transfer', 'web': !isDesktop }">
      <!-- 顶部栏(桌面壳): 各页统一展示标题与地址; 传输页额外用房间头承载对话操作 -->
      <div v-if="isRealDesktop" class="topbar">
        <div class="topbar-head">
          <h2>{{ VIEW_TITLES[view] || VIEW_TITLES.transfer }}</h2>
          <div class="sub">{{ running ? "同一 Wi-Fi 下的设备扫码或复制地址即可访问" : "服务未启动 —— 到「服务中心」页启动" }}</div>
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
        <div class="transfer-page" :class="{ 'is-mobile': isNarrow, 'is-web': !isDesktop }"
             @dragover.prevent="dragOver = true" @dragleave.prevent="onPageDragLeave"
             @drop.prevent="onDrop">
          <section class="tx-chat">
            <div v-if="isRealDesktop && !running" class="tx-service-bar">
              <span>🔒 服务未启动，手机暂时无法访问</span>
              <button @click="setView('service')">去启动</button>
            </div>
            <header class="tx-head" :class="{ slim: isRealDesktop }">
              <div v-if="isRealDesktop" class="tx-room-brief">{{ running ? "同一 Wi-Fi · 局域网实时互通" : "服务未启动" }}</div>
              <span v-if="!isRealDesktop" class="tx-room">CC</span>
              <div v-if="!isRealDesktop" class="tx-room-title">
                <div class="tx-room-head">
                  <h2>消息发送</h2>
                  <span class="chip" :class="running ? 'run' : 'stop'">
                    <span class="dots" aria-hidden="true"><i></i><i></i><i></i></span><span class="chip-label">{{ running ? "运行中" : "已停止" }}</span>
                  </span>
                </div>
                <p>{{ running ? "同一 Wi-Fi · 局域网实时互通" : "服务未启动" }}</p>
              </div>
              <div class="tx-acts">
                <button class="tx-hbtn tx-clear-btn" title="清空本窗口的消息显示（不影响其他设备）" @click="clearChatDisplay(); moreOpen = false">清空消息</button>
                <button class="tx-hbtn" :class="{ 'hint-pulse': fileHintShow }" title="共享文件" @click="toggleFilePanel">
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z"/></svg>
                  <b>{{ stats.count }}</b>
                  <i v-if="fileDot" class="tx-dot" role="status" title="有新文件"></i>
                  <div v-if="fileHintShow" class="tx-hint-card" @click.stop>
                    <div class="tx-hint-head"><span class="tx-hint-ico">📁</span><b>共享文件都在这里</b></div>
                    <p>预览、下载、分享、删除都在抽屉里，聊天里的文件卡片也会同步。</p>
                    <div class="tx-hint-acts">
                      <button class="tx-hint-ok" @click.stop="openFilePanel">打开共享文件</button>
                    </div>
                  </div>
                </button>
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
                <button v-if="isNarrow" @click="clearChatDisplay(); moreOpen = false">清空消息</button>
                <button v-if="!isRealDesktop" @click="copyUrl(); moreOpen = false">复制地址</button>
                <button v-if="!isRealDesktop" @click="openQr(); moreOpen = false">显示二维码</button>
                <button v-if="!isDesktop" @click="installPwa(); moreOpen = false">安装到主屏幕</button>
                <button v-if="mobileShell" @click="setView('settings'); moreOpen = false">CC设置</button>
                <button v-if="isRealDesktop && isNarrow" @click="setView('service'); moreOpen = false">服务中心</button>
              </div>
            </header>
            <div v-if="peersSorted.length" class="tx-peers">
              <span class="tx-peers-label">附近设备</span>
              <button v-for="p in peersSorted" :key="p.id" class="tx-peer" :class="{ busy: p.busy }"
                      :title="p.busy ? '正在传输' : '发送消息或文件'" @click="openPeerSend(p)">
                <i class="dot"></i><span class="nm">{{ p.nickname || "未命名设备" }}</span>
                <span class="pf" :class="{ on: isFavPeer(p) }" role="button" tabindex="0" title="收藏设备" aria-label="收藏设备" @click.stop="toggleFavPeer($event, p)" @keydown.enter.stop.prevent="toggleFavPeer($event, p)">★</span>
              </button>
            </div>
            <div v-if="moreOpen" class="tx-menu-backdrop" @click="moreOpen = false"></div>

            <div class="tx-list" ref="chatBox" @scroll="onTimelineScroll" @click.self="activeMsgKey = ''">
              <div v-if="!timelineItems.length" class="tx-date">{{ running ? fmtDay(Date.now() / 1000) : "服务已停止" }}</div>
              <div v-if="!timelineItems.length" class="tx-empty">
                <div class="tx-empty-ic">📁</div>
                <h3>还没有内容</h3>
                <p>拖个文件进来，或说句话。</p>
                <div class="tx-empty-actions">
                  <button class="btn btn-primary" @click="pickFiles">上传文件</button>
                  <button class="btn btn-ghost" @click="openFilePanel">共享文件</button>
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
                        <button title="复制" aria-label="复制消息" @click="copyMessage(it.msg)">
                          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z"/><polyline points="14 2 14 8 20 8"/></svg>
                        </button>
                      </div>
                    </div>
                  </div>
                </div>

                <div v-else-if="it.kind === 'remote'" class="tx-file">
                  <span class="tx-ava" :title="it.r.uploader ? '上传者：' + it.r.uploader : '来自其他设备'">{{ avatarFor(it.r.uploader || '共享文件') }}</span>
                  <div class="tx-file-col">
                    <div class="tx-name"><span class="nm">{{ it.r.uploader || "其他设备" }}</span><span class="tm">{{ fmtTime(it.r.modified || it.r.ts) }}</span></div>
                    <div class="tx-filecard">
                      <div class="tx-file-main">
                        <span class="tx-ficon clickable" :class="'tone-' + fileTone(it.r.name)"
                              :title="isRealDesktop ? '打开' : (isPreviewable(it.r.name) ? '预览' : '下载')"
                              @click.stop="openThumb(it.r.name)">
                          <img v-if="isImage(it.r.name)" :src="downloadUrl(it.r.name)" loading="lazy" decoding="async" alt="" @error="$event.target.style.display = 'none'" />
                          <template v-else>{{ iconFor(it.r.name) }}</template>
                        </span>
                        <span class="tx-finfo">
                          <span class="tx-fname" :title="it.r.name">{{ it.r.name }}</span>
                          <span class="tx-fmeta">{{ it.r.uploader ? "来自 " + it.r.uploader : "来自其他设备" }} · {{ fmtSize(it.r.size) }}</span>
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
                    <div class="tx-filecard">
                      <div class="tx-file-main">
                        <span class="tx-ficon" :class="['tone-' + fileTone(it.u.name), { clickable: it.u.status === 'done' }]"
                              :title="it.u.status === 'done' ? (isRealDesktop ? '打开' : (isPreviewable(uploadName(it.u)) ? '预览' : '下载')) : ''"
                              @click.stop="it.u.status === 'done' && openThumb(uploadName(it.u))">
                          <img v-if="it.u.status === 'done' && isImage(it.u.name)" :src="downloadUrl(uploadName(it.u))" loading="lazy" decoding="async" alt="" @error="$event.target.style.display = 'none'" />
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
              </template>

            </div>
            <button v-if="jumpCount" class="tx-jump" :style="{ bottom: `calc(${uploadCount ? 100 : 70}px + env(safe-area-inset-bottom, 0px))` }" @click="jumpToLatest">{{ jumpCount }} 条新消息 · 回到底部</button>

            <div v-if="uploadCount" class="tx-upload-strip">
              <span>正在上传 {{ uploadCount }} 个</span>
              <span class="bar"><i :style="{ width: uploadAvgPct + '%' }"></i></span>
              <span>{{ uploadAvgPct }}%</span>
            </div>

            <div v-if="plusOpen" class="tx-plus-panel" @click.stop>
              <button class="tx-pa" @click="pickFiles(); plusOpen = false"><span class="ic">📄</span>文件</button>
              <button class="tx-pa" @click="pickDir(); plusOpen = false"><span class="ic">🗂</span>文件夹</button>
              <button class="tx-pa" @click="pasteFromClipboard"><span class="ic">📋</span>粘贴</button>
              <button class="tx-pa" @click="openFilePanel(); plusOpen = false"><span class="ic">📁</span>共享文件</button>
            </div>

            <div v-if="quoting" class="tx-quoting">
              <span class="tx-quoting-txt">引用 {{ quoting.nickname }}：{{ quoting.message }}</span>
              <button title="取消引用" aria-label="取消引用" @click="quoting = null">✕</button>
            </div>

            <footer class="tx-composer" :class="{ 'has-quote': quoting }">
              <button class="tx-plus" :class="{ open: plusOpen }" title="上传文件 / 文件夹" @click="togglePlus">＋</button>
              <textarea ref="chatInputEl" class="tx-input" v-model="chatInput" :maxlength="cfg.msgMax" rows="1"
                        :placeholder="running ? (touchOnly ? '说点什么…' : '说点什么…（回车发送，Shift+回车换行）') : '服务未启动'"
                        @focus="onInputFocus" @keydown.enter="onChatEnter" @input="autoGrowInput"></textarea>
              <button class="tx-send" :disabled="!chatInput.trim() || sending" @click="sendChat">{{ sending ? "发送中…" : "发送" }}</button>
            </footer>

            <input type="file" id="fileInput" multiple style="display:none" @change="onPick" />
            <input type="file" id="dirInput" webkitdirectory multiple style="display:none" @change="onPickDir" />

            <div v-if="dragOver" class="tx-drag">
              <div class="tx-drag-ic">⬆</div>
              <b>松手传到电脑</b>
              <small>{{ collecting ? "正在读取拖入的文件…" : "文件 / 文件夹 · 保持目录结构" }}</small>
            </div>
          </section>

          <aside class="tx-files" :class="{ collapsed: !isNarrow && drawerCollapsed, sheet: isNarrow, open: fileSheetOpen }">
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
                <div>📦</div>
                <p>还没有文件</p>
                <button class="btn btn-primary" @click="pickFiles">上传文件</button>
              </div>
              <div v-for="f in pageFiles" :key="f.name" class="tx-frow" :class="{ located: locatedName === f.name }" :data-file="f.name" @dblclick="rowDblClick($event, f)">
                <label class="tx-check" @click.stop>
                  <input type="checkbox" :checked="selected.has(f.name)" @change="toggleSel(f.name)" />
                </label>
                <div class="tx-thumb clickable" :class="'tone-' + fileTone(f.name)"
                     :title="isRealDesktop ? '打开' : (isPreviewable(f.name) ? '预览' : '下载')"
                     @click="openThumb(f.name)">
                  <span>{{ iconFor(f.name) }}</span>
                  <img v-if="isImage(f.name)" :src="downloadUrl(f.name)" loading="lazy" decoding="async" @error="$event.target.style.display = 'none'" />
                </div>
                <div class="tx-finfo2">
                  <div class="tx-fname2" :title="f.name">{{ f.name }}</div>
                  <div class="tx-fmeta2">{{ fmtSize(f.size) }} · {{ fmtTime(f.modified) }}</div>
                </div>
                <div class="tx-frow-acts">
                  <button v-if="isNarrow" title="更多" @click="openFileMenu(f.name)">⋯</button>
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
            </div>
            <div class="tx-files-foot">
              <button v-if="selectedCount" class="tx-del" @click="confirmDeleteSelected">🗑 删除选中 ({{ selectedCount }})</button>
              <span v-else-if="isNarrow" class="tx-files-tip">点 ⋯ 查看操作</span>
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
          <div v-if="isNarrow && fileSheetOpen" class="tx-sheet-backdrop" @click="fileSheetOpen = false"></div>
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

      <!-- 移动壳的非传输页(设置): 顶部返回条 -->
      <div v-if="mobileShell && view !== 'transfer'" class="tx-mobile-viewhead">
        <button title="返回消息发送" @click="setView('transfer')">‹</button>
        <h2>{{ VIEW_TITLES[view] }}</h2>
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

      <!-- CC设置: 真实桌面 = 全量设置; 移动壳 = 连接电脑(客户端模式) -->
      <template v-if="(isRealDesktop || mobileShell) && view === 'settings'">
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
                      :title="pinEnabled ? '点击关闭' : '点击开启'" @click="toggleAccessPin">
                <span class="knob"></span>
              </button>
            </div>
          </div>
          <div class="setting-row">
            <div class="setting-info">
              <div class="setting-title">局域网访问</div>
              <div class="setting-desc">默认开启（同 Wi-Fi 设备可访问）；关闭后仅本机可访问，重启服务后生效</div>
            </div>
            <button class="switch" :class="{ on: openLanEnabled }" role="switch" :aria-checked="openLanEnabled"
                    :title="openLanEnabled ? '点击关闭' : '点击开启'" @click="toggleOpenLan">
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
        <!-- Android/iOS App: 默认本机独立节点, 也可切换到"连接电脑"模式 -->
        <div class="card" v-else>
          <h3>本机服务</h3>
          <p class="about-text">手机 App 作为独立节点运行本地文件服务与聊天，文件保存在应用专属目录；附近设备自动发现，也可改为连接电脑。</p>
          <div class="field-hint">当前连接：{{ apiBase || "启动中…" }} · {{ running ? "运行中" : "未运行" }}</div>
          <div class="setting-row" v-if="!serverAddr || isLocalAddr(serverAddr)">
            <div class="setting-info">
              <div class="setting-title">局域网访问</div>
              <div class="setting-desc">默认关闭（仅本机可访问）；开启后同一 Wi-Fi 下的设备可通过扫码/地址访问本机文件</div>
            </div>
            <button class="switch" :class="{ on: openLanEnabled }" role="switch" :aria-checked="openLanEnabled"
                    :title="openLanEnabled ? '点击关闭' : '点击开启'" @click="toggleOpenLan">
              <span class="knob"></span>
            </button>
          </div>
          <div class="setting-row" v-if="!serverAddr || isLocalAddr(serverAddr)">
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
                      :title="pinEnabled ? '点击关闭' : '点击开启'" @click="toggleAccessPin">
                <span class="knob"></span>
              </button>
            </div>
          </div>
          <div class="setting-row" v-if="!serverAddr || isLocalAddr(serverAddr)">
            <div class="setting-info">
              <div class="setting-title">重启本机服务</div>
              <div class="setting-desc">局域网访问/令牌开关修改后需重启服务才会生效</div>
            </div>
            <button class="btn btn-ghost" style="height:28px" @click="restartMobileService">重启服务</button>
          </div>
          <p class="about-text" v-if="serverAddr && !isLocalAddr(serverAddr)" style="margin-top:10px;">
            当前为「连接电脑」模式：手机本机服务已停止，本机设置已隐藏。
          </p>
          <div class="svc-fields" style="grid-template-columns: 1fr; margin-top:10px;">
            <div class="svc-field">
              <label>连接电脑（可选）</label>
              <div class="dir-row">
                <input v-model="addrInput" placeholder="192.168.1.8:1688" spellcheck="false" @keydown.enter="saveServerAddr" />
                <button class="btn btn-primary" style="height:34px; padding:0 16px;" @click="saveServerAddr">连接</button>
              </div>
              <div class="field-hint">{{ addrErr || "留空使用本机节点；连接后手机可浏览电脑上的共享文件" }}</div>
            </div>
          </div>
          <button class="btn btn-ghost" style="height:30px; margin-top:8px;" @click="useLocalNode">切回本机节点</button>
          <p class="about-text" style="margin-top:10px;">开机自启、共享目录选择等桌面功能仅在 macOS / Windows 版提供。</p>
        </div>
        <!-- 皮肤设置: 两行"标签+控件"(外观/配色), 色板芯片即按钮本体 -->
        <div class="card svc-skin">
          <h3>皮肤设置</h3>
          <div class="skin-row">
            <span class="skin-label">外观</span>
            <div class="mode-row" role="group" aria-label="主题模式">
              <button v-for="m in THEME_MODES" :key="m.id" class="mode-btn" :class="{ active: themeMode === m.id }"
                      @click="setThemeMode(m.id)">
                <span>{{ m.icon }}</span><span>{{ m.name }}</span>
              </button>
            </div>
          </div>
          <div class="skin-row">
            <span class="skin-label">配色</span>
            <div class="theme-row">
              <button v-for="t in THEMES" :key="t.id" class="theme-sw" :class="{ active: theme === t.id }"
                      :style="{ background: t.preview }" :title="t.name" :aria-label="t.name"
                      @click="setTheme(t.id)"></button>
              <span class="skin-cur">当前 · {{ currentThemeName }}</span>
            </div>
          </div>
          <div class="sub-hint">跟随系统随系统外观自动切换；即时生效并自动记忆</div>
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

      <!-- 页脚: 版权(内容不满一屏时贴底)。浏览器/移动壳所有页面都显示(含传输页),
          桌面端仅 CC设置页保留 -->
      <!-- 手机浏览器不占底部版权条: 把高度全部留给输入栏与聊天区 -->
      <footer v-if="(!isDesktop && !isNarrow) || (isRealDesktop && view === 'settings')" class="app-foot">{{ FOOTER_TEXT }}</footer>
    </main>
  </div>

  <!-- 独立桌面 Pet 窗：贴边收拢的呼吸圆点，悬停预览扫码卡；显隐只由 CC设置开关控制 -->
  <template v-if="isQrPetWindow">
    <div class="qr-pet"
         :class="[{ down: !running || !qrData, expanded: qrPetShown }, 'edge-' + petEdge]"
         role="button" tabindex="0"
         :title="qrPetShown ? '' : (petNotify ? '有新文件或消息 · 点按查看' : '')"
         :aria-label="(petNotify ? '有新文件或消息。' : '') + 'CC-Flash 悬浮二维码：指针停留 1.5 秒预览，拖动调整位置，点按打开主窗口'"
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
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 01-2 2H7l-4 4V5a2 2 0 012-2h14a2 2 0 012 2z"/></svg></div>
          <h3>文件与消息</h3>
          <p>文件和文字在一条对话里：上传即出现文件卡片（等待 / 上传中 / 完成 / 失败），批量上传自动合并成文件组；聊天支持标题、列表、引用、代码块、粗体与链接等 Markdown，输入框可多行编辑（桌面 Enter 发送、Shift+Enter 换行；手机 Enter 换行后点发送）。图片、PDF、文本、音视频可在应用内直接预览，右上角关闭即回到聊天。完整共享文件列表收进右侧抽屉 / 手机底部面板，排序、分页、批量删除都在。</p>
          <div class="tags"><span class="tag">文件即消息</span><span class="tag">Markdown</span><span class="tag">应用内预览</span><span class="tag">共享文件</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg></div>
          <h3>服务中心</h3>
          <p>启停服务、改端口（被占自动顺延）、换共享目录，磁盘余量一目了然。双二维码入口：侧边栏常驻码（流光动效）+ 屏幕边缘<b>呼吸球悬浮码</b>（悬停 1.5 秒出码、拖动换边、支持扩展屏与 Windows 混合 DPI、新消息绿点提醒），任意页面随扫随传；服务日志 loguru 风格实时滚动，五套配色 × 亮色 / 暗色 / 跟随系统即点即换，桌面对话气泡也随主题皮肤同步。</p>
          <div class="tags"><span class="tag">服务控制</span><span class="tag">悬浮扫码</span><span class="tag">多主题配色</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 01-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z"/></svg></div>
          <h3>CC设置</h3>
          <p>开机自动启动可按需开启（在 CC设置 中打开开关），登录电脑服务就在；关窗收进托盘、服务保持运行。一键检查更新，新版本发布即刻知晓。</p>
          <div class="tags"><span class="tag">开机自启</span><span class="tag">托盘常驻</span><span class="tag">检查更新</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="4" width="20" height="12" rx="2"/><line x1="8" y1="20" x2="16" y2="20"/><line x1="12" y1="16" x2="12" y2="20"/></svg></div>
          <h3>附近设备</h3>
          <p>同一 Wi-Fi 下运行的 CC-Flash 节点（电脑、移动 App）通过 UDP 多播/广播自动出现，点一下即可把消息或文件投递到对方节点，常用设备可收藏置顶。纯浏览器页面没有自己的监听端口，不会出现在可发送列表里，避免"点得到、连不上"。</p>
          <div class="tags"><span class="tag">自动发现</span><span class="tag">点对点投递</span><span class="tag">设备收藏</span></div>
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
          <p>拖文件上传、点按钮下载、随时发消息；顶部「附近设备」可把内容直接投递给另一台 CC-Flash 节点。手机不用装 App；电脑关窗进托盘，服务也不停。</p>
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
          <p>HTTP 服务只在局域网内直达 —— 不上云、无账号体系、不经过任何第三方服务器，断外网照样用。桌面默认开放局域网；移动 App 默认仅本机可访问，需在 CC设置 手动开启。可选 6 位访问令牌（单 IP 指数退避 + 公网全局失败预算、本机免输），分享链接为一次性票据，文件删除/同名重传后旧链接立即失效。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg></div>
          <h3>大文件不占内存</h3>
          <p>后端流式读写，超大文件轻松传；浏览器端下载直接交给浏览器原生流式下载，边下边存、实时进度，稳定省资源。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="7" width="20" height="14" rx="2"/><path d="M16 21V5a2 2 0 00-2-2h-4a2 2 0 00-2 2v16"/></svg></div>
          <h3>服务常驻不断</h3>
          <p>关窗即收进托盘，服务保持运行，手机端不受影响；开机自启可在「CC设置」开启，登录电脑即可用，真正退出走托盘菜单。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="2" width="14" height="20" rx="2"/><line x1="12" y1="18" x2="12.01" y2="18"/></svg></div>
          <h3>多端覆盖</h3>
          <p>桌面端支持 macOS（11.0+）与 Windows（Linux 可源码构建）；手机浏览器免安装即可用；Android App 默认作为独立节点，也可切换为连接电脑；iOS 暂无安装包，请直接用 Safari 打开电脑端地址。</p>
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
          <div class="faq-a" :style="{ maxHeight: faqOpen === i ? '640px' : '0' }"><p>{{ f.a }}</p></div>
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
      <img v-if="previewKind === 'image'" :src="previewUrl" alt="" @error="previewErr = '图片加载失败'" />
      <video v-else-if="previewKind === 'video'" :src="previewUrl" controls playsinline preload="metadata"></video>
      <audio v-else-if="previewKind === 'audio'" :src="previewUrl" controls preload="metadata"></audio>
      <iframe v-else-if="previewKind === 'pdf'" :src="previewUrl" title="PDF 预览"></iframe>
      <template v-else>
        <div v-if="previewLoading" class="pv-tip">加载中…</div>
        <div v-else-if="previewErr" class="pv-tip err">{{ previewErr }}</div>
        <pre v-else class="pv-text">{{ previewText }}</pre>
      </template>
      <div v-if="previewErr && previewKind !== 'text'" class="pv-tip err">{{ previewErr }}</div>
    </div>
  </div>

  <!-- 点对点发送到附近设备(不合并聊天, 只把消息/文件投递到对方节点) -->
  <div v-if="peerTarget" class="peer-send-mask" @click.self="peerTarget = null">
    <div class="peer-send-box">
      <h4>发送到 {{ peerTarget.nickname || "设备" }}</h4>
      <p>{{ peerTarget.ip }}:{{ peerTarget.port || port }} · {{ peerTarget.busy ? "忙碌中" : "可接收" }}</p>
      <p class="peer-hint">消息/文件会投递到对方的公共聊天与共享目录，同网段设备可见；请勿发送敏感内容。</p>
      <div class="peer-pin-row">
        <input v-model="peerPin" :disabled="peerSending" placeholder="对方配对码（若对方开启访问令牌）"
               inputmode="numeric" autocomplete="off" maxlength="12" />
      </div>
      <textarea v-model="peerText" :disabled="peerSending" placeholder="输入消息，发送后对方会在自己的聊天里看到…"></textarea>
      <input ref="peerFileInput" type="file" multiple hidden @change="sendFilesToPeer" />
      <div class="peer-send-actions">
        <button class="btn btn-primary" :disabled="peerSending" @click="pickPeerFiles">{{ peerSending ? "发送中…" : "发送文件" }}</button>
        <button class="btn btn-ghost" :disabled="peerSending" @click="sendTextToPeer">发送消息</button>
        <button class="btn btn-ghost" @click="copyPeerAddr">复制地址</button>
        <button class="btn btn-ghost" @click="peerTarget = null">关闭</button>
      </div>
    </div>
  </div>

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

  <!-- 配对弹框(网页端): 电脑端开启访问令牌保护后, 401 引导到此输入一次 -->
  <div v-if="needPin" class="dialog-overlay">
    <div class="dialog-box pin-box">
      <h4>需要配对</h4>
      <p class="pin-sub">输入电脑「访问令牌」完成配对</p>
      <input class="svc-input pin-input" v-model="pinInput" maxlength="12" placeholder="配对码"
             autocomplete="off" inputmode="numeric"
             @keydown.enter.prevent="submitPin" />
      <button class="btn btn-primary pin-btn" @click="submitPin">配 对</button>
      <button class="btn btn-ghost" style="margin-top:8px;width:100%" @click="needPin = false">稍后再输入</button>
    </div>
  </div>

  <!-- 失联全屏(网页端): 电视"无信号"测试卡样式, 电脑/手机浏览器表现一致 -->
  <div v-if="!isRealDesktop && offline" class="nosignal-overlay">
    <img class="ns-img" :src="noSignalImg" alt="无信号测试卡" />
    <h3>📡 无信号</h3>
    <p class="ns-line">{{ mobileShell ? (isLocalAddr(serverAddr) ? "与本机服务的连接已断开" : "与电脑的连接已断开") : "与服务的连接已断开 —— 服务可能已停止，或电脑地址已变更" }}</p>
    <p class="ns-sub">{{ mobileShell ? (isLocalAddr(serverAddr) ? "本地服务可能正在重启，请稍候或点下方重试" : "请确认电脑已开机、地址未变，或点下方修改电脑地址") : "请在电脑上查看最新地址后重新打开本页" }}</p>
    <button v-if="mobileShell" class="btn btn-primary" style="margin-top:18px" @click="retryMobileLocal">
      {{ isLocalAddr(serverAddr) ? "重新启动本机服务" : "重新连接电脑" }}
    </button>
    <button v-if="mobileShell" class="btn btn-ghost" style="margin-top:8px" @click="mobileConnectOpen = !mobileConnectOpen">
      {{ mobileConnectOpen ? "收起" : "修改电脑地址" }}
    </button>
    <div v-if="mobileShell && mobileConnectOpen" class="cg-row" style="margin-top:10px;width:100%;max-width:340px">
      <input v-model="addrInput" placeholder="192.168.1.8:1688" spellcheck="false" @keydown.enter="saveServerAddr" />
      <button class="btn btn-primary" @click="saveServerAddr">连接</button>
    </div>
    <p v-if="mobileShell && mobileConnectOpen && addrErr" class="cg-err">❌ {{ addrErr }}</p>
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
      <h4>CC-Flash 已开启开机启动</h4>
      <p>登录电脑后，CC-Flash 将在后台自动启动（不弹出主窗口）。请从「应用程序」目录中的正式安装版开启 —— 若当前从 DMG 或开发目录运行，登录自启会指向该临时路径而失效。可随时在「CC设置」中关闭。</p>
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
  <div class="toast-container">
    <div v-for="t in toasts" :key="t.id" class="toast-msg" :class="'toast-' + t.type">
      <span>{{ t.msg }}</span>
      <button v-if="t.action" type="button" class="toast-act" @click="runToastAction(t)">{{ t.action.label }}</button>
    </div>
  </div>
</template>
