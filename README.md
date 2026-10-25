# CC-Flash · 局域网高速文件互传

电脑开机当服务器，手机扫码就能传。同一 Wi-Fi 下的手机、平板、电脑用浏览器互相传文件、发消息——**零登录、零公网、开箱即用**，速度就是路由器的速度。

技术栈：**Tauri 2 + Rust(axum) + Vue3 + Vite**。桌面端安装包 dmg 5.9 MB / .app 13.5 MB。

## 功能特性

- 📁 **文件互传**：拖拽 / 点击 / 粘贴上传（进度条+实时速率），流式下载，中文文件名完美支持
- 💬 **聊天室**：同网段实时消息互通，昵称头像、链接可点击、轻量 Markdown
- 📱 **三端形态**：桌面窗口（托盘常驻）/ 电脑浏览器双栏 / 手机浏览器单列+悬浮球，同一份页面自适应
- 🗂 **原生目录选择**：共享目录点「选择」弹出系统目录框，即选即存
- 🎨 **5 套深色皮肤**：深空蓝 / 石墨灰 / 极光蓝 / 护眼暖黑 / 暗夜紫，即点即换全端同步
- 🛡 **安全防护**：路径穿越双重防护、聊天消息转义防 XSS、上传磁盘预检+半截文件自清理
- 🔄 **自动更新检查**：对接 GitHub Releases，发现新版本弹框提示、一键前往下载
- ⚙️ **配置集中管理**：前后端各一个配置中心，运行参数由服务端统一下发，改一处全端生效
- 🚀 **CI 自动发版**：推送 `v*` 标签自动构建 macOS / Windows / Android 安装包并挂到 Release

## 快速开始

### 用户（下载安装包）

前往 [Releases 页面](https://github.com/bdpqlhk/CC-Flash/releases) 下载对应平台安装包：

- macOS：`.dmg`，双击拖入「应用程序」
- Windows：`.msi` / `.exe` 安装包或绿色单文件

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
    ├── tauri.conf.json
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

自动完成：升版本（Cargo.toml 唯一来源）→ 提交 → 打标签 → 推送 → CI 三平台构建并挂到 Release（Android 产物为 debug 签名 APK，可直接安装）。

## 安全须知

本工具面向**可信局域网**（家庭、可信办公网）：同一网络的任何人都能查看/下载/删除共享文件、发言，接口无密码。请勿在公共 Wi-Fi 使用，勿将服务地址暴露给外网。

## 文档

- [需求设计文档](docs/需求设计文档.html)
- [技术实现文档](docs/技术实现文档.html)
- [保姆级使用说明](docs/保姆级使用说明.html)
- [整个过程历史改动记录](docs/整个过程历史改动记录_2026-10-23至2026-10-25.html)

---

© 2026-现在 by hangkun.li
