#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
demo_root=/tmp/tuxcleaner-vhs-demo

for program in cargo vhs ttyd ffmpeg; do
    if ! command -v "$program" >/dev/null 2>&1; then
        printf 'Missing required program: %s\n' "$program" >&2
        exit 1
    fi
done

rm -rf "$demo_root"
mkdir -p \
    "$demo_root/home/.cache/pip" \
    "$demo_root/home/.npm/_cacache" \
    "$demo_root/home/Downloads" \
    "$demo_root/home/Videos" \
    "$demo_root/home/Projects/nebula-web/target" \
    "$demo_root/state"
truncate -s 96M "$demo_root/home/.cache/pip/wheels.bin"
truncate -s 184M "$demo_root/home/.npm/_cacache/index.bin"
truncate -s 1200M "$demo_root/home/Downloads/demo-archive.iso"
truncate -s 680M "$demo_root/home/Videos/sample-cut.mp4"
truncate -s 220M "$demo_root/home/Projects/nebula-web/target/demo-binary"
touch -d '60 days ago' "$demo_root/home/Projects/nebula-web/target"

cd "$project_root"
cargo build --locked --release
vhs docs/tuxcleaner-demo.tape

printf 'Rendered %s\n' "$project_root/docs/tuxcleaner-demo.gif"
