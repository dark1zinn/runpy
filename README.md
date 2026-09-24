![Runpy](docs/assets/runpy_logo.png)

> <p style="font-size: 12px;">This logo was generated with AI and heavily inspired on <a href="https://elysiajs.com">Elisya</a> logo</p>

---

A Rust crate for spawning, managing, and communicating with Python worker processes over Unix sockets.

Combine Rust's performance and robustness with Python's simplicity for writing scripts — data analysis, scraping, ML inference, whatever you need. Rust acts as the **control plane**; Python scripts are **workers**.

## Why?

Python is simple to write but limited in concurrency and reliability. Rust is fast and robust but overkill for throwaway scripts.

**Runpy** lets you write your business logic in Python, while Rust handles process orchestration, health monitoring, and structured communication. For example:

- Build a web server in Rust that spawns Python scrapers on demand
- Run data analysis pipelines where Rust manages scheduling and Python does the heavy lifting
- Offload ML inference to Python workers while Rust handles the API layer

## Features

- **Worker Management**: Spawn, monitor, and terminate Python workers
- **uv-managed Runtime**: Per-script Python versions and dependencies from PEP 723 metadata
- **Bare JSON Envelopes**: Developer-owned metadata and data with minimal Runpy routing
- **Watchdog Service**: Automatic health monitoring and dead worker cleanup
- **Structured Logging**: Environment-aware logging via \`Scribbler\`
- **Bidirectional Communication**: Send commands and receive responses
- **Extra Arguments**: Pass custom \`--key=value\` arguments to workers

## Architecture

```text
┌─────────────────────────────────────────────────┐
│  Rust (Manager)                                 │
│    ├─ IntegrityChecker   (uv & script checks)   │
│    ├─ Scribbler          (structured logging)   │
│    ├─ Workers[]          (builder + handle)     │
│    │    └─ ControlPlane  (Unix socket protocol) │
│    └─ WatchdogService    (health & resources)   │
│                                                 │
│         ┌─── Unix Socket (length-prefixed JSON) │
│         ▼                                       │
│  Python (Worker)                                │
│    └─ runpyrs/worker.py                         │
│         ├─ execute()         — managed execution │
│         └─ handle_envelope() — custom messaging  │
└─────────────────────────────────────────────────┘
```

## Quick Start

### Prerequisites

- Rust / Cargo
- [`uv`](https://docs.astral.sh/uv/) in development and production

### Setup

> Note that `Runpy` isn't available in crates.io yet, nor `runpyrs` Python package in PyPi!

```bash
# Create your project
mkdir myapp && cd myapp
cargo init

# Add '--branch dev' to get the latest commits
cargo add --git https://github.com/dark1zinn/runpy -p runpy
cargo add tokio serde_json

# Create a self-contained worker script
mkdir worker
uv init --script worker/my_script.py --python 3.10
uv add --script worker/my_script.py \
  "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker"

# Optional but recommended for reproducible deployments
uv lock --script worker/my_script.py
```

For a better understanding on how to add the crate/package to your project see [this instalation guide](docs/instalation.md)

You can take a look in the [examples folder](examples) for a suggested project folder structure

### Usage

```rust
use runpy::{Data, Envelope, Manager, Meta};
use serde_json::{json, Value};

fn object(value: Value) -> Data {
    value.as_object().cloned().expect("JSON object")
}

#[tokio::main]
async fn main() {
    let mut manager = Manager::new("path/to/scripts");

    manager.on_message(|inbound| {
        let operation = inbound
            .envelope
            .meta()
            .get("x_op")
            .and_then(Value::as_str);

        match operation {
            Some("ready") => inbound
                .mailer
                .send(Envelope::execute(object(json!({"url": "https://example.com"})))),
            Some("done") => println!("result: {}", Value::Object(inbound.envelope.data().clone())),
            Some("error") => eprintln!("worker error: {}", Value::Object(inbound.envelope.data().clone())),
            _ => println!("custom envelope: {:?}", inbound.envelope),
        }
    });

    let mut worker = manager.worker("my_script");
    worker.spawn().await.expect("Failed to spawn worker");

    let mut meta = Meta::new();
    meta.insert("some_custom_meta".into(), json!(42));
    worker
        .send_message(
            Envelope::new(meta, object(json!({"some": "data"})))
                .expect("application metadata cannot use x_ keys"),
        )
        .await
        .unwrap();
}
```

### Python side

```python
from runpyrs import Envelope, Worker, RunScript


class MyWorker(Worker):
    def execute(self, data: dict) -> dict:
        return {
            "status": "ok",
            "url": data.get("url", ""),
            "links": 42,
        }

    def handle_envelope(self, envelope: Envelope) -> None:
        self.log(
            {"message": "received custom data", "data": envelope["data"]},
            level="debug",
        )
        self.send(
            {"accepted": True},
            meta={"correlation_id": envelope["meta"].get("correlation_id")},
        )


if __name__ == "__main__":
    RunScript(MyWorker)
```

Developers define and validate the schema of their own `meta` and `data`
objects. Runpy only owns metadata keys beginning with `x_`.

### uv-managed workers

Runpy launches workers with:

```text
uv run --no-project --script <worker.py> <socket-path> <worker-id>
```

Every managed worker declares its own Python and dependency requirements:

```python
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker",
# ]
# ///
```

`uv` selects or downloads a compatible Python and creates an isolated cached
environment for that script. Runpy does not create a `.venv`, run `uv sync`,
or inject `runpyrs`; the script metadata is authoritative. Ambient
`pyproject.toml` dependencies are ignored.

`Manager::new("path/to/scripts")` resolves `uv` from `PATH`. Packaged
deployments can select another executable:

```rust
let manager = Manager::with_uv_path("path/to/scripts", "/opt/runpy/bin/uv");
```

An adjacent lockfile is optional:

```bash
uv lock --script worker/my_script.py
```

Commit `<worker>.py.lock` for reproducible deployments. When it exists,
Runpy passes `--locked`, so a stale lock fails the worker launch instead of
being modified. Scripts without a lock continue to resolve normally. For
dependency resolution bounded by publication time, add an RFC 3339 cutoff to
the inline metadata:

```python
# [tool.uv]
# exclude-newer = "2025-01-01T00:00:00Z"
```

### Development checks

Enable the repository's pre-commit hook once per clone:

```bash
git config --local core.hooksPath .githooks
```

The hook checks only languages affected by staged files. It verifies formatting
and linting without rewriting files.

Run the complete Rust checks from the repository root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked -- --test-threads=1
```

Run the complete Python checks from the repository root:

```bash
uv run --frozen ruff format --check worker/src/runpyrs worker/tests examples/playground/worker/my_script.py
uv run --frozen ruff check worker/src/runpyrs worker/tests examples/playground/worker/my_script.py
uv run --frozen --package runpyrs --extra dev pytest worker/tests
```

To apply formatting intentionally before re-staging files, run:

```bash
cargo fmt --all
uv run ruff format worker/src/runpyrs worker/tests examples/playground/worker/my_script.py
```


## Environment Variables

The `Scribbler` logger respects these environment variables:

| Variable      | Values                                                         | Description                   |
| ------------- | -------------------------------------------------------------- | ----------------------------- |
| `ENVIRONMENT` | `development`, `dev`                                           | Enables maximum log verbosity |
| `LOG`         | `0`-`5`, `off`, `error`, `warning`, `info`, `debug`, `verbose` | Sets log level                |
| `NO_COLOR`    | (any value)                                                    | Disables ANSI color output    |

Example environment variables:

```bash
# So far nothing really usefull for production
ENVIRONMENT=development
LOG=debug
```

## Project Structure

```text
runpy/
├── manager/                   # Rust crate (the library)
│   ├── src/
│   │   ├── lib.rs             # Manager — top-level orchestrator
│   │   ├── manager.rs         # Worker builder + handle
│   │   ├── protocol.rs        # Bare envelope transport and ControlPlane
│   │   ├── integrity.rs       # uv availability & script validation
│   │   ├── scribbler.rs       # Structured logging service
│   │   └── watchdog.rs        # Health monitoring & /proc stats
│   └── tests/
│       ├── unit.rs            # Unit test harness
│       ├── unit/              # Per-module unit tests
│       └── manager_test.rs    # Integration tests
├── worker/                    # Python worker package (runpyrs)
│   ├── src/
│   │   └── runpyrs/
│   │       ├── __init__.py    # Package exports
│   │       ├── worker.py      # Worker base class
│   │       ├── runScript.py   # RunScript helper
│   │       ├── utils.py       # Envelope types and application builder
│   │       └── py.typed       # PEP 561 marker
│   └── pyproject.toml
├── examples/
│   └── playground/            # Development/testing playground
├── docs/
│   ├── assets/                # Logo and images
│   └── instalation.md         # Installation guide
├── Cargo.toml                 # Workspace root
├── pyproject.toml             # Root Python uv workspace config
├── flake.nix                  # Nix development environment
├── .env.example               # Example environment variables
└── LICENSE
```

## Key Concepts

| Concept               | Description                                                                                       |
| --------------------- | ------------------------------------------------------------------------------------------------- |
| **Manager**           | Top-level orchestrator. Creates workers, owns global handlers, and manages the watchdog.          |
| **Worker**            | Builder before `.spawn()`, remote handle after. Sends envelopes and controls worker lifecycle.    |
| **ControlPlane**      | Per-worker Unix socket listener using 8-byte little-endian length-prefixed JSON.                  |
| **Envelope**          | The serialized `{meta, data}` value exchanged between Rust and Python.                            |
| **InboundEnvelope**   | Rust-only callback context containing the received `Envelope` and a worker-bound `Mailer`.         |
| **MessageSender**     | Channel-based sender for a running worker.                                                        |
| **Mailer**            | Callback responder that sends an envelope to the worker associated with an inbound envelope.      |
| **WatchdogService**   | Background process health monitor and dead-worker cleanup service.                                |
| **IntegrityChecker**  | Validates the Python environment, socket directory, and scripts directory.                        |
| **Scribbler**         | Environment-aware structured logger.                                                             |

## Protocol

Every wire payload is a JSON object with exactly two required object fields:

```json
{
    "meta": {
        "x_wid": "my_script_29032026-1200_Ax4f",
        "x_spath": "/tmp/runpy/rp_my_script.sock",
        "some_custom_meta": 42
    },
    "data": {
        "some": "data"
    }
}
```

`data` belongs entirely to the application. `meta` accepts application metadata
with arbitrary JSON values, but every key beginning with `x_` is reserved by
Runpy. Public builders reject application attempts to create reserved keys.

### Reserved metadata

| Key       | Description                                                                  |
| --------- | ---------------------------------------------------------------------------- |
| `x_wid`   | Manager-generated worker identity; stamped by Runpy at each transport edge.  |
| `x_spath` | Manager-bound Unix socket path; stamped by Runpy at each transport edge.     |
| `x_op`    | Optional Runpy lifecycle operation. Its absence means a custom envelope.      |

Runpy uses these lower-case `x_op` values:

| Operation   | Direction     | Meaning                                      |
| ----------- | ------------- | -------------------------------------------- |
| `ready`     | Python → Rust | Worker connected and is ready.               |
| `execute`   | Rust → Python | Pass `data` to `Worker.execute`.              |
| `retry`     | Rust → Python | Repeat the most recent execution.             |
| `terminate` | Rust → Python | Gracefully close the worker.                  |
| `done`      | Python → Rust | `data` is the direct execution result.        |
| `error`     | Python → Rust | `data.message` describes a worker failure.    |
| `log`       | Python → Rust | `data` contains developer-selected log data.  |

Applications build their own higher-level routing, schemas, validation, and
type safety with non-`x_` metadata and the `data` object. Old
`method`/`headers`/`body` payloads are not accepted.

## Found a bug?

- Open an issue.
- Include your OS, architecture, and Python/Rust versions.
- Include the output you got (screenshot or gist).
- Describe the steps to reproduce.

## Contributing

Feel free to fork and open PRs.
PRs that improve stability, reliability, and test coverage are prioritized.

## License

Apache-2.0 License — see [LICENSE](LICENSE) for details.

---

With ❤️ [@dark1zinn](https://github.com/dark1zinn)
