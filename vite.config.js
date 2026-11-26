import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  // 全项目只用 <script setup> 组合式 API: 关掉 Options API 与 devtools 后
  // Vue 运行时可被 tree-shake 掉一整块(实测 bundle 再减十几 KB)。
  define: {
    __VUE_OPTIONS_API__: false,
    __VUE_PROD_DEVTOOLS__: false,
  },
  esbuild: {
    legalComments: "none",
    // 生产包去掉 console/debugger: 体积更小, 也避免把调试输出带到用户端
    drop: ["console", "debugger"],
  },
  server: {
    port: 5173,
    // Tauri devUrl 固定 5173: 端口被占时不得静默换端口(换了 dev 窗口会白屏且难定位)
    strictPort: true,
    // docs/*.md 是文档不是前端代码: 改文档不触发整页 reload —— 每次 reload 都会让
    // 桌面壳两个 WebView(主窗/悬浮球)白屏闪一下, 悬浮球窗尤其明显(透明窗瞬间变白)
    watch: {
      ignored: ["**/docs/**", "**/*.md"],
    },
    // dev/preview 下前端(5173/4173)与 API(桌面壳的 1688)不同源, 代理对齐线上同源形态:
    // 网页端 apiBase 为空串, 请求按相对路径打到同源 —— 不代理则 dev 调试下载/上传全 404
    proxy: {
      "/api": "http://127.0.0.1:1688",
      "/ws": { target: "ws://127.0.0.1:1688", ws: true },
    },
  },
  preview: {
    port: 4173,
    strictPort: true,
    proxy: {
      "/api": "http://127.0.0.1:1688",
      "/ws": { target: "ws://127.0.0.1:1688", ws: true },
    },
  },
  build: {
    target: "es2021",
    // ≤200KB 的图片直接内联为 base64: 失联提示图必须在断网前就在页面内存里, 不能靠断连后的请求
    assetsInlineLimit: 200 * 1024,
    // Terser 二次压缩比 esbuild 略彻底; passes 2 对 Vue 模板产物更省
    minify: "terser",
    terserOptions: {
      compress: { passes: 2, drop_console: true, drop_debugger: true },
      format: { comments: false },
    },
    // lightningcss 的 CSS 压缩比 esbuild 更彻底(不改变选择器语义)
    cssMinify: "lightningcss",
    sourcemap: false,
    reportCompressedSize: false,
  },
});
