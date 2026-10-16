#!/bin/sh
# `gpud run` examples. Each command waits for the card, runs under a lease, and releases
# it on exit. The lease lives as long as the connection, so a crash cannot leak it.
#
# This file is a menu, not a script to run top to bottom. Copy the line you want.

# Anything that uses the GPU. Waits until the LLM has been quiet for llm.idle_s.
gpud run --class batch --vram 8G -- python embed_all.py

# Whole card, with a name that shows up in `gpud status`.
gpud run --class batch --vram all --name render -- ffmpeg -hwaccel cuda -i in.mp4 out.mp4

# Higher class: waits only for the request in flight, not for LLM silence.
gpud run --class interactive --vram 12G -- python generate_images.py

# A yieldable job. When something more important waits, gpud creates $GPUD_YIELD_FILE and
# sends SIGUSR1 (change with --yield-signal or [run] yield_signal). The job should save its
# state and exit; it is never killed. See train.py below and the loop that resumes it.
gpud run --class batch --vram 20G --yieldable --name train -- python train.py

# Run without a lease if gpud is not running (default: exit 69).
gpud run --fallback --class batch --vram 8G -- python embed_all.py

# Hold the card by hand, for gaming or debugging, until Ctrl-C.
gpud hold --class interactive --vram all --name manual

exit 0

# ---- train.py: a yieldable loop that checkpoints and stops --------------------------------
#
#     import os, signal, sys
#
#     yield_file = os.environ.get("GPUD_YIELD_FILE")   # set by `gpud run`; created on yield
#     asked = False
#
#     def on_yield(signum, frame):
#         global asked
#         asked = True
#     signal.signal(signal.SIGUSR1, on_yield)
#
#     def should_yield():
#         return asked or (yield_file is not None and os.path.exists(yield_file))
#
#     step = load_checkpoint_or_zero()
#     while step < TOTAL_STEPS:
#         train_one_step(step)
#         step += 1
#         if should_yield():
#             save_checkpoint(step)
#             print("yielding at step", step, flush=True)
#             sys.exit(75)          # frees the VRAM; 75 means "not done, run me again"
#
# ---- resume until done ---------------------------------------------------------------------
#
#     while true; do
#         gpud run --class batch --vram 20G --yieldable --name train -- python train.py
#         rc=$?
#         [ "$rc" -eq 75 ] || exit "$rc"
#         sleep 5
#     done
