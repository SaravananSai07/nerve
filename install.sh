#!/usr/bin/env bash
set -euo pipefail

# nerve installer
# Builds nerve from source and optionally installs platform extras.

BOLD='\033[1m'
DIM='\033[2m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
RESET='\033[0m'

info()  { printf "${GREEN}✓${RESET} %s\n" "$1"; }
warn()  { printf "${YELLOW}!${RESET} %s\n" "$1"; }
err()   { printf "${RED}✗${RESET} %s\n" "$1"; }

# ---------------------------------------------------------------------------
# 1. Build nerve
# ---------------------------------------------------------------------------
printf "\n${BOLD}Building nerve...${RESET}\n"

if ! command -v cargo &>/dev/null; then
    err "cargo not found. Install Rust first: https://rustup.rs"
    exit 1
fi

cargo build --release
BINARY="$(pwd)/target/release/nerve-tui"

if [[ ! -f "$BINARY" ]]; then
    err "Build failed — binary not found at $BINARY"
    exit 1
fi
info "Built $BINARY"

# ---------------------------------------------------------------------------
# 2. Install binary
# ---------------------------------------------------------------------------
INSTALL_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "$INSTALL_DIR"
cp "$BINARY" "$INSTALL_DIR/nerve"
info "Installed to $INSTALL_DIR/nerve"

# ---------------------------------------------------------------------------
# 3. Optional dependencies (macOS only)
# ---------------------------------------------------------------------------
if [[ "$(uname)" == "Darwin" ]]; then
    printf "\n${BOLD}Optional macOS extras${RESET}\n"
    printf "${DIM}These improve the experience but are not required.${RESET}\n\n"

    EXTRAS=(
        "terminal-notifier|Click-to-activate terminal from notifications"
    )

    for entry in "${EXTRAS[@]}"; do
        pkg="${entry%%|*}"
        desc="${entry##*|}"

        if command -v "$pkg" &>/dev/null; then
            info "$pkg already installed — $desc"
            continue
        fi

        printf "  ${BOLD}%s${RESET} — %s\n" "$pkg" "$desc"
        printf "  Install? [y/N/q] "
        read -r answer </dev/tty
        case "$answer" in
            y|Y)
                if command -v brew &>/dev/null; then
                    brew install "$pkg"
                    info "Installed $pkg"
                else
                    warn "Homebrew not found — install manually: brew install $pkg"
                fi
                ;;
            q|Q)
                warn "Skipping remaining extras"
                break
                ;;
            *)
                warn "Skipped $pkg"
                ;;
        esac
    done
fi

# ---------------------------------------------------------------------------
# 4. Done
# ---------------------------------------------------------------------------
printf "\n${GREEN}${BOLD}nerve installed successfully.${RESET}\n"
printf "Run ${BOLD}nerve${RESET} to start the TUI.\n\n"
