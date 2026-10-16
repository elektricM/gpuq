#!/bin/sh
# A realtime-class job: transcribe an audio file on the GPU, ahead of the LLM.
#
# The "realtime" class outranks "llm", so this does not wait for the LLM to go quiet: it
# waits for the request in flight, takes the card, and gives it back when whisper exits.
# The job is not yieldable, so it always runs to completion. Reserve only what it needs so
# a second job could share the card.
#
# Change the model path and the --vram figure. If gpud is not running, `gpud run` exits
# with 69; add --fallback to run without a lease instead.
#
# Usage: whisper.sh recording.wav
set -eu
exec gpud run --class realtime --vram 4G --name transcribe -- \
    whisper-cli -m /models/ggml-large-v3-turbo.bin -f "$1" -otxt
