# gpud

gpud is a small daemon that sits in front of [llama-swap](https://github.com/mostlygeek/llama-swap) and decides who gets the GPU on a machine that has only one. It lets a local LLM stay loaded for interactive use, and it hands the card to other GPU jobs (transcription, image generation, photo indexing, a training run) when they ask for it, then gives it back.

## The problem

A single consumer GPU, say 24 GB, can hold one big LLM or one big diffusion model, not both. If the LLM is always loaded, every other GPU job either fails with an out-of-memory error or forces you to stop the LLM by hand and remember to start it again. If you unload the LLM to be safe, the first chat message after a job pays for a full model load, and the conversation you were in has lost its KV cache.

Doing this by hand works for a week. gpud automates the part you keep forgetting:

- A chat request arrives while a GPU job is running: the job is asked to yield, the request waits for the card, then the model is loaded again and served.
- A GPU job wants the card while the LLM is in use: it waits until the LLM has been quiet for a while, so it does not land in the middle of a conversation.
- A job ends: the resident model is loaded again and its slot 0 (the conversation) is restored from disk.

## How it works

```
                      +------------------------------+
 chat clients  :8080  |  gate (reverse proxy)        |  :8081
 ---------------------> counts or holds LLM requests  ---------> llama-swap --> llama-server
                      |              |               |              ^
 gpu run,      :9390  |              v               |              | unload, preload,
 API clients  ---------> arbiter (state machine)  ----------------+   slot save/restore
                      |   llm -> unloading -> jobs   |
                      |    ^                    |    |
                      |    +---- restoring <----+    |  <--- VRAM probe (sysfs / nvidia-smi)
                      +------------------------------+
```

Clients keep talking to the port they already use for llama-swap. gpud listens there and forwards everything to llama-swap, which you move to a loopback port. Requests for models that run on the GPU are counted while they are in flight. Requests for CPU-only models (embeddings, for example) pass straight through, always.

The arbiter has four states. In `llm` state llama-swap owns the card. When a job is waiting and the rules allow it, gpud saves slot 0 of the resident model, tells llama-swap to unload everything, waits for the memory to come back (`unloading`), and grants leases (`jobs`). When the last lease ends and nobody else is queued, it waits a short while in case another job follows, then preloads the resident model and restores the slot (`restoring`), and goes back to `llm`.

Jobs hold a lease. A lease has a priority class, an amount of VRAM (or the whole card), and a flag saying whether the job can be asked to yield. Several leases can be active at once as long as their declared VRAM fits next to each other.

Who goes first is decided by class. The defaults are, from most to least important:

| class | meaning |
| --- | --- |
| `realtime` | ahead of the LLM: takes the card as soon as the requests in flight have finished |
| `llm` | requests for models behind the gate |
| `interactive` | waits until the LLM has been silent for `gap_s` (10 s) |
| `background` | waits until the LLM has been silent for `idle_s` (5 min) |
| `batch` | same as `background`, and the fallback for unknown names |

A waiter asks every yieldable lease of lower priority to yield. A job that is not yieldable keeps the card until it exits, and whoever is waiting waits. You can rename the classes, add more and renumber them in `[classes]`; only `llm` is required by name.

If you often get a burst of chat requests while a long batch job (say a few hundred image captions) holds the card, `batch_hold_s` makes the first request wait a bit before the job is asked to yield, so the burst is served in one hand-over instead of one per request. `batch_hold_count` ends the wait early once that many requests are queued.

## Requirements

- Linux, Python 3.11 or newer.
- llama-swap with the models you want served. For the conversation to survive a hand-over, the resident model needs `--slot-save-path` in its llama-server command line. Without it everything still works, the next chat just starts from an empty cache.
- An AMD GPU with the amdgpu driver (VRAM is read from sysfs) or an NVIDIA GPU with `nvidia-smi`.
- Optional: PyYAML, so gpud can read llama-swap's config for aliases, CPU-only models and `--slot-save-path`.

## Install

```sh
git clone https://github.com/elektricM/gpud
cd gpud
pip install ".[yaml]"
```

This installs two commands, `gpud` (the daemon) and `gpu` (the client). Use a virtualenv or `pipx` if you do not want them in the system Python. Leave out `[yaml]` if you do not want PyYAML; then list your CPU-only models and slot-save models in the config by hand (`llm.cpu_models`, `llm.slot_save_models`).

1. Move llama-swap to a loopback port. With systemd, a drop-in does it; see `contrib/llama-swap-port.conf`. The new port has to match `gate.backend` in gpud's config.
2. Copy `config.example.toml` to `/etc/gpud/config.toml` and edit it. Every key is listed there with its default and a short comment. At minimum set `llm.resident` to the model that should be loaded when the card is idle, and check `gpu.vendor`.
3. Start gpud. A unit file is in `contrib/gpud.service`. You can also run it in a terminal: `gpud -c config.toml -v`.
4. Check it: `gpu status`.

gpud reads the first of `-c FILE`, `/etc/gpud/config.toml`, `~/.config/gpud/config.toml` that exists. Tables in the file are merged over the defaults; lists and scalars replace them.

## Configuration

The full list is in `config.example.toml`. The keys you are most likely to touch:

| key | default | what it does |
| --- | --- | --- |
| `gate.port`, `gate.backend` | `8080`, `http://127.0.0.1:8081` | where gpud listens and where llama-swap lives |
| `api.port` | `9390` | control API and `gpu` CLI. Keep it on loopback. |
| `gpu.vendor` | `auto` | `amd`, `nvidia`, or `auto` |
| `gpu.pci` | empty | AMD: PCI address such as `0000:03:00.0`. Empty picks the amdgpu device with the most VRAM. |
| `gpu.nvidia_index` | `0` | NVIDIA: index passed to `nvidia-smi -i` |
| `gpu.margin_mib` | `256` | VRAM kept free on top of what leases declare |
| `llm.config` | `/etc/llama-swap/config.yaml` | llama-swap's config, read for aliases, CPU-only models and `--slot-save-path` |
| `llm.resident` | empty | model reloaded when the card goes idle. Empty means nothing is preloaded. |
| `llm.idle_s` | `300` | LLM silence required before `idle_wait_classes` jobs start |
| `llm.gap_s` | `10` | silence required before other classes below the LLM start |
| `llm.reload_delay_s` | `20` | pause after the last job before the resident model comes back |
| `llm.model_classes` | empty | schedule requests for some models as another class |
| `llm.class_tuning` | empty | per-class `gap_s` and `reload_delay_s` |
| `llm.batch_hold_s`, `llm.batch_hold_count` | `0`, `0` | the batch hold described above; `0` turns it off |
| `llm.resume_grace_s` | `30` | after a gpud restart, how long `gpu run` clients get to re-adopt their leases |
| `classes` | see above | class name to priority number, lower is more important |
| `hooks.offload` | empty | command to run when a job does not fit; see below |

VRAM size is never configured: it is read from the card.

`llm.model_classes` and `llm.class_tuning` exist for pipelines that send several requests in a row to one model. If a model's requests belong to a realtime pipeline, put it in the `realtime` class, and give the class a longer `gap_s` and `reload_delay_s` so a job does not slip in between two requests of the same pipeline:

```toml
[llm.model_classes]
"summarizer" = "realtime"

[llm.class_tuning.realtime]
gap_s = 120
reload_delay_s = 180
```

## The gpu command

```
gpu status [--json]
gpu run  [--class C] [--vram SIZE] [--yieldable] [--fallback] [--name N] -- COMMAND...
gpu hold [--class C] [--vram SIZE] [--name N]
```

`gpu run` asks gpud for a lease, waits in the queue if it has to, runs the command once the lease is granted, and releases the lease when the command exits. The exit status is the command's. If gpud is not running it exits with 69; with `--fallback` it runs the command without a lease instead.

- `--class` is one of the names in `[classes]`. The default is `batch`.
- `--vram` is an amount (`8G`, `3000M`, a plain number is MiB) or `all`, which is the default. `all` waits for an otherwise empty card, so it excludes other leases and anything else using VRAM beyond `gpu.whole_card_baseline_mib` (desktop, compositor). Declaring what the job really needs lets two small jobs share the card.
- `--yieldable` means the job may be stopped when something more important needs the card. gpud then sends it SIGTERM through `gpu run`; the command is expected to exit. Restarting it is up to you.

`gpu hold` keeps a lease until you press Ctrl-C. It is useful for keeping the card free by hand, for example while you start something that gpud does not know about.

If gpud restarts while a job runs, `gpu run` reconnects and re-adopts the lease with its old id, so the job is not interrupted.

A high-priority job is just `gpu run --class realtime`. The script `contrib/gpu-transcribe` wraps a whisper.cpp call this way: the transcription goes ahead of the LLM, runs to completion, and the LLM comes back afterwards.

`gpu status` shows the state, VRAM use, what llama-swap has loaded, active leases, the queue with the reason each entry is waiting, and the last few scheduling decisions.

## Lease API

The `gpu` command is a thin client of an HTTP API on `api.host:api.port`. Anything that can hold an HTTP connection open can take a lease.

`GET /status` returns the same data as `gpu status --json`.

`POST /lease` takes a JSON body and answers with a stream of newline-delimited JSON:

```
$ curl -N -X POST localhost:9390/lease \
    -d '{"class": "batch", "vram_mib": 8192, "yieldable": true, "name": "demo"}'
{"event": "queued"}
{"event": "granted", "id": "L3f2a1-1"}
{"event": "ping"}
{"event": "yield"}
```

Request fields: `class` (default `batch`), `vram_mib` (a number, or `-1` for the whole card), `yieldable` (default false), `name`, `pid` (informational) and `resume_id`. An unknown class or a malformed body gets a 400 with `{"error": ...}`.

Events:

- `queued`: sent at once.
- `granted`: the card is yours. `id` identifies the lease.
- `yield`: something more important is waiting. Wrap up and close the connection. Only sent for yieldable leases, once.
- `ping`: every 15 seconds, so a dead peer is noticed.

The lease lasts exactly as long as the connection. Closing it, or crashing, releases the card, so a lease cannot leak. After a gpud restart, reconnect with `resume_id` set to the old id and the lease is adopted immediately.

A minimal client in Python:

```python
import http.client, json

conn = http.client.HTTPConnection("127.0.0.1", 9390)
conn.request("POST", "/lease", json.dumps({"class": "batch", "vram_mib": 4096, "name": "mine"}))
for line in conn.getresponse():
    event = json.loads(line)
    if event["event"] == "granted":
        break
# run the GPU work here; closing the connection releases the lease
conn.close()
```

## Integrations

Both adapters are off by default and need no extra Python packages.

### ComfyUI

Set `comfyui.enabled = true`. gpud then serves ComfyUI's port (`comfyui.port`, default 8188) and forwards to ComfyUI, which you move to `comfyui.backend` (`contrib/comfyui-port.conf` shows the systemd drop-in). Clients, including the web UI, keep using the old address.

When you press Run and gpud does not hold the card, it answers the prompt at once with a prompt id and keeps the prompt itself. It shows up as pending in ComfyUI's queue panel and can be cancelled there. When the lease is granted, the prompt is submitted to ComfyUI under the same id. The lease is yieldable between renders: when the LLM or a realtime job is waiting, new prompts are held again, prompts ComfyUI had queued but not started are taken back, and the card is handed over after the render in progress. Once the ComfyUI queue has been empty for `comfyui.idle_release_s`, gpud tells ComfyUI to unload its models and releases the lease.

When a prompt loads different model files than the previous one, gpud waits for the queue to empty and then restarts ComfyUI (`comfyui.restart_cmd`) or calls its `/free` endpoint, so each model family starts on an empty card. ComfyUI sometimes keeps VRAM after `/free`; if the card does not come back to the level measured before the job, gpud runs `restart_cmd` if you set one and warns otherwise.

`comfyui.vram_mib` is what a ComfyUI lease reserves. `comfyui.class` is its priority class.

### Immich machine learning

Set `immich.enabled = true`. Immich's smart search, face detection and OCR queues can be very large after an import, and the CPU container is slow. This adapter runs a second, GPU-enabled machine-learning container only while gpud grants a lease for it.

It polls Immich's queue statistics. When the backlog in the ML queues reaches `immich.min_backlog`, it pauses the queues, asks for a yieldable lease in `immich.class`, starts the container with `docker compose`, waits until it is healthy, raises the queue concurrency to `immich.gpu_concurrency`, and resumes the queues. When the backlog is done, or gpud asks to yield, it pauses the queues, lets active jobs finish, stops the container, and puts the concurrency back. A small leftover backlog (up to `immich.cpu_ok_backlog`) is resumed on the CPU container; a large one stays paused until the next lease.

You need an Immich API key in a file (`immich.key_file`), a compose file with a GPU ML service in a profile (`contrib/immich-gpu.compose.yml` is a starting point), and both ML URLs in Immich's settings, GPU first and CPU second. gpud's user must be able to run `docker compose`.

### Offload hook

If a waiting job needs more VRAM than is free and no lease is active, something other than gpud is using the card, typically a desktop session. Set `hooks.offload` to a command that frees it, for example one that moves the compositor onto the integrated GPU. gpud runs it once per waiting job, waits `hooks.offload_settle_s`, and measures again. The command is executed directly, not through a shell. See `contrib/offload-hook.example`.

gpud does not undo what the hook did. If the hook stops your display, something else has to bring it back.

## Limitations

- Restarting gpud drops in-flight proxied streams. Leases survive because `gpu run` clients re-adopt them.
- llama.cpp's prompt cache outside slot 0 (`--cache-ram`) is lost when llama-swap unloads the model. Only slot 0 is saved and restored, so a second concurrent conversation starts cold.
- Slot save can be refused for some models, multimodal ones for instance. gpud logs a warning and carries on without a restore.
- Requests held behind a job that cannot yield wait as long as the job runs. Clients with short timeouts will give up first. A held request that is abandoned leaves the queue.
- gpud decides from what jobs declare and from VRAM readings. It cannot stop a job that uses more than it declared.
- The NVIDIA probe runs `nvidia-smi` as a subprocess on every reading, with a 0.2 s cache. It blocks the event loop for the duration of the call, which is short in practice.
- Without PyYAML, llama-swap's config is not parsed. Aliases are not resolved automatically, and CPU-only and slot-save models have to be listed in gpud's config.
- The Immich adapter depends on the Immich queues API as it is in recent releases, and may need changes when that API moves.
- Only one GPU is managed per gpud instance.

## Development

```sh
pip install -e ".[yaml]"
python3 -m unittest discover -s tests
```

The tests run the whole stack (gate, arbiter, control API, `gpu` CLI) against a fake llama-swap and a fake sysfs, so they need no GPU. Layout:

```
gpud/
  config.py      defaults and TOML loading
  vram.py        VRAM probes (AMD sysfs, nvidia-smi)
  llamaswap.py   llama-swap client and model table
  leases.py      Lease and Waiter types
  arbiter.py     the scheduler
  proxy.py       reverse proxy base
  gate.py        the LLM-facing proxy
  api.py         control API
  cli.py         the gpu command
  hooks.py       offload hook runner
  adapters/      ComfyUI and Immich
contrib/         systemd units, drop-ins, compose and config examples
tests/
```

## License

Copyright (C) 2026 Martin Dolez

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details. The full text is in `LICENSE`.
