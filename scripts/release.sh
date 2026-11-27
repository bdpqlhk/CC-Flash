#!/usr/bin/env bash
# =====================================================================
# CC-Flash · 一键发版脚本
#
# 用法:
#   ./scripts/release.sh <版本号> [提交信息]
#   例: ./scripts/release.sh 1.2.0
#       ./scripts/release.sh 1.2.0 "feat: 新增xxx"
#       ./scripts/release.sh 1.2.1.1 "fix: xxx"   # 四段: 完整四段作 tag, 应用版本取前三段
#
# 流程:
#   升版本(Cargo.toml 唯一来源, 自动刷新 Cargo.lock)
#   → 提交全部改动 → 打 v* 标签 → 推送
#   → 触发 GitHub Actions 自动构建 macOS/Windows/Android 并挂 Release
#
# 网络兜底: git 推送标签失败时自动改走 GitHub API 建 tag 引用(等效触发);
#           推送均带 3 次重试。iOS 真机包需开发者账号(workflow 内有模板)。
# =====================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="${1:-}"
MSG="${2:-}"

# 四段版本 X.Y.Z.W 的第四段是同一应用版本内的迭代构建号:
# git tag 用完整四段(v1.2.1.1), 应用版本取前三段(1.2.1) ——
# Cargo 只接受 semver, Cargo.toml 里写 1.2.1.1 会直接构建失败, 故必须拆分。
TAG_VERSION="$VERSION"
APP_VERSION="$VERSION"
if [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  APP_VERSION="${VERSION%.*}"
fi

# ---------- 前置检查 ----------
[ -z "$VERSION" ] && { echo "用法: $0 <版本号 X.Y.Z 或 X.Y.Z.W> [提交信息]"; exit 1; }
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || { echo "✘ 版本号需为 X.Y.Z 或 X.Y.Z.W 格式: $VERSION"; exit 1; }
command -v gh >/dev/null 2>&1 || { echo "✘ 缺少 gh CLI (https://cli.github.com)"; exit 1; }
# 远端已有同名 tag 时默认拒绝: 重发会覆盖已公开 Release 的资产指向,
# 造成"Release 资产 ≠ tag 代码"。确认安全后用 FORCE_REPUBLISH=1 显式放行。
remote_tag=$(git ls-remote --tags origin "refs/tags/v$TAG_VERSION" 2>/dev/null || true)
if [ -n "$remote_tag" ]; then
  if [ "${FORCE_REPUBLISH:-0}" != "1" ]; then
    echo "✘ 远端已存在 tag v$TAG_VERSION"
    echo "  重发会覆盖已发布产物；确认该 tag 尚无公开 Release 后可设置 FORCE_REPUBLISH=1 重跑。"
    exit 1
  fi
  # 重发必须重建 tag 才能重新触发 Actions: 已公开的 Release 绝不允许移动 tag。
  # fail-closed: 网络/权限/API 错误不能等同于"未公开"；只有明确读到 draft=true
  # 或确认 Release 不存在时才允许移动 tag。
  draft_err="$(mktemp)"
  if draft_state=$(gh release view "v$TAG_VERSION" --json isDraft -q .isDraft 2>"$draft_err"); then
    if [ "$draft_state" = "false" ]; then
      echo "✘ Release v$TAG_VERSION 已公开，拒绝删除/移动 tag（资产会与代码错位）"
      rm -f "$draft_err"
      exit 1
    fi
  else
    if ! grep -qiE 'not found|404' "$draft_err"; then
      echo "✘ 无法确认 Release v$TAG_VERSION 状态（网络/API/权限错误），拒绝移动 tag"
      cat "$draft_err"
      rm -f "$draft_err"
      exit 1
    fi
  fi
  rm -f "$draft_err"
  FORCE_TAG=1
  echo "⚠ FORCE_REPUBLISH=1: 允许重发 v${TAG_VERSION}（稍后删除远端旧 tag）"
fi

BRANCH="$(git branch --show-current)"
# detached HEAD 下 BRANCH 为空, 尾部 git push origin "" 必失败且报错无指向 —— 提前拦
[ -n "$BRANCH" ] || { echo "✘ 当前处于 detached HEAD, 无法确定推送分支; 请先 git checkout main"; exit 1; }
[ "$BRANCH" = "main" ] || echo "⚠ 当前分支是 $BRANCH (推荐在 main 上发版)"

REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)
[ -z "$REPO" ] && REPO=$(git remote get-url origin | sed -E 's#.*github.com[:/]##; s#\.git$##')
echo "仓库: $REPO"

CUR=$(grep -m1 '^version' src-tauri/Cargo.toml | sed 's/[^0-9.]//g' || true)
[ -n "$CUR" ] || { echo "✘ 无法从 Cargo.toml 读取版本号"; exit 1; }
# 只拦"连 tag 都没变"的三段写法; 四段 tag(如 1.2.1.1)可以复用当前应用版本,
# 但此时必须有代码改动可供提交(下面 staged 为空会中止), 避免打空标签。
if [ "$TAG_VERSION" = "$CUR" ] && [ "${FORCE_REPUBLISH:-0}" != "1" ]; then
  echo "✘ 版本号未变化(仍是 $CUR), 请传新版本号；重发同一版本请设置 FORCE_REPUBLISH=1"
  exit 1
fi
echo "应用版本: $CUR → ${APP_VERSION}, tag: v$TAG_VERSION"

# 失败回滚: 版本文件改完但尚未提交时出错(缺 cargo/hook 拒绝/网络中断), 恢复原版本,
# 否则工作区停在"已改版未提交", 重跑会因"版本号未变化"直接退出, 发版流程卡死
ORIG_VERSION="$CUR"
VERSION_CHANGED=0
COMMITTED=0
STAGED=0
LOCK_BAK_DIR="$(mktemp -d)"
cp package-lock.json "$LOCK_BAK_DIR/package-lock.json" 2>/dev/null || true
cp src-tauri/Cargo.lock "$LOCK_BAK_DIR/Cargo.lock" 2>/dev/null || true
restore_on_fail() {
  local rc=$?
  if [ "$rc" -ne 0 ] && [ "$VERSION_CHANGED" = "1" ] && [ "$COMMITTED" = "0" ]; then
    echo "↩ 发版中断, 回滚版本文件到 $ORIG_VERSION"
    sed -i.bak "s/^version = \".*\"/version = \"$ORIG_VERSION\"/" src-tauri/Cargo.toml && rm -f src-tauri/Cargo.toml.bak
    sed -i.bak "s/APP_VERSION_FALLBACK = \".*\"/APP_VERSION_FALLBACK = \"$ORIG_VERSION\"/" src/config.js && rm -f src/config.js.bak
    sed -i.bak "s/\"version\": \".*\"/\"version\": \"$ORIG_VERSION\"/" package.json && rm -f package.json.bak
    if [ -f "$LOCK_BAK_DIR/package-lock.json" ]; then cp "$LOCK_BAK_DIR/package-lock.json" package-lock.json; fi
    if [ -f "$LOCK_BAK_DIR/Cargo.lock" ]; then cp "$LOCK_BAK_DIR/Cargo.lock" src-tauri/Cargo.lock; fi
    if [ -n "${OLD_VCODE:-}" ]; then
      node -e "const fs=require('fs');const f='src-tauri/tauri.conf.json';const j=JSON.parse(fs.readFileSync(f,'utf8'));j.bundle=j.bundle||{};j.bundle.android=Object.assign({},j.bundle.android,{versionCode:Number(process.argv[1])});fs.writeFileSync(f,JSON.stringify(j,null,2)+'\n')" "$OLD_VCODE" || true
    fi
    if [ -n "${OLD_BVER:-}" ]; then
      node -e "const fs=require('fs');const f='src-tauri/tauri.conf.json';const j=JSON.parse(fs.readFileSync(f,'utf8'));j.bundle=j.bundle||{};j.bundle.macOS=Object.assign({},j.bundle.macOS,{bundleVersion:process.argv[1]});fs.writeFileSync(f,JSON.stringify(j,null,2)+'\n')" "$OLD_BVER" || true
    fi
  fi
  # 版本文件已回写原值但 index 还是 add -A 后的暂存态: 撤暂存, 防止后续
  # 手动 git commit 把半套版本改动(或守卫漏网文件)提交进 Public 仓库
  if [ "$STAGED" = "1" ] && [ "$COMMITTED" = "0" ]; then
    git reset -q HEAD 2>/dev/null || true
  fi
  rm -rf "$LOCK_BAK_DIR" 2>/dev/null || true
  return $rc
}
trap restore_on_fail EXIT

# ---------- 1) 升版本 ----------
OLD_VCODE=$(node -e "try{const j=require('./src-tauri/tauri.conf.json');console.log(j.bundle&&j.bundle.android&&j.bundle.android.versionCode||'')}catch(e){}" 2>/dev/null || true)
OLD_BVER=$(node -e "try{const j=require('./src-tauri/tauri.conf.json');console.log(j.bundle&&j.bundle.macOS&&j.bundle.macOS.bundleVersion||'')}catch(e){}" 2>/dev/null || true)
# 一旦开始改版本文件, 后续任何失败(缺 cargo/hook 拒绝/网络中断)都由 trap 回滚
VERSION_CHANGED=1
# -i.bak 双平台兼容(macOS BSD sed 需后缀参数, GNU sed 同样接受)
sed -i.bak "s/^version = \".*\"/version = \"$APP_VERSION\"/" src-tauri/Cargo.toml
rm -f src-tauri/Cargo.toml.bak
# 同步前端兜底版本与 package.json —— APP_VERSION_FALLBACK 与 Cargo.toml 脱同步会
# 让更新检查在 invoke 未返回时拿 1.0.0 参与比较, 误弹"发现新版本"
sed -i.bak "s/APP_VERSION_FALLBACK = \".*\"/APP_VERSION_FALLBACK = \"$APP_VERSION\"/" src/config.js
rm -f src/config.js.bak
# package.json 仅一处 "version"(无 dependencies 内嵌同名字段), 全局替换安全且 BSD/GNU sed 通用
sed -i.bak "s/\"version\": \".*\"/\"version\": \"$APP_VERSION\"/" package.json
rm -f package.json.bak
# Android versionCode: 四段迭代(z 段)必须递增, 否则系统/市场把新包当同版本拒绝覆盖。
# 映射 major*1e7 + minor*1e5 + patch*1e2 + build, 上限 2.1e9。
IFS=. read -r vM vN vP vB <<< "$VERSION"
vB=${vB:-0}
# 先按十进制归一(10#): 直接对 "08" 做算术比较会报 value too great for base, 错误信息会误导
for part in "$vM" "$vN" "$vP" "$vB"; do
  # 前导零直接拒绝: "1.08.0" 会写进 Cargo.toml 后被 cargo metadata 以
  # "invalid leading zero" 拒绝, 用户看到的是晦涩的 cargo 报错(B3-7)
  if [[ "$part" =~ ^0[0-9]+$ ]]; then
    echo "✘ 版本段不得含前导零(cargo 会拒绝): $VERSION"
    exit 1
  fi
  part=$((10#$part))
  [[ "$part" -ge 0 && "$part" -lt 100 ]] || { echo "✘ 版本段需在 0-99: $VERSION"; exit 1; }
done
NEW_VCODE=$((10#$vM * 10000000 + 10#$vN * 100000 + 10#$vP * 100 + 10#$vB))
# macOS CFBundleVersion 只允许 1-3 段整数: 四段 tag 映射为 M.N.(P*100+B),
# 如 1.2.9.2 → 1.2.902(第三段上限 99*100+99=9999, 满足 Apple 规范, 上架/公证不被拒)
BUNDLE_VER="$vM.$vN.$((10#$vP * 100 + 10#$vB))"
node -e "const fs=require('fs');const f='src-tauri/tauri.conf.json';const j=JSON.parse(fs.readFileSync(f,'utf8'));j.bundle=j.bundle||{};j.bundle.android=Object.assign({},j.bundle.android,{versionCode:Number(process.argv[1])});j.bundle.macOS=Object.assign({},j.bundle.macOS,{bundleVersion:process.argv[2]});fs.writeFileSync(f,JSON.stringify(j,null,2)+'\n')" "$NEW_VCODE" "$BUNDLE_VER"
# 同步 package-lock 根 version(两处): 勿用全局 sed(lock 里有几千行依赖 version 字段)
npm install --package-lock-only >/dev/null 2>&1 || echo "⚠ package-lock 同步失败(不阻断发版, CI 用 npm ci 严格校验 lock, 不同步会整链报红)"
cargo metadata --manifest-path src-tauri/Cargo.toml --format-version 1 >/dev/null  # 刷新 Cargo.lock
echo "Cargo.toml → $(grep -m1 '^version' src-tauri/Cargo.toml)"
echo "config.js  → $(grep -m1 'APP_VERSION_FALLBACK' src/config.js)"

# ---------- 2) 提交 ----------
[ -z "$MSG" ] && MSG="release: v$VERSION"
# 未跟踪清单必须在 git add -A 之前取: add 之后 ls-files --others 必为空
untracked=$(git ls-files --others --exclude-standard)
if [ -n "$untracked" ]; then
  echo "⚠ 以下未跟踪文件将一并提交:"
  echo "$untracked" | sed 's/^/    /'
fi
git add -A
STAGED=1
# 私钥守卫: 仓库为 Public, keystore 入库即泄露(任何人可签冒名更新包)。
# 先把清单落到变量再 grep(不带 -q): grep -q 命中大列表开头即关管道,
# git diff 吃 SIGPIPE 以 141 退出, pipefail 下整条 if 为假 —— 守卫恰好在
# "发版夹带大量改动+私钥"这种该触发的场景被绕过(已实测复现)
staged_files=$(git diff --cached --name-only)
KEY_NAME_RE='\.(keystore|jks|p12|mobileprovision|b64|p8|key|cer|der|pfx|pem|ppk|gpg|jceks|bks|p7b|p7c)$|(^|/)\.env($|\.)|(^|/)key\.properties$|(^|/)keystore\.properties$|(^|/)id_(rsa|ed25519|ecdsa|dsa)$|(^|/)\.netrc$|(^|/)\.npmrc$|(^|/)credentials$'
if echo "$staged_files" | grep -iE "$KEY_NAME_RE" >/dev/null; then
  echo "✘ 暂存区含签名私钥/环境密钥文件, 已中止(请移出仓库目录后重试)"
  echo "$staged_files" | grep -iE "$KEY_NAME_RE"
  # 命中即撤暂存: 否则文件仍在 index, 后续误执行 git commit 会把私钥提交进 Public 仓库
  git reset -q HEAD 2>/dev/null || true
  exit 1
fi
# 私钥内容守卫: 改扩展名(如把 keystore 命名成 .txt)可绕过上面的文件名检查。
# 在暂存区内容里直接扫 PEM 私钥头(仓库为 Public, 一旦推上去即泄露)
# 模式写成字符类而非 ".*": 本文件自身就含这行守卫文本, 用 .* 会把 release.sh
# 自己判成私钥文件(已实测发版被自己拦下)。[A-Z ]* 仍覆盖 RSA/EC/OPENSSH/
# ENCRYPTED 等全部标准 PEM 头, 且不会匹配到本行字面量。
# LS0tLS1CRUdJ[T]i 是 base64("-----BEGIN") 的字面量(类括号防本文件自匹配):
# base64 包裹的 PEM 私钥不会出现明文头, 但其编码必然包含这段前缀
if git grep --cached -IlE 'BEGIN [A-Z ]*PRIVATE KEY|LS0tLS1CRUdJ[T]i' >/dev/null 2>&1; then
  echo "✘ 暂存文件内容含私钥(PEM/base64 PEM), 已中止:"
  git grep --cached -IlE 'BEGIN [A-Z ]*PRIVATE KEY|LS0tLS1CRUdJ[T]i' || true
  git reset -q HEAD 2>/dev/null || true
  exit 1
fi
# 二进制 keystore 守卫: JKS 魔数 FE ED FE ED / PKCS12 DER 30 82 / base64 证书 MII
# (git grep -I 会跳过二进制, 旧守卫对改名 keystore / base64 .b64 完全无效)
# 读取前几个字节必然让 git show 收到 SIGPIPE; pipefail 下会把整个发版误判为失败,
# 这里临时关闭 pipefail, 循环结束再恢复。
set +o pipefail
while IFS= read -r f; do
  [ -z "$f" ] && continue
  magic=$(git show ":$f" 2>/dev/null | head -c 4 | od -An -tx1 | tr -d ' \n')
  case "$magic" in
    feedfeed|3082*)
      echo "✘ 暂存文件 $f 是二进制 keystore/证书(PKCS12/JKS 魔数), 已中止"
      git reset -q HEAD 2>/dev/null || true
      exit 1
      ;;
  esac
  first=$(git show ":$f" 2>/dev/null | head -c 8)
  case "$first" in
    MII*)
      echo "✘ 暂存文件 $f 以 MII 开头, 疑似 base64 证书/keystore, 已中止"
      git reset -q HEAD 2>/dev/null || true
      exit 1
      ;;
  esac
  # base64 编码的 JKS: 文件是纯 ASCII, 以 /u3+7Q== 开头, 扩展名/裸魔数/MII
  # 都匹配不到。取前 8 个 base64 字符解码, 检查 FEEDFEED 魔数。
  # 暂存区含 PNG/ICO 等二进制文件: 必须用 C locale 逐字节处理, 否则 macOS 的
  # tr 会在非 UTF-8 字节上报 "Illegal byte sequence" 并中止整个发版(已实测触发)。
  head_b64=$(git show ":$f" 2>/dev/null | sed '/^[[:space:]]*#/d' | head -c 8192 | LC_ALL=C tr -d ' \t\r\n')
  if [ "${#head_b64}" -ge 512 ]; then
    case "$head_b64" in
      *[!A-Za-z0-9+/=]*) ;;
      *)
        b64magic=$(printf '%s' "${head_b64:0:8}" | base64 -d 2>/dev/null | head -c 4 | od -An -tx1 | tr -d ' \n')
        case "$b64magic" in
          feedfeed)
            echo "✘ 暂存文件 $f 是 base64 编码的 JKS keystore, 已中止"
            git reset -q HEAD 2>/dev/null || true
            exit 1
            ;;
        esac
        ;;
    esac
  fi
done <<< "$staged_files"
set -o pipefail
# 发版提交会带走全部改动(git add -A): 明示数量, 避免"一锅端"夹带
echo "暂存文件: $(echo "$staged_files" | grep -c . || true) 个"
# 注意: workflow 改动必须经由本脚本提交进 tag —— 手打 tag 会用旧版工作流构建
if git diff --cached --quiet; then
  echo "⚠ 暂无改动可提交(版本号与当前一致且工作树干净, FORCE 也无法凭空造 commit)"
  echo "  同 tag 重建重发可手动:"
  echo "    git tag v$TAG_VERSION"
  echo "    git push origin :refs/tags/v$TAG_VERSION && git push origin v$TAG_VERSION"
  echo "  (先确认 Release 仍为草稿; 或随便提交一处改动后重跑本脚本)"
  exit 1
fi
# commit 失败必须立即退出: 否则后续会把新版本 tag 打在旧代码上发出去
if ! git commit -m "$MSG"; then
  echo "✘ git commit 失败(检查 identity 配置/hook), 已中止 —— 不给旧代码打新版本标签"
  # 注意: 此处 COMMITTED 仍为 0, EXIT trap 会回滚版本文件, 可直接重跑

  exit 1
fi
COMMITTED=1   # 已提交: 后续失败不再回滚版本文件(否则工作区与 HEAD 不一致)

# ---------- 3) 打标签 ----------
git tag -d "v$TAG_VERSION" >/dev/null 2>&1 || true   # 清理同名本地标签(重发场景; 不存在属正常)
git tag -a "v$TAG_VERSION" -m "v$TAG_VERSION"

# ---------- 4) 推送(带重试) ----------
push_retry() {
  local ref="$1" n
  for n in 1 2 3; do
    echo "推送 $ref (第 $n 次)..."
    # 直接用 git push 退出码判定: 经 grep 过滤 "Everything up-to-date" 时,
    # push 实际成功但 grep 退出 1, pipefail 下整条管道误判失败 → 走 fallback
    # 删远端 tag 重推, 重复触发 workflow 并发上传同名资产
    if git push origin "$ref"; then
      return 0
    fi
    echo "  失败, 10 秒后重试..."
    sleep 10
  done
  return 1
}

# 推当前分支(而非硬编码 main): 在特性分支发版时不会把 main 误推到远端
push_retry "$BRANCH" || { echo "✘ $BRANCH 推送失败, 网络恢复后手动执行: git push origin $BRANCH"; exit 1; }

if [ "${FORCE_TAG:-0}" = "1" ]; then
  echo "删除远端旧 tag v${TAG_VERSION}，以重新触发构建..."
  git push origin ":refs/tags/v$TAG_VERSION" >/dev/null 2>&1 \
    || gh api -X DELETE "repos/$REPO/git/refs/tags/v$TAG_VERSION" >/dev/null 2>&1 \
    || { echo "✘ 删除远端旧 tag 失败，请手动处理"; exit 1; }
  # 删后复核: 输出被吞掉时"假成功"会让后续 push 全程 already exists
  # (2026-11-27 v1.3.0 重发实测踩坑), 这里 fail-loud
  if git ls-remote --tags origin "refs/tags/v$TAG_VERSION" | grep -q .; then
    echo "✘ 远端 tag v$TAG_VERSION 仍存在(删除未生效), 中止以免打出错位 tag"
    exit 1
  fi
fi
if push_retry "v$TAG_VERSION"; then
  echo "✔ 标签已推送, 应已触发构建"
else
  # 不再自动删除远端 tag: 已发布 Release 的资产可能仍指向旧提交, 删 tag 重推
  # 会造成"资产 ≠ tag 代码"。改为尝试直接创建引用; 已存在则明确失败。
  echo "git 通道失败 → 不删除远端旧标签, 尝试创建远程引用..."
  SHA=$(git rev-parse "v$TAG_VERSION")
  gh api -X POST "repos/$REPO/git/refs" -f ref="refs/tags/v$TAG_VERSION" -f sha="$SHA" >/dev/null \
    && echo "✔ 已通过 API 创建远程标签引用" \
    || { echo "✘ 标签创建失败(可能已存在)。为避免覆盖已发布 Release, 未删除远端标签。网络恢复后手动: git push origin v$TAG_VERSION"; exit 1; }
fi

# ---------- 5) 验证 Actions 触发 ----------
echo
echo "等待 Actions 触发(最多 60 秒)..."
SHA=$(git rev-parse HEAD)
RUN_URL=""
for _ in $(seq 1 12); do
  RUN_URL=$(gh api "repos/$REPO/actions/runs?head_sha=$SHA" --jq '.workflow_runs[0].html_url // empty' 2>/dev/null || true)
  [ -n "$RUN_URL" ] && break
  sleep 5
done

echo
if [ -n "$RUN_URL" ]; then
  echo "🚀 构建已触发: $RUN_URL"
  echo "   预计 15~25 分钟后, v$TAG_VERSION Release 自动挂上 macOS/Windows/Android 产物"
else
  echo "⚠ 未检测到构建触发, 请到 Actions 页确认: https://github.com/$REPO/actions"
  echo "  (若没有运行记录: 检查 Settings → Actions → General 是否 Allow all actions)"
fi
echo
echo "提示: ① iOS 真机包需 Apple 开发者账号(workflow 内有注释模板)"
echo "      ② 仓库为 Public 时, 版本更新检查与对外下载才生效"
