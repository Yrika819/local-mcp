#!/usr/bin/env python3
"""Generate and verify the SHA256SUMS file for a Local MCP release.

`generate` collects the archives present in a directory and writes the standard
two-column `<sha256>  <name>` format in sorted filename order, so a user can
check a download with `sha256sum -c SHA256SUMS` (or `shasum -a 256 -c` on
macOS) without needing anything from this repository.

`verify` re-reads that file and checks it against the archives on disk. CI runs
it after generation so a truncated or corrupted asset cannot reach a release
page with a checksum that describes a different file.

Usage:
    checksums.py generate --dist dist --version 0.1.0
    checksums.py verify --dist dist --version 0.1.0
"""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path
from typing import List

sys.path.insert(0, str(Path(__file__).resolve().parent))

from targets import CHECKSUMS_NAME, TARGET_ORDER, get  # noqa: E402


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def collect(dist: Path, version: str) -> List[tuple]:
    """Return ``(filename, sha256)`` for every expected archive present."""
    rows: List[tuple] = []
    missing: List[str] = []
    for key in TARGET_ORDER:
        name = get(key).archive_name(version)
        path = dist / name
        if not path.is_file():
            missing.append(name)
            continue
        rows.append((name, sha256_of(path)))
    if missing:
        raise SystemExit(
            "error: missing release archives:\n  " + "\n  ".join(missing)
        )
    # Sorted by filename so the published file is stable across runs and
    # independent of the order matrix jobs happened to finish in.
    rows.sort(key=lambda row: row[0])
    return rows


def generate(dist: Path, version: str) -> int:
    rows = collect(dist, version)
    out_path = dist / CHECKSUMS_NAME
    # Two spaces before the name is the format both sha256sum and shasum
    # expect for binary-mode text output.
    out_path.write_text("".join(f"{digest}  {name}\n" for name, digest in rows), encoding="utf-8")
    print(f"checksums={out_path}")
    print(f"entries={len(rows)}")
    for name, digest in rows:
        print(f"  {digest}  {name}")
    return 0


def verify(dist: Path, version: str) -> int:
    out_path = dist / CHECKSUMS_NAME
    if not out_path.is_file():
        print(f"error: {out_path} does not exist", file=sys.stderr)
        return 1

    problems: List[str] = []
    seen = 0
    for line_number, line in enumerate(out_path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        parts = line.split("  ", 1)
        if len(parts) != 2:
            problems.append(f"line {line_number}: malformed checksum entry: {line!r}")
            continue
        digest, name = parts
        if len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
            problems.append(f"line {line_number}: not a lowercase sha256 digest: {digest!r}")
            continue
        archive = dist / name
        if not archive.is_file():
            problems.append(f"missing archive referenced by checksums: {name}")
            continue
        actual = sha256_of(archive)
        if actual != digest:
            problems.append(
                f"checksum mismatch for {name}: recorded {digest}, actual {actual}"
            )
            continue
        seen += 1
        print(f"ok {digest}  {name}")

    # Every matrix target must be represented, so a job that failed to publish
    # its archive cannot be papered over by a shorter checksum file.
    expected = {get(key).archive_name(version) for key in TARGET_ORDER}
    recorded = {
        line.split("  ", 1)[1]
        for line in out_path.read_text(encoding="utf-8").splitlines()
        if "  " in line
    }
    for absent in sorted(expected - recorded):
        problems.append(f"no checksum recorded for expected archive: {absent}")

    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        print(f"checksum_verification=FAIL ({len(problems)} problems)", file=sys.stderr)
        return 1

    print(f"checksum_verification=PASS ({seen} archives)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("generate", "verify"))
    parser.add_argument("--dist", required=True, help="directory holding the archives")
    parser.add_argument("--version", required=True)
    args = parser.parse_args()

    dist = Path(args.dist)
    if args.mode == "generate":
        return generate(dist, args.version)
    return verify(dist, args.version)


if __name__ == "__main__":
    sys.exit(main())
