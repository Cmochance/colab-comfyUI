# colab-comfyUI

<p align="center">
  <a href="README.md">简体中文</a> |
  <a href="README.en.md">English</a> |
  <a href="documents/CHANGELOG.md">Changelog</a>
</p>

<p align="center">
  <a href="LICENSE.txt"><img alt="License" src="https://img.shields.io/github/license/Cmochance/colab-comfyUI"></a>
  <a href="https://www.rust-lang.org/"><img alt="Rust" src="https://img.shields.io/badge/Rust-stable-DEA584?logo=rust&logoColor=white"></a>
  <a href="https://tauri.app/"><img alt="Tauri" src="https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white"></a>
  <a href="https://developer.apple.com/macos/"><img alt="macOS" src="https://img.shields.io/badge/macOS-13%2B-000000?logo=apple&logoColor=white"></a>
</p>

**colab-comfyUI** is a macOS desktop app. It starts [ComfyUI](https://github.com/comfyanonymous/ComfyUI) on a Google Colab GPU and shows its interface in the app's own window, so you can generate images and video without typing `colab` commands in a terminal.

## Current state

Usable (**v0.6.0 Phase 5**):

- One-click launch: supports `L4` (`High-RAM` disabled), `T4`, `G4`, `A100`, and `H100`, with preflight `colab` auth and compute-unit balance checks (confirm prompt on 0 CU)
- Vision Interrogation & Fixed-Port Local API Gateway: on-demand prebuilt `b12722` CUDA `llama-server` (`sha256`-verified, driver/`glibc` probed, `L4`/`T4` auto-eviction via ComfyUI `/free`) forwarded over main `ControlMaster` to `llm.sock`; supports image-to-prompt interrogation (FLUX / Tags / Wan 2.2) and a keychain-authenticated `127.0.0.1:18080` passthrough gateway for native `/v1/messages` & `/v1/chat/completions`
- Verified Model/Workflow Catalog & `HF_TOKEN` Keychain: ships verified presets (`FLUX.1-dev FP8`, `FLUX.1-schnell FP8`, `Wan 2.2 TI2V 5B`) plus custom Hugging Face URL/repo import, dual-disk 2 GiB quota checks, `/content`-first download before atomic Drive copy, and stdin-fed `aria2c` injection for `HF_TOKEN`
- Auto Output Sync & PNG Metadata Gallery: syncs images/videos via `/ws` (`executed`) and `/history` cursor deduplication, and extracts PNG `tEXt`/`iTXt` prompt & workflow metadata with prompt copy and Finder reveal
- `agy-cs` multi-account integration & drift guard: detects local `agy-cs` and `Agy Switcher.app` (gracefully degrading to single-account mode when absent), displays per-account CU balances, blocks switching while a session is active, and supports one-click switch-back by resolving `owner_email` via `agy-cs colab ls`
- Dual-clock sleep-aware idle watchdog: enabled at 30 minutes by default (`15 / 30 / 60 min / Off`); detects macOS sleep/wake clock jumps, re-checks `/queue` after wake, always gives a full 3-minute stop countdown (with "+30 min" extend), and warns when estimated remaining runtime drops below 15 minutes
- Google Drive persistence (zero token upload): interactive browser OAuth prompt for `drivemount`, frozen core Colab package constraints (`-c /tmp/colab_constraints.txt`), commit-pinned `custom_nodes_repos.txt`, byte-verified directory linking, `--no-drive` symlink/YAML cleanup, and manual/pre-stop `/history` workflow export + `drive.flush_and_unmount()` (non-blocking on stop)
- Live GPU & CU telemetry: 3s local proxy VRAM/RAM/queue polling, 20s `ControlMaster`-reused `nvidia-smi` & disk probe, 60s `colab usage` remaining-time estimate, 120-sample live chart, live Chinese/English switching, and a privacy-mask toggle
- Zero-exposure local tunnel & dual windows: single-slot SSH `ControlMaster` + Unix socket + `127.0.0.1:18188` `Host`/`Origin`-validated reverse proxy, with zero Tauri IPC permissions on the `canvas` window
- Billing safeguards: two-phase local session ledger (`~/.colab-comfyui/`), startup reconciliation, three-way exit confirmation, triple-verified stop reports, and a link to manage `[?]` orphan sessions

## Requirements

- macOS 13 or later
- The `colab` command from [`google-colab-cli`](https://pypi.org/project/google-colab-cli/), already signed in

## Running from source

You need Rust (stable), Node.js 22 or later, and the Xcode command line tools.

```bash
git clone https://github.com/Cmochance/colab-comfyUI.git
cd colab-comfyUI
make dev      # for development; restarts when the code changes
make test     # tests
make app      # builds build/Colab ComfyUI.app
make dist     # builds the dmg and its checksum under dist/
```

Building, testing and packaging all go through `make`. Development rules are in [AGENTS.md](AGENTS.md).

## License

[MIT](LICENSE.txt)
