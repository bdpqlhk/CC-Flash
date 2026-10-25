import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: { port: 5173 },
  build: {
    target: "es2021",
    // ≤200KB 的图片直接内联为 base64: 失联提示图必须在断网前就在页面内存里, 不能靠断连后的请求
    assetsInlineLimit: 200 * 1024,
  },
});
