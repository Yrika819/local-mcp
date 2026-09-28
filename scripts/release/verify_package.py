#!/usr/bin/env python3
"""Verify a packaged Local MCP release archive against the target matrix.

This runs in CI against archives produced by the packaging job, so a green
check is evidence about the real published bytes rather than about a local
build. It answers the questions a user would otherwise have to ask by hand:
does the archive extract, does it contain exactly the intended files, are the
right executables marked executable, is the binary actually built for the
architecture its name claims, and did anything private or incidental get
packaged along for the ride.

Architecture is read from the binary header directly rather than by running the
binary, so the check works for cross-platform archives on a single Linux
runner and never depends on being able to execute what it is inspecting.

Usage:
    verify_package.py --target linux-x86_64 --version 0.1.0 \\
                      --archive dist/local-mcp-v0.1.0-linux-x86_64.tar.gz
"""

from __future__ import annotations

import argparse
import hashlib
import posixpath
import struct
import sys
import tarfile
import zipfile
from pathlib import Path
from typing import Dict, List, Optional, Tuple

sys.path.insert(0, str(Path(__file__).resolve().parent))

from targets import DOCUMENT_FILES, Target, get  # noqa: E402

# ELF e_machine values (generic System V ABI).
ELF_MACHINE_X86_64 = 0x3E
ELF_MACHINE_AARCH64 = 0xB7

# Mach-O cpu_type values.
MACHO_CPU_X86_64 = 0x01000007
MACHO_CPU_ARM64 = 0x0100000C

# PE Machine values.
PE_MACHINE_X86_64 = 0x8664
PE_MACHINE_ARM64 = 0xAA64

# Mach-O filetype for an executable.
MACHO_FILETYPE_EXEC = 0x2


class VerificationError(Exception):
    """Raised with a complete list of problems found in an archive."""


def read_archive(path: Path) -> List[Tuple[str, int, bytes]]:
    """Return ``(member_name, unix_mode, contents)`` for every file member.

    Directory entries are skipped: a tar that stores an explicit root directory
    entry and one that does not are equally valid, and requiring a particular
    choice here would fail correct archives.
    """
    name = path.name
    if name.endswith(".tar.gz"):
        members: List[Tuple[str, int, bytes]] = []
        with tarfile.open(path, "r:gz") as archive:
            for info in archive.getmembers():
                if info.isdir():
                    continue
                if not info.isfile():
                    raise VerificationError(
                        f"unexpected non-regular member in archive: {info.name}"
                    )
                handle = archive.extractfile(info)
                members.append((info.name, info.mode, handle.read() if handle else b""))
        return members

    if name.endswith(".zip"):
        members = []
        with zipfile.ZipFile(path, "r") as archive:
            for info in archive.infolist():
                if info.is_dir():
                    continue
                # external_attr holds the Unix mode in the high 16 bits when
                # the entry was written with create_system = 3.
                mode = (info.external_attr >> 16) & 0o7777
                members.append((info.filename, mode, archive.read(info)))
        return members

    raise VerificationError(f"unsupported archive format: {name}")


def check_membership(
    target: Target,
    version: str,
    members: List[Tuple[str, int, bytes]],
) -> List[str]:
    """Compare actual members against the exact expected set."""
    problems: List[str] = []
    present = {name for name, _, _ in members}
    expected = set(target.expected_members(version))

    for missing in sorted(expected - present):
        problems.append(f"missing required member: {missing}")
    for extra in sorted(present - expected):
        problems.append(f"unexpected member: {extra}")

    # Every member must live under the single versioned root. This is what
    # keeps a stray path from scattering files across the user's directory.
    root = target.root_dir(version)
    for name, _, _ in members:
        if posixpath.dirname(name) != root:
            problems.append(
                f"member {name!r} is outside the single root directory {root!r}"
            )
    return problems


def check_denied_content(
    target: Target,
    version: str,
    members: List[Tuple[str, int, bytes]],
) -> List[str]:
    """Reject source trees, caches, and local state in the payload."""
    problems: List[str] = []
    root = target.root_dir(version)

    for name, _, data in members:
        relative = posixpath.relpath(name, root)
        for fragment in target.denied_name_fragments:
            if fragment in relative or f"/{fragment}" in name:
                problems.append(
                    f"member {relative!r} matches denied content fragment {fragment!r}"
                )

        # User-specific absolute paths must never be baked into a distributed
        # asset. These markers are what a leaked build path looks like, and
        # they can only be checked against the documentation files: a compiled
        # binary can legitimately contain any byte sequence, including a string
        # that resembles a path, so scanning binaries here would produce
        # false alarms.
        if relative in DOCUMENT_FILES:
            for marker, label in (
                (b"/Users/", "macOS home directory"),
                (b"/home/runner/", "GitHub Actions home directory"),
                (b"C:\\Users\\", "Windows home directory"),
            ):
                if marker in data:
                    problems.append(
                        f"documentation file {relative!r} embeds a {label} path"
                    )
    return problems


def check_exec_bits(
    target: Target,
    version: str,
    members: Dict[str, int],
) -> List[str]:
    """Confirm executables carry the executable bit and documents do not."""
    if not target.carries_modes:
        # Windows archives carry no Unix mode bits; the executable bit is a
        # property of the destination filesystem, not of the zip.
        return []

    problems: List[str] = []
    expected_exec = set(target.expected_exec_members(version))
    for name, mode in sorted(members.items()):
        is_exec = bool(mode & 0o111)
        if name in expected_exec and not is_exec:
            problems.append(f"{name} is not marked executable (mode {mode:04o})")
        if name not in expected_exec and is_exec:
            problems.append(f"{name} is unexpectedly marked executable (mode {mode:04o})")
    return problems


def elf_machine(data: bytes) -> Optional[str]:
    """Identify an ELF binary and return its architecture, or None."""
    if len(data) < 20 or data[:4] != b"\x7fELF":
        return None
    little = data[5] == 1
    fmt = "<H" if little else ">H"
    machine = struct.unpack_from(fmt, data, 18)[0]
    return {
        ELF_MACHINE_X86_64: "x86_64",
        ELF_MACHINE_AARCH64: "aarch64",
    }.get(machine, f"unknown-elf-machine-0x{machine:04x}")


def macho_arch(data: bytes) -> Optional[str]:
    """Identify a Mach-O binary and return its architecture, or None."""
    if len(data) < 32:
        return None
    magic = struct.unpack_from(">I", data, 0)[0]
    if magic not in (0xFEEDFACF, 0xFEEDFACE):
        return None
    cpu_type = struct.unpack_from("<I", data, 4)[0]
    # 32-bit binaries (0xFEEDFACE) are not a supported release target; the
    # caller sees None and treats the file as unrecognized.
    if magic == 0xFEEDFACE:
        return None
    return {
        MACHO_CPU_X86_64: "x86_64",
        MACHO_CPU_ARM64: "aarch64",
    }.get(cpu_type, f"unknown-macho-cpu-0x{cpu_type:08x}")


def macho_filetype(data: bytes) -> Optional[int]:
    """Return the Mach-O filetype, used to confirm this is an executable."""
    if len(data) < 16 or struct.unpack_from(">I", data, 0)[0] != 0xFEEDFACF:
        return None
    return struct.unpack_from("<I", data, 12)[0]


def pe_machine(data: bytes) -> Optional[str]:
    """Identify a PE binary and return its architecture, or None."""
    if len(data) < 0x40 or data[:2] != b"MZ":
        return None
    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if pe_offset + 6 > len(data) or data[pe_offset : pe_offset + 4] != b"PE\0\0":
        return None
    machine = struct.unpack_from("<H", data, pe_offset + 4)[0]
    return {
        PE_MACHINE_X86_64: "x86_64",
        PE_MACHINE_ARM64: "aarch64",
    }.get(machine, f"unknown-pe-machine-0x{machine:04x}")


def check_architecture(
    target: Target,
    version: str,
    members: Dict[str, bytes],
) -> List[str]:
    """Confirm every binary really is built for the architecture it is named for.

    This is the check that catches a mislabeled matrix entry -- an aarch64
    binary shipped under an x86_64 name would otherwise pass every other gate.
    """
    problems: List[str] = []
    root = target.root_dir(version)

    for name in tuple(target.binaries) + tuple(target.required_helpers):
        relative = posixpath.join(root, name)
        data = members.get(relative)
        if data is None:
            # Already reported as a missing member.
            continue

        if target.platform == "linux":
            detected = elf_machine(data)
        elif target.platform == "macos":
            detected = macho_arch(data)
            if detected is None:
                problems.append(f"{name} is not a 64-bit Mach-O executable")
                continue
            filetype = macho_filetype(data)
            if filetype != MACHO_FILETYPE_EXEC:
                problems.append(
                    f"{name} is Mach-O filetype {filetype}, expected an executable"
                )
        else:
            detected = pe_machine(data)

        if detected is None:
            problems.append(
                f"{name} is not a recognizable {target.platform} executable"
            )
        elif detected != target.arch:
            problems.append(
                f"{name} is built for {detected} but the archive declares {target.arch}"
            )
    return problems


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--archive", required=True)
    args = parser.parse_args()

    target = get(args.target)
    path = Path(args.archive)
    expected_name = target.archive_name(args.version)

    if not path.is_file():
        print(f"error: archive not found: {path}", file=sys.stderr)
        return 1
    if path.name != expected_name:
        print(
            f"error: archive name {path.name!r} does not match the convention "
            f"{expected_name!r}",
            file=sys.stderr,
        )
        return 1

    members = read_archive(path)
    problems: List[str] = []
    problems += check_membership(target, args.version, members)
    problems += check_denied_content(target, args.version, members)
    problems += check_exec_bits(
        target, args.version, {name: mode for name, mode, _ in members}
    )
    problems += check_architecture(
        target, args.version, {name: data for name, _, data in members}
    )

    digest = sha256_of(path)
    print(f"archive={path.name}")
    print(f"target={target.key}")
    print(f"bytes={path.stat().st_size}")
    print(f"sha256={digest}")
    print(f"members={len(members)}")
    for name, _, _ in sorted(members):
        print(f"  member={name}")

    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        print(f"verification=FAIL ({len(problems)} problems)", file=sys.stderr)
        return 1

    print("verification=PASS")
    if target.experimental:
        print("status=EXPERIMENTAL")
    return 0


if __name__ == "__main__":
    sys.exit(main())
