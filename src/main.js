import { createApp } from "vue";
import App from "./App.vue";
import "./style.css";
import { LS } from "./config";

// 挂载前先恢复上次选择的主题与明暗模式, 避免闪默认色
try {
  const t = localStorage.getItem(LS.theme);
  if (t && t !== "default") document.documentElement.dataset.theme = t;
  const m = localStorage.getItem(LS.mode) || "dark";
  document.documentElement.dataset.mode =
    m === "system" ? (window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light") : m;
} catch (e) {}

createApp(App).mount("#app");
