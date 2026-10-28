#!/usr/bin/env bash
# Generic blocking hook: park the hook input in a local mailbox and wait for a
# remote answer. The sync daemon (not this script) moves requests/answers over
# the network. Usage in settings: "command": "mailbox-hook.sh <timeout-secs>"
set -u
MB=${NERVE_MAILBOX:-$HOME/.nerve-lab/mailbox}
mkdir -p "$MB/req" "$MB/resp"
in=$(cat)
id=$(date +%s%N)-$$
printf '%s' "$in" > "$MB/req/$id.json.tmp" && mv "$MB/req/$id.json.tmp" "$MB/req/$id.json"
deadline=$(( $(date +%s) + ${1:-300} ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  if [ -f "$MB/resp/$id.json" ]; then
    cat "$MB/resp/$id.json"; rm -f "$MB/resp/$id.json" "$MB/req/$id.json"; exit 0
  fi
  sleep 0.2
done
rm -f "$MB/req/$id.json"
exit 0   # timeout: no output = fall back to Claude's normal local prompt
