<div align="center">

# gpud

**A scheduler for one GPU shared by many AI programs**

[![ci](https://github.com/elektricM/gpud/actions/workflows/ci.yml/badge.svg)](https://github.com/elektricM/gpud/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)
[![rust](https://img.shields.io/badge/rust-1.80%2B-orange)](Cargo.toml)

[Installation](#installation) · [Quick start](#quick-start) · [Documentation](#documentation) · [Roadmap](#roadmap)

</div>

---

## Overview

A home server or workstation with one GPU often runs several programs that want it: an LLM
server for chat, ComfyUI for images, a speech model, Immich for photo search, the occasional
training run. They do not know about each other. The usual result is out-of-memory crashes,
or stopping and restarting things by hand.

gpud sits between these programs and the card. Every program asks gpud before using the GPU,
waits its turn according to a priority class, and gives the card back when something more
important needs it. gpud starts and stops services, unloads and reloads models, and keeps
track of how much VRAM each program declared, so several of them can share the card when
they fit.

gpud is a single static binary written in Rust. It does not link against any GPU library and
needs no changes to your clients.

> **Status:** v0.1, early. The scheduler and every listed integration are covered by tests.
> Expect configuration changes before 1.0.

## Features

- **Priority scheduling.** Five default classes (`realtime`, `llm`, `interactive`,
  `background`, `batch`), with your own classes and order.
- **Cooperative hand-over.** Programs are asked to yield through a signal, a file or a service
  stop. Nothing is killed.
- **Crash-safe leases.** A lease is an open connection. When a program exits or crashes, its
  lease ends with it and the card is never left reserved.
- **VRAM sharing.** Jobs declare what they need (`--vram 8G`), and gpud runs them side by side
  when they fit, with a safety margin.
- **Service control.** Starts and stops GPU services through systemd, Docker, Podman or plain
  commands, and can hold their HTTP requests until the card is free.
- **Resident LLM.** Optionally keeps one model loaded while the card is idle, unloads it for
  other work and restores it afterwards, KV cache included where the server supports it.
- **Transparent gate.** LLM clients keep their address. gpud listens on the server's usual
  port and streams requests through.

## Supported software

| Category | Supported |
|---|---|
| LLM servers | llama-swap, Ollama, llama.cpp `llama-server`, vLLM, SGLang |
| GPU services | ComfyUI, Immich ML, and any GPU daemon run by systemd, Docker, Podman or a command |
| Jobs | Any program, through `gpud run` |
| Native clients | Any program that can open an HTTP connection, through the [lease API](docs/lease-api.md) |
| GPUs | AMD (sysfs), NVIDIA (`nvidia-smi`), others through a custom probe command |

Ready configurations for each of them are in [examples/](examples/).

## Installation

### Prebuilt binaries and packages

Each [release](https://github.com/elektricM/gpud/releases/latest) provides static binaries
for x86_64 and aarch64, plus `.deb` and `.rpm` packages, with a `SHA256SUMS` file.

```sh
# Debian, Ubuntu
sudo apt install ./gpud_0.1.0-1_amd64.deb

# Fedora, RHEL
sudo dnf install ./gpud-0.1.0-1.x86_64.rpm
```

The packages install the binary, the systemd units and a default `/etc/gpud/config.toml`.

### Arch Linux

A `PKGBUILD` is in [packaging/aur](packaging/aur). The AUR package is coming.

### From source

Requires Rust 1.80 or newer.

```sh
cargo install --git https://github.com/elektricM/gpud
```

## Quick start

**1. Configure.** The smallest configuration schedules jobs and services, with no LLM server:

```toml
# /etc/gpud/config.toml
[llm]
backend = "none"
```

To put an LLM server behind gpud, move the server to another port and point gpud at it.
gpud takes over the original port, so clients do not change:

```toml
[gate]
port = 8080                          # the port your clients already use
backend = "http://127.0.0.1:8081"    # where the LLM server now listens

[llm]
backend = "llama-swap"               # or ollama, llama-server, vllm, sglang
resident = "my-model"                # kept loaded while the card is idle
```

**2. Check and start.**

```sh
gpud check-config                    # validates the file and reads the GPU
sudo systemctl enable --now gpud
```

Use the user unit `gpud.user.service` instead when gpud has to control `systemctl --user`
services.

**3. Run something.**

```sh
gpud run --class batch --vram 8G --yieldable -- python train.py
gpud status
```

## Usage

| Command | Description |
|---|---|
| `gpud daemon` | Run the scheduler (normally started by systemd) |
| `gpud run [--class C] [--vram SIZE] [--yieldable] -- CMD` | Wait for the card, run a command, release it on exit |
| `gpud hold [--class C] [--vram SIZE]` | Hold the card until Ctrl-C, for example while gaming |
| `gpud status` | Show the card, the current holders and the queue |
| `gpud check-config [-c FILE]` | Validate a configuration and read the GPU |

The CLI talks to the daemon on `127.0.0.1:9390`. Use `--api HOST:PORT` or `GPUD_API` to
change it.

## Documentation

| Document | Contents |
|---|---|
| [How it works](docs/how-it-works.md) | Leases, classes, yielding, the resident LLM, an example timeline |
| [Integrations](docs/integrations.md) | Setting up each LLM server, service and job type |
| [Configuration](docs/config.md) | Every option with its default |
| [Lease API](docs/lease-api.md) | The HTTP protocol, with a Python client example |
| [Roadmap](docs/roadmap.md) | Planned features and current workarounds |

## Roadmap

- Multi-GPU support, with one queue across several cards
- A web UI showing the card, the queue and the leases
- Automatic desktop and game detection
- Prometheus metrics
- Configuration reload without restart
- Intel VRAM probe

Details and current workarounds are in [docs/roadmap.md](docs/roadmap.md).

## Contributing

Bug reports, integration requests and pull requests are welcome. Before opening a pull
request, please run:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests use fake servers and a fake GPU, so no graphics card is needed to run them.

## License

Copyright (C) 2026 Martin Dolez

gpud is free software: you can redistribute it and/or modify it under the terms of the
[GNU General Public License](LICENSE) as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version.
