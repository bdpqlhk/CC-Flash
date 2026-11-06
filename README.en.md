# CC-Flash · Blazing-Fast LAN File Transfer

**English** | [简体中文](./README.md)

Turn your computer into a server with one click — phones on the same Wi-Fi scan a QR code and start transferring. Share files and messages between phones, tablets, and PCs via the browser: **no login, no internet, no setup**. Speed is whatever your router can do.

<!-- 🎬 Demo GIF (阶段一素材): 3-second loop — start service → phone scans QR → file transferred. Save as assets/demo.gif and uncomment:
[![CC-Flash demo](assets/demo.gif)](assets/demo.gif)
-->

Built with **Tauri 2 + Rust (axum) + Vue 3 + Vite**. Desktop installer is just a ~6 MB dmg.

## Features

- 📁 **File transfer** — drag & drop / click / paste upload (live progress + speed), large files uploaded as a **single whole-file streaming request** (server-side streaming to disk, zero memory bloat), **screen keep-alive during upload** (prevents lock-screen interruptions, smart concurrency queue), **whole-folder upload** (structure preserved); **always-on QR code in the desktop sidebar** — scan from any page; same-name files are **never overwritten** — auto-renamed to `file(1).zip`, `(2)`, `(3)`…; streaming download with Range resume support, full Unicode filename support
- 💠 **Floating scan orb** — an always-on-top transparent breathing dot docked half-hidden at the screen edge (default: left middle, **position persisted across restarts**), coral breathing ring + viewfinder CC badge; dwell the pointer on it for 1.5s to preview the "Scan with browser" card, leave to collapse; **a breathing ring means the service is running, a still ring means stopped**; a green dot (grouped with the ring, scaling together) flags new files / messages (hidden & cleared while the main window is open); drag it and it snaps to the nearest edge — the only off switch lives in Settings
- 🖥 **Service center** — start/stop, port, disk space at a glance; **loguru-style structured logs** (color-coded levels / source file:line / auto-follow), issues spotted at a glance
- 💬 **Chat** — real-time LAN messaging (WebSocket push) with nicknames, emoji avatars, clickable links, and lightweight Markdown
- 📱 **Three form factors** — desktop window (tray-resident) / two-pane desktop browser / single-column mobile browser with floating chat ball; one codebase adapts everywhere
- 🗂 **Native folder picker** — click "Choose" next to the share directory and pick with the OS dialog
- 🎨 **Theme system** — Light / Dark / Follow-system modes (auto-switches with OS appearance) × **6 color schemes** (Deep Space Blue, Graphite, Forest Mint, Dark Purple, Ink Amber, Graphite Rose); switch instantly, synced everywhere
- 🛡 **Security hardening** — double path-traversal protection, browser cross-origin guard (DNS rebinding / CSRF), XSS escaping in chat, disk-space pre-check, optional **access token** (6-digit pairing code with brute-force backoff; enter once per device; the desktop itself is exempt)
- 🔄 **Update checker** — watches GitHub Releases, prompts with release notes, one click to download
- 📘 **In-app tour** — sidebar "ⓘ Intro" opens a full-screen walkthrough (features / 3-step guide / tech / FAQ)
- 🖥 **Tray & single-instance** — closing the window keeps the service running; dock icon, tray click, or relaunching all re-focus the same window
- ⏱ **Optional autostart** — off by default; enable in Settings to start silently in the background at login
- ⚙️ **Centralized config** — backend is the single source of truth; runtime params are pushed to all clients
- 🚀 **CI releases** — push a `v*` tag; macOS / Windows / Android (4 ABI splits) builds are published automatically

## Quick Start

### Users

Grab your installer from the [Releases page](https://github.com/bdpqlhk/CC-Flash/releases):

- **macOS**: `.dmg` — drag into Applications
- **Windows**: `.msi` / `.exe`
- **Android**: pick the `.apk` for your CPU — `arm64-v8a` (almost all phones) / `armeabi-v7a` / `x86` / `x86_64`

Launch the app — the service starts automatically (default port `1688`, auto-increments if taken). Scan the QR code or type the address on your phone. Done.

### Developers

```bash
# Prerequisites: Rust 1.77+, Node.js 18+; macOS needs Xcode CLT, Windows needs MSVC Build Tools + WebView2
npm install
npm run tauri dev      # dev mode: frontend HMR + incremental Rust builds
npm run check          # fast Rust sanity check
npm run tauri build    # release build: produces .app / .dmg / .msi
```

## Project Layout

```
├── index.html / vite.config.js     # Vue 3 + Vite frontend
├── src/
│   ├── App.vue                     # all UI + interaction logic
│   ├── config.js                   # frontend config center (polling, thresholds, copy…)
│   ├── update.js                   # release checker (GitHub Releases + semver compare)
│   ├── style.css                   # theme tokens + all styles (6 color schemes × light/dark)
│   ├── main.js
│   └── assets/                     # embedded static assets
├── docs/                           # design / tech / user guides (Chinese)
├── scripts/                        # release.sh one-shot publishing
├── .github/workflows/              # tag-triggered tri-platform CI
└── src-tauri/
    ├── Cargo.toml                  # single source of truth for the version
    ├── tauri.conf.json             # window hidden by default (no autostart flash)
    ├── capabilities/desktop.json   # desktop-only plugin permissions (autostart)
    ├── gen/android/                # Android project (checked in; release signing via CI env)
    └── src/
        ├── main.rs
        ├── lib.rs                  # Tauri shell: tray / close-to-tray / autostart
        ├── config.rs               # Rust config center (pushed to frontend via API)
        └── server.rs               # axum LAN service: files / chat / logs / control
```

## Configuration

- **Backend** (`src-tauri/src/config.rs`) — port + fallback range, chat retention, length limits, log capacity, share directory: the single source of truth;
- **Push mechanism** — `GET /api/info` and the desktop status command both carry `version` and `config`; web clients follow automatically — tweak Rust config, restart the service, no frontend rebuild;
- **Frontend** (`src/config.js`) — pure UI behavior only (polling fallbacks, page size, copy templates, storage keys);
- **Version** — edit only `src-tauri/Cargo.toml`; desktop display, update check, and API all read the same value.

## Releasing (maintainers)

```bash
./scripts/release.sh 1.2.0 "feat: release notes"
```

Bumps the version, commits, tags, pushes — CI builds all three platforms and attaches artifacts to the Release.

Android signing: configure the `ANDROID_KEYSTORE_*` secrets for **release signing** (stable cert → in-place upgrades, `--split-per-abi` per-CPU APKs). If any secret is missing, CI falls back to debug signing — those APKs are named with a **`-debug` suffix** for clarity. Releases use an **all-green publish policy**: artifacts upload as a draft first, and the Release goes live only after both desktop and Android jobs succeed.

macOS builds are **universal** (arm64 + x86_64 in one package). **iOS**: no automated CI release yet (requires an Apple Developer account; a commented template exists in the workflow) — build locally with `./scripts/build-all.sh ios` and configure signing in Xcode.

## Security Notice

This tool is built for **trusted LANs** (home / trusted office): by default anyone on the same network can view, download, delete shared files, and post messages. On semi-trusted networks, enable the **access token** in "CC Settings" on the desktop — phones enter the 6-digit pairing code on first open (once per device; too many wrong attempts trigger a temporary lockout as brute-force protection; the desktop itself is exempt). In any case, do not use it on public Wi-Fi, and never expose the service address to the internet.

⚠️ The service listens on all network interfaces: while connected to a **corporate VPN**, other hosts on the VPN segment can reach your shared directory — keep the service stopped on untrusted networks. The address/QR code automatically prefers the real LAN IP over VPN virtual interfaces (multiple candidates are selectable in the QR dialog).

## Documentation

Docs are currently in Chinese:

- [Requirements Design](docs/需求设计文档.html) · [Technical Implementation](docs/技术实现文档.html) · [User Guide](docs/保姆级使用说明.html)
- [History Log 2026-10-28 ~ 11-02](docs/整个过程历史改动记录_2026-10-28至2026-11-02.html) · [History Log 10-23 ~ 10-27](docs/整个过程历史改动记录_2026-10-23至2026-10-27.html) · [Marketing Plan](docs/推广营销方案.html) · [Roadmap](docs/后期规划.html)

<!-- 🖼 Screenshots (阶段一素材): save under assets/ and place here, e.g.
- assets/screenshot-desktop.png   — desktop window (files view)
- assets/screenshot-mobile.png    — mobile browser page
- assets/screenshot-tour.png      — in-app intro page
-->

---

Copyright © 2026-Present 李航坤. All Rights Reserved.
