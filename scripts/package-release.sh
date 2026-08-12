#!/bin/sh
set -eu

target="${1:?usage: scripts/package-release.sh TARGET [BINARY]}"
binary="${2:-target/${target}/release/tuxcleaner}"
output_dir="${OUTPUT_DIR:-dist}"
archive="tuxcleaner-${target}.tar.gz"

[ -f "$binary" ] || {
    printf 'missing release binary: %s\n' "$binary" >&2
    exit 1
}

mkdir -p "$output_dir"
staging=$(mktemp -d 2>/dev/null || mktemp -d -t tuxcleaner-release)
trap 'rm -rf "$staging"' EXIT HUP INT TERM

install -m 0755 "$binary" "$staging/tuxcleaner"
install -m 0644 README.md LICENSE "$staging/"
tar -czf "${output_dir}/${archive}" -C "$staging" tuxcleaner README.md LICENSE

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$output_dir" && sha256sum "$archive" > "${archive}.sha256")
else
    (cd "$output_dir" && shasum -a 256 "$archive" > "${archive}.sha256")
fi

printf '%s\n' "${output_dir}/${archive}"

