#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
demo_root=/tmp/tuxcleaner-vhs-demo

for program in cargo vhs ttyd ffmpeg magick; do
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
magick -background none docs/demo/adwaita-frame.svg "$demo_root/adwaita-frame.png"
ffmpeg -loglevel error -y \
    -loop 1 -framerate 25 -i "$demo_root/adwaita-frame.png" \
    -i "$demo_root/raw.gif" \
    -filter_complex '[0:v][1:v]overlay=40:96:shortest=1,split[frames][palette_source];[palette_source]palettegen=stats_mode=diff[palette];[frames][palette]paletteuse=dither=bayer:bayer_scale=3' \
    -shortest -loop 0 docs/tuxcleaner-demo.gif
ffmpeg -loglevel error -y -ss 2.5 -i docs/tuxcleaner-demo.gif \
    -vf 'crop=1200:776:40:40' -frames:v 1 docs/demo/tuxcleaner-menu.png
scripts/render-social-preview.sh

printf 'Rendered %s\n' "$project_root/docs/tuxcleaner-demo.gif"
