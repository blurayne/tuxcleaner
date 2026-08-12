#!/bin/sh
set -eu

REPOSITORY="${TUXCLEANER_REPOSITORY:-debba/tuxcleaner}"
VERSION="${TUXCLEANER_VERSION:-latest}"
INSTALL_DIR="${TUXCLEANER_INSTALL_DIR:-}"
BASE_URL="${TUXCLEANER_BASE_URL:-https://github.com/${REPOSITORY}/releases}"

fail() {
    printf 'tuxcleaner installer: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

detect_target() {
    os=$(uname -s)
    [ "$os" = "Linux" ] || fail "unsupported operating system: $os (Linux is required)"

    case "$(uname -m)" in
        x86_64 | amd64) arch="x86_64" ;;
        aarch64 | arm64) arch="aarch64" ;;
        *) fail "unsupported CPU architecture: $(uname -m)" ;;
    esac

    libc="gnu"
    if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
        libc="musl"
    elif [ -e /etc/alpine-release ]; then
        libc="musl"
    fi
    printf '%s-unknown-linux-%s\n' "$arch" "$libc"
}

checksum_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail "sha256sum or shasum is required to verify the download"
    fi
}

choose_install_dir() {
    if [ -n "$INSTALL_DIR" ]; then
        printf '%s\n' "$INSTALL_DIR"
    elif [ -d "$HOME/.local/bin" ] || { [ -d "$HOME/.local" ] && [ -w "$HOME/.local" ]; }; then
        printf '%s/.local/bin\n' "$HOME"
    elif [ -w /usr/local/bin ]; then
        printf '/usr/local/bin\n'
    else
        printf '%s/.local/bin\n' "$HOME"
    fi
}

need curl
need tar
need grep
need awk
need install
need uname
need mktemp

target=$(detect_target)
archive="tuxcleaner-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
    release_url="${BASE_URL}/latest/download"
else
    case "$VERSION" in
        v*) tag="$VERSION" ;;
        *) tag="v${VERSION}" ;;
    esac
    release_url="${BASE_URL}/download/${tag}"
fi

tmp_dir=$(mktemp -d 2>/dev/null || mktemp -d -t tuxcleaner)
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

printf 'Downloading TuxCleaner for %s...\n' "$target"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    "${release_url}/${archive}" -o "${tmp_dir}/${archive}"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    "${release_url}/${archive}.sha256" -o "${tmp_dir}/${archive}.sha256"

expected=$(awk '{print $1}' "${tmp_dir}/${archive}.sha256")
actual=$(checksum_file "${tmp_dir}/${archive}")
[ -n "$expected" ] && [ "$expected" = "$actual" ] || fail "SHA-256 checksum verification failed"

tar -xzf "${tmp_dir}/${archive}" -C "$tmp_dir"
[ -f "${tmp_dir}/tuxcleaner" ] || fail "release archive does not contain tuxcleaner"

install_dir=$(choose_install_dir)
if [ -d "$install_dir" ] && [ -w "$install_dir" ]; then
    install -m 0755 "${tmp_dir}/tuxcleaner" "${install_dir}/tuxcleaner"
elif mkdir -p "$install_dir" 2>/dev/null; then
    install -m 0755 "${tmp_dir}/tuxcleaner" "${install_dir}/tuxcleaner"
elif command -v sudo >/dev/null 2>&1; then
    printf 'Installing to %s requires sudo.\n' "$install_dir"
    sudo install -d -m 0755 "$install_dir"
    sudo install -m 0755 "${tmp_dir}/tuxcleaner" "${install_dir}/tuxcleaner"
else
    fail "cannot write to ${install_dir}; set TUXCLEANER_INSTALL_DIR to a writable directory"
fi

printf 'Installed TuxCleaner to %s/tuxcleaner\n' "$install_dir"
case ":$PATH:" in
    *":${install_dir}:"*) ;;
    *) printf 'Add %s to PATH, then run: tuxcleaner\n' "$install_dir" ;;
esac
