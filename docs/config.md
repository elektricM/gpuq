# Configuration reference

gpuq reads one TOML file. `gpuq daemon -c FILE` and `gpuq check-config -c FILE` take it
explicitly. Without `-c` the first existing file of these wins: `/etc/gpuq/config.toml`,
then `$XDG_CONFIG_HOME/gpuq/config.toml` (or `~/.config/gpuq/config.toml`). With no file at all
the built-in defaults apply.

Rules:

- Every key is optional. Tables merge into the defaults; lists and scalars replace them.
- Unknown keys are an error, so a typo does not silently do nothing.
- Durations are seconds (`_s` suffix), integer or float.
- Run `gpuq check-config` after every edit. It prints what it understood and reads the VRAM
  probe once. `--strict` makes a failing probe an error.
- There is no reload. Restart gpuq after editing (SIGHUP is logged and ignored).
- `examples/gpuq.toml` lists every key with its default and is checked against the code in the
  test suite.

## [gate]

The reverse proxy in front of the LLM server.

| Key | Default | Meaning |
|---|---|---|
| `host` | `"127.0.0.1"` | Listen address. `"*"` listens on all interfaces. |
| `port` | `8080` | Listen port. Clients keep pointing at the port they already use. |
| `backend` | `"http://127.0.0.1:8081"` | Where requests are forwarded: the real LLM server, moved to this address. |

## [api]

The control API used by the `gpuq` CLI and by lease clients (see `lease-api.md`). Keep it on
loopback: anyone who can reach it can take the card.

| Key | Default | Meaning |
|---|---|---|
| `host` | `"127.0.0.1"` | Listen address. |
| `port` | `9390` | Listen port. CLI: `--api HOST:PORT` or env `GPUQ_API`. |

## [gpu]

Which cards gpuq manages, how VRAM is read and how much room to leave.

By default gpuq detects every GPU on the machine and manages all of them (see
[Detected cards](#detected-cards)). You only write something to leave a card out, usually the
CPU's integrated GPU. Pinning one device in this table (`pci`, `nvidia_index`, or `vendor =
"command"` or `"intel"`) turns detection off and describes a single card, as before 0.3.0.
`[[gpu.card]]` tables, when present, define the set of cards exactly.

| Key | Default | Meaning |
|---|---|---|
| `exclude` | `[]` | PCI addresses of detected GPUs to leave alone, such as `["0000:15:00.0"]`. The short form `"15:00.0"` works too. `gpuq check-config` lists the addresses. |
| `skip_integrated` | `false` | Leave every integrated GPU alone. |
| `vendor` | `"auto"` | `auto`, `amd`, `nvidia`, `intel` or `command`. With detection, `auto` finds AMD and NVIDIA cards, `amd` or `nvidia` only that vendor. Without it, `auto` tries AMD sysfs, then `nvidia-smi` on PATH, then `command` if `gpu.command` is set. `intel` is not supported yet; use `command`. |
| `pci` | `""` | AMD: manage only the card at this PCI address, such as `0000:03:00.0`. |
| `sysfs_root` | `"/sys/bus/pci/devices"` | AMD: where to look for devices. |
| `nvidia_index` | unset | NVIDIA: manage only the card with this `nvidia-smi -i` index. |
| `nvidia_smi` | `"nvidia-smi"` | NVIDIA: the binary to run. |
| `command` | `[]` | Argv of a program that prints `used_mib total_mib` (an optional third number is GTT in MiB). Required for `vendor = "command"`. |
| `device` | unset | Name of the card when there is only one. Default `gpu0`. |
| `margin_mib` | `256` | VRAM kept free on top of what leases declare. |
| `settle_timeout_s` | `60` | How long to wait for the backend to report its models unloaded. |
| `driver_settle_s` | `2` | Pause after an unload so the driver releases VRAM. |
| `whole_card_baseline_mib` | `512` | `--vram all` needs at most this much used by other processes. |

### Detected cards

With no `[[gpu.card]]` and no single device pinned in `[gpu]`, gpuq looks for:

- AMD cards: every PCI device under `sysfs_root` with amdgpu's `mem_info_vram_total`.
- NVIDIA cards: every line of `nvidia-smi --query-gpu=index,pci.bus_id,uuid,memory.total`.

Cards are ordered discrete first, then by VRAM (largest first), then by PCI address, and named
`gpu0`, `gpu1`, ... in that order after exclusions. A single card is named `gpu.device` if set.
`gpu0` is the primary card, so the LLM goes to the largest card unless `llm.card` says
otherwise. If nothing is detected, the `[gpu]` probe settings describe one card as before.

Each card is identified by its PCI address, never by an assumed device number. HIP, CUDA and
Vulkan each number devices their own way, and the numbers can change after a driver update
(an integrated GPU can become device 0). gpuq works out the env that points a program at each
card:

| Vendor | Variable | Value |
|---|---|---|
| NVIDIA | `CUDA_VISIBLE_DEVICES` | The GPU UUID, `GPU-...`. CUDA accepts it in place of an index, whatever `CUDA_DEVICE_ORDER` says. |
| AMD | `ROCR_VISIBLE_DEVICES` | `GPU-<unique_id>` from sysfs, the UUID `rocminfo` shows. Cards without a `unique_id` (integrated GPUs) get their ROCm index, read from the KFD topology (`/sys/class/kfd/kfd/topology/nodes`) by PCI address. |

Only `ROCR_VISIBLE_DEVICES` is set for detected AMD cards. HIP numbers the cards it is given
from 0 again, so setting `HIP_VISIBLE_DEVICES` to the same value as well would point at the
wrong device. Vulkan programs (for example llama.cpp built with Vulkan) ignore both variables;
pin them by hand.

An AMD GPU counts as integrated when sysfs has no `mem_info_vram_vendor` for it: amdgpu creates
that file only for cards with their own memory chips, which an APU's GPU does not have.
`gpuq check-config` shows `integrated` next to such a card. If your hardware is reported
wrongly, exclude the card by PCI address instead of relying on `skip_integrated`.

A detected card without a device string (an integrated GPU whose KFD topology gpuq cannot
read) can still be named with `--card`, but `--card any` skips it.

```toml
[gpu]
exclude = ["0000:15:00.0"]   # the CPU's integrated GPU
```

`gpuq check-config` prints every detected card, its PCI address, size and env, and the
excluded ones:

```
gpus: 3 detected, 2 in use
  gpu0: amd pci=0000:03:00.0 24560 MiB, ROCR_VISIBLE_DEVICES=GPU-1111aaaa2222bbbb, margin 256 MiB (primary) (llm)
  gpu1: amd pci=0000:0c:00.0 16368 MiB, ROCR_VISIBLE_DEVICES=GPU-3333cccc4444dddd, margin 256 MiB
  excluded: amd 0000:15:00.0 2048 MiB (gpu.exclude)
```

Card names used elsewhere (`llm.card`, `[llm.models]`, `[[service]]`, `[immich]`) are checked
against the detected cards when gpuq starts and by `gpuq check-config`.

### [[gpu.card]]

To list the cards yourself, write one `[[gpu.card]]` table per GPU. They define the set
exactly: nothing is detected, and `gpu.exclude` and `gpu.skip_integrated` do not apply.
`margin_mib`, `settle_timeout_s`, `driver_settle_s` and `whole_card_baseline_mib` in `[gpu]`
stay the defaults for all cards. The first card is the primary card.

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Unique; `a-z`, `0-9`, `_` and `-`; not `any`, `all` or `primary`. Used in `--card`, status and the API. |
| `vendor` | `"auto"` | `amd`, `nvidia`, `intel` or `command`. `auto` is accepted only when there is a single card. |
| `pci`, `nvidia_index`, `command` | as in `[gpu]` | The probe options of this card. |
| `sysfs_root`, `nvidia_smi` | the `[gpu]` value | Probe options that default to the `[gpu]` table. |
| `visible_devices` | unset | The device list that points a program at this card, for example `"1"` or a GPU UUID. Exported to lease holders, see below. |
| `visible_devices_env` | by vendor | Variables to export `visible_devices` as, for stacks that use other names. Needs `visible_devices`. |
| `margin_mib` | `gpu.margin_mib` | VRAM kept free on this card. |
| `whole_card_baseline_mib` | `gpu.whole_card_baseline_mib` | Baseline for `--vram all` on this card. |

`visible_devices` becomes the card's env by vendor:

| Vendor | Variables set to `visible_devices` |
|---|---|
| `nvidia` | `CUDA_VISIBLE_DEVICES` |
| `amd` | `HIP_VISIBLE_DEVICES` and `ROCR_VISIBLE_DEVICES` |
| `command`, `intel` | `CUDA_VISIBLE_DEVICES` |

For AMD with an index other than 0, set `visible_devices_env = ["ROCR_VISIBLE_DEVICES"]`, or
use the card's UUID: when both variables carry the same index, HIP applies it a second time to
the cards ROCm already filtered.

The env is sent in the `granted` event, exported by `gpuq run`, and passed to the start
command of a managed service of `kind = "command"`. A card without `visible_devices` can be
named (`--card gpu1`) but is skipped by `--card any` and by jobs that name no card, because
gpuq could not tell the job where it landed. If no card has `visible_devices`, `--card any` is
refused, jobs that name no card go to the primary card, and `gpuq check-config` says so.

```toml
[[gpu.card]]
name = "amd"
vendor = "amd"
pci = "0000:03:00.0"
visible_devices = "0"

[[gpu.card]]
name = "nvidia"
vendor = "nvidia"
nvidia_index = 0
visible_devices = "0"
```

A full file is in `examples/multi-gpu.toml`. `gpu.device` cannot be combined with
`[[gpu.card]]`.

### Leases on several cards

A lease can hold several cards at once: `--card gpu0,gpu1`, `--card all` or `--cards N`
(`"cards"`, `"card": "all"` or `"card_count"` in the lease API). Its env is the env of its
cards merged in grant order: a variable set by several cards gets their values joined with
commas, for example `CUDA_VISIBLE_DEVICES=GPU-a,GPU-b`. `GPUQ_CARD` is `gpu0,gpu1`.

Cards of different vendors can share a lease only when their env uses different variables
(detected AMD and NVIDIA cards do: `ROCR_VISIBLE_DEVICES` and `CUDA_VISIBLE_DEVICES`). Two
cards of different vendors that set the same variable are refused, because the same number
means a different device to each stack. Every card of a multi-card lease needs an env.

## [hooks]

| Key | Default | Meaning |
|---|---|---|
| `offload` | `[]` | Argv run once per waiting job and card when the job does not fit on an otherwise idle card, for example to move a desktop session off the card. Run directly, not through a shell, with `GPUQ_CARD` set to the card name. Empty disables it. |
| `offload_timeout_s` | `180` | Kill the hook after this long. |
| `offload_settle_s` | `3` | Wait this long after the hook before measuring VRAM again. |

## [llm]

The LLM server behind the gate and when it gives up the card.

| Key | Default | Meaning |
|---|---|---|
| `backend` | `"llama-swap"` | `llama-swap`, `ollama`, `llama-server`, `vllm`, `sglang` or `none`. `none` means no LLM gate: gpuq only arbitrates leases and managed services. `llama-server`, `vllm` and `sglang` need `[llm.service]`. |
| `llama_swap_config` | `"/etc/llama-swap/config.yaml"` | llama-swap's own YAML, read for aliases and CPU-only models. The old name `config` still works. |
| `card` | the primary card | The card the LLM server runs on, or a list such as `["gpu0", "gpu1"]` for a server split over several cards. The LLM then holds all of them: their VRAM is counted as its, a job that needs any of them unloads it from all of them, and it reloads on all of them. |
| `resident` | `""` | Model reloaded when the card goes idle. Empty = load on demand only. |
| `cpu_models` | `[]` | Models that never use the GPU, for when the YAML cannot be read. |
| `slot_save_models` | `[]` | Models started with `--slot-save-path`, for when the YAML cannot be read. gpuq saves and restores slot 0 around an unload. |
| `cpu_markers` | `["HIP_VISIBLE_DEVICES=-1", "ROCR_VISIBLE_DEVICES=-1", "CUDA_VISIBLE_DEVICES=-1"]` | A llama-swap model whose `cmd` contains one of these is CPU-only. |
| `load_timeout_s` | `600` | Time allowed for a model load. |
| `idle_s` | `300` | LLM silence required before the classes in `idle_wait_classes` take the card. |
| `idle_wait_classes` | `["background", "batch"]` | Classes that wait for `idle_s`. |
| `gap_s` | `10` | LLM silence required before other classes below the LLM take the card. |
| `reload_when_idle` | `true` | Reload the resident model when the card goes idle. |
| `reload_delay_s` | `20` | Pause after the last job before reloading the resident model. |
| `reload_slack_mib` | `1024` | Reload once VRAM is within this of the level seen right after the unload... |
| `reload_wait_s` | `120` | ...or after waiting this long. |
| `resume_grace_s` | `30` | After a gpuq restart, time for `gpuq run` clients to re-adopt their leases. |
| `batch_hold_s` | `0` | Hold LLM requests this long behind yieldable batch jobs. `0` = off. |
| `batch_hold_count` | `0` | Hand over earlier once this many LLM requests are queued. `0` = timer only. |

### [llm.models.MODEL]

Where one model runs, for models that need other cards than `llm.card`. The key is the
model id as in llama-swap's config (aliases resolve to it).

| Key | Default | Meaning |
|---|---|---|
| `cards` | `[]` | The cards the model is split over. |
| `vram` | unset | VRAM the model needs over all its cards, such as `"48G"`. When it is more than `llm.card` offers, gpuq adds the fewest other cards (largest first) that fit it. |

Use one of the two. Without either, gpuq estimates the VRAM of llama-swap models from the size
of the GGUF files in their `cmd` (`-m`/`--model` and `--mmproj`, all shards of a split model)
plus a fifth for the KV cache and buffers. A model that fits on `llm.card` stays there.

```toml
[llm]
card = "gpu0"              # small models

[llm.models."llama-70b"]
cards = ["gpu0", "gpu1"]   # this one is split over both cards
```

The split itself is set in llama-swap's command line, which gpuq does not rewrite; see
[integrations.md](integrations.md#several-gpus). For `llm.backend = "llama-server"` with
`[llm.service] kind = "command"`, gpuq starts the server with the cards' env merged, `GPUQ_CARD`,
and, with several cards, `LLAMA_ARG_SPLIT_MODE=layer` and `LLAMA_ARG_TENSOR_SPLIT` set to the
free VRAM of each card in GiB. llama-server reads these variables; flags on its command line
win over them. vLLM and SGLang started by a command get the env and `GPUQ_CARD`.

### [llm.model_classes]

Model name to class. Requests for these models are scheduled as that class instead of `llm`.
The class must exist in `[classes]`.

```toml
[llm.model_classes]
"summarizer" = "realtime"
```

### [llm.class_tuning.CLASS]

Per-class overrides, for a class that is often followed by another LLM request.

| Key | Default | Meaning |
|---|---|---|
| `gap_s` | falls back to `llm.gap_s` | Silence required before other classes take the card after a request of this class. |
| `reload_delay_s` | falls back to `llm.reload_delay_s` | Pause before reloading the resident model after a job of this class. |

### [llm.service]

How to stop and start a server that cannot unload a model at runtime. Same shape as a
service's `[service.control]`.

| Key | Applies to | Meaning |
|---|---|---|
| `kind` | all | `systemd`, `docker`, `podman` or `command`. Required. |
| `unit` | systemd | Unit name. Required. |
| `user` | systemd | `true` for a `systemctl --user` unit. Default `false`. |
| `container` | docker, podman | Container name. Required. The container must already exist. |
| `start` | command | Argv. Required. |
| `stop` | command | Argv. Required. |
| `status` | command | Argv, optional. Exit 0 = running. |
| `health_url` | all | Polled after a start; up when it answers 2xx. Without it gpuq only checks that the service is running. |
| `start_timeout_s` | all | Default `120`. |

Keys that do not belong to the chosen `kind` are an error.

## [classes]

Priority classes, lower number = more important. Your keys are added to the defaults (or
override them).

| Class | Default |
|---|---|
| `realtime` | `0` |
| `llm` | `1` |
| `interactive` | `2` |
| `background` | `3` |
| `batch` | `4` |

`llm` must exist. Unknown class names in a lease request are refused.

## [run]

| Key | Default | Meaning |
|---|---|---|
| `yield_signal` | `"SIGUSR1"` | One of SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, SIGALRM, SIGCONT, SIGTSTP. Sent by `gpuq run` to a yieldable job when it is asked to yield. |

gpuq announces this signal in every `granted` event; `gpuq run --yield-signal` overrides it
for one job.

## [immich]

The Immich ML adapter. The section is off unless it is present. gpuq watches Immich's ML job
queues and runs the GPU ML container only while it holds a lease. See `integrations.md`
for the setup and `examples/immich.toml`.

| Key | Default | Meaning |
|---|---|---|
| `url` | required | Immich server, e.g. `http://127.0.0.1:2283`. gpuq adds `/api`. |
| `key_file` | required | File holding an Immich API key. Read on every call. |
| `compose_dir` | required | Directory of the compose file; gpuq runs docker compose there. |
| `service` | required | Compose service of the GPU ML container. |
| `container` | required | Its container name, for `docker inspect` (health). |
| `compose_profile` | `"gpu"` | `docker compose --profile <this> up -d <service>`. |
| `docker` | `["docker"]` | argv prefix for docker, e.g. `["sudo", "docker"]`. |
| `class` | `"immich"` | Priority class of the lease; it must exist in `[classes]`, so add `immich = 3` or set another class. |
| `vram` | `"6G"` | VRAM the lease declares. |
| `card` | primary card | A card name. |
| `poll_s` | `30` | Time between backlog checks. |
| `min_backlog` | `200` | Backlog that makes gpuq pause the queues and ask for the card. |
| `cpu_ok_backlog` | `50` | After a release, a backlog this small is resumed for the CPU container; a larger one stays paused. |
| `queues` | `["smartSearch", "faceDetection", "ocr"]` | The ML queues to watch, pause and resume. |
| `gpu_concurrency` | `{ smartSearch = 8, faceDetection = 4, ocr = 2 }` | Queue concurrency while on the GPU. The old values come back on release. Names must be in `queues`. |
| `health_timeout_s` | `600` | How long the container may take to become healthy. |
| `settle_s` | `35` | Wait after "healthy" so Immich notices the container before the queues resume. |
| `drain_poll_s` | `5` | Time between checks for active jobs on release (60 checks at most). |
| `state_file` | `$XDG_STATE_HOME/gpuq/immich.json`, else `~/.local/state/gpuq/immich.json` | Remembers that gpuq paused the queues. |

The backlog is `waiting + active + delayed + paused` summed over `queues`. `gpuq check-config`
prints one `immich:` line for the section.

## [[service]]

A managed GPU service. Repeat the table for each one. See `integrations.md` for how they behave.

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Unique. Shown in `gpuq status`, used as the lease name. |
| `class` | `"interactive"` | Priority class. |
| `vram` | required | `"16G"`, `"6144M"`, `"512"` (MiB) or `"all"`. |
| `yieldable` | `true` | May be asked to give the card back. When asked, the service is stopped (if it has `control`), the lease released, and held requests queue again. |
| `stop_on_release` | `true` | Stop the service when its lease ends. With a gate, also stops it once at gpuq start. |
| `idle_release_s` | `120` | No in-flight GPU request and `busy_url` idle for this long: release the card. |
| `flavor` | `"generic"` | `generic` or `comfyui`. `comfyui` needs a gate. |
| `card` | primary card | The card the service runs on, or a list such as `["gpu0", "gpu1"]` for a service that uses several cards at once. `"any"` is not allowed: a service is bound to its devices. `vram` applies to each card. |

For `control.kind = "command"` gpuq starts the process itself and gives it the cards' env
(`CUDA_VISIBLE_DEVICES` and so on, merged for several cards). For systemd, Docker and Podman
pin the devices in the unit or container (`Environment=CUDA_VISIBLE_DEVICES=1`,
`--gpus device=1`); `card` then only tells gpuq which cards' VRAM the lease counts against.

A service with no `[service.gate]` is "always wanted": gpuq takes its lease at startup and
holds it. It never idle-releases. When asked to yield it stops (if it has control), releases
and queues again.

### [service.control]

Optional. Omit it for a service gpuq does not start or stop (then it only reserves VRAM and
gates requests). Same keys as `[llm.service]` above.

### [service.gate]

Optional. A small HTTP proxy in front of the service: it starts the service on the first
request that needs the GPU and releases the card when idle.

| Key | Default | Meaning |
|---|---|---|
| `listen` | required | `host:port` gpuq listens on. |
| `backend` | required | URL requests are forwarded to. |
| `gpu_paths` | `[]` | Path prefixes that need the card (query ignored). Empty = every request. |
| `busy_url` | unset | GET URL; while it reports work the lease is kept. See `integrations.md`. |

For the `comfyui` flavor `busy_url` defaults to `<backend>/queue`.

The listen addresses of `[gate]`, `[api]` and every service gate must differ.
