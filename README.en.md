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

The project has only just been set up and **cannot be used yet**. The window shows the version and where the `colab` command was found on this Mac.

What is planned:

- Start with one click: create the Colab session, mount Google Drive, install ComfyUI, and open the tunnel to this Mac
- Show the remote ComfyUI interface in the app's window
- Show the session state, GPU usage, compute unit balance and usage rate
- Before stopping, export workflows, settings and outputs to Drive; after stopping, check that billing has stopped
- Download models when they are needed

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
