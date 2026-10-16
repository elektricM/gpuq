# gpud

[![ci](https://github.com/elektricM/gpud/actions/workflows/ci.yml/badge.svg)](https://github.com/elektricM/gpud/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)

gpud is a GPU scheduler for Linux machines with a single GPU. Programs request the GPU
through gpud, which grants it by priority and available VRAM. To free the card, gpud can
signal running jobs, stop and start services, and unload models from LLM servers.

It works with any program that uses the GPU: renders, video encodes, simulations, games,
compute jobs, and AI tools such as LLM servers and image generators.

gpud is in early development. The configuration format may change before 1.0.

## Features

- Priority classes, configurable
- Leases tied to a client connection, released when the client exits
- VRAM accounting, so jobs that fit can share the card
- Cooperative preemption: jobs receive a signal and exit on their own
- Service control through systemd, Docker, Podman or custom commands
- HTTP gate in front of LLM servers and services, holding requests while the GPU is busy
- Resident LLM model, reloaded with its KV cache after other jobs
- AMD and NVIDIA VRAM probes, plus a custom probe command

## Supported software

- Any command, through `gpud run`: Blender, ffmpeg, training scripts, CUDA, ROCm or Vulkan programs
- Any service managed by systemd, Docker, Podman or custom commands
- Programs that integrate directly, through the [lease API](docs/lease-api.md)
- LLM servers, with model unloading: llama-swap, Ollama, llama.cpp `llama-server`, vLLM, SGLang
- ComfyUI and Immich ML, with request gating

Example configurations are in [examples/](examples/).

## Installation

Static binaries for x86_64 and aarch64, and `.deb` and `.rpm` packages, are available on the
[releases page](https://github.com/elektricM/gpud/releases/latest).

Building from source requires Rust 1.80 or later:

```sh
cargo install --git https://github.com/elektricM/gpud
```

A PKGBUILD for Arch Linux is in [packaging/aur](packaging/aur).

## Usage

Minimal configuration, `/etc/gpud/config.toml`:

```toml
[llm]
backend = "none"
```

Start the daemon:

```sh
gpud check-config
sudo systemctl enable --now gpud
```

Run a job:

```sh
gpud run --class batch --vram 8G -- blender -b scene.blend -a
```

Other commands:

```
gpud status          show the current lease holders and the queue
gpud hold            hold the GPU until interrupted
gpud check-config    validate the configuration and read the GPU
gpud daemon          run the scheduler
```

To run an LLM server behind gpud, see [docs/integrations.md](docs/integrations.md).

## Roadmap

- Multi-GPU support
- Web UI
- Desktop and game detection
- Prometheus metrics
- Configuration reload
- Intel VRAM probe

See [docs/roadmap.md](docs/roadmap.md).

## Documentation

- [How it works](docs/how-it-works.md)
- [Integrations](docs/integrations.md)
- [Configuration reference](docs/config.md)
- [Lease API](docs/lease-api.md)
- [Roadmap](docs/roadmap.md)

## Building and testing

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

The test suite does not require a GPU.

## License

Copyright (C) 2026 Martin Dolez

This program is free software: you can redistribute it and/or modify it under the terms of
the GNU General Public License as published by the Free Software Foundation, either version 3
of the License, or (at your option) any later version. See [LICENSE](LICENSE).
