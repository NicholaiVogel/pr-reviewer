#!/usr/bin/env bash
set -euo pipefail

SCRIPT_VERSION="0.1.0"
REPO_URL="https://github.com/NicholaiVogel/pr-reviewer.git"
MIN_RUST_VERSION="1.75"
TMPDIR_CLONE=""

# -- flags (defaults) --------------------------------------------------------

FLAG_YES=false
FLAG_NO_SYSTEMD=false
FLAG_NO_INIT=false

# -- output formatting --------------------------------------------------------

if [ -t 1 ]; then
    BOLD='\033[1m'
    DIM='\033[2m'
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    CYAN='\033[0;36m'
    RESET='\033[0m'
else
    BOLD='' DIM='' RED='' GREEN='' YELLOW='' CYAN='' RESET=''
fi

info()    { printf '%s %s\n' "${DIM}--${RESET}" "$*"; }
warn()    { printf '%s %s\n' "${YELLOW}!!${RESET}" "$*"; }
error()   { printf '%s %s\n' "${RED}**${RESET}" "$*" >&2; }
success() { printf '%s %s\n' "${GREEN}ok${RESET}" "$*"; }
step()    { printf '\n%s\n\n' "${BOLD}${CYAN}>> $*${RESET}"; }

# -- interactive helpers ------------------------------------------------------

# read user input, falling back to /dev/tty when stdin is a pipe
read_input() {
    if [ -t 0 ]; then
        read "$@"
    else
        read "$@" </dev/tty
    fi
}

# confirm [prompt] [default: y|n]
# returns 0 for yes, 1 for no
confirm() {
    local prompt="$1"
    local default="${2:-n}"

    if [ "$FLAG_YES" = true ]; then
        return 0
    fi

    local hint
    if [ "$default" = "y" ]; then
        hint="[Y/n]"
    else
        hint="[y/N]"
    fi

    printf "%s %s " "$prompt" "$hint"
    local answer
    read_input -r answer || answer=""
    answer="${answer:-$default}"

    case "$answer" in
        [yY]*) return 0 ;;
        *)     return 1 ;;
    esac
}

# -- utility ------------------------------------------------------------------

command_exists() { command -v "$1" >/dev/null 2>&1; }

get_cargo_bin_dir() {
    printf '%s\n' "${CARGO_HOME:-$HOME/.cargo}/bin"
}

get_user_bin_dir() {
    printf '%s\n' "${PR_REVIEWER_INSTALL_BIN_DIR:-${XDG_BIN_HOME:-$HOME/.local/bin}}"
}

path_contains_dir() {
    local dir="$1"
    case ":$PATH:" in
        *":$dir:"*) return 0 ;;
        *)          return 1 ;;
    esac
}

resolve_installed_binary() {
    local candidate
    local cargo_binary
    local user_binary

    cargo_binary="$(get_cargo_bin_dir)/pr-reviewer"
    user_binary="$(get_user_bin_dir)/pr-reviewer"

    for candidate in \
        "$user_binary" \
        "$cargo_binary" \
        "$(type -P pr-reviewer 2>/dev/null || true)" \
        "$(command -v pr-reviewer 2>/dev/null || true)"
    do
        [ -n "$candidate" ] || continue
        if [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done

    return 1
}

ensure_user_bin_shim() {
    local cargo_binary
    local user_bin_dir
    local shim_path
    local existing_target

    cargo_binary="$(get_cargo_bin_dir)/pr-reviewer"
    user_bin_dir="$(get_user_bin_dir)"
    shim_path="$user_bin_dir/pr-reviewer"

    if [ ! -x "$cargo_binary" ]; then
        return 0
    fi

    if [ "$user_bin_dir" = "$(dirname "$cargo_binary")" ]; then
        return 0
    fi

    mkdir -p "$user_bin_dir"

    if [ -L "$shim_path" ]; then
        existing_target="$(readlink "$shim_path" 2>/dev/null || true)"
        if [ "$existing_target" = "$cargo_binary" ]; then
            return 0
        fi
    elif [ -e "$shim_path" ]; then
        warn "Existing $shim_path is not managed by the installer, leaving it alone."
        return 0
    fi

    ln -sfn "$cargo_binary" "$shim_path"
    success "Shell shim refreshed: $shim_path -> $cargo_binary"

    if ! path_contains_dir "$user_bin_dir"; then
        warn "$user_bin_dir is not on your PATH."
        warn "Add this to your shell profile:"
        printf "\n    export PATH=\"%s:\$PATH\"\n\n" "$user_bin_dir"
    fi
}

# version_ge "1.80.0" "1.75.0" => true (0)
# compares two dotted version strings
version_ge() {
    local IFS=.
    local i a=($1) b=($2)
    for ((i=0; i<${#b[@]}; i++)); do
        local va="${a[i]:-0}"
        local vb="${b[i]:-0}"
        if ((va > vb)); then return 0; fi
        if ((va < vb)); then return 1; fi
    done
    return 0
}

get_rust_version() {
    rustc --version 2>/dev/null | grep -oP '\d+\.\d+\.\d+' | head -1
}

get_pr_reviewer_version() {
    local binary_path
    binary_path="$(resolve_installed_binary 2>/dev/null || true)"
    if [ -z "$binary_path" ]; then
        return 0
    fi
    "$binary_path" --version 2>/dev/null | grep -oP '\d+\.\d+\.\d+' | head -1 || true
}

# -- cleanup ------------------------------------------------------------------

cleanup() {
    if [ -n "$TMPDIR_CLONE" ] && [ -d "$TMPDIR_CLONE" ]; then
        rm -rf "$TMPDIR_CLONE"
    fi
}

on_error() {
    error "Installation failed at line $1. See output above for details."
}

trap 'on_error $LINENO' ERR
trap cleanup EXIT

# -- phase 1: platform -------------------------------------------------------

detect_platform() {
    step "Detecting platform"

    PLATFORM="$(uname -s)"
    PKG_MANAGER=""
    INSTALL_HINT=""

    case "$PLATFORM" in
        Linux)
            info "Platform: Linux"
            if command_exists apt-get; then
                PKG_MANAGER="apt"
                INSTALL_HINT="sudo apt-get install"
            elif command_exists dnf; then
                PKG_MANAGER="dnf"
                INSTALL_HINT="sudo dnf install"
            elif command_exists pacman; then
                PKG_MANAGER="pacman"
                INSTALL_HINT="sudo pacman -S"
            elif command_exists apk; then
                PKG_MANAGER="apk"
                INSTALL_HINT="sudo apk add"
            fi
            ;;
        Darwin)
            info "Platform: macOS"
            warn "macOS support is less tested. Things should work, but please report issues."
            if command_exists brew; then
                PKG_MANAGER="brew"
                INSTALL_HINT="brew install"
            fi
            ;;
        *)
            error "Unsupported platform: $PLATFORM"
            error "pr-reviewer currently supports Linux and macOS."
            exit 1
            ;;
    esac

    if [ -n "$PKG_MANAGER" ]; then
        info "Package manager: $PKG_MANAGER"
    fi
}

# -- phase 2: prerequisites --------------------------------------------------

check_prerequisites() {
    step "Checking prerequisites"

    # git (required)
    if command_exists git; then
        info "git: $(git --version | head -1)"
    else
        error "git is not installed."
        if [ -n "$INSTALL_HINT" ]; then
            error "Install it with: $INSTALL_HINT git"
        fi
        exit 1
    fi

    # C compiler (soft check, needed for rusqlite bundled build)
    if command_exists cc || command_exists gcc; then
        info "C compiler: found"
    else
        warn "No C compiler found (cc/gcc). The build may fail."
        case "$PKG_MANAGER" in
            apt)    warn "Install with: sudo apt-get install build-essential" ;;
            dnf)    warn "Install with: sudo dnf install gcc" ;;
            pacman) warn "Install with: sudo pacman -S base-devel" ;;
            apk)    warn "Install with: sudo apk add build-base" ;;
            brew)   warn "Install with: xcode-select --install" ;;
        esac
    fi

    # cargo/rustc (checked here, installed in next phase if missing)
    if command_exists cargo; then
        local rust_ver
        rust_ver="$(get_rust_version)"
        if [ -n "$rust_ver" ]; then
            if version_ge "$rust_ver" "$MIN_RUST_VERSION"; then
                info "Rust: $rust_ver (>= $MIN_RUST_VERSION, good)"
            else
                warn "Rust $rust_ver is below the minimum ($MIN_RUST_VERSION)."
                if command_exists rustup; then
                    if confirm "  Update Rust via rustup?" "y"; then
                        rustup update stable
                        success "Rust updated."
                    else
                        error "Rust >= $MIN_RUST_VERSION is required. Aborting."
                        exit 1
                    fi
                else
                    error "Rust >= $MIN_RUST_VERSION is required and rustup is not available to update."
                    exit 1
                fi
            fi
        fi
    else
        info "Rust: not found (will install in next step)"
    fi
}

# -- phase 3: rust toolchain -------------------------------------------------

ensure_rust() {
    if command_exists cargo; then
        return 0
    fi

    step "Installing Rust"

    # check if rustup exists but cargo isn't on PATH
    if command_exists rustup; then
        warn "rustup is installed but cargo is not on PATH."
        info "Trying to source ~/.cargo/env ..."
        # shellcheck disable=SC1091
        [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
        if command_exists cargo; then
            success "cargo is now available."
            return 0
        fi
    fi

    if ! confirm "Rust is required. Install via rustup.rs?" "y"; then
        error "Cannot proceed without Rust. Install it from https://rustup.rs/ and re-run."
        exit 1
    fi

    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable

    # shellcheck disable=SC1091
    [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

    if command_exists cargo; then
        success "Rust installed: $(get_rust_version)"
    else
        error "Rust installation completed but cargo is not on PATH."
        error "Try running: source ~/.cargo/env"
        exit 1
    fi
}

# -- phase 4: build and install ----------------------------------------------

detect_install_mode() {
    # check if we're in a pr-reviewer clone
    if [ -f "Cargo.toml" ] && grep -q 'name = "pr-reviewer"' Cargo.toml 2>/dev/null; then
        echo "clone"
    else
        echo "standalone"
    fi
}

build_and_install() {
    step "Building and installing pr-reviewer"

    local mode
    mode="$(detect_install_mode)"

    local existing_ver
    existing_ver="$(get_pr_reviewer_version)"
    if [ -n "$existing_ver" ]; then
        info "Existing installation found: v$existing_ver"
    fi

    local source_dir
    if [ "$mode" = "clone" ]; then
        source_dir="."
        info "Building from local clone ..."
    else
        info "No local clone detected, cloning from $REPO_URL ..."
        TMPDIR_CLONE="$(mktemp -d)"
        git clone --depth 1 "$REPO_URL" "$TMPDIR_CLONE"
        source_dir="$TMPDIR_CLONE"
    fi

    cargo install --path "$source_dir"
    ensure_user_bin_shim

    # verify
    local binary_path
    binary_path="$(resolve_installed_binary || true)"
    if [ -n "$binary_path" ]; then
        local new_ver
        new_ver="$(get_pr_reviewer_version)"
        if [ -n "$existing_ver" ]; then
            success "pr-reviewer upgraded: v$existing_ver -> v$new_ver"
        else
            success "pr-reviewer installed: v$new_ver"
        fi
        info "Binary: $binary_path"
        info "If your current shell cached an older pr-reviewer path, run: hash -r"
    else
        # cargo install puts it in ~/.cargo/bin, which might not be on PATH
        local cargo_bin
        cargo_bin="$(get_cargo_bin_dir)"
        if [ -x "$cargo_bin/pr-reviewer" ]; then
            warn "pr-reviewer was installed to $cargo_bin but it's not on your PATH."
            warn "Add this to your shell profile:"
            printf "\n    export PATH=\"%s:\$PATH\"\n\n" "$cargo_bin"
            warn "Then restart your shell or run: source ~/.cargo/env"
        else
            error "Build succeeded but pr-reviewer binary not found."
            exit 1
        fi
    fi
}

# -- phase 5: initialize -----------------------------------------------------

initialize_pr_reviewer() {
    if [ "$FLAG_NO_INIT" = true ]; then
        info "Skipping init (--no-init)"
        return 0
    fi

    step "Initializing pr-reviewer"

    local config_dir="${PR_REVIEWER_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/pr-reviewer}"
    local config_file="$config_dir/config.toml"

    if [ -f "$config_file" ]; then
        info "Config already exists at $config_file, skipping init."
        return 0
    fi

    local binary_path
    binary_path="$(resolve_installed_binary || true)"
    if [ -z "$binary_path" ]; then
        error "pr-reviewer binary not found after install."
        exit 1
    fi

    "$binary_path" init
    success "Initialized at $config_dir"
}

# -- phase 6: runtime deps audit ---------------------------------------------

audit_runtime_deps() {
    step "Checking runtime dependencies"

    local found_harness=false

    # AI harnesses
    if command_exists claude; then
        success "claude (Claude Code) -- found"
        found_harness=true
    else
        info "claude (Claude Code) -- not found"
        info "  Install: https://docs.anthropic.com/en/docs/claude-code"
    fi

    if command_exists opencode; then
        success "opencode -- found"
        found_harness=true
    else
        info "opencode -- not found"
        info "  Install: https://github.com/opencode-ai/opencode"
    fi

    if command_exists codex; then
        success "codex -- found"
        found_harness=true
    else
        info "codex -- not found"
        info "  Install: https://github.com/openai/codex"
    fi

    if [ "$found_harness" = false ]; then
        echo
        warn "No AI harness found. At least one is required (claude, opencode, or codex)."
        warn "Install one before running reviews."
    fi

    echo

    # gitnexus (optional)
    if command_exists gitnexus; then
        success "gitnexus -- found (optional, enables impact analysis)"
    elif command_exists npx; then
        info "gitnexus -- not installed, but npx is available (will use npx gitnexus as fallback)"
    else
        info "gitnexus -- not found (optional)"
        info "  Install: npm install -g gitnexus"
    fi
}

# -- phase 7: systemd --------------------------------------------------------

setup_systemd() {
    if [ "$FLAG_NO_SYSTEMD" = true ]; then
        return 0
    fi

    # only on linux with systemctl
    if [ "$PLATFORM" != "Linux" ] || ! command_exists systemctl; then
        return 0
    fi

    echo
    if ! confirm "Set up systemd service for background daemon?" "n"; then
        return 0
    fi

    step "Setting up systemd service"

    if ! command_exists sudo; then
        warn "sudo is not available. Here are the manual steps:"
        echo
        info "1. Write the unit file to /etc/systemd/system/pr-reviewer.service"
        info "2. Create /etc/pr-reviewer/secrets with PR_REVIEWER_PASSPHRASE"
        info "3. See docs/deployment.md for the full template"
        return 0
    fi

    local binary_path
    binary_path="$(resolve_installed_binary || true)"
    if [ -z "$binary_path" ]; then
        binary_path="$(get_cargo_bin_dir)/pr-reviewer"
    fi
    local run_user
    run_user="$(whoami)"

    # unit file
    if [ -f "/etc/systemd/system/pr-reviewer.service" ]; then
        if ! confirm "  Service file already exists. Overwrite?" "n"; then
            info "Keeping existing service file."
        else
            write_unit_file "$binary_path" "$run_user"
        fi
    else
        write_unit_file "$binary_path" "$run_user"
    fi

    # secrets file
    if [ ! -f "/etc/pr-reviewer/secrets" ]; then
        sudo mkdir -p /etc/pr-reviewer
        sudo tee /etc/pr-reviewer/secrets >/dev/null <<'SECRETS'
# pr-reviewer daemon secrets
# Uncomment and set your passphrase if token was encrypted with --passphrase
# PR_REVIEWER_PASSPHRASE=
RUST_LOG=info
SECRETS
        sudo chmod 600 /etc/pr-reviewer/secrets
        sudo chown "root:$run_user" /etc/pr-reviewer/secrets
        success "Secrets template created at /etc/pr-reviewer/secrets"
    else
        info "Secrets file already exists at /etc/pr-reviewer/secrets"
    fi

    sudo systemctl daemon-reload

    if confirm "  Enable pr-reviewer to start on boot?" "n"; then
        sudo systemctl enable pr-reviewer
        success "Service enabled."
    fi

    echo
    info "The service is NOT started yet. First:"
    info "  1. Set your token:   pr-reviewer config set-token --passphrase"
    info "  2. Edit secrets:     sudo editor /etc/pr-reviewer/secrets"
    info "  3. Start:            sudo systemctl start pr-reviewer"
}

write_unit_file() {
    local binary_path="$1"
    local run_user="$2"

    sudo tee /etc/systemd/system/pr-reviewer.service >/dev/null <<EOF
[Unit]
Description=pr-reviewer daemon
After=network.target

[Service]
Type=simple
User=$run_user
EnvironmentFile=/etc/pr-reviewer/secrets
ExecStart=$binary_path start
Restart=on-failure
RestartSec=10s
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF
    success "Unit file written to /etc/systemd/system/pr-reviewer.service"
}

# -- phase 8: summary --------------------------------------------------------

print_next_steps() {
    local config_dir="${PR_REVIEWER_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/pr-reviewer}"
    local binary_path
    binary_path="$(resolve_installed_binary || true)"
    if [ -z "$binary_path" ]; then
        binary_path="$(get_cargo_bin_dir)/pr-reviewer"
    fi

    step "Installation complete"

    info "Binary:   $binary_path"
    info "Config:   $config_dir/config.toml"
    info "Database: $config_dir/state.db"

    cat <<EOF

  ${BOLD}Next steps:${RESET}

  1. Set your GitHub token:
     pr-reviewer config set-token --passphrase

  2. Add a repository to watch:
     pr-reviewer add owner/repo

  3. Test with a dry run:
     pr-reviewer review owner/repo#42 --dry-run

  4. Start the daemon:
     pr-reviewer start

  ${DIM}Docs: docs/configuration.md, docs/troubleshooting.md, docs/deployment.md${RESET}

EOF
}

# -- banner and args ----------------------------------------------------------

print_banner() {
    printf '\n%s v%s\n' "${BOLD}  pr-reviewer installer${RESET}" "${DIM}$SCRIPT_VERSION${RESET}"
    printf '%s\n\n' "  ${DIM}Self-hosted PR review daemon${RESET}"
}

print_usage() {
    cat <<EOF
Usage: install.sh [OPTIONS]

Options:
  -y, --yes          Answer yes to all prompts
  --no-systemd       Skip systemd service setup
  --no-init          Skip pr-reviewer init (for upgrades)
  -h, --help         Show this help
  -v, --version      Show installer version
EOF
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            -y|--yes)        FLAG_YES=true ;;
            --no-systemd)    FLAG_NO_SYSTEMD=true ;;
            --no-init)       FLAG_NO_INIT=true ;;
            -h|--help)       print_usage; exit 0 ;;
            -v|--version)    echo "pr-reviewer installer v$SCRIPT_VERSION"; exit 0 ;;
            *)               error "Unknown option: $1"; print_usage; exit 1 ;;
        esac
        shift
    done
}

# -- main ---------------------------------------------------------------------

main() {
    print_banner
    parse_args "$@"
    detect_platform
    check_prerequisites
    ensure_rust
    build_and_install
    initialize_pr_reviewer
    audit_runtime_deps
    setup_systemd
    print_next_steps
}

main "$@"
