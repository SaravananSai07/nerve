#!/usr/bin/env bash
# E7 handoff packer: everything another device needs to continue a session.
#   handoff-pack.sh <repo-dir> <session-id> <claude-config-root> <out.tar>
# Contents: git bundle of all refs (+ HEAD), binary diff of the working tree
# (tracked changes), untracked files, the transcript and its sidecar dir
# (subagents/, tool-results/), and a manifest with the source cwd.
set -euo pipefail
repo=$1 sid=$2 root=$3 out=$4
work=$(mktemp -d)
cd "$repo"
git bundle create "$work/repo.bundle" --all HEAD >/dev/null 2>&1
git diff --binary HEAD > "$work/worktree.patch"
git ls-files --others --exclude-standard -z | tar --null -T - -cf "$work/untracked.tar" 2>/dev/null || tar -cf "$work/untracked.tar" -T /dev/null
t=$(ls "$root"/projects/*/"$sid".jsonl | head -1)
cp "$t" "$work/transcript.jsonl"
side="${t%.jsonl}"
[ -d "$side" ] && tar -C "$(dirname "$side")" -cf "$work/sidecar.tar" "$(basename "$side")"
jq -n --arg sid "$sid" --arg cwd "$(pwd -P)" --arg head "$(git rev-parse HEAD)" \
  --arg branch "$(git rev-parse --abbrev-ref HEAD)" --arg host "$(hostname)" \
  '{v:1, session_id:$sid, src_cwd:$cwd, head:$head, branch:$branch, src_host:$host, created:now}' > "$work/manifest.json"
tar -C "$work" -cf "$out" .
rm -rf "$work"
ls -l "$out" | awk '{print "packed", $5, "bytes"}'
