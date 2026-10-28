#!/usr/bin/env bash
# Two-device check of runs, places and refs on one machine: a private
# relay, two agents ("alpha", "beta") with their own NERVE_CONFIG_DIR
# under demo/.state, and each side driving the other from the CLI.
#
#   research/sync/demo/places.sh
#
# Non-interactive; exits non-zero on the first failed expectation.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
state="$here/.state/places"
port="${DEMO_PORT:-7789}"
relay="ws://127.0.0.1:$port"

cargo build --quiet --release --features sync --manifest-path "$repo/Cargo.toml"
nerve="$repo/target/release/nerve"

pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT INT TERM

rm -rf "$state"
mkdir -p "$state"
alpha() { NERVE_CONFIG_DIR="$state/alpha" "$nerve" "$@"; }
beta() { NERVE_CONFIG_DIR="$state/beta" "$nerve" "$@"; }
bg() { local dev=$1; shift; NERVE_CONFIG_DIR="$state/$dev" "$nerve" "$@" >"$state/$dev-agent.log" 2>&1 & pids+=($!); }
step() { printf '\n› %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
g() { git -c user.email=t@t -c user.name=t -c init.defaultBranch=main "$@"; }

"$nerve" sync serve-relay --port "$port" >"$state/relay.log" 2>&1 &
pids+=($!)
until nc -z 127.0.0.1 "$port" 2>/dev/null; do sleep 0.2; done

# alpha's harness: a git repo (beta has a clone) plus a folder of unit
# files and a script that only alpha has so far.
A="$state/alpha-harness" B="$state/beta-harness"
mkdir -p "$A"
(cd "$A" && g init -q && echo harness >README && g add . && g commit -qm base)
g clone -q "$A" "$B"
mkdir -p "$A/units/S5" "$A/bin"
for i in $(seq 1 60); do
  for j in $(seq 1 300); do echo "unit $i line $j: some plan text that goes on for a while"; done >"$A/units/S5/u$i.md"
done
printf '#!/bin/sh\necho "v2 $*"; [ "$1" = verify ] && exit 3; exit 0\n' >"$A/bin/v2.sh"
chmod 755 "$A/bin/v2.sh"

alpha sync init --name alpha --relay "$relay" >/dev/null
beta sync init --name beta --relay "$relay" >/dev/null
cat >>"$state/alpha/sync/fleet.toml" <<EOF

[places]
harness = "$A"
EOF
cat >>"$state/beta/sync/fleet.toml" <<EOF

[places]
harness = "$B"

[run]
allow = [{ place = "harness", argv = ["./bin/v2.sh", "*"] }]
EOF
alpha sync pair beta "$(beta sync id)" >/dev/null
beta sync pair alpha "$(alpha sync id)" >/dev/null
bg alpha sync agent
alpha_pid=$!
bg beta sync agent
sleep 3

step "alpha → beta: put units bin (first send)"
alpha sync put beta harness units bin --wait 2>&1 | grep -E "✓|✗" | tail -1
cmp -s "$A/units/S5/u7.md" "$B/units/S5/u7.md" || fail "u7 differs"
[ -x "$B/bin/v2.sh" ] || fail "mode not kept"

step "alpha → beta: put again after changing one file"
echo "one more line" >>"$A/units/S5/u3.md"
alpha sync put beta harness units bin --wait 2>&1 | grep -E "✓|✗" | tail -1
cmp -s "$A/units/S5/u3.md" "$B/units/S5/u3.md" || fail "u3 differs"

step "alpha runs beta's allowlisted script; --wait exits with its code"
set +e
alpha sync run beta --place harness --wait -- ./bin/v2.sh verify S5 2>/dev/null
code=$?
set -e
echo "exit code: $code"
[ "$code" = 3 ] || fail "expected 3"

step "a command beta's allowlist doesn't name is refused"
if out=$(alpha sync run beta --place harness -- rm -rf units 2>&1); then fail "rm was allowed"; fi
echo "$out" | grep -o "not allowed on beta.*" | head -1
[ -d "$B/units" ] || fail "units deleted"

step "beta pulls from alpha with --delete (beta initiates)"
rm "$A/units/S5/u60.md"
echo "stray" >"$B/units/S5/stray.md"
beta sync get alpha harness units --delete --wait 2>&1 | grep -E "✓|✗" | tail -1
[ ! -e "$B/units/S5/u60.md" ] && [ ! -e "$B/units/S5/stray.md" ] || fail "--delete left files"

step "alpha sends a branch into beta's refs/remotes/alpha/v2-build"
(cd "$A" && g checkout -qb v2-build && echo x >x.txt && g add x.txt && g commit -qm "on v2-build")
head_before=$(git -C "$B" rev-parse HEAD)
alpha sync refs beta harness v2-build:refs/remotes/alpha/v2-build --wait 2>&1 | grep -E "✓|✗" | tail -1
[ "$(git -C "$B" rev-parse refs/remotes/alpha/v2-build)" = "$(git -C "$A" rev-parse v2-build)" ] || fail "ref not set"
[ "$(git -C "$B" rev-parse HEAD)" = "$head_before" ] || fail "beta HEAD moved"
[ ! -e "$B/x.txt" ] || fail "worktree touched"

step "beta pulls alpha's main into refs/remotes/alpha/main"
beta sync refs alpha harness --pull --wait main:refs/remotes/alpha/main 2>&1 | grep -E "✓|✗" | tail -1
[ "$(git -C "$B" rev-parse refs/remotes/alpha/main)" = "$(git -C "$A" rev-parse main)" ] || fail "pull ref not set"

step "status shows the run"
alpha sync status | grep -E "run " | head -2

step "without an agent on alpha: the CLI runs its own for the exchange"
kill "$alpha_pid"; sleep 1
echo "two" >>"$A/units/S5/u2.md"
alpha sync put beta harness units --wait 2>&1 | grep -E "✓|✗" | tail -1
cmp -s "$A/units/S5/u2.md" "$B/units/S5/u2.md" || fail "u2 differs"
echo "from beta" >"$B/units/S5/beta-note.md"
alpha sync get beta harness units/S5/beta-note.md --wait 2>&1 | grep -E "✓|✗" | tail -1
cmp -s "$B/units/S5/beta-note.md" "$A/units/S5/beta-note.md" || fail "get without agent"
set +e
alpha sync run beta --place harness --wait -- ./bin/v2.sh build 2>/dev/null
code=$?
set -e
[ "$code" = 0 ] || fail "expected 0, got $code"

echo
echo "places demo passed"
