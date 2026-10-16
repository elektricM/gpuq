# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-17

### Changed

- The project is renamed from gpud to gpuq, because another project is already called gpud.
  The binary, crate, systemd units, system user and paths change with it:
  - binary `gpud` is now `gpuq`
  - `/etc/gpud/config.toml` is now `/etc/gpuq/config.toml`, and
    `~/.config/gpud/config.toml` is now `~/.config/gpuq/config.toml`
  - `gpud.service` is now `gpuq.service` (system and user units), and the `gpud` system
    user and group are now `gpuq`
  - environment variables `GPUD_API`, `GPUD_LOG`, `GPUD_LEASE_ID`, `GPUD_YIELD_FILE` and
    `GPUD_CARD` are now `GPUQ_API`, `GPUQ_LOG`, `GPUQ_LEASE_ID`, `GPUQ_YIELD_FILE` and
    `GPUQ_CARD`
  - the Immich state file moves from `gpud/immich.json` to `gpuq/immich.json` under the
    state directory. Its `paused_by_gpud` field, also reported by `status --json` and the
    control API, is now `paused_by_gpuq`. Resume any Immich queues paused by gpud before
    upgrading.

  To upgrade, stop and disable `gpud.service`, move `/etc/gpud` to `/etc/gpuq`, install
  gpuq, then enable `gpuq.service`.

### Added

- Man pages for `gpuq` and each subcommand, generated from the command line definitions.
- Shell completions for bash, zsh and fish.
- Packaging recipes for Arch Linux (AUR), Fedora and openSUSE (RPM spec), Debian and
  Ubuntu (debian/ directory) and Nix.
- Release asset `gpuq-<version>-vendor.tar.xz` with the vendored dependencies, for
  builds without network access.
- Release tarballs, `.deb` and `.rpm` packages include the man pages and completions.

### Fixed

- Tests pick listener ports outside the ephemeral range, so they no longer collide with
  ports in use by outgoing connections.

## [0.1.1] - 2026-10-17

### Added

- Several GPUs in one arbiter, with one queue. A job can be pinned to a card with
  `--card NAME` or placed on the first card with room with `--card any`.
- Immich ML adapter: pauses Immich job queues until the GPU is free, or gates its
  requests.

### Fixed

- Held ComfyUI prompts get a UUID, because ComfyUI rejects other ids.
- `gpud run` kills its job when it dies itself, so an orphaned job does not keep using the
  card outside a lease.
- `gpud status` prints history times as HH:MM:SS.

## [0.1.0] - 2026-10-16

### Added

- First release of the Rust version.
- Arbiter with priority classes, leases tied to the client connection and VRAM
  accounting.
- Cooperative preemption by signal.
- Service control through systemd, Docker, Podman or custom commands.
- HTTP gate in front of LLM servers and services.
- LLM backends: llama-swap, Ollama, llama.cpp `llama-server`, vLLM, SGLang.
- Managed services, with a ComfyUI flavor.
- AMD and NVIDIA VRAM probes, a custom probe command and an offload hook.
- Control API and the `daemon`, `check-config`, `status`, `run` and `hold` commands.
- Static musl binaries, `.deb` and `.rpm` packages, and a PKGBUILD.

[Unreleased]: https://github.com/elektricM/gpuq/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/elektricM/gpuq/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/elektricM/gpuq/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/elektricM/gpuq/releases/tag/v0.1.0
