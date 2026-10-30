import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 5173,
    // dev/preview 下前端(5173/4173)与 API(桌面壳的 1688)不同源, 代理对齐线上同源形态:
    // 网页端 apiBase 为空串, 请求按相对路径打到同源 —— 不代理则 dev 调试下载/上传全 404
    proxy: {
      "/api": "http://127.0.0.1:1688",
      "/ws": { target: "ws://127.0.0.1:1688", ws: true },
    },
  },
  preview: {
    port: 4173,
    proxy: {
      "/api": "http://127.0.0.1:1688",
      "/ws": { target: "ws://127.0.0.1:1688", ws: true },
    },
  },
  build: {
    target: "es2021",
    // ≤200KB 的图片直接内联为 base64: 失联提示图必须在断网前就在页面内存里, 不能靠断连后的请求
    assetsInlineLimit: 200 * 1024,
  },
});
