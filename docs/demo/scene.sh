#!/usr/bin/env bash
# What docs/demo.gif shows. record.sh runs it inside asciinema against the demo daemon.
# Background jobs write their own messages to $GPUQ_DEMO/jobs.log so the terminal stays
# readable; everything printed here comes from the real gpuq client and daemon.

say() {
    printf '\033[1;32m$\033[0m '
    local i
    for ((i = 0; i < ${#1}; i++)); do
        printf '%s' "${1:i:1}"
        sleep 0.02
    done
    printf '\n'
}

# Type a command, then run it.
run() {
    say "$1"
    eval "$1"
}

# Type a command, then run it in the background with its output in jobs.log.
bg() {
    say "$1 &"
    eval "$1" >>"$GPUQ_DEMO/jobs.log" 2>&1 &
    sleep 0.6
}

comment() {
    printf '\033[2m# %s\033[0m\n' "$1"
    sleep 1.2
}

printf '\033[H\033[2J'
comment "two 24 GB cards; a 32B model is split over both"
run "gpuq status"
sleep 3
comment "jobs ask gpuq for VRAM; gpuq unloads the model and places them"
bg "gpuq run --vram 8G -- render"
bg "gpuq run --vram 8G -- render"
bg "gpuq run --vram 22G -- train"
bg "gpuq run --vram 8G -- render"
sleep 1
run "gpuq status"
sleep 4
comment "a chat request waits in gpuq's gate until the cards are free"
run "curl -s localhost:19434/api/generate -d '{\"model\":\"qwen2.5:32b\",\"prompt\":\"hi\"}'"
echo
run "gpuq status"
sleep 4
