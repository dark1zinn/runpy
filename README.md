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
- **HTTP-like Protocol**: Clean JSON messages with methods, headers, and body
- **Watchdog Service**: Automatic health monitoring and dead worker cleanup
- **Structured Logging**: Environment-aware logging via \`Scribbler\`
- **Bidirectional Communication**: Send commands and receive responses
- **Extra Arguments**: Pass custom \`--key=value\` arguments to workers

## Architecture

```text
┌─────────────────────────────────────────────────┐
│  Rust (Manager)                                 │
│    ├─ IntegrityChecker   (venv & script checks) │
│    ├─ Scribbler          (structured logging)   │
│    ├─ Workers[]          (builder + handle)     │
│    │    └─ ControlPlane  (Unix socket protocol) │
│    └─ WatchdogService    (health & resources)   │
│                                                 │
│         ┌─── Unix Socket (length-prefixed JSON) │
│         ▼                                       │
│  Python (Worker)                                │
│    └─ runpyrs/worker.py                         │
│         ├─ execute()        — your logic        │
│         └─ handle_request() — custom routing    │
└─────────────────────────────────────────────────┘
```

## Quick Start

### Prerequisites

- Rust / Cargo
- Python 3.10+

### Setup

> Note that `Runpy` isn't available in crates.io yet, nor `runpyrs` Python package in PyPi!

```bash
# Create your project
mkdir myapp && cd myapp
cargo init

# Add '--branch dev' to get from latest commits
cargo add --git https://github.com/dark1zinn/runpy -p runpy
# Also the needed dependencies
cargo add tokio serde_json

# Create the Python environment and the worker folder
python -m venv .venv
mkdir worker && cd worker
uv sync
# Append '#branch=dev' for latest commits
uv add "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker"
cd ..
```

For a better understanding on how to add the crate/package to your project see [this instalation guide](docs/instalation.md)

You can take a look in the [examples folder](examples) for a suggested project folder structure

### Usage

```rust
use runpy::{scribbler, Manager, Message, Method};

#[tokio::main]
async fn main() {
    let log = scribbler();

    // 1. Create the Manager
    let mut manager = Manager::new("path/to/.venv", "path/to/scripts");
    log.success("Manager initialized");

    // 2. (Optional) Global message handler — fires for every worker
    manager.on_message(|envelope| {
        scribbler().verbose_with(
            "Global",
            &format!("Worker '{}' → {:?}", envelope.worker_id, envelope.message),
        );
    });

    // 3. Create, configure, and spawn a worker
    let mut worker = manager.worker("my_script");
    worker.env("API_KEY", "secret");           // Environment variable
    worker.arg("db", "postgres");              // Extra argument (--db=postgres)
    worker.on_message(|envelope| {
        let log = scribbler();
        let msg = &envelope.message;
        match msg.method {
            Method::Done => {
                let data = msg.body.as_ref()
                    .and_then(|b| b.get("data"))
                    .unwrap_or(&serde_json::json!({}));
                log.success(&format!("Done: {}", data));
            }
            Method::Error => {
                let message = msg.body.as_ref()
                    .and_then(|b| b.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown error");
                log.error_with("Worker", message);
            }
            Method::Log => {
                let level = msg.get_header("X-Log-Level").unwrap_or("info");
                let message = msg.body.as_ref()
                    .and_then(|b| b.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match level {
                    "debug" => log.debug_with("Worker", message),
                    "warning" => log.warning_with("Worker", message),
                    "error" => log.error_with("Worker", message),
                    _ => log.info_with("Worker", message),
                }
            }
            _ => log.debug_with("Worker", &format!("{:?}", msg)),
        }
    });

    let worker_id = worker.spawn().await.expect("Failed to spawn worker");
    log.success(&format!("Spawned: {}", worker_id));

    // 4. Send messages to the worker
    worker.send_message(Message::execute(
        serde_json::json!({"url": "https://example.com"})
    )).await.unwrap();

    // 5. Check health via watchdog
    let reports = manager.dog.report().await;
    log.separator();
    log.info("Watchdog Report:");
    for r in &reports {
        log.info_with("Health", &format!("[{:?}] {} (pid {}, mem {:?} kB)", r.state, r.worker_name, r.pid, r.memory_kb));
    }

    // 6. Broadcast to all workers
    let _results = manager.broadcast(Message::get("status")).await;

    // 7. Terminate a specific worker (or use terminate_all() from the manager)
    log.info("Shutting down...");
    worker.terminate().await.unwrap();

    // Manager's Drop automatically kills remaining workers and cleans up sockets.
}
```

### Python side

Write a worker script in your scripts directory:

```python
from runpyrs import Worker, RunScript

class MyWorker(Worker):
    def execute(self, payload: dict) -> dict:
        # Access extra args passed from Rust
        db = self.extra.get("db", "default")  # --db=postgres → "postgres"

        # Your business logic here
        url = payload.get("url", "")
        return {"status": "ok", "url": url, "db": db, "links": 42}

    def handle_request(self, request_data: dict):
        # Optional: handle custom (non-internal) message types
        self.send("LOG", message=f"Custom request: {request_data}", level="debug")

if __name__ == "__main__":
    # A helper to run the worker, takes care of args and few checks
    RunScript(MyWorker)
```

### Run the tests

```bash
cargo test
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
│   │   ├── protocol.rs        # ControlPlane, Message, Envelope, MessageSender
│   │   ├── integrity.rs       # Venv & script validation
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
│   │       ├── utils.py       # Protocol types & message builders
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

| Concept              | Description                                                                                          |
| -------------------- | ---------------------------------------------------------------------------------------------------- |
| **Manager**          | Top-level orchestrator. Creates workers, holds the global message handler, owns the watchdog.        |
| **Worker**           | Builder before `.spawn()`, remote handle after. Configure env vars, extra args, handlers.          |
| **ControlPlane**     | Per-worker Unix socket listener. Handles bidirectional length-prefixed JSON messaging.               |
| **Envelope**         | Wraps every `Message` with a `worker_id` and `mailer` for responses.                           |
| **MessageSender**    | Channel-based sender for sending messages to a running worker.                                       |
| **Mailer**           | Response channel attached to envelopes for replying to specific messages.                            |
| **WatchdogService**  | Background health monitor. Checks process state, reads `/proc` for memory, cleans up dead workers. |
| **IntegrityChecker** | Validates the venv, ensures socket directories exist, indexes available scripts.                     |
| **Scribbler**        | Environment-aware structured logging with colors and levels.                                         |

## Protocol

Messages follow an HTTP-like JSON structure:

```json
{
    "method": "EXECUTE",
    "headers": {
        "X-Worker-Id": "my_script_29032026_abc1",
        "Content-Type": "application/json",
        "X-Socket-Path": "/tmp/runpy/rp_xxx.sock"
    },
    "body": { "task": "process", "data": [1, 2, 3] }
}
```

### Methods

> Notice that some of these methods may not make sense nor have a clear usage purpose, these are in work, feel fre to open issues with suggestions

| Method      | Direction     | Description                                   |
| ----------- | ------------- | --------------------------------------------- |
| `READY`     | Python → Rust | Worker has connected and is ready             |
| `EXECUTE`   | Rust → Python | Send a payload for the worker to process      |
| `DONE`      | Python → Rust | Execution completed with result data          |
| `ERROR`     | Python → Rust | An error occurred (with optional stack trace) |
| `LOG`       | Python → Rust | Log message (level in `X-Log-Level` header)   |
| `TERMINATE` | Rust → Python | Request graceful shutdown                     |
| `RETRY`     | Rust → Python | Re-execute the last payload                   |
| `META`      | Rust → Python | Send metadata (e.g. worker name)              |
| `STATUS`    | Either        | Request/report uptime                         |
| `GET`       | Rust → Python | Request a value by key                        |
| `POST`      | Rust → Python | Send data                                     |
| `PUT`       | Rust → Python | Update data                                   |
| `DELETE`    | Rust → Python | Remove data                                   |
| `ACTION`    | Rust → Python | Trigger a named action with params            |

### Standard Headers

| Header          | Description                                    |
| --------------- | ---------------------------------------------- |
| `X-Worker-Id`   | Worker's unique identifier                     |
| `X-Socket-Path` | Path to the Unix socket                        |
| `Content-Type`  | Always `application/json`                      |
| `X-Log-Level`   | Log level: `debug`, `info`, `warning`, `error` |
| `X-Error-Level` | Error severity: `dismissable`, `critical`      |
| `X-Stack-Trace` | Optional Python stack trace for errors         |
| `X-Uptime`      | Worker uptime in seconds                       |
| `X-Action`      | Action name for `ACTION` method                |
| `X-Key`         | Key name for `GET` requests                    |

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
