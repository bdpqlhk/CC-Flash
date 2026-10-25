#!/usr/bin/env bash
# =====================================================================
# CC-Flash · 一键发版脚本
#
# 用法:
#   ./scripts/release.sh <版本号> [提交信息]
#   例: ./scripts/release.sh 1.2.0
#       ./scripts/release.sh 1.2.0 "feat: 新增xxx"
#
# 流程:
#   升版本(Cargo.toml 唯一来源, 自动刷新 Cargo.lock)
#   → 提交全部改动 → 打 v* 标签 → 推送
#   → 触发 GitHub Actions 自动构建 macOS/Windows/Android 并挂 Release
#
# 网络兜底: git 推送标签失败时自动改走 GitHub API 建 tag 引用(等效触发);
#           推送均带 3 次重试。iOS 真机包需开发者账号(workflow 内有模板)。
# =====================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="${1:-}"
MSG="${2:-}"

# ---------- 前置检查 ----------
[ -z "$VERSION" ] && { echo "用法: $0 <版本号 X.Y.Z> [提交信息]"; exit 1; }
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "✘ 版本号需为 X.Y.Z 格式: $VERSION"; exit 1; }
command -v gh >/dev/null 2>&1 || { echo "✘ 缺少 gh CLI (https://cli.github.com)"; exit 1; }

BRANCH="$(git branch --show-current)"
[ "$BRANCH" = "main" ] || echo "⚠ 当前分支是 $BRANCH (推荐在 main 上发版)"

REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null)
[ -z "$REPO" ] && REPO=$(git remote get-url origin | sed -E 's#.*github.com[:/]##; s#\.git$##')
echo "仓库: $REPO"

CUR=$(grep -m1 '^version' src-tauri/Cargo.toml | sed 's/[^0-9.]//g')
if [ "$VERSION" = "$CUR" ]; then echo "✘ 版本号未变化(仍是 $CUR), 请传新版本号"; exit 1; fi
echo "版本: $CUR → $VERSION"

# ---------- 1) 升版本 ----------
sed -i '' "s/^version = \".*\"/version = \"$VERSION\"/" src-tauri/Cargo.toml
cargo metadata --manifest-path src-tauri/Cargo.toml --format-version 1 >/dev/null 2>&1  # 刷新 Cargo.lock
echo "Cargo.toml → $(grep -m1 '^version' src-tauri/Cargo.toml)"

# ---------- 2) 提交 ----------
[ -z "$MSG" ] && MSG="release: v$VERSION"
git add -A
if git diff --cached --quiet; then
  echo "⚠ 暂无改动可提交(检查版本号是否真的变了)"
  exit 1
fi
git commit -m "$MSG" | head -2

# ---------- 3) 打标签 ----------
git tag -d "v$VERSION" >/dev/null 2>&1   # 清理同名本地标签(重发场景)
git tag -a "v$VERSION" -m "v$VERSION"

# ---------- 4) 推送(带重试) ----------
push_retry() {
  local ref="$1" n
  for n in 1 2 3; do
    echo "推送 $ref (第 $n 次)..."
    if git push origin "$ref" 2>&1 | grep -v "^Everything up-to-date$"; then
      return 0
    fi
    echo "  失败, 10 秒后重试..."
    sleep 10
  done
  return 1
}

push_retry "main" || { echo "✘ main 推送失败, 网络恢复后手动执行: git push origin main"; exit 1; }

if push_retry "v$VERSION"; then
  echo "✔ 标签已推送, 应已触发构建"
else
  echo "git 通道失败 → 清理远程旧标签后重推 / 再不行走 API 建引用(等效触发)..."
  gh api -X DELETE "repos/$REPO/git/refs/tags/v$VERSION" >/dev/null 2>&1
  if git push origin "v$VERSION" 2>&1; then
    echo "✔ 清理后重推成功"
  else
    SHA=$(git rev-parse "v$VERSION")
    gh api -X POST "repos/$REPO/git/refs" -f ref="refs/tags/v$VERSION" -f sha="$SHA" >/dev/null \
      && echo "✔ 已通过 API 创建远程标签引用" \
      || { echo "✘ 全部通道失败, 网络恢复后手动: git push origin v$VERSION"; exit 1; }
  fi
fi

# ---------- 5) 验证 Actions 触发 ----------
echo
echo "等待 Actions 触发(最多 60 秒)..."
SHA=$(git rev-parse HEAD)
RUN_URL=""
for _ in $(seq 1 12); do
  RUN_URL=$(gh api "repos/$REPO/actions/runs?head_sha=$SHA" --jq '.workflow_runs[0].html_url // empty' 2>/dev/null)
  [ -n "$RUN_URL" ] && break
  sleep 5
done

echo
if [ -n "$RUN_URL" ]; then
  echo "🚀 构建已触发: $RUN_URL"
  echo "   预计 15~25 分钟后, v$VERSION Release 自动挂上 macOS/Windows/Android 产物"
else
  echo "⚠ 未检测到构建触发, 请到 Actions 页确认: https://github.com/$REPO/actions"
  echo "  (若没有运行记录: 检查 Settings → Actions → General 是否 Allow all actions)"
fi
echo
echo "提示: ① iOS 真机包需 Apple 开发者账号(workflow 内有注释模板)"
echo "      ② 仓库为 Public 时, 版本更新检查与对外下载才生效"
