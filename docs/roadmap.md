# Roadmap

Nothing here is implemented. It is the order I would do it in, and what works today instead.

## Desktop and game detection

Taking the card back for the desktop (a game, a video call, a compositor that needs VRAM)
should be automatic: detect the foreground GPU user, ask everything to yield, hand the card
back afterwards.

Today it is manual. The `interactive` class outranks `background` and `batch`, and
`gpud hold --class interactive --vram all` takes the whole card until you press Ctrl-C.
Yieldable jobs get asked to give it up; the LLM is unloaded. Wrap a game launcher in
`gpud run --class interactive --vram all -- game` to get the same without remembering.

## Multi-GPU

gpud arbitrates one card. The goal is one queue for several cards: a lease names a card or
takes any card with enough free VRAM, and each LLM server or service is pinned to its card. The config already has a `gpu.device` field and the state carries a
device, so leases can later name a card. Needed: one probe per device, per-device VRAM
accounting, and a way to say which backend lives on which card.

## Web UI

A small page served by the control API: the card's VRAM, which program holds it, the queue
with each waiting lease and its class, and the services with their state. Buttons to hold the
card, release a lease or ask a job to yield, so day-to-day use does not need the CLI. It should
ship inside the binary (no separate frontend build) and stay on loopback by default, like the
API itself. `GET /status` already returns most of what it needs.

Today: `gpud status`, or `GET /status` on the control API.

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

## Immich queue-aware adapter

The Python prototype paused Immich's job queues based on their backlog, so indexing ran when
the card was free and not while it was in use. Today Immich ML is a gated managed service
(`examples/immich-ml.toml`): the card is taken per `/predict` request and released when the
requests stop. An adapter that talks to Immich's API could do better: pause and resume queues
by backlog instead of waiting for requests, and avoid starting the container for a single photo.
