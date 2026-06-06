#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

BORE_RELEASE_BASE="${BORE_RELEASE_BASE:-https://github.com/manprint/bore/releases/latest/download}"

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

info() {
    printf '%s\n' "$*"
}

make_tmpdir() {
    local tmpdir

    if tmpdir="$(mktemp -d 2>/dev/null)"; then
        printf '%s\n' "$tmpdir"
        return 0
    fi

    if tmpdir="$(mktemp -d -t bore-live 2>/dev/null)"; then
        printf '%s\n' "$tmpdir"
        return 0
    fi

    tmpdir="/tmp/bore-live-$$"
    mkdir -p "$tmpdir"
    printf '%s\n' "$tmpdir"
}

cleanup() {
    if [[ -n "${TMPDIR:-}" && -d "$TMPDIR" ]]; then
        rm -rf "$TMPDIR"
    fi
}

detect_os() {
    local uname_s uname_o

    uname_s="$(uname -s 2>/dev/null || true)"
    uname_o="$(uname -o 2>/dev/null || true)"

    case "$uname_s" in
        Darwin)
            printf '%s\n' "macos"
            return 0
            ;;
        *CYGWIN*|*MINGW*|*MSYS*|*Windows*)
            printf '%s\n' "windows"
            return 0
            ;;
    esac

    if [[ "$uname_s" == "Linux" ]] || [[ "$uname_o" == *Linux* ]] || [[ -n "${ANDROID_ROOT:-}" ]]; then
        if [[ -f /system/build.prop ]] || [[ -n "${ANDROID_ROOT:-}" ]]; then
            printf '%s\n' "android"
            return 0
        fi

        if command -v getprop >/dev/null 2>&1; then
            if getprop ro.build.version.release >/dev/null 2>&1; then
                printf '%s\n' "android"
                return 0
            fi
        fi

        printf '%s\n' "linux"
        return 0
    fi

    die "Unsupported operating system: ${uname_s:-unknown}"
}

detect_arch() {
    local arch

    arch="$(uname -m 2>/dev/null || true)"
    case "$arch" in
        x86_64|amd64)
            printf '%s\n' "x86_64"
            ;;
        i386|i686)
            printf '%s\n' "i686"
            ;;
        aarch64|arm64)
            printf '%s\n' "aarch64"
            ;;
        *)
            die "Unsupported architecture: ${arch:-unknown}"
            ;;
    esac
}

path_mode_for_os() {
    case "$1" in
        windows) printf '%s\n' "Windows (backslash paths)" ;;
        *)       printf '%s\n' "POSIX (slash paths)" ;;
    esac
}

release_asset_for() {
    local os="$1" arch="$2"

    case "$os:$arch" in
        linux:x86_64)
            printf '%s\n' "bore-x86_64-unknown-linux-musl"
            ;;
        linux:aarch64)
            printf '%s\n' "bore-aarch64-unknown-linux-musl"
            ;;
        macos:x86_64)
            printf '%s\n' "bore-x86_64-apple-darwin"
            ;;
        macos:aarch64)
            printf '%s\n' "bore-aarch64-apple-darwin"
            ;;
        android:aarch64)
            printf '%s\n' "bore-aarch64-linux-android"
            ;;
        windows:x86_64)
            printf '%s\n' "bore-x86_64-pc-windows-msvc.exe"
            ;;
        windows:i686)
            printf '%s\n' "bore-i686-pc-windows-msvc.exe"
            ;;
        *)
            die "No live-bore asset for OS=$os ARCH=$arch"
            ;;
    esac
}

download() {
    local url="$1" dest="$2"

    if command -v curl >/dev/null 2>&1; then
        curl --fail --silent --show-error --location --connect-timeout 15 --max-time 60 --retry 3 --retry-delay 2 -o "$dest" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget --timeout=60 --tries=3 -q -O "$dest" "$url"
    else
        die "Need curl or wget to run bore live"
    fi

    if [[ ! -s "$dest" ]]; then
        die "Download failed or returned an empty file: $url"
    fi
}

prepare_binary() {
    local url="$1" dest="$2"

    download "$url" "$dest"
    chmod +x "$dest"

    if ! "$dest" --version >/dev/null 2>&1; then
        die "Downloaded bore binary failed the --version check"
    fi
}

main() {
    local os arch path_mode asset url binary_status

    [[ -n "${BASH_VERSION:-}" ]] || die "This script requires bash"

    TMPDIR="$(make_tmpdir)"
    trap cleanup EXIT INT TERM HUP

    os="$(detect_os)"
    arch="$(detect_arch)"
    path_mode="$(path_mode_for_os "$os")"
    asset="$(release_asset_for "$os" "$arch")"
    url="${BORE_RELEASE_BASE%/}/$asset"

    info "-> bore live runner"
    info "  OS:         $os"
    info "  Arch:       $arch"
    info "  Path mode:   $path_mode"
    info "  Release:     $asset"
    info "  Cleanup:     automatic on exit"

    prepare_binary "$url" "$TMPDIR/$asset"

    if [[ $# -eq 0 ]]; then
        info
        info "No bore command supplied; showing bore help."
        "$TMPDIR/$asset" --help
        return 0
    fi

    info
    info "Launching bore..."

    if "$TMPDIR/$asset" "$@"; then
        binary_status=0
    else
        binary_status=$?
    fi

    return "$binary_status"
}

main "$@"