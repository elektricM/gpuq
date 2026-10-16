# Integrations

"I run X. How do I plug it in?" Find X below, copy the matching file from `examples/`, change
the names and ports, run `gpud check-config -c FILE`.

## Four kinds of workload

| Kind | Use it for | You write | gpud does |
|---|---|---|---|
| LLM backend behind the gate | llama-swap, Ollama, llama-server, vLLM, SGLang | `[gate]`, `[llm]` | Proxies the API, holds requests while a job has the card, unloads and reloads models. |
| Managed service | ComfyUI, Immich ML, a TTS/ASR server, any CUDA/ROCm daemon | `[[service]]` | Starts and stops it (systemd, docker, podman, command), optionally gates its HTTP port. |
| `gpud run` command | Training, whisper, ffmpeg, any script | Nothing in the config | Queues, takes a lease, runs the command, releases on exit. |
| Native lease client | Your own program | HTTP calls (`lease-api.md`) | Same lease, as an NDJSON stream. |

Everything shares one card through priority classes (`realtime`, `llm`, `interactive`,
`background`, `batch`). The README has the short version of how the queue works.

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
`status`). Add `health_url` so gpud waits until the server answers after a start.

Pick the example for your server:

- **llama-swap**: `examples/llama-swap.toml` with `examples/llama-swap.yaml`. A resident model
  with slot save, an alias, a CPU-only embedding model, and `model_classes` to schedule one
  model as a different class. gpud reads the YAML for aliases and CPU-only models, so it
  recognises requests for `gpt-4o-mini` as requests for the aliased model.
- **Ollama**: `examples/ollama.toml`. gpud sends `keep_alive -1`, so models stay loaded until
  gpud unloads them.
- **vLLM**: `examples/vllm.toml`. Unload means stopping it; the next LLM request starts it and
  waits for `/health`. Docker, systemd and command variants are in the file.
- **SGLang**: `examples/sglang.toml`. Same as vLLM.
- **No LLM at all**: `backend = "none"`. No gate port is opened; gpud arbitrates leases and
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

1. A request that needs the GPU arrives and is held. gpud acquires a lease (`class`, `vram`,
   `yieldable`, lease name = service name), starts the service unless it is running, waits for
   `health_url`, then forwards the request.
2. While the service holds the lease, requests go straight through. Each counts as in flight
   until its response body ends.
3. When no GPU request is in flight and `busy_url` (if set) reports idle for `idle_release_s`,
   gpud stops the service if `stop_on_release` is set, then drops the lease. New work during
   the release keeps the lease.
4. If something more important wants the card and the lease is `yieldable`, new GPU requests are
   held, in-flight work drains, the service is stopped (when it has control, even with
   `stop_on_release = false`), the lease is released, and the held requests re-acquire.
5. If the start or health check fails, gpud stops the service, drops the lease, answers held
   requests with 503 `gpud: NAME did not start: REASON`, and backs off for 10 s.

At gpud start, a gated service with control and `stop_on_release` is stopped once so everything
begins from a known state.

**Without `[service.gate]`**, the service is "always wanted": gpud takes the lease at startup and
holds it. It never idle-releases. When something more important needs the card, a service with
control is stopped, the lease released, and gpud queues again. Without control it only reserves
VRAM.

**Without `[service.control]`**, gpud does not start or stop it. It still gates requests and
accounts for the VRAM.

WebSocket upgrades are tunnelled, never gated (503 while the service is stopped).

### gpu_paths and busy_url

`gpu_paths` is a list of path prefixes (the query string is ignored). A request needs the card
if its path starts with one of them. An empty list means every request needs it.

- A GPU request without a lease is held until gpud has the card, then forwarded.
- A request that does not need the GPU passes while the service is up. While it is stopped it gets
  `503 gpud: NAME is stopped; it starts on demand`. So a health check or status page never wakes
  the GPU.

`busy_url` is for services that accept work and process it later, where "no request in flight"
does not mean "idle". gpud sends a GET to it:

- connection error: idle
- JSON object: busy if any top-level member is a non-empty array
- JSON array: busy if it is non-empty
- any other JSON: idle
- not JSON: busy if the status is 2xx except 204, otherwise idle

This fits ComfyUI's `/queue` (`queue_running` and `queue_pending` are arrays).

### ComfyUI

`flavor = "comfyui"` requires a gate. `examples/comfyui.toml` is the full config.

- `POST /prompt` and `/api/prompt` are answered at once with `{"prompt_id", "number",
  "node_errors": {}}` and held. Once gpud holds the lease and ComfyUI is up, prompts are
  submitted in order. A submission that fails shows up as a failed job.
- `GET /queue` and `/api/jobs` include held prompts. `/queue` delete and clear and
  `/api/jobs/{id}/cancel` cancel them.
- `busy_url` defaults to `<backend>/queue`.
- Switching to a different set of model weight files waits for the queue to go idle, then
  restarts ComfyUI (with control) or calls `POST /free` (without).
- On yield, queued prompts are taken back with `POST /queue` delete (their `extra_data` is
  kept), the running one is allowed to finish, ComfyUI is stopped or freed, and the lease is
  released. The prompts are resubmitted when gpud holds the card again.

Move ComfyUI to 8189 and let gpud take 8188 with a drop-in:

```ini
# ~/.config/systemd/user/comfyui.service.d/50-gpud.conf
[Service]
ExecStart=
ExecStart=%h/ComfyUI/.venv/bin/python main.py --listen 127.0.0.1 --port 8189
```

Use a user unit for gpud when ComfyUI is a `systemctl --user` unit (below).

### Immich machine learning

`examples/immich-ml.toml`. Immich server sends inference calls to `/predict` on the ML
container. gpud sits in front of it:

- Point Immich at gpud's port (Admin, Settings, Machine Learning). The container's published
  port moves to the `backend` address.
- `gpu_paths = ["/predict"]`: only inference needs the card; `/ping` passes.
- `control.kind = "docker"` with `stop_on_release = true`: the container runs only while
  Immich is doing inference, and the VRAM is free the rest of the time.
- Immich admin settings accept several ML URLs. List gpud's first and a CPU-only ML container
  second: Immich falls back to the CPU one when the GPU one is unavailable or still starting.

### Other CUDA or ROCm services

`examples/cuda-service.toml`: a TTS or ASR HTTP server, an image API, a custom daemon. Pick the
control kind (`systemd`, `docker`, `podman`, or `command` with your own start and stop scripts)
and the `gpu_paths` that really use the card.

## Commands with gpud run

```sh
gpud run --class batch --vram 8G -- python embed_all.py
gpud run --class realtime --vram 4G --name transcribe -- whisper-cli -m model.bin -f in.wav
gpud run --class batch --vram all --yieldable --name train -- python train.py
```

`examples/batch-job.sh` has these and a resumable training loop; `examples/whisper.sh` is a
realtime-class job.

`gpud run [--class batch] [--vram all] [--name N] [--yieldable] [--yield-signal SIGUSR1]
[--fallback] -- cmd...`

- Waits in the queue, then runs the command with `GPUD_LEASE_ID` and `GPUD_YIELD_FILE` in its
  environment.
- Exit code is the command's (128 + signal if it died of one). 69 if gpud is not running,
  unless `--fallback` (run without a lease, with a warning). 1 for an unknown class, 2 for
  usage errors, 126/127 if the command cannot be started.
- SIGINT, SIGTERM and SIGHUP are forwarded to the command.
- If the connection to gpud drops, `gpud run` re-adopts the lease every 2 s.
- `gpud hold [--class] [--vram] [--name]` takes a lease and holds it until Ctrl-C. Use it to
  reserve the card by hand.

## The yield contract

A lease is yieldable if you say so (`--yieldable`, or `"yieldable": true`). gpud never kills a
job. When something more important waits, a yieldable job is asked to give the card back:

- `gpud run` creates the file at `$GPUD_YIELD_FILE` (it does not exist before) and sends the
  yield signal to the child. The signal is `[run] yield_signal` from gpud's config (default
  `SIGUSR1`); `--yield-signal` overrides it per job (`TERM` and `SIGTERM` are both accepted).
- A native client receives `{"event":"yield"}` on its lease stream.

What the program should do: save its state, free its VRAM, and exit (or close the connection).
Either check for the file between steps, or handle the signal. Then run it again to resume;
it queues like any other job. `examples/batch-job.sh` has a Python training loop that does this.
A job that ignores the request keeps the card, and the waiting work keeps waiting.

Non-yieldable jobs run to completion. Use that for short jobs where a restart would cost more
than the wait.

## Running gpud: system unit or user unit

`packaging/systemd/gpud.service` is the system unit. It runs as the `gpud` user, in the `video`
and `render` groups, with hardening on. Config in `/etc/gpud/config.toml`. Use it for LLM
backends, containers and system units.

`packaging/systemd/gpud.user.service` is the user unit. It reads `~/.config/gpud/config.toml`.
Use it when gpud must control `systemctl --user` services such as ComfyUI: only a user instance
can start and stop them.

What gpud needs depending on what it controls:

- **Docker or podman containers**: the user needs the `docker` group (add it to
  `SupplementaryGroups=` in the system unit). That group is root-equivalent; decide if you accept it.
- **System units** from the unprivileged `gpud` user: a polkit rule that allows
  `org.freedesktop.systemd1.manage-units` for that user, or run the unit as root (drop `User=`,
  `Group=` and `NoNewPrivileges=true`).
- **`systemctl --user` units**: use the user unit.
- **Extra write paths**: `ReadWritePaths=` in the hardened unit.

The daemon sends `READY=1` (the unit is `Type=notify`).

## Reading GPU state

`gpud status` shows the state, who holds the card, the queue and the backend's models.
`gpud status --json` and `GET /status` give the same as JSON.
`gpud check-config` prints how the config was understood and one VRAM probe reading.
