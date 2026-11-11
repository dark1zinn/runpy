"""Validate audit suppressions and print pip-audit ignore arguments.

Every suppression in deny.toml ([advisories] ignore) and
audit-suppressions.toml (ignore) must be a table with an advisory `id` and a
`reason` ending in "(expires YYYY-MM-DD)" that has not passed. Bare IDs,
missing reasons, and expired entries fail with exit status 1.

Usage: python3 scripts/check-suppressions.py [--pip-audit-args]
"""

from __future__ import annotations

import datetime
import re
import sys
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parents[1]
EXPIRY = re.compile(r"\(expires (\d{4}-\d{2}-\d{2})\)$")


def entries(path: Path, *keys: str) -> list[object]:
    document: object = tomllib.loads(path.read_text())
    for key in keys:
        document = document.get(key, {}) if isinstance(document, dict) else {}
    return list(document) if isinstance(document, list) else []


def problems(source: str, items: list[object], today: datetime.date) -> list[str]:
    found = []
    for item in items:
        if not isinstance(item, dict) or not item.get("id"):
            found.append(f"{source}: {item!r} must be a table with an id")
            continue
        match = EXPIRY.search(str(item.get("reason", "")))
        if not match:
            found.append(
                f"{source}: {item['id']} needs a reason ending in (expires YYYY-MM-DD)"
            )
        elif datetime.date.fromisoformat(match.group(1)) < today:
            found.append(
                f"{source}: {item['id']} expired on {match.group(1)}; re-review it"
            )
    return found


def main() -> int:
    today = datetime.date.today()
    rust = entries(ROOT / "deny.toml", "advisories", "ignore")
    python = entries(ROOT / "audit-suppressions.toml", "ignore")
    found = problems("deny.toml", rust, today) + problems(
        "audit-suppressions.toml", python, today
    )
    for problem in found:
        print(problem, file=sys.stderr)
    if found:
        return 1
    if "--pip-audit-args" in sys.argv[1:]:
        print(
            " ".join(
                f"--ignore-vuln {item['id']}"
                for item in python
                if isinstance(item, dict)
            )
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
