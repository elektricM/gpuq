# gpud

[![ci](https://github.com/elektricM/gpud/actions/workflows/ci.yml/badge.svg)](https://github.com/elektricM/gpud/actions/workflows/ci.yml)

gpud decides who gets the GPU when several programs want it. An LLM server stays loaded for
chat, other jobs queue for the card by priority, and the LLM comes back when they are done.
One static binary, no GPU libraries.

## Supported

| Kind | What | Notes |
|---|---|---|
| LLM backend | llama-swap | Unload/load, KV slot save and restore, aliases, CPU-only models. Tested against a fake server and on real hardware. |
| LLM backend | Ollama | Unload/load via `keep_alive`. No slots. Tested against a fake server. |
| LLM backend | llama-server, vLLM, SGLang | Cannot unload, so gpud stops and starts the service. llama-server also has slots. Tested against fake servers and fake service control. |
| Managed service | ComfyUI | Held prompts, queue-aware idle release, model switch, yield. Tested against a fake ComfyUI. |
| Managed service | Immich ML, any CUDA/ROCm daemon | Start on first request, release when idle. Control by systemd, docker, podman or command. Tested with fake control scripts. |
| Command | `gpud run` | Any program, optional yield by signal or file. |
| Lease API | HTTP + NDJSON | For programs that integrate directly. |
| GPU | AMD | sysfs read. Also tested on real hardware. |
| GPU | NVIDIA | `nvidia-smi` query. Tested against a fake `nvidia-smi`. |
| GPU | any | `command` probe: a program that prints used and total MiB. Intel: not yet, use this. |

## How the queue works

```
  llm ---(job waits, LLM idle)---> unloading ---> jobs ---(jobs done)---> restoring ---> llm
   ^                                                |                                    |
   +------------------ LLM request while a yieldable job runs: ask it to yield ----------+
```

| Class | Priority |
|---|---|
| realtime | 0 |
| llm | 1 |
| interactive | 2 |
| background | 3 |
| batch | 4 |

- Lower number wins. Add your own classes in `[classes]`.
- `background` and `batch` jobs wait until the LLM has been quiet for `idle_s`; other classes wait `gap_s`.
- `batch_hold_s` and `batch_hold_count` hold LLM requests briefly behind yieldable batch jobs.
- Jobs share the card by declared VRAM, with a margin.
- Nothing is killed. A yieldable job is asked to yield (signal or file) and exits when it is ready.
- A lease lives exactly as long as its connection, so a crashed client cannot leak the card.
- When the card is free again, the resident model is reloaded and its KV slot is restored.

## Quick start

Install a release tarball, `.deb` or `.rpm` from the
[releases page](https://github.com/elektricM/gpud/releases), the AUR package, or:

```sh
cargo install --git https://github.com/elektricM/gpud
```

Minimal `/etc/gpud/config.toml` for llama-swap (move llama-swap to port 8081, gpud takes 8080):

```toml
[gate]
backend = "http://127.0.0.1:8081"

[llm]
resident = "chat"
```

```sh
gpud check-config
sudo systemctl enable --now gpud
```

Use the user unit (`packaging/systemd/gpud.user.service`) when gpud must control
`systemctl --user` services such as ComfyUI.

## CLI

```sh
gpud run --class batch --vram 8G --yieldable -- python train.py   # queue, run, release
gpud hold --class interactive --vram all                          # keep the card until Ctrl-C
gpud status                                                       # who has the card, who waits
gpud check-config -c examples/comfyui.toml                        # validate and probe VRAM
```

## Docs

- [docs/integrations.md](docs/integrations.md): plug in your LLM server, service or job
- [docs/config.md](docs/config.md): every config key
- [docs/lease-api.md](docs/lease-api.md): the HTTP lease protocol, with a Python client
- [docs/roadmap.md](docs/roadmap.md): what is not done yet
- [examples/](examples/): ready configs for llama-swap, Ollama, vLLM, SGLang, ComfyUI, Immich ML, a generic service, and `gpud run` jobs

## License

Copyright (C) Martin. GPL-3.0-or-later, see [LICENSE](LICENSE).
