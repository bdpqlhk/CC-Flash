import { createApp } from "vue";
import App from "./App.vue";
import "./style.css";

// 挂载前先恢复上次选择的主题, 避免闪默认色
try {
  const t = localStorage.getItem("wft-theme");
  if (t && t !== "default") document.documentElement.dataset.theme = t;
} catch (e) {}

createApp(App).mount("#app");
