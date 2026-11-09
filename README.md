![Runpy](docs/assets/runpy_logo.png)

> <p style="font-size: 12px;">This logo was generated with AI and heavily inspired by the <a href="https://elysiajs.com">Elysia</a> logo.</p>

---

Runpy connects a Rust control plane to Python worker processes over Unix domain
sockets. The Rust `runpy` crate owns process lifecycle, health monitoring,
message routing, and attributed output. The Python `runpyrs` SDK turns a PEP
723 script into a managed worker. [`uv`](https://docs.astral.sh/uv/) supplies
the script's declared Python version and dependencies.

Both packages are currently installed from this Git repository; they are not
published to crates.io or PyPI.

## Why Runpy?

Use Rust for orchestration and Python for task-specific code such as scraping,
data processing, or model inference. Runpy provides:

- **Managed lifecycle** — start, monitor, message, and terminate complete
  worker process groups.
- **PEP 723 execution** — each script declares its own Python and dependencies;
  adjacent uv lockfiles are supported.
- **Typed envelope APIs** — exchange application-owned JSON `meta` and `data`
  while Runpy protects its `x_` routing namespace.
- **Bidirectional messaging** — global and per-worker handlers, targeted sends,
  replies, and broadcasts.
- **Watchdog reporting** — inspect registered process state and available
  resource data.
- **Structured logging** — route Python `Worker.log` envelopes through the
  application.
- **Attributed process output** — observe bounded, nonblocking worker stdout
  and stderr records with trusted worker IDs.

## Quick start

### Install and create a worker

```bash
cargo add runpy --git https://github.com/dark1zinn/runpy
cargo add tokio --features full
cargo add serde_json

mkdir -p worker
uv init --script worker/my_worker.py --python 3.10
uv add --script worker/my_worker.py \
  "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker"
```

Create `worker/my_worker.py`:

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
        return {"status": "ok", "input": data}


if __name__ == "__main__":
    RunScript(MyWorker)
```

Start it from Rust:

```rust
use runpy::{Data, Manager};
use serde_json::json;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), runpy::RunpyError> {
    let manager = Manager::builder("worker").build().await?;

    // `spawn` returns once the worker reported `ready`.
    let worker = manager.worker("my_worker").spawn().await?;
    println!("spawned {}", worker.id());

    let input = Data::from_iter([("task".into(), json!("example"))]);
    match worker.execute(input, Duration::from_secs(30)).await {
        Ok(output) => println!("result: {output:?}"),
        Err(error) => eprintln!("request failed: {error}"),
    }

    let exit = worker.terminate().await?;
    println!("worker stopped: {:?}", exit.reason);
    Ok(())
}
```

Runpy starts:

```text
uv run --no-project [--locked] --script <worker.py> <socket-path> <worker-id>
```

The worker sends a versioned `ready` and `spawn` returns. `execute` sends a
request with its own ID; the worker runs one request at a time, and the Python
return value comes back as that request's result. Runpy owns trusted
worker/socket metadata and cleans up the process group and socket when the
worker terminates or the Manager drops.

## Observe worker output

Process output is separate from structured socket envelopes:

```rust
use runpy::WorkerOutputStream;

manager.on_worker_output(|output| match output.stream {
    WorkerOutputStream::Stdout => {
        println!("{}: {}", output.worker_id, output.line);
    }
    WorkerOutputStream::Stderr => {
        eprintln!("{}: {}", output.worker_id, output.line);
    }
});
```

The callback runs on Runpy's dedicated output-dispatcher thread and should
return promptly. Capture is bounded and best effort. Runpy never treats stderr
or output text as an automatic termination signal; applications own that
policy.

## Run requests in parallel

A pool runs independent requests for one script on separate, fresh worker
processes, bounded per pool and by the Manager-wide `max_workers` (default 3):

```rust
use runpy::PoolConfig;

let pool = manager.pool(manager.worker("parse"), PoolConfig::default()).await?;
let first = pool.submit(Data::from_iter([("text".into(), json!("a"))]), Duration::from_secs(30)).await?;
let second = pool.submit(Data::from_iter([("text".into(), json!("b"))]), Duration::from_secs(30)).await?;
let (first, second) = tokio::join!(first.result(), second.result());
```

Each worker serves one request and is then retired. See
[Worker pools](docs/overview.md#worker-pools) for queueing, deadlines, and
shutdown.

## Documentation

- [Project overview](docs/overview.md) — architecture, lifecycle, protocol,
  output guarantees, Python execution model, and operations.
- [Installation and worker setup](docs/installation.md) — Git dependencies,
  PEP 723 scripts, uv lockfiles, and Manager configuration.
- [Migration guide](docs/migration.md) — breaking API and protocol changes
  with their replacements.
- [Python SDK guide](worker/README.md) — `runpyrs` setup and API.
- [Playground](examples/playground/) — executable Rust/Python integration.
- Rust API reference — generate locally with
  `cargo doc -p runpy --no-deps --open`.

## Configuration

The Manager-owned `Scribbler` reads logging settings at construction:

| Variable | Values | Description |
| --- | --- | --- |
| `ENVIRONMENT` | `development`, `dev` | Enables maximum verbosity. |
| `LOG` | `0`–`5`, `off`, `error`, `warning`, `info`, `debug`, `verbose` | Selects the maximum visible level. |
| `NO_COLOR` | any value | Disables ANSI color output. |

Managed workers always receive `PYTHONUNBUFFERED=1` so captured Python output
is prompt.

## Development

Enable the pre-commit hook once per clone:

```bash
git config --local core.hooksPath .githooks
```

The hook runs quick format and lint checks on a snapshot of the staged tree,
never on unstaged edits; behavior tests run in CI (see the
[overview](docs/overview.md#development-and-validation)).

Run Rust checks:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked -- --test-threads=1
cargo test --doc -p runpy --locked
```

The Rust tests include end-to-end runs through real `uv` and the local Python
SDK, so `uv` must be on `PATH`.

Run Python checks:

```bash
uv run --frozen --python 3.10 ruff format --check worker/src/runpyrs worker/tests tests examples/playground/worker/my_script.py
uv run --frozen --python 3.10 ruff check worker/src/runpyrs worker/tests tests examples/playground/worker/my_script.py
uv run --frozen --python 3.10 --package runpyrs --extra dev pytest worker/tests
uv run --frozen --python 3.10 pytest tests/hooks
```

Run the integration playground:

```bash
cargo run -p playground
```

## Contributing

Open an issue with reproduction steps, operating system, architecture, and
Rust/Python/uv versions. Pull requests that improve stability, reliability,
documentation, and useful behavior coverage are welcome.

## License

Apache-2.0 — see [LICENSE](LICENSE).

---

With ❤️ [@dark1zinn](https://github.com/dark1zinn)
