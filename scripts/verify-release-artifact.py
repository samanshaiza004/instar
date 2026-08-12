#!/usr/bin/env python3
"""Verify, install, and smoke-test one cargo-dist release archive.

This intentionally uses only the Python standard library so the same verifier
can run on the macOS, Linux, and Windows runners in the release matrix.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path


class StageError(RuntimeError):
    def __init__(self, stage: str, message: str) -> None:
        super().__init__(message)
        self.stage = stage


def fail(stage: str, message: str) -> "NoReturn":
    raise StageError(stage, message)


def checksum_entry(path: Path) -> tuple[str, str]:
    try:
        line = next(
            line.strip()
            for line in path.read_text(encoding="utf-8").splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        )
    except (OSError, StopIteration) as error:
        fail("package", f"could not read checksum file {path}: {error}")

    match = re.match(r"^([0-9a-fA-F]{64})\s+(?:\*?)(.+?)\s*$", line)
    if not match:
        fail("package", f"unrecognised SHA-256 entry in {path}: {line!r}")
    return match.group(1).lower(), Path(match.group(2)).name


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        fail("package", f"could not hash {path}: {error}")
    return digest.hexdigest()


def verify_checksum_file(archive: Path, checksum_file: Path) -> None:
    expected, filename = checksum_entry(checksum_file)
    if filename != archive.name:
        fail(
            "package",
            f"{checksum_file.name} names {filename!r}, expected {archive.name!r}",
        )
    actual = sha256(archive)
    if actual != expected:
        fail(
            "package",
            f"SHA-256 mismatch for {archive.name}: expected {expected}, got {actual}",
        )


def verify_unified_checksum(archive: Path, checksum_file: Path) -> None:
    try:
        lines = checksum_file.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        fail("package", f"could not read unified checksum file {checksum_file}: {error}")

    for raw_line in lines:
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        match = re.match(r"^([0-9a-fA-F]{64})\s+(?:\*?)(.+?)\s*$", line)
        if not match:
            fail("package", f"unrecognised SHA-256 entry in {checksum_file}: {line!r}")
        if Path(match.group(2)).name == archive.name:
            actual = sha256(archive)
            expected = match.group(1).lower()
            if actual != expected:
                fail(
                    "package",
                    f"unified SHA-256 mismatch for {archive.name}: expected {expected}, got {actual}",
                )
            return

    fail("package", f"{checksum_file.name} has no entry for {archive.name}")


def safe_member_path(root: Path, member_name: str) -> Path:
    destination = (root / member_name).resolve()
    if os.path.commonpath((str(root.resolve()), str(destination))) != str(root.resolve()):
        fail("install", f"archive member escapes install directory: {member_name!r}")
    return destination


def unpack(archive: Path, extract_dir: Path) -> None:
    try:
        extract_dir.mkdir(parents=True, exist_ok=True)
        if archive.name.endswith((".tar.xz", ".tar.gz", ".tar.zst")):
            with tarfile.open(archive, mode="r:*") as bundle:
                for member in bundle.getmembers():
                    safe_member_path(extract_dir, member.name)
                bundle.extractall(extract_dir, filter="data")
        elif archive.suffix == ".zip":
            with zipfile.ZipFile(archive) as bundle:
                for member in bundle.infolist():
                    safe_member_path(extract_dir, member.filename)
                bundle.extractall(extract_dir)
        else:
            fail("install", f"unsupported distribution archive: {archive.name}")
    except StageError:
        raise
    except (OSError, tarfile.TarError, zipfile.BadZipFile) as error:
        fail("install", f"could not unpack {archive}: {error}")


def install_binary(extract_dir: Path, install_dir: Path) -> Path:
    binary_name = "instar.exe" if sys.platform == "win32" else "instar"
    candidates = [path for path in extract_dir.rglob(binary_name) if path.is_file()]
    if len(candidates) != 1:
        fail(
            "install",
            f"expected exactly one {binary_name} in archive, found {len(candidates)}",
        )

    destination = install_dir / "bin" / binary_name
    try:
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(candidates[0], destination)
        if sys.platform != "win32":
            destination.chmod(destination.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    except OSError as error:
        fail("install", f"could not install {candidates[0]} as {destination}: {error}")
    return destination


def verify_archive_contains_only_host(extract_dir: Path) -> None:
    """Reject benchmark or otherwise unexpected executables in a release."""

    expected = {"instar", "instar.exe"}
    executables = sorted(
        path
        for path in extract_dir.rglob("*")
        if path.is_file()
        and (
            path.name in expected
            or path.suffix.lower() == ".exe"
            or (os.name != "nt" and os.access(path, os.X_OK))
        )
    )
    if len(executables) != 1 or executables[0].name not in expected:
        found = ", ".join(str(path.relative_to(extract_dir)) for path in executables)
        fail(
            "install",
            f"release archive must contain exactly one host executable (instar); found {found or 'none'}",
        )


def execute(binary: Path, args: list[str]) -> None:
    try:
        result = subprocess.run(
            [str(binary), *args],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        fail("execute", f"could not run {binary.name} {' '.join(args)}: {error}")

    if result.returncode != 0:
        output = (result.stdout + result.stderr).strip()
        fail(
            "execute",
            f"{binary.name} {' '.join(args)} exited {result.returncode}: {output[-2000:]}",
        )


def write_report(path: Path, target: str, archive: Path, binary: Path) -> Path:
    report = path / f"release-size-{target}.tsv"
    try:
        report.write_text(
            "target\tarchive\tarchive_bytes\tbinary\tbinary_bytes\n"
            f"{target}\t{archive.name}\t{archive.stat().st_size}\t"
            f"{binary.name}\t{binary.stat().st_size}\n",
            encoding="utf-8",
        )
    except OSError as error:
        fail("package", f"could not write size report {report}: {error}")
    return report


def verify_all_checksums(distribution_dir: Path) -> int:
    archives = sorted(
        path
        for path in distribution_dir.iterdir()
        if path.is_file() and path.name.startswith("instar-shell-") and path.name.endswith((".tar.xz", ".zip"))
    )
    if not archives:
        fail("package", f"no release archives found in {distribution_dir}")
    for archive in archives:
        checksum_file = archive.with_name(archive.name + ".sha256")
        if not checksum_file.is_file():
            fail("package", f"missing checksum file for {archive.name}")
        verify_checksum_file(archive, checksum_file)
        unified = distribution_dir / "sha256.sum"
        if unified.is_file():
            verify_unified_checksum(archive, unified)
        print(f"verified checksum target={archive.name} archive_bytes={archive.stat().st_size}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target")
    parser.add_argument("--all-checksums", action="store_true")
    parser.add_argument("--distribution-dir", type=Path, default=Path("target/distrib"))
    parser.add_argument("--install-dir", type=Path, default=Path("target/release-install"))
    args = parser.parse_args()

    try:
        if args.all_checksums:
            return verify_all_checksums(args.distribution_dir)
        if not args.target:
            parser.error("--target is required unless --all-checksums is used")
        archives = sorted(
            path
            for path in args.distribution_dir.glob(f"instar-shell-{args.target}.*")
            if path.is_file() and not path.name.endswith(".sha256")
        )
        if len(archives) != 1:
            fail(
                "package",
                f"expected one packaged archive for {args.target}, found {[p.name for p in archives]}",
            )
        archive = archives[0]
        checksum_file = archive.with_name(archive.name + ".sha256")
        if not checksum_file.is_file():
            fail("package", f"missing checksum file for {archive.name}: {checksum_file}")
        verify_checksum_file(archive, checksum_file)
        unified = args.distribution_dir / "sha256.sum"
        if unified.is_file():
            verify_unified_checksum(archive, unified)

        if args.install_dir.exists():
            shutil.rmtree(args.install_dir)
        unpack(archive, args.install_dir / "unpacked")
        verify_archive_contains_only_host(args.install_dir / "unpacked")
        binary = install_binary(args.install_dir / "unpacked", args.install_dir)
        execute(binary, ["--help"])
        execute(binary, ["run", "--help"])
        report = write_report(args.distribution_dir, args.target, archive, binary)
        github_output = os.environ.get("GITHUB_OUTPUT")
        if github_output:
            try:
                with open(github_output, "a", encoding="utf-8") as output:
                    output.write(f"archive={archive.resolve()}\n")
                    output.write(f"size_report={report.resolve()}\n")
            except OSError as error:
                fail("package", f"could not publish verifier outputs: {error}")

        print(f"verified target={args.target} archive={archive.name} archive_bytes={archive.stat().st_size} binary_bytes={binary.stat().st_size}")
        return 0
    except StageError as error:
        print(f"::error title={error.stage} stage failed::{error}", file=sys.stderr)
        print(f"{error.stage.upper()} FAILED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
