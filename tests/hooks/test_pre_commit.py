"""The pre-commit hook checks exactly the staged tree and never touches files.

Each test clones this repository's HEAD into a disposable directory, installs
the working-tree version of the hook, stages a scenario, and runs the hook.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
HOOK = REPO / ".githooks" / "pre-commit"
GOOD_PY = "VALUE = 1\n"
BAD_PY = "import os\n\nVALUE = 1\n"  # formatted, but an unused import: ruff F401
SDK_FILE = "worker/src/runpyrs/scratch.py"


def git(clone: Path, *args: str, check: bool = True) -> str:
    return subprocess.run(
        ["git", *args], cwd=clone, check=check, capture_output=True, text=True
    ).stdout


@pytest.fixture
def clone(tmp_path: Path) -> Path:
    target = tmp_path / "repo"
    subprocess.run(["git", "clone", "--quiet", str(REPO), str(target)], check=True)
    git(target, "config", "user.email", "hook@test.invalid")
    git(target, "config", "user.name", "hook test")
    shutil.copy2(HOOK, target / ".githooks" / "pre-commit")
    git(target, "add", ".githooks/pre-commit")
    git(
        target,
        "commit",
        "--quiet",
        "--no-verify",
        "--allow-empty",
        "-m",
        "install hook under test",
    )
    return target


def state(clone: Path) -> tuple[str, str, str, dict[str, bytes]]:
    """Index, worktree diff, status, and contents of every changed file."""
    status = git(clone, "status", "--porcelain=v1", "--untracked-files=all")
    files = {}
    for line in status.splitlines():
        path = clone / line[3:].split(" -> ")[-1]
        if path.is_file():
            files[str(path)] = path.read_bytes()
    return (
        git(clone, "diff", "--cached", "--binary"),
        git(clone, "diff", "--binary"),
        status,
        files,
    )


def run_hook(clone: Path) -> subprocess.CompletedProcess[str]:
    before = state(clone)
    env = {
        key: value for key, value in os.environ.items() if not key.startswith("GIT_")
    }
    result = subprocess.run(
        ["sh", ".githooks/pre-commit"],
        cwd=clone,
        capture_output=True,
        text=True,
        env=env,
        check=False,
    )
    assert state(clone) == before, "the hook changed the index or working tree"
    assert not (
        Path(git(clone, "rev-parse", "--absolute-git-dir").strip())
        / "runpy-pre-commit"
        / "tree"
    ).exists()
    return result


def stage(clone: Path, path: str, staged: str, worktree: str | None) -> None:
    file = clone / path
    file.write_text(staged)
    git(clone, "add", path)
    if worktree is not None:
        file.write_text(worktree)


def test_staged_bad_python_fails_even_when_the_worktree_is_fixed(clone):
    stage(clone, SDK_FILE, BAD_PY, GOOD_PY)
    result = run_hook(clone)
    assert result.returncode != 0
    assert "F401" in result.stdout + result.stderr


def test_staged_good_python_passes_despite_bad_unstaged_and_untracked_files(clone):
    stage(clone, SDK_FILE, GOOD_PY, BAD_PY)
    (clone / "worker/src/runpyrs/untracked.py").write_text(BAD_PY)
    assert run_hook(clone).returncode == 0


def test_deletions_and_renames_are_checked_against_the_index(clone):
    git(clone, "mv", "worker/src/runpyrs/utils.py", "worker/src/runpyrs/helpers.py")
    git(clone, "rm", "--quiet", "worker/tests/test_strict_json.py")
    assert run_hook(clone).returncode == 0


def test_staged_bad_rust_formatting_fails_even_when_the_worktree_is_fixed(clone):
    path = "manager/src/strict_json.rs"
    original = (clone / path).read_text()
    stage(clone, path, original + "fn  extra( ) {}\n", original + "fn extra() {}\n")
    result = run_hook(clone)
    assert result.returncode != 0
    assert "Checking Rust formatting" in result.stdout
    assert "Checking Rust lints" not in result.stdout


def test_unmerged_entries_are_rejected(clone):
    blob = git(
        clone, "rev-parse", f"HEAD:{SDK_FILE.replace('scratch', '__init__')}"
    ).strip()
    entries = "".join(
        f"100644 {blob} {stage_number}\tconflicted.py\n" for stage_number in (1, 2, 3)
    )
    subprocess.run(
        ["git", "update-index", "--index-info"],
        cwd=clone,
        input=entries,
        text=True,
        check=True,
    )
    result = run_hook(clone)
    assert result.returncode != 0
    assert "unmerged" in result.stderr


def test_unrelated_changes_skip_all_checks(clone):
    stage(clone, "docs/note.md", "notes\n", None)
    result = run_hook(clone)
    assert result.returncode == 0
    assert result.stdout == ""
