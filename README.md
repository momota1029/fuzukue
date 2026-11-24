# fuzukue

日本語: [README.ja.md](README.ja.md)

fuzukue is a workbench for writing mathematics and doing research with AI agents. The Rust server, `secretaire`, opens a project and runs agents. The GPUI desktop client brings conversations, mathematical notes, drafts, and PDFs into one workspace. The Android client lets you send instructions, follow conversations, and respond to approval requests from a phone or tablet. [Android guide](android/README.md)

The project is early. The maintainer uses it for daily work, and setup and workflows may change.

## Features

- Organize definitions, theorems, proofs, and notes into cards and decks. Follow references between cards and browse the sample workspace for examples. [Card graph](docs/graph.md) · [Sample workspace](examples/lab/README.md)
- Track findings from human and agent reviews of proofs and drafts in a deck’s review findings ledger. [Review findings ledger](docs/review-findings.md)
- Grant agents read access to a specific server-side folder for a thread, choose how long it lasts, and review or revoke access. [Folder grants](docs/folder-grants.md)
- Render callouts, tables, and review findings from XML components embedded in Markdown replies on the desktop. [XML components](docs/xml-components.md)
- Browse a research deck’s exploration tree to see questions and conjectures by branch or by status. [Sample exploration tree](examples/lab/research/curvature/exploration/tree.json)
- Receive desktop update notices from the connected server and resume interrupted downloads. [Client distribution and updates](docs/update.md)

## Install from a release

When a matching release is published, use [install-server.sh](scripts/install-server.sh) for the Linux or WSL2 server, [install-client.sh](scripts/install-client.sh) for Linux desktop, or [install-client.ps1](scripts/install-client.ps1) for Windows desktop. See the [installation guide](docs/install.md#english) for prerequisites, release channels, and migration.

To run both components on the same Mac when macOS beta packages are available, see the [macOS beta quick start (Japanese)](docs/install-mac.md).

## Quick start from source

Install Git and a Rust toolchain. From the repository root, build the server and start it with the sample workspace.

```sh
cargo build --locked --release -p secretaire
./target/release/secretaire examples/lab
```

The server listens on `127.0.0.1:7878` by default. On Linux or macOS, use a second terminal at the repository root to build and start the desktop client. The GUI build requires platform development tools. The macOS desktop build and runtime are still being verified. [Mac guide](docs/mac.md)

```sh
cargo build --locked --release --manifest-path crates/fuzukue/Cargo.toml
FUZUKUE_URL=ws://127.0.0.1:7878/ws ./crates/fuzukue/target/release/fuzukue
```

On Linux or macOS, you can also build and launch both processes with [`scripts/run.sh`](scripts/run.sh) by passing it `examples/lab`.

On Windows, run the server in WSL2 and the desktop client on Windows. Set up Ubuntu in WSL2, clone the repository onto a local Windows drive, and install the Rust MSVC toolchain and the Visual Studio Build Tools C++ workload. From the Windows clone, use PowerShell to preview and then run setup. The script builds the server in WSL and the client on Windows, configures the connection, and creates a Start menu shortcut. [Windows guide](docs/windows.md) · [Setup script](scripts/setup-windows.ps1)

```powershell
.\scripts\setup-windows.ps1 -WhatIf -Shortcut
.\scripts\setup-windows.ps1 -Shortcut
```

To work with an agent, install its CLI and sign in with your own account in the environment where `secretaire` runs.

For Android source builds and signed APKs, follow the [Android guide](android/README.md). The repository includes an [Android release script](scripts/release-android.sh).

## Add a custom skill

Create `<workspace>/skills/<name>/SKILL.md` and keep any supporting files in the same skill folder. Give the file YAML front matter with a `description`. The optional `name` sets the role name and defaults to the folder name.

```markdown
---
description: Review notation in a draft.
---

Check that each symbol is defined before its first use.
```

Fuzukue picks up the skill in that workspace. Select it as the worker role when delegating to pass its instructions to the worker. For the standard layers, later entries override earlier entries with the same name: built-ins, rule packs, personal extensions, project extensions, then workspace-local skills.

Claude workers run with user settings and Claude Code's native skills disabled, so move skills from `~/.claude/skills` into the workspace's `skills/` directory or an extension. Adapt any Claude Code- or Codex-specific tool names and commands in `SKILL.md` to the worker's available tools.

To reuse a skill across workspaces or share it with others, package it with `extension.toml` and `skills/`. Register the extension in `~/.config/fuzukue/extensions.toml` for personal use or in `fuzukue.toml` for a project. Either can use a local path or a Git source. See the [extension guide](docs/extensions.md) for setup and other extension options.

## Connect from another device

For local use, the desktop client can connect over loopback at `ws://127.0.0.1:7878/ws`. Tailscale is optional.

For remote use, Tailscale is recommended. Join the server and client devices to the same tailnet, configure the server to listen on its Tailscale IP, and allow the client’s Tailscale login. Use the server’s Tailscale IP in the client’s remote connection settings.

Devices outside the tailnet can connect through an HTTPS reverse proxy using a per-device token. See the [remote connection guide](docs/remote.md) for both remote setups.

## Updates

A server operator can register a desktop build with `secretaire`. The server tells connected desktop clients when a build is available, and clients can download it and restart to install it. Interrupted desktop downloads can resume from the bytes already received. The updater checks the file size and SHA-256 before installation. [Desktop update guide](docs/update.md)

Android uses signed APKs and has its own in-app update check. [Android update guide](android/README.md#配布と更新)

## Contributing

Use [GitHub Issues](https://github.com/momota1029/fuzukue/issues) for bug reports, suggestions, and questions from users and contributors. The maintainer tracks project plans in the [roadmap deck](roadmap/README.md). See the [contribution guide](CONTRIBUTING.md) or the [guide to opening a PR with AI (Japanese)](docs/contributing-with-ai.md).

## Security

GitHub private vulnerability reporting is enabled for this repository. Submit reports through GitHub’s [private reporting form](https://github.com/momota1029/fuzukue/security/advisories/new).

## License

fuzukue’s own code is dual-licensed under `MIT OR Apache-2.0`. You may choose the [MIT License](LICENSE-MIT) or the [Apache License 2.0](LICENSE-APACHE). Bundled third-party code and assets are covered by their respective licenses. [Third-party notices](THIRD_PARTY_NOTICES)
