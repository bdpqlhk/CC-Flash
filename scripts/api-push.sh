#!/usr/bin/env bash
# 应急: github.com:443 不可用时, 经 api.github.com 把本地领先 main 的提交推上去, 并重建指定标签
# 用法: bash scripts/api-push.sh <标签名>   (在仓库根目录执行; 提交信息取 .git-commit-msg.txt 或最新提交)
set -euo pipefail
cd "$(dirname "$0")/.."
# REPO 从 gh 推导: 硬编码在 fork/改名场景会推错仓库
REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
TAG="${1:?用法: $0 <标签名>}"

BASE=$(git rev-parse origin/main)
BASE_TREE=$(gh api "repos/$REPO/commits/$BASE" --jq .commit.tree.sha)
MSG=$(cat .git-commit-msg.txt 2>/dev/null || git log -1 --format=%B HEAD)

# 收集变更: 逐个路径检查 HEAD 是否存在 —— 删除/改名时远端旧路径也要显式置 null,
# 不能像旧版那样 --diff-filter=d 跳过, 否则远端会残留已删除文件。
LINES=()
add_path() {
  local f="$1"
  [ -z "$f" ] && return
  if git cat-file -e "HEAD:$f" 2>/dev/null; then
    echo "上传 blob: $f"
    # 内容取 HEAD 而非工作区: 旧版 base64 -i "$f" 会把未提交改动推上远端
    local B64 SHA MODE
    B64=$(git show "HEAD:$f" | base64 | tr -d '\n')
    SHA=$(gh api "repos/$REPO/git/blobs" -f content="$B64" -f encoding=base64 --jq .sha)
    MODE=$(git ls-tree HEAD -- "$f" | awk '{print $1}')
    LINES+=("${f}"$'\t'"${MODE:-100644}"$'\t'"$SHA")
  else
    echo "删除远端 blob: $f"
    LINES+=("${f}"$'\t100644\t')
  fi
}
# --name-status 才能看到重命名/复制的旧路径: --name-only 对 R 只输出新路径,
# 旧文件会永远留在远端树里(可能含废弃配置/密钥), 与本地 HEAD 不一致
while IFS=$'\t' read -r st a b; do
  case "$st" in
    R*|C*) add_path "$a"; add_path "$b" ;;
    D*)    add_path "$a" ;;
    *)     add_path "$a" ;;
  esac
done < <(git diff --name-status "$BASE" HEAD)
if [ ${#LINES[@]} -eq 0 ]; then echo "本地与远程无差异, 无需推送"; exit 0; fi

TREE_JSON=$(printf '%s\n' "${LINES[@]}" | BASE_TREE="$BASE_TREE" node -e '
const fs = require("fs");
const lines = fs.readFileSync(0, "utf8").split("\n").filter(Boolean);
const tree = lines.map(l => {
  const [path, mode, sha] = l.split("\t");
  return { path, mode: mode || "100644", type: "blob", sha: sha || null };
});
process.stdout.write(JSON.stringify({ base_tree: process.env.BASE_TREE, tree }));
')
NEW_TREE=$(gh api "repos/$REPO/git/trees" --input - --jq .sha <<< "$TREE_JSON")
# 树一致性校验: 重命名/删除漏项会让远端树与本地 HEAD 不同, 必须中止而不是继续打 tag
LOCAL_TREE=$(git rev-parse 'HEAD^{tree}')
if [ "$NEW_TREE" != "$LOCAL_TREE" ]; then
  echo "✘ 远端树($NEW_TREE)与本地 HEAD 树($LOCAL_TREE)不一致, 已中止; 请检查重命名/删除处理"
  exit 1
fi
NEW_COMMIT=$(gh api "repos/$REPO/git/commits" -f message="$MSG" -f tree="$NEW_TREE" -f "parents[]=$BASE" --jq .sha)
gh api -X PATCH "repos/$REPO/git/refs/heads/main" -f sha="$NEW_COMMIT" --jq .object.sha >/dev/null
echo "✔ main 已通过 API 更新: $NEW_COMMIT"

# 远程同名标签若存在则删除后重建, 指向新提交
gh api -X DELETE "repos/$REPO/git/refs/tags/$TAG" >/dev/null 2>&1 || true
TS=$(gh api "repos/$REPO/git/tags" -f tag="$TAG" -f message="$TAG" -f object="$NEW_COMMIT" -f type=commit --jq .sha)
gh api -X POST "repos/$REPO/git/refs" -f ref="refs/tags/$TAG" -f sha="$TS" --jq .ref >/dev/null
echo "✔ 标签 $TAG 已指向 $NEW_COMMIT, 构建将自动触发"
rm -f .git-commit-msg.txt
echo "完成。本地同步前先确认没有未提交改动(有则 git stash): git fetch origin && git reset --hard origin/main && git fetch --tags --force"
