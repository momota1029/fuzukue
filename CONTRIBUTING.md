# Contributing to fuzukue

日本語: [CONTRIBUTING.ja.md](CONTRIBUTING.ja.md)

Bug reports, suggestions, questions, and pull requests are welcome.

## Project status

fuzukue is early and the maintainer uses it for daily work. Replies may take time.

## Open an issue

Use the [GitHub Issue forms](https://github.com/momota1029/fuzukue/issues/new/choose) for reports, suggestions, and questions from users and contributors. The maintainer tracks project plans in the [roadmap deck](roadmap/README.md).

- For a bug, describe what happened, what you expected, and how to reproduce it. Include the version or build SHA, the operating systems, and the clients or server involved.
- For a feature request, describe what you want to do, what happened with the current version, and any workarounds you tried.
- If you asked an AI agent to change fuzukue, use the AI change form to share your request, the outcome, and where you got stuck.
- For a question, describe what you want to know and, where relevant, the guides you’ve read or steps you’ve tried.

Remove tokens, credentials, private network names and addresses, personal file paths, emails, and private contents from anything you post publicly, including logs and screenshots.

## Public issue triage

The maintainer can use these labels as needed.

| Label | Use |
| --- | --- |
| `bug` | Unexpected behavior |
| `enhancement` | Proposed improvement |
| `question` | Usage or setup question |
| `needs-info` | Reproduction steps or build details needed |
| `duplicate` | Already tracked in a linked issue or card |

## Report a security vulnerability

Use GitHub’s [private vulnerability reporting form](https://github.com/momota1029/fuzukue/security/advisories/new) to share vulnerability details and reproduction steps with the maintainer.

## Pull requests

For small fixes, push a branch to your fork and open a pull request against `momota1029/fuzukue:main`. For new features or larger changes, open an issue first to discuss the goal and approach. Fill out the [pull request template](.github/PULL_REQUEST_TEMPLATE.md) and link any related issue or [roadmap card](roadmap/README.md) in its body.

## AI-assisted contributions

AI assistance is welcome. Explain how and where you used it in the pull request, review the changes yourself, and run the relevant tests. Keep sensitive information out of AI tool inputs and generated changes. The [AI contribution guide (Japanese)](docs/contributing-with-ai.md) walks through the fork, checks, and publication steps.

## Testing

Run the checks relevant to your changes. See the [README](README.md) for setup.

Before pushing, run `scripts/pre-pr-check.sh` in a clean checkout to scan the branch history, test the Rust workspace, and check Cards. Set `TMPDIR` to a writable directory outside any Git repository. To run the Rust tests directly from the repository root, use these commands. The desktop client uses its own manifest.

```sh
unset FUZUKUE_SERVICE FUZUKUE_SELF_UPDATE_READY FUZUKUE_SELF_UPDATE_SOURCE
cargo test --locked --workspace
cargo test --locked --manifest-path crates/fuzukue/Cargo.toml
```

For Linux desktop UI checks, see the [headless verification guide](docs/headless.md) for Xvfb setup. To run the chat scenario:

```sh
scripts/shot.sh examples/lab tests/shots/chat.json
```

To check Android protocol compatibility and bundled third-party notices, run these commands from the repository root. The notices check requires JDK 21. See the [Android guide](android/README.md) for unit tests.

```sh
scripts/check-android-protocol.sh
scripts/check-android-notices.sh
```

CI runs Linux Rust tests and a macOS server compile check. Cards has its own workflow. Android JVM tests run manually because the SDK setup and Gradle downloads take time.

## Documentation

Start with the [README](README.md). Setup and feature guides are in [`docs/`](docs/).

## License

Contributions are licensed under the project’s terms, `MIT OR Apache-2.0`. See the [MIT License](LICENSE-MIT) and the [Apache License 2.0](LICENSE-APACHE).
