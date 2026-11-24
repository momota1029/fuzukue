# Contributing to fuzukue

日本語: [CONTRIBUTING.ja.md](CONTRIBUTING.ja.md)

Bug reports, suggestions, questions, and pull requests are welcome.

## Project status

fuzukue is early and the maintainer uses it for daily work. Replies may take time.

## Open an issue

Use [GitHub Issues](https://github.com/momota1029/fuzukue/issues) for reports, suggestions, and questions from users and contributors. The maintainer tracks project plans in the [roadmap deck](roadmap/README.md).

- For a bug, describe what happened, what you expected, and how to reproduce it. Include the version or build ID, the operating systems, and the clients or server involved.
- For a suggestion, start with what you want to do and where you run into trouble. Include any approaches you’ve tried.
- For a question, describe what you want to know and, where relevant, the guides you’ve read or steps you’ve tried.

Remove tokens, credentials, tailnet names or addresses, personal file paths, and private document contents from anything you post publicly, including logs and screenshots.

## Report a security vulnerability

Use GitHub’s [private vulnerability reporting form](https://github.com/momota1029/fuzukue/security/advisories/new) to share vulnerability details and reproduction steps with the maintainer.

## Pull requests

For small fixes, open a pull request directly. For new features or larger changes, open an issue first to discuss the goal and approach. In your pull request, explain why you made the change and describe the tests or checks you ran and their results.

## AI-assisted contributions

AI assistance is welcome. Explain how and where you used it in the pull request, review the changes yourself, and run the relevant tests. Keep sensitive information out of AI tool inputs and generated changes.

## Testing

Run the checks relevant to your changes. See the [README](README.md) for setup.

Run the Rust workspace tests (server and shared crates) from the repository root. The desktop client uses its own manifest.

```sh
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

## Documentation

Start with the [README](README.md). Setup and feature guides are in [`docs/`](docs/).

## License

Contributions are licensed under the project’s terms, `MIT OR Apache-2.0`. See the [MIT License](LICENSE-MIT) and the [Apache License 2.0](LICENSE-APACHE).
