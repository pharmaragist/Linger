#!/usr/bin/env bash
# Build and install linger into a user-writable prefix.
#
#   ./install.sh                       # installs to ~/.local/bin/linger
#   PREFIX=/usr/local ./install.sh     # system-wide (needs root)
#   ./install.sh --init                # also run `linger init`
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
RUN_INIT=0

usage() {
    cat <<'EOF'
Usage: ./install.sh [OPTIONS]

Options:
  --prefix DIR   Install into DIR/bin (default: $HOME/.local)
  --init         Run `linger init` after installing
  -h, --help     Show this help

Environment:
  PREFIX         Same as --prefix
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)
            [[ $# -ge 2 ]] || { echo "install.sh: --prefix needs a directory" >&2; exit 2; }
            PREFIX="$2"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#--prefix=}"
            shift
            ;;
        --init)
            RUN_INIT=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "install.sh: unknown option '$1'" >&2
            usage >&2
            exit 2
            ;;
    esac
done

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$PREFIX/bin"

command -v cargo >/dev/null 2>&1 || {
    echo "install.sh: cargo not found in PATH" >&2
    exit 1
}

echo "==> building release binary"
cargo build --release --manifest-path "$ROOT/Cargo.toml"

echo "==> installing to $BIN_DIR/linger"
mkdir -p "$BIN_DIR"
install -m 755 "$ROOT/target/release/linger" "$BIN_DIR/linger"

if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
    echo
    echo "note: $BIN_DIR is not in your PATH. Add it with:"
    echo "  export PATH=\"$BIN_DIR:\$PATH\""
fi

if [[ $RUN_INIT -eq 1 ]]; then
    if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
        echo "install.sh: WAYLAND_DISPLAY is unset; run \`linger init\` from inside a Wayland session" >&2
        exit 1
    fi
    echo "==> running linger init"
    "$BIN_DIR/linger" init
else
    echo
    echo "next: run \`linger init\` from inside a Wayland session, then \`linger add ... && linger up\`"
fi

echo "installed $BIN_DIR/linger"
