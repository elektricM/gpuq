#!/bin/sh
# Pretend VRAM probe for the demo: prints "used_mib total_mib" for a 24 GB card named $1.
# Used VRAM is a 600 MiB baseline plus the MiB written by the demo programs into
# $GPUQ_DEMO/vram/<card>/<pid>.<tag> while that pid is alive.
dir="${GPUQ_DEMO:-/tmp/gpuq-demo}/vram/$1"
used=600
for f in "$dir"/*; do
    [ -f "$f" ] || continue
    name=${f##*/}
    if kill -0 "${name%%.*}" 2>/dev/null; then
        used=$((used + $(cat "$f")))
    else
        rm -f "$f"
    fi
done
echo "$used 24576"
