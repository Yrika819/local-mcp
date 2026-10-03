# Contributing

Keep changes narrowly scoped and preserve the authority, sandbox, approval,
filesystem, credential, and safety boundaries described in `README.md` and
`SECURITY.md`.

Before opening a pull request, run:

```sh
# `atomic-publish` is the host-owned helper that performs the Writer's file commit.
# Several tests drive the real mutation path, and `cargo test` builds a bin *test
# harness* rather than the bin itself, so build it explicitly first. CI does the same.
cargo build --locked --bin atomic-publish
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --locked -- -D warnings
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --release --locked
```

`atomic-publish` must also ship next to `local-mcp` in a release archive. It resolves
as a sibling of the host executable and has no fallback to an in-process write, so an
install without it refuses to write rather than quietly losing the sandbox containment
the Writer's authority model depends on. On Linux the `codex-linux-sandbox` helper is
required alongside it.

The test suite uses deterministic local fixtures. These commands do not require API
keys, real credentials, secret test accounts, or production data; never add secrets
to tests or commit them.

Do not add fallback routes around platform or safety refusals. New host-native
operations require exact scope validation, explicit approval, bounded execution,
and postcondition verification.
