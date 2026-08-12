#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM

if ! command -v magick >/dev/null 2>&1; then
    printf 'Missing required program: magick\n' >&2
    exit 1
fi

cd "$project_root"
magick -background none docs/tuxcleaner-social-preview.svg "$work_dir/background.png"
magick "$work_dir/background.png" \
    \( docs/demo/tuxcleaner-menu.png -resize '568x367!' \) \
    -geometry +664+136 -compose over -composite \
    docs/tuxcleaner-social-preview.png

printf 'Rendered %s\n' "$project_root/docs/tuxcleaner-social-preview.png"
