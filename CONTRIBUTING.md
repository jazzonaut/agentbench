# Contributing

Contributions are welcome through GitHub issues and pull requests.

## Development checks

Install the stable Rust toolchain, then run:

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo package --locked
```

Do not run paid live-LLM benchmarks in automated tests. Live scenarios must remain explicit at test time and redact prompts, responses, paths, and environment values from reports.

## Releases

1. Update the version in `Cargo.toml` and add the matching `CHANGELOG.md` section.
2. Merge the change after CI passes.
3. Push an annotated `vX.Y.Z` tag pointing at the release commit.
4. The release workflow validates the version, packages all platforms, generates checksums and attestations, and publishes the GitHub Release.
