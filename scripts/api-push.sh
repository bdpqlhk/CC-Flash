#!/usr/bin/env bash
# 应急: github.com:443 不可用时, 经 api.github.com 把本地领先 main 的提交推上去, 并重建指定标签
# 用法: bash scripts/api-push.sh <标签名>   (在仓库根目录执行; 提交信息取 .git-commit-msg.txt 或最新提交)
set -euo pipefail
cd "$(dirname "$0")/.."
# REPO 从 gh 推导: 硬编码在 fork/改名场景会推错仓库
REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
TAG="${1:?用法: $0 <标签名>}"

BASE=$(git rev-parse origin/main)
REMOTE_HEAD=$(gh api "repos/$REPO/git/ref/heads/main" --jq .object.sha)
# API 兜底只允许“本地 HEAD 在远端 main 之上”的 fast-forward。
# 否则会把远端 main 回退到旧 tree，或把 tag 指向与本地不同的提交。
git merge-base --is-ancestor "$BASE" HEAD || {
  echo "✘ 本地 HEAD 未包含 origin/main($BASE)，拒绝用 API 推送（先 fetch/rebase）" >&2
  exit 1
}
[ "$BASE" != "$(git rev-parse HEAD)" ] || {
  echo "✘ 本地 HEAD 与 origin/main 相同，没有可推送提交" >&2
  exit 1
}
if [ "$REMOTE_HEAD" != "$BASE" ]; then
  echo "✘ 远端 main 已变化($REMOTE_HEAD)，本地 origin/main=$BASE 陈旧；先 git fetch 后重试，拒绝非 fast-forward 覆盖" >&2
  exit 1
fi
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

# 已存在 tag / 已公开 Release 时默认拒绝重写, 避免资产与 tag 代码不一致。
# 只有明确设置 CCFLASH_ALLOW_TAG_MOVE=1 才允许移动旧 tag（仍会先确认 Release 是草稿）。
if gh api "repos/$REPO/git/ref/tags/$TAG" >/dev/null 2>&1; then
  if [ "${CCFLASH_ALLOW_TAG_MOVE:-0}" != "1" ]; then
    echo "✘ tag $TAG 已存在；拒绝覆盖。请使用新版本 tag，或显式设置 CCFLASH_ALLOW_TAG_MOVE=1" >&2
    exit 1
  fi
  if RELEASE_DRAFT=$(gh api "repos/$REPO/releases/tags/$TAG" --jq .draft 2>/tmp/cc-api-push-release.err); then
    :
  elif grep -qiE 'not found|404' /tmp/cc-api-push-release.err; then
    RELEASE_DRAFT=missing
  else
    echo "✘ 无法确认 Release 状态（网络/API/权限错误），拒绝移动 tag" >&2
    cat /tmp/cc-api-push-release.err >&2
    exit 1
  fi
  if [ "$RELEASE_DRAFT" = "false" ]; then
    echo "✘ Release $TAG 已公开，拒绝移动 tag/替换线上资产" >&2
    exit 1
  fi
  gh api -X DELETE "repos/$REPO/git/refs/tags/$TAG" >/dev/null
fi
TS=$(gh api "repos/$REPO/git/tags" -f tag="$TAG" -f message="$TAG" -f object="$NEW_COMMIT" -f type=commit --jq .sha)
gh api -X POST "repos/$REPO/git/refs" -f ref="refs/tags/$TAG" -f sha="$TS" --jq .ref >/dev/null
echo "✔ 标签 $TAG 已指向 $NEW_COMMIT, 构建将自动触发"
rm -f .git-commit-msg.txt
echo "完成。本地同步前先确认没有未提交改动(有则 git stash): git fetch origin && git reset --hard origin/main && git fetch --tags --force"
