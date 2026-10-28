#!/usr/bin/env bash
# Two-device nerve sync demo on one machine, no Pi, Tailscale or internet.
#
#   research/sync/demo/demo.sh           relay + "beta" agent, then the TUI as "alpha" (press D)
#   research/sync/demo/demo.sh --job     also run a headless Claude job on beta (spends a few cents)
#   research/sync/demo/demo.sh --check   non-interactive smoke test: status + acknowledged sync
#
# Each device is a separate NERVE_CONFIG_DIR under demo/.state, so your real
# nerve config is untouched. Both "devices" see the same local Claude
# sessions, so anything you type from the Devices view lands in a real session.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
state="$here/.state"
port="${DEMO_PORT:-7788}"
relay="ws://127.0.0.1:$port"
mode="${1:-tui}"

echo "› building nerve with --features sync"
cargo build --quiet --release --features sync --manifest-path "$repo/Cargo.toml"
nerve="$repo/target/release/nerve"

pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT INT TERM

rm -rf "$state"
mkdir -p "$state/alpha" "$state/beta" "$state/beta-work"
alpha() { NERVE_CONFIG_DIR="$state/alpha" "$nerve" "$@"; }
beta() { NERVE_CONFIG_DIR="$state/beta" "$nerve" "$@"; }
# Background a long-running nerve so that $! is nerve itself (a
# backgrounded shell function is a subshell, and killing it would
# leave the agent running after the demo).
bg() { local dev=$1 log=$2; shift 2; NERVE_CONFIG_DIR="$state/$dev" "$nerve" "$@" >"$log" 2>&1 & pids+=($!); }

echo "› private relay on $relay"
"$nerve" sync serve-relay --port "$port" >"$state/relay.log" 2>&1 &
pids+=($!)
until nc -z 127.0.0.1 "$port" 2>/dev/null; do sleep 0.2; done

echo "› two devices: alpha (this TUI) and beta (headless agent)"
alpha sync init --name alpha --relay "$relay" >/dev/null
beta sync init --name beta --relay "$relay" >/dev/null
# Keep beta's handoffs and job checkouts inside the demo folder.
f="$state/beta/sync/fleet.toml"
{ printf 'handoff_root = "%s"\n' "$state/beta-work"; cat "$f"; } >"$f.tmp" && mv "$f.tmp" "$f"
alpha sync pair beta "$(beta sync id)"
beta sync pair alpha "$(alpha sync id)"

bg beta "$state/beta-agent.log" sync agent
echo "› waiting for beta's first state event"
for _ in $(seq 1 40); do
  alpha sync status 2>/dev/null | grep -q 'beta: last synced' && break
  sleep 0.5
done
alpha sync status
echo
echo "› manual sync (✓ only when beta acknowledges end to end)"
alpha sync sync

if [ "$mode" = "--job" ]; then
  echo
  echo "› job on beta: headless claude -p, then a verify command"
  alpha sync job --on beta --allow 'Bash(uname *)' --verify 'grep -q . arch.txt' \
    arch -- "Run uname -m and write its output to arch.txt. Nothing else."
  alpha sync status
fi

if [ "$mode" = "--check" ]; then
  echo "demo check passed"
  exit 0
fi

cat <<EOF

Opening the TUI as "alpha". Try:
  D   Devices view: beta's sessions, last synced, pending prompts, jobs
  r   sync now            m  type into a session      d/l/g  diff/log/subagents
  H   hand a session off to beta (lands in $state/beta-work, tmux)
  q   quit (stops the relay and beta)
Logs: $state/*.log
EOF
read -r -p "Press Enter to open the TUI… " _
NERVE_CONFIG_DIR="$state/alpha" "$nerve"
