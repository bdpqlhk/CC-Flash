# CC-Flash · Blazing-Fast LAN File Transfer

**English** | [简体中文](./README.md)

Turn your computer into a server with one click — phones on the same Wi-Fi scan a QR code and start transferring. Share files and messages between phones, tablets, and PCs via the browser: **no login, no internet, no setup**. Speed is whatever your router can do.

<!-- 🎬 Demo GIF (阶段一素材): 3-second loop — start service → phone scans QR → file transferred. Save as assets/demo.gif and uncomment:
[![CC-Flash demo](assets/demo.gif)](assets/demo.gif)
-->

Built with **Tauri 2 + Rust (axum) + Vue 3 + Vite** (current app version **1.2.2**). Desktop installers are per-arch dmg (~**2.7 MB**) / .app (~4.3 MB); the frontend bundle is 329 KB (143 KB gzip) and the binary is 4.0 MB. Windows and Android packages benefit from the same lossless icon/PNG and frontend slimming (not built on this machine).

## Features

- 💬 **Conversational transfer (one entry: "Transfer")** — files and messages share one timeline: uploads appear as **file cards** (waiting / uploading / done / failed, live progress + speed, cancel / retry), and batch uploads collapse into a **file group**. Text stays as bubbles with nicknames, avatars, **Markdown** (headings / lists / quotes / dividers / code blocks / inline code / bold / italic / strikethrough / links), **multi-line input** (desktop: Enter sends, Shift+Enter adds a line; mobile: Enter adds a line, tap Send), and unread. The full shared-file list lives in a **right drawer (340–400px, larger type, full-width pager footer) / mobile bottom sheet** with sort, pagination, select-all, bulk delete, open / copy / download / share; thumbnails and the "Preview" button open the in-app preview directly
- 📁 **File capabilities** — drag & drop / click / `Ctrl+V` / whole-folder upload (structure preserved), large files as a **single whole-file streaming request** (streaming to disk, zero memory bloat; failed / timed-out / stopped uploads roll back the partial file), **screen keep-alive during upload** (3-slot client queue + stalled-connection watchdog; server-side upload / download / WebSocket gates of 16 / 32 / 64), same-name files **never overwritten** (auto `file(1).zip`), Range-resume downloads, full Unicode filenames
- 🔍 **In-app file preview** — images / PDF / text (txt/md/log/csv) / audio and video open full-screen from a thumbnail or the "Preview" button; the top-right "Close" button or Esc returns to the chat. Office documents and archives still need to be downloaded; html/svg/xml are never opened inline as web pages
- 💠 **Floating scan orb** — an always-on-top transparent breathing dot docked half-hidden at the screen edge (default: left middle, **position persisted across restarts**), coral breathing ring + viewfinder CC badge; dwell the pointer on it for 1.5s to preview the "Scan with browser" card, leave to collapse; **a breathing ring means the service is running, a still ring means stopped**; a green dot (grouped with the ring, scaling together) flags new files / messages (hidden & cleared while the main window is open); drag it and it snaps to the nearest edge — **multi-monitor ready**: it docks to whichever display you drop it on, remembers that display too, places correctly under Windows mixed DPI, and comes back there after restart — the only off switch lives in Settings
- 🖥 **Service center** — start/stop, port, disk space at a glance; **loguru-style structured logs** (color-coded levels / source file:line / auto-follow), issues spotted at a glance
- 📱 **Four surfaces, one conversation** — desktop shell (sidebar + file drawer + tray/pet; the title sits on the left while running status + address + copy / QR / browser buttons form one group on the top right, the address field sizes to its content, and the group wraps on narrow screens) / desktop browser (top bar + drawer) / mobile browser (single column + bottom file sheet) / mobile app (**standalone node by default**: local file service + chat; switch to "connect to computer" in Settings to browse the desktop's shared files); the same timeline and components everywhere. Desktop runs on macOS 11+ / Windows / Linux (Linux builds from source); mobile ships an Android APK, while iOS has no installer — open the desktop address in Safari. WeChat's built-in browser shows an "open in browser" hint. The mobile app listens on **127.0.0.1 only** until you enable "LAN access" in Settings; Android keeps the process alive with a foreground service
- 📡 **Nearby devices** — CC-Flash nodes on the same Wi-Fi discover each other over UDP multicast (239.255.42.99:53318) plus broadcast; tap a device to deliver a message or file to that node, with favourites pinned. Pure browser pages have no listening port and are intentionally excluded (no more "tappable but unreachable" entries). Android holds a MulticastLock; the client only allows delivery to private LAN addresses. **Browser-to-device delivery without an access token is limited to delivery-style POSTs (chat/upload)**; reads, deletes and share-ticket creation remain rejected to prevent a malicious LAN page from acting through the same-machine browser. Desktop/mobile app shells are not affected. If the target has enabled the access token, enter its pairing code in the delivery dialog (remembered per endpoint).
- 🗂 **Native folder picker** — click "Choose" next to the share directory and pick with the OS dialog
- 🎨 **Theme system** — Light / Dark / Follow-system modes (auto-switches with OS appearance) × **5 color schemes** (**Deep Space Blue**, Graphite Rose, Forest Mint, Dark Purple, Ink Amber); Deep Space Blue light is the default; switches apply instantly, and desktop chat bubbles follow the theme
- 🛡 **Security hardening** — double path-traversal protection, browser cross-origin guard (DNS rebinding / CSRF; Host limited to IP/localhost; control endpoints additionally reject `Sec-Fetch-Site: cross-site` so `<a ping>` cannot stop the service or wipe logs), XSS escaping in chat, `nosniff / SAMEORIGIN / frame-ancestors` on served pages to block clickjacking, disk-space pre-check with a 256 MB reserve, optional **access token** (6-digit pairing code with per-IP exponential backoff plus a **global failure budget for public sources** against IPv6 prefix rotation; enter once per device; the desktop itself is exempt; a stale code is cleared automatically so a rotated token cannot lock the device out); shared links carry a **one-time ticket** (12-hex-char token bound to file size/ETag, 10-minute expiry, burned on first download; invalidated immediately when the file is deleted or replaced; new tickets are refused rather than evicting old ones); upload / download / WebSocket concurrency gates (16 / 32 / 64); CSP permits LAN media and iframes without opening up scripts or objects; deletes never follow symlinks; the mobile app defaults to loopback-only
- 🔄 **Update checker** — reads the GitHub Releases list (including prereleases published when builds are unsigned, rather than relying only on `/latest`), prompts with release notes, one click to download
- 📘 **In-app tour** — sidebar "ⓘ Intro" opens a full-screen walkthrough (features / 3-step guide / tech / FAQ)
- 🖥 **Tray & single-instance** — closing the window keeps the service running; dock icon, tray click, or relaunching all re-focus the same window
- ⏱ **Optional autostart** — off by default; enable in Settings to start silently in the background at login
- ⚙️ **Centralized config** — backend is the single source of truth; runtime params are pushed to all clients
- 🚀 **CI releases** — push a `v*` tag (four-part `vX.Y.Z.W` tags are supported; the fourth part is a build number); macOS / Windows / Android (4 ABI splits) builds are published automatically. CI validates the tag against `Cargo.toml`, `package.json`, `config.js` and the Android `versionCode`, and installs dependencies with `npm ci`

## Quick Start

### Users

Grab your installer from the [Releases page](https://github.com/bdpqlhk/CC-Flash/releases):

- **macOS**: pick the `.dmg` for your chip — `_aarch64.dmg` (Apple Silicon) or `_x64.dmg` (Intel) — drag into Applications
- **Windows**: `.msi` / `.exe`
- **Android**: pick the `.apk` for your CPU — `arm64-v8a` (almost all phones) / `armeabi-v7a` / `x86` / `x86_64`
- **Linux**: build from source (`npm run tauri build`); CI does not publish Linux installers yet

Launch the app — the service starts automatically (default port `1688`, auto-increments if taken). Scan the QR code or type the address on your phone. There is one "Transfer" entry: uploaded files appear as cards in the conversation, and the full shared-file list lives in the right drawer (340–400px, bottom sheet on mobile); thumbnails and the "Preview" button open the in-app preview. The Android app starts as a loopback-only node; to browse the desktop's files, open "CC Settings → Connect to computer" and enter the desktop address.

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
│   ├── update.js                   # release checker (GitHub Releases list + semver compare, prerelease-aware)
│   ├── style.css                   # theme tokens + all styles (5 color schemes × light/dark)
│   ├── pet-activity.js             # floating-orb activity state
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
- **Version** — edit only `src-tauri/Cargo.toml` by hand; `release.sh` automatically syncs the `src/config.js` fallback, `package.json`, and the Android `versionCode`; desktop display, update check, and API all read the same value.

## Releasing (maintainers)

```bash
./scripts/release.sh 1.2.2.1 "feat: release notes"
```

Versions may be `X.Y.Z` or four-part `X.Y.Z.W`: **the git tag uses all four parts (e.g. `v1.2.2.1`)**, where the fourth part is a build number within the same app version; the app version (Cargo.toml / installers) uses the first three (`1.2.2`). CI validates the first three tag parts against `Cargo.toml`, and also checks `package.json`, the `config.js` fallback and the Android `versionCode`. `release.sh` computes the Android `versionCode` as `major×10^7 + minor×10^5 + patch×10^2 + build` (capped at 2.1e9), so rebuilds of the same version can upgrade in place.

The script bumps the version (Cargo.toml is the source of truth), syncs the fallback version / `package.json` / Android `versionCode`, commits, tags, pushes — CI builds all three platforms and attaches artifacts to the Release.

Android signing: all four `ANDROID_KEYSTORE_*` secrets are required for **release signing** (stable cert → in-place upgrades, `--split-per-abi` per-CPU APKs). If any is missing, CI **fails** — a debug-signed APK must never reach a public release (debuggable, different cert every build, cannot upgrade in place). Releases use an **all-green publish policy**: artifacts upload as a draft first, and the Release goes live only after both desktop and Android jobs succeed; the Android job also verifies the Release is still a draft, so re-running an already-published tag cannot replace live assets. R8 keeps `SourceFile,LineNumberTable` and the app package, and Gradle's NDK version follows `NDK_HOME` to match the Rust build. Optional signing/notarization is wired through `APPLE_*` and `WINDOWS_CERTIFICATE*` secrets. Without them, a fully green build is still published, but marked as a **prerelease** rather than a stable release; the update checker reads the releases list, so these builds are still offered. Local `./scripts/build-all.sh android` fails unless `CC_KEYSTORE_*` is set and verifies no unsigned APK remains.

macOS builds ship as **per-arch packages** (`_aarch64.dmg` / `_x64.dmg`, roughly half the former universal bundle) and target **macOS 11.0+** (the ES2021 frontend needs a Safari 14+ WebView). macOS 15 asks for Local Network permission on first run; the purpose string comes from `src-tauri/Info.plist` (re-enable under System Settings → Privacy & Security → Local Network if denied). **iOS**: no iOS build is currently provided (requires an Apple Developer account; the `gen/apple` project and local-network permission declarations do not exist yet, and shell capabilities such as open_url are not adapted). Use Safari against the desktop address instead; `./scripts/build-all.sh ios` remains the entry point for future work.

## Security Notice

This tool is built for **trusted LANs** (home / trusted office): the desktop exposes the shared directory to anyone on the same network by default. On semi-trusted networks, enable the **access token** in "CC Settings" — phones enter the 6-digit pairing code on first open (once per device; repeated wrong attempts trigger per-IP exponential backoff plus a global failure budget for public sources that defeats IPv6 prefix rotation; the desktop itself is exempt). Once enabled, shared links carry a **one-time authorization** — they can only download that single file, expire in 10 minutes, and are burned on first download; recipients need no code. Disabling/rotating the token, changing the share directory, deleting or replacing the file invalidates outstanding tickets, and invalid ticket attempts are rate-limited per IP. The server caps upload / download / WebSocket concurrency at 16 / 32 / 64, its CSP permits LAN media and iframes without opening up scripts or objects, and deletes never follow symlinks. **Public peers (including global IPv6) are rejected unless the access token is enabled.** Use an IP address (the Host guard intentionally rejects `*.local` to prevent DNS rebinding).

- **The mobile app is loopback-only by default**; it cannot be reached from the Wi-Fi until you enable "LAN access" in CC Settings → Local service (restart the service to apply). Enable the access token too if other people share the network.
- Content delivered via **Nearby devices** lands in the recipient's public chat / shared directory and is visible to everyone on that node — do not send secrets.
- **VPNs and overlay networks count as LAN**: `10/8`, `172.16/12`, `192.168/16`, `100.64/10` (CGNAT / Tailscale) and IPv6 ULA/link-local are treated as private and reachable without a token. If you share a corporate VPN or Tailscale tailnet, either enable the token or keep the service stopped. The address/QR code automatically prefers the real LAN IP over VPN virtual interfaces (multiple candidates are selectable in the QR dialog).

⚠️ The service listens on all network interfaces: while connected to a **corporate VPN**, other hosts on the VPN segment can reach your shared directory — keep the service stopped on untrusted networks. Never use it on public Wi-Fi, and never expose the service address to the internet.

## Documentation

Docs are currently in Chinese:

- [Requirements Design](docs/需求设计文档.html) · [Technical Implementation](docs/技术实现文档.html) · [User Guide](docs/保姆级使用说明.html)
- [User Guide · PixPin-style edition](docs/保姆级使用说明-PixPin版.html) (also [UU edition](docs/保姆级使用说明-UU版.html) / [SkillPay edition](docs/保姆级使用说明-SkillPay版.html))
- [History Log 2026-11-13 ~ 11-16](docs/整个过程历史改动记录_2026-11-13至2026-11-16.html) · [11-03 ~ 11-12](docs/整个过程历史改动记录_2026-11-03至2026-11-12.html) · [10-28 ~ 11-02](docs/整个过程历史改动记录_2026-10-28至2026-11-02.html) · [10-23 ~ 10-27](docs/整个过程历史改动记录_2026-10-23至2026-10-27.html) · [Marketing Plan](docs/推广营销方案.html) · [Roadmap](docs/后期规划.html)

<!-- 🖼 Screenshots (阶段一素材): save under assets/ and place here, e.g.
- assets/screenshot-desktop.png   — desktop window (files view)
- assets/screenshot-mobile.png    — mobile browser page
- assets/screenshot-tour.png      — in-app intro page
-->

---

Copyright © 2026-Present 李航坤. All Rights Reserved.
