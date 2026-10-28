#!/usr/bin/env bash
# E25b: E25 the other way round. The Pi's Claude account hit its usage
# limit during E23-E26, so the permission-from-another-device check runs
# with the Claude session on the Mac (mod loaded) and the answer typed on
# the Pi, over public relays only.
set -uo pipefail
repo="$(cd "$(dirname "$0")/../../.." && pwd)"
nerve="$repo/target/release/nerve"
PI="${PI:-sspi@100.73.26.46}"
PI_KEY="${PI_KEY:-$HOME/.ssh/m5_sspi}"
mac="${TMPDIR:-/tmp}/nerve-e25b"
pidir='$HOME/nerve-e25b'
pinerve='$HOME/nerve-src/target/release/nerve'
relays=(--relay wss://relay.primal.net --relay wss://relay.snort.social)
pi() { ssh -o BatchMode=yes -o ControlMaster=auto -o ControlPath=/tmp/ssh-e25-%r@%h -o ControlPersist=600 -i "$PI_KEY" "$PI" "export PATH=\$HOME/.local/bin:\$HOME/.cargo/bin:\$PATH NERVE_CONFIG_DIR=$pidir; $*"; }
m() { NERVE_CONFIG_DIR="$mac" "$nerve" "$@"; }
say() { printf '\n=== %s\n' "$*"; }
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
  tmux kill-session -t e25b 2>/dev/null
  pi "[ -f $pidir/agent.pid ] && kill \$(cat $pidir/agent.pid) 2>/dev/null; true"
}
trap cleanup EXIT
rm -rf "$mac"; mkdir -p "$mac/work"; pi "rm -rf $pidir"

say "pair (Pi invites, Mac joins), agents on both"
pi "$pinerve setup --name pi ${relays[*]} --no-service --no-claude --no-wait" > "$mac/pi-setup.txt"
code=$(grep -o 'nerve1[A-Za-z0-9_-]*' "$mac/pi-setup.txt" | head -1)
pi "nohup $pinerve sync agent > $pidir/agent.out 2>&1 & echo \$! > $pidir/agent.pid"
sleep 4
m setup "$code" --name mac --no-service --no-claude | grep -E '✓|!'
NERVE_CONFIG_DIR="$mac" "$nerve" sync agent >"$mac/agent.log" 2>&1 & pids+=($!)
sleep 3

say "Claude on the Mac with the nerve mod"
cd "$mac/work" && git init -q -b main
mkdir -p "$mac/mod" && cp -R "$repo/mod/nerve" "$mac/mod/" && printf '{"socket":"%s"}' "$mac/sync/agent.sock" > "$mac/mod/nerve/nerve.json"
tmux new-session -d -s e25b -x 160 -y 50 -c "$mac/work" -e "NERVE_CONFIG_DIR=$mac" "claude --plugin-dir $mac/mod/nerve --permission-mode default --model haiku"
sleep 7; tmux capture-pane -p -t e25b | grep -q 'trust this folder' && { tmux send-keys -t e25b Down Enter; sleep 5; }
tmux send-keys -t e25b "Use the Write tool to write the word mac-ok into e25.txt. Nothing else."; sleep 0.5; tmux send-keys -t e25b C-m

say "E25b the Pi sees the prompt and answers it"
req=""; t0=$(date +%s)
for _ in $(seq 1 40); do req=$(pi "$pinerve sync status 2>/dev/null" | awk '/\[permission\]/{print $2; exit}'); [ -n "$req" ] && break; sleep 2; done
echo "visible on the Pi after $(( $(date +%s) - t0 )) s: ${req:-NEVER}"
pi "$pinerve sync status 2>/dev/null | grep -E '^\s+\?'"
echo "--- Mac screen (dialog up):"; tmux capture-pane -p -t e25b | grep -v '^\s*$' | grep -v '╌' | tail -6
t0=$(date +%s.%N)
pi "$pinerve sync answer mac $req allow 2>&1 | tail -1"
echo "answer → ack: $(echo "$(date +%s.%N) - $t0" | bc) s (includes the SSH round trip and a one-shot client on the Pi)"
sleep 4; echo "e25.txt on the Mac: $(cat "$mac/work/e25.txt" 2>/dev/null || echo MISSING)"
tmux capture-pane -p -t e25b | grep -v '^\s*$' | grep -v '╌' | tail -4
say done
