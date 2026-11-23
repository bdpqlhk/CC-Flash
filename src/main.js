import { createApp } from "vue";
import App from "./App.vue";
import "./style.css";
import { LS, THEME_MIGRATE } from "./config";

// Android Tauri 壳已在 MainActivity 里把系统栏/刘海 inset 加到内容根节点；
// 再叠加 CSS 的 safe-area + 18px 页头内边距会形成明显空白。这里标记平台，
// 只让 Android 壳使用更紧凑的顶部间距；iOS/浏览器仍走 safe-area。
if (("__TAURI_INTERNALS__" in window) && /Android/i.test(navigator.userAgent)) {
  document.documentElement.dataset.androidApp = "1";
}

// 普通 Android 浏览器（vivo/Chrome/微信等）自己已经处理状态栏和地址栏；
// 若继续叠加 env(safe-area-inset-top)，vivo 等内核会多留一整条空白。
// 仅非 Tauri、非 standalone 的 Android 浏览器标记，页面头部不再吃 safe-area-top。
if (!("__TAURI_INTERNALS__" in window) && /Android/i.test(navigator.userAgent)) {
  try {
    if (!window.matchMedia("(display-mode: standalone)").matches) {
      document.documentElement.dataset.androidBrowser = "1";
    }
  } catch (e) {}
}

// 挂载前先恢复上次选择的主题与明暗模式, 避免闪默认色
try {
  let t = localStorage.getItem(LS.theme) || "signal";
  // 主题精简迁移(v1.1.6): 9 套收敛为 6 套, 被移除的选择映射到色相最近的新皮肤
  if (t && THEME_MIGRATE[t]) {
    t = THEME_MIGRATE[t];
    localStorage.setItem(LS.theme, t);
  }
  // 与 App.vue 同步的一次性迁移：整体视觉改造后首次进入信号台。
  if (localStorage.getItem("wft-signal-theme-v1") !== "1") {
    t = "signal";
    localStorage.setItem("wft-signal-theme-v1", "1");
    localStorage.setItem(LS.theme, t);
  }
  if (t && t !== "default") document.documentElement.dataset.theme = t;
  const m = localStorage.getItem(LS.mode) || "dark";
  const dark = m === "system"
    ? window.matchMedia("(prefers-color-scheme: dark)").matches
    : m === "dark";
  document.documentElement.dataset.mode = dark ? "dark" : "light";
  document.documentElement.style.colorScheme = dark ? "dark" : "light";
} catch (e) {}

createApp(App).mount("#app");

// PWA 仅浏览器端注册: 桌面/移动壳已有原生 App, 不需要 Service Worker。
// http://局域网IP 不是安全上下文, 浏览器会直接拒绝注册 —— 这是浏览器策略;
// https / localhost 下才注册, 并在新版本就绪时通知 App 弹「刷新」提示。
if (!("__TAURI_INTERNALS__" in window) && "serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    if (!window.isSecureContext) return;
    navigator.serviceWorker.register("/sw.js").then((reg) => {
      const notify = (worker) => {
        if (!worker) return;
        worker.addEventListener("statechange", () => {
          if (worker.state === "installed" && navigator.serviceWorker.controller) {
            window.dispatchEvent(new CustomEvent("ccflash:sw-update"));
          }
        });
      };
      notify(reg.installing);
      reg.addEventListener("updatefound", () => notify(reg.installing));
      // 每次打开静默检查一次更新(浏览器另有 24h 限频)
      reg.update().catch(() => {});
    }).catch(() => {});
  });
}
