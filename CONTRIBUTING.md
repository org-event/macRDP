# Contributing to OpenRDP

Thanks for your interest! Bug reports, test results on other Macs and clients, and pull requests are all welcome. Issues and pull requests can be written in English or Russian.

## Before you start

- For anything larger than a small fix, open an issue first so we can agree on the approach.
- The open questions and the roadmap are in [docs/plan.md](docs/plan.md).
- Apple Silicon is the only target for now.

## Development setup

```bash
git clone https://github.com/org-event/OpenRDP.git
cd OpenRDP
cargo build
cargo test
```

`rust-toolchain.toml` pins stable Rust with `rustfmt` and `clippy`. For a quick check of the video path without granting any permissions, use `--test-pattern`.

When you run the server from a terminal, macOS grants Screen Recording and Accessibility to the terminal app. Do not test with a second server on port 3389 while the installed agent is serving: use `--bind 127.0.0.1:3390`.

## Before you open a pull request

CI runs these on Apple Silicon runners, so run them locally first:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Dependency licenses and advisories are checked with [cargo-deny](https://github.com/EmbarkStudios/cargo-deny): `cargo deny check`.

- Keep each pull request to one change, and describe how you tested it: macOS version, chip, client and its version.
- Commit messages: a short imperative summary line ("Wake a sleeping display when a client connects"), then the why in the body if it is not obvious.
- Code style follows the existing code: explain *why* in comments, not what; every `unsafe` block gets a `// SAFETY:` comment.
- Add a line to `CHANGELOG.md` under "Unreleased" for user-visible changes.

## Reporting bugs

Use the bug report template. Attach the relevant part of `~/Library/Logs/openrdp.log` (or the terminal output, with `RUST_LOG=openrdp=debug` if you can) and the output of `scripts/openrdp-doctor`. Remove anything private from them first.

## License

By contributing, you agree that your contributions will be licensed under the MIT OR Apache-2.0 dual license, as described in the [README](README.md#license).
