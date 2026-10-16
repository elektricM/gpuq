# Configuration reference

gpud reads one TOML file. `gpud daemon -c FILE` and `gpud check-config -c FILE` take it
explicitly. Without `-c` the first existing file of these wins: `/etc/gpud/config.toml`,
then `$XDG_CONFIG_HOME/gpud/config.toml` (or `~/.config/gpud/config.toml`). With no file at all
the built-in defaults apply.

Rules:

- Every key is optional. Tables merge into the defaults; lists and scalars replace them.
- Unknown keys are an error, so a typo does not silently do nothing.
- Durations are seconds (`_s` suffix), integer or float.
- Run `gpud check-config` after every edit. It prints what it understood and reads the VRAM
  probe once. `--strict` makes a failing probe an error.
- There is no reload. Restart gpud after editing (SIGHUP is logged and ignored).
- `examples/gpud.toml` lists every key with its default and is checked against the code in the
  test suite.

## [gate]

The reverse proxy in front of the LLM server.

| Key | Default | Meaning |
|---|---|---|
| `host` | `"127.0.0.1"` | Listen address. `"*"` listens on all interfaces. |
| `port` | `8080` | Listen port. Clients keep pointing at the port they already use. |
| `backend` | `"http://127.0.0.1:8081"` | Where requests are forwarded: the real LLM server, moved to this address. |

## [api]

The control API used by the `gpud` CLI and by lease clients (see `lease-api.md`). Keep it on
loopback: anyone who can reach it can take the card.

| Key | Default | Meaning |
|---|---|---|
| `host` | `"127.0.0.1"` | Listen address. |
| `port` | `9390` | Listen port. CLI: `--api HOST:PORT` or env `GPUD_API`. |

## [gpu]

How VRAM is read and how much room to leave.

| Key | Default | Meaning |
|---|---|---|
| `vendor` | `"auto"` | `auto`, `amd`, `nvidia`, `intel` or `command`. `auto` tries AMD sysfs, then `nvidia-smi` on PATH, then `command` if `gpu.command` is set. `intel` is not supported yet; use `command`. |
| `pci` | `""` | AMD: PCI address such as `0000:03:00.0`. Empty picks the amdgpu device with the most VRAM. |
| `sysfs_root` | `"/sys/bus/pci/devices"` | AMD: where to look for devices. |
| `nvidia_index` | `0` | NVIDIA: GPU index passed to `nvidia-smi -i`. |
| `nvidia_smi` | `"nvidia-smi"` | NVIDIA: the binary to run. |
| `command` | `[]` | Argv of a program that prints `used_mib total_mib` (an optional third number is GTT in MiB). Required for `vendor = "command"`. |
| `device` | unset | Name of the card in status and leases when there is no `[[gpu.card]]`. Default `gpu0`. |
| `margin_mib` | `256` | VRAM kept free on top of what leases declare. |
| `settle_timeout_s` | `60` | How long to wait for the backend to report its models unloaded. |
| `driver_settle_s` | `2` | Pause after an unload so the driver releases VRAM. |
| `whole_card_baseline_mib` | `512` | `--vram all` needs at most this much used by other processes. |

### [[gpu.card]]

Several cards: write one `[[gpu.card]]` table per GPU. When present they replace the single
card described by the keys above (`margin_mib`, `settle_timeout_s`, `driver_settle_s` and
`whole_card_baseline_mib` stay the defaults for all cards). Without any `[[gpu.card]]`, the
`[gpu]` table is one card named `gpu.device`, else `gpu0`, with no env, exactly as before.
The first card is the primary card: jobs and services that name no card go there.

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Unique; `a-z`, `0-9`, `_` and `-`; not `any` or `primary`. Used in `--card`, status and the API. |
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

The env is sent in the `granted` event, exported by `gpud run`, and passed to the start
command of a managed service of `kind = "command"`. A card without `visible_devices` can be
named (`--card gpu1`) but is skipped by `--card any`, because gpud could not tell the job where
it landed. If no card has `visible_devices`, `--card any` is refused and `gpud check-config`
says so.

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

## [hooks]

| Key | Default | Meaning |
|---|---|---|
| `offload` | `[]` | Argv run once per waiting job that does not fit while no lease is active, for example to move a desktop session off the card. Run directly, not through a shell. Empty disables it. |
| `offload_timeout_s` | `180` | Kill the hook after this long. |
| `offload_settle_s` | `3` | Wait this long after the hook before measuring VRAM again. |

## [llm]

The LLM server behind the gate and when it gives up the card.

| Key | Default | Meaning |
|---|---|---|
| `backend` | `"llama-swap"` | `llama-swap`, `ollama`, `llama-server`, `vllm`, `sglang` or `none`. `none` means no LLM gate: gpud only arbitrates leases and managed services. `llama-server`, `vllm` and `sglang` need `[llm.service]`. |
| `llama_swap_config` | `"/etc/llama-swap/config.yaml"` | llama-swap's own YAML, read for aliases and CPU-only models. The old name `config` still works. |
| `card` | first card | With several cards, the card the LLM server runs on. Must be a `[[gpu.card]]` name. |
| `resident` | `""` | Model reloaded when the card goes idle. Empty = load on demand only. |
| `cpu_models` | `[]` | Models that never use the GPU, for when the YAML cannot be read. |
| `slot_save_models` | `[]` | Models started with `--slot-save-path`, for when the YAML cannot be read. gpud saves and restores slot 0 around an unload. |
| `cpu_markers` | `["HIP_VISIBLE_DEVICES=-1", "ROCR_VISIBLE_DEVICES=-1", "CUDA_VISIBLE_DEVICES=-1"]` | A llama-swap model whose `cmd` contains one of these is CPU-only. |
| `load_timeout_s` | `600` | Time allowed for a model load. |
| `idle_s` | `300` | LLM silence required before the classes in `idle_wait_classes` take the card. |
| `idle_wait_classes` | `["background", "batch"]` | Classes that wait for `idle_s`. |
| `gap_s` | `10` | LLM silence required before other classes below the LLM take the card. |
| `reload_when_idle` | `true` | Reload the resident model when the card goes idle. |
| `reload_delay_s` | `20` | Pause after the last job before reloading the resident model. |
| `reload_slack_mib` | `1024` | Reload once VRAM is within this of the level seen right after the unload... |
| `reload_wait_s` | `120` | ...or after waiting this long. |
| `resume_grace_s` | `30` | After a gpud restart, time for `gpud run` clients to re-adopt their leases. |
| `batch_hold_s` | `0` | Hold LLM requests this long behind yieldable batch jobs. `0` = off. |
| `batch_hold_count` | `0` | Hand over earlier once this many LLM requests are queued. `0` = timer only. |

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
| `health_url` | all | Polled after a start; up when it answers 2xx. Without it gpud only checks that the service is running. |
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
| `yield_signal` | `"SIGUSR1"` | One of SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, SIGALRM, SIGCONT, SIGTSTP. Sent by `gpud run` to a yieldable job when it is asked to yield. |

gpud announces this signal in every `granted` event; `gpud run --yield-signal` overrides it
for one job.

## [[service]]

A managed GPU service. Repeat the table for each one. See `integrations.md` for how they behave.

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Unique. Shown in `gpud status`, used as the lease name. |
| `class` | `"interactive"` | Priority class. |
| `vram` | required | `"16G"`, `"6144M"`, `"512"` (MiB) or `"all"`. |
| `yieldable` | `true` | May be asked to give the card back. When asked, the service is stopped (if it has `control`), the lease released, and held requests queue again. |
| `stop_on_release` | `true` | Stop the service when its lease ends. With a gate, also stops it once at gpud start. |
| `idle_release_s` | `120` | No in-flight GPU request and `busy_url` idle for this long: release the card. |
| `flavor` | `"generic"` | `generic` or `comfyui`. `comfyui` needs a gate. |
| `card` | primary card | The card the service runs on; a `[[gpu.card]]` name. `"any"` is not allowed: a service is bound to its device. |

For `control.kind = "command"` gpud starts the process itself and gives it the card's env
(`CUDA_VISIBLE_DEVICES` and so on). For systemd, Docker and Podman pin the device in the
unit or container (`Environment=CUDA_VISIBLE_DEVICES=1`, `--gpus device=1`); `card` then only
tells gpud which card's VRAM the lease counts against.

A service with no `[service.gate]` is "always wanted": gpud takes its lease at startup and
holds it. It never idle-releases. When asked to yield it stops (if it has control), releases
and queues again.

### [service.control]

Optional. Omit it for a service gpud does not start or stop (then it only reserves VRAM and
gates requests). Same keys as `[llm.service]` above.

### [service.gate]

Optional. A small HTTP proxy in front of the service: it starts the service on the first
request that needs the GPU and releases the card when idle.

| Key | Default | Meaning |
|---|---|---|
| `listen` | required | `host:port` gpud listens on. |
| `backend` | required | URL requests are forwarded to. |
| `gpu_paths` | `[]` | Path prefixes that need the card (query ignored). Empty = every request. |
| `busy_url` | unset | GET URL; while it reports work the lease is kept. See `integrations.md`. |

For the `comfyui` flavor `busy_url` defaults to `<backend>/queue`.

The listen addresses of `[gate]`, `[api]` and every service gate must differ.
