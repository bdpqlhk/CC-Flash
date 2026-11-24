# CC Flash · Blazing-Fast LAN File Transfer

**English** | [简体中文](./README.md)

Turn your computer into a server with one click — phones on the same Wi-Fi scan a QR code and start transferring. Share files and messages between phones, tablets, and PCs via the browser: **no login, no internet, no setup**. Speed is whatever your router can do.

<!-- 🎬 Demo GIF (阶段一素材): 3-second loop — start service → phone scans QR → file transferred. Save as assets/demo.gif and uncomment:
[![CC-Flash demo](assets/demo.gif)](assets/demo.gif)
-->

Built with **Tauri 2 + Rust (axum) + Vue 3 + Vite** (current app version **1.2.6**, full-review fixes, full self-test and artifact slimming of 2026-11-23). The frontend main bundle is **322,224 B (~315 KiB)** plus a 24,533 B QR chunk loaded on demand, CSS **134,734 B (~132 KiB)**, `dist` **541,530 B (~529 KiB)** total; the Rust binary is **4,084,368 B (arm64) / 5,045,392 B (x64)** (strip + fat LTO + opt-level=z + panic=abort). The Windows installer now uses the WebView2 **downloadBootstrapper** (no ~1.8 MB embedded bootstrapper), Android keeps only Chinese/English resources and ships a single arm64-v8a APK; the per-arch macOS dmg is **2.73–2.85 MiB** and the .app **4.12 MiB (arm64) / 5.03 MiB (x64)** (see the Release assets).

## Features

- 💬 **Conversational transfer (one entry: "Transfer")** — files and messages share one timeline: uploads appear as **file cards** (waiting / uploading / done / failed, live progress + speed, cancel / retry), and batch uploads collapse into a **file group**. Text stays as bubbles with nicknames, avatars, **Markdown** (headings / lists / quotes / dividers / code blocks / inline code / bold / italic / strikethrough / links), **multi-line input** (desktop: Enter sends, Shift+Enter adds a line; mobile: Enter adds a line, tap Send), unread, and a **200,000-character** per-message cap (Unicode-counted, live counter, send blocked with a clear warning when exceeded). The full shared-file list lives in a **right drawer (340–400px, larger type, full-width pager footer) / mobile App Files page** with sort, pagination, select-all, bulk delete, open / copy / download / share; thumbnails and the "Preview" button open the in-app preview directly
- 📁 **File capabilities** — drag & drop / click / `Ctrl+V` / whole-folder upload (structure preserved), large files as a **single whole-file streaming request** (streaming to disk, zero memory bloat; failed / timed-out / stopped uploads roll back the partial file), **screen keep-alive during upload** (3-slot client queue + stalled-connection watchdog; server-side upload / download / WebSocket gates of 16 / 32 / 64), same-name files **never overwritten** (auto `file(1).zip`), Range-resume downloads (including `bytes=-N` suffix ranges) and full Unicode filenames; uploads must carry `X-File-Size` or `Content-Length` (the server uses it for completeness checks; missing size returns 411)
- 🔍 **In-app file preview** — images / PDF / text (txt/md/log/csv) / audio and video open full-screen from a thumbnail or the "Preview" button; the top-right "Close" button or Esc returns to the chat. Office documents and archives still need to be downloaded; html/svg/xml are never opened inline as web pages
- 💠 **Floating scan orb** — an always-on-top transparent breathing dot docked half-hidden at the screen edge (default: left middle, **position persisted across restarts**), coral breathing ring + viewfinder CC badge; dwell the pointer on it for 1.5s to preview the "Scan with browser" card, leave to collapse; **a breathing ring means the service is running, a still ring means stopped**; a green dot (grouped with the ring, scaling together) flags new files / messages (hidden & cleared while the main window is open); drag it and it snaps to the nearest edge — **multi-monitor ready**: it docks to whichever display you drop it on, remembers that display too, places correctly under Windows mixed DPI, and comes back there after restart — the only off switch lives in Settings
- 🖥 **Service center** — start/stop, port, disk space at a glance; **loguru-style structured logs** (color-coded levels / source file:line / auto-follow), issues spotted at a glance
- 📱 **Four surfaces, one conversation** — desktop shell (sidebar + file drawer + tray/pet; the title sits on the left while running status + address + copy / QR / browser buttons form one group on the top right, the address field sizes to its content, and the group wraps on narrow screens) / desktop browser (top bar + drawer) / mobile browser (single column + original chat page; no bottom tabs, discovery or settings) / mobile app (**standalone node**: local file service + chat, discoverable by peers on the same Wi-Fi); the same timeline and components everywhere. Desktop runs on macOS 11+ / Windows / Linux (Linux builds from source); mobile ships an Android APK, while iOS has no installer — open the desktop address in Safari. WeChat's built-in browser shows an "open in browser" hint. The mobile app always listens on the LAN and auto-generates a local access token (can be turned off in Settings after a risk confirmation); Android keeps the process alive with a foreground service
- 📱 **Message workspace (devices merged into Send)** — on desktop, the Send page has a Snapdrop-style left rail (local identity card + radar + nearby list with online / busy / last-seen states); tapping a device selects it as the send target (multi-select is supported for sending to several at once), switches the composer to direct send and appends a local receipt to the timeline. The mobile App has four tabs (Send / Devices / Files / Settings): the Send page is chat + direct send, Devices is the radar + device list + manual send, Files is the shared-file manifest (Send / Devices / Files follow the dir-2 signal-console design: TARGET/UDP/MANIFEST readout headers, ticked dial, RSSI rows, numbered manifest with progress bars, square composer and amber send key), Settings (dir-5) holds the access-token switch / clipboard sync / local token / appearance. On narrow desktop windows, a compact “Devices” button on the left of the chat header opens a full-page vertical device panel (chat list/composer hidden, no modal); selecting a device returns to the chat, while wide desktop keeps the 200px rail. A legend at the bottom of the radar module explains online / busy / offline dots. Chat, files and shared clipboard stay on one timeline; the device rail can be collapsed/reopened, the nearby section has its own scan button, and a clock button at the top-right of the local identity card opens the transfer-history dialog; tapping a device only selects the target (no auto-opened panel); the Manual Send entry opens the IP:port/folder/text panel, and in multi-select mode messages and dropped files are sent to every selected device in turn; send failures and the peer pairing dialog appear above the panel, and uploads from the mobile Files page always go to the local share instead of reusing the Send-page target; file transfer events are persisted by the local service (`file-history.json`, last 200) so sent files remain in the chat timeline after a browser refresh or app restart; mobile/desktop browsers keep the original no-discovery chat page. The sidebar is 176px, the device rail 200px and the file drawer 320–380px; at 981–1180px the drawer becomes an overlay with a backdrop
- 📋 **Shared clipboard (per local service, opt-in)** — system-clipboard auto-sync is **off by default**; enable "System clipboard auto-sync" in CC Settings. Once enabled, an App writes its local clipboard into **its own service's** shared slot while visible and applies slot changes back to the local clipboard; browsers opening the same computer/phone address share that slot (copy on the computer → paste in a phone browser; paste in a phone-browser chat composer → back to the computer). **The desktop App and mobile App are independent nodes and do not auto-connect**: send text across Apps with the Send panel's Clipboard tab. All text (including tokens/verification codes) is synced; there is no content filter. Browsers on `http://LAN-IP` cannot read/write the system clipboard programmatically: tapping Paste fills the composer from the shared slot, and only text pasted into the **chat composer** is pushed back. 20k char cap, text only
- 📲 **PWA** — installable on `https` or `localhost` (manifest + service-worker offline shell + update prompt). `http://LAN-IP` is not a secure context, so browsers disable service workers and installation there; in that case the UI gives platform-specific "add to home screen" guidance (iOS Safari included)
- 📡 **Nearby devices** — CC Flash nodes on the same Wi-Fi discover each other over UDP multicast (IPv4 `239.255.42.99:53318` + IPv6 link-local `ff02::ccf1:53318`) plus broadcast, with **per-interface multicast egress** (Wi-Fi + VPN/virtual adapters no longer pick the wrong interface), a 3s announcement loop, an immediate active probe on start/manual refresh, 30s TTL, a bye packet on graceful stop, instant `devices:presence` with server-computed `online` (12s grace), presence on every valid announce merged to at most once per second (trailing guaranteed) plus a 3s full-snapshot fallback, 3s unicast announces / 5s unicast probes to known peers, a 3s polling fallback and a 53318-53321 port fallback; unicast replies to senders let asymmetric AP isolation converge; the radar itself is clickable to run an active scan, while tapping a device dot selects that send target; recently seen peer addresses are cached and receive unicast heartbeats/probes when multicast is filtered; a full /24 sweep only runs on an explicit user scan, and the empty state gives macOS/Windows network-permission guidance; online/busy/last-seen state and favourites pinned. **Discovery runs only in the desktop and mobile Apps**; browsers no longer call `/api/devices` or `/api/hello` and are intentionally excluded (no more "tappable but unreachable" entries). Android holds a MulticastLock; the client only allows delivery to private LAN addresses. **Browsers no longer expose a cross-device direct-send UI** (cross-origin HTTP would be blocked by the page CSP); the server still only allows delivery-style POSTs from a malicious cross-origin page and rejects reads/deletes/share-ticket creation. Desktop/mobile app shells can direct-send; if the target has enabled the access token, enter its pairing code (remembered by stable device id). If devices don't appear: check that both sides report “LAN visible” in Diagnostics; allow macOS Local Network permission and Windows Firewall UDP 53318-53321 on private networks; both machines must be on the same Wi-Fi/subnet (guest networks, AP isolation and wired/wireless subnets block discovery; VPN/Tailscale interfaces are intentionally skipped). In the desktop/mobile app, Diagnostics can send a manual unicast probe to the peer IP; if that still fails, use Manual Send with IP:port over TCP to bypass multicast/broadcast filtering.
- 🗂 **Native folder picker** — click "Choose" next to the share directory and pick with the OS dialog
- 🎨 **Theme system** — Light / Dark / Follow-system modes (auto-switches with OS appearance) × **6 color schemes** (**Signal Desk**, Deep Space Blue, Graphite Rose, Forest Mint, Dark Purple, Ink Amber); Signal Desk dark is the default; switches apply instantly, and desktop chat bubbles follow the theme
- 🛡 **Security hardening** — double path-traversal protection; on Windows, delete/download reject trailing-dot/space/ADS aliases and re-derive in-flight/marker checks from the canonical path; browser cross-origin guard (DNS rebinding / CSRF; Host limited to IP/localhost; control endpoints additionally reject `Sec-Fetch-Site: cross-site` so `<a ping>` cannot stop the service or wipe logs); XSS escaping in chat; `nosniff / SAMEORIGIN` plus a **full script-src CSP** on served pages; disk-space pre-check with a 256 MB reserve; optional **access token** (6-digit CSPRNG pairing code with per-IP exponential backoff plus a **global failure budget for public sources**; HTTP requests carry the code in a header (the WebSocket handshake uses a query parameter because browsers cannot set custom headers); a separate enabled marker is persisted so a temporarily unreadable/missing code fails closed and an explicit disable stays disabled; codes are written atomically); shared links carry a **one-time ticket** (12-hex-char token bound to file size/ETag, 10-minute expiry, burned on first download; invalidated when the file is deleted or replaced; new tickets are refused rather than evicting old ones); thumbnails/previews use a short-lived media-session token (10 min, bound to the source IP, reusable for HEAD/GET/Range), so the long-lived PIN never appears in `<img>/<video>/<iframe>` URLs; download responses send `Referrer-Policy: no-referrer`; ticket/file-fingerprint checks and upload directory probes run on `spawn_blocking`; upload / download / WebSocket gates (16 / 32 / 64) plus **per-source quotas** (4 uploads / 8 downloads, IPv6 per /64) and a **45s download idle timeout**; audit logs reserve 200 entries for warnings and gate successful transfer logs per source; chat/clipboard persistence runs on a blocking worker; deletes never follow symlinks; discovery `bye` packets must echo the per-process announce token (raises the bar against blind eviction); **clipboard auto-sync is off by default**; the desktop **always listens on the LAN** (no switch in CC Settings), and the mobile app **also always listens on the LAN and auto-generates a local access token** (peers need the pairing code; other apps on the same phone cannot connect unless the user explicitly disables the token in Settings after a risk confirmation)
- 🔄 **Update checker** — reads the GitHub Releases list and **only considers stable releases (prereleases are skipped)**; prompts with release notes and one click to download. Unsigned builds are published with a clear warning when certificates are missing; stable releases are offered to the in-app updater.
- 📘 **In-app tour** — sidebar "ⓘ Intro" opens a full-screen walkthrough (features / 3-step guide / tech / FAQ)
- 🖥 **Tray & single-instance** — closing the window keeps the service running; dock icon, tray click, or relaunching all re-focus the same window
- ⏱ **Optional autostart** — off by default; enable in Settings to start silently in the background at login
- ⚙️ **Centralized config** — backend is the single source of truth; runtime params are pushed to all clients
- 🚀 **CI releases** — push a `v*` tag (four-part `vX.Y.Z.W` tags are supported; the fourth part is a build number); two per-arch macOS dmg files, one Windows exe and one Android arm64-v8a APK are published automatically. CI validates the tag against `Cargo.toml`, `package.json`, `config.js` and the Android `versionCode`, and installs dependencies with `npm ci`

## Quick Start

### Users

Grab your installer from the [Releases page](https://github.com/bdpqlhk/CC-Flash/releases):

- **macOS**: pick the `.dmg` for your chip — `_aarch64.dmg` (Apple Silicon) or `_x64.dmg` (Intel) — drag into Applications
- **Windows**: `.exe` installer
- **Android**: a single `arm64-v8a` APK covers mainstream 64-bit phones (Android 7.0+, minSdk 24); x86 and 32-bit splits are no longer published
- **Linux**: build from source (`npm run tauri build`); CI does not publish Linux installers yet

Launch the app — the service starts automatically (default port `1688`, auto-increments if taken). Scan the QR code or type the address on your phone. There is one "Transfer" entry: uploaded files appear as cards in the conversation, and the full shared-file list lives in the right drawer (340–400px) or the mobile App Files tab; thumbnails and the "Preview" button open the in-app preview. The Android app runs as an independent LAN node with an auto-generated pairing code and is discoverable by peers on the same Wi-Fi.

### Developers

```bash
# Prerequisites: Rust 1.77+, Node.js 18+; macOS needs Xcode CLT, Windows needs MSVC Build Tools + WebView2
npm install
npm run tauri dev      # dev mode: frontend HMR + incremental Rust builds
npm run check          # fast Rust sanity check
npm run tauri build    # release build: produces .app / .dmg / .exe
```

## Project Layout

```
├── index.html / vite.config.js     # Vue 3 + Vite frontend
├── src/
│   ├── App.vue                     # all UI + interaction logic
│   ├── config.js                   # frontend config center (polling, thresholds, copy…)
│   ├── update.js                   # release checker (GitHub Releases list, stable-only + semver compare)
│   ├── style.css                   # theme tokens + all styles (6 color schemes × light/dark)
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

Versions may be `X.Y.Z` or four-part `X.Y.Z.W`: **the git tag uses all four parts (e.g. `v1.2.2.1`)**, where the fourth part is a build number within the same app version; the app version (Cargo.toml / installers) uses the first three (`1.2.2`). CI validates the first three tag parts against `Cargo.toml`, and also checks `package.json`, the `config.js` fallback and the Android `versionCode`. `release.sh` computes the Android `versionCode` as `major×10^7 + minor×10^5 + patch×10^2 + build` (capped at 2.1e9), so rebuilds of the same version can upgrade in place. **Delivered filenames always use the full four-part version** (e.g. `CC-Flash_1.2.2.1_aarch64.dmg`, `CC-Flash_1.2.2.1_x64-setup.exe`, `CC-Flash-v1.2.2.1-android-arm64-v8a.apk`), and Android's `versionName` is synced to that version (no longer the template default `1.0`), so the installer shows the real build number. When `tauri.properties` is absent, `versionCode` falls back to `tauri.conf.json > bundle > android > versionCode` instead of the template default `1`.

The script bumps the version (Cargo.toml is the source of truth), syncs the fallback version / `package.json` / Android `versionCode`, commits, tags, pushes — CI builds all three platforms and attaches artifacts to the Release.

Android signing: all four `ANDROID_KEYSTORE_*` secrets are required for **release signing** (stable cert → in-place upgrades, a single arm64-v8a APK). If any is missing, CI **fails** — a debug-signed APK must never reach a public release (debuggable, different cert every build, cannot upgrade in place). Releases use an **all-green publish policy**: artifacts upload as a draft first, and the Release goes live only after both desktop and Android jobs succeed; the Android job also verifies the Release is still a draft, so re-running an already-published tag cannot replace live assets. R8 keeps `SourceFile,LineNumberTable` and the app package, and Gradle's NDK version follows `NDK_HOME` to match the Rust build. Optional signing/notarization is wired through `APPLE_*` and `WINDOWS_CERTIFICATE*` secrets. **Unsigned builds are published with a clear warning when certificates are missing**: the publish job emits a `::warning::` and continues, and the Release notes must state the macOS right-click-to-open / Windows SmartScreen steps. When certificates are configured, builds are signed/notarized and verified with `codesign/stapler` / Authenticode. Empty certificate secrets are not injected into the build environment (avoiding Tauri's `Unable to decode`); desktop jobs also verify `codesign/stapler` (macOS) and Authenticode (Windows), Release lookup treats only 404 as missing, and third-party Actions are pinned to commit SHAs. **This repository currently only has Android signing secrets; macOS/Windows artifacts are therefore published unsigned, and the release notes must carry the right-click-to-open / SmartScreen notice.** Local `./scripts/build-all.sh android` fails unless `CC_KEYSTORE_*` is set and verifies no unsigned APK remains.

macOS ships as **two per-arch dmg files** (Apple Silicon + Intel), Windows as **one exe**, Android as **one arm64-v8a APK**, and targets **macOS 11.0+** (the ES2021 frontend needs a Safari 14+ WebView). macOS 15 asks for Local Network permission on first run; the purpose string comes from `src-tauri/Info.plist` (re-enable under System Settings → Privacy & Security → Local Network if denied). **iOS**: no iOS build is currently provided (requires an Apple Developer account; the `gen/apple` project and local-network permission declarations do not exist yet, and shell capabilities such as open_url are not adapted). Use Safari against the desktop address instead; `./scripts/build-all.sh ios` now refuses by default (use `CCFLASH_IOS_EXPERIMENTAL=1` only for experimental work; there is no `gen/apple`, multicast entitlement, background mode or signing, so it is not a deliverable).

## Security Notice

This tool is built for **trusted LANs** (home / trusted office): the desktop exposes the shared directory to anyone on the same network by default. On semi-trusted networks, enable the **access token** in CC Settings — phones enter the 6-digit pairing code on first open (once per device; repeated wrong attempts trigger per-IP exponential backoff plus a global failure budget for public sources that defeats IPv6 prefix rotation; the desktop itself is exempt). Once enabled, shared links carry a **one-time authorization** — they can only download that single file, expire in 10 minutes, and are burned on first download; recipients need no code. Disabling/rotating the token, changing the share directory, deleting or replacing the file invalidates outstanding tickets, and invalid ticket attempts are rate-limited per IP. The server caps upload / download / WebSocket concurrency at 16 / 32 / 64, adds per-source quotas (4 uploads / 8 downloads, IPv6 per /64) and a 45s download idle timeout, serves pages with a full script-src CSP, and never follows symlinks on delete. Clipboard auto-sync is off by default; when enabled, all text (including tokens/codes) is synced with no content filter. **Public peers (including global IPv6) are rejected unless the access token is enabled.** Use an IP address (the Host guard intentionally rejects `*.local` to prevent DNS rebinding).

- **The mobile app always listens on the LAN and auto-generates a local access token**: peers need the pairing code; other apps on the same phone cannot reach the local service. CC Settings no longer offers a “LAN access” switch; the access token can be turned off in the mobile Settings page after a risk confirmation.
- **The desktop and mobile app always listen on the LAN**: CC Settings no longer offers a “LAN access” switch, and an old saved loopback-only preference no longer applies.
- **System clipboard auto-sync is off by default**; enable it in CC Settings. All text (including tokens and pairing codes) is synced with no content filter. On public Wi-Fi, leave it off.
- Content delivered via **Nearby devices** lands in the recipient's public chat / shared directory and is visible to everyone on that node — do not send secrets.
- **VPNs and overlay networks count as LAN**: `10/8`, `172.16/12`, `192.168/16`, `100.64/10` (CGNAT / Tailscale) and IPv6 ULA/link-local are treated as private and reachable without a token. If you share a corporate VPN or Tailscale tailnet, either enable the token or keep the service stopped. The address/QR code automatically prefers the real LAN IP over VPN virtual interfaces (multiple candidates are selectable in the QR dialog).

⚠️ The service listens on all network interfaces: while connected to a **corporate VPN**, other hosts on the VPN segment can reach your shared directory — keep the service stopped on untrusted networks. Never use it on public Wi-Fi, and never expose the service address to the internet.

## Documentation

Docs are currently in Chinese:

- [Requirements Design](docs/需求设计文档.html) · [Technical Implementation](docs/技术实现文档.html) · [User Guide](docs/保姆级使用说明.html)
- [User Guide · PixPin-style edition](docs/保姆级使用说明-PixPin版.html) (also [UU edition](docs/保姆级使用说明-UU版.html) / [SkillPay edition](docs/保姆级使用说明-SkillPay版.html))
- [History Log 2026-11-24 release pipeline & Android version fixes](docs/整个过程历史改动记录_2026-11-24.html) · [History Log 2026-11-23](docs/整个过程历史改动记录_2026-11-23.html) · [2026-11-22 full review](docs/整个过程历史改动记录_2026-11-22.html) · [2026-11-20 ~ 11-21](docs/整个过程历史改动记录_2026-11-20至2026-11-21.html) · [2026-11-19](docs/整个过程历史改动记录_2026-11-19.html) · [11-17 ~ 11-18](docs/整个过程历史改动记录_2026-11-17至2026-11-18.html) · [11-13 ~ 11-16](docs/整个过程历史改动记录_2026-11-13至2026-11-16.html) · [11-03 ~ 11-12](docs/整个过程历史改动记录_2026-11-03至2026-11-12.html) · [10-28 ~ 11-02](docs/整个过程历史改动记录_2026-10-28至2026-11-02.html) · [10-23 ~ 10-27](docs/整个过程历史改动记录_2026-10-23至2026-10-27.html) · [Marketing Plan](docs/推广营销方案.html) · [Roadmap](docs/后期规划.html)

<!-- 🖼 Screenshots (阶段一素材): save under assets/ and place here, e.g.
- assets/screenshot-desktop.png   — desktop window (files view)
- assets/screenshot-mobile.png    — mobile browser page
- assets/screenshot-tour.png      — in-app intro page
-->

---

Copyright © 2026-Present 李航坤. All Rights Reserved.
