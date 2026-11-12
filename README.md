# CC-Flash · 局域网高速文件互传

[English](./README.en.md) | **简体中文**

电脑开机当服务器，手机扫码就能传。同一 Wi-Fi 下的手机、平板、电脑用浏览器互相传文件、发消息——**零登录、零公网、开箱即用**，速度就是路由器的速度。

<!-- 🎬 演示 GIF（阶段一素材）: 3 秒循环——启动服务 → 手机扫码 → 文件传完, 存为 assets/demo.gif 后取消下面注释
[![CC-Flash 演示](assets/demo.gif)](assets/demo.gif)
-->

技术栈：**Tauri 2 + Rust(axum) + Vue3 + Vite**（当前应用版本 **1.2.1**）。桌面端安装包 macOS 分架构 dmg ≈4.1~4.3 MB / .app ≈4.7 MB，Windows exe ≈3.0 MB，Android APK(arm64) ≈7.0 MB。

## 功能特性

- 💬 **对话式传输（唯一入口「传输」）**：文件和消息放进同一条时间线——上传即出现**文件卡片**（等待 / 上传中 / 完成 / 失败、实时进度与速率、取消 / 重试），批量上传自动合并为**文件组**；文字仍是左右气泡，支持昵称、头像、**Markdown**（标题 / 列表 / 引用 / 分割线 / 代码块 / 行内代码 / 粗体 / 斜体 / 删除线 / 链接）与**多行输入**（桌面 Enter 发送、Shift+Enter 换行；手机 Enter 换行后点发送）、未读。完整共享文件列表收进**右侧抽屉（340~400px，字号加大、分页占满底栏）/ 手机底部面板**，排序、分页、全选、批量删除、打开 / 复制 / 下载 / 分享全部保留，缩略图与「预览」按钮可直接预览
- 📁 **文件能力**：拖拽 / 点击 / `Ctrl+V` / 整个文件夹上传（保持目录结构），大文件**单请求整文件直传**（流式落盘、零内存占用；失败 / 超时 / 停服即时回滚半截文件），上传期间**锁屏自动保活**（客户端 3 并发队列 + 无进展看门狗；服务端上传 / 下载 / WebSocket 并发闸 16 / 32 / 64），同名文件**永不覆盖**（自动 `报告(1).zip` 顺延），流式下载支持 Range 断点续传，中文文件名完美支持
- 🔍 **应用内文件预览**：图片 / PDF / 文本（txt/md/log/csv）/ 音视频点击缩略图或「预览」即全屏查看，右上角「关闭」或 Esc 直接回到聊天；Office 文档、压缩包等浏览器无法渲染的类型仍需下载；html/svg/xml 不会以网页形式内联打开
- 💠 **桌面悬浮码**：置顶透明呼吸圆点吸附屏幕边缘（默认左侧中部，**位置持久化重启不丢**），珊瑚呼吸环 + 取景框 CC 徽章；指针停留 1.5 秒滑出「浏览器扫一扫」预览卡、移开即收；**外圈呼吸 = 服务运行中、静止 = 已停止**；外圈绿点（与呼吸环同组随环缩放）提示新文件 / 新消息（主窗打开自动隐藏并清零）；按住拖动自动吸附最近边，**支持扩展屏**（拖到哪块屏吸哪块屏，位置连同显示器一起记忆、重启回原屏；Windows 混合 DPI 下也按目标屏缩放正确落位），关闭入口仅在 CC设置
- 🖥 **服务中心**：启停/端口/磁盘空间一目了然，**loguru 风格结构化日志**（INFO·SUCCESS·WARNING·ERROR 级别着色 / 来源 文件:行号 / 贴底跟随），问题一眼定位
- 📱 **四种运行形态**：桌面壳（侧栏 + 共享文件抽屉 + 托盘/悬浮码；顶栏标题居左，运行状态 + 地址 + 复制 / 二维码 / 浏览器按钮整组靠右上，地址宽度随内容、窄屏自动换行）/ 电脑浏览器（顶部栏 + 抽屉）/ 手机浏览器（单列 + 底部文件面板）/ 手机端 App（纯客户端 + 首次连接电脑），共用同一条时间线与同一套组件；桌面端支持 macOS / Windows / Linux（Linux 从源码构建），手机端提供 Android APK，iOS 不提供安装包、用 Safari 打开电脑地址；微信内置浏览器会提示点右上角「在浏览器打开」；Android/iOS 壳命令层拒绝在手机上托管服务，失联页提供「重新设置电脑地址」出口
- 🗂 **原生目录选择**：共享目录点「选择」弹出系统目录框，即选即存
- 🎨 **主题系统**：亮色 / 暗色 / 跟随系统三种模式（随系统外观自动切换）× **5 套配色**（**深空蓝** / 石墨玫瑰 / 森林薄荷 / 暗夜紫 / 墨色琥珀），默认深空蓝浅色，即点即换、即时生效，桌面对话气泡跟随主题
- 🛡 **安全防护**：路径穿越双重防护、浏览器跨源拦截（DNS 重绑定/CSRF，Host 仅认 IP/localhost）、聊天消息转义防 XSS、上传磁盘预检 + 256MB 安全余量、可选**访问令牌**（6 位配对码 + 按来源 IP 指数退避防爆破，输一次全端透明，本机豁免；配对码走请求头，轮换后旧码自动清除、不再自锁）、分享链接**一次性票据**（12 位十六进制 token、仅授权下载该文件、10 分钟有效、下载即焚，对方免输配对码；无效票据按 IP 限流防穷举）、上传 / 下载 / WebSocket 并发闸（16 / 32 / 64）、CSP 放行局域网媒体与 iframe（不放开脚本与对象）、删除不跟随符号链接
- 🔄 **自动更新检查**：读取 GitHub Releases 列表（兼容未签名时发布的 prerelease，不只依赖 `/latest`），发现新版本弹框提示、一键前往下载
- 📘 **软件介绍页**：侧边栏「介绍」入口，全屏介绍（功能 / 三步上手 / 技术实现 / FAQ），锚点导航平滑滚动
- 🖥 **托盘唤窗 & 单实例**：关窗收托盘服务不停；左键托盘图标 / 点程序坞(macOS) / 重复启动均唤起同一主窗口，不再堆积托盘图标
- ⏱ **开机自启（手动开启）**：CC设置里打开开关才注册登录项，登录后后台启动不弹窗；默认关闭
- ⚙️ **配置集中管理**：前后端各一个配置中心，运行参数由服务端统一下发，改一处全端生效
- 🚀 **CI 自动发版**：推送 `v*` 标签（支持四段 `vX.Y.Z.W`，第四段为构建号，CI 校验前三位与应用版本一致）自动构建 macOS / Windows / Android（按 4 个 ABI 分包）安装包并挂到 Release

## 快速开始

### 用户（下载安装包）

前往 [Releases 页面](https://github.com/bdpqlhk/CC-Flash/releases) 下载对应平台安装包：

- macOS：按芯片选 `.dmg` —— Apple Silicon 用 `_aarch64.dmg`、Intel 用 `_x64.dmg`，双击拖入「应用程序」
- Windows：`.msi` / `.exe` 安装包或绿色单文件
- Android：按机型选 `.apk` —— `arm64-v8a`（绝大多数手机）/ `armeabi-v7a` / `x86` / `x86_64`
- Linux：从源码构建（`npm run tauri build`），CI 暂未发布 Linux 安装包

打开软件服务即自动启动（默认端口 1688，被占用自动顺延），手机扫码或输入窗口里的地址即可使用。界面只有一个「传输」入口：上传的文件会以卡片出现在对话里，完整共享文件在右侧抽屉（340~400px，手机为底部面板），缩略图与「预览」按钮可直接预览。

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
│   ├── update.js                   # 版本更新检查（GitHub Releases 列表 + 语义化比较，兼容 prerelease）
│   ├── style.css                   # 主题变量 + 全部样式（5 套配色 × 明暗模式）
│   ├── pet-activity.js             # 悬浮码活动状态
│   ├── main.js
│   └── assets/                     # 内嵌静态资源（如失联提示图）
├── docs/                           # 需求设计 / 技术实现 / 使用说明 / 历史改动记录
├── scripts/                        # release.sh 一键发版 + 多平台打包脚本
├── .github/workflows/              # 推 v* 标签自动三平台构建发版
└── src-tauri/
    ├── Cargo.toml                  # 版本号唯一来源
    ├── tauri.conf.json             # 桌面窗口默认隐藏（防自启闪窗，就绪后 show）；移动壳初始可见
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
- **版本号**：手动只需改 `src-tauri/Cargo.toml`；`release.sh` 发版时会自动同步 `src/config.js` 兜底版本、`package.json` 与 Android `versionCode`，桌面端展示、更新检查、接口下发全部同源。

## 发版（维护者）

```bash
./scripts/release.sh 1.2.1.1 "feat: 更新说明"
```

版本号支持 `X.Y.Z` 与四段 `X.Y.Z.W`：**git tag 使用完整四段（如 `v1.2.1.1`）**，第四段是同一应用版本内的构建号；应用版本（Cargo.toml / 安装包）取前三段（`1.2.1`），CI 会校验四段 tag 的前三位与 `Cargo.toml` 一致。`release.sh` 同时按 `major×10^7 + minor×10^5 + patch×10^2 + build` 计算 Android `versionCode`（上限 2.1e9），保证同版本迭代可覆盖安装。

自动完成：升版本（Cargo.toml 唯一来源）→ 同步兜底版本 / `package.json` / Android `versionCode` → 提交 → 打标签 → 推送 → CI 三平台构建并挂到 Release。

Android 产物说明：

- 必须配置 `ANDROID_KEYSTORE_BASE64 / ANDROID_KEYSTORE_PASSWORD / ANDROID_KEY_ALIAS / ANDROID_KEY_PASSWORD` 四个 Secrets，走 **release 签名**（证书固定，版本间可覆盖安装），`--split-per-abi` 按 4 个 ABI 分包；**四个任一缺失 CI 直接失败** —— 正式 Release 绝不允许 debug 包混入（debuggable、每次签名不同，用户无法覆盖升级）；
- Release 采用「**全部构建成功才发布**」策略：desktop 与 APK 先上传到草稿，两者全绿后自动 Publish；任一失败则保持草稿（修复后重跑 publish job 或手动 Publish）；Android 上传前会校验 Release 仍为草稿，避免重跑已发布 tag 时把新 APK 写进线上正式版；
- R8 已配置 keep 规则与 `SourceFile,LineNumberTable`，崩溃栈保留可读行号；Gradle 的 NDK 版本跟随 `NDK_HOME`，与 Rust 侧编译保持一致。
- **可选代码签名**：workflow 已预留 `APPLE_CERTIFICATE / APPLE_SIGNING_IDENTITY / APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID` 与 `WINDOWS_CERTIFICATE / WINDOWS_CERTIFICATE_PASSWORD`；配置后自动签名/公证。未配置时桌面与 Android 仍会在全部构建成功后公开，但 Release 标记为 **prerelease（预发布）** 而非正式版；更新检查读取 releases 列表，仍能收到这类版本，macOS Gatekeeper / Windows SmartScreen 需用户手动放行。
- 本地 `./scripts/build-all.sh android` 的 release 构建会校验 `CC_KEYSTORE_*`；缺证书直接失败，不再把 unsigned APK 当成功产物。

macOS 按架构出**双包**：`CC-Flash_x.y.z_aarch64.dmg`（Apple Silicon）与 `CC-Flash_x.y.z_x64.dmg`（Intel）——单包体积约减半（命名沿用 v1.1.2 先例）。

> Android 端共享目录默认位于应用专属存储（无需存储权限）。当前上传为**单请求整文件直传**：中断后需重新选择文件上传（半截文件即时清理，不在目录留残缺文件）；7 天清理逻辑仅用于回收 v1.1.5-1.1.7 旧版遗留的 `.ccpart` 断点。下载侧 Range 断点续传不受影响。

> **iOS 说明**：当前不提供 iOS 安装包（需 Apple 开发者账号与签名证书；`gen/apple` 工程与本地网络权限声明尚未生成，open_url 等壳能力也未适配）。iOS 用户请直接用 Safari 打开电脑端地址使用网页版；`./scripts/build-all.sh ios` 仅作为未来适配的构建入口。

## 安全须知

本工具面向**可信局域网**（家庭、可信办公网）：默认同一网络的任何人都能查看/下载/删除共享文件、发言。半可信网络可在电脑端「CC设置」开启**访问令牌**——手机首次打开输入 6 位配对码即可（输一次全端透明；连续输错会按来源 IP 指数退避锁定，稍候再输；电脑本机不受影响）。开启后分享出去的链接自带**一次性授权**——只能下载那一个文件、10 分钟有效、下载即焚，对方无需输码；关闭令牌、轮换配对码或切换共享目录时旧票据同步作废，无效票据尝试会按来源 IP 限流。服务端对上传 / 下载 / WebSocket 分别设 16 / 32 / 64 并发上限，CSP 放行局域网媒体与 iframe（不放开脚本与对象），删除操作不跟随符号链接。电脑地址请使用 IP（服务端为防 DNS 重绑定不接受 `*.local` 域名）。**公网来源（含全局 IPv6）在未开启访问令牌时会被直接拒绝**，但请仍勿将服务地址暴露给外网；请勿在公共 Wi-Fi 使用。

⚠️ 服务监听所有网络接口：连着**公司 VPN** 时，VPN 网段内的其他主机也能访问你的共享目录——在不受信任的网络环境下请保持服务停止。地址/二维码会自动避开 VPN 虚拟网卡优先展示真实局域网 IP（多候选时在二维码弹窗中点选）。

## 文档

<!-- 🖼 截图（阶段一素材）: 存入 assets/ 后在此引用, 如 assets/screenshot-desktop.png(桌面传输页) /
     assets/screenshot-mobile.png(手机页) / assets/screenshot-tour.png(应用内介绍页) -->

- [需求设计文档](docs/需求设计文档.html)
- [技术实现文档](docs/技术实现文档.html)
- [保姆级使用说明](docs/保姆级使用说明.html)
- [整个过程历史改动记录 2026-10-28 ~ 11-02](docs/整个过程历史改动记录_2026-10-28至2026-11-02.html)（[10-23 ~ 10-27](docs/整个过程历史改动记录_2026-10-23至2026-10-27.html)）
- [推广营销方案](docs/推广营销方案.html)
- [后期规划](docs/后期规划.html)

---

Copyright © 2026-Present 李航坤. All Rights Reserved.
