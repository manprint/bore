#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

# ── Config ───────────────────────────────────────────────────────────────────
BORE_AMD64="https://github.com/manprint/bore/releases/latest/download/bore-x86_64-unknown-linux-musl"
BORE_ARM64="https://github.com/manprint/bore/releases/latest/download/bore-aarch64-unknown-linux-musl"
BORE_MACOS="https://github.com/manprint/bore/releases/latest/download/bore-x86_64-apple-darwin"
BORE_ANDROID="https://github.com/manprint/bore/releases/latest/download/bore-aarch64-linux-android"

USER_INSTALL_PATH="$HOME/.bin"
INSTALL_PATH="${INSTALL_PATH:-$USER_INSTALL_PATH}"
die() { echo "ERROR: $*" >&2; exit 1; }

# Create a safe temporary directory; fall back to /tmp when mktemp variants differ
TMPDIR="$(mktemp -d 2>/dev/null || mktemp -d -t bore 2>/dev/null || printf '/tmp/bore-install-%s' "$$")"
if [[ -z "${TMPDIR:-}" || ! -d "$TMPDIR" ]]; then
    die "Failed to create temporary directory"
fi

# ── Helpers ──────────────────────────────────────────────────────────────────
cleanup() {
    if [[ -n "${TMPDIR:-}" && -d "$TMPDIR" ]]; then
        rm -rf "$TMPDIR"
    fi
}

trap cleanup EXIT

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
    local uname_s uname_o
    uname_s="$(uname -s 2>/dev/null || true)"
    uname_o="$(uname -o 2>/dev/null || true)"

    # macOS / Darwin
    if [[ "$uname_s" == "Darwin" ]]; then
        echo "macos"
        return 0
    fi

    # Windows-like environments (Cygwin/Mingw/MSYS) — unsupported here
    case "$uname_s" in
        *CYGWIN*|*MINGW*|*MSYS*|*Windows*)
            die "Unsupported OS: $uname_s (Windows-like environments are not supported)"
            ;;
    esac

    # Linux-family (including WSL and Android)
    if [[ "$uname_s" == "Linux" ]] || [[ "$uname_o" == *Linux* ]] || [[ -n "${uname_o:-}" ]]; then
        # Android detection heuristics
        if [[ -f /system/build.prop ]] || [[ -n "${ANDROID_ROOT:-}" ]]; then
            echo "android"
            return 0
        fi

        if command -v getprop >/dev/null 2>&1; then
            if getprop ro.build.version.release 2>/dev/null | grep -q .; then
                echo "android"
                return 0
            fi
        fi

        # WSL detection (Microsoft string in /proc/version) — treat as linux but note it
        if [[ -f /proc/version ]] && grep -qi microsoft /proc/version 2>/dev/null; then
            echo "linux"
            return 0
        fi

        # Generic Linux fallback
        echo "linux"
        return 0
    fi

    die "Unsupported OS: $uname_s"
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
    if command -v curl >/dev/null 2>&1; then
        curl --fail --silent --show-error --location --max-time 60 --retry 3 --retry-delay 2 -o "$dest" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget --timeout=60 --tries=3 -q -O "$dest" "$url"
    else
        die "Need curl or wget. Install one and retry."
    fi
    if [[ ! -s "$dest" ]]; then
        die "Download failed or resulted in empty file: $url"
    fi
}

detect_shell_rc() {
    local shell_base bash_rc zsh_rc

    bash_rc="$HOME/.bashrc"
    zsh_rc="$HOME/.zshrc"
    shell_base="${SHELL##*/}"

    case "$shell_base" in
        bash)
            [[ -f "$bash_rc" ]] && { printf '%s\n' "$bash_rc"; return 0; }
            [[ -f "$zsh_rc" ]] && { printf '%s\n' "$zsh_rc"; return 0; }
            ;;
        zsh)
            [[ -f "$zsh_rc" ]] && { printf '%s\n' "$zsh_rc"; return 0; }
            [[ -f "$bash_rc" ]] && { printf '%s\n' "$bash_rc"; return 0; }
            ;;
        *)
            [[ -f "$bash_rc" ]] && { printf '%s\n' "$bash_rc"; return 0; }
            [[ -f "$zsh_rc" ]] && { printf '%s\n' "$zsh_rc"; return 0; }
            ;;
    esac

    return 1
}

rc_has_install_path() {
    local rc_file="$1" install_path="$2" home_prefix home_relative

    if grep -Fq "$install_path" "$rc_file"; then
        return 0
    fi

    home_prefix="${HOME%/}"
    case "$install_path" in
        "$home_prefix"/*)
            home_relative="${install_path#"$home_prefix"/}"
            if grep -Fq "\$HOME/$home_relative" "$rc_file" || grep -Fq "~/$home_relative" "$rc_file"; then
                return 0
            fi
            ;;
    esac

    return 1
}

append_path_to_shell_rc() {
    local rc_file="$1" install_path="$2"

    {
        printf '\n# Added by bore installer\n'
        printf 'export PATH="$PATH:%s"\n' "$install_path"
    } >> "$rc_file" || die "Failed to update $rc_file"
}

update_shell_path() {
    local install_path="$1" rc_file

    if ! rc_file="$(detect_shell_rc)"; then
        echo "  PATH: nessun file ~/.bashrc o ~/.zshrc trovato, salto l'aggiornamento automatico"
        return 1
    fi

    if rc_has_install_path "$rc_file" "$install_path"; then
        echo "  PATH: già presente in $rc_file, nessuna modifica"
        return 0
    fi

    append_path_to_shell_rc "$rc_file" "$install_path"
    echo "  PATH: aggiunto automaticamente a $rc_file"
    return 0
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
    mkdir -p "$INSTALL_PATH" || die "Failed to create install directory: $INSTALL_PATH"

    download "$url" "$dest"

    chmod +x "$dest" || die "Failed to mark downloaded file as executable: $dest"

    if ! "$dest" --version >/dev/null 2>&1; then
        die "Downloaded binary fails --version check. URL may be wrong."
    fi

    # Install the binary (use sudo if we cannot write to the destination)
    if [[ -w "$INSTALL_PATH" ]] || [[ -w "$(dirname "$INSTALL_PATH")" ]]; then
        mv "$dest" "$INSTALL_PATH/bore" || die "Failed to move file to $INSTALL_PATH/bore"
    else
        echo "Need root to write to $INSTALL_PATH — using sudo."
        if command -v sudo >/dev/null 2>&1; then
            sudo mv "$dest" "$INSTALL_PATH/bore" || die "Failed to move file to $INSTALL_PATH/bore (sudo)"
        else
            die "Need root to write to $INSTALL_PATH and 'sudo' is not available. Run the script as root or set INSTALL_PATH to a writable directory."
        fi
    fi

    echo
    echo "✓ bore installed at $INSTALL_PATH/bore"
    "$INSTALL_PATH/bore" --version
    update_shell_path "$INSTALL_PATH" || true
    echo "Done!"
}

main "$@"
