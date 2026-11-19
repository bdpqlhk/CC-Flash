import { createApp } from "vue";
import App from "./App.vue";
import "./style.css";
import { LS, THEME_MIGRATE } from "./config";

// 挂载前先恢复上次选择的主题与明暗模式, 避免闪默认色
try {
  let t = localStorage.getItem(LS.theme) || "default";
  // 主题精简迁移(v1.1.6): 9 套收敛为 6 套, 被移除的选择映射到色相最近的新皮肤
  if (t && THEME_MIGRATE[t]) {
    t = THEME_MIGRATE[t];
    localStorage.setItem(LS.theme, t);
  }
  if (t && t !== "default") document.documentElement.dataset.theme = t;
  const m = localStorage.getItem(LS.mode) || "light";
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
