#!/usr/bin/env bash
# E17-E22: the mod path and the one-command setup, end to end on one Mac.
#
# Two devices (alpha, beta) with their own NERVE_CONFIG_DIR, a private relay
# on localhost, and a real interactive Claude session (Haiku) on "beta" that
# loads the nerve mod with --plugin-dir. Nothing is installed into your real
# Claude config: the mod is loaded only for the test session.
#
#   E17  nerve setup prints an invite; nerve setup <code> joins with it
#   E18  a prompt sent from alpha is submitted by the mod (idle and mid-turn)
#   E19  a permission prompt answered from alpha, 20 s after it appeared
#        (twice a mod hook's 10 s budget: the wait must not spend it)
#   E20  an AskUserQuestion answered from alpha, without away mode
#   E21  a permission answered at the keyboard is withdrawn from alpha's view
#   E22  hand-off twice: the second transfer only carries what changed
#
# Costs a few cents of Haiku. Run from the repo root:
#   research/sync/experiments/e17_mod_e2e.sh | tee research/sync/experiments/e17_mod_e2e.out
set -uo pipefail
repo="$(cd "$(dirname "$0")/../../.." && pwd)"
nerve="$repo/target/release/nerve"
# Outside the repo: Claude would otherwise ask about this repo's CLAUDE.md imports.
st="${E17_DIR:-${TMPDIR:-/tmp}/nerve-e17}"
port=7791
relay="ws://127.0.0.1:$port"
tm=e17beta
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
  tmux kill-session -t "$tm" 2>/dev/null
  tmux ls 2>/dev/null | awk -F: '/^nerve-/{print $1}' | while read -r s; do
    tmux show-environment -t "$s" NERVE_CONFIG_DIR 2>/dev/null | grep -q "$st" && tmux kill-session -t "$s"
  done
}
trap cleanup EXIT
alpha() { NERVE_CONFIG_DIR="$st/alpha" "$nerve" "$@"; }
beta() { NERVE_CONFIG_DIR="$st/beta" "$nerve" "$@"; }
# Background an agent so that $! is nerve itself, not a subshell.
bg() { local dev=$1; NERVE_CONFIG_DIR="$st/$dev" "$nerve" sync agent >"$st/$dev-agent.log" 2>&1 & pids+=($!); }
say() { printf '\n=== %s\n' "$*"; }
pane() { tmux capture-pane -p -t "$tm" | grep -v '^\s*$' | grep -v '╌' | tail -"${1:-8}"; }
wait_for() { # seconds, command...
  local n=$1; shift
  for _ in $(seq 1 $((n * 2))); do "$@" >/dev/null 2>&1 && return 0; sleep 0.5; done
  return 1
}

cargo build --quiet --release --features sync --manifest-path "$repo/Cargo.toml" || exit 1
rm -rf "$st"; mkdir -p "$st/work" "$st/beta-handoff"
"$nerve" sync serve-relay --port "$port" >"$st/relay.log" 2>&1 & pids+=($!)
until nc -z 127.0.0.1 $port 2>/dev/null; do sleep 0.2; done

say "E17 one-command setup + invite"
beta setup --name beta --relay "$relay" --no-service --no-claude --no-wait | tee "$st/beta-setup.txt" | grep -v '█\|▀\|▄'
code=$(grep -o 'nerve1[A-Za-z0-9_-]*' "$st/beta-setup.txt" | head -1)
f="$st/beta/sync/fleet.toml"; { printf 'handoff_root = "%s"\n' "$st/beta-handoff"; cat "$f"; } >"$f.tmp" && mv "$f.tmp" "$f"
bg beta
wait_for 10 test -S "$st/beta/sync/agent.sock"
t0=$(date +%s.%N)
alpha setup "$code" --name alpha --no-service --no-claude
echo "join took $(echo "$(date +%s.%N) - $t0" | bc) s"
echo "alpha peers: $(grep -A3 '^\[peers\]' "$st/alpha/sync/fleet.toml" | tail -n +2 | cut -c1-30 | tr '\n' ' ')"
sleep 6
echo "beta peers:  $(grep -A3 '^\[peers\]' "$st/beta/sync/fleet.toml" | tail -n +2 | cut -c1-30 | tr '\n' ' ')"
echo "reusing the code:"; alpha setup "$code" --no-service --no-claude 2>&1 | tail -1

say "a git repo with a Claude session on beta, loading the nerve mod"
cd "$st/work" && git init -q -b main && for i in 1 2 3; do echo "line $i" > "f$i.txt"; git add .; git -c user.email=t@t -c user.name=t commit -qm "c$i"; done
mkdir -p "$st/mod" && cp -R "$repo/mod/nerve" "$st/mod/"
printf '{"socket":"%s"}' "$st/beta/sync/agent.sock" > "$st/mod/nerve/nerve.json"
tmux new-session -d -s "$tm" -x 160 -y 50 -c "$st/work" -e "NERVE_CONFIG_DIR=$st/beta" \
  "claude --plugin-dir $st/mod/nerve --permission-mode default --model haiku"
sleep 6
pane 3 | grep -q 'trust this folder' && { tmux send-keys -t "$tm" Down Enter; sleep 5; }
pane 8 | grep -q 'external imports' && { tmux send-keys -t "$tm" Enter; sleep 4; }
sid=""
for _ in $(seq 1 30); do
  sid=$(alpha sync status --json 2>/dev/null | python3 -c 'import json,sys
for d in json.load(sys.stdin):
  for s in d["sessions"]:
    if s.get("via_mod"): print(s["id"]); raise SystemExit' 2>/dev/null)
  [ -n "$sid" ] && break; sleep 1
done
echo "session on beta with mod connected: ${sid:-NONE}"
[ -n "$sid" ] || { pane 20; exit 1; }

say "E18 prompt from alpha (idle session)"
t0=$(date +%s.%N)
alpha sync send beta "${sid:0:8}" "Run the bash command: echo e18-idle" | tail -2
echo "send→ack $(echo "$(date +%s.%N) - $t0" | bc) s"
sleep 8; pane 6
say "E18b two prompts while a turn runs (queued, in order)"
alpha sync send beta "${sid:0:8}" "Run the bash command: sleep 6; echo e18-long" | tail -1
sleep 1
alpha sync send beta "${sid:0:8}" "Now reply with just the word E18-QUEUED" | tail -1
sleep 22; pane 8

say "E19 permission prompt answered from alpha"
alpha sync send beta "${sid:0:8}" "Use the Write tool to write the word nineteen into e19.txt. Nothing else." | tail -1
req=""
for _ in $(seq 1 30); do
  req=$(alpha sync status 2>/dev/null | awk '/\[permission\]/{print $2; exit}'); [ -n "$req" ] && break; sleep 1
done
alpha sync status | grep -E '^\s+\?' | head -3
echo "--- beta's screen while waiting (dialog visible):"; pane 6
sleep 20
echo "still pending on alpha after 20 s: $(alpha sync status | grep -c '\[permission\]')"
alpha sync answer beta "$req" allow | tail -1
sleep 5; echo "e19.txt: $(cat "$st/work/e19.txt" 2>/dev/null || echo MISSING)"; pane 4

say "E20 question answered from alpha, no away mode"
alpha sync send beta "${sid:0:8}" "Use the AskUserQuestion tool to ask me to pick one: Red or Blue. Then reply with only my pick." | tail -1
req=""
for _ in $(seq 1 40); do
  req=$(alpha sync status 2>/dev/null | awk '/\[question\]/{print $2; exit}'); [ -n "$req" ] && break; sleep 1
done
alpha sync status | grep -E '^\s+\?' | head -3
echo "--- beta's screen (the question is visible locally too):"; pane 7
sleep 15
alpha sync answer beta "$req" Blue | tail -1
sleep 6; pane 5

say "E21 answered at the keyboard → withdrawn on alpha"
alpha sync send beta "${sid:0:8}" "Use the Write tool to write the word twentyone into e21.txt. Nothing else." | tail -1
for _ in $(seq 1 30); do alpha sync status 2>/dev/null | grep -q '\[permission\]' && break; sleep 1; done
echo "pending before:"; alpha sync status | grep -E '^\s+\?'
sleep 12
tmux send-keys -t "$tm" 1; sleep 6
echo "pending after local answer:"; alpha sync status | grep -E '^\s+\?' || echo "  (none)"
pane 6
echo "e21.txt: $(cat "$st/work/e21.txt" 2>/dev/null || echo MISSING)"

say "E22 hand-off twice from beta to alpha"
# Give alpha a long-lived agent and a handoff root of its own.
f="$st/alpha/sync/fleet.toml"; { printf 'handoff_root = "%s"\n' "$st/alpha-handoff"; cat "$f"; } >"$f.tmp" && mv "$f.tmp" "$f"
bg alpha
sleep 4
beta sync handoff "${sid:0:8}" alpha | tail -3
for _ in $(seq 1 60); do grep -q 'resumed' "$st/alpha-agent.log" && break; sleep 1; done
grep -E 'handing over|asking for|resumed|failed' "$st/alpha-agent.log" | tail -3
grep -E 'handoff .* sent' "$st/beta-agent.log" | tail -1
echo "--- one more turn and one more commit on beta, then hand off again"
alpha_tmux=$(tmux ls | awk -F: '/^nerve-/{print $1}' | head -1); [ -n "$alpha_tmux" ] && tmux kill-session -t "$alpha_tmux"
git -C "$st/alpha-handoff/work" checkout -q -- . 2>/dev/null; git -C "$st/alpha-handoff/work" clean -qfd 2>/dev/null
alpha sync send beta "${sid:0:8}" "Reply with just the word E22" | tail -1
sleep 8
( cd "$st/work" && git add -A && git -c user.email=t@t -c user.name=t commit -qm "work from the session" )
beta sync handoff "${sid:0:8}" alpha | tail -2
for _ in $(seq 1 60); do [ "$(grep -c 'resumed' "$st/alpha-agent.log")" -ge 2 ] && break; sleep 1; done
grep -E 'handoff .* sent' "$st/beta-agent.log" | tail -2
grep -E 'resumed|failed' "$st/alpha-agent.log" | tail -2
echo "chunk store sizes: beta $(du -sh "$st/beta/sync/chunks" | cut -f1), alpha $(du -sh "$st/alpha/sync/chunks" | cut -f1)"
say done
