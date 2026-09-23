# Installation and uv-managed workers

Runpy consists of the Rust `runpy` manager crate and the Python `runpyrs`
worker SDK. `uv` is a runtime dependency in development, staging, and
production: the manager invokes it for every worker script.

## Prerequisites

- Rust and Cargo
- [`uv`](https://docs.astral.sh/uv/) on `PATH`

The repository development environment requires `uv >= 0.11.2`. Runpy checks
that the configured executable responds successfully to `uv --version`.

## Install the Rust crate

The repository is a Cargo workspace; Cargo discovers the `runpy` crate in its
`manager` member:

```bash
cargo add --git https://github.com/dark1zinn/runpy runpy
```

Select a branch, tag, or revision when a deployment must pin the Rust source:

```bash
cargo add --git https://github.com/dark1zinn/runpy runpy --branch main
cargo add --git https://github.com/dark1zinn/runpy runpy --tag v0.1.0-dev.1
cargo add --git https://github.com/dark1zinn/runpy runpy --rev <commit>
```

## Create a managed worker script

Runpy uses uv's PEP 723 single-file script environments instead of a
caller-created project `.venv`:

```bash
mkdir -p worker
uv init --script worker/my_worker.py --python 3.10
uv add --script worker/my_worker.py \
  "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker"
```

The resulting worker declares its complete runtime:

```python
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker",
# ]
# ///

from runpyrs import RunScript, Worker


class MyWorker(Worker):
    def execute(self, data: dict) -> dict:
        return {"result": data}


if __name__ == "__main__":
    RunScript(MyWorker)
```

Each script may declare a different `requires-python` value and dependency
set. At launch, uv finds or downloads a compatible Python and creates an
isolated cached environment for that script. Inline script metadata takes
precedence over surrounding projects, and Runpy also passes `--no-project`;
ambient `pyproject.toml` dependencies and `.venv` directories are not used.

The script itself must declare `runpyrs`. Runpy does not inject the SDK with
`--with`, create a `.venv`, or run `uv sync`.

## Lock script dependencies

PEP 723 script locks are explicit and adjacent to the script:

```bash
uv lock --script worker/my_worker.py
```

This creates `worker/my_worker.py.lock`. Commit it for reproducible
deployments. Runpy does not require or modify this file; `uv run --script`
reuses it when present and resolves from inline metadata when absent.

To exclude packages published after a chosen deployment cutoff, add uv's
RFC 3339 `exclude-newer` setting to the inline metadata:

```python
# /// script
# requires-python = ">=3.10"
# dependencies = ["requests"]
# [tool.uv]
# exclude-newer = "2025-01-01T00:00:00Z"
# ///
```

## Configure the manager

`Manager::new` resolves `uv` from `PATH`:

```rust
let manager = runpy::Manager::new("path/to/worker");
```

When production packages uv at a fixed location, select it explicitly:

```rust
let manager =
    runpy::Manager::with_uv_path("path/to/worker", "/opt/runpy/bin/uv");
```

Runpy performs `uv --version` during integrity checks and again before a
worker starts. The manager then owns the `uv run --no-project --script`
process group, including the Python descendant, so watchdog reporting and
termination remain under Rust control.

The first uncached dependency or Python resolution can require network
access. Production images may pre-populate uv's cache and should commit
script locks when repeatable versions are required.
