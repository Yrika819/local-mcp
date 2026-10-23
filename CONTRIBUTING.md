# Contributing to GoalLatch

Thanks for considering a contribution. Small, reviewable changes are easiest to
verify. Before proposing a change, read [README.md](README.md) and
[SECURITY.md](SECURITY.md), especially the platform and authority boundaries.

## Before opening an issue or pull request

- Search existing Issues and pull requests for related work.
- For bugs, include the GoalLatch version, OS/architecture, minimal
  reproduction, actual result, and expected result.
- For changes affecting security, permissions, sandboxing, recovery, or public
  behavior, explain the invariant and how it is tested.
- Never attach real credentials, private project data, or unredacted logs.
  Approval/activity output can include paths, commands, and file diffs. Redact
  personal data and secrets before sharing.
- Report vulnerabilities privately using the process in [SECURITY.md](SECURITY.md);
  do not create a public exploit issue.

## Local checks

Use Rust 1.96 (the toolchain declared by the project) and the locked dependency
graph. Before a pull request, run the repository's documented checks when your
change can be built on the current platform:

```sh
cargo build --locked --bin atomic-publish
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --locked -- -D warnings
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --release --locked
```

The test suite uses deterministic local fixtures; these checks do not require
API keys, real credentials, secret test accounts, or production data. Never add
secrets to tests or commit them. `cargo test` and release builds can take time;
wait for completion and include actual results. `atomic-publish` is a required
sibling helper in release archives and must not be replaced with an in-process
write fallback. Linux
sandbox tests also require the documented Bubblewrap and kernel prerequisites.
A successful build on one OS does not prove another platform's security
behavior; Windows remains Experimental. In a docs-only pull request, run the
checks relevant to the changed material, such as link, command, and diff
validation, and say what was not tested.

## Pull requests

- Target the repository's current default branch and keep the change scoped.
- Explain user-visible behavior, security impact, verification commands/results,
  and known limitations.
- Do not weaken or disable security tests to make a change pass.
- Do not add secrets or generated build outputs.
- Do not publish releases/tags or perform force pushes as part of a contribution.

Maintainers may ask to split large changes or add negative tests for refused
operations. CI is authoritative for platform-specific verification.
