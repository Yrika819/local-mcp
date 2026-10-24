# Public roadmap

This is a planning document, not a promise or release schedule. Items are
subject to maintainer capacity, security review, and actual test evidence. Only
work that has landed and been verified should be described as complete.

## Current public baseline

- GoalLatch v0.1.1 is released; `local-mcp` remains the compatibility package
  and executable name.
- Linux x86_64 and macOS Intel have documented supported release boundaries;
  Linux aarch64 and Windows remain Experimental, and Apple Silicon is not
  locally release-closure validated. See [SECURITY.md](../SECURITY.md).
- Third-party MCP client configurations are not claimed as verified by this
  documentation campaign.

## Next: external beta readiness

1. **Recruit a small, opt-in external beta cohort.** Publish the exact version,
   platforms, prerequisites, and security limitations before inviting anyone.
   Do not collect secrets, private repositories, or unnecessary telemetry.
2. **Measure first-run success.** Define a voluntary checklist for download
   checksum, install, CLI smoke check, session start, MCP tool discovery, and
   first read-only Goal. Record denominators and platform/version separately;
   report missing responses as unknown, not failure or success.
3. **Collect real use cases.** Ask participants to submit opt-in summaries with
   a template recording scenario, version/platform, expected outcome, actual
   outcome, and consent to publish. Do not turn participation into an implied
   endorsement.
4. **Record release downloads.** Capture GitHub release asset download counts
   at dated intervals from the public release page/API. Treat counts as
   aggregate downloads, not unique users or successful installs.
5. **Triage feedback consistently.** Label reports by reproducibility,
   platform, severity, security impact, and area (install/MCP/permissions/
   sandbox/Goal/recovery/docs). Route suspected vulnerabilities privately.
6. **Plan an independent soak-harness PR.** Specify request/response accounting,
   bounded resource use, failure injection, platform matrix, artifacts, and
   explicit non-goals. Keep the harness out of the product runtime and do not
   use incomplete historical soak results as a reliability claim.
7. **Plan a security audit.** Scope threat model, supported platforms,
   permission-root checks, sandbox setup/refusal behavior, Windows
   host-native boundary, durable recovery, dependencies, reviewer independence,
   and remediation tracking. Publish scope and limitations with any findings.

No beta, user study, successful onboarding rate, adoption story, or independent
audit is claimed by this roadmap. Do not create fictional user feedback or
metrics.
