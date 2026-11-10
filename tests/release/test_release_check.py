"""The release gate rejects bad tags, versions, and commits before publishing.

Each test builds a disposable repository with a bare ``origin``; nothing is
pushed anywhere else and no release is created.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "release-check.py"
WORKFLOW = REPO / ".github" / "workflows" / "release.yml"


def git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=cwd, check=True, capture_output=True, text=True
    ).stdout.strip()


def write_versions(repo: Path, root: str, rust: str, python: str) -> None:
    (repo / "manager").mkdir(exist_ok=True)
    (repo / "worker").mkdir(exist_ok=True)
    (repo / "pyproject.toml").write_text(
        f'[project]\nname = "runpy-monorepo"\nversion = "{root}"\n\n[tool.ruff]\nversion = "x"\n'
    )
    (repo / "manager/Cargo.toml").write_text(
        f'[package]\nname = "runpy"\nversion = "{rust}"\n\n[dependencies]\n'
        'serde = { version = "1.0" }\n'
    )
    (repo / "worker/pyproject.toml").write_text(
        f'[project]\nname = "runpyrs"\nversion = "{python}"\n'
    )


def commit(repo: Path, message: str) -> str:
    git(repo, "add", "-A")
    git(repo, "commit", "--quiet", "--allow-empty", "-m", message)
    return git(repo, "rev-parse", "HEAD")


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    origin = tmp_path / "origin.git"
    work = tmp_path / "work"
    git(tmp_path, "init", "--quiet", "--bare", str(origin))
    git(tmp_path, "init", "--quiet", "--initial-branch", "main", str(work))
    git(work, "config", "user.email", "release@test.invalid")
    git(work, "config", "user.name", "release test")
    git(work, "remote", "add", "origin", str(origin))
    write_versions(work, "0.1.0", "0.1.0", "0.1.0")
    commit(work, "initial")
    commit(work, "second")
    git(work, "push", "--quiet", "origin", "main")
    git(work, "fetch", "--quiet", "origin")
    return work


def release_check(repo: Path, tag: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), tag], cwd=repo, capture_output=True, text=True
    )


@pytest.mark.parametrize(
    ("tag", "prerelease"),
    [("v0.1.0", "false"), ("v0.1.0-dev", "true"), ("v0.1.0-dev.3", "true")],
)
def test_well_formed_tags_on_main_pass(repo, tag, prerelease):
    git(repo, "tag", tag)
    result = release_check(repo, tag)
    assert result.returncode == 0, result.stderr
    assert f"prerelease={prerelease}" in result.stdout.splitlines()
    assert "version=0.1.0" in result.stdout.splitlines()


def test_tags_on_older_main_commits_pass(repo):
    git(repo, "tag", "v0.1.0", "HEAD~1")
    assert release_check(repo, "v0.1.0").returncode == 0


@pytest.mark.parametrize(
    "tag",
    [
        "v0.1",
        "0.1.0",
        "v0.1.0-rc.1",
        "v01.1.0",
        "v0.1.0-dev1",
        "v0.1.0-dev.01",
        "v0.1.0.1",
    ],
)
def test_malformed_tags_fail(repo, tag):
    git(repo, "tag", tag)
    result = release_check(repo, tag)
    assert result.returncode == 1
    assert "is not vX.Y.Z" in result.stderr


def test_tag_must_match_every_manifest(repo):
    git(repo, "tag", "v0.2.0")
    result = release_check(repo, "v0.2.0")
    assert result.returncode == 1
    for manifest in ("pyproject.toml", "manager/Cargo.toml", "worker/pyproject.toml"):
        assert manifest in result.stderr


@pytest.mark.parametrize("which", ["root", "rust", "python"])
def test_one_mismatched_manifest_fails(repo, which):
    versions = {"root": "0.1.0", "rust": "0.1.0", "python": "0.1.0", which: "0.1.1"}
    write_versions(repo, **versions)
    commit(repo, "bump one manifest")
    git(repo, "push", "--quiet", "origin", "main")
    git(repo, "fetch", "--quiet", "origin")
    git(repo, "tag", "v0.1.0")
    result = release_check(repo, "v0.1.0")
    assert result.returncode == 1
    assert "declares 0.1.1" in result.stderr


def test_prereleases_share_the_base_manifest_version(repo):
    write_versions(repo, "0.1.0-dev", "0.1.0-dev", "0.1.0-dev")
    commit(repo, "dev manifests")
    git(repo, "push", "--quiet", "origin", "main")
    git(repo, "fetch", "--quiet", "origin")
    git(repo, "tag", "v0.1.0-dev")
    assert release_check(repo, "v0.1.0-dev").returncode == 1


def test_commits_not_on_main_fail(repo):
    git(repo, "switch", "--quiet", "-c", "side")
    commit(repo, "side work")
    git(repo, "tag", "v0.1.0")
    result = release_check(repo, "v0.1.0")
    assert result.returncode == 1
    assert "not reachable from origin/main" in result.stderr


def test_missing_tag_or_main_fails(repo):
    assert "does not exist" in release_check(repo, "v0.1.0").stderr
    git(repo, "tag", "v0.1.0")
    git(repo, "update-ref", "-d", "refs/remotes/origin/main")
    assert "fetch the main branch" in release_check(repo, "v0.1.0").stderr


def job(name: str) -> str:
    """The text of one top-level job in the release workflow."""
    text = WORKFLOW.read_text()
    match = re.search(rf"^  {name}:\n(.*?)(?=^  \S|\Z)", text, re.MULTILINE | re.DOTALL)
    assert match, f"job {name} missing"
    return match.group(1)


def test_publication_requires_the_tag_check_and_the_quality_gate():
    # GitHub skips a job whose needs did not all succeed unless its `if`
    # overrides that, so publication depends on both gates passing.
    release = job("release")
    assert re.search(r"^    needs: \[verify, quality\]$", release, re.MULTILINE)
    assert not re.search(r"^    if:", release, re.MULTILINE)
    assert "uses: ./.github/workflows/quality.yml" in job("quality")
    assert "scripts/release-check.py" in job("verify")
    assert "contents: write" not in WORKFLOW.read_text().split("jobs:")[0]
