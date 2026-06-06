#!/usr/bin/env bash
set -euo pipefail

# ── Config ───────────────────────────────────────────────────────────────────
BORE_AMD64="https://github.com/manprint/bore/releases/latest/download/bore-x86_64-unknown-linux-musl"
BORE_ARM64="https://github.com/manprint/bore/releases/latest/download/bore-aarch64-unknown-linux-musl"
BORE_MACOS="https://github.com/manprint/bore/releases/latest/download/bore-x86_64-apple-darwin"
BORE_ANDROID="https://github.com/manprint/bore/releases/latest/download/bore-aarch64-linux-android"

USER_INSTALL_PATH="$HOME/.local/bin"
INSTALL_PATH="${INSTALL_PATH:-$USER_INSTALL_PATH}"
TMPDIR="$(mktemp -d)"

# ── Helpers ──────────────────────────────────────────────────────────────────
cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

die() { echo "ERROR: $*" >&2; exit 1; }

detect_arch() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64)  echo "amd64" ;;
        aarch64|arm64) echo "arm64" ;;
        *)             die "Unsupported arch: $arch" ;;
    esac
}

detect_os() {
    local os
    os="$(uname -s)"
    case "$os" in
        Linux)
            if [[ -f /system/build.prop ]] || [[ "${ANDROID_ROOT:-}" ]]; then
                echo "android"
            else
                echo "linux"
            fi
            ;;
        Darwin) echo "macos" ;;
        *)      die "Unsupported OS: $os" ;;
    esac
}

pick_url() {
    local os="$1" arch="$2"
    case "$os" in
        linux)
            case "$arch" in
                amd64) echo "$BORE_AMD64" ;;
                arm64) echo "$BORE_ARM64" ;;
            esac
            ;;
        macos)
            echo "$BORE_MACOS"
            ;;
        android)
            echo "$BORE_ANDROID"
            ;;
    esac
}

download() {
    local url="$1" dest="$2"
    if command -v curl &>/dev/null; then
        curl -fsSL --progress-bar -o "$dest" "$url"
    elif command -v wget &>/dev/null; then
        wget -q --show-progress -O "$dest" "$url"
    else
        die "Need curl or wget. Install one and retry."
    fi
}

# ── Main ─────────────────────────────────────────────────────────────────────
main() {
    local os arch url dest

    os="$(detect_os)"
    arch="$(detect_arch)"
    url="$(pick_url "$os" "$arch")"

    [[ -z "$url" ]] && die "No download URL for OS=$os ARCH=$arch. Set BORE_AMD64/BORE_ARM64/BORE_MACOS/BORE_ANDROID manually."

    dest="$TMPDIR/bore"

    echo "→ bore installer"
    echo "  OS:   $os"
    echo "  Arch: $arch"
    echo "  URL:  $url"
    echo "  Dest: $INSTALL_PATH/bore"

    echo

    echo "Downloading..."
    echo

    echo "Create install directory if needed..."
    mkdir -p "$INSTALL_PATH"
    
    download "$url" "$dest"

    chmod +x "$dest"

    if ! "$dest" --version &>/dev/null; then
        die "Downloaded binary fails --version check. URL may be wrong."
    fi

    if [[ -w "$INSTALL_PATH" ]] || [[ -w "$(dirname "$INSTALL_PATH")" ]]; then
        mv "$dest" "$INSTALL_PATH/bore"
    else
        echo "Need root to write to $INSTALL_PATH — using sudo."
        sudo mv "$dest" "$INSTALL_PATH/bore"
    fi

    echo
    echo "✓ bore installed at $INSTALL_PATH/bore"
    "$INSTALL_PATH/bore" --version
}

main "$@"
