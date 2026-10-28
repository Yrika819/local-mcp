# Doctor JSON contract validator (CI only)

Draft 2020-12 conformance gate for the Doctor Core V1 report contract. This is a
CI tool, not part of the `local-mcp` package: it lives in its own workspace, it
has its own `Cargo.lock`, and no runtime build, release artifact or Nix input
depends on it.

## Supply-chain pinning

- `Cargo.toml` pins the validator and its JSON dependency to exact versions
  (`jsonschema = "=0.26.0"`, `serde_json = "=1.0.140"`).
- `Cargo.lock` is committed and records the registry checksum of every
  transitive crate. `cargo ... --locked` refuses to resolve anything that is not
  already in that lock file, so the graph cannot drift between runs.
- No package is fetched from an unpinned source and the validator does not
  download a remote meta-schema; the draft is selected from the schema's own
  `$schema` field.

## What is checked

| Check | Detail |
| --- | --- |
| Schema document | compiles as Draft 2020-12; a non-string `type` probe must be rejected, proving the compile-time gate is active |
| Synthetic fixtures | `docs/examples/doctor-v1-*.json` are schema-valid |
| Negative control | 8 corrupted documents (missing required field, unknown enum, bad exit code, empty check list, malformed reason code, cleared redaction flag, unknown OS) are rejected |
| Producer report | `local-mcp doctor --json` output is schema-valid, single JSON value, stderr empty, within 64 KiB |
| Producer semantics | counts, `checks_total`, required/advisory/runtime aggregates, exit code 0/1/2 and process exit agreement, per-capability worst-wins and reason codes, probe honesty (`attempted: false` implies UNKNOWN/null), `not_run` evidence implies UNKNOWN, redaction flags, no path-like strings |

Exit code is `0` on success and `1` on any failure, with one line per failure.

## Three evidence classes

A schema-valid document is not a truthful one, and this tool keeps the classes
apart on purpose:

1. **Schema-valid synthetic fixtures.** They exercise the schema surface,
   including shapes this build never emits (the `pass` fixture carries an
   executed Bubblewrap probe; the producer always reports `attempted: false`).
2. **Current producer-compatible JSON.** Additionally checked against the
   producer profile above: observations must be null or boolean, redaction flags
   must be true, and no string may look like a local path or environment value.
3. **Runtime-observed evidence.** Not produced by Doctor V1 at all. A probe that
   did not run is UNKNOWN, never PASS.

The schema itself stays permissive about `observed` for forward compatibility,
so schema validity does not prove redaction or truthfulness on its own.

## Running

```sh
cargo run --locked --quiet --manifest-path scripts/doctor/schema-validator/Cargo.toml -- \
  --schema docs/schemas/doctor-v1.schema.json \
  --fixtures docs/examples
```

Add `--producer target/debug/local-mcp` (or `local-mcp.exe` on Windows) after
`cargo test` has built the binary to also validate real producer output.
