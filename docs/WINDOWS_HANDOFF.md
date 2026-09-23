# Windows Handoff

## Validated (this campaign)

- Windows path authority (drive-letter, case, UNC, dangling-final-symlink, junction/reparse escapes).
- Approval fail-closed (invalid, deny, missing session).
- Named-pipe `first_pipe_instance` double-bind rejection.
- Named-pipe remote-client rejection (`PIPE_REJECT_REMOTE_CLIENTS`).
- Named-pipe current-user-only DACL isolation (owner + sole allow ACE = current user SID;
  no Everyone / Anonymous / Authenticated Users / SYSTEM / Administrators).
- Full Windows test suite, fmt, production clippy, all-targets clippy, release build.

## Still unresolved (blocks removing Windows experimental status)

- No Unix-equivalent process/filesystem/network sandbox on Windows.
- Host-native `execute`, `start_command`, `write_file`, and `without_sandbox` remain
  approval-gated host access and must not be mistaken for sandboxed execution.
- Cross-user live connection test (requires a secondary local account) — ACL evaluation
  is the current evidence; live cross-user connection = NOT EXECUTED.
- Remaining Windows security boundaries beyond named-pipe IPC isolation.
