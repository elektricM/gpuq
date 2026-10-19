# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `[desktop]`: free the VRAM the graphical desktop holds on a card. `policy = "stop"` stops the
  configured user units and display manager while an LLM model from `models` is loaded, less
  than `min_free_mib` is free, or a job does not fit, and starts the display manager again
  `restore_delay_s` after the last reason. `policy = "offload"` restarts the desktop pointed at
  another card instead (`[desktop.offload]`), checks that it left the card, and uses
  `fallback` when it cannot. Off by default. Never acts while a user is logged in at the seat.
- The processes holding a card are found through `/proc/*/fd` and DRM fdinfo by PCI address,
  with their resident VRAM; processes in `ignore`, such as a headless compositor, never count
  as the desktop. Nothing is killed: only the configured units are stopped and started.
- Each change is checked: units down or up, the desktop's processes gone, VRAM before and
  after; a release that does not complete is rolled back. After each change the system and
  user journals are read for new errors, failed units and core dumps.
- After a stop, `session_env` is set in the user manager and the D-Bus activation environment,
  so D-Bus activated services do not start against the display that is gone.
- `gpuq desktop [status|release|restore]`, `GET /desktop`, `POST /desktop/release` and
  `POST /desktop/restore`; `gpuq status` and `GET /status` show the desktop state.
- Optional `wake_on_input`: a key press at the seat restores the desktop.
- `[llm.class_tuning.<class>] hold_other_models_s`: while that class uses the LLM, and that
  long after its last request, requests of less important classes for another model wait, so a
  server with one model at a time does not swap back and forth. Same-model and more important
  requests pass. `gpuq status` shows a `holding` line and `GET /status` a `model_hold` object;
  the log has one line when a hold starts and one when it ends.

### Fixed

- An unreachable Immich is logged once, and again when it is back, instead of on every poll.

## [0.3.1] - 2026-10-17

### Changed

- README: a short statement of the problem gpuq solves and an overview diagram at the top;
  systemd setup moved to `packaging/systemd/README.md`. No code changes.

## [0.3.0] - 2026-10-17

### Added

- Leases on several cards at once: `gpuq run --card gpu0,gpu1`, `--card all` and `--cards N`
  (any N cards with room); in the lease API `"cards": [...]`, `"card": "all"` or a comma list,
  and `"card_count": N`. `--vram` applies to each card. A multi-card lease is granted all at
  once or not at all and holds nothing while it waits. Later leases of its class or below do
  not overtake it on its cards. A yieldable multi-card lease is asked to yield when something
  more important needs any one of its cards.
- The env of a multi-card lease is the cards' env merged in grant order
  (`CUDA_VISIBLE_DEVICES=GPU-a,GPU-b`), and `GPUQ_CARD` is a comma list. Cards of different
  vendors can share a lease only when their env uses different variables.
- The LLM can span several cards: `llm.card` takes a list, and `[llm.models.<id>]` gives a
  model its own `cards` or its `vram`. A model that does not fit on the LLM's card gets the
  fewest extra cards that fit it; for llama-swap models the size is estimated from their GGUF
  files. The LLM is unloaded from all its cards when a job needs any of them, and reloaded on
  all of them.
- `llama-server` and other servers started by gpuq with `[llm.service] kind = "command"` get
  the cards' env and `GPUQ_CARD`; llama-server with several cards also gets
  `LLAMA_ARG_SPLIT_MODE` and `LLAMA_ARG_TENSOR_SPLIT` from the free VRAM of each card.
- A `[[service]]` can name several cards: `card = ["gpu0", "gpu1"]`.
- `gpuq status` and `GET /status` show multi-card leases, a `cards` list per lease and
  `llm_cards`. The `granted` event has `cards`.
- `docs/architecture.svg`, a diagram of the parts of gpuq, in the README and in
  `how-it-works.md`.

### Changed

- gpuq detects every AMD and NVIDIA GPU and manages all of them when the config has no
  `[[gpu.card]]` and does not pin a single device in `[gpu]` (`pci`, `nvidia_index`,
  `vendor = "command"` or `"intel"`). Cards are named gpu0, gpu1, ... (discrete first, largest
  first). Each card gets its device id from its PCI address: `CUDA_VISIBLE_DEVICES=<UUID>` on
  NVIDIA, `ROCR_VISIBLE_DEVICES=GPU-<unique_id>` (or the ROCm index from the KFD topology) on
  AMD. Leave cards out with `gpu.exclude = ["<pci address>"]` or `gpu.skip_integrated = true`.
  A single detected card now hands its device id to jobs too.
- A job that names no card goes to any card with room instead of the primary card. Send
  `"card": "primary"` (or `--card primary`) for the old behaviour. Services and the Immich
  adapter still default to the primary card.
- `gpu.nvidia_index` has no default any more; setting it means one NVIDIA card.
- `all` is a reserved card name.
- CPU markers (`llm.cpu_markers`) are also looked for in the `env` list of a llama-swap model.

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
- Tests no longer fail at random when a child process forked by another test inherits a
  socket used to check that a port is free.
- The crate builds with Rust 1.80 again, as declared in `rust-version`.

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

[Unreleased]: https://github.com/elektricM/gpuq/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/elektricM/gpuq/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/elektricM/gpuq/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/elektricM/gpuq/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/elektricM/gpuq/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/elektricM/gpuq/releases/tag/v0.1.0
