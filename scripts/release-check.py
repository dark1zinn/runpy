"""Decide whether a tag may become a source release.

Usage: python3 scripts/release-check.py <tag> [--main-ref REF]

Accepted tags:

- ``vX.Y.Z``: a stable release;
- ``vX.Y.Z-dev`` or ``vX.Y.Z-dev.N``: a prerelease of the same base version.

Releases are source-only: nothing is published to crates.io or PyPI. The
root ``pyproject.toml``, ``manager/Cargo.toml``, and ``worker/pyproject.toml``
must all declare exactly ``X.Y.Z``; prereleases deliberately share that base
version instead of encoding ``-dev`` in the manifests. The tag must point at
a commit reachable from the main branch (default ``origin/main``), though not
necessarily its tip.

On success, prints ``tag=...``, ``version=...``, and ``prerelease=true|false``
lines suitable for ``$GITHUB_OUTPUT``. Any violation exits with status 1.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

TAG = re.compile(r"^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-dev(\.(0|[1-9]\d*))?)?$")
MANIFESTS = {
    "pyproject.toml": "project",
    "manager/Cargo.toml": "package",
    "worker/pyproject.toml": "project",
}


class ReleaseError(Exception):
    pass


def manifest_version(path: Path, table: str) -> str:
    """The ``version`` key of ``[table]``, read without a TOML dependency."""
    section = None
    for line in path.read_text().splitlines():
        header = re.match(r"^\s*\[([^\]]+)\]\s*$", line)
        if header:
            section = header.group(1).strip()
            continue
        match = re.match(r'^\s*version\s*=\s*"([^"]*)"', line)
        if section == table and match:
            return match.group(1)
    raise ReleaseError(f"{path}: no version in [{table}]")


def git(root: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", *args], cwd=root, capture_output=True, text=True, check=False
    )


def check(root: Path, tag: str, main_ref: str) -> dict[str, str]:
    match = TAG.match(tag)
    if not match:
        raise ReleaseError(f"tag {tag!r} is not vX.Y.Z, vX.Y.Z-dev, or vX.Y.Z-dev.N")
    version = ".".join(match.group(i) for i in (1, 2, 3))

    mismatched = [
        f"{name} declares {found}"
        for name, table in MANIFESTS.items()
        if (found := manifest_version(root / name, table)) != version
    ]
    if mismatched:
        raise ReleaseError(
            f"tag {tag} needs version {version}: " + "; ".join(mismatched)
        )

    commit = git(
        root, "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}"
    )
    if commit.returncode != 0:
        raise ReleaseError(f"tag {tag} does not exist locally")
    if git(root, "rev-parse", "--verify", "--quiet", main_ref).returncode != 0:
        raise ReleaseError(f"{main_ref} is not available; fetch the main branch first")
    sha = commit.stdout.strip()
    if git(root, "merge-base", "--is-ancestor", sha, main_ref).returncode != 0:
        raise ReleaseError(f"tag {tag} ({sha[:12]}) is not reachable from {main_ref}")

    return {
        "tag": tag,
        "version": version,
        "prerelease": "true" if match.group(4) else "false",
    }


def main(argv: list[str]) -> int:
    if not argv or argv[0].startswith("-"):
        print(__doc__, file=sys.stderr)
        return 2
    main_ref = "origin/main"
    if "--main-ref" in argv:
        main_ref = argv[argv.index("--main-ref") + 1]
    root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").stdout.strip() or ".")
    try:
        result = check(root, argv[0], main_ref)
    except ReleaseError as error:
        print(f"release check failed: {error}", file=sys.stderr)
        return 1
    for key, value in result.items():
        print(f"{key}={value}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
