# fuzukue

日本語: [README.ja.md](README.ja.md)

fuzukue is a workbench for writing mathematics and doing research with AI agents. The Rust server, `secretaire`, opens a project and runs agents. The GPUI desktop client brings conversations, mathematical notes, drafts, and PDFs into one workspace. The Android client lets you send instructions, follow conversations, and respond to approval requests from a phone or tablet. [Android guide](android/README.md)

The project is early. The maintainer uses it for daily work, and setup and workflows may change.

## Desktop keyboard shortcuts

| Action | Windows / Linux | macOS |
| --- | --- | --- |
| Previous / next pinned or current thread | Alt+Up / Down | Control+Command+Up / Down |
| Switch to workspace 1–9 by name, including the current workspace | Ctrl+1–9 | Command+1–9 |
| Jump to the next thread awaiting a reply or approval in this workspace | Ctrl+Shift+J | Command+Shift+J |
| Reorder a focused sidebar thread | Ctrl+Shift+Up / Down | Command+Shift+Up / Down |

## Install on Mac

Paste this into Terminal on macOS 15 or later once a release with the Mac packages is published.

```sh
curl -fsSL https://raw.githubusercontent.com/momota1029/fuzukue/main/install.sh | sh
```

The installer selects Apple Silicon or Intel, adds fuzukue to Applications in your home folder, and starts the local server. It requires Git, GitHub CLI, and either Claude Code or Codex, and guides you through installing any that are missing. When Apple’s Command Line Tools finish installing, run the same command again. The installer initializes the default project with Git. You can sign in to GitHub later, when you need its online features. [Mac installation guide (Japanese)](docs/install-mac.md)

## Features

- Organize definitions, theorems, proofs, and notes into cards and decks. Follow references between cards and browse the sample workspace for examples. [Card graph](docs/graph.md) · [Sample workspace](examples/lab/README.md)
- Track findings from human and agent reviews of proofs and drafts in a deck’s review findings ledger. [Review findings ledger](docs/review-findings.md)
- Grant agents read access to a specific server-side folder for a thread, choose how long it lasts, and review or revoke access. [Folder grants](docs/folder-grants.md)
- Keep requests and promises that outlast a turn on the thread’s TODO list, and check the count or mark items done on the desktop. [Thread TODOs (Japanese)](docs/thread-todo.md)
- Keep private preferences and work notes across conversations, with per-item sharing for worker assignments. [Front-desk memory (Japanese)](docs/memory.md)
- Render callouts, tables, and review findings from XML components embedded in Markdown replies on the desktop. [XML components](docs/xml-components.md)
- Browse a research deck’s exploration tree to see questions and conjectures by branch or by status. [Sample exploration tree](examples/lab/research/curvature/exploration/tree.json)
- Receive desktop update notices from the connected server and resume interrupted downloads. [Client distribution and updates](docs/update.md)

## Quick start from source

On macOS or Linux, clone into your home directory and run setup from your desktop session. It detects your OS and architecture, builds both components from the clone, and starts a user service. The preview shows any prerequisites to install. [Mac setup guide (Japanese)](docs/install-mac.md)

```sh
cd "$HOME"
git clone https://github.com/momota1029/fuzukue.git
cd fuzukue
sh scripts/setup-unix.sh --dry-run
sh scripts/setup-unix.sh
"$HOME/.local/bin/fuzukue"
```

The default project is `examples/lab`. To use an existing folder under your home directory, pass `--project "$HOME/path/to/project"`. On Linux, use a systemd user session and a Wayland or X11 desktop with Vulkan support. To update, close the desktop, run `git pull --ff-only`, and run setup again with the same project option.

On Windows, run the server in WSL2 and the desktop client on Windows. Set up Ubuntu in WSL2, clone the repository onto a local Windows drive, and install the Rust MSVC toolchain and the Visual Studio Build Tools C++ workload. From the Windows clone, use PowerShell to preview and then run setup. The script builds the server in WSL and the client on Windows, configures the connection, and creates a Start menu shortcut. [Windows guide](docs/windows.md) · [Setup script](scripts/setup-windows.ps1)

```powershell
.\scripts\setup-windows.ps1 -WhatIf -Shortcut
.\scripts\setup-windows.ps1 -Shortcut
```

To work with an agent, install its CLI and sign in with your own account in the environment where `secretaire` runs.

For Android source builds and signed APKs, follow the [Android guide](android/README.md). The repository includes an [Android release script](scripts/release-android.sh).

## Install from a release

When matching packages are published, use the Mac command above or follow the packaged installation instructions for Linux and Windows. [Installation guide](docs/install.md#english)

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

## Personal preferences

Put Markdown preferences in `~/.fuzukue/rules/` on the server for all workspaces, or in `<workspace>/.fuzukue/rules/` for one workspace. Start with the preferences you explicitly want to keep. Fuzukue reads immediate `.md` files in filename order, home first and workspace second. Workspace preferences take precedence where they conflict. Shared policy, approval checks, and publishing checks still apply, including when filenames match.

To set preferences for a paper, create a dedicated memo and set `writing_memo = "2026-11-28-writing-style"` in the paper's deck manifest, using the ID from `memos/<id>/card.md` in the same workspace. Attach the paper to the conversation. Fuzukue uses the memo body when exactly one paper is attached and the task has no other recognizable paper reference. An ordinary delegated job keeps the paper selected for its assignment. A plan keeps its originating paper key and uses the memo only while that paper is the sole paper attachment and still applies to the task. A worker digest uses a paper memo only when all included assignments share that paper selection.

Fuzukue supplies these preferences to front desks and ordinary delegated workers for trusted user turns. External-input turns, GitHub and issue sessions, workshop sessions, and blind reviews receive no new preference text. Codex workers receive it in developer instructions on ephemeral threads.

Claude and the resumable Codex front desk receive current-turn guidance as input. Preference text sent earlier stays in that conversation's context, like the rest of the conversation, including on later automatic turns. Codex also retains that input in its local rollout history. Fuzukue can reference the latest confirmed full block when guidance is unchanged in a live session. It sends the full block again after a failed turn, on a new backend connection, or after compaction. Updated guidance supersedes earlier blocks, and turns outside the trust rules explicitly cancel earlier preferences. A paper memo applies only to the named paper.

Each directory can contain up to 32 Markdown files and supply up to 24 KiB of text. Each file or linked memo can be up to 8 KiB. Missing preference directories are empty. Read failures, symbolic links, and over-limit content produce a warning. Fuzukue skips Git-tracked Markdown preferences and warns in both the home and workspace directories. To keep workspace preferences out of the index, add `/.fuzukue/` to your local Git exclude file.

Both rule directories and linked paper memo folders are protected destinations for delegated Fuzukue file tools. Claude's native file tools check those destinations. A scoped Codex worker treats existing protected directories in its checkout as read-only, and Codex worker patch approval requests are declined when they identify a protected path. Workers with unrestricted shell access can still bypass these protections. Ask the front desk to handle changes that need approval.

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
