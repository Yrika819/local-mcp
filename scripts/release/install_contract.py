#!/usr/bin/env python3
"""Install Contract V1 checks for GoalLatch release artifacts.

The release scripts already prove what is *inside* an archive: exact members,
executable bits, architecture headers, and the absence of private content. This
module covers the next boundary: what a user gets *after* extracting, and
whether that install can actually run on the host it was installed on.

It deliberately reuses `targets.py` for the matrix and `verify_package.py` for
archive-level checks. The only new rules here are install-layout rules, safe
extraction, native launch observations, and the helper-resolution contract.
Nothing in this file adds a PATH search or a fallback for a missing helper.

Commands:
    install_contract.py contract
    install_contract.py archive --target linux-x86_64 --version 0.1.1 \
        --archive dist/local-mcp-v0.1.1-linux-x86_64.tar.gz
    install_contract.py native --version 0.1.1 --binary-dir target/release
    install_contract.py published --target windows-x86_64 --version 0.1.1

`archive` and `published` execute only when the target matches the host. A
foreign-architecture binary is never run; the report says `not-run` instead.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import platform
import posixpath
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
import zipfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence, Tuple

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))

import verify_package  # noqa: E402
from targets import DOCUMENT_FILES, TARGET_ORDER, TARGETS, Target, get  # noqa: E402

CONTRACT_VERSION = 1
RELEASE_BASE_URL = "https://github.com/Yrika819/local-mcp/releases/download"
CHECKSUMS_NAME = "SHA256SUMS"

#: The Install Contract V1 matrix. `targets.py` remains the build/archive source
#: of truth; this table is the installed-runtime specification and the contract
#: test asserts the two agree.
CONTRACTS: Dict[str, "InstallContract"] = {}


class InstallContractError(Exception):
    """A refused install, archive, or runtime observation."""


@dataclass(frozen=True)
class InstallContract:
    key: str
    os_name: str
    arch: str
    required_executables: Tuple[str, ...]
    forbidden_executables: Tuple[str, ...]
    archive_suffix: str
    experimental: bool
    helper_resolution: str = "current_exe-sibling"
    carries_modes: bool = True

    @property
    def local_mcp(self) -> str:
        return "local-mcp.exe" if self.os_name == "windows" else "local-mcp"


def _contract(
    key: str,
    os_name: str,
    arch: str,
    required: Tuple[str, ...],
    forbidden: Tuple[str, ...],
    suffix: str,
    experimental: bool,
    carries_modes: bool = True,
) -> InstallContract:
    return InstallContract(
        key=key,
        os_name=os_name,
        arch=arch,
        required_executables=required,
        forbidden_executables=forbidden,
        archive_suffix=suffix,
        experimental=experimental,
        carries_modes=carries_modes,
    )


for _contract_entry in (
    _contract(
        "linux-x86_64",
        "linux",
        "x86_64",
        ("local-mcp", "codex-linux-sandbox", "atomic-publish"),
        (),
        "tar.gz",
        False,
    ),
    _contract(
        "linux-aarch64",
        "linux",
        "aarch64",
        ("local-mcp", "codex-linux-sandbox", "atomic-publish"),
        (),
        "tar.gz",
        True,
    ),
    _contract(
        "macos-x86_64",
        "macos",
        "x86_64",
        ("local-mcp", "atomic-publish"),
        ("codex-linux-sandbox",),
        "tar.gz",
        False,
    ),
    _contract(
        "macos-aarch64",
        "macos",
        "aarch64",
        ("local-mcp", "atomic-publish"),
        ("codex-linux-sandbox",),
        "tar.gz",
        False,
    ),
    _contract(
        "windows-x86_64",
        "windows",
        "x86_64",
        ("local-mcp.exe", "atomic-publish.exe"),
        ("codex-linux-sandbox", "codex-linux-sandbox.exe"),
        "zip",
        True,
        carries_modes=False,
    ),
    _contract(
        "windows-aarch64",
        "windows",
        "aarch64",
        ("local-mcp.exe", "atomic-publish.exe"),
        ("codex-linux-sandbox", "codex-linux-sandbox.exe"),
        "zip",
        True,
        carries_modes=False,
    ),
):
    CONTRACTS[_contract_entry.key] = _contract_entry


def contract_for(key: str) -> InstallContract:
    try:
        return CONTRACTS[key]
    except KeyError:
        known = ", ".join(TARGET_ORDER)
        raise InstallContractError(
            f"unknown install-contract target {key!r}; known targets: {known}"
        ) from None


def validate_contract_matrix() -> List[str]:
    """Prove the install contract and the packaging matrix describe one matrix."""
    problems: List[str] = []
    if set(CONTRACTS) != set(TARGET_ORDER):
        problems.append(
            f"install contract targets {sorted(CONTRACTS)} do not match packaging targets {sorted(TARGET_ORDER)}"
        )
    for key in TARGET_ORDER:
        target = TARGETS[key]
        contract = CONTRACTS[key]
        required = tuple(target.binaries) + tuple(target.required_helpers)
        if required != contract.required_executables:
            problems.append(
                f"{key}: packaging requires {required}, install contract requires {contract.required_executables}"
            )
        if tuple(target.forbidden) != contract.forbidden_executables:
            problems.append(
                f"{key}: packaging forbids {target.forbidden}, install contract forbids {contract.forbidden_executables}"
            )
        if target.archive_suffix != contract.archive_suffix:
            problems.append(
                f"{key}: packaging suffix {target.archive_suffix}, install contract suffix {contract.archive_suffix}"
            )
        if target.experimental != contract.experimental:
            problems.append(
                f"{key}: packaging experimental={target.experimental}, install contract experimental={contract.experimental}"
            )
        if target.carries_modes != contract.carries_modes:
            problems.append(
                f"{key}: packaging carries_modes={target.carries_modes}, install contract carries_modes={contract.carries_modes}"
            )
    return problems


def host_target_key() -> Optional[str]:
    """The matrix key for the current host, or None when it is unsupported.

    CI may set `GOALLATCH_HOST_TARGET` after it has already asserted the runner
    architecture. That keeps the native check meaningful on runners where the
    interpreter reports an emulated machine, without letting the override
    silently turn a foreign binary into a native pass.
    """
    override = os.environ.get("GOALLATCH_HOST_TARGET")
    if override:
        if override not in TARGETS:
            raise InstallContractError(
                f"GOALLATCH_HOST_TARGET={override!r} is not a release target"
            )
        return override
    system = sys.platform
    machine = platform.machine().lower()
    if machine in ("arm64", "aarch64"):
        arch = "aarch64"
    elif machine in ("x86_64", "amd64", "x64"):
        arch = "x86_64"
    else:
        return None
    if system.startswith("win"):
        name = "windows"
    elif system == "darwin":
        name = "macos"
    elif system.startswith("linux"):
        name = "linux"
    else:
        return None
    key = f"{name}-{arch}"
    return key if key in TARGETS else None


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def verify_archive_checksum(path: Path, expected: str) -> None:
    actual = sha256_of(path)
    if actual != expected:
        raise InstallContractError(
            f"sha256 mismatch for {path.name}: recorded {expected}, actual {actual}"
        )


def read_checksums(path: Path) -> Dict[str, str]:
    entries: Dict[str, str] = {}
    for line_number, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), 1
    ):
        if not line.strip():
            continue
        parts = line.split("  ", 1)
        if len(parts) != 2:
            raise InstallContractError(
                f"{path}: line {line_number} is not '<sha256>  <name>': {line!r}"
            )
        digest, name = parts
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise InstallContractError(
                f"{path}: line {line_number} has an invalid sha256: {digest!r}"
            )
        entries[name] = digest
    return entries


def _normalise_member(name: str) -> str:
    """Reject absolute, drive-relative, and traversal member names."""
    normalised = name.replace("\\", "/")
    if normalised.startswith("/") or normalised.startswith("//"):
        raise InstallContractError(f"archive member is absolute: {name!r}")
    if re.match(r"^[A-Za-z]:", normalised):
        raise InstallContractError(f"archive member has a drive prefix: {name!r}")
    parts = [part for part in normalised.split("/") if part not in ("", ".")]
    if any(part == ".." for part in parts):
        raise InstallContractError(f"archive member escapes the destination: {name!r}")
    return "/".join(parts)


def _write_member(destination: Path, member: str, data: bytes, mode: int) -> None:
    relative = _normalise_member(member)
    if not relative:
        return
    target = destination / Path(*relative.split("/"))
    target.parent.mkdir(parents=True, exist_ok=True)
    if target.exists() and not target.is_file():
        raise InstallContractError(f"archive member collides with a directory: {member!r}")
    target.write_bytes(data)
    if os.name != "nt":
        target.chmod(mode & 0o777)


def safe_extract(archive: Path, destination: Path) -> Path:
    """Extract an archive without trusting member names, links, or metadata.

    The destination must be a new/empty directory. The result is the single
    top-level directory the Install Contract requires. Symlinks, hard links,
    device nodes, absolute paths, and `..` are refused rather than filtered.
    """
    destination.mkdir(parents=True, exist_ok=True)
    if any(destination.iterdir()):
        raise InstallContractError(f"extraction destination is not empty: {destination}")

    name = archive.name
    try:
        if name.endswith(".tar.gz"):
            with tarfile.open(archive, "r:gz") as source:
                for info in source.getmembers():
                    if info.isdir():
                        continue
                    if info.issym() or info.islnk():
                        raise InstallContractError(
                            f"archive contains a link member, which the install contract refuses: {info.name}"
                        )
                    if not info.isfile():
                        raise InstallContractError(
                            f"archive contains a non-regular member: {info.name}"
                        )
                    handle = source.extractfile(info)
                    if handle is None:
                        raise InstallContractError(
                            f"could not read archive member: {info.name}"
                        )
                    _write_member(destination, info.name, handle.read(), info.mode)
        elif name.endswith(".zip"):
            with zipfile.ZipFile(archive, "r") as source:
                for info in source.infolist():
                    if info.is_dir():
                        continue
                    unix_mode = (info.external_attr >> 16) & 0o170000
                    if unix_mode == stat.S_IFLNK:
                        raise InstallContractError(
                            f"archive contains a symlink member, which the install contract refuses: {info.filename}"
                        )
                    _write_member(
                        destination,
                        info.filename,
                        source.read(info),
                        (info.external_attr >> 16) & 0o777,
                    )
        else:
            raise InstallContractError(f"unsupported archive format: {name}")
    except InstallContractError:
        raise
    except (tarfile.TarError, zipfile.BadZipFile, EOFError, OSError) as error:
        raise InstallContractError(f"archive is corrupt or unreadable: {error}") from error

    entries = list(destination.iterdir())
    directories = [entry for entry in entries if entry.is_dir()]
    files = [entry for entry in entries if entry.is_file()]
    if len(directories) != 1 or files:
        raise InstallContractError(
            "archive must extract to exactly one top-level directory and no top-level files; "
            f"found directories={[entry.name for entry in directories]} files={[entry.name for entry in files]}"
        )
    return directories[0]


def _is_executable(path: Path) -> bool:
    return bool(path.stat().st_mode & stat.S_IXUSR)


def check_install_layout(target: Target, version: str, root: Path) -> List[str]:
    """Check the installed tree after extraction, not the archive bytes."""
    contract = contract_for(target.key)
    problems: List[str] = []

    if root.name != target.root_dir(version):
        problems.append(
            f"installed root is {root.name!r}, expected the versioned directory {target.root_dir(version)!r}"
        )

    expected = sorted(
        list(DOCUMENT_FILES)
        + list(target.binaries)
        + list(target.required_helpers)
    )
    actual = sorted(
        path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_file()
    )
    for missing in sorted(set(expected) - set(actual)):
        problems.append(f"missing installed file: {missing}")
    for extra in sorted(set(actual) - set(expected)):
        problems.append(f"unexpected installed file: {extra}")

    for forbidden in contract.forbidden_executables:
        if (root / forbidden).exists():
            problems.append(
                f"forbidden platform helper is installed: {forbidden} ({target.key})"
            )

    for name in tuple(target.binaries) + tuple(target.required_helpers):
        path = root / name
        if not path.is_file():
            continue
        if path.stat().st_size == 0:
            problems.append(f"required executable is empty: {name}")
        if target.carries_modes and os.name != "nt" and not _is_executable(path):
            problems.append(f"required executable is not executable: {name}")

    for name in DOCUMENT_FILES:
        path = root / name
        if path.is_file() and target.carries_modes and os.name != "nt" and _is_executable(path):
            problems.append(f"documentation file is unexpectedly executable: {name}")

    # Helper resolution is sibling-only. A helper in a subdirectory is not an
    # install: the runtime would not look there.
    for name in target.required_helpers:
        path = root / name
        if path.is_file() and path.parent != root:
            problems.append(f"helper is not a sibling of local-mcp: {name}")

    return problems


def run_version(
    executable: Path,
    expected_version: str,
    *,
    env: Optional[Dict[str, str]] = None,
    cwd: Optional[Path] = None,
    timeout: int = 30,
) -> Tuple[bool, str]:
    """Run `--version` and compare the reported version, returning (ok, detail)."""
    try:
        result = subprocess.run(
            [str(executable), "--version"],
            capture_output=True,
            text=True,
            timeout=timeout,
            env=env,
            cwd=cwd,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        return False, f"could not execute {executable.name}: {error}"
    if result.returncode != 0:
        return (
            False,
            f"{executable.name} --version exited {result.returncode}: {result.stderr.strip()}",
        )
    match = re.search(r"\b(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)\b", result.stdout)
    if match is None:
        return False, f"{executable.name} --version did not report a version: {result.stdout!r}"
    reported = match.group(1)
    if reported != expected_version:
        return (
            False,
            f"{executable.name} reports version {reported}, expected {expected_version}",
        )
    return True, f"{executable.name} --version = {reported}"


def check_atomic_publish_startup(root: Path, target: Target) -> Tuple[bool, str]:
    """Prove the commit helper starts and rejects a malformed request.

    The helper reads one framed request from stdin. An empty frame is a
    deterministic refusal (exit 65), so this is a safe way to observe that the
    installed helper is executable without granting it a write.
    """
    name = "atomic-publish.exe" if target.platform == "windows" else "atomic-publish"
    executable = root / name
    if not executable.is_file():
        return False, f"missing helper: {name}"
    try:
        result = subprocess.run(
            [str(executable)],
            input=b"",
            capture_output=True,
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        return False, f"could not execute {name}: {error}"
    if result.returncode != 65:
        return (
            False,
            f"{name} accepted or crashed on a malformed request (exit {result.returncode})",
        )
    stderr = result.stderr.decode("utf-8", errors="replace")
    if "rejected its request" not in stderr:
        return False, f"{name} did not report a structured refusal: {stderr.strip()}"
    return True, f"{name} starts and rejects a malformed request (exit 65)"


def check_launch_modes(
    target: Target, version: str, root: Path, work: Path
) -> Dict[str, object]:
    """Observe direct, PATH, and symlink launches on the native host only."""
    host = host_target_key()
    report: Dict[str, object] = {
        "target": target.key,
        "host_target": host,
        "native": host == target.key,
        "modes": {},
    }
    modes: Dict[str, object] = report["modes"]  # type: ignore[assignment]
    if host != target.key:
        modes["direct"] = "not-run (non-native target)"
        modes["path"] = "not-run (non-native target)"
        modes["symlink"] = "not-run (non-native target)"
        return report

    executable = root / contract_for(target.key).local_mcp
    ok, detail = run_version(executable, version)
    modes["direct"] = detail
    if not ok:
        modes["_failed"] = True

    elsewhere = work / "elsewhere"
    elsewhere.mkdir(parents=True, exist_ok=True)
    # Resolve with PATH semantics explicitly. Windows' CreateProcess does not
    # reliably consult the child's PATH block, so invoking a bare name would
    # test the parent environment rather than the install's PATH entry.
    resolved = shutil.which(executable.name, path=str(root))
    if resolved is None:
        modes["path"] = f"{executable.name} was not found on a PATH containing {root}"
        modes["_failed"] = True
    else:
        ok, detail = run_version(Path(resolved), version, cwd=elsewhere)
        modes["path"] = detail
        if not ok:
            modes["_failed"] = True

    link_dir = work / "link-bin"
    link_dir.mkdir(parents=True, exist_ok=True)
    link = link_dir / executable.name
    try:
        if link.exists() or link.is_symlink():
            link.unlink()
        os.symlink(executable, link)
    except OSError as error:
        modes["symlink"] = f"not-run (cannot create symlink on this host: {error})"
    else:
        resolved = shutil.which(executable.name, path=str(link_dir))
        if resolved is None:
            modes["symlink"] = f"{executable.name} was not found through {link}"
            modes["_failed"] = True
        else:
            ok, detail = run_version(Path(resolved), version, cwd=elsewhere)
            modes["symlink"] = detail
            if not ok:
                modes["_failed"] = True

    ok, detail = check_atomic_publish_startup(root, target)
    modes["atomic_publish_startup"] = detail
    if not ok:
        modes["_failed"] = True
    return report


def _stage_native_install(
    target: Target, version: str, binary_dir: Path, root: Path
) -> None:
    root.mkdir(parents=True, exist_ok=True)
    for name in tuple(target.binaries) + tuple(target.required_helpers):
        source = binary_dir / name
        if not source.is_file():
            raise InstallContractError(f"built executable not found: {source}")
        shutil.copy2(source, root / name)
        if os.name != "nt":
            (root / name).chmod(0o755)
    for name in DOCUMENT_FILES:
        source = REPO_ROOT / name
        if not source.is_file():
            raise InstallContractError(f"missing documentation file: {source}")
        shutil.copy2(source, root / name)


def check_archive(
    target_key: str,
    version: str,
    archive: Path,
    *,
    execute: bool,
) -> Tuple[Dict[str, object], List[str]]:
    """Verify an archive and then the install it produces."""
    target = get(target_key)
    problems: List[str] = []
    report: Dict[str, object] = {
        "contract_version": CONTRACT_VERSION,
        "target": target_key,
        "version": version,
        "archive": archive.name,
        "sha256": sha256_of(archive) if archive.is_file() else None,
        "experimental": target.experimental,
        "formal": "pending",
        "runtime": "not-run",
    }

    if not archive.is_file():
        return report, [f"archive not found: {archive}"]
    if archive.name != target.archive_name(version):
        problems.append(
            f"archive name {archive.name!r} does not match {target.archive_name(version)!r}"
        )

    try:
        members = verify_package.read_archive(archive)
    except (InstallContractError, verify_package.VerificationError, tarfile.TarError, zipfile.BadZipFile, OSError) as error:
        return report, [f"archive could not be read: {error}"]

    problems += verify_package.check_membership(target, version, members)
    problems += verify_package.check_denied_content(target, version, members)
    problems += verify_package.check_exec_bits(
        target, version, {name: mode for name, mode, _ in members}
    )
    problems += verify_package.check_architecture(
        target, version, {name: data for name, _, data in members}
    )

    with tempfile.TemporaryDirectory(prefix="goallatch-install-contract-") as temporary:
        work = Path(temporary)
        try:
            root = safe_extract(archive, work / "extracted")
        except (InstallContractError, tarfile.TarError, zipfile.BadZipFile, OSError) as error:
            return report, problems + [f"archive extraction refused: {error}"]

        problems += check_install_layout(target, version, root)
        report["formal"] = "PASS" if not problems else "FAIL"
        report["install_root"] = root.name
        if execute and not problems:
            runtime = check_launch_modes(target, version, root, work / "launch")
            report["runtime"] = runtime
            if runtime["modes"].get("_failed"):  # type: ignore[union-attr]
                problems.append("native launch checks failed")
        elif not execute:
            report["runtime"] = "not-run (--structural-only)"
        else:
            report["runtime"] = "not-run (formal checks failed)"
    return report, problems


def _download(url: str, destination: Path) -> None:
    request = urllib.request.Request(url, headers={"User-Agent": "goallatch-install-contract"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            destination.write_bytes(response.read())
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise InstallContractError(f"download failed for {url}: {error}") from error


def check_published(
    target_key: str,
    version: str,
    *,
    execute: bool,
    download_dir: Optional[Path],
) -> Tuple[Dict[str, object], List[str]]:
    target = get(target_key)
    archive_name = target.archive_name(version)
    base = f"{RELEASE_BASE_URL}/v{version}"
    with tempfile.TemporaryDirectory(prefix="goallatch-published-contract-") as temporary:
        work = Path(download_dir) if download_dir else Path(temporary)
        work.mkdir(parents=True, exist_ok=True)
        checksums = work / CHECKSUMS_NAME
        archive = work / archive_name
        if not checksums.is_file():
            _download(f"{base}/{CHECKSUMS_NAME}", checksums)
        if not archive.is_file():
            _download(f"{base}/{archive_name}", archive)

        problems: List[str] = []
        try:
            entries = read_checksums(checksums)
            expected = entries.get(archive_name)
            if expected is None:
                problems.append(f"SHA256SUMS has no entry for {archive_name}")
            else:
                verify_archive_checksum(archive, expected)
        except InstallContractError as error:
            problems.append(str(error))

        report, archive_problems = check_archive(
            target_key, version, archive, execute=execute and not problems
        )
        problems += archive_problems
        report["source"] = f"{base}/{archive_name}"
        report["checksums_source"] = f"{base}/{CHECKSUMS_NAME}"
        return report, problems


def _print_report(report: Dict[str, object], problems: Sequence[str]) -> None:
    for key, value in report.items():
        if key == "modes" and isinstance(value, dict):
            for mode, detail in value.items():
                if mode.startswith("_"):
                    continue
                print(f"mode.{mode}={detail}")
        else:
            print(f"{key}={value}")
    for problem in problems:
        print(f"error: {problem}", file=sys.stderr)
    print(f"install_contract={'FAIL' if problems else 'PASS'}")


def _read_cargo_version() -> str:
    text = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(
        r'^\[package\].*?^version\s*=\s*"([^"]+)"',
        text,
        flags=re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise InstallContractError("could not read [package] version from Cargo.toml")
    return match.group(1)


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser("contract", help="check the Install Contract V1 matrix")

    archive_parser = subparsers.add_parser(
        "archive", help="verify an archive and the install it produces"
    )
    archive_parser.add_argument("--target", required=True)
    archive_parser.add_argument("--version", required=True)
    archive_parser.add_argument("--archive", required=True)
    archive_parser.add_argument(
        "--structural-only",
        action="store_true",
        help="do not execute anything, even on the native host",
    )

    native_parser = subparsers.add_parser(
        "native", help="stage built binaries and observe a native install"
    )
    native_parser.add_argument("--version", default=None)
    native_parser.add_argument("--binary-dir", default="target/release")

    published_parser = subparsers.add_parser(
        "published", help="verify a published GitHub Release artifact"
    )
    published_parser.add_argument("--target", required=True)
    published_parser.add_argument("--version", required=True)
    published_parser.add_argument(
        "--download-dir",
        default=None,
        help="keep downloads here instead of a temporary directory",
    )
    published_parser.add_argument(
        "--structural-only",
        action="store_true",
        help="do not execute anything, even on the native host",
    )

    args = parser.parse_args(argv)

    if args.command == "contract":
        problems = validate_contract_matrix()
        _print_report(
            {"contract_version": CONTRACT_VERSION, "targets": len(CONTRACTS)},
            problems,
        )
        return 1 if problems else 0

    if args.command == "archive":
        report, problems = check_archive(
            args.target,
            args.version,
            Path(args.archive),
            execute=not args.structural_only,
        )
        _print_report(report, problems)
        return 1 if problems else 0

    if args.command == "native":
        version = args.version or _read_cargo_version()
        host = host_target_key()
        if host is None:
            print(
                f"error: unsupported host {sys.platform}/{platform.machine()}; no install-contract target",
                file=sys.stderr,
            )
            return 1
        target = get(host)
        with tempfile.TemporaryDirectory(prefix="goallatch-native-contract-") as temporary:
            work = Path(temporary)
            root = work / target.root_dir(version)
            try:
                _stage_native_install(target, version, Path(args.binary_dir), root)
            except InstallContractError as error:
                _print_report({"target": host, "version": version}, [str(error)])
                return 1
            problems = check_install_layout(target, version, root)
            report: Dict[str, object] = {
                "contract_version": CONTRACT_VERSION,
                "target": host,
                "version": version,
                "experimental": target.experimental,
                "formal": "PASS" if not problems else "FAIL",
                "runtime": "not-run (formal checks failed)",
            }
            if not problems:
                runtime = check_launch_modes(target, version, root, work / "launch")
                report["runtime"] = runtime
                if runtime["modes"].get("_failed"):  # type: ignore[union-attr]
                    problems.append("native launch checks failed")
            _print_report(report, problems)
            return 1 if problems else 0

    if args.command == "published":
        report, problems = check_published(
            args.target,
            args.version,
            execute=not args.structural_only,
            download_dir=Path(args.download_dir) if args.download_dir else None,
        )
        _print_report(report, problems)
        return 1 if problems else 0

    parser.error(f"unknown command: {args.command}")
    return 2


if __name__ == "__main__":
    sys.exit(main())
