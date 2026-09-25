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
use runpy::{Data, Envelope, Manager};
use serde_json::{json, Value};
use std::time::Duration;

fn object(value: Value) -> Data {
    value.as_object().cloned().expect("JSON object")
}

#[tokio::main]
async fn main() {
    let mut manager = Manager::new("worker");
    let (finished_tx, mut finished_rx) = tokio::sync::mpsc::unbounded_channel();
    manager.on_message(move |inbound| {
        let operation = inbound
            .envelope
            .meta()
            .get("x_op")
            .and_then(Value::as_str);

        match operation {
            Some("ready") => inbound.reply(Envelope::execute(object(
                json!({"task": "example"}),
            ))),
            Some("done") => {
                println!(
                    "result: {}",
                    Value::Object(inbound.envelope.data().clone())
                );
                let _ = finished_tx.send(());
            }
            Some("error") => {
                eprintln!(
                    "worker error: {}",
                    Value::Object(inbound.envelope.data().clone())
                );
                let _ = finished_tx.send(());
            }
            _ => {}
        }
    });

    let mut worker = manager.worker("my_worker");
    let worker_id = worker.spawn().await.expect("worker should start");
    println!("spawned {worker_id}");
    tokio::time::timeout(Duration::from_secs(30), finished_rx.recv())
        .await
        .expect("worker response timed out")
        .expect("worker response channel closed");
    worker.terminate().await.expect("worker should stop");
}
```

Runpy starts:

```text
uv run --no-project [--locked] --script <worker.py> <socket-path> <worker-id>
```

The worker sends `ready`, Rust replies with `execute`, and the Python return
value arrives as `done`. Runpy owns trusted worker/socket metadata and cleans up
the process group and socket when the worker terminates or the Manager drops.

## Observe worker output

Process output is separate from structured socket envelopes:

```rust
use runpy::{Manager, WorkerOutputStream};

let mut manager = Manager::new("worker");
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

## Documentation

- [Project overview](docs/overview.md) — architecture, lifecycle, protocol,
  output guarantees, Python execution model, and operations.
- [Installation and worker setup](docs/installation.md) — Git dependencies,
  PEP 723 scripts, uv lockfiles, and Manager configuration.
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

Run Rust checks:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked -- --test-threads=1
cargo test --doc -p runpy --locked
```

Run Python checks:

```bash
uv run --frozen --python 3.10 ruff format --check worker/src/runpyrs worker/tests examples/playground/worker/my_script.py
uv run --frozen --python 3.10 ruff check worker/src/runpyrs worker/tests examples/playground/worker/my_script.py
uv run --frozen --python 3.10 --package runpyrs --extra dev pytest worker/tests
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
