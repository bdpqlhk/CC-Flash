# CC-Flash · 局域网高速文件互传

[English](./README.en.md) | **简体中文**

电脑开机当服务器，手机扫码就能传。同一 Wi-Fi 下的手机、平板、电脑用浏览器互相传文件、发消息——**零登录、零公网、开箱即用**，速度就是路由器的速度。

<!-- 🎬 演示 GIF（阶段一素材）: 3 秒循环——启动服务 → 手机扫码 → 文件传完, 存为 assets/demo.gif 后取消下面注释
[![CC-Flash 演示](assets/demo.gif)](assets/demo.gif)
-->

技术栈：**Tauri 2 + Rust(axum) + Vue3 + Vite**。桌面端安装包 dmg 5.9 MB / .app 13.5 MB。

## 功能特性

- 📁 **文件互传**：拖拽 / 点击 / 粘贴上传（进度条+实时速率），支持**整个文件夹**（保持目录结构）与**大文件断点续传**（断网/锁屏/重启后自动续，断点保留 7 天），流式下载，中文文件名完美支持
- 💬 **聊天室**：同网段实时消息互通（WebSocket 即时推送），昵称头像、链接可点击、轻量 Markdown
- 📱 **三端形态**：桌面窗口（托盘常驻）/ 电脑浏览器双栏 / 手机浏览器单列+悬浮球，同一份页面自适应
- 🗂 **原生目录选择**：共享目录点「选择」弹出系统目录框，即选即存
- 🎨 **5 套深色皮肤**：深空蓝 / 石墨灰 / 极光蓝 / 护眼暖黑 / 暗夜紫，即点即换全端同步
- 🛡 **安全防护**：路径穿越双重防护、浏览器跨源拦截（DNS 重绑定/CSRF）、聊天消息转义防 XSS、上传磁盘预检
- 🔄 **自动更新检查**：对接 GitHub Releases，发现新版本弹框提示、一键前往下载
- 📘 **软件介绍页**：侧边栏「介绍」入口，全屏介绍（功能 / 三步上手 / 技术实现 / FAQ），锚点导航平滑滚动
- 🖥 **托盘唤窗 & 单实例**：关窗收托盘服务不停；左键托盘图标 / 点程序坞(macOS) / 重复启动均唤起同一主窗口，不再堆积托盘图标
- ⏱ **开机自启（手动开启）**：CC设置里打开开关才注册登录项，登录后后台启动不弹窗；默认关闭
- ⚙️ **配置集中管理**：前后端各一个配置中心，运行参数由服务端统一下发，改一处全端生效
- 🚀 **CI 自动发版**：推送 `v*` 标签自动构建 macOS / Windows / Android（按 4 个 ABI 分包）安装包并挂到 Release

## 快速开始

### 用户（下载安装包）

前往 [Releases 页面](https://github.com/bdpqlhk/CC-Flash/releases) 下载对应平台安装包：

- macOS：`.dmg`，双击拖入「应用程序」
- Windows：`.msi` / `.exe` 安装包或绿色单文件
- Android：按机型选 `.apk` —— `arm64-v8a`（绝大多数手机）/ `armeabi-v7a` / `x86` / `x86_64`

打开软件服务即自动启动（默认端口 1688，被占用自动顺延），手机扫码或输入窗口里的地址即可使用。

### 开发者（源码构建）

```bash
# 前置：Rust 1.77+ / Node.js 18+ / macOS 需 Xcode CLT；Windows 需 MSVC Build Tools + WebView2
npm install
npm run tauri dev      # 开发运行：前端热更 + Rust 增量编译
npm run check          # cargo check 快速校验 Rust 编译
npm run tauri build    # 发布构建：自动产出 .app / .dmg / .msi
```

## 目录结构

```
├── index.html / vite.config.js     # Vue3 + Vite 前端
├── src/
│   ├── App.vue                     # 全部界面与交互逻辑
│   ├── config.js                   # 前端配置中心（轮询/阈值/API 路径/文案…）
│   ├── update.js                   # 版本更新检查（GitHub Releases + 语义化比较）
│   ├── style.css                   # 主题变量 + 全部样式（含 5 套皮肤）
│   ├── main.js
│   └── assets/                     # 内嵌静态资源（如失联提示图）
├── docs/                           # 需求设计 / 技术实现 / 使用说明 / 历史改动记录
├── scripts/                        # release.sh 一键发版 + 多平台打包脚本
├── .github/workflows/              # 推 v* 标签自动三平台构建发版
└── src-tauri/
    ├── Cargo.toml                  # 版本号唯一来源
    ├── tauri.conf.json             # 窗口默认隐藏（防自启闪窗，手动启动时 setup 内 show）
    ├── capabilities/desktop.json   # 桌面端插件权限（开机自启）
    ├── gen/android/                # Android 工程（入库；release 签名读 CI 环境变量）
    └── src/
        ├── main.rs
        ├── lib.rs                  # Tauri 壳：托盘 + 关窗收纳 + 服务自启
        ├── config.rs               # Rust 配置中心（端口/限制/白名单…，随接口下发前端）
        └── server.rs               # axum 局域网服务：文件/聊天/日志/控制 API
```

## 配置说明

- **服务端**（`src-tauri/src/config.rs`）：端口与顺延范围、聊天保留条数、昵称/消息长度上限、日志容量、共享目录等，是运行参数的唯一事实源；
- **下发机制**：`GET /api/info` 与桌面端状态接口均携带 `version` 与 `config` 字段，网页端启动后自动跟随服务端参数——改 Rust 配置并重启服务即可，无需重新构建前端；
- **前端**（`src/config.js`）：仅承载纯界面行为（轮询兜底值、分页大小、话术文案、存储键等），与服务端约定项以 `[同步: …]` 注释互相指向；
- **版本号**：只在 `src-tauri/Cargo.toml` 修改一处，桌面端展示、更新检查、接口下发全部自动同源。

## 发版（维护者）

```bash
./scripts/release.sh 1.2.0 "feat: 更新说明"
```

自动完成：升版本（Cargo.toml 唯一来源）→ 提交 → 打标签 → 推送 → CI 三平台构建并挂到 Release。

Android 产物说明：

- 配置 `ANDROID_KEYSTORE_BASE64 / ANDROID_KEYSTORE_PASSWORD / ANDROID_KEY_ALIAS / ANDROID_KEY_PASSWORD` 四个 Secrets 后，走 **release 签名**（证书固定，版本间可覆盖安装），`--split-per-abi` 按 4 个 ABI 分包；
- 未配置 Secrets（四个任一缺失即视为未配置）时退回 **debug 签名**构建，产物文件名带 **`-debug` 后缀**以便区分（可直接安装，但各版本证书不同，覆盖安装需先卸载，CI 会出警告）；
- Release 采用「**全部构建成功才发布**」策略：desktop 与 APK 先上传到草稿，两者全绿后自动 Publish；任一失败则保持草稿（修复后重跑 publish job 或手动 Publish）。

macOS 产物为 **universal 包**（arm64 + x86_64 单包，Apple Silicon 与 Intel 通用）。

> Android 端共享目录默认位于应用专属存储（无需存储权限）；退后台被系统回收后重新打开应用，未完成的传输可凭断点继续（保留 7 天）。

> **iOS 说明**：CI 暂无 iOS 自动发布（需 Apple 开发者账号与签名证书，workflow 内留有注释模板）；本地可用 `./scripts/build-all.sh ios` 构建，需自行在 Xcode 中配置签名。

## 安全须知

本工具面向**可信局域网**（家庭、可信办公网）：同一网络的任何人都能查看/下载/删除共享文件、发言，接口无密码。请勿在公共 Wi-Fi 使用，勿将服务地址暴露给外网。

## 文档

<!-- 🖼 截图（阶段一素材）: 存入 assets/ 后在此引用, 如 assets/screenshot-desktop.png(桌面文件页) /
     assets/screenshot-mobile.png(手机页) / assets/screenshot-tour.png(应用内介绍页) -->

- [需求设计文档](docs/需求设计文档.html)
- [技术实现文档](docs/技术实现文档.html)
- [保姆级使用说明](docs/保姆级使用说明.html)
- [整个过程历史改动记录](docs/整个过程历史改动记录_2026-10-23至2026-10-27.html)
- [推广营销方案](docs/推广营销方案.html)
- [后期规划](docs/后期规划.html)

---

© 2026-现在 by hangkun.li
