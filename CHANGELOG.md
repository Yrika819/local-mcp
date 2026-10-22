# Changelog

## Unreleased

### Project identity

- Adopted **GoalLatch** as the public project and brand name.
- Retained the `Yrika819/local-mcp` repository path, `local-mcp` Cargo package and
  executable, existing release asset names, and durable compatibility identifiers.
  This branding change does not alter runtime behavior, authority, or stored state.

## 0.1.1 — Stability release

### Reliability

- Strengthened job ownership and cleanup for lost responses.
- Capped durable Goal lock wait at 5 seconds.
- Moved synchronous Goal store work off async Tokio workers to reduce worker
  starvation.
- Stabilized Windows snapshot job timeout tests.

### Verification

- 1,301 Rust tests pass.
- Linux / macOS / Windows CI green.
- 2-hour continuous operation observed; old soak harness did not prove complete
  response coverage for every sent request.
- Improved harness short-duration full-response verification.
- 6-platform release builds.

### Known limitations

- Windows remains experimental.
- Linux aarch64 sandbox closure not release-validated.
- Known inherited dependency advisories remain disclosed.
- Long-running mixed Goal/Job load not fully verified.
- Tail latency under high load.
- No signing/notarization.

## 0.1.0 — Initial release

### Goal / orchestration

- Added durable Goal / Task Orchestrator V1 tools with bounded host-controlled
  runs, durable state transitions, recovery, verification, and replanning.
- Kept Goal authority separate from filesystem, sandbox, approval, side-effect,
  and legacy job authority.

### Security hardening

- Hardened session filesystem and execution-cwd authority checks.
- Removed public executable Codex fallback; `codex_fallback` is diagnose-only.
- Prevented `read_only_command` labels from granting host-native fallback.
- Separated sandbox-wrapper lifecycle from requested-command lifecycle. A wrapper
  that starts, exits nonzero, or reports a setup failure no longer implies the
  requested command started, and executable fallback now requires host-owned
  proof of that start.
- Refused an unsupported or unusable sandbox runtime in the trusted parent before
  the sandbox helper or any requested command is spawned, classifying the refusal
  from typed host-owned evidence as a terminal block rather than a permission
  failure.
- Automatic executable fallback is diagnose-only on Linux and macOS, where the
  sandbox wrapper cannot report whether the requested command started.
- Documented safety, approval, Windows, and credential boundaries.

Formatting and lint-only cleanup is intentionally not listed as a behavioral
release change.
