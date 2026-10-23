# CC-Flash · Tauri 重构版(Rust + Vue3)

CC-Flash 的 Tauri 重构实验线:**Rust(axum) 重写文件服务核心 + Vue3 重写布局E 网页端**,
目标体积 ~5-12MB(PyQt6 版为 56MB)。协议与 Python 版完全一致,网页端 API 不变。

> 状态:**脚手架 + 首版实现,未经完整跨平台验证**。首次构建遇到问题优先对照本文「已知事项」。

## 前置要求

- Rust 1.77+(rustup)
- Node.js 18+
- macOS:Xcode CLT;Windows:WebView2(Win10/11 一般自带)+ MSVC Build Tools

## 开发运行

```bash
cd tauri
npm install
npm run tauri dev     # 前端热更 + Rust 增量编译
```

应用启动时会自动在 127.0.0.1:8765 起 LAN 服务(被占用自动顺延到 8785),
窗口内显示「控制台」(状态/地址/二维码/日志),手机浏览器访问同地址即为网页端。

## 打包

```bash
npm run tauri build
```

> **图标**:首次打包前需要图标文件。任选一张方形 PNG 执行
> `npm run tauri icon <图片路径>`(生成到 `src-tauri/icons/`),
> 再把 `src-tauri/tauri.conf.json` 里 `bundle.active` 改为 `true`。

## 目录

```
tauri/
├── package.json / vite.config.js / index.html   Vue3 + Vite 前端
├── src/
│   ├── App.vue        布局E 页面(控制台/上传/文件分页/三色按钮/聊天/悬浮球)
│   ├── style.css      布局E 样式
│   └── main.js
└── src-tauri/
    ├── Cargo.toml / tauri.conf.json
    └── src/
        ├── lib.rs      Tauri 壳:托盘 + 关窗收到托盘 + 服务自启
        ├── server.rs   axum 文件服务:files/download/upload/delete/chat/info/control
        └── main.rs
```

## 与 Python 版的协议对照

| API | 行为 | 一致性 |
|---|---|---|
| GET /api/files | 名称/大小/修改时间,倒序 | ✅ |
| GET /api/download/:name | 流式下载,RFC5987 中文名 | ✅ |
| POST /api/upload | 头 X-Filename(URL 编码)+ 原始体;磁盘空间预检;失败清理半截文件 | ✅ |
| GET/POST /api/chat | 昵称≤20 字,消息≤2000 字,保留 200 条 | ✅ |
| DELETE /api/delete/:name | 删除 | ✅ |
| GET /api/info · /api/control/* | Tauri 新增:桌面控制台用(无鉴权,与该版本线信任模型一致) | 🆕 |

## 已知事项

- `bundle.active` 默认关闭:未生成图标前打不了安装包,dev/`tauri build` 的绿色 exe 不受影响
- `src-tauri/src/server.rs` 引用 `../dist`(前端构建产物),`npm run tauri build` 会自动先构建前端;
  单独 `cargo build` 前需至少手动跑过一次 `npm run build`
- 服务日志为本地时间(chrono),请求行含方法/路径/状态/耗时/来源 IP/设备;聊天与控制台日志轮询不记录
- 控制台接口无鉴权,信任模型与 v2.0.x Python 版一致(可信局域网使用)
