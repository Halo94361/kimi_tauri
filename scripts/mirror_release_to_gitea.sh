#!/usr/bin/env bash
# 把 GitHub 上已发布的 Windows 包镜像到本机 Gitea 的 Release。
#
# GitHub 一侧只读公开 API，经本机代理（HTTPS_PROXY）；Gitea 一侧直连
# 127.0.0.1:3000 并绕过代理。由 systemd user timer 每 15 分钟调用一次：
# 已存在的 release 与附件会跳过，下载失败不会留下空的 release，可安全重跑。
#
# 依赖：gh（已认证）、curl、python3；Gitea 访问令牌放在
# ~/.config/kimi-tauri-mirror/gitea-token（600，仓库写权限）。
set -euo pipefail

GH_REPO="Halo94361/kimi_tauri"
GITEA_API="http://127.0.0.1:3000/api/v1/repos/Halo94361/kimi_tauri"
ASSET="tauri_kimi-windows-x64.zip"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/kimi-tauri-mirror"
WORK_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/kimi-tauri-mirror"
TOKEN_FILE="$CONFIG_DIR/gitea-token"

log() { printf '%s %s\n' "$(date '+%F %T')" "$*"; }
mkdir -p "$WORK_DIR"

TOKEN=$(<"$TOKEN_FILE")
AUTH="Authorization: token $TOKEN"
CURL=(curl -sS --connect-timeout 10 --max-time 300 -H "$AUTH")

# 已存在的 release id，没有则输出空串
release_id() {
  "${CURL[@]}" "$GITEA_API/releases/tags/$1" \
    | python3 -c 'import sys,json
try: print(json.load(sys.stdin)["id"])
except Exception: print("")'
}

has_asset() {
  "${CURL[@]}" "$GITEA_API/releases/$1/assets" \
    | python3 -c 'import sys,json
print("yes" if any(a["name"] == sys.argv[1] for a in json.load(sys.stdin)) else "no")' "$ASSET"
}

# 下载 GitHub release 里的 zip 到给定目录，失败返回非 0
download() {
  gh release download "$1" --repo "$GH_REPO" --pattern "$ASSET" --dir "$2" --clobber
}

# 在 Gitea 上按 GitHub release 建同名 release，输出 release id
create_release() {
  local tag="$1" body sha payload code
  body=$(gh release view "$tag" --repo "$GH_REPO" --json body --jq '.body')
  # 让 Gitea 的 tag 指向同一 commit；标签不存在时由 Gitea 新建
  sha=$(gh api "repos/$GH_REPO/commits/$tag" --jq '.sha')
  payload=$(TAG="$tag" BODY="$body" SHA="$sha" python3 -c 'import json,os
print(json.dumps({"tag_name": os.environ["TAG"], "name": os.environ["TAG"],
                  "body": os.environ["BODY"], "target_commitish": os.environ["SHA"],
                  "draft": False, "prerelease": False}))')

  code=$("${CURL[@]}" -o "$WORK_DIR/release.json" -w '%{http_code}' \
    -X POST -H 'Content-Type: application/json' \
    -d "$payload" "$GITEA_API/releases")
  if [ "$code" = "201" ]; then
    python3 -c 'import json;print(json.load(open("'"$WORK_DIR/release.json"'"))["id"])'
    return 0
  fi
  # 并发或重跑：退回按标签查询
  local id
  id=$(release_id "$tag")
  [ -n "$id" ] || { log "$tag：创建 release 失败（HTTP $code）"; return 1; }
  echo "$id"
}

upload_asset() {
  local id="$1" file="$2" code
  code=$("${CURL[@]}" -o /dev/null -w '%{http_code}' \
    -F "attachment=@$file" "$GITEA_API/releases/$id/assets?name=$ASSET")
  [ "$code" = "201" ]
}

# 只取正式发布（跳过 draft / prerelease），从旧到新处理
mapfile -t tags < <(gh release list --repo "$GH_REPO" --limit 30 \
  --json tagName,isDraft,isPrerelease \
  --jq '.[] | select(.isDraft == false and .isPrerelease == false) | .tagName' | tac)

[ "${#tags[@]}" -gt 0 ] || { log "GitHub 上没有正式 release"; exit 0; }

for tag in "${tags[@]}"; do
  id=$(release_id "$tag")
  if [ -n "$id" ] && [ "$(has_asset "$id")" = "yes" ]; then
    continue
  fi

  dir="$WORK_DIR/$tag"
  rm -rf "$dir" && mkdir -p "$dir"
  if ! download "$tag" "$dir"; then
    log "$tag：下载 $ASSET 失败，下轮再试"
    rm -rf "$dir"
    continue
  fi

  if [ -z "$id" ]; then
    id=$(create_release "$tag") || { rm -rf "$dir"; continue; }
    log "$tag：已在 Gitea 建 release（id=$id）"
  fi

  if upload_asset "$id" "$dir/$ASSET"; then
    log "$tag：附件已上传"
  else
    log "$tag：附件上传失败，下轮补传"
  fi
  rm -rf "$dir"
done

log "同步完成"
