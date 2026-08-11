#!/usr/bin/env python3
"""Check that release.yml consumes the dist-workspace contract."""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path


def error(message: str) -> "NoReturn":
    print(f"::error title=configuration stage failed::{message}", file=sys.stderr)
    raise SystemExit(f"CONFIGURATION FAILED: {message}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--workflow", type=Path, default=Path(".github/workflows/release.yml"))
    parser.add_argument("--dist-config", type=Path, default=Path("dist-workspace.toml"))
    parser.add_argument("--plan", type=Path, required=True)
    args = parser.parse_args()

    try:
        config = tomllib.loads(args.dist_config.read_text(encoding="utf-8"))
        workflow = args.workflow.read_text(encoding="utf-8")
        plan = json.loads(args.plan.read_text(encoding="utf-8"))
    except (OSError, ValueError, tomllib.TOMLDecodeError) as exc:
        error(f"could not read release inputs: {exc}")

    dist = config.get("dist", {})
    targets = set(dist.get("targets", []))
    planned = plan.get("ci", {}).get("github", {}).get("artifacts_matrix", {}).get("include", [])
    matrix_targets = {target for item in planned for target in item.get("targets", [])}
    if not targets:
        error("dist-workspace.toml declares no release targets")
    if targets != matrix_targets:
        error(f"dist targets {sorted(targets)} do not match dist plan targets {sorted(matrix_targets)}")

    version = dist.get("cargo-dist-version")
    required_fragments = {
        "dynamic dist matrix": "fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix",
        "dist package command": "dist build",
        "dist version": f"cargo-dist/releases/download/v{version}/",
        "attestation action": "actions/attest@v4",
        "checksum verification": "verify-release-artifact.py",
        "size reporting": "release-size-",
    }
    for description, fragment in required_fragments.items():
        if fragment not in workflow:
            error(f"release.yml is missing {description} ({fragment!r})")

    if dist.get("pr-run-mode") != "upload":
        error("dist-workspace.toml must set pr-run-mode = \"upload\" for the release gate")
    if "ci" not in dist.get("allow-dirty", []):
        error("dist-workspace.toml must allow the wrapped CI workflow")
    if dist.get("checksum") != "sha256":
        error("dist-workspace.toml must set checksum = \"sha256\"")
    if dist.get("github-attestations") is not True:
        error("dist-workspace.toml must keep github-attestations = true")
    if dist.get("source-tarball") is not False:
        error("dist-workspace.toml must keep source-tarball = false for runtime-only distribution")

    artifact_names = set(plan.get("artifacts", {}))
    for target in targets:
        archive = next(
            (name for name in artifact_names if target in name and name.endswith((".tar.xz", ".zip"))),
            None,
        )
        checksum = f"{archive}.sha256" if archive else None
        if not archive or checksum not in artifact_names:
            error(f"dist plan has no archive/checksum pair for {target}")
        executables = [
            asset.get("name")
            for asset in plan["artifacts"][archive].get("assets", [])
            if asset.get("kind") == "executable"
        ]
        if executables != ["instar"]:
            error(f"dist plan for {target} contains unexpected executables: {executables}")

    print(f"release configuration is synchronized for {len(targets)} targets: {', '.join(sorted(targets))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
