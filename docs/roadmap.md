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

## Multi-GPU leftovers

Several cards work: one queue, a probe and an env per card, leases that name a card or take
`any` (see `integrations.md`). Still to do:

- Several LLM servers, one per card, each behind its own gate. Today the LLM lives on one card
  (`llm.card`).
- Automatic `visible_devices`. You write the device number or UUID per card; gpud could work
  out the mapping from PCI addresses.
- Multi-card `auto`. `vendor = "auto"` only works with a single card, so each card names its
  vendor.

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
