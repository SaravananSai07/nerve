#!/bin/bash
# Linux end-to-end checks for nerve, run inside the image built from this
# directory with the repo mounted read-only at /src. See docs/TESTING.md:
#
#   docker build -t nerve-linux-e2e tests/linux
#   docker run --rm -v "$PWD":/src:ro nerve-linux-e2e bash /e2e.sh
#
# Uses Rust 1.85 (the declared MSRV), real procps/inotify/tmux/xclip, and
# stand-ins for `claude` and `notify-send`, so no Claude account is needed.
set -u
PASS=0; FAIL=0
ok()   { echo "PASS  $1"; PASS=$((PASS+1)); }
bad()  { echo "FAIL  $1"; FAIL=$((FAIL+1)); }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi; }

export CARGO_TARGET_DIR=/target
echo "== build / clippy / tests on Linux, Rust $(rustc --version | cut -d' ' -f2) (MSRV)"
cargo build --locked --release -q 2>&1 | tail -3
check "release build" "test -x /target/release/nerve"
cargo clippy --locked -q --all-targets -- -D warnings >/tmp/clippy.log 2>&1; check "clippy -D warnings" "[ $? -eq 0 ]"
cargo test --locked -q --all-targets >/tmp/test.log 2>&1; R=$?
grep -E "^test result" /tmp/test.log
check "cargo test" "[ $R -eq 0 ]"
NERVE=/target/release/nerve

# Two Claude roots, as with CLAUDE_CONFIG_DIR + ~/.claude.
export HOME=/root
ALT=/root/.claude-alt
mkdir -p /root/.claude/sessions $ALT/sessions $ALT/jobs /tmp/nervecfg
export CLAUDE_CONFIG_DIR=$ALT NERVE_CONFIG_DIR=/tmp/nervecfg
printf '[notifications]\non_waiting = true\non_complete = true\nsound = false\n' > /tmp/nervecfg/config.toml

mkdir -p /tmp/bin && cp /fake/claude-proc /tmp/bin/claude
session_file() { # root pid id cwd status [extra-json]
  printf '{"pid":%s,"sessionId":"%s","cwd":"%s","status":"%s","kind":"interactive","entrypoint":"cli"%s}' \
    "$2" "$3" "$4" "$5" "${6:-}" > "$1/sessions/$2.json"
}

echo "== discovery: ps parsing on procps, both roots"
tmux -L t new-session -d -s main -x 200 -y 50 "/tmp/bin/claude 600"
sleep 0.5
P1=$(pgrep -nx claude)
session_file $ALT "$P1" "11111111-aaaa" /work/alpha idle
check "procps reports ucomm=claude" "[ \"\$(ps -o ucomm= -p $P1 | tr -d ' ')\" = claude ]"
OUT=$($NERVE --list)
echo "$OUT"
check "session in \$CLAUDE_CONFIG_DIR found" "echo \"\$OUT\" | grep -q '^alpha | Idle | pts/'"

tmux -L t new-window -d "/tmp/bin/claude 601"; sleep 0.5
P2=$(pgrep -nx claude)
session_file /root/.claude "$P2" "22222222-bbbb" /work/beta waiting ',"waitingFor":"permission prompt"'
OUT=$($NERVE --list)
check "session in ~/.claude found too" "echo \"\$OUT\" | grep -q '^beta | Waiting (permission prompt) | pts/'"

# Daemon noise: a tty-less claude whose parent isn't a shell must be dropped.
# python3 (not a shell) is the parent, and setsid drops the tty.
python3 -c "import subprocess,time,os; subprocess.Popen(['/tmp/bin/claude','602'], start_new_session=True, stdin=subprocess.DEVNULL); time.sleep(30)" &
sleep 0.8; P3=$(pgrep -nx claude)
PT=$(ps -o tty= -p $P3 | tr -d ' '); PP=$(ps -o ucomm= -p "$(ps -o ppid= -p $P3 | tr -d ' ')" | tr -d ' ')
check "noise fixture: no tty ($PT), parent $PP" '[ "$PT" = "?" ] && [ "$PP" = python3 ]'
session_file $ALT "$P3" "33333333-cccc" /work/noise idle
check "tty-less non-shell claude dropped" "! $NERVE --list | grep -q noise"
rm -f $ALT/sessions/$P3.json; kill $P3

echo "== TUI: inotify on both roots, notification, clipboard, kill"
Xvfb :99 >/dev/null 2>&1 & XVFB=$!; export DISPLAY=:99; sleep 1
rm -f /tmp/notify.log /tmp/claude-cli.log
mkdir -p /tmp/fakebin && cp /fake/notify-send /tmp/fakebin/ && cp /fake/claude-cli /tmp/fakebin/claude
# NoOp bridge (no TMUX/TERM_PROGRAM) so Enter on a bg job takes the clipboard path.
tmux -L t new-window -d -n nerve "env -u TMUX TERM_PROGRAM= PATH=/tmp/fakebin:\$PATH DISPLAY=:99 HOME=$HOME CLAUDE_CONFIG_DIR=$ALT NERVE_CONFIG_DIR=/tmp/nervecfg $NERVE"
sleep 2
screen() { tmux -L t capture-pane -p -t main:nerve; }
check "TUI shows both sessions" "screen | grep -q alpha && screen | grep -q beta"
NP=$(pgrep -x nerve)
WD=$(cat /proc/$NP/fdinfo/* 2>/dev/null | grep -c 'inotify wd')
check "inotify watches on both sessions dirs ($WD)" "[ $WD -eq 2 ]"

# A new session file in each root must appear well inside the 30 s heartbeat.
tmux -L t new-window -d "/tmp/bin/claude 603"; sleep 0.3; P4=$(pgrep -nx claude)
session_file /root/.claude "$P4" "44444444-dddd" /work/gamma idle
# Under the 5 s process-table cache this used to take up to 5 s.
sleep 2
check "new session in ~/.claude shown within 2 s" "screen | grep -q gamma"
tmux -L t new-window -d "/tmp/bin/claude 604"; sleep 0.3; P5=$(pgrep -nx claude)
session_file $ALT "$P5" "55555555-eeee" /work/delta idle
sleep 2
check "new session in \$CLAUDE_CONFIG_DIR shown within 2 s" "screen | grep -q delta"

# Authoritative idle -> waiting fires notify-send at once, with the reason.
session_file $ALT "$P1" "11111111-aaaa" /work/alpha waiting ',"waitingFor":"input needed"'
sleep 2
check "notify-send fired with reason" "grep -q 'alpha: input needed' /tmp/notify.log 2>/dev/null"
cat /tmp/notify.log 2>/dev/null

# Background job (parked): Enter copies `claude attach` via xclip.
mkdir -p $ALT/jobs/abcd1234
printf '{"state":"blocked","sessionId":"abcd1234-ffff","cwd":"/work/bgjob","name":"bg job","needs":"pick A or B"}' > $ALT/jobs/abcd1234/state.json
touch $ALT/sessions/.wake; sleep 2
tmux -L t send-keys -t main:nerve / ; sleep 0.3; tmux -L t send-keys -t main:nerve "bg job" Enter; sleep 0.8
tmux -L t send-keys -t main:nerve Enter; sleep 1
CLIP=$(xclip -selection clipboard -o 2>/dev/null)
echo "clipboard: $CLIP"
check "clipboard via xclip" "[ \"\$CLIP\" = 'claude attach abcd1234' ]"
tmux -L t send-keys -t main:nerve Escape; sleep 0.3

# x / y on gamma sends SIGTERM to the right pid.
tmux -L t send-keys -t main:nerve / ; sleep 0.3; tmux -L t send-keys -t main:nerve gamma Enter; sleep 0.8
tmux -L t send-keys -t main:nerve x; sleep 0.5; tmux -L t send-keys -t main:nerve y; sleep 1.5
check "SIGTERM killed gamma (pid $P4)" "! kill -0 $P4 2>/dev/null"
check "other sessions untouched" "kill -0 $P1 && kill -0 $P2 && kill -0 $P5"

echo "== tmux attach split for background job"
# tmux runs split commands with the server's PATH, not nerve's.
cp /fake/claude-cli /usr/local/bin/claude
tmux -L t kill-window -t main:nerve
# Single-instance lock: wait for the old nerve to exit before starting another.
for _ in $(seq 1 50); do pgrep -x nerve >/dev/null || break; sleep 0.1; done
tmux -L t new-window -d -n nerve2 "PATH=/tmp/fakebin:\$PATH HOME=$HOME CLAUDE_CONFIG_DIR=$ALT NERVE_CONFIG_DIR=/tmp/nervecfg $NERVE"
sleep 2
tmux -L t send-keys -t main:nerve2 / ; sleep 0.3; tmux -L t send-keys -t main:nerve2 "bg job" Enter; sleep 0.8
tmux -L t send-keys -t main:nerve2 Enter; sleep 1.5
check "Enter ran 'claude attach abcd1234' in a split" "grep -q 'attach abcd1234' /tmp/claude-cli.log 2>/dev/null"

echo "== orphan watchdog (Linux pty hangup)"
NP=$(pgrep -x nerve)
tmux -L t kill-server; sleep 3.5
check "nerve exits after its tty dies" "! kill -0 $NP 2>/dev/null"

kill $XVFB 2>/dev/null
echo; echo "Linux e2e: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
