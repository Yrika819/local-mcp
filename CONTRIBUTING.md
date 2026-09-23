# Contributing

Keep changes narrowly scoped and preserve the authority, sandbox, approval,
filesystem, credential, and safety boundaries described in `README.md` and
`SECURITY.md`.

Before opening a pull request, run:

```sh
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --locked -- -D warnings
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --release --locked
```

The test suite uses deterministic local fixtures. These commands do not require API
keys, real credentials, secret test accounts, or production data; never add secrets
to tests or commit them.

Do not add fallback routes around platform or safety refusals. New host-native
operations require exact scope validation, explicit approval, bounded execution,
and postcondition verification.
