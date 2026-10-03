"""Single source of truth for the Local MCP Public v1 release artifact matrix.

Both `package.py` (which builds an archive) and `verify_package.py` (which
proves an archive is correct) import this module, so a name, a binary, or a
layout can never drift between the code that builds a release asset and the
code that later rejects one.

The layout rules that are not negotiable: on Linux, `local-mcp` resolves the
`codex-linux-sandbox` helper as a *sibling* of its own executable
(`src/sandbox.rs`, `build_sandbox_process`), and refuses to run when that file
is absent. A Linux archive that ships only `local-mcp` is not a smaller
release, it is a broken one, so the two binaries travel together and
`verify_package.py` fails if either is missing.

Every platform also ships `atomic-publish`, and for a stronger reason than
convenience: it is the component that performs the Writer's file commit
(`src/workspace_publish.rs`). On Unix it runs *inside* the platform sandbox so
that making the commit atomic does not also make it less contained, and on
Windows it is the shell-free commit mechanism. `local-mcp` resolves it as a
sibling of its own executable and refuses to publish rather than falling back
to an unsandboxed in-process write, so an archive missing it would silently
lose the containment the Writer's authority model depends on.

macOS and Windows deliberately do not ship `codex-linux-sandbox`: macOS sandboxes
through the system `sandbox-exec`, and Windows has no equivalent process sandbox at
all, so neither platform has that helper to ship.
"""

from __future__ import annotations

import posixpath
from dataclasses import dataclass, field
from typing import Dict, List, Tuple

# Bumped only when the archive layout or naming convention changes in a way
# that would invalidate an already-published asset.
ARCHIVE_FORMAT_VERSION = 1

# Files that ship alongside the binaries in every archive. The MIT project
# license and Apache-2.0 Codex license require their notices to travel with
# copies; the pinned upstream Codex NOTICE is retained verbatim too. The
# project notice explains the broader locked dependency graph. README.md and
# SECURITY.md travel with every download so the security model and known
# advisory status are not separated from the binaries.
DOCUMENT_FILES: Tuple[str, ...] = (
    "CODEX-LICENSE.txt",
    "CODEX-NOTICE.txt",
    "LICENSE",
    "README.md",
    "SECURITY.md",
    "THIRD_PARTY_NOTICES.md",
)


@dataclass(frozen=True)
class Target:
    """One platform/architecture cell of the release matrix."""

    #: Short, stable identifier used in workflow matrix entries and job names.
    key: str
    #: Human-facing platform name, matching the repository's own docs.
    platform: str
    #: Architecture name as used in the archive file name.
    arch: str
    #: Rust target triple this artifact is built for.
    rust_target: str
    #: GitHub-hosted runner label used to produce the artifact.
    runner: str
    #: Expected `runner.arch` value, asserted before anything is built.
    runner_arch: str
    #: Archive format. Unix platforms get tar.gz, Windows gets zip, because
    #: that is what the platform's own tooling unpacks without extra software.
    archive_suffix: str
    #: Executables the archive must contain, in stable order.
    binaries: Tuple[str, ...]
    #: Optional extra executables that must be present for the target to work.
    required_helpers: Tuple[str, ...] = ()
    #: Executable names that are explicit platform traps. A non-Linux archive
    #: that quietly grew a `codex-linux-sandbox` is a packaging bug: on those
    #: platforms the helper panics on startup, so shipping it would be worse
    #: than shipping nothing.
    forbidden: Tuple[str, ...] = ()
    #: True when the platform's security boundary is not yet at parity with
    #: Linux/macOS. Surfaced in verification output and release notes so the
    #: experimental status cannot be lost between a run and a published page.
    experimental: bool = False
    #: Executable mode applied to binaries inside the archive, on Unix.
    binary_mode: int = 0o755
    #: Mode applied to documentation files inside the archive, on Unix.
    doc_mode: int = 0o644
    #: Whether the archive carries Unix permission bits at all. False for zip,
    #: where the format and the Windows platform both discard them.
    carries_modes: bool = True
    #: Substrings that must not appear anywhere in the archive, checked
    #: against member names. Guards against a source tree, a target cache, or
    #: local state being packaged by accident.
    denied_name_fragments: Tuple[str, ...] = field(
        default=(
            ".git",
            "/target",
            "Cargo.toml",
            "Cargo.lock",
            "flake.nix",
            "flake.lock",
            "rust-toolchain.toml",
            ".cargo",
            ".env",
            ".DS_Store",
            "node_modules",
            ".local",
        )
    )

    @property
    def stem(self) -> str:
        """Archive name without the version or format suffix."""
        return f"local-mcp-{self.platform}-{self.arch}"

    def archive_name(self, version: str) -> str:
        """Deterministic release file name, e.g. ``local-mcp-v0.1.0-linux-x86_64.tar.gz``."""
        return f"local-mcp-v{version}-{self.platform}-{self.arch}.{self.archive_suffix}"

    def root_dir(self, version: str) -> str:
        """Top-level directory inside the archive.

        A single versioned root directory keeps ``tar xzf`` from scattering
        files into whatever directory the user happened to be standing in, and
        it makes the layout of a downloaded asset obvious before extraction.
        """
        return f"local-mcp-v{version}-{self.platform}-{self.arch}"

    def expected_members(self, version: str) -> List[str]:
        """Every member path the archive must contain, in stable order.

        Verification asserts exact set equality against this list, so an extra
        file is as much a failure as a missing one.
        """
        root = self.root_dir(version)
        members = [posixpath.join(root, name) for name in DOCUMENT_FILES]
        members += [posixpath.join(root, name) for name in self.binaries]
        members += [posixpath.join(root, name) for name in self.required_helpers]
        return sorted(members)

    def expected_exec_members(self, version: str) -> List[str]:
        """Members that must carry the executable bit, on Unix only."""
        root = self.root_dir(version)
        return [
            posixpath.join(root, name)
            for name in tuple(self.binaries) + tuple(self.required_helpers)
        ]


def _linux(arch: str, rust_target: str, runner: str, runner_arch: str) -> Target:
    return Target(
        key=f"linux-{arch}",
        platform="linux",
        arch=arch,
        rust_target=rust_target,
        runner=runner,
        runner_arch=runner_arch,
        archive_suffix="tar.gz",
        binaries=("local-mcp",),
        # Shipped together with local-mcp, not optionally: the sandbox helper is
        # resolved as a sibling executable and the server refuses to start a
        # sandboxed command without it. `atomic-publish` ships on every platform
        # because it performs the Writer commit; see the module docstring.
        required_helpers=("codex-linux-sandbox", "atomic-publish"),
        # ARM64 builds are native in CI, but the current Linux sandbox
        # capability evidence documents x86_64 only; keep the ARM asset
        # explicitly experimental until sandbox behavior is validated on a
        # host that can create the required namespaces.
        experimental=(arch == "aarch64"),
    )


TARGETS: Dict[str, Target] = {
    target.key: target
    for target in (
        _linux("x86_64", "x86_64-unknown-linux-gnu", "ubuntu-22.04", "X64"),
        _linux("aarch64", "aarch64-unknown-linux-gnu", "ubuntu-24.04-arm", "ARM64"),
        Target(
            key="macos-x86_64",
            platform="macos",
            arch="x86_64",
            rust_target="x86_64-apple-darwin",
            runner="macos-15-intel",
            runner_arch="X64",
            archive_suffix="tar.gz",
            binaries=("local-mcp",),
            # Seats are provided by the system sandbox-exec; there is no
            # in-tree sandbox helper to ship on this platform. The Writer commit
            # helper still is required, because it runs inside that Seatbelt seat.
            required_helpers=("atomic-publish",),
            forbidden=("codex-linux-sandbox",),
        ),
        Target(
            key="macos-aarch64",
            platform="macos",
            arch="aarch64",
            rust_target="aarch64-apple-darwin",
            runner="macos-15",
            runner_arch="ARM64",
            archive_suffix="tar.gz",
            binaries=("local-mcp",),
            required_helpers=("atomic-publish",),
            forbidden=("codex-linux-sandbox",),
        ),
        Target(
            key="windows-x86_64",
            platform="windows",
            arch="x86_64",
            rust_target="x86_64-pc-windows-msvc",
            runner="windows-2025",
            runner_arch="X64",
            archive_suffix="zip",
            binaries=("local-mcp.exe",),
            # Windows has no Unix-equivalent process sandbox for Public v1. The
            # Writer commit helper is still required: it is the shell-free commit
            # mechanism, and it inherits the existing Windows approval gate.
            required_helpers=("atomic-publish.exe",),
            # Windows has no Unix-equivalent process sandbox for Public v1.
            forbidden=("codex-linux-sandbox", "codex-linux-sandbox.exe"),
            experimental=True,
            # zip on Windows carries no Unix permission bits; the executable
            # bit is an artifact of the filesystem once extracted.
            carries_modes=False,
        ),
        Target(
            key="windows-aarch64",
            platform="windows",
            arch="aarch64",
            rust_target="aarch64-pc-windows-msvc",
            runner="windows-11-arm",
            runner_arch="ARM64",
            archive_suffix="zip",
            binaries=("local-mcp.exe",),
            required_helpers=("atomic-publish.exe",),
            forbidden=("codex-linux-sandbox", "codex-linux-sandbox.exe"),
            experimental=True,
            carries_modes=False,
        ),
    )
}

#: Deterministic order used for SHA256SUMS and for release notes.
TARGET_ORDER: Tuple[str, ...] = (
    "linux-x86_64",
    "linux-aarch64",
    "macos-x86_64",
    "macos-aarch64",
    "windows-x86_64",
    "windows-aarch64",
)

CHECKSUMS_NAME = "SHA256SUMS"


def get(key: str) -> Target:
    """Look up a target by key, with an actionable error for a typo."""
    try:
        return TARGETS[key]
    except KeyError:
        known = ", ".join(TARGET_ORDER)
        raise KeyError(f"unknown release target {key!r}; known targets: {known}") from None


def expected_archive_names(version: str) -> List[str]:
    """All release archive names for a version, in deterministic order."""
    return [TARGETS[key].archive_name(version) for key in TARGET_ORDER]
