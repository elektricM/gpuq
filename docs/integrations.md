# Integrations

"I run X. How do I plug it in?" Find X below, copy the matching file from `examples/`, change
the names and ports, run `gpuq check-config -c FILE`.

## Kinds of workload

| Kind | Use it for | You write | gpuq does |
|---|---|---|---|
| LLM backend behind the gate | llama-swap, Ollama, llama-server, vLLM, SGLang | `[gate]`, `[llm]` | Proxies the API, holds requests while a job has the card, unloads and reloads models. |
| Managed service | ComfyUI, Immich ML, a TTS/ASR server, any CUDA/ROCm daemon | `[[service]]` | Starts and stops it (systemd, docker, podman, command), optionally gates its HTTP port. |
| Immich adapter | Immich ML with a CPU fallback | `[immich]` | Reads Immich's queues, pauses them, takes a lease and runs the GPU ML container only then. |
| `gpuq run` command | Training, whisper, ffmpeg, any script | Nothing in the config | Queues, takes a lease, runs the command, releases on exit. |
| Native lease client | Your own program | HTTP calls (`lease-api.md`) | Same lease, as an NDJSON stream. |

Everything shares one card through priority classes (`realtime`, `llm`, `interactive`,
`background`, `batch`). [how-it-works.md](how-it-works.md) explains the queue.

## LLM backends

The gate listens on the port your clients already use and forwards to the real server, which
you move to another port (`gate.backend`). Requests for models that need the GPU are held
while a job has the card. Requests for CPU-only models, and anything without a model, pass.

| Backend | Unload / load | KV slots | Health | Needs `[llm.service]` |
|---|---|---|---|---|
| `llama-swap` | at runtime | save and restore for models started with `--slot-save-path` | `/upstream/<model>/health` | no |
| `ollama` | at runtime (`keep_alive` 0 per loaded model; load by an empty generate) | no | `/api/ps` | no |
| `llama-server` | stop / start the service | yes, via `/slots/0`, for models in `slot_save_models` | `/health` | yes |
| `vllm` | stop / start the service | no | `/health` | yes |
| `sglang` | stop / start the service | no | `/health` | yes |
| `none` | not applicable | no | not applicable | no |

`[llm.service]` says how to stop and start the server: `kind = "systemd"` (with `unit`, `user`),
`"docker"` or `"podman"` (with `container`), or `"command"` (with `start`, `stop`, optional
`status`). Add `health_url` so gpuq waits until the server answers after a start.

Pick the example for your server:

- **llama-swap**: `examples/llama-swap.toml` with `examples/llama-swap.yaml`. A resident model
  with slot save, an alias, a CPU-only embedding model, and `model_classes` to schedule one
  model as a different class. gpuq reads the YAML for aliases and CPU-only models, so it
  recognises requests for `gpt-4o-mini` as requests for the aliased model.
- **Ollama**: `examples/ollama.toml`. gpuq sends `keep_alive -1`, so models stay loaded until
  gpuq unloads them.
- **vLLM**: `examples/vllm.toml`. Unload means stopping it; the next LLM request starts it and
  waits for `/health`. Docker, systemd and command variants are in the file.
- **SGLang**: `examples/sglang.toml`. Same as vLLM.
- **No LLM at all**: `backend = "none"`. No gate port is opened; gpuq arbitrates leases and
  managed services only.

## Managed services

For any GPU app that runs as a service. One `[[service]]` table per app:

```toml
[[service]]
name = "tts"
class = "interactive"
vram = "6G"
stop_on_release = true
idle_release_s = 120

[service.control]       # how to start and stop it
kind = "systemd"
unit = "tts.service"
health_url = "http://127.0.0.1:5001/health"

[service.gate]          # optional: start on first request
listen = "127.0.0.1:5000"
backend = "http://127.0.0.1:5001"
gpu_paths = ["/synthesize"]
```

Each service has a small state machine: idle, acquiring, starting, running, releasing, idle.
After a failed start there is a 10 s backoff during which requests get an immediate 503.

With a gate:

1. A request that needs the GPU arrives and is held. gpuq acquires a lease (`class`, `vram`,
   `yieldable`, lease name = service name), starts the service unless it is running, waits for
   `health_url`, then forwards the request.
2. While the service holds the lease, requests go straight through. Each counts as in flight
   until its response body ends.
3. When no GPU request is in flight and `busy_url` (if set) reports idle for `idle_release_s`,
   gpuq stops the service if `stop_on_release` is set, then drops the lease. New work during
   the release keeps the lease.
4. If something more important wants the card and the lease is `yieldable`, new GPU requests are
   held, in-flight work drains, the service is stopped (when it has control, even with
   `stop_on_release = false`), the lease is released, and the held requests re-acquire.
5. If the start or health check fails, gpuq stops the service, drops the lease, answers held
   requests with 503 `gpuq: NAME did not start: REASON`, and backs off for 10 s.

At gpuq start, a gated service with control and `stop_on_release` is stopped once so everything
begins from a known state.

**Without `[service.gate]`**, the service is "always wanted": gpuq takes the lease at startup and
holds it. It never idle-releases. When something more important needs the card, a service with
control is stopped, the lease released, and gpuq queues again. Without control it only reserves
VRAM.

**Without `[service.control]`**, gpuq does not start or stop it. It still gates requests and
accounts for the VRAM.

WebSocket upgrades are tunnelled, never gated (503 while the service is stopped).

### gpu_paths and busy_url

`gpu_paths` is a list of path prefixes (the query string is ignored). A request needs the card
if its path starts with one of them. An empty list means every request needs it.

- A GPU request without a lease is held until gpuq has the card, then forwarded.
- A request that does not need the GPU passes while the service is up. While it is stopped it gets
  `503 gpuq: NAME is stopped; it starts on demand`. So a health check or status page never wakes
  the GPU.

`busy_url` is for services that accept work and process it later, where "no request in flight"
does not mean "idle". gpuq sends a GET to it:

- connection error: idle
- JSON object: busy if any top-level member is a non-empty array
- JSON array: busy if it is non-empty
- any other JSON: idle
- not JSON: busy if the status is 2xx except 204, otherwise idle

This fits ComfyUI's `/queue` (`queue_running` and `queue_pending` are arrays).

### ComfyUI

`flavor = "comfyui"` requires a gate. `examples/comfyui.toml` is the full config.

- `POST /prompt` and `/api/prompt` are answered at once with `{"prompt_id", "number",
  "node_errors": {}}` and held. Once gpuq holds the lease and ComfyUI is up, prompts are
  submitted in order. A submission that fails shows up as a failed job.
- `GET /queue` and `/api/jobs` include held prompts. `/queue` delete and clear and
  `/api/jobs/{id}/cancel` cancel them.
- `busy_url` defaults to `<backend>/queue`.
- Switching to a different set of model weight files waits for the queue to go idle, then
  restarts ComfyUI (with control) or calls `POST /free` (without).
- On yield, queued prompts are taken back with `POST /queue` delete (their `extra_data` is
  kept), the running one is allowed to finish, ComfyUI is stopped or freed, and the lease is
  released. The prompts are resubmitted when gpuq holds the card again.

Move ComfyUI to 8189 and let gpuq take 8188 with a drop-in:

```ini
# ~/.config/systemd/user/comfyui.service.d/50-gpuq.conf
[Service]
ExecStart=
ExecStart=%h/ComfyUI/.venv/bin/python main.py --listen 127.0.0.1 --port 8189
```

Use a user unit for gpuq when ComfyUI is a `systemctl --user` unit (below).

### Immich machine learning

There are two ways. Pick the adapter if you can run a CPU ML container as well; use the gated
service if you cannot.

#### With the queue-aware adapter (`[immich]`)

`examples/immich.toml`. gpuq reads Immich's ML job queues and runs the GPU ML container only
while it holds a lease. Nothing proxies through gpuq: Immich talks to its ML containers
directly.

1. **Two ML URLs.** In Immich, Administration, Settings, Machine Learning, list two URLs: the
   GPU container first, a CPU-only container second. Immich tries them in order and uses the
   CPU one while the GPU one is down. The GPU container exists only while gpuq holds a lease,
   because a container that answers without VRAM would fail requests instead of letting Immich
   fall back.
2. **A compose profile.** Define the GPU ML service in your Immich compose file under
   `profiles: ["gpu"]` with `restart: "no"`. gpuq runs
   `docker compose --profile gpu up -d <service>` and `... stop <service>` in `compose_dir`.
   Set `compose_profile` if you named the profile differently.
3. **An API key.** Create one in Immich (Account settings, API keys) with these permissions:
   `queue.read` and `queue.update` (list, pause and resume queues) and `systemConfig.read` and
   `systemConfig.update` (queue concurrency). Put the key alone in `key_file`. gpuq reads the
   file on every call, so you can rotate the key without restarting. If the file is missing or
   unreadable, or Immich answers 401, gpuq logs one warning, shows the reason in `gpuq status`
   and stays idle until the file changes.
4. **gpuq user.** It runs docker, so it needs the docker group (or set `docker`).

What gpuq does, once per `poll_s`:

- The backlog is `waiting + active + delayed + paused` over `queues`. At `min_backlog` or more,
  gpuq pauses the queues (so the CPU container does not chew through a reindex) and queues for
  a yieldable lease of class `class` named `immich-ml`.
- When the lease is granted it starts the container, waits for `docker inspect` to report it
  healthy, waits `settle_s` so Immich has noticed it, raises the queue concurrency to
  `gpu_concurrency` and resumes the queues.
- When the arbiter asks the lease to yield, or the backlog has been empty for two polls, gpuq
  pauses the queues, waits for active jobs (60 checks, `drain_poll_s` apart), stops the
  container, restores the concurrency and releases the lease.
- Then, a backlog of `cpu_ok_backlog` or less is resumed for the CPU container. A bigger one
  stays paused until the next lease. A yield during the health wait or the settle sleep ends
  the start at once.

**State file.** gpuq records `{"paused_by_gpuq": true}` in `state_file` while it has the queues
paused. If gpuq restarts, it keeps going from there: with an empty backlog it resumes the
queues and clears the flag, otherwise it queues for the card again.

**Crashes and shutdown.** At startup, a GPU container that is running while gpuq holds no lease
is stopped. On `SIGTERM` while serving, gpuq runs the release path (at most 30 s) so the
container does not stay on the GPU. Errors in a poll are logged and retried on the next one.

`gpuq status` shows an `immich` line (state `idle`, `waiting`, `starting`, `serving` or
`draining`, the backlog, and any error); `/status` has the same under `immich`.

#### As a gated service (`[[service]]`)

`examples/immich-ml.toml`, for setups without a CPU fallback. Immich server sends inference
calls to `/predict` on the ML container. gpuq sits in front of it:

- Point Immich at gpuq's port (Admin, Settings, Machine Learning). The container's published
  port moves to the `backend` address.
- `gpu_paths = ["/predict"]`: only inference needs the card; `/ping` passes.
- `control.kind = "docker"` with `stop_on_release = true`: the container runs only while
  Immich is doing inference, and the VRAM is free the rest of the time.
- Immich admin settings accept several ML URLs. List gpuq's first and a CPU-only ML container
  second: Immich falls back to the CPU one when the GPU one is unavailable or still starting.
  This setup does not pause the queues, so a reindex goes on whichever container answers.

### Other CUDA or ROCm services

`examples/cuda-service.toml`: a TTS or ASR HTTP server, an image API, a custom daemon. Pick the
control kind (`systemd`, `docker`, `podman`, or `command` with your own start and stop scripts)
and the `gpu_paths` that really use the card.

## Commands with gpuq run

```sh
gpuq run --class batch --vram 8G -- python embed_all.py
gpuq run --class realtime --vram 4G --name transcribe -- whisper-cli -m model.bin -f in.wav
gpuq run --class batch --vram all --yieldable --name train -- python train.py
```

`examples/batch-job.sh` has these and a resumable training loop; `examples/whisper.sh` is a
realtime-class job.

`gpuq run [--class batch] [--vram all] [--card NAME|LIST|all|any | --cards N] [--name N]
[--yieldable] [--yield-signal SIGUSR1] [--fallback] -- cmd...`

- Waits in the queue, then runs the command with `GPUQ_LEASE_ID`, `GPUQ_YIELD_FILE` and
  `GPUQ_CARD` (the card it got, or `gpu0,gpu1` for several) in its environment, plus the
  cards' `CUDA_VISIBLE_DEVICES` or `ROCR_VISIBLE_DEVICES`.
- Exit code is the command's (128 + signal if it died of one). 69 if gpuq is not running,
  unless `--fallback` (run without a lease, with a warning). 1 for an unknown class, 2 for
  usage errors, 126/127 if the command cannot be started.
- SIGINT, SIGTERM and SIGHUP are forwarded to the command.
- If the connection to gpuq drops, `gpuq run` re-adopts the lease every 2 s.
- On Linux, if `gpuq run` itself is killed (`kill -9`, OOM killer), the kernel kills the command
  too, so nothing keeps using the card without a lease. This reaches only the command itself:
  in a wrapper script, start the real job with `exec` (`exec python train.py`).
- `gpuq hold [--class] [--vram] [--card | --cards] [--name]` takes a lease and holds it until
  Ctrl-C. Use it to reserve cards by hand.

## Several GPUs

gpuq detects every AMD and NVIDIA GPU and schedules all of them from one queue (see
[config.md](config.md#detected-cards); leave the integrated GPU out with `gpu.exclude` or
`gpu.skip_integrated`). `gpuq check-config` lists the cards, their PCI addresses and the env
each one hands to jobs.

- **Default.** A job that names no card goes to any card with room.
- **Pinning.** `gpuq run --card gpu1 -- cmd` waits for that card only. A `[[service]]` with
  `card = "gpu1"` is bound to it. Services and the Immich adapter that name no card use the
  primary card, `gpu0`.
- **Several cards at once.** `--card gpu0,gpu1`, `--card all`, or `--cards 2` for any two cards
  with room. `--vram` applies to each card. The lease is granted all at once or not at all; see
  [how-it-works.md](how-it-works.md#one-job-on-several-cards).
- **Env.** gpuq hands the cards' env to the job, merged for several cards, and `GPUQ_CARD`
  names them. Native clients read `cards` and `env` from the `granted` event. A
  `kind = "command"` service is started with the env too; for systemd and containers, pin the
  devices in the unit or container, for example `Environment=CUDA_VISIBLE_DEVICES=1` or
  `--gpus device=1`.
- **Device numbers.** Detected cards are pointed at by UUID where the stack accepts one
  (`CUDA_VISIBLE_DEVICES=GPU-...`, `ROCR_VISIBLE_DEVICES=GPU-...`), so the order CUDA, HIP or a
  driver update gives the devices does not matter. With `[[gpu.card]]` you write
  `visible_devices` yourself: prefer the UUID, or set `CUDA_DEVICE_ORDER=PCI_BUS_ID` wherever
  CUDA programs start so indices match `nvidia-smi`.
- **The LLM.** `llm.card` is one card (default `gpu0`) or a list. `[llm.models]` gives a model
  its own cards or its size. One LLM backend runs at a time; a server per card is on the
  roadmap.

### llama.cpp and llama-swap on two cards

A 24 GiB and a 16 GiB card: small models run on gpu0, a 70B model is split over both. gpuq
cannot rewrite llama-swap's command lines, so each model is pinned in llama-swap's config, and
gpuq is told which cards each model holds so its accounting matches.

Take the device strings from `gpuq check-config` (the values below are placeholders):

```
gpus: 3 detected, 2 in use
  gpu0: amd pci=0000:03:00.0 24560 MiB, ROCR_VISIBLE_DEVICES=GPU-1111aaaa2222bbbb, margin 256 MiB (primary) (llm)
  gpu1: amd pci=0000:0c:00.0 16368 MiB, ROCR_VISIBLE_DEVICES=GPU-3333cccc4444dddd, margin 256 MiB
  excluded: amd 0000:15:00.0 2048 MiB (gpu.exclude)
```

llama-swap's `config.yaml`:

```yaml
models:
  # Fits on gpu0: only that card is visible to it.
  "chat":
    env: ["ROCR_VISIBLE_DEVICES=GPU-1111aaaa2222bbbb"]
    cmd: |
      llama-server --port ${PORT} -m /models/chat-14b-q6.gguf -ngl 99 --ctx-size 32768
      --slot-save-path /var/cache/llama-swap/slots

  # About 40 GB: both cards, layers split 24:16 like their VRAM.
  "llama-70b":
    env: ["ROCR_VISIBLE_DEVICES=GPU-1111aaaa2222bbbb,GPU-3333cccc4444dddd"]
    cmd: |
      llama-server --port ${PORT} -m /models/llama-70b-q4_k_m.gguf -ngl 99
      --split-mode layer --tensor-split 24,16 --ctx-size 16384
```

gpuq's `config.toml`:

```toml
[gpu]
exclude = ["0000:15:00.0"]          # the integrated GPU

[llm]
backend = "llama-swap"
llama_swap_config = "/etc/llama-swap/config.yaml"
card = "gpu0"                       # where models run unless said otherwise
resident = "chat"

[llm.models."llama-70b"]
cards = ["gpu0", "gpu1"]            # matches its ROCR_VISIBLE_DEVICES above
```

What happens:

- A request for `chat` needs gpu0 only. A job can run on gpu1 at the same time.
- A request for `llama-70b` needs both cards. If a job holds gpu1, it is asked to yield (if
  yieldable) and the request waits for it; then the LLM takes gpu1 too.
- From then on the LLM holds both cards. A job that needs either card unloads it from both,
  and the resident `chat` model reloads on gpu0 once gpu0 is free.

Without `[llm.models]`, gpuq estimates each model from its GGUF files (plus a fifth for the KV
cache) and gives a model that does not fit on gpu0 the fewest extra cards that fit it. Name the
cards when the llama-swap config pins them, so both agree.

Notes for the command line:

- On NVIDIA, use `CUDA_VISIBLE_DEVICES` with the UUIDs instead of `ROCR_VISIBLE_DEVICES`.
- llama.cpp numbers the cards it can see from 0 (`ROCm0`, `ROCm1` or `CUDA0`, `CUDA1` in
  `--list-devices`), so `-dev ROCm0,ROCm1` and `--main-gpu` refer to the visible cards, in
  the order of the list.
- Without any of these options, llama.cpp uses every visible GPU and splits layers by free
  VRAM. That includes small models, which then hold memory on every card while gpuq counts
  them on one. Pin every model.
- On AMD, prefer `ROCR_VISIBLE_DEVICES`. Setting `HIP_VISIBLE_DEVICES` to the same list as
  well applies it twice.
- Vulkan builds ignore these variables; use `GGML_VK_VISIBLE_DEVICES` and check the numbers
  with `--list-devices`.

### llama-server started by gpuq

With `llm.backend = "llama-server"` and `[llm.service] kind = "command"`, gpuq starts the server
itself and gives it the cards:

```toml
[llm]
backend = "llama-server"
card = ["gpu0", "gpu1"]
resident = "llama-70b"

[llm.service]
kind = "command"
start = ["/usr/local/bin/llama-70b", "start"]
stop = ["/usr/local/bin/llama-70b", "stop"]
health_url = "http://127.0.0.1:8081/health"
```

The start command runs with `ROCR_VISIBLE_DEVICES=GPU-1111aaaa2222bbbb,GPU-3333cccc4444dddd`,
`GPUQ_CARD=gpu0,gpu1`, `LLAMA_ARG_SPLIT_MODE=layer` and `LLAMA_ARG_TENSOR_SPLIT=23,15` (free
VRAM per card in GiB when it starts). llama-server reads the `LLAMA_ARG_*` variables, and a
flag on its command line wins over them. With systemd, Docker or Podman, gpuq cannot pass
variables: put them in the unit or container.

### ComfyUI and PyTorch

A training run on any two cards with room, 12 GiB on each:

```sh
gpuq run --class batch --cards 2 --vram 12G --yieldable -- \
  torchrun --nproc-per-node 2 train.py
```

PyTorch sees the two cards as `cuda:0` and `cuda:1`, whichever ones gpuq granted (ROCm builds
of PyTorch use the same names). `GPUQ_CARD` says which they are. When something more important
needs either card, the job gets `SIGUSR1` and `$GPUQ_YIELD_FILE` appears.

ComfyUI on gpu1 only, so gpu0 stays free for the LLM:

```toml
[[service]]
name = "comfyui"
flavor = "comfyui"
card = "gpu1"
vram = "14G"

[service.control]
kind = "command"
start = ["/usr/local/bin/comfyui-start"]
stop = ["/usr/local/bin/comfyui-stop"]
health_url = "http://127.0.0.1:8189/system_stats"

[service.gate]
listen = "127.0.0.1:8188"
backend = "http://127.0.0.1:8189"
gpu_paths = ["/prompt"]
```

ComfyUI itself works on one device. With a multi-GPU node pack that places models on several
devices, give the service both cards: `card = ["gpu0", "gpu1"]`. Its start command then sees
both in its env, and its `vram` is held on each card while it runs.

## The yield contract

A lease is yieldable if you say so (`--yieldable`, or `"yieldable": true`). gpuq never kills a
job. When something more important waits, a yieldable job is asked to give the card back:

- `gpuq run` creates the file at `$GPUQ_YIELD_FILE` (it does not exist before) and sends the
  yield signal to the child. The signal is `[run] yield_signal` from gpuq's config (default
  `SIGUSR1`); `--yield-signal` overrides it per job (`TERM` and `SIGTERM` are both accepted).
- A native client receives `{"event":"yield"}` on its lease stream.

What the program should do: save its state, free its VRAM, and exit (or close the connection).
Either check for the file between steps, or handle the signal. Then run it again to resume;
it queues like any other job. `examples/batch-job.sh` has a Python training loop that does this.
A job that ignores the request keeps the card, and the waiting work keeps waiting.

Non-yieldable jobs run to completion. Use that for short jobs where a restart would cost more
than the wait.

## Running gpuq: system unit or user unit

`packaging/systemd/gpuq.service` is the system unit. It runs as the `gpuq` user, in the `video`
and `render` groups, with hardening on. Config in `/etc/gpuq/config.toml`. Use it for LLM
backends, containers and system units.

`packaging/systemd/gpuq.user.service` is the user unit. It reads `~/.config/gpuq/config.toml`.
Use it when gpuq must control `systemctl --user` services such as ComfyUI: only a user instance
can start and stop them.

What gpuq needs depending on what it controls:

- **Docker or podman containers**: the user needs the `docker` group (add it to
  `SupplementaryGroups=` in the system unit). That group is root-equivalent; decide if you accept it.
- **System units** from the unprivileged `gpuq` user: a polkit rule that allows
  `org.freedesktop.systemd1.manage-units` for that user, or run the unit as root (drop `User=`,
  `Group=` and `NoNewPrivileges=true`).
- **`systemctl --user` units**: use the user unit.
- **Extra write paths**: `ReadWritePaths=` in the hardened unit.

The daemon sends `READY=1` (the unit is `Type=notify`).

## Reading GPU state

`gpuq status` shows the state, who holds the card, the queue and the backend's models.
`gpuq status --json` and `GET /status` give the same as JSON.
`gpuq check-config` prints how the config was understood and one VRAM probe reading.
