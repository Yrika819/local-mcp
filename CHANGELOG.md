# Changelog

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
- Documented safety, approval, Windows, and credential boundaries.

Formatting and lint-only cleanup is intentionally not listed as a behavioral
release change.
