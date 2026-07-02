# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-07-02

### Added

- Org-style release pipeline in `ci.yml`: 7-target build matrix (linux gnu/musl
  x86_64+aarch64 via cargo-zigbuild glibc 2.28, windows msvc, both mac arches
  with `MACOSX_DEPLOYMENT_TARGET`), `.deb`/`.rpm` on gnu targets, sha256
  sidecars, dry-run builds on every main push with tag-gated publishing: GitHub
  Release, crates.io, AUR (`gpur-bin`), Homebrew tap, Scoop bucket, Alpine
  `.apk`. Templates under `pkg/`.
- NVIDIA backend: NVML via `nvml-wrapper` (Linux/Windows) — utilization, VRAM,
  temperature, power + limit, fan, core/mem clocks, PCIe gen/width and RX/TX
  throughput. Driver library loaded dynamically; probe fails soft.
- Apple backend (macOS): IOKit IOAccelerator `PerformanceStatistics` —
  utilization + memory for Apple Silicon (AGX, SoC-derived name with GPU core
  count, unified-memory totals) and Intel-Mac GPUs.
- Windows generic backend: PDH `GPU Engine`/`GPU Adapter Memory` counters (Task
  Manager semantics: busiest-engine sum per adapter LUID) + DXGI for names/VRAM
  totals; covers AMD/Intel where NVML is absent.
- nvtop-style header details: integrated-GPU tag, PCIe gen@width, PCIe RX/TX,
  memory-controller busy %, plus a second per-GPU VRAM% sparkline.
- AMD: APU detection via `gpu_metrics` format revision, PCIe link speed/width
  from sysfs, APU memory clock via `pp_dpm_mclk` active level.
- AMD backend (Linux): sysfs/amdgpu — utilization (`gpu_busy_percent`), VRAM
  (`mem_info_vram_*`), edge temperature, power draw + cap, PWM fan %, core/mem
  clocks via hwmon; multi-card (iGPU + dGPU), marketing names from `pci.ids`.
  Zero power caps and gated clocks at idle are handled.
- Initial scaffold: `GpuBackend` trait with nvidia/amd/apple probe stubs and a
  deterministic mock backend (`--mock`).
- btop-style ratatui dashboard: per-GPU utilization/VRAM gauges, history
  sparklines, temperature/power/clock readouts.
- hjkl stack integration: `hjkl-theme` theming, `hjkl-config` XDG config
  loading, `hjkl-keymap` chord keybindings, `hjkl-kitty` keyboard protocol,
  `hjkl-splash` startup screen.
- CI (`ci.yml`) with lint/test/smoke across Linux/macOS/Windows and tag-driven
  release workflow (`release.yml`).

[Unreleased]: https://github.com/kryptic-sh/gpur/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.1.0
