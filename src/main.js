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
  document.documentElement.dataset.mode =
    m === "system" ? (window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light") : m;
} catch (e) {}

createApp(App).mount("#app");

// PWA 仅浏览器端注册: 桌面/移动壳已有原生 App, 不需要 Service Worker
if (!("__TAURI_INTERNALS__" in window) && "serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js").catch(() => {});
  });
}
