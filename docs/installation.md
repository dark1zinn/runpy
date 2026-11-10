# Installation and worker setup

Runpy has two Git-installable packages:

- `runpy`, the Rust manager crate;
- `runpyrs`, the Python worker SDK.

The Rust manager starts each worker through [`uv`](https://docs.astral.sh/uv/)
and communicates with it over a Unix domain socket. Neither package is
published to crates.io or PyPI yet.

For architecture, lifecycle, protocol, and operations details, see the
[project overview](overview.md).

## Prerequisites

- A Unix-like operating system with Unix domain sockets.
- Rust and Cargo.
- `uv` available on `PATH`, or at a path supplied to
  `Manager::builder(...).uv_path(...)`.
- Python 3.10 or newer for the `runpyrs` SDK. `uv` can install the Python
  version declared by each worker.

Developing this repository requires uv 0.10 or newer (see
[Contributor setup](#contributor-setup)). At runtime, Runpy executes the
configured binary with `uv --version` and requires a successful exit status;
it does not parse or compare the reported version.

## Install the Rust crate

Add `runpy` directly from this repository:

```bash
cargo add runpy --git https://github.com/dark1zinn/runpy
```

Pin a branch, tag, or revision when the deployment must use an immutable
source:

```bash
cargo add runpy --git https://github.com/dark1zinn/runpy --branch main
cargo add runpy --git https://github.com/dark1zinn/runpy --tag v0.1.0-dev.1
cargo add runpy --git https://github.com/dark1zinn/runpy --rev <commit>
```

An application using Runpy's asynchronous APIs also needs a Tokio runtime.
Examples commonly use `serde_json` to build `Data` and `Meta` objects:

```bash
cargo add tokio --features full
cargo add serde_json
```

## Create a managed worker

Create a self-contained [PEP 723](https://peps.python.org/pep-0723/) script:

```bash
mkdir -p worker
uv init --script worker/my_worker.py --python 3.10
uv add --script worker/my_worker.py \
  "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker"
```

The worker file should declare all of its runtime dependencies:

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

The script metadata is authoritative. Runpy launches it with
`uv run --no-project --script`, so ambient `pyproject.toml` dependencies and
project virtual environments are not used. Runpy does not:

- create a `.venv`;
- invoke `uv sync`;
- inject `runpyrs`;
- create or update a lockfile.

Each worker script can declare a different Python version and dependency set.
`uv` selects or downloads a compatible interpreter and reuses its cached
environment.

## Lock worker dependencies

Create an optional lockfile adjacent to the script:

```bash
uv lock --script worker/my_worker.py
```

This creates `worker/my_worker.py.lock`. Commit it for reproducible
deployments. When the adjacent lock exists, Runpy includes `--locked` in the
launch command; a stale lock then fails the launch instead of being modified.
Without a lockfile, `uv` resolves the inline metadata normally.

To exclude packages uploaded after a deployment cutoff, add uv's RFC 3339
`exclude-newer` setting:

```python
# /// script
# requires-python = ">=3.10"
# dependencies = ["requests"]
# [tool.uv]
# exclude-newer = "2025-01-01T00:00:00Z"
# ///
```

## Configure the manager

`Manager::builder` takes the directory containing worker scripts and resolves
`uv` from `PATH` by default. `build` must be awaited inside a Tokio runtime:

```rust
use runpy::Manager;

#[tokio::main]
async fn main() -> Result<(), runpy::RunpyError> {
    let manager = Manager::builder("worker").build().await?;
    // The worker name is a file stem, without `.py`.
    let worker = manager.worker("my_worker").spawn().await?;
    println!("spawned {}", worker.id());
    worker.terminate().await?;
    Ok(())
}
```

Packaged deployments can provide an explicit executable with
`.uv_path("/opt/runpy/bin/uv")`. `build` fails with `UvUnavailable` when the
executable cannot run and with `ScriptsDirectory` when the scripts directory is
missing; `Manager::check_integrity` repeats these checks later.

The effective command is:

```text
uv run --no-project [--locked] --script <script.py> <socket-path> <worker-id> [--key=value ...]
```

Runpy forces `PYTHONUNBUFFERED=1`, captures both process streams, and owns the
worker process group until explicit termination or Manager drop. Continue with
the [project overview](overview.md) for message routing, output guarantees, and
shutdown behavior.

## Contributor setup

Two equivalent paths prepare a checkout for development.

**Nix and direnv.** `flake.nix` provides Rust 1.93.0 (rustfmt, Clippy,
rust-src), rust-analyzer, cargo-deny, cargo-edit, cargo-watch, Python 3.14,
uv, git, gh, and actionlint. With direnv, `.envrc` enters that shell, runs
`uv sync --frozen --all-packages --group dev --python 3.14`, and activates
`.venv`; it re-runs when `pyproject.toml`, `worker/pyproject.toml`, or
`uv.lock` change. `nix flake check` verifies the shell's tool versions on the
host system. Shells evaluate for x86_64/aarch64 Linux and macOS; CI executes
them on x86_64 Linux and Apple Silicon macOS only.

**uv only.** Install Rust 1.93.0 or newer and uv 0.10 or newer, then:

```bash
uv sync --all-packages --group dev
```

Python development tools (pytest, Ruff, mypy, Hypothesis, build, pip-audit)
live in the root `dev` dependency group of the uv workspace and are pinned by
`uv.lock`; uv itself is a bootstrap tool, not a project dependency.
