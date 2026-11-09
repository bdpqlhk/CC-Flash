#!/usr/bin/env bash
# =====================================================================
# CC-Flash · 四端打包脚本 (macOS 入口)
#
# 用法:
#   ./build-all.sh mac       # macOS 桌面包 (.app + .dmg)
#   ./build-all.sh win       # Windows 包 (需在 Windows 上跑, 本脚本会给出指引)
#   ./build-all.sh android   # Android APK [debug]
#   ./build-all.sh ios       # iOS (真机包需签名, 详见提示)
#   ./build-all.sh all       # mac + android + ios 依次打包
#
# 可选: 第二参数 debug  → android/ios 构建调试包(免签名, 可直接安装)
#   例: ./build-all.sh android debug
#
# 各端前置条件(脚本会逐项检查, 缺什么提示什么):
#   通用:   Node 18+ / Rust 1.77+(rustup) / 项目已 npm install
#   mac:    Xcode Command Line Tools
#   win:    必须在 Windows 机器上运行 scripts/build-windows.ps1
#           (或使用 GitHub Actions 的 windows-latest)
#   android: Android Studio(SDK+NDK) + JDK 17, 环境变量
#           ANDROID_HOME / NDK_HOME / JAVA_HOME
#           rustup target add aarch64-linux-android armv7-linux-androideabi \
#                          i686-linux-android x86_64-linux-android
#   ios:    Xcode 15+(完整版, 含 iOS SDK)
#           rustup target add aarch64-apple-ios aarch64-apple-ios-sim
#           真机包需 Apple 开发者账号签名; 免签名可用 debug 包/模拟器
#           ⚠ 当前 iOS 尚未交付: gen/apple 工程/本地网络权限(NSLocalNetworkUsageDescription、
#             Bonjour)/open_url 等壳能力均未适配, 本入口仅用于后续适配
# =====================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGET="${1:-all}"
VARIANT="${2:-release}"          # release | debug (仅 android/ios 有效)
FAIL=0
WIN_SKIPPED=0                    # all 目标下 Windows 无法本机构建: 结束时不得再报"全部完成"

log()  { echo -e "\n\033[1;34m==== $* ====\033[0m"; }
ok()   { echo -e "\033[1;32m✔ $*\033[0m"; }
warn() { echo -e "\033[1;33m⚠ $*\033[0m"; }
die()  { echo -e "\033[1;31m✘ $*\033[0m"; FAIL=1; }

need() { command -v "$1" >/dev/null 2>&1 && ok "已安装: $1" || die "缺少: $1 ($2)"; }

has_target() { rustup target list --installed 2>/dev/null | grep -q "^$1$"; }

ensure_dist_frontend() {
  log "构建前端 (vite → dist/)"
  npm run build || die "前端构建失败"
}

# ---------------------------------------------------------------- mac
build_mac() {
  log "macOS 桌面包"
  need node "https://nodejs.org"
  need cargo "https://rustup.rs"
  ensure_dist_frontend
  npm run tauri build || die "macOS 打包失败"
  ok "产物: src-tauri/target/release/bundle/macos/*.app"
  ok "产物: src-tauri/target/release/bundle/dmg/*.dmg"
}

# ---------------------------------------------------------------- win
build_win() {
  log "Windows 包"
  if [[ "$(uname -s)" == MINGW* || "$(uname -s)" == CYGWIN* ]]; then
    # ps1 失败必须计入 FAIL: 否则 all 目标下 Windows 打包挂了仍打印"全部完成"并 exit 0
    powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1 || die "Windows 打包失败"
    return
  fi
  WIN_SKIPPED=1
  warn "Windows 包无法在 macOS/Linux 上交叉编译(需 MSVC + WebView2 SDK)。两种方式:"
  echo "  1) 把项目拷到 Windows 机器, 执行: powershell -File scripts/build-windows.ps1"
  echo "     前置: Node 18+ / rustup(MSVC 工具链) / Visual Studio Build Tools(C++ 工作负载) / Windows 10+ (自带 WebView2)"
  echo "  2) 用 GitHub Actions: 推送仓库后建 windows-latest 工作流调用同一 ps1"
}

# ------------------------------------------------------------ android
build_android() {
  log "Android APK ($VARIANT)"
  need node "https://nodejs.org"
  need cargo "https://rustup.rs"
  [[ -n "${JAVA_HOME:-}" ]] || warn "未设 JAVA_HOME (JDK 17), 若失败请安装 Android Studio 并配置"
  # 缺 SDK 时立即终止本目标: die 只置 FAIL 不退出, 继续跑 rustup/构建会级联出一堆误导性报错
  [[ -n "${ANDROID_HOME:-}" || -n "${ANDROID_SDK_ROOT:-}" ]] || { die "未设 ANDROID_HOME —— 请先安装 Android Studio 并在 shell 配置 ANDROID_HOME/NDK_HOME"; return 1; }
  for t in aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android; do
    has_target "$t" || { warn "添加 Rust 目标: rustup target add $t"; rustup target add "$t" || die "rustup target 添加失败: $t"; }
  done

  # 首次使用初始化 Android 工程
  if [[ ! -d src-tauri/gen/android ]]; then
    log "初始化 Android 工程 (首次)"
    npm run tauri android init || die "android init 失败"
    warn "已生成 src-tauri/gen/android —— 应用名/图标/签名可在其中调整"
  fi

  ensure_dist_frontend
  if [[ "$VARIANT" == "debug" ]]; then
    npm run tauri android build --apk --debug || die "Android debug 构建失败"
    ok "产物(debug apk, 可直接安装): src-tauri/gen/android/app/build/outputs/apk/"
  else
    npm run tauri android build --apk || die "Android release 构建失败"
    ok "产物(release apk): src-tauri/gen/android/app/build/outputs/apk/universal/release/"
    warn "上架应用市场需正式签名: 在 gen/android/app/build.gradle.kts 配置 signingConfigs, 或用 Google Play 应用签名"
  fi
}

# ---------------------------------------------------------------- ios
build_ios() {
  log "iOS ($VARIANT)"
  [[ "$(uname -s)" == Darwin ]] || die "iOS 打包只能在 macOS 上进行"
  need xcodebuild "App Store 安装完整版 Xcode (含 iOS SDK)"
  need cargo "https://rustup.rs"
  for t in aarch64-apple-ios aarch64-apple-ios-sim; do
    has_target "$t" || { warn "添加 Rust 目标: rustup target add $t"; rustup target add "$t" || die "rustup target 添加失败: $t"; }
  done

  if [[ ! -d src-tauri/gen/apple ]]; then
    log "初始化 iOS 工程 (首次)"
    npm run tauri ios init || die "ios init 失败"
    warn "已生成 src-tauri/gen/apple —— 真机签名需在 Xcode 中为两个 target 选择 Developer Team"
  fi
  warn "iOS 尚未交付: 还需补 Info.plist 本地网络权限(NSLocalNetworkUsageDescription/NSBonjourServices)、"
  warn "适配 open_url_cmd 等壳能力; 当前构建产物仅供适配调试, 不作为正式发布包"

  ensure_dist_frontend
  if [[ "$VARIANT" == "debug" || "$VARIANT" == "sim" ]]; then
    npm run tauri ios build -- --target aarch64-apple-ios-sim --debug || die "iOS 模拟器构建失败"
    ok "产物(模拟器): src-tauri/target/aarch64-apple-ios-sim/debug/ —— 可用 Xcode 或 模拟器 安装运行"
  else
    npm run tauri ios build -- --target aarch64-apple-ios || die "iOS 真机构建失败(多为未签名)"
    ok "产物: src-tauri/target/aarch64-apple-ios/release/bundle/ios/*.ipa"
    warn "真机安装: Xcode → gen/apple 工程 → 两个 target 选 Team → 或使用 altstore/自签工具安装未签名 ipa 需自有证书"
  fi
  warn "若构建在签名一步失败: 用 Xcode 打开 src-tauri/gen/apple/CC-Flash.xcworkspace,"
  warn "为 CC-Flash 与 CC-Flash_iOS 两个 target 设置 Signing Team 后重新执行。"
}

# ---------------------------------------------------------------- main
case "$TARGET" in
  mac)     build_mac ;;
  win)     build_win ;;
  android) build_android ;;
  ios)     build_ios ;;
  all)     build_mac; build_android; build_ios; build_win ;;
  *) echo "用法: $0 [mac|win|android|ios|all] [debug]"; exit 1 ;;
esac

echo
if [[ $FAIL -ne 0 ]]; then
  die "存在失败项, 请回看上方日志"
  exit 1
elif [[ $WIN_SKIPPED -eq 1 && "$TARGET" == "all" ]]; then
  # all 声称覆盖四端: Windows 未构建就不能打印"全部完成"(曾因此误报成功)
  warn "已完成当前平台目标; Windows 包未构建(需 Windows 或 CI, 见上方指引)"
  exit 1
else
  ok "全部完成 🎉"
fi
