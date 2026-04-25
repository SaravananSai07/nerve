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

printf "\n${BOLD}nerve installer${RESET}\n"
printf "${DIM}First-time install takes a few minutes — Rust toolchain (if missing)${RESET}\n"
printf "${DIM}plus compiling nerve from source. Subsequent runs are instant.${RESET}\n"

if ! command -v cargo &>/dev/null; then
    if [[ ! -e /dev/tty ]]; then
        err "cargo not found. Install Rust first: https://rustup.rs"
        exit 1
    fi

    warn "Rust toolchain not found."
    printf "  Install via rustup (https://rustup.rs)? [y/N] "
    read -r answer </dev/tty
    case "$answer" in
        y|Y)
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
            if [[ -f "$HOME/.cargo/env" ]]; then
                # shellcheck source=/dev/null
                source "$HOME/.cargo/env"
            fi
            if ! command -v cargo &>/dev/null; then
                err "rustup finished but cargo is still not on PATH. Open a new shell and re-run."
                exit 1
            fi
            info "Rust installed"
            ;;
        *)
            err "Install Rust first: https://rustup.rs"
            exit 1
            ;;
    esac
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
# 2. Update-check preference
# ---------------------------------------------------------------------------
if [[ -e /dev/tty ]]; then
    printf "\n${BOLD}Update checks${RESET}\n"
    printf "${DIM}Once a day, nerve can check crates.io for a newer release and${RESET}\n"
    printf "${DIM}show a quiet banner at the top of the TUI. Off-the-wire, no telemetry.${RESET}\n"
    printf "Enable update checks on launch? [Y/n] "
    read -r answer </dev/tty
    case "$answer" in
        n|N)
            CONFIG_DIR="$HOME/.config/nerve"
            CONFIG_FILE="$CONFIG_DIR/config.toml"
            mkdir -p "$CONFIG_DIR"
            if [[ -f "$CONFIG_FILE" ]]; then
                warn "$CONFIG_FILE already exists — set [updates] check_on_launch = false manually to disable"
            else
                printf "[updates]\ncheck_on_launch = false\n" > "$CONFIG_FILE"
                info "Update checks disabled (wrote $CONFIG_FILE)"
            fi
            ;;
        *)
            info "Update checks enabled"
            ;;
    esac
fi

# ---------------------------------------------------------------------------
# 3. Optional dependencies (macOS only)
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
# 4. Done
# ---------------------------------------------------------------------------
printf "\n${GREEN}${BOLD}nerve installed successfully.${RESET}\n"
printf "Run ${BOLD}nerve${RESET} to start the TUI.\n\n"
