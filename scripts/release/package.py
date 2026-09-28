#!/usr/bin/env python3
"""Build a deterministic Local MCP release archive for one target.

Determinism here means: given the same inputs, the archive bytes are
reproducible. Every field that would otherwise record the build machine --
timestamps, uid/gid, owner names -- is pinned to a fixed value, and members
are written in a stable order. That is what makes the published SHA256SUMS a
meaningful integrity record rather than a record of one machine's clock.

Usage:
    package.py --target linux-x86_64 --version 0.1.0 \\
               --binary local-mcp=target/release/local-mcp \\
               --binary codex-linux-sandbox=target/release/codex-linux-sandbox \\
               --repo-root . --out-dir dist

The version defaults to the `version` field of the repository's Cargo.toml so
the published asset name cannot drift from the package it was built from.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import os
import re
import sys
import tarfile
import zipfile
from pathlib import Path
from typing import Dict, List, Tuple

sys.path.insert(0, str(Path(__file__).resolve().parent))

from targets import DOCUMENT_FILES, Target, get  # noqa: E402

# 1980-01-01T00:00:00Z. The zip format cannot represent anything earlier, so
# using this single value keeps tar and zip archives stamped identically and
# keeps a zip from silently rolling a timestamp forward.
FIXED_MTIME = 315532800


def read_cargo_version(repo_root: Path) -> str:
    """Read the package version from Cargo.toml.

    Parsed by regex rather than a TOML library so the packaging path stays
    dependency-free on a runner that may not have one installed.
    """
    cargo_toml = repo_root / "Cargo.toml"
    text = cargo_toml.read_text(encoding="utf-8")
    # Only the [package] version counts: dependencies below can declare their
    # own `version` keys, and a first-match scan would find the wrong one.
    package_section = re.split(r"^\[", text, flags=re.MULTILINE)
    for section in package_section:
        if section.startswith("package]"):
            match = re.search(r'^version\s*=\s*"([^"]+)"', section, flags=re.MULTILINE)
            if match:
                return match.group(1)
    raise SystemExit(f"could not find [package] version in {cargo_toml}")


def parse_binary_args(values: List[str]) -> Dict[str, Path]:
    """Parse repeated ``--binary NAME=PATH`` arguments into a mapping."""
    parsed: Dict[str, Path] = {}
    for value in values:
        if "=" not in value:
            raise SystemExit(f"--binary expects NAME=PATH, got {value!r}")
        name, _, raw_path = value.partition("=")
        name = name.strip()
        if not name:
            raise SystemExit(f"--binary has an empty name in {value!r}")
        if name in parsed:
            raise SystemExit(f"--binary given twice for {name!r}")
        parsed[name] = Path(raw_path)
    return parsed


def _tar_filter(member: tarfile.TarInfo) -> tarfile.TarInfo:
    """Normalize ownership, timestamps, and mode on a tar member."""
    member.uid = 0
    member.gid = 0
    member.uname = "root"
    member.gname = "root"
    member.mtime = FIXED_MTIME
    return member


def build_tar_gz(
    destination: Path,
    root: str,
    files: List[Tuple[str, Path, int]],
) -> None:
    """Write a gzip-compressed tar with fully normalized member metadata.

    `gzip` is driven manually with `mtime=0` because `tarfile.open(..., "w:gz")`
    embeds the current time in the gzip header, which would defeat the whole
    point of pinning tar member timestamps.
    """
    raw = destination.with_suffix(destination.suffix + ".tmp")
    with raw.open("wb") as handle:
        # mtime=0 keeps the gzip header itself deterministic; a filename is
        # deliberately not supplied so the header carries no build path.
        with gzip.GzipFile(filename="", mode="wb", fileobj=handle, mtime=0) as gz:
            with tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as tar:
                # The root directory entry itself, so extraction preserves a
                # single predictable top level even for an empty payload.
                root_info = tarfile.TarInfo(root)
                root_info.type = tarfile.DIRTYPE
                root_info.mode = 0o755
                _tar_filter(root_info)
                tar.addfile(root_info)
                for arcname, source, mode in files:
                    info = tar.gettarinfo(str(source), arcname=arcname)
                    info.type = tarfile.REGTYPE
                    info.mode = mode
                    _tar_filter(info)
                    with source.open("rb") as payload:
                        tar.addfile(info, payload)
    os.replace(raw, destination)


def build_zip(
    destination: Path,
    files: List[Tuple[str, Path, int]],
) -> None:
    """Write a zip with normalized timestamps.

    `create_system=3` records Unix semantics in the external attributes, which
    is what lets a zip built on a Windows runner still carry the same member
    ordering and timestamps as one built anywhere else. The mode is applied
    only where the target actually consumes it; Windows targets set
    `carries_modes` to False, so the recorded mode is informational there.
    """
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for arcname, source, mode in files:
            info = zipfile.ZipInfo(arcname, date_time=_zip_timestamp())
            info.create_system = 3
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (mode & 0xFFFF) << 16
            archive.writestr(info, source.read_bytes())


def _zip_timestamp() -> Tuple[int, int, int, int, int, int]:
    """The fixed DOS timestamp used for every zip member."""
    # 1980-01-01 00:00:00, the earliest value the format can express.
    return (1980, 1, 1, 0, 0, 0)


def sha256_of(path: Path) -> str:
    """Streamed SHA-256 of a file."""
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def collect_files(
    target: Target,
    version: str,
    binaries: Dict[str, Path],
    repo_root: Path,
) -> Tuple[List[Tuple[str, Path, int]], List[str]]:
    """Assemble the archive payload, failing loudly on any layout mistake.

    Returns the payload entries and a list of human-readable problems. The
    caller decides whether to abort, so a run can report every problem at once
    instead of only the first.
    """
    problems: List[str] = []
    root = target.root_dir(version)
    payload: List[Tuple[str, Path, int]] = []

    for name in DOCUMENT_FILES:
        source = repo_root / name
        if not source.is_file():
            problems.append(f"missing documentation file: {name}")
            continue
        payload.append((f"{root}/{name}", source, target.doc_mode))

    required = tuple(target.binaries) + tuple(target.required_helpers)
    for name in required:
        source = binaries.get(name)
        if source is None:
            problems.append(f"no --binary supplied for required executable {name!r}")
            continue
        if not source.is_file():
            problems.append(f"executable {name!r} not found at {source}")
            continue
        mode = target.binary_mode if target.carries_modes else 0o644
        payload.append((f"{root}/{name}", source, mode))

    # A helper that only works on Linux must not ride along on other
    # platforms, where it panics on startup. Catching it here keeps the
    # mistake cheap instead of shipping a broken asset.
    for name in target.forbidden:
        if name in binaries:
            problems.append(
                f"{target.key}: {name!r} must not be packaged; it is Linux-only"
            )

    unexpected = sorted(set(binaries) - set(required))
    if unexpected:
        problems.append(
            f"{target.key}: unexpected binaries supplied: {', '.join(unexpected)}"
        )

    return payload, problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, help="target key, e.g. linux-x86_64")
    parser.add_argument("--version", default=None, help="defaults to the Cargo.toml version")
    parser.add_argument(
        "--binary",
        action="append",
        default=[],
        metavar="NAME=PATH",
        help="an executable to package; repeat for each required binary",
    )
    parser.add_argument("--repo-root", default=".", help="repository root for docs")
    parser.add_argument("--out-dir", default="dist", help="output directory")
    args = parser.parse_args()

    repo_root = Path(args.repo_root).resolve()
    version = args.version or read_cargo_version(repo_root)
    target = get(args.target)
    binaries = parse_binary_args(args.binary)

    payload, problems = collect_files(target, version, binaries, repo_root)
    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        return 1

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    archive_path = out_dir / target.archive_name(version)

    if target.archive_suffix == "tar.gz":
        build_tar_gz(archive_path, target.root_dir(version), payload)
    elif target.archive_suffix == "zip":
        # zip archives carry no root directory entry of their own; the shared
        # prefix in the member names provides the same single top level.
        build_zip(archive_path, payload)
    else:
        raise SystemExit(f"unsupported archive suffix: {target.archive_suffix}")

    digest = sha256_of(archive_path)
    print(f"archive={archive_path}")
    print(f"target={target.key}")
    print(f"version={version}")
    print(f"bytes={archive_path.stat().st_size}")
    print(f"sha256={digest}")
    for name in required_names(target):
        print(f"contains={name}")
    return 0


def required_names(target: Target) -> Tuple[str, ...]:
    """Executables the finished archive is expected to contain."""
    return tuple(target.binaries) + tuple(target.required_helpers)


if __name__ == "__main__":
    sys.exit(main())
