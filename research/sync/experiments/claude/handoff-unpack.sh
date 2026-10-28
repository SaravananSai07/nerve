#!/usr/bin/env bash
# E7 handoff unpacker on the target device:
#   handoff-unpack.sh <pack.tar> <dest-repo-dir> <claude-config-root> [--rewrite-paths]
# Restores the repo at <dest> (clone from bundle, apply worktree patch and
# untracked files), then installs the transcript under the slug Claude derives
# from <dest>, so `claude --resume <id>` run in <dest> finds it.
set -euo pipefail
pack=$1 dest=$2 root=$3 rewrite=${4:-}
work=$(mktemp -d); tar -C "$work" -xf "$pack"
sid=$(jq -r .session_id "$work/manifest.json"); src=$(jq -r .src_cwd "$work/manifest.json")
branch=$(jq -r .branch "$work/manifest.json")
if [ ! -d "$dest/.git" ]; then git clone -q "$work/repo.bundle" "$dest"; fi
cd "$dest"
git fetch -q "$work/repo.bundle" "+refs/heads/*:refs/remotes/handoff/*"
git checkout -q -B "$branch" "handoff/$branch"
[ -s "$work/worktree.patch" ] && git apply --binary "$work/worktree.patch"
tar -xf "$work/untracked.tar"
dest=$(pwd -P)
slug=$(printf '%s' "$dest" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$root/projects/$slug"
if [ "$rewrite" = "--rewrite-paths" ]; then
  # Rewrite absolute source paths so later turns' file paths point at dest.
  jq -c --arg s "$src" --arg d "$dest" 'walk(if type=="string" then gsub($s; $d) else . end)' \
    "$work/transcript.jsonl" > "$root/projects/$slug/$sid.jsonl"
else
  cp "$work/transcript.jsonl" "$root/projects/$slug/$sid.jsonl"
fi
[ -f "$work/sidecar.tar" ] && tar -C "$root/projects/$slug" -xf "$work/sidecar.tar"
echo "installed $sid into $root/projects/$slug (src_cwd=$src -> $dest)"
git status --short
rm -rf "$work"
