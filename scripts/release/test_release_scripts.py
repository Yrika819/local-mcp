#!/usr/bin/env python3
"""Self-tests for the release packaging and verification scripts.

These run on synthetic archives and tiny hand-built binaries, so they need no
Rust toolchain, no network, and no real release build. They are what lets the
packaging logic be proven correct on a laptop before GitHub Actions spends a
runner-minute on it, and they are the regression net for the two rules that
would silently corrupt a real release: the Linux helper must ship beside the
server, and a non-Linux archive must never carry it.

Usage:
    python3 scripts/release/test_release_scripts.py
"""

from __future__ import annotations

import io
import os
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from typing import List

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))

import package  # noqa: E402
import targets  # noqa: E402
import verify_package  # noqa: E402


def write_elf(path: Path, machine: int) -> None:
    """Write a minimal ELF header with the given e_machine value."""
    header = bytearray(64)
    header[0:4] = b"\x7fELF"
    header[4] = 2  # 64-bit
    header[5] = 1  # little endian
    header[6] = 1  # version
    struct.pack_into("<H", header, 16, 1)  # ET_REL-ish; type is irrelevant here
    struct.pack_into("<H", header, 18, machine)
    path.write_bytes(bytes(header))


def write_macho(path: Path, cpu_type: int) -> None:
    """Write a minimal 64-bit Mach-O header for the given cpu type."""
    header = bytearray(32)
    header[0:4] = b"\xcf\xfa\xed\xfe"
    struct.pack_into("<I", header, 4, cpu_type)
    struct.pack_into("<I", header, 8, 3)  # cpusubtype
    struct.pack_into("<I", header, 12, verify_package.MACHO_FILETYPE_EXEC)
    path.write_bytes(bytes(header))


def write_pe(path: Path, machine: int) -> None:
    """Write a minimal PE header with the given Machine value."""
    data = bytearray(0x100)
    data[0:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 0x80)
    data[0x80:0x84] = b"PE\0\0"
    struct.pack_into("<H", data, 0x84, machine)
    path.write_bytes(bytes(data))


def write_target_binary(target: targets.Target, name: str, path: Path) -> None:
    """Write a minimal executable header matching the target's real format.

    Using the target's own architecture (rather than a fixed one) is what lets
    the round-trip test assert that a correct archive verifies, instead of
    only that a wrong one fails.
    """
    if target.platform == "linux":
        machine = (
            verify_package.ELF_MACHINE_AARCH64
            if target.arch == "aarch64"
            else verify_package.ELF_MACHINE_X86_64
        )
        write_elf(path, machine)
    elif target.platform == "macos":
        cpu = (
            verify_package.MACHO_CPU_ARM64
            if target.arch == "aarch64"
            else verify_package.MACHO_CPU_X86_64
        )
        write_macho(path, cpu)
    else:
        machine = (
            verify_package.PE_MACHINE_ARM64
            if target.arch == "aarch64"
            else verify_package.PE_MACHINE_X86_64
        )
        write_pe(path, machine)


def stage_target(target: targets.Target, version: str, out: Path) -> None:
    """Create fake built binaries and the docs a target needs, then package."""
    stage = out / f"stage-{target.key}"
    stage.mkdir(parents=True, exist_ok=True)
    binary_args: List[str] = []
    for name in tuple(target.binaries) + tuple(target.required_helpers):
        binary_path = stage / name
        write_target_binary(target, name, binary_path)
        binary_args += ["--binary", f"{name}={binary_path}"]

    argv = [
        sys.executable,
        str(HERE / "package.py"),
        "--target",
        target.key,
        "--version",
        version,
        "--repo-root",
        str(REPO_ROOT),
        "--out-dir",
        str(out),
        *binary_args,
    ]
    result = subprocess.run(argv, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise AssertionError(f"package.py failed for {target.key}: {result.stderr}")


class TargetMatrixTest(unittest.TestCase):
    """The matrix itself is a specification, so assert it directly."""

    def test_every_target_has_a_distinct_archive_name(self) -> None:
        names = [t.archive_name("0.1.0") for t in targets.TARGETS.values()]
        self.assertEqual(len(names), len(set(names)), f"duplicate archive names: {names}")

    def test_archive_names_follow_the_convention(self) -> None:
        self.assertEqual(
            targets.get("linux-x86_64").archive_name("0.1.0"),
            "local-mcp-v0.1.0-linux-x86_64.tar.gz",
        )
        self.assertEqual(
            targets.get("macos-aarch64").archive_name("0.1.0"),
            "local-mcp-v0.1.0-macos-aarch64.tar.gz",
        )
        self.assertEqual(
            targets.get("windows-x86_64").archive_name("0.1.0"),
            "local-mcp-v0.1.0-windows-x86_64.zip",
        )

    def test_linux_ships_the_sandbox_helper(self) -> None:
        for key in ("linux-x86_64", "linux-aarch64"):
            self.assertIn("codex-linux-sandbox", targets.get(key).required_helpers)

    def test_non_linux_targets_never_ship_the_linux_helper(self) -> None:
        for key in ("macos-x86_64", "macos-aarch64", "windows-x86_64", "windows-aarch64"):
            target = targets.get(key)
            self.assertEqual(target.required_helpers, ())
            self.assertIn("codex-linux-sandbox", target.forbidden)

    def test_documented_experimental_targets_are_marked_in_the_matrix(self) -> None:
        # Windows has no process sandbox. Linux ARM64 has a native build but
        # no sandbox closure evidence on a host that can create namespaces.
        self.assertTrue(targets.get("windows-x86_64").experimental)
        self.assertTrue(targets.get("windows-aarch64").experimental)
        self.assertTrue(targets.get("linux-aarch64").experimental)
        for key in ("linux-x86_64", "macos-x86_64", "macos-aarch64"):
            self.assertFalse(targets.get(key).experimental)

    def test_matrix_covers_the_required_platforms(self) -> None:
        self.assertEqual(
            set(targets.TARGET_ORDER),
            {
                "linux-x86_64",
                "linux-aarch64",
                "macos-x86_64",
                "macos-aarch64",
                "windows-x86_64",
                "windows-aarch64",
            },
        )

    def test_project_and_pinned_upstream_notices_ship_in_every_archive(self) -> None:
        # MIT and Apache-2.0 require their license notices to travel with
        # copies. Preserve the pinned Codex NOTICE verbatim alongside the
        # repository's dependency summary.
        for name in (
            "LICENSE",
            "CODEX-LICENSE.txt",
            "CODEX-NOTICE.txt",
            "THIRD_PARTY_NOTICES.md",
        ):
            self.assertIn(name, targets.DOCUMENT_FILES)
        for target in targets.TARGETS.values():
            members = target.expected_members("0.1.0")
            for name in targets.DOCUMENT_FILES:
                self.assertIn(f"{target.root_dir('0.1.0')}/{name}", members, target.key)


class PackageTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.out = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def test_package_and_verify_round_trip_for_every_target(self) -> None:
        for key in targets.TARGET_ORDER:
            with self.subTest(target=key):
                target = targets.get(key)
                stage_target(target, "0.1.0", self.out)
                archive = self.out / target.archive_name("0.1.0")
                self.assertTrue(archive.is_file(), f"{key} produced no archive")

                result = subprocess.run(
                    [
                        sys.executable,
                        str(HERE / "verify_package.py"),
                        "--target",
                        key,
                        "--version",
                        "0.1.0",
                        "--archive",
                        str(archive),
                    ],
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(
                    result.returncode, 0, f"{key} failed verification: {result.stderr}"
                )
                self.assertIn("verification=PASS", result.stdout)

    def test_packaging_is_byte_for_byte_reproducible(self) -> None:
        # Determinism is what makes a published SHA256SUMS an integrity record
        # instead of a record of one machine's clock.
        target = targets.get("linux-x86_64")
        stage_target(target, "0.1.0", self.out)
        first = (self.out / target.archive_name("0.1.0")).read_bytes()

        second_dir = self.out / "again"
        second_dir.mkdir()
        stage = self.out / "stage-again" / "src"
        stage.mkdir(parents=True, exist_ok=True)
        args = []
        for name in target.required_helpers + target.binaries:
            copy = stage / name
            copy.write_bytes((self.out / f"stage-{target.key}" / name).read_bytes())
            args += ["--binary", f"{name}={copy}"]
        subprocess.run(
            [
                sys.executable,
                str(HERE / "package.py"),
                "--target",
                target.key,
                "--version",
                "0.1.0",
                "--repo-root",
                str(REPO_ROOT),
                "--out-dir",
                str(second_dir),
                *args,
            ],
            check=True,
            capture_output=True,
        )
        second = (second_dir / target.archive_name("0.1.0")).read_bytes()
        self.assertEqual(
            first, second, "repackaging the same inputs produced different bytes"
        )

    def test_linux_archive_without_the_helper_is_rejected(self) -> None:
        # The helper is resolved as a sibling executable, so an archive missing
        # it would install a local-mcp that cannot run any sandboxed command.
        target = targets.get("linux-x86_64")
        stage = self.out / "no-helper"
        stage.mkdir()
        binary = stage / "local-mcp"
        write_elf(binary, verify_package.ELF_MACHINE_X86_64)
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "package.py"),
                "--target",
                target.key,
                "--version",
                "0.1.0",
                "--repo-root",
                str(REPO_ROOT),
                "--out-dir",
                str(self.out),
                "--binary",
                f"local-mcp={binary}",
            ],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("codex-linux-sandbox", result.stderr)

    def test_linux_helper_on_macos_is_rejected(self) -> None:
        target = targets.get("macos-x86_64")
        stage = self.out / "macos-extra"
        stage.mkdir()
        binary = stage / "local-mcp"
        write_macho(binary, verify_package.MACHO_CPU_X86_64)
        helper = stage / "codex-linux-sandbox"
        write_macho(helper, verify_package.MACHO_CPU_X86_64)
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "package.py"),
                "--target",
                target.key,
                "--version",
                "0.1.0",
                "--repo-root",
                str(REPO_ROOT),
                "--out-dir",
                str(self.out),
                "--binary",
                f"local-mcp={binary}",
                "--binary",
                f"codex-linux-sandbox={helper}",
            ],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Linux-only", result.stderr)

    def test_missing_binary_is_rejected(self) -> None:
        target = targets.get("macos-aarch64")
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "package.py"),
                "--target",
                target.key,
                "--version",
                "0.1.0",
                "--repo-root",
                str(REPO_ROOT),
                "--out-dir",
                str(self.out),
            ],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("local-mcp", result.stderr)


class VerifyTest(unittest.TestCase):
    """Negative tests: each must construct a bad archive and see it rejected."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.out = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def _package(self, key: str) -> Path:
        target = targets.get(key)
        stage_target(target, "0.1.0", self.out)
        return self.out / target.archive_name("0.1.0")

    def _repack_tar(self, source: Path, destination: Path, transform) -> None:
        """Rewrite a tar.gz with `transform` applied to each member."""
        import gzip

        # Payload bytes are read eagerly: an ExFileObject is a lazy view into
        # the source archive and becomes unreadable once that archive closes.
        with tarfile.open(source, "r:gz") as archive:
            entries = []
            for info in archive.getmembers():
                handle = archive.extractfile(info)
                entries.append((info, handle.read() if handle else None))

        with destination.open("wb") as handle:
            with gzip.GzipFile(filename="", mode="wb", fileobj=handle, mtime=0) as gz:
                with tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as out:
                    for info, payload in entries:
                        new_info = transform(info)
                        if new_info is None:
                            continue
                        if payload is None:
                            out.addfile(new_info)
                        else:
                            out.addfile(new_info, io.BytesIO(payload))

    def _verify(self, archive: Path, key: str = "linux-x86_64"):
        return subprocess.run(
            [
                sys.executable,
                str(HERE / "verify_package.py"),
                "--target",
                key,
                "--version",
                "0.1.0",
                "--archive",
                str(archive),
            ],
            capture_output=True,
            text=True,
        )

    def test_wrong_architecture_is_rejected(self) -> None:
        # A mislabeled matrix entry is the failure this exists to catch.
        target = targets.get("linux-x86_64")
        stage = self.out / "wrong-arch"
        stage.mkdir(parents=True)
        binary = stage / "local-mcp"
        write_elf(binary, verify_package.ELF_MACHINE_AARCH64)
        helper = stage / "codex-linux-sandbox"
        write_elf(helper, verify_package.ELF_MACHINE_AARCH64)
        out_dir = self.out / "wrong-arch-out"
        subprocess.run(
            [
                sys.executable,
                str(HERE / "package.py"),
                "--target",
                target.key,
                "--version",
                "0.1.0",
                "--repo-root",
                str(REPO_ROOT),
                "--out-dir",
                str(out_dir),
                "--binary",
                f"local-mcp={binary}",
                "--binary",
                f"codex-linux-sandbox={helper}",
            ],
            check=True,
            capture_output=True,
        )
        result = self._verify(out_dir / target.archive_name("0.1.0"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("aarch64", result.stderr)

    def test_extra_member_is_rejected(self) -> None:
        archive = self._package("linux-x86_64")
        # The tampered archive keeps the canonical name so the verifier reaches
        # the content checks instead of stopping at the naming convention.
        tampered = self.out / targets.get("linux-x86_64").archive_name("0.1.0")
        os.replace(archive, tampered.with_suffix(".tar.gz.orig"))
        archive = tampered.with_suffix(".tar.gz.orig")

        def add_source_tree(info):
            if info.isdir():
                return info
            new = tarfile.TarInfo("local-mcp-v0.1.0-linux-x86_64/src/main.rs")
            new.size = 5
            new.mtime = package.FIXED_MTIME
            new.mode = 0o644
            return new

        self._repack_tar(archive, tampered, add_source_tree)
        result = self._verify(tampered)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected member", result.stderr)

    def test_missing_executable_bit_is_rejected(self) -> None:
        archive = self._package("linux-x86_64")
        tampered = self.out / targets.get("linux-x86_64").archive_name("0.1.0")
        os.replace(archive, tampered.with_suffix(".tar.gz.orig"))
        archive = tampered.with_suffix(".tar.gz.orig")
        root = "local-mcp-v0.1.0-linux-x86_64"

        def strip_exec(info):
            if info.isdir():
                return info
            if info.name == f"{root}/local-mcp":
                info.mode = 0o644
            return info

        self._repack_tar(archive, tampered, strip_exec)
        result = self._verify(tampered)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not marked executable", result.stderr)

    def test_misplaced_member_outside_root_is_rejected(self) -> None:
        archive = self._package("macos-x86_64")
        tampered = self.out / targets.get("macos-x86_64").archive_name("0.1.0")
        os.replace(archive, tampered.with_suffix(".tar.gz.orig"))
        archive = tampered.with_suffix(".tar.gz.orig")

        def flatten(info):
            if info.isdir():
                return info
            info.name = info.name.split("/")[-1]
            return info

        self._repack_tar(archive, tampered, flatten)
        result = self._verify(tampered, "macos-x86_64")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("single root directory", result.stderr)

    def test_zip_archive_is_verified(self) -> None:
        archive = self._package("windows-aarch64")
        self.assertTrue(zipfile.is_zipfile(archive))
        result = self._verify(archive, "windows-aarch64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("status=EXPERIMENTAL", result.stdout)

    def test_archive_name_must_follow_the_convention(self) -> None:
        archive = self._package("linux-x86_64")
        renamed = self.out / "local-mcp-linux-x86_64.tar.gz"
        os.replace(archive, renamed)
        result = self._verify(renamed)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match the convention", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
