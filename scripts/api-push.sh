#!/usr/bin/env bash
# 应急: github.com:443 不可用时, 经 api.github.com 把本地领先 main 的提交推上去, 并重建指定标签
# 用法: bash scripts/api-push.sh <标签名>   (在仓库根目录执行; 提交信息取 .git-commit-msg.txt 或最新提交)
set -euo pipefail
REPO=bdpqlhk/CC-Flash
TAG="${1:?用法: $0 <标签名>}"
cd "$(dirname "$0")/.."

BASE=$(git rev-parse origin/main)
BASE_TREE=$(gh api "repos/$REPO/commits/$BASE" --jq .commit.tree.sha)
MSG=$(cat .git-commit-msg.txt 2>/dev/null || git log -1 --format=%B HEAD)

ARGS=(-f base_tree="$BASE_TREE")
while IFS= read -r f; do
  [ -z "$f" ] && continue
  echo "上传 blob: $f"
  B64=$(base64 -i "$f" | tr -d '\n')
  SHA=$(gh api "repos/$REPO/git/blobs" -f content="$B64" -f encoding=base64 --jq .sha)
  ARGS+=(-f "tree[][path]=$f" -f "tree[][mode]=100644" -f "tree[][type]=blob" -f "tree[][sha]=$SHA")
done < <(git diff --name-only "$BASE" HEAD)
if [ ${#ARGS[@]} -le 1 ]; then echo "本地与远程无差异, 无需推送"; exit 0; fi

NEW_TREE=$(gh api "repos/$REPO/git/trees" "${ARGS[@]}" --jq .sha)
NEW_COMMIT=$(gh api "repos/$REPO/git/commits" -f message="$MSG" -f tree="$NEW_TREE" -f "parents[]=$BASE" --jq .sha)
gh api -X PATCH "repos/$REPO/git/refs/heads/main" -f sha="$NEW_COMMIT" --jq .object.sha >/dev/null
echo "✔ main 已通过 API 更新: $NEW_COMMIT"

# 远程同名标签若存在则删除后重建, 指向新提交
gh api -X DELETE "repos/$REPO/git/refs/tags/$TAG" >/dev/null 2>&1 || true
TS=$(gh api "repos/$REPO/git/tags" -f tag="$TAG" -f message="$TAG" -f object="$NEW_COMMIT" -f type=commit --jq .sha)
gh api -X POST "repos/$REPO/git/refs" -f ref="refs/tags/$TAG" -f sha="$TS" --jq .ref >/dev/null
echo "✔ 标签 $TAG 已指向 $NEW_COMMIT, 构建将自动触发"
rm -f .git-commit-msg.txt
echo "完成。本地稍后同步: git fetch origin && git reset --hard origin/main && git fetch --tags --force"
