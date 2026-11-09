"""Repository Markdown: local links resolve and Python examples compile."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
LINK = re.compile(r"(?<!!)\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
FENCE = re.compile(r"^```(\w*)\n(.*?)^```", re.MULTILINE | re.DOTALL)


def markdown_files() -> list[Path]:
    tracked = subprocess.run(
        ["git", "ls-files", "*.md"],
        cwd=REPO,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.split()
    return [REPO / path for path in tracked if not path.startswith(".github/")]


def without_code(text: str) -> str:
    return FENCE.sub("", text)


def slug(heading: str) -> str:
    """GitHub's heading anchor: lowercase, punctuation dropped, spaces to '-'."""
    text = re.sub(r"`|\*|_(?=\w)|(?<=\w)_", "", heading.strip().lower())
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", "-")


def anchors(path: Path) -> set[str]:
    seen: dict[str, int] = {}
    result = set()
    for line in without_code(path.read_text()).splitlines():
        match = re.match(r"^#{1,6}\s+(.*?)\s*#*\s*$", line)
        if not match:
            continue
        base = slug(match.group(1))
        count = seen.get(base, 0)
        seen[base] = count + 1
        result.add(base if count == 0 else f"{base}-{count}")
    return result


@pytest.mark.parametrize(
    "path", markdown_files(), ids=lambda path: str(path.relative_to(REPO))
)
def test_local_links_resolve(path: Path):
    broken = []
    for target in LINK.findall(without_code(path.read_text())):
        if re.match(r"^[a-z][a-z0-9+.-]*:", target):
            continue  # http(s), mailto, and other schemes
        file_part, _, anchor = target.partition("#")
        destination = (path.parent / file_part).resolve() if file_part else path
        if not destination.exists():
            broken.append(f"{target}: missing {destination.relative_to(REPO)}")
        elif (
            anchor
            and destination.suffix == ".md"
            and anchor not in anchors(destination)
        ):
            broken.append(f"{target}: no heading #{anchor}")
    assert not broken, "\n".join(broken)


@pytest.mark.parametrize(
    "path", markdown_files(), ids=lambda path: str(path.relative_to(REPO))
)
def test_python_examples_compile(path: Path):
    for language, code in FENCE.findall(path.read_text()):
        if language == "python":
            compile(code, f"{path.relative_to(REPO)} example", "exec")
