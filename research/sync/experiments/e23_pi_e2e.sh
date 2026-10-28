#!/usr/bin/env bash
# E23-E26: the same flows between two real machines, public relays only
# (relay.primal.net + relay.snort.social), no Tailscale relay, no VPN path.
#
#   E23  `nerve setup` on the Mac prints a code; `nerve setup <code>` on the
#        Pi joins, and installs the agent as a systemd user service
#   E24  a prompt from the Mac reaches a Claude session on the Pi via its mod
#   E25  a permission prompt on the Pi answered from the Mac
#   E26  the Pi hands its session to the Mac, twice (Pi → Mac had never been
#        run); the second transfer is incremental
#
# Throwaway config dirs on both sides; the Pi's test service is removed at
# the end; neither machine's real Claude config is touched (the mod is
# loaded with --plugin-dir). Needs: SSH to the Pi (PI=user@host, PI_KEY),
# nerve built at ~/nerve-src on the Pi. Costs a few cents of Haiku.
set -uo pipefail
repo="$(cd "$(dirname "$0")/../../.." && pwd)"
nerve="$repo/target/release/nerve"
PI="${PI:-sspi@100.73.26.46}"
PI_KEY="${PI_KEY:-$HOME/.ssh/m5_sspi}"
mac="${TMPDIR:-/tmp}/nerve-e23"
pidir='$HOME/nerve-e23'
pinerve='$HOME/nerve-src/target/release/nerve'
relays=(--relay wss://relay.primal.net --relay wss://relay.snort.social)
pi() { ssh -o BatchMode=yes -o ControlMaster=auto -o ControlPath=/tmp/ssh-e23-%r@%h -o ControlPersist=600 -i "$PI_KEY" "$PI" "export PATH=\$HOME/.local/bin:\$HOME/.cargo/bin:\$PATH NERVE_CONFIG_DIR=$pidir; $*"; }
m() { NERVE_CONFIG_DIR="$mac" "$nerve" "$@"; }
say() { printf '\n=== %s\n' "$*"; }
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
  pi "tmux kill-session -t e23 2>/dev/null; u=\$(ls ~/.config/systemd/user | grep '^nerve-sync\\..*\\.service' | head -1); [ -n \"\$u\" ] && { systemctl --user disable --now \"\$u\" >/dev/null 2>&1; rm -f ~/.config/systemd/user/\$u; systemctl --user daemon-reload; echo \"removed test service \$u\"; }; true"
  tmux ls 2>/dev/null | awk -F: '/^nerve-/{print $1}' | while read -r s; do
    tmux show-environment -t "$s" NERVE_CONFIG_DIR 2>/dev/null | grep -q "$mac" && tmux kill-session -t "$s"
  done
}
trap cleanup EXIT

rm -rf "$mac"; mkdir -p "$mac/handoff"
pi "rm -rf $pidir ~/nerve-e23-work; mkdir -p ~/nerve-e23-work"

say "E23 setup on the Mac, join from the Pi"
m setup --name mac "${relays[@]}" --no-service --no-claude --no-wait > "$mac/setup.txt"
grep -v '█\|▀\|▄\|^ *$' "$mac/setup.txt" | head -4
code=$(grep -o 'nerve1[A-Za-z0-9_-]*' "$mac/setup.txt" | head -1)
f="$mac/sync/fleet.toml"; { printf 'handoff_root = "%s"\n' "$mac/handoff"; cat "$f"; } >"$f.tmp" && mv "$f.tmp" "$f"
NERVE_CONFIG_DIR="$mac" "$nerve" sync agent >"$mac/agent.log" 2>&1 & pids+=($!)
sleep 4
t0=$(date +%s)
pi "$pinerve setup $code --name pi --no-claude 2>&1"
echo "join + service install took $(( $(date +%s) - t0 )) s"
sleep 5
pi "systemctl --user list-units 'nerve-sync*' --no-legend | cut -c1-80; $pinerve sync doctor 2>&1 | head -6"

say "Claude on the Pi with the nerve mod"
pi "cd ~/nerve-e23-work && git init -q -b main && for i in 1 2 3; do echo \$i > f\$i.txt; git add .; git -c user.email=t@t -c user.name=t commit -qm c\$i; done
    rm -rf ~/nerve-e23-mod && cp -R ~/nerve-src/mod/nerve ~/nerve-e23-mod
    printf '{\"socket\":\"%s\"}' \"$pidir/sync/agent.sock\" > ~/nerve-e23-mod/nerve.json
    tmux new-session -d -s e23 -x 160 -y 50 -c ~/nerve-e23-work -e NERVE_CONFIG_DIR=$pidir 'claude --plugin-dir \$HOME/nerve-e23-mod --permission-mode default --model haiku'
    sleep 8; tmux capture-pane -p -t e23 | grep -q 'trust this folder' && { tmux send-keys -t e23 Down Enter; sleep 6; }; true"
sid=""
for _ in $(seq 1 40); do
  sid=$(m sync status --json 2>/dev/null | python3 -c 'import json,sys
for d in json.load(sys.stdin):
  for s in d["sessions"]:
    if s.get("via_mod"): print(s["id"]); raise SystemExit' 2>/dev/null)
  [ -n "$sid" ] && break; sleep 2
done
echo "Pi session with the mod, seen from the Mac: ${sid:-NONE}"
[ -n "$sid" ] || { pi "tmux capture-pane -p -t e23 | tail -20"; exit 1; }

say "E24 prompt from the Mac"
t0=$(date +%s.%N)
m sync send pi "${sid:0:8}" "Run the bash command: uname -m" | tail -1
echo "send → ack: $(echo "$(date +%s.%N) - $t0" | bc) s"
sleep 12; pi "tmux capture-pane -p -t e23 | grep -v '^\\s*\$' | tail -6"

say "E25 permission on the Pi, answered from the Mac"
m sync send pi "${sid:0:8}" "Use the Write tool to write the word pi-ok into e25.txt. Nothing else." | tail -1
req=""; t0=$(date +%s)
for _ in $(seq 1 40); do req=$(m sync status 2>/dev/null | awk '/\[permission\]/{print $2; exit}'); [ -n "$req" ] && break; sleep 2; done
echo "prompt visible on the Mac after $(( $(date +%s) - t0 )) s: ${req:-NEVER}"
[ -n "$req" ] || { echo '--- Pi screen:'; pi "tmux capture-pane -p -t e23 | grep -v '^\\s*\$' | tail -12"; echo '--- Pi activity:'; pi "tail -8 $pidir/sync/activity.log"; }
t0=$(date +%s.%N)
m sync answer pi "$req" allow | tail -1
echo "answer → ack: $(echo "$(date +%s.%N) - $t0" | bc) s"
sleep 4; pi "cat ~/nerve-e23-work/e25.txt"

say "E26 hand the Pi session to the Mac, twice"
pi "cd ~/nerve-e23-work && $pinerve sync handoff ${sid:0:8} mac 2>&1 | tail -1"
for _ in $(seq 1 90); do grep -q 'resumed\|handoff failed' "$mac/agent.log" && break; sleep 2; done
grep -E 'handing over|asking for|resumed|failed' "$mac/agent.log" | tail -3
pi "grep -E 'handoff .* sent' $pidir/sync/activity.log | tail -1"
tmux ls 2>/dev/null | awk -F: '/^nerve-/{print $1}' | while read -r s; do
  tmux show-environment -t "$s" NERVE_CONFIG_DIR 2>/dev/null | grep -q "$mac" && tmux kill-session -t "$s"
done
git -C "$mac/handoff/nerve-e23-work" checkout -q -- . 2>/dev/null; git -C "$mac/handoff/nerve-e23-work" clean -qfd 2>/dev/null
m sync send pi "${sid:0:8}" "Reply with just the word AGAIN" | tail -1
sleep 10
pi "cd ~/nerve-e23-work && echo more >> f1.txt && git add -A && git -c user.email=t@t -c user.name=t commit -qm more && $pinerve sync handoff ${sid:0:8} mac 2>&1 | tail -1"
for _ in $(seq 1 90); do [ "$(grep -c 'resumed\|handoff failed' "$mac/agent.log")" -ge 2 ] && break; sleep 2; done
grep -E 'resumed|failed' "$mac/agent.log" | tail -2
pi "grep -E 'handoff .* sent' $pidir/sync/activity.log | tail -2"
echo "Mac checkout HEAD: $(git -C "$mac/handoff/nerve-e23-work" log -1 --format=%s 2>/dev/null)"
say done
