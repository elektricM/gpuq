# gpuq

[![ci](https://github.com/elektricM/gpuq/actions/workflows/ci.yml/badge.svg)](https://github.com/elektricM/gpuq/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)

gpuq is a GPU scheduler for Linux machines with one or more GPUs. Programs request the GPU
through gpuq, which grants it by priority and available VRAM. To free the card, gpuq can
signal running jobs, stop and start services, and unload models from LLM servers.

It works with any program that uses the GPU: renders, video encodes, simulations, games,
compute jobs, and AI tools such as LLM servers and image generators.

gpuq is in early development. The configuration format may change before 1.0.

## Features

- Priority classes, configurable
- Leases tied to a client connection, released when the client exits
- VRAM accounting, so jobs that fit can share the card
- Cooperative preemption: jobs receive a signal and exit on their own
- Service control through systemd, Docker, Podman or custom commands
- HTTP gate in front of LLM servers and services, holding requests while the GPU is busy
- Resident LLM model, reloaded with its KV cache after other jobs
- Several GPUs with one queue: a job can be pinned to a card or placed on the first card with room
- AMD and NVIDIA VRAM probes, plus a custom probe command

## Supported software

- Any command, through `gpuq run`: Blender, ffmpeg, training scripts, CUDA, ROCm or Vulkan programs
- Any service managed by systemd, Docker, Podman or custom commands
- Programs that integrate directly, through the [lease API](docs/lease-api.md)
- LLM servers, with model unloading: llama-swap, Ollama, llama.cpp `llama-server`, vLLM, SGLang
- ComfyUI, with request gating
- Immich ML, by pausing its job queues until the GPU is free, or with request gating

Example configurations are in [examples/](examples/).

## Installation

Static binaries for x86_64 and aarch64, and `.deb` and `.rpm` packages, are available on the
[releases page](https://github.com/elektricM/gpuq/releases/latest).

Building from source requires Rust 1.80 or later:

```sh
cargo install --git https://github.com/elektricM/gpuq
```

A PKGBUILD for Arch Linux is in [packaging/aur](packaging/aur).

## Usage

Minimal configuration, `/etc/gpuq/config.toml`:

```toml
[llm]
backend = "none"
```

Start the daemon:

```sh
gpuq check-config
sudo systemctl enable --now gpuq
```

`enable --now` starts gpuq now and at every boot. The packages install the systemd unit but
do not enable it, because the configuration has to be written first. The unit runs as the
`gpuq` user. To control `systemctl --user` services such as ComfyUI, use the user unit
instead (`systemctl --user enable --now gpuq`, with `loginctl enable-linger` so it runs without
a login session). Logs are in `journalctl -u gpuq`.

When installing with `cargo install`, copy the unit and the system user from
[packaging/systemd](packaging/systemd):

```sh
sudo install -m 755 ~/.cargo/bin/gpuq /usr/bin/gpuq
sudo install -m 644 packaging/systemd/gpuq.service /etc/systemd/system/
sudo install -m 644 packaging/systemd/gpuq.sysusers /usr/lib/sysusers.d/gpuq.conf
sudo systemd-sysusers && sudo systemctl daemon-reload
```

Run a job:

```sh
gpuq run --class batch --vram 8G -- blender -b scene.blend -a
```

With several GPUs, `--card NAME` picks a card and `--card any` takes the first one with room.

Other commands:

```
gpuq status          show the current lease holders and the queue
gpuq hold            hold the GPU until interrupted
gpuq check-config    validate the configuration and read the GPU
gpuq daemon          run the scheduler
```

To run an LLM server behind gpuq, see [docs/integrations.md](docs/integrations.md).

## Roadmap

- One LLM server per GPU
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

gpuq is licensed under the GNU General Public License v3.0 or later. See [LICENSE](LICENSE).
