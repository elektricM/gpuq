#!/bin/sh
# The diagrams share one palette. Each NAME.svg is the source; this script puts light.css in
# its <style> block and writes NAME-dark.svg with dark.css, for GitHub's dark mode.
#   docs/diagrams/build.sh            update the styles and the dark variants
#   docs/diagrams/build.sh png DIR    also render PNGs of all of them to DIR (rsvg-convert)
set -eu
cd "$(dirname "$0")"
style() {
    awk 'FNR == NR { css = css $0 "\n"; next }
         /<style>/ { printf "<style>\n%s", css; skip = 1; next }
         /<\/style>/ { skip = 0 }
         !skip' "$1" "$2"
}
for f in *.svg; do
    case $f in *-dark.svg) continue ;; esac
    style light.css "$f" > "$f.tmp" && mv "$f.tmp" "$f"
    style dark.css "$f" > "${f%.svg}-dark.svg"
done
if [ "${1:-}" = png ]; then
    mkdir -p "$2"
    for f in *.svg; do
        case $f in *-dark.svg) bg='#0d1117' ;; *) bg='#ffffff' ;; esac
        rsvg-convert -z "${3:-2}" -b "$bg" -o "$2/${f%.svg}.png" "$f"
    done
fi
