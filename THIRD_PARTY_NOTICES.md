# Third-party notices

This file records the technical dependency-license evidence used for the Public v1
release review. It is not legal advice and does not replace the license and notice
files distributed by each upstream project.

## Project license

`local-mcp` is distributed under the MIT License. See [`LICENSE`](LICENSE).

## Direct dependencies

The direct dependencies in `Cargo.toml` use permissive licenses identified by Cargo
package metadata:

- `anyhow`, `allocative`, `base64`, `clap`, `dirs`, `rama-*`, `serde`, `serde_json`,
  `sha2`, and `uuid`: MIT and/or Apache-2.0.
- `tokio`: MIT.
- `similar`: Apache-2.0.
- `libc`: MIT and/or Apache-2.0.
- `openssl`: Apache-2.0.
- `codex-protocol`, `codex-utils-absolute-path`, `codex-sandboxing`, and
  `codex-linux-sandbox`: Apache-2.0, from the pinned OpenAI Codex Git revision in
  `Cargo.toml` and `Cargo.lock`.
- `windows-sys` (Windows only): Apache-2.0 OR MIT. Its enabled feature set is
  limited to the Windows foundation, security, authorization, filesystem, system
  services, and threading APIs used by the named-pipe ACL implementation.

The OpenAI Codex dependencies retain their upstream attribution and license terms.
When distributing binaries, preserve the relevant Codex repository license and
notice files alongside this project notice.

## Transitive dependencies

The locked dependency graph was inspected with `cargo metadata --locked --all-features`;
at release-review time it contained 503 packages, including `local-mcp`, and every
package exposed license metadata. `cargo-license` 0.7.0 also produced records for
all 503 packages. The complete transitive inventory is generated from the lockfile
rather than copied into this document, so the lockfile remains the authoritative
version/source record.

The metadata inventory includes licenses that require deliberate upstream notice
review rather than being summarized as only MIT/Apache-2.0: `option-ext` is
MPL-2.0; `webpki-roots` is CDLA-Permissive-2.0; `r-efi` offers
Apache-2.0 OR LGPL-2.1-or-later OR MIT; and other transitive packages expose
Unicode-3.0 or BSL-1.0 terms. This file is technical evidence, not a legal
compatibility opinion or a claim that all upstream notices have been reproduced.

For a refreshed distribution review, regenerate the inventory with a trusted
`cargo-license` or equivalent tool and re-check upstream notices before publishing.
