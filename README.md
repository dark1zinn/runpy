# RUNPY

A Rust crate for spawning, managing, and communicating with Python worker processes over Unix sockets.

Combine Rust's performance and robustness with Python's simplicity for writing scripts — data analysis, scraping, ML inference, whatever you need. Rust acts as the **control plane**; Python scripts are **workers**.

## Why?

Python is simple to write but limited in concurrency and reliability. Rust is fast and robust but overkill for throwaway scripts.

**Runpy** lets you write your business logic in Python, while Rust handles process orchestration, health monitoring, and structured communication. For example:

- Build a web server in Rust that spawns Python scrapers on demand
- Run data analysis pipelines where Rust manages scheduling and Python does the heavy lifting
- Offload ML inference to Python workers while Rust handles the API layer

## Architecture

```text
┌─────────────────────────────────────────────────┐
│  Rust (Manager)                                 │
│    ├─ IntegrityChecker   (venv & script checks) │
│    ├─ Workers[]          (builder + handle)      │
│    │    └─ ControlPlane  (Unix socket protocol)  │
│    └─ WatchdogService    (health & resources)    │
│                                                  │
│         ┌─── Unix Socket (length-prefixed JSON)  │
│         ▼                                        │
│  Python (Worker)                                 │
│    └─ bridge/worker.py                           │
│         ├─ execute()        — your logic         │
│         └─ handle_request() — custom routing     │
└──────────────────────────────────────────────────┘
```

## Quick Start

### Prerequisites

- Rust / Cargo
- Python 3.10+

### Setup

```bash
# Clone the repo
git clone https://github.com/dark1zinn/runpy.git
cd runpy

# Set up the Python worker environment
cd worker
python -m venv .venv
source .venv/bin/activate
pip install -e .
cd ..
```

### Usage

Add `runpy` as a dependency in your project:

```toml
[dependencies]
runpy = { path = "path/to/runpy/manager" }
tokio = { version = "1", features = ["full"] }
serde_json = "1.0"
```

Then use the API:

```rust
use runpy::{Manager, Message};

#[tokio::main]
async fn main() {
    // 1. Create the Manager
    let mut manager = Manager::new("path/to/.venv", "path/to/scripts");

    // 2. (Optional) Global message handler — fires for every worker
    manager.on_message(|envelope| {
        println!("[{}] {:?}", envelope.worker_id, envelope.message);
    });

    // 3. Create, configure, and spawn a worker
    let mut worker = manager.worker("my_script");
    worker.env("API_KEY", "secret");
    worker.on_message(|envelope| {
        match &envelope.message {
            Message::Done { message, data } => println!("✓ {}: {}", message, data),
            Message::Error { message, .. } => eprintln!("✗ {}", message),
            _ => {}
        }
    });

    let worker_id = worker.spawn().await.expect("Failed to spawn worker");
    println!("Spawned: {}", worker_id);

    // 4. Send messages to the worker
    worker.send_message(&Message::Execute {
        payload: serde_json::json!({"url": "https://example.com"}),
    }).await.unwrap();

    // 5. Check health via watchdog
    let reports = manager.dog.report().await;
    for r in &reports {
        println!("[{:?}] {} (pid {}, mem {:?} kB)", r.state, r.worker_name, r.pid, r.memory_kb);
    }

    // 6. Terminate a specific worker
    worker.terminate().await.unwrap();

    // Manager's Drop automatically kills remaining workers and cleans up sockets.
}
```

### Python side

Write a worker script in your scripts directory:

```python
from bridge.worker import Worker, RunScript

class MyWorker(Worker):
    def execute(self, payload: dict) -> dict:
        # Your business logic here
        url = payload.get("url", "")
        return {"status": "ok", "url": url, "links": 42}

    def handle_request(self, request_data: dict):
        # Optional: handle custom (non-internal) message types
        self.send("INFO", f"Custom request: {request_data}")

if __name__ == "__main__":
    RunScript(MyWorker)
```

### Run the tests

```bash
cargo test
```

## Project Structure

```text
runpy/
├── manager/                 # Rust crate (the library)
│   ├── src/
│   │   ├── lib.rs           # Manager — top-level orchestrator
│   │   ├── manager.rs       # Worker builder + handle
│   │   ├── protocol.rs      # ControlPlane, Message, Envelope, MessageSender
│   │   ├── integrity.rs     # Venv & script validation
│   │   └── watchdog.rs      # Health monitoring & /proc stats
│   └── tests/
│       └── manager_test.rs
├── worker/                  # Python worker package
│   ├── src/
│   │   ├── bridge/
│   │   │   └── worker.py    # Worker base class & RunScript helper
│   │   └── scripts/
│   │       └── test.py      # Example worker script
│   └── pyproject.toml
├── examples/
│   └── template/            # Example Rust project using runpy
├── sockets/
│   └── main.rs              # API reference / documentation example
└── Cargo.toml               # Workspace root
```

## Key Concepts

| Concept | Description |
| --- | --- |
| **Manager** | Top-level orchestrator. Creates workers, holds the global message handler, owns the watchdog. |
| **Worker** | Builder before `.spawn()`, remote handle after. Configure env vars, message handlers, then spawn. |
| **ControlPlane** | Per-worker Unix socket listener. Handles bidirectional length-prefixed JSON messaging. |
| **Envelope** | Wraps every `Message` with a `worker_id` so handlers know which worker sent it. |
| **MessageSender** | Channel-based sender returned by the control plane for sending messages to a running worker. |
| **WatchdogService** | Background health monitor. Checks process state, reads `/proc` for memory, cleans up dead workers. |
| **IntegrityChecker** | Validates the venv, ensures socket directories exist, indexes available scripts. |

## Message Types

The protocol supports these message types (exchanged as JSON over Unix sockets):

| Type | Direction | Description |
| --- | --- | --- |
| `READY` | Python → Rust | Worker has connected and is ready |
| `EXECUTE` | Rust → Python | Send a payload for the worker to process |
| `DONE` | Python → Rust | Execution completed with result data |
| `ERROR` | Python → Rust | An error occurred (with optional stack trace) |
| `INFO` | Python → Rust | Informational log message |
| `DEBUG` | Python → Rust | Debug log message |
| `TERMINATE` | Rust → Python | Request graceful shutdown |
| `RETRY` | Rust → Python | Re-execute the last payload |
| `META` | Rust → Python | Send metadata (e.g. worker name) |
| `STATUS` | Either | Request/report uptime |
| `GET` | Rust → Python | Request a value by key |
| `ACTION` | Rust → Python | Trigger a named action with params |

## Found a bug?

- Open an issue.
- Include your OS, architecture, and Python/Rust versions.
- Include the output you got (screenshot or gist).
- Describe the steps to reproduce.

## Contributing

Feel free to fork and open PRs.
PRs that improve stability, reliability, and test coverage are prioritized.

> With ❤️ @dark1zinn
