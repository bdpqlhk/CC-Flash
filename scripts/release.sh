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

BRANCH="$(git branch --show-current)"
[ "$BRANCH" = "main" ] || echo "⚠ 当前分支是 $BRANCH (推荐在 main 上发版)"

REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)
[ -z "$REPO" ] && REPO=$(git remote get-url origin | sed -E 's#.*github.com[:/]##; s#\.git$##')
echo "仓库: $REPO"

CUR=$(grep -m1 '^version' src-tauri/Cargo.toml | sed 's/[^0-9.]//g' || true)
[ -n "$CUR" ] || { echo "✘ 无法从 Cargo.toml 读取版本号"; exit 1; }
# 只拦"连 tag 都没变"的三段写法; 四段 tag(如 1.2.1.1)可以复用当前应用版本,
# 但此时必须有代码改动可供提交(下面 staged 为空会中止), 避免打空标签。
if [ "$TAG_VERSION" = "$CUR" ]; then echo "✘ 版本号未变化(仍是 $CUR), 请传新版本号"; exit 1; fi
echo "应用版本: $CUR → ${APP_VERSION}, tag: v$TAG_VERSION"

# ---------- 1) 升版本 ----------
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
# 同步 package-lock 根 version(两处): 勿用全局 sed(lock 里有几千行依赖 version 字段)
npm install --package-lock-only >/dev/null 2>&1 || echo "⚠ package-lock 同步失败(不阻断发版, CI 用 npm install)"
cargo metadata --manifest-path src-tauri/Cargo.toml --format-version 1 >/dev/null  # 刷新 Cargo.lock
echo "Cargo.toml → $(grep -m1 '^version' src-tauri/Cargo.toml)"
echo "config.js  → $(grep -m1 'APP_VERSION_FALLBACK' src/config.js)"

# ---------- 2) 提交 ----------
[ -z "$MSG" ] && MSG="release: v$TAG_VERSION"
git add -A
# 私钥守卫: 仓库为 Public, keystore 入库即泄露(任何人可签冒名更新包)。
# 先把清单落到变量再 grep(不带 -q): grep -q 命中大列表开头即关管道,
# git diff 吃 SIGPIPE 以 141 退出, pipefail 下整条 if 为假 —— 守卫恰好在
# "发版夹带大量改动+私钥"这种该触发的场景被绕过(已实测复现)
staged_files=$(git diff --cached --name-only)
if echo "$staged_files" | grep -iE '\.(keystore|jks|p12|mobileprovision)$' >/dev/null; then
  echo "✘ 暂存区含签名私钥文件, 已中止(请移出仓库目录并从暂存区移除)"
  echo "$staged_files" | grep -iE '\.(keystore|jks|p12|mobileprovision)$'
  exit 1
fi
# 私钥内容守卫: 改扩展名(如把 keystore 命名成 .txt)可绕过上面的文件名检查。
# 在暂存区内容里直接扫 PEM 私钥头(仓库为 Public, 一旦推上去即泄露)
if git grep --cached -IlE 'BEGIN (RSA |EC |OPENSSH |DSA )?PRIVATE KEY' >/dev/null 2>&1; then
  echo "✘ 暂存文件内容含私钥(PEM PRIVATE KEY), 已中止:"
  git grep --cached -IlE 'BEGIN (RSA |EC |OPENSSH |DSA )?PRIVATE KEY' || true
  exit 1
fi
# 发版提交会带走全部改动(git add -A): 明示数量并列出未跟踪文件, 避免"一锅端"夹带
echo "暂存文件: $(echo "$staged_files" | grep -c . || true) 个"
untracked=$(git ls-files --others --exclude-standard)
if [ -n "$untracked" ]; then
  echo "⚠ 以下未跟踪文件将一并提交:"
  echo "$untracked" | sed 's/^/    /'
fi
# 注意: workflow 改动必须经由本脚本提交进 tag —— 手打 tag 会用旧版工作流构建
if git diff --cached --quiet; then
  echo "⚠ 暂无改动可提交(检查版本号是否真的变了)"
  exit 1
fi
# commit 失败必须立即退出: 否则后续会把新版本 tag 打在旧代码上发出去
if ! git commit -m "$MSG"; then
  echo "✘ git commit 失败(检查 identity 配置/hook), 已中止 —— 不给旧代码打新版本标签"
  exit 1
fi

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

if push_retry "v$TAG_VERSION"; then
  echo "✔ 标签已推送, 应已触发构建"
else
  echo "git 通道失败 → 清理远程旧标签后重推 / 再不行走 API 建引用(等效触发)..."
  gh api -X DELETE "repos/$REPO/git/refs/tags/v$TAG_VERSION" >/dev/null 2>&1 || true  # 远端无同名 tag 属正常
  if git push origin "v$TAG_VERSION" 2>&1; then
    echo "✔ 清理后重推成功"
  else
    SHA=$(git rev-parse "v$TAG_VERSION")
    gh api -X POST "repos/$REPO/git/refs" -f ref="refs/tags/v$TAG_VERSION" -f sha="$SHA" >/dev/null \
      && echo "✔ 已通过 API 创建远程标签引用" \
      || { echo "✘ 全部通道失败, 网络恢复后手动: git push origin v$TAG_VERSION"; exit 1; }
  fi
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
