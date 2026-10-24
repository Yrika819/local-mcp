#!/usr/bin/env python3
"""Self-tests for Install Contract V1.

These are the fast, network-free tests. They build synthetic archives with the
existing release packaging code, then exercise the install-layout, safe
extraction, checksum, version, and Nix-static contracts. Native execution is
covered separately by `install_contract.py native` on the matching CI runner and
by `install_contract.py published` against a real release artifact.

Usage:
    python3 scripts/release/test_install_contract.py
"""

from __future__ import annotations

import gzip
import io
import os
import re
import shutil
import tarfile
import tempfile
import unittest
from unittest import mock
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
sys_path = str(HERE)
import sys

if sys_path not in sys.path:
    sys.path.insert(0, sys_path)

import install_contract  # noqa: E402
import targets  # noqa: E402
import test_release_scripts  # noqa: E402


class ContractMatrixTest(unittest.TestCase):
    def test_install_contract_matches_the_packaging_matrix(self) -> None:
        self.assertEqual(install_contract.validate_contract_matrix(), [])

    def test_each_platform_requires_exactly_its_executables(self) -> None:
        expected = {
            "linux-x86_64": ("local-mcp", "codex-linux-sandbox", "atomic-publish"),
            "linux-aarch64": ("local-mcp", "codex-linux-sandbox", "atomic-publish"),
            "macos-x86_64": ("local-mcp", "atomic-publish"),
            "macos-aarch64": ("local-mcp", "atomic-publish"),
            "windows-x86_64": ("local-mcp.exe", "atomic-publish.exe"),
            "windows-aarch64": ("local-mcp.exe", "atomic-publish.exe"),
        }
        for key, executables in expected.items():
            with self.subTest(target=key):
                self.assertEqual(
                    install_contract.contract_for(key).required_executables, executables
                )

    def test_only_linux_contracts_carry_the_sandbox_helper(self) -> None:
        for key in ("linux-x86_64", "linux-aarch64"):
            self.assertIn(
                "codex-linux-sandbox",
                install_contract.contract_for(key).required_executables,
            )
        for key in (
            "macos-x86_64",
            "macos-aarch64",
            "windows-x86_64",
            "windows-aarch64",
        ):
            self.assertIn(
                "codex-linux-sandbox",
                install_contract.contract_for(key).forbidden_executables,
            )

    def test_helper_resolution_is_sibling_only(self) -> None:
        for contract in install_contract.CONTRACTS.values():
            self.assertEqual(contract.helper_resolution, "current_exe-sibling")

    def test_versioned_root_and_document_set(self) -> None:
        for key in targets.TARGET_ORDER:
            target = targets.get(key)
            root = target.root_dir("0.1.1")
            self.assertTrue(root.startswith("local-mcp-v0.1.1-"), key)
            members = target.expected_members("0.1.1")
            for name in targets.DOCUMENT_FILES:
                self.assertIn(f"{root}/{name}", members, key)


class ExtractionAndLayoutTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.work = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def _package(self, key: str) -> Path:
        target = targets.get(key)
        test_release_scripts.stage_target(target, "0.1.1", self.work)
        return self.work / target.archive_name("0.1.1")

    def _extract(self, key: str):
        target = targets.get(key)
        archive = self._package(key)
        root = install_contract.safe_extract(
            archive, self.work / f"extract-{key}-{os.urandom(4).hex()}"
        )
        return target, root

    def test_every_target_extracts_to_the_contract_layout(self) -> None:
        for key in targets.TARGET_ORDER:
            with self.subTest(target=key):
                target, root = self._extract(key)
                self.assertEqual(
                    install_contract.check_install_layout(target, "0.1.1", root), []
                )

    def test_missing_atomic_publish_is_rejected(self) -> None:
        target, root = self._extract("linux-x86_64")
        (root / "atomic-publish").unlink()
        problems = install_contract.check_install_layout(target, "0.1.1", root)
        self.assertTrue(any("atomic-publish" in problem for problem in problems), problems)

    def test_missing_linux_sandbox_helper_is_rejected(self) -> None:
        target, root = self._extract("linux-x86_64")
        (root / "codex-linux-sandbox").unlink()
        problems = install_contract.check_install_layout(target, "0.1.1", root)
        self.assertTrue(
            any("codex-linux-sandbox" in problem for problem in problems), problems
        )

    def test_linux_helper_on_macos_is_rejected(self) -> None:
        target, root = self._extract("macos-x86_64")
        shutil.copy2(root / "atomic-publish", root / "codex-linux-sandbox")
        problems = install_contract.check_install_layout(target, "0.1.1", root)
        self.assertTrue(
            any("forbidden platform helper" in problem for problem in problems), problems
        )

    def test_helper_in_a_subdirectory_is_rejected(self) -> None:
        target, root = self._extract("linux-x86_64")
        nested = root / "helpers"
        nested.mkdir()
        shutil.move(str(root / "atomic-publish"), str(nested / "atomic-publish"))
        problems = install_contract.check_install_layout(target, "0.1.1", root)
        self.assertTrue(any("missing installed file" in problem for problem in problems), problems)
        self.assertTrue(any("unexpected installed file" in problem for problem in problems), problems)

    def test_wrong_versioned_root_is_rejected(self) -> None:
        target, root = self._extract("linux-x86_64")
        renamed = root.with_name("local-mcp-v9.9.9-linux-x86_64")
        root.rename(renamed)
        problems = install_contract.check_install_layout(target, "0.1.1", renamed)
        self.assertTrue(any("versioned directory" in problem for problem in problems), problems)

    @unittest.skipIf(os.name == "nt", "Unix executable bits are not meaningful on Windows")
    def test_non_executable_binary_is_rejected(self) -> None:
        target, root = self._extract("linux-x86_64")
        (root / "local-mcp").chmod(0o644)
        problems = install_contract.check_install_layout(target, "0.1.1", root)
        self.assertTrue(
            any("not executable" in problem for problem in problems), problems
        )

    def test_corrupt_archive_is_rejected(self) -> None:
        archive = self._package("linux-x86_64")
        corrupt = self.work / archive.name
        data = archive.read_bytes()
        corrupt.write_bytes(data[: len(data) // 2])
        with self.assertRaises((install_contract.InstallContractError, tarfile.TarError)):
            install_contract.safe_extract(corrupt, self.work / "corrupt-out")

    def test_member_count_limit_is_explicit(self) -> None:
        bounds = install_contract.verify_package.ArchiveBounds(max_members=1)
        bounds.check_member("one", 0)
        with self.assertRaisesRegex(
            install_contract.verify_package.VerificationError, "more than 1 members"
        ):
            bounds.check_member("two", 0)

    def test_expansion_ratio_limit_is_explicit(self) -> None:
        bounds = install_contract.verify_package.ArchiveBounds(max_compression_ratio=10)
        with self.assertRaisesRegex(
            install_contract.verify_package.VerificationError, "above the 10 limit"
        ):
            bounds.check_compression_ratio(10, 101)

    def test_archive_is_structural_only_unless_execute_is_explicit(self) -> None:
        archive = self._package("linux-x86_64")
        with mock.patch.object(install_contract, "check_launch_modes") as launch:
            report, problems = install_contract.check_archive(
                "linux-x86_64", "0.1.1", archive
            )
        launch.assert_not_called()
        self.assertEqual(problems, [])
        self.assertEqual(
            report["runtime"],
            "not-run (structural verification only; --execute not requested)",
        )

    def test_archive_execution_requires_local_build_attestation(self) -> None:
        archive = self._package("linux-x86_64")
        with mock.patch.object(install_contract, "check_archive") as check_archive:
            result = install_contract.main(
                [
                    "archive",
                    "--target",
                    "linux-x86_64",
                    "--version",
                    "0.1.1",
                    "--archive",
                    str(archive),
                    "--execute",
                ]
            )
        self.assertEqual(result, 1)
        check_archive.assert_not_called()

    def _tar_with(self, name: str, members) -> Path:
        archive = self.work / name
        with archive.open("wb") as handle:
            with gzip.GzipFile(filename="", mode="wb", fileobj=handle, mtime=0) as gz:
                with tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as tar:
                    for info, data in members:
                        tar.addfile(info, io.BytesIO(data) if data is not None else None)
        return archive

    @staticmethod
    def _member(
        name: str,
        *,
        kind: bytes = tarfile.REGTYPE,
        data: bytes = b"fixture",
        linkname: str = "",
    ):
        info = tarfile.TarInfo(name)
        info.type = kind
        info.mode = 0o644
        info.linkname = linkname
        info.size = len(data) if kind == tarfile.REGTYPE else 0
        return info, data if kind == tarfile.REGTYPE else None

    def test_symlink_member_is_rejected(self) -> None:
        archive = self._tar_with(
            "with-link.tar.gz",
            [
                self._member(
                    "local-mcp-v0.1.1-linux-x86_64/local-mcp",
                    kind=tarfile.SYMTYPE,
                    linkname="/bin/true",
                )
            ],
        )
        with self.assertRaises(install_contract.InstallContractError):
            install_contract.safe_extract(archive, self.work / "link-out")

    def test_hard_link_member_is_rejected(self) -> None:
        archive = self._tar_with(
            "with-hardlink.tar.gz",
            [
                self._member(
                    "local-mcp-v0.1.1-linux-x86_64/local-mcp",
                    kind=tarfile.LNKTYPE,
                    linkname="local-mcp-v0.1.1-linux-x86_64/other",
                )
            ],
        )
        with self.assertRaises(install_contract.InstallContractError):
            install_contract.safe_extract(archive, self.work / "hardlink-out")

    def test_traversal_member_is_rejected(self) -> None:
        archive = self._tar_with(
            "with-traversal.tar.gz",
            [self._member("local-mcp-v0.1.1-linux-x86_64/../escape")],
        )
        with self.assertRaises(install_contract.InstallContractError):
            install_contract.safe_extract(archive, self.work / "traversal-out")

    def test_absolute_member_is_rejected(self) -> None:
        archive = self._tar_with(
            "with-absolute.tar.gz", [self._member("/tmp/escape")]
        )
        with self.assertRaises(install_contract.InstallContractError):
            install_contract.safe_extract(archive, self.work / "absolute-out")

    def test_drive_prefixed_member_is_rejected(self) -> None:
        archive = self._tar_with(
            "with-drive.tar.gz", [self._member("C:/escape")]
        )
        with self.assertRaises(install_contract.InstallContractError):
            install_contract.safe_extract(archive, self.work / "drive-out")

    def test_archive_name_version_mismatch_is_rejected(self) -> None:
        target = targets.get("linux-x86_64")
        archive = self._package("linux-x86_64")
        wrong = self.work / "local-mcp-v9.9.9-linux-x86_64.tar.gz"
        shutil.copy2(archive, wrong)
        report, problems = install_contract.check_archive(
            target.key, "0.1.1", wrong, execute=False
        )
        self.assertTrue(any("does not match" in problem for problem in problems), problems)
        self.assertEqual(report["formal"], "FAIL")


class ChecksumAndVersionTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.work = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def test_wrong_sha256_is_rejected(self) -> None:
        archive = self.work / "a.tar.gz"
        archive.write_bytes(b"not the published bytes")
        with self.assertRaises(install_contract.InstallContractError):
            install_contract.verify_archive_checksum(archive, "0" * 64)

    def test_checksums_parser_rejects_malformed_entries(self) -> None:
        checksums = self.work / "SHA256SUMS"
        checksums.write_text("not-a-checksum  file.tar.gz\n", encoding="utf-8")
        with self.assertRaises(install_contract.InstallContractError):
            install_contract.read_checksums(checksums)

    def test_published_hash_mismatch_never_calls_archive_processing(self) -> None:
        archive = self.work / targets.get("windows-x86_64").archive_name("0.1.1")
        archive.write_bytes(b"untrusted executable archive bytes")
        def download(url: str, destination: Path) -> None:
            destination.write_text(
                f"{'0' * 64}  {archive.name}\n", encoding="utf-8"
            )

        with mock.patch.object(install_contract, "_download", side_effect=download), mock.patch.object(
            install_contract, "check_archive"
        ) as check_archive:
            report, problems = install_contract.check_published(
                "windows-x86_64", "0.1.1", execute=True, download_dir=self.work
            )
        check_archive.assert_not_called()
        self.assertTrue(any("sha256 mismatch" in item for item in problems), problems)
        self.assertEqual(report["formal"], "FAIL")
        self.assertIn("not extracted", report["runtime"])

    def test_missing_checksum_entry_never_calls_archive_processing(self) -> None:
        archive = self.work / targets.get("windows-x86_64").archive_name("0.1.1")
        archive.write_bytes(b"untrusted executable archive bytes")
        def download(url: str, destination: Path) -> None:
            destination.write_text(f"{'0' * 64}  other.zip\n", encoding="utf-8")

        with mock.patch.object(install_contract, "_download", side_effect=download), mock.patch.object(
            install_contract, "check_archive"
        ) as check_archive:
            report, problems = install_contract.check_published(
                "windows-x86_64", "0.1.1", execute=True, download_dir=self.work
            )
        check_archive.assert_not_called()
        self.assertTrue(any("no entry" in item for item in problems), problems)
        self.assertIn("not extracted", report["runtime"])

    def test_malformed_official_checksums_never_calls_archive_processing(self) -> None:
        archive = self.work / targets.get("windows-x86_64").archive_name("0.1.1")
        archive.write_bytes(b"untrusted executable archive bytes")

        def download(url: str, destination: Path) -> None:
            destination.write_text("malformed checksum manifest\n", encoding="utf-8")

        with mock.patch.object(install_contract, "_download", side_effect=download), mock.patch.object(
            install_contract, "check_archive"
        ) as check_archive:
            report, problems = install_contract.check_published(
                "windows-x86_64", "0.1.1", execute=True, download_dir=self.work
            )
        check_archive.assert_not_called()
        self.assertTrue(any("SHA256SUMS is unusable" in item for item in problems), problems)
        self.assertIn("not extracted", report["runtime"])

    def test_download_dir_checksum_is_never_trusted_as_official(self) -> None:
        archive = self.work / targets.get("windows-x86_64").archive_name("0.1.1")
        archive.write_bytes(b"caller-controlled archive")
        (self.work / install_contract.CHECKSUMS_NAME).write_text(
            f"{install_contract.sha256_of(archive)}  {archive.name}\n",
            encoding="utf-8",
        )

        def download(url: str, destination: Path) -> None:
            self.assertTrue(url.endswith("/SHA256SUMS"))
            destination.write_text(f"{'0' * 64}  {archive.name}\n", encoding="utf-8")

        with mock.patch.object(install_contract, "_download", side_effect=download), mock.patch.object(
            install_contract, "check_archive"
        ) as check_archive:
            report, problems = install_contract.check_published(
                "windows-x86_64", "0.1.1", execute=True, download_dir=self.work
            )
        check_archive.assert_not_called()
        self.assertTrue(any("sha256 mismatch" in item for item in problems), problems)
        self.assertIn("not extracted", report["runtime"])

    def test_unconfirmed_host_override_cannot_produce_native_target(self) -> None:
        with mock.patch.dict(
            os.environ,
            {"GOALLATCH_HOST_TARGET": "linux-x86_64"},
            clear=True,
        ), mock.patch.object(install_contract, "detected_host_key", return_value="windows-x86_64"), mock.patch.object(
            install_contract, "runner_evidence_key", return_value=None
        ):
            evidence = install_contract.host_target_evidence()
        self.assertIsNone(evidence.target)
        self.assertEqual(evidence.evidence, "override-unconfirmed")

    @unittest.skipIf(os.name == "nt", "uses a POSIX shell fixture")
    def test_runtime_version_mismatch_is_detected(self) -> None:
        executable = self.work / "local-mcp"
        executable.write_text("#!/bin/sh\nprintf 'local-mcp 9.9.9\\n'\n", encoding="utf-8")
        executable.chmod(0o755)
        ok, detail = install_contract.run_version(executable, "0.1.1")
        self.assertFalse(ok)
        self.assertIn("9.9.9", detail)
        ok, detail = install_contract.run_version(executable, "9.9.9")
        self.assertTrue(ok, detail)

    def test_host_target_detection_is_stable(self) -> None:
        host = install_contract.host_target_key()
        if host is not None:
            self.assertIn(host, targets.TARGET_ORDER)


class NixStaticContractTest(unittest.TestCase):
    def setUp(self) -> None:
        self.flake = (REPO_ROOT / "flake.nix").read_text(encoding="utf-8")
        self.cargo = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")

    def test_flake_version_matches_cargo_version(self) -> None:
        cargo_version = re.search(
            r'^\[package\].*?^version\s*=\s*"([^"]+)"',
            self.cargo,
            flags=re.MULTILINE | re.DOTALL,
        )
        flake_version = re.search(r'version\s*=\s*"([^"]+)";', self.flake)
        self.assertIsNotNone(cargo_version)
        self.assertIsNotNone(flake_version)
        self.assertEqual(flake_version.group(1), cargo_version.group(1))

    def test_flake_still_declares_the_expected_build_contract(self) -> None:
        self.assertIn("buildRustPackage", self.flake)
        self.assertIn("cargoHash", self.flake)
        self.assertIn("wrapProgram $out/bin/local-mcp", self.flake)
        for system in (
            "x86_64-linux",
            "aarch64-linux",
            "x86_64-darwin",
            "aarch64-darwin",
        ):
            self.assertIn(system, self.flake)


class DocumentationTest(unittest.TestCase):
    def test_install_contract_document_covers_the_required_topics(self) -> None:
        document = REPO_ROOT / "docs" / "INSTALL_CONTRACT_V1.md"
        self.assertTrue(document.is_file(), document)
        text = document.read_text(encoding="utf-8")
        for heading in (
            "Supported platforms",
            "Required executables",
            "Install layout",
            "Helper resolution",
            "Symlink constraints",
            "Platform verification status",
            "Negative tests",
            "Nix",
            "Doctor V1",
        ):
            self.assertIn(heading, text)


if __name__ == "__main__":
    unittest.main(verbosity=2)
