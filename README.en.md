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

Usable (**v0.2.0 Phase 1 MVP**):

- One-click launch: supports `L4` (`High-RAM` disabled), `T4`, `G4`, `A100`, and `H100`, with preflight `colab` auth and compute-unit balance checks (confirm prompt on 0 CU)
- Zero-exposure local tunnel: single-slot SSH `ControlMaster` forwarding to a local Unix socket behind a `127.0.0.1:18188` `Host`/`Origin`-validated reverse proxy (no public tunnels, no `--enable-cors-header`)
- Isolated dual windows: the `main` shell manages session lifecycle and redacted logs, while the `canvas` window displays ComfyUI with zero Tauri IPC permissions
- Billing safeguards: two-phase local session ledger (`~/.colab-comfyui/`), startup reconciliation, three-way exit confirmation, triple-verified stop reports, and a link to manage `[?]` orphan sessions

Planned next:

- Google Drive persistence, pre-stop export, and live GPU/compute-unit telemetry
- `agy-cs` multi-account integration and sleep-aware idle watchdogs
- Verified model/workflow catalog and automatic output gallery sync
- On-demand vision prompt helper and fixed-port local API proxy

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
