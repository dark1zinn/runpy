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
  `Manager::with_uv_path`.
- Python 3.10 or newer for the `runpyrs` SDK. `uv` can install the Python
  version declared by each worker.

The repository development environment requires `uv >= 0.11.2`. At runtime,
Runpy executes the configured binary with `uv --version` and requires a
successful exit status; it does not parse or compare the reported version.

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

`Manager::new` resolves `uv` from `PATH` and accepts the directory containing
worker scripts:

```rust
use runpy::Manager;

let manager = Manager::new("worker");
drop(manager);
```

Packaged deployments can provide an explicit executable:

```rust
use runpy::Manager;

let manager = Manager::with_uv_path("worker", "/opt/runpy/bin/uv");
drop(manager);
```

Runpy checks the configured executable and scripts directory during Manager
construction. Construction logs an integrity failure; `Manager::check_integrity`
and `Worker::spawn` return validation failures to the caller.

The worker name passed to `Manager::worker` is a file stem, without `.py`:

```rust
use runpy::Manager;

#[tokio::main]
async fn main() {
    let manager = Manager::new("worker");
    let mut worker = manager.worker("my_worker");
    let worker_id = worker.spawn().await.expect("worker should start");
    println!("spawned {worker_id}");
}
```

The effective command is:

```text
uv run --no-project [--locked] --script <script.py> <socket-path> <worker-id> [--key=value ...]
```

Runpy forces `PYTHONUNBUFFERED=1`, captures both process streams, and owns the
worker process group until explicit termination or Manager drop. Continue with
the [project overview](overview.md) for message routing, output guarantees, and
shutdown behavior.
