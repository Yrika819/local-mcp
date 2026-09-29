# Changelog

## Unreleased

### Project identity

- Adopted **GoalLatch** as the public project and brand name.
- Retained the `Yrika819/local-mcp` repository path, `local-mcp` Cargo package and
  executable, existing release asset names, and durable compatibility identifiers.
  This branding change does not alter runtime behavior, authority, or stored state.

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
