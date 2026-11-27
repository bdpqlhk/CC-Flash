# =====================================================================
# CC-Flash · Windows 打包脚本
# 用法(在 Windows 机器上, 项目根目录):
#   powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1
#
# 产物(本机默认全 bundle; 需 WiX 才出 msi):
#   src-tauri\target\release\bundle\nsis\*-setup.exe   (NSIS 安装程序 —— Release 只发这个)
#   src-tauri\target\release\bundle\msi\*.msi          (MSI 安装包 —— 仅本地产物, CI 不打包不上传)
#   src-tauri\target\release\CC-Flash.exe              (绿色单文件)
#
# 前置条件:
#   1. Node.js 18+            https://nodejs.org
#   2. Rust (rustup, MSVC 工具链)   https://rustup.rs
#   3. Visual Studio Build Tools —— 勾选"使用 C++ 的桌面开发"工作负载
#   4. WebView2 —— Windows 10/11 一般自带
#   首次构建若提示缺少 Windows 目标:
#       rustup target add x86_64-pc-windows-msvc
# =====================================================================
$ErrorActionPreference = "Stop"

function Step($m)  { Write-Host "`n==== $m ====" -ForegroundColor Blue }
function Ok($m)    { Write-Host "OK  $m" -ForegroundColor Green }
function Warn($m)  { Write-Host "WARN  $m" -ForegroundColor Yellow }

# 进入项目根(脚本位于 scripts\ 下)
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

Step "环境检查"
foreach ($cmd in @("node", "npm", "cargo")) {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
        Write-Host "缺少: $cmd  —— 见脚本头部前置条件" -ForegroundColor Red
        exit 1
    }
    Ok "已安装: $cmd"
}

# Rust 目标(MSVC)
$targets = & rustup target list --installed
if ($targets -notcontains "x86_64-pc-windows-msvc") {
    Warn "添加 Rust 目标: rustup target add x86_64-pc-windows-msvc"
    rustup target add x86_64-pc-windows-msvc
}

Step "构建前端 (vite -> dist\)"
npm run build
if ($LASTEXITCODE -ne 0) { Write-Host "前端构建失败" -ForegroundColor Red; exit 1 }

Step "Tauri 打包 (NSIS; 与 Release 口径一致)"
# 显式只打 NSIS: 缺 WiX 时 MSI 失败会把已产出的 NSIS 误判成整体失败(B3-9)
npm run tauri build -- --bundles nsis
if ($LASTEXITCODE -ne 0) { Write-Host "Tauri 打包失败" -ForegroundColor Red; exit 1 }

Step "Tauri 打包 (MSI, 可选, 需 WiX)"
npm run tauri build -- --bundles msi
if ($LASTEXITCODE -ne 0) { Write-Host "MSI 打包失败(可选, 已忽略; NSIS 产物完好)" -ForegroundColor Yellow }

Step "完成"
Ok "产物目录: src-tauri\target\release\bundle\"
Get-ChildItem -Recurse "$Root\src-tauri\target\release\bundle" -Include *.exe,*.msi |
    ForEach-Object { Ok "  $($_.FullName)" }
