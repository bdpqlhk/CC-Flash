<script setup>
import { ref, computed, watch, onMounted, onUnmounted, nextTick } from "vue";
import QRCode from "qrcode";
// 失联提示图: 构建期内联为 base64, 服务停止后无需再发请求
import noSignalImg from "./assets/no-signal.png";
// 可调参数集中管理(轮询/阈值/路径/文案等)
import {
  API, APP_VERSION_FALLBACK, AVATAR_POOL, BLOB_REVOKE_MS, COPIED_RESET_MS, CONTROL_RETRY_MS,
  CONTROL_RETRY_TIMES, DEFAULT_NICK, DEFAULT_PORT, FOOTER_TEXT, ICON_MAP, IMG_EXTS, LS, MSG_MAX,
  GITHUB_REPO, NARROW_MAX_PX, NICK_MAX, OFFLINE_FAILS, PAGE_SIZE, POLL_CHAT_MS,
  POLL_FILES_MS, POLL_LOGS_MS, POLL_STATUS_MS, REFRESH_SPIN_MS, RELEASES_PAGE,
  REL_TIME_MS, THEME_MIGRATE, TIP_COPY_URL, TIP_QR, TIP_SHARE, TOAST_MS, UPDATE_CHECK_DELAY_MS,
  UPDATE_CHECK_INTERVAL_MS, UPLOAD_DONE_KEEP_MS, WS_HB_TIMEOUT_MS, WS_WATCHDOG_MS,
} from "./config";
import { fetchLatestRelease, isNewer } from "./update";
import { open as openDialog, ask as askDialog } from "@tauri-apps/plugin-dialog";
// 模块化 API 取代 window.__TAURI__ 全局暴露(withGlobalTauri=false): XSS 即便发生也拿不到
// 覆盖全部自定义命令的 invoke 面 —— isDesktop 判定走 __TAURI_INTERNALS__(IPC 内部注入, 不受此开关影响)
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

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
function tauriInvoke(cmd, args) { return invoke(cmd, args || {}); }

/* ---------- 访问码(可选; 电脑端 CC设置 开启后, 局域网网页端需输入一次) ---------- */
const accessPin = ref((() => { try { return localStorage.getItem(LS.pin) || ""; } catch (e) { return ""; } })());
const needPin = ref(false);      // 401 → 弹出输入框; 验证通过后全量重载
const pinInput = ref("");
const pinEnabled = ref(false);   // 桌面设置页展示用(仅 Tauri 命令下发, HTTP /api/info 不携带码值)
const pinCode = ref("");
function withPin(url, pin = accessPin.value) {
  return pin ? url + (url.includes("?") ? "&" : "?") + "pin=" + encodeURIComponent(pin) : url;
}
/* 统一请求入口: 自动附带令牌; 401/429 时置 needPin 配对弹框(网页端输入一次后全流程透明)。
   429 = 配对码连续错进退避锁定 —— 若不当配对信号处理, 页面会静默退化成"已停止/无文件" */
async function apiFetch(url, opts) {
  const r = await fetch(withPin(url), opts);
  if (r.status === 401 || r.status === 429) {
    needPin.value = true;
    throw new Error(r.status === 429 ? "配对尝试过于频繁" : "需要配对码");
  }
  return r;
}
/* 下载/图片/分享链接统一经此: 分享链接带上令牌, 接收方打开即验证 */
function downloadUrl(name) { return withPin(apiBase.value + API.download(name)); }
async function submitPin() {
  const p = pinInput.value.trim();
  if (!p) return;
  try {
    const r = await fetch(apiBase.value + API.info + "?pin=" + encodeURIComponent(p));
    if (r.ok) {
      accessPin.value = p;
      try { localStorage.setItem(LS.pin, p); } catch (e) {}
      needPin.value = false;
      statusFail = 0;
      offline.value = false;
      showToast("配对成功");
      refreshStatus(); loadFiles(); loadChat(); connectWs();
    } else if (r.status === 429) showToast("尝试过于频繁，请稍后再试", "error");
    else showToast("配对码不正确", "error");
  } catch (e) { showToast("验证失败，请检查网络", "error"); }
}
async function toggleAccessPin() {
  try {
    const d = await tauriInvoke("set_access_pin_cmd", { enable: !pinEnabled.value });
    if (d.ok) {
      pinEnabled.value = !pinEnabled.value;
      pinCode.value = d.pin || "";
      showToast(pinEnabled.value ? `访问令牌已开启：${d.pin}` : "访问令牌已关闭");
    } else showToast(d.error || "设置失败", "error");
  } catch (e) { showToast("设置失败，请重试", "error");
  }
}
/* 当前设备昵称(供日志/请求头使用) */
function deviceNick() { return nickInput.value.trim() || autoNick; }

/* ---------- 视图切换 ---------- */
const view = ref("files");   // files | chat | service | settings
const VIEW_TITLES = { files: "文件传输", chat: "文本消息", service: "服务中心", settings: "CC设置" };
function setView(v) {
  view.value = v;
  // 切进聊天页立即追平已读: WS 在线时 loadChat 只由新消息事件触发,
  // 不追平的话未读徽标会一直挂到下一条消息到达才消失
  if (v === "chat") {
    seenChat.value = chatCount.value;
    // 桌面聊天视图是 v-if 惰性挂载: 全新容器从顶部(最旧一条)开始, 切入即定位到最新一条;
    // loadChat 的滚动有"条数变化"门槛, 消息没更新时不会滚 —— 必须在此处主动滚
    nextTick(scrollChats);
  }
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
/* 6 套 = 每色相一个(蓝/灰/绿/紫/金/红粉); v1.1.6 从 9 套精简,
   被移除的选择在 main.js 启动时迁移(aurora→default, mist→graphite, eye→mint) */
const THEMES = [
    { id: "default",  name: "深空蓝",   preview: "linear-gradient(135deg,#0d1017,#1a2233)" },
    { id: "graphite", name: "石墨灰",   preview: "linear-gradient(135deg,#15171b,#2a2f38)" },
    { id: "mint",     name: "森林薄荷", preview: "linear-gradient(135deg,#0f1712,#1d4634)" },
    { id: "purple",   name: "暗夜紫",   preview: "linear-gradient(135deg,#15121d,#33265a)" },
    { id: "amber",    name: "墨色琥珀", preview: "linear-gradient(135deg,#141109,#4a3512)" },
    { id: "rose",     name: "石墨玫瑰", preview: "linear-gradient(135deg,#16151a,#4a2531)" },
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
try {
  // 读取时同样应用迁移: 本模块先于 main.js 本体求值, 不迁移的话升级后
  // 首次启动选择器高亮会停留在已被移除的旧 id 上(网格无高亮)
  let t = localStorage.getItem(LS.theme) || "default";
  if (THEME_MIGRATE[t]) {
    t = THEME_MIGRATE[t];
    try { localStorage.setItem(LS.theme, t); } catch (e) {}
  }
  theme.value = t;
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
const themeMode = ref((() => { try { return localStorage.getItem(LS.mode) || "dark"; } catch (e) { return "dark"; } })());
function applyThemeMode() {
    // 显式写 data-mode("dark" 无对应 CSS 规则 = 默认深色), system 按当前系统外观解析
    document.documentElement.dataset.mode =
        themeMode.value === "system" ? (systemDarkMq.matches ? "dark" : "light") : themeMode.value;
}
function setThemeMode(m) {
    themeMode.value = m;
    applyThemeMode();
    try { localStorage.setItem(LS.mode, m); } catch (e) {}
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
  // sheetUp 入场动画(0.3s)期间, 部分移动内核(WebKit 系尤其)会丢弃 display:none→block
  // 容器刚设置的滚动位置 → 打开面板停在顶部。动画结束后再钉一次, 确保必定位到最新一条
  setTimeout(scrollChats, 350);
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
        : await (await apiFetch(API.info)).json();
      running.value = d.running;
      port.value = d.port;
      lanIp.value = d.ip;
      if (typeof d.version === "string") appVersion.value = d.version;
      if (typeof d.pinEnabled === "boolean") { pinEnabled.value = d.pinEnabled; pinCode.value = d.pin || ""; }
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
  if (wasRunning) {
    const restarted = await _restartForDirSwitch();
    if (restarted) showToast("共享目录已切换，服务已重启");
    return { ok: true, restarted: restarted };
  }
  return d2;
}
async function startServer() {
  if (!isDesktop) { showToast("请在桌面端控制服务", "error"); return; }
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
    const r = await apiFetch(apiBase.value + API.files + "?nick=" + encodeURIComponent(deviceNick()));
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
  // 浏览器端优先 shareUrl(与二维码同源), 但首次状态刷新前 lanIp 还是占位 "--" ——
  // 此时必须回落 location.origin, 否则复制出 http://--:0 无效地址
  const base = isDesktop
    ? `http://${lanIp.value}:${port.value}`
    : (lanIp.value === "--" ? location.origin : shareUrl.value);
  // 链接嵌入当前访问码, 接收方打开即过校验:
  // 浏览器端用本机已验证的 accessPin; 桌面壳走回环豁免不存码, 但可嵌入可信 IPC
  // 下发的 pinCode —— 否则壳复制的链接永远裸奔, 接收方必然撞 401
  const url = withPin(base + API.download(name), accessPin.value || pinCode.value);
  // 链接内嵌了配对码 = 收到的人不止能下这个文件, 而是可访问全部共享内容(上传/删除/聊天)
  const pinNote = (accessPin.value || pinCode.value)
    ? "\n⚠️ 链接含配对码：对方可访问全部共享内容，请仅发给可信对象"
    : "";
  copyText(url + TIP_SHARE + pinNote, "分享链接已复制：" + name);
}

/* ---------- 下载(浏览器端): 手机小文件 fetch 回来再触发保存(有"下载完成"时机);
   桌面浏览器一律 HEAD 预检 + 原生流式下载, 见 downloadFileWeb 内注释 ---------- */
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
  const url = downloadUrl(name);
  const base = name.split("/").pop() || name;   // download 属性不带子目录路径(部分移动端拒收)
  // 桌面浏览器: HEAD 预检后直接交浏览器原生流式下载。
  // 旧逻辑 ≤1GB 会把整个文件 fetch 进内存(blob)才触发保存 —— 500MB 在 Wi-Fi 上要
  // 等几十秒且期间零进度零提示, 用户感知就是"点了没反应"; 等 blob 期间浏览器瞬态
  // 用户激活早已过期, 程序化 click 可能被静默拦截。现在文件本体由浏览器下载管理器
  // 流式落盘(自带进度条/断点续传, 零内存占用), fetch 只拿状态码 —— 响应头毫秒级
  // 到达, 激活仍有效; 404/401 如实报错而不是把 JSON 错误体当文件下载。
  // 桌面/移动壳(isDesktop)同样走原生下载: WebView 的锚点下载由 on_download
  // 处理器接管落盘(壳内没有浏览器下载管理器, blob 路径在壳内不可靠)
  if (isDesktop || (!isIOSFamily && !/Android/.test(navigator.userAgent))) {
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
  let gotResp = false;
  try {
    // 响应头必须带超时(对齐桌面分支的 8s): 服务半开时 fetch 永久挂起 = "点了没反应"。
    // 拿到响应后立即清表, blob 阶段不限时, 免得慢速大文件被误杀
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), 10000);
    const r = await fetch(url, { signal: ctl.signal });
    clearTimeout(timer);
    gotResp = true;
    if (!r.ok) {
      // HTTP 错误(404/400 等)如实报错, 不走原生回退 —— 否则会把 JSON 错误体当文件下载
      showToast(`下载失败：HTTP ${r.status}`, "error");
      return;
    }
    const len = Number(r.headers.get("Content-Length") || 0);
    // 手机浏览器/WebView 的 blob 内存配额远低于桌面(iOS Safari 数百 MB 即杀标签页且无报错),
    // 阈值降到 50MB, 超出一律走原生流式下载
    if (len > 50 * 1024 * 1024) {
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
    // 响应头都没等到 = 链路不通, 回退原生下载也只会再失败一次 —— 直接报超时
    if (!gotResp) {
      showToast("下载失败：连接超时，请确认电脑端服务已启动", "error");
      return;
    }
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
      const r = await apiFetch(apiBase.value + API.del(n), { method: "DELETE" });
      r.ok ? ok++ : fail++;
    } catch (e) { fail++; }
  }
  if (fail) showToast(`删除完成：成功 ${ok} 个，失败 ${fail} 个`, "error");
  else showToast(targets.length > 1 ? `已删除 ${ok} 个文件` : "已删除 「" + targets[0] + "」");
  selected.value = new Set();
  loadFiles(true);   // 手动刷新: 带旋转反馈; 与轮询在途竞争时由补拉机制保证不丢
}

/* ---------- 上传(单请求整文件直传 + 整目录) ---------- */
const uploads = ref([]);
let upSeq = 0;
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
const collecting = ref(false);   // 递归收集拖入条目期间置位: 超大目录树收集耗时秒级, 得让用户知道在干活
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
async function updateWakeLock() {
  const want = uploads.value.some(u => u.status === "uploading" || u.status === "waiting");
  setScreenKeepAlive(want);
  try {
    if (want && !wakeLock && navigator.wakeLock) {
      wakeLock = await navigator.wakeLock.request("screen");
      wakeLock.addEventListener("release", () => { wakeLock = null; });
    } else if (!want && wakeLock) {
      try { await wakeLock.release(); } catch (e) {}
      wakeLock = null;
    }
  } catch (e) {}
}
function uploadFile(file) {
  // 单请求整文件直传(1.1.2 传输) + 保留功能: 同名自动改名、取消✕、锁屏防中断
  const id = "up-" + Date.now() + "-" + upSeq++;
  const name = uploadRelName(file);   // 文件夹入口保留: 相对路径经 X-Filename 下传
  uploads.value.push({ id, name, pct: 0, speed: "", status: "waiting" });   // waiting = 真实排队
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
      xhr.open("POST", withPin(apiBase.value + API.upload), true);
      xhr.setRequestHeader("X-Filename", encodeURIComponent(name));
      xhr.setRequestHeader("X-File-Size", String(file.size));
      xhr.setRequestHeader("Content-Type", "application/octet-stream");
      const t0 = Date.now();
      xhr.upload.onprogress = (e) => {
        const it = stat(); if (!it) return;
        it.status = "uploading";
        if (e.lengthComputable) {
          it.pct = Math.round((e.loaded / e.total) * 100);
          const sp = e.loaded / Math.max((Date.now() - t0) / 1000, 0.1);
          it.speed = fmtSize(sp) + "/s";
        }
      };
      const finish = () => resolve();
      xhr.onload = () => {
        const it = stat(); if (!it) return finish();
        if (xhr.status === 200) {
          it.pct = 100; it.status = "done";
          // 同名自动改名: 服务端把实际落盘名放 final 字段, 变了才提示"已存为"
          let d = {};
          try { d = JSON.parse(xhr.responseText); } catch (_) {}
          const base = name.split("/").pop();
          const finalBase = typeof d.final === "string" && d.final ? d.final.split("/").pop() : "";
          if (finalBase && finalBase !== base) {
            it.final = finalBase;
            showToast("目录里已有同名文件，已改存为 「" + finalBase + "」");
          }
          updateWakeLock();
          loadFiles();
          setTimeout(() => { uploads.value = uploads.value.filter(u => u.id !== id); }, UPLOAD_DONE_KEEP_MS);
        } else {
          // 配对码是独立保留功能: 401/429 唤起弹框; 失败文案与 1.1.2 逐字一致
          if (xhr.status === 401 || xhr.status === 429) needPin.value = true;
          it.status = "error";
          updateWakeLock();
          showToast("上传失败: " + name, "error");
        }
        finish();
      };
      xhr.onerror = () => {
        const it = stat(); if (!it) return finish();
        it.status = "error";
        updateWakeLock();
        showToast("上传失败: " + name, "error");
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
    const r = await apiFetch(apiBase.value + API.chat + "?nick=" + encodeURIComponent(deviceNick()), { signal: ctl.signal });
    clearTimeout(timer);
    const d = await r.json();
    const msgs = d.messages || [];
    const old = chatCount.value;
    // 水位过滤是"已清空历史不再复现"的唯一保证: 旧实现用 chatCleared 布尔开关, 新消息一到
    // 开关即解除, 全量 msgs 覆盖显示 → 之前清掉的历史整段回来。过滤后新消息正常出现,
    // 水位之下(已清空)的永远不再进入本窗口; clearedTs 初始为 0, 未清空时全员放行
    chatMsgs.value = msgs.filter((m) => m.timestamp > clearedTs.value);
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
/* 回车发送需豁免输入法组字: 中文 IME 组字期间的 Enter 是"确认候选词",
   isComposing/keyCode 229 时不得触发发送 —— 否则把未上屏的拼音串当消息发出去 */
function onChatEnter(e) {
  if (e.isComposing || e.keyCode === 229) return;
  e.preventDefault();
  sendChat();
}
async function sendChat() {
  const text = chatInput.value.trim();
  if (!text) return;
  const nickname = curNick.value;
  saveNick();
  try {
    const r = await apiFetch(apiBase.value + API.chat + "?nick=" + encodeURIComponent(nickname), {
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
  if (isDesktop) return port.value ? `ws://127.0.0.1:${port.value}${API.ws}` : null;
  // withPin: 访问码开启后非回环 WS 握手同样需要校验
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
let unlistenDl = null;   // 壳内下载完成事件的反注册
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
  // 壳内下载完成通知(Rust on_download 落盘后 emit): 提示保存位置
  if (isDesktop) {
    listen("download://done", (e) => {
      const p = e.payload || {};
      p.success ? showToast("已下载到 " + (p.path || "下载目录")) : showToast("下载失败", "error");
    }).then((un) => { unlistenDl = un; });
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
  if (unlistenDl) unlistenDl();
  clearInterval(wsWatchdogTimer);
  clearTimeout(filesThrottleTimer);
  document.body.style.overflow = "";
  stopIntroSpy();
  closeWs();
  stopPolling();
});

const logsBox = ref(null);
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
watch(() => view.value, (v) => { if (v === "service") refreshLogs(); });
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
      : await (await apiFetch(API.info)).json();
    if (typeof d.pinEnabled === "boolean") { pinEnabled.value = d.pinEnabled; pinCode.value = d.pin || ""; }
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
    if (needPin.value) return;   // 401 已弹访问码输入框, 不按失联处理(不清空数据不显示无信号)
    // 浏览器端连续失败 → 判定与服务失联: 状态回落为"已停止", 共享列表/聊天数据一并清空不再展示
    if (!isDesktop && ++statusFail >= OFFLINE_FAILS) {
      offline.value = true;
      running.value = false;
      allFiles.value = [];
      selected.value = new Set();
      chatMsgs.value = [];
      chatCount.value = 0;
      seenChat.value = 0;
      // clearedTs 不重置: 本机清空意图不应被一次网络抖动(离线→恢复)作废,
      // 否则短暂失联重连后已清空的历史又会全量回来
    }
  }
}
</script>

<template>
  <div class="shell">
    <!-- 微信内置浏览器拦非微信域名下载: 顶部常驻引导(右上角 ⋯ → 在浏览器打开) -->
    <div v-if="isWeChat" class="wx-banner">ⓘ 当前在微信内打开，文件下载可能被拦截 —— 点右上角 <b>⋯</b> 选择「<b>在浏览器打开</b>」即可正常传输</div>
    <!-- ===== 侧边栏(仅桌面壳) ===== -->
    <aside v-if="isDesktop" class="sidebar" :class="{ collapsed: sidebarCollapsed }">
      <div class="brand">
        <div class="logo"><img class="logo-img" src="/logo.png" alt="logo" /></div>
        <div class="t">CC-Flash</div>
        <button class="intro-entry" title="软件介绍" aria-label="软件介绍" @click="openIntro">
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>
          介绍
        </button>
      </div>
      <nav class="nav">
        <button class="nav-item" :class="{ active: view === 'files' }" title="文件传输" @click="setView('files')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z"/></svg></span><span class="nav-label">文件传输</span>
        </button>
        <button v-if="isDesktop || !isNarrow" class="nav-item" :class="{ active: view === 'chat' }" title="文本消息" @click="setView('chat')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 01-2 2H7l-4 4V5a2 2 0 012-2h14a2 2 0 012 2z"/></svg></span><span class="nav-label">文本消息</span>
          <span v-if="unread > 0 && view !== 'chat'" class="nav-badge">{{ unread > 99 ? "99+" : unread }}</span>
        </button>
        <button v-if="isDesktop || !isNarrow" class="nav-item svc" :class="{ active: view === 'service' }" title="服务中心" @click="setView('service')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg></span><span class="nav-label">服务中心</span>
        </button>
        <button v-if="isDesktop || !isNarrow" class="nav-item" :class="{ active: view === 'settings' }" title="CC设置" @click="setView('settings')">
          <span class="ico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 01-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z"/></svg></span><span class="nav-label">CC设置</span>
        </button>
      </nav>
      <!-- 全局常驻扫码(桌面壳): 服务运行中即在任意视图直接可扫, 不用切页/点顶栏图标;
           点击弹大图(含完整地址)。收起态保留 40px 迷你码, 悬停浮出可扫大图(见 CSS) -->
      <div v-if="running && qrData" class="side-qr" title="点击放大" @click="openQr">
        <span class="side-qr-view">
          <img class="side-qr-img" :src="qrData" alt="扫码打开" />
          <span class="side-qr-scan" aria-hidden="true"></span>
        </span>
        <div class="side-qr-cap">扫码打开</div>
      </div>
      <div class="side-foot">
        <div class="side-status" :class="{ on: running }" :title="running ? `运行中 :${port}` : '已停止'">
          <span class="d"></span><span class="side-status-text">{{ running ? `运行中 :${port}` : "已停止" }}</span>
        </div>
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
            <h5>{{ collecting ? "正在读取拖入的文件…" : "点击选择文件 或 拖拽到此处" }}</h5>
            <p>支持多选与<b>整个文件夹</b>（保持目录结构） · 可直接 Ctrl+V / Cmd+V 粘贴文件
              <span v-if="!isIOS"> · <a class="dir-pick" @click.stop="pickDir" title="选择文件夹上传">选择文件夹</a></span>
              · 大文件稳定传输，锁屏自动防中断</p>
            <input type="file" id="fileInput" multiple style="display:none" @change="onPick" />
            <input type="file" id="dirInput" webkitdirectory multiple style="display:none" @change="onPickDir" />
          </div>
          <div v-for="u in uploads" :key="u.id" class="upload-item">
            <div class="up-line">
              <span class="up-name">{{ u.name }}</span>
              <span class="up-status" :class="u.status">
                {{ u.status === "uploading" ? u.pct + "% " + u.speed
                 : u.status === "waiting" ? "等待中"
                 : u.status === "done" ? (u.final ? "已存为 " + u.final : "完成") : u.speed || "失败" }}
              </span>
              <button v-if="u.status === 'waiting' || u.status === 'uploading'" class="icon-btn" title="取消上传"
                      style="margin-left:6px" @click="cancelUpload(u)">✕</button>
            </div>
            <div class="up-bar-wrap"><div class="up-bar-fill" :class="u.status" :style="{ width: u.pct + '%' }"></div></div>
          </div>
        </div>

        <div class="card files-card">
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
                <img v-if="isImage(f.name)" :src="downloadUrl(f.name)" loading="lazy"
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
                   @keydown.enter="onChatEnter" />
            <button class="btn btn-primary send-btn" @click="sendChat">发送</button>
          </div>
        </div>
      </template>

      <!-- 服务中心: 控制卡通栏 + 日志卡吃满剩余高度; 扫码走顶栏「显示二维码」弹窗(信息更全), 页面不再放冗余扫码卡 -->
      <template v-if="isDesktop && view === 'service'">
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
        <!-- 服务日志: 吃满剩余高度 -->
        <div class="card svc-log-card">
          <div class="list-head">
            <div class="lh-left">
              <h3>服务日志</h3>
              <span class="badge">{{ logs.length }} 条</span>
            </div>
            <button class="btn btn-ghost" style="height:28px" title="清空服务日志显示（不影响服务运行）"
                    @click="clearLogs">清空日志</button>
          </div>
          <div class="svc-box" style="flex:1;display:flex;flex-direction:column;margin-bottom:0;min-height:0">
            <div class="logs" ref="logsBox" style="flex:1;height:auto">
              <div v-for="(e, i) in logs" :key="i" class="log-line">
                <span class="log-ts">{{ e.ts }}</span>
                <span class="log-sep">|</span>
                <span class="log-lvl" :class="'lvl-' + e.level">{{ lvlName(e.level) }}</span>
                <span class="log-sep">|</span>
                <span class="log-loc">{{ e.loc }}</span>
                <span class="log-sep">-</span>
                <span class="log-msg" v-html="logHtml(e.msg)"></span>
              </div>
              <div v-if="!logs.length" class="log-empty">HTTP 请求与服务事件…</div>
            </div>
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
        </div>
        <!-- Android/iOS 的 Tauri WebView 也走桌面壳(isDesktop), 但桌面插件未注册, 给出说明而非坏开关 -->
        <div class="card" v-else>
          <h3>通用</h3>
          <p class="about-text">当前平台暂无可配置项 —— 开机自动启动等桌面功能仅在 macOS / Windows 版提供。</p>
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

      <!-- 网页端(PyQt 风格): 左列上传+文件, 右列聊天室 -->
      <div v-if="!isDesktop" class="web-cols">
        <div class="web-lcol">
          <div class="card">
            <h3>上传文件</h3>
            <div class="dropzone" :class="{ 'drag-over': dragOver }" @click="pickFiles"
                 @dragover.prevent="dragOver = true" @dragleave.prevent="dragOver = false"
                 @dragend.prevent="dragOver = false" @drop.prevent="onDrop">
              <div class="ic">⬆️</div>
              <h5>{{ collecting ? "正在读取拖入的文件…" : "点击选择文件 或 拖拽到此处" }}</h5>
              <p>支持多选与<b>整个文件夹</b>（保持目录结构） · 可直接 Ctrl+V / Cmd+V 粘贴文件
                <span v-if="!isIOS"> · <a class="dir-pick" @click.stop="pickDir" title="选择文件夹上传">选择文件夹</a></span>
                · 大文件稳定传输，锁屏自动防中断</p>
              <input type="file" id="fileInput" multiple style="display:none" @change="onPick" />
              <input type="file" id="dirInput" webkitdirectory multiple style="display:none" @change="onPickDir" />
            </div>
            <div v-for="u in uploads" :key="u.id" class="upload-item">
              <div class="up-line">
                <span class="up-name">{{ u.name }}</span>
                <span class="up-status" :class="u.status">
                  {{ u.status === "uploading" ? u.pct + "% " + u.speed
                   : u.status === "waiting" ? "等待中"
                   : u.status === "done" ? (u.final ? "已存为 " + u.final : "完成") : u.speed || "失败" }}
                </span>
                <button v-if="u.status === 'waiting' || u.status === 'uploading'" class="icon-btn" title="取消上传"
                        style="margin-left:6px" @click="cancelUpload(u)">✕</button>
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
                  <img v-if="isImage(f.name)" :src="downloadUrl(f.name)" loading="lazy"
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

      <!-- 页脚: 版权(内容不满一屏时贴底)。桌面端仅 CC设置页保留, 其余页面去掉、
         底部统一收在主区内距上; 网页端不受影响, 全页显示 -->
      <footer v-if="!isDesktop || view === 'settings'" class="app-foot">{{ FOOTER_TEXT }}</footer>
    </main>
  </div>

  <!-- 悬浮球(手机端浏览器): 桌面壳窄窗不展示 —— 聊天走侧边栏「文本消息」, FAB 是手机端专属入口 -->
  <button v-if="!isDesktop && isNarrow && !chatOpen" class="chat-fab" title="聊天室" @click="openChat">
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
                   @keydown.enter="onChatEnter" />
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
          <p>电脑手机实时互通（WebSocket 推送秒级到达，断线自动降级轮询）。自定义昵称与 24 款 emoji 头像，支持代码块、粗体、链接的轻量 Markdown；链接点击直达系统浏览器，未读角标不漏消息。</p>
          <div class="tags"><span class="tag">实时互通</span><span class="tag">Markdown</span><span class="tag">未读提醒</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg></div>
          <h3>服务中心</h3>
          <p>启停服务、改端口（被占自动顺延）、换共享目录，磁盘余量一目了然。二维码常驻侧边栏（流光动效），任意页面扫码直达；服务日志 loguru 风格实时滚动，六套配色 × 明暗主题即点即换。</p>
          <div class="tags"><span class="tag">服务控制</span><span class="tag">二维码直达</span><span class="tag">多主题配色</span></div>
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
          <p>后端流式读写，超大文件轻松传；浏览器端下载直接交给浏览器原生流式下载，边下边存、实时进度，稳定省资源。</p>
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

  <!-- 配对弹框(网页端): 电脑端开启访问令牌保护后, 401 引导到此输入一次 -->
  <div v-if="needPin" class="dialog-overlay">
    <div class="dialog-box pin-box">
      <h4>需要配对</h4>
      <p class="pin-sub">输入电脑「访问令牌」完成配对</p>
      <input class="svc-input pin-input" v-model="pinInput" maxlength="12" placeholder="配对码"
             autocomplete="off" inputmode="numeric"
             @keydown.enter.prevent="submitPin" />
      <button class="btn btn-primary pin-btn" @click="submitPin">配 对</button>
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
