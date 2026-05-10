# Workflow Strategy

This directory intentionally keeps GitHub Actions boring:

- `commit-check.yml` is the only commit and pull request check.
- `rust-release.yml` is the only release workflow.

## Commit Check

`commit-check.yml` runs on pull requests, pushes to `main`, and manual dispatch. It keeps the check path small:

- repository invariant scripts
- `cargo fmt --check`
- `cargo check --workspace --locked`

## Release

`rust-release.yml` runs only for `rust-v*.*.*` tags. It validates that the tag matches `codex-rs/Cargo.toml`, builds the supported release targets, and publishes the GitHub Release artifacts.
