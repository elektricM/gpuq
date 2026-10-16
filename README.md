<div align="center">

# gpud

**A scheduler for one GPU shared by many AI programs.**

[![ci](https://github.com/elektricM/gpud/actions/workflows/ci.yml/badge.svg)](https://github.com/elektricM/gpud/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)

[Install](#install) · [Quick start](#quick-start) · [How it works](docs/how-it-works.md) · [Integrations](docs/integrations.md) · [Configuration](docs/config.md)

</div>

Running an LLM server, ComfyUI and a training script on the same card usually ends in
out-of-memory crashes or in stopping things by hand. With gpud, every program asks for the card
first, waits its turn by priority, and gives it back when asked.

```mermaid
flowchart TB
    cmd["Scripts and jobs<br/><i>gpud run -- cmd</i>"] --> queue
    svc["GPU services<br/><i>ComfyUI, Immich ML, TTS</i>"] --> queue
    llm["LLM servers<br/><i>llama-swap, Ollama, vLLM</i>"] --> queue
    app["Your own app<br/><i>lease API</i>"] --> queue
    queue(["<b>gpud</b><br/>priority queue and leases"]) ==> gpu[("GPU")]
```

## Features

- Priority queue with classes you can reorder
- Jobs are asked to yield, never killed
- Starts and stops GPU services through systemd, docker, podman or commands
- Keeps an LLM loaded while the card is idle and restores its KV cache after other work
- Works with llama-swap, Ollama, llama-server, vLLM, SGLang, ComfyUI and any CUDA or ROCm program
- AMD and NVIDIA, other GPUs through a custom probe
- One static binary, no GPU libraries, clients keep their ports

## Install

Download a static binary, `.deb` or `.rpm` from the
[latest release](https://github.com/elektricM/gpud/releases/latest), or build it:

```sh
cargo install --git https://github.com/elektricM/gpud
```

An AUR package is in [packaging/aur](packaging/aur).

## Quick start

```toml
# /etc/gpud/config.toml
[llm]
backend = "none"    # or llama-swap, ollama, llama-server, vllm, sglang
```

```sh
gpud check-config
sudo systemctl enable --now gpud

gpud run --class batch --vram 8G --yieldable -- python train.py
gpud status
```

To put an LLM server behind gpud, or ComfyUI, Immich and other services, pick a ready config
in [examples/](examples/) and follow the [integrations guide](docs/integrations.md).

## Roadmap

Multi-GPU support, a web UI, automatic desktop and game detection, Prometheus metrics,
config reload, Intel VRAM probe. See [docs/roadmap.md](docs/roadmap.md).

## License

Copyright (C) 2026 Martin Dolez. Licensed under the [GNU GPL v3](LICENSE) or later.
