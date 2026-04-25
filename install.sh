#!/usr/bin/env bash
set -euo pipefail

# nerve installer
#
# Usage:
#   ./install.sh                                                 (from a cloned repo)
#   curl -fsSL https://raw.githubusercontent.com/SaravananSai07/nerve/master/install.sh | bash
#
# When run from the repo, builds from source.
# When run via curl | bash, installs from crates.io.

BOLD='\033[1m'
DIM='\033[2m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
RESET='\033[0m'

info()  { printf "${GREEN}✓${RESET} %s\n" "$1"; }
warn()  { printf "${YELLOW}!${RESET} %s\n" "$1"; }
err()   { printf "${RED}✗${RESET} %s\n" "$1"; }

if ! command -v cargo &>/dev/null; then
    err "cargo not found. Install Rust first: https://rustup.rs"
    exit 1
fi

INSTALL_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "$INSTALL_DIR"

# ---------------------------------------------------------------------------
# 1. Build or fetch nerve
# ---------------------------------------------------------------------------
if [[ -f "Cargo.toml" ]] && grep -q '^name = "nerve-tui"' Cargo.toml 2>/dev/null; then
    printf "\n${BOLD}Building nerve from source...${RESET}\n"
    cargo build --release
    SRC_BINARY="$(pwd)/target/release/nerve-tui"
    if [[ ! -f "$SRC_BINARY" ]]; then
        err "Build failed — binary not found at $SRC_BINARY"
        exit 1
    fi
    cp "$SRC_BINARY" "$INSTALL_DIR/nerve"
else
    printf "\n${BOLD}Installing nerve from crates.io...${RESET}\n"
    TMP_ROOT="$(mktemp -d)"
    trap 'rm -rf "$TMP_ROOT"' EXIT
    cargo install nerve-tui --root "$TMP_ROOT" --quiet
    cp "$TMP_ROOT/bin/nerve-tui" "$INSTALL_DIR/nerve"
fi

info "Installed to $INSTALL_DIR/nerve"

if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    warn "$INSTALL_DIR is not on your PATH — add it to your shell profile:"
    printf "    ${BOLD}export PATH=\"%s:\$PATH\"${RESET}\n" "$INSTALL_DIR"
fi

# ---------------------------------------------------------------------------
# 2. Optional dependencies (macOS only)
# ---------------------------------------------------------------------------
if [[ "$(uname)" == "Darwin" ]] && [[ -t 0 || -e /dev/tty ]]; then
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
# 3. Done
# ---------------------------------------------------------------------------
printf "\n${GREEN}${BOLD}nerve installed successfully.${RESET}\n"
printf "Run ${BOLD}nerve${RESET} to start the TUI.\n\n"
