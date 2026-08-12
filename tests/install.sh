#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
test_root=$(mktemp -d 2>/dev/null || mktemp -d -t tuxcleaner-install-test)
trap 'rm -rf "$test_root"' EXIT HUP INT TERM

release_dir="$test_root/releases/latest/download"
fixture_dir="$test_root/fixture"
fake_bin="$test_root/bin"
install_dir="$test_root/installed"
mkdir -p "$release_dir" "$fixture_dir" "$fake_bin" "$install_dir"

printf '#!/bin/sh\nprintf "fixture tuxcleaner\\n"\n' > "$fixture_dir/tuxcleaner"
chmod 0755 "$fixture_dir/tuxcleaner"
cp "$project_root/README.md" "$project_root/LICENSE" "$fixture_dir/"
archive="tuxcleaner-x86_64-unknown-linux-gnu.tar.gz"
tar -czf "$release_dir/$archive" -C "$fixture_dir" tuxcleaner README.md LICENSE
(cd "$release_dir" && sha256sum "$archive" > "$archive.sha256")

cat > "$fake_bin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
    -s) printf 'Linux\n' ;;
    -m) printf 'x86_64\n' ;;
    *) printf 'Linux\n' ;;
esac
EOF

cat > "$fake_bin/ldd" <<'EOF'
#!/bin/sh
printf 'ldd (GNU libc) 2.39\n'
EOF

cat > "$fake_bin/curl" <<'EOF'
#!/bin/sh
output=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) output="$2"; shift 2 ;;
        http://* | https://*) url="$1"; shift ;;
        *) shift ;;
    esac
done
[ -n "$output" ] && [ -n "$url" ] || exit 2
cp "${FIXTURE_RELEASE_DIR}/${url##*/}" "$output"
EOF
chmod 0755 "$fake_bin/uname" "$fake_bin/ldd" "$fake_bin/curl"

PATH="$fake_bin:$PATH" \
FIXTURE_RELEASE_DIR="$release_dir" \
TUXCLEANER_BASE_URL="https://fixture.invalid/releases" \
TUXCLEANER_INSTALL_DIR="$install_dir" \
sh "$project_root/install.sh"

test -x "$install_dir/tuxcleaner"
output=$($install_dir/tuxcleaner)
test "$output" = "fixture tuxcleaner"
printf 'installer test passed\n'

