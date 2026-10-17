# Roadmap

Nothing here is implemented. It is the order I would do it in, and what works today instead.

## Desktop and game detection

Taking the card back for the desktop (a game, a video call, a compositor that needs VRAM)
should be automatic: detect the foreground GPU user, ask everything to yield, hand the card
back afterwards.

Today it is manual. The `interactive` class outranks `background` and `batch`, and
`gpuq hold --class interactive --vram all` takes the whole card until you press Ctrl-C.
Yieldable jobs get asked to give it up; the LLM is unloaded. Wrap a game launcher in
`gpuq run --class interactive --vram all -- game` to get the same without remembering.

## Multi-GPU leftovers

Several cards work: every GPU is detected, one queue, leases on one card, on any card or on
several cards at once, and an LLM split over several cards (see `integrations.md`). Still to
do:

- Several LLM servers, one per card, each behind its own gate. Today there is one LLM backend;
  it can span several cards (`llm.card` as a list) but not run twice.
- Vulkan device numbers. Detected cards get `CUDA_VISIBLE_DEVICES` or `ROCR_VISIBLE_DEVICES`;
  Vulkan programs (`GGML_VK_VISIBLE_DEVICES`) are still pinned by hand.
- Reading llama-swap's device options. gpuq takes a model's cards from `[llm.models]` or from
  its estimated size; it could read `-dev`, `--tensor-split` and the `*_VISIBLE_DEVICES` of each
  model's command instead.
- Reserving fewer cards for `--cards N`. A waiting `--cards 2` lease is first in line on every
  card it could use, not only on two of them.

## Web UI

A small page served by the control API: the card's VRAM, which program holds it, the queue
with each waiting lease and its class, and the services with their state. Buttons to hold the
card, release a lease or ask a job to yield, so day-to-day use does not need the CLI. It should
ship inside the binary (no separate frontend build) and stay on loopback by default, like the
API itself. `GET /status` already returns most of what it needs.

Today: `gpuq status`, or `GET /status` on the control API.

## Metrics

A Prometheus endpoint: state, queue length, lease counts per class, wait times, VRAM
readings, unload and load durations. `GET /status` has most of the data already.

## Config reload

SIGHUP is logged and ignored; changes need a restart. A reload should swap classes, tuning and
services without dropping live leases. Changing listen ports will keep needing a restart.

## Intel VRAM probe

Intel is not supported because there is no single simple place to read VRAM from for both
iGPUs and Arc cards. Until then, use `gpu.vendor = "command"` with a script that prints
`used_mib total_mib`.
