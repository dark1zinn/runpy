# Runpy project overview

Runpy combines a Rust process manager with a small Python worker SDK. Start with
the [repository README](../README.md) for a quick example or the
[installation guide](installation.md) for setup. This page is the canonical
architecture, lifecycle, protocol, and operations reference.

## What Runpy is

The workspace contains two packages:

- `runpy`: a Rust library that validates, starts, observes, messages, and stops
  Python worker processes;
- `runpyrs`: a typed Python SDK that connects a worker to its Manager and maps
  protocol operations to overridable methods.

Every worker is a PEP 723 script started through `uv`. Rust and Python exchange
length-prefixed JSON over a per-worker Unix domain socket. The implementation
therefore targets Unix-like systems; the public manager currently uses Tokio's
`UnixListener` directly.

`runpy` and `runpyrs` are currently installed from this Git repository rather
than crates.io or PyPI. See the [Python package guide](../worker/README.md) for
the SDK-only view.

## Components and ownership

```text
Manager
├── IntegrityChecker
├── Scribbler
└── ControlPlane
    ├── worker registry and process groups
    ├── protocol sessions and Mailer
    ├── Watchdog
    └── stdout/stderr readers and output dispatcher
```

### Manager

`Manager` is the composition root. It owns the integrity checker, logger, and
one shared `ControlPlane`. Dropping it synchronously shuts down registered
workers, removes their sockets, and aborts protocol/output tasks. A Manager
also exposes the global message handler, output observer, logger, watchdog,
broadcast, and all-worker termination APIs.

### Worker facade

`Manager::worker("name")` returns a lightweight `Worker`. Before `spawn`, it is
a builder for environment variables, `--key=value` arguments, and a per-worker
message handler. After `spawn`, it is a remote handle for messaging and
termination. It stores weak references to Manager-owned services, so operations
fail when the Manager has already been dropped. One facade can spawn only once.

### IntegrityChecker

The private `IntegrityChecker` executes the configured `uv --version`, ensures
`/tmp/runpy` exists, verifies the scripts directory, and recursively indexes
Python file stems for diagnostics. `Worker::spawn` performs the checks again
and resolves the requested worker as `<scripts-directory>/<name>.py`.

### ControlPlane, Mailer, and Watchdog

The private `ControlPlane` owns the worker registry and the lifecycle tasks for
each process. A protocol session accepts reconnects on the worker's socket. The
Mailer routes replies to the originating worker's bounded outbound channel.
The single shared Watchdog polls registered child processes and removes process
groups whose `uv` parent has exited.

### Scribbler

`Scribbler` is the Manager-owned stderr logger. It provides severity filtering,
optional component tags, color control, and convenience output. Worker stdout
and stderr are routed through the same logger without being interpreted as
protocol messages.

### Output dispatcher

Each worker has independent asynchronous stdout and stderr readers. They submit
records without waiting to a bounded queue. One detached standard thread owns
synchronous Scribbler output and invokes the current user observer, keeping
terminal writes and callbacks off Tokio runtime workers.

## End-to-end lifecycle

### 1. Construct a Manager

```rust
use runpy::Manager;

let manager = Manager::new("worker");
```

`Manager::new` uses `uv` from `PATH`; `Manager::with_uv_path` accepts another
executable. Construction performs the integrity check and logs a failure rather
than returning `Result`. Call `Manager::check_integrity` when startup must fail
explicitly. `Worker::spawn` also runs the check and returns any failure.

The Watchdog monitor starts immediately when construction occurs inside a
Tokio runtime. If construction occurs outside one, the first worker
registration starts it.

### 2. Configure a Worker facade

```rust
use runpy::Manager;

let manager = Manager::new("worker");
let mut worker = manager.worker("my_worker");
worker
    .env("API_BASE", "https://example.com")
    .arg("mode", "fast");
```

The script name omits `.py`. Repeated environment or argument keys replace the
prior value. Runpy later forces `PYTHONUNBUFFERED=1`, even if the builder sets a
different value.

### 3. Spawn the process

`Worker::spawn` revalidates `uv` and the scripts directory, canonicalizes
runtime paths, verifies the root-level script file, creates a unique identity
and socket under `/tmp/runpy`, and starts:

```text
uv run --no-project [--locked] --script <script.py> <socket-path> <worker-id> [--key=value ...]
```

`--locked` is present only when `<script.py>.lock` exists. The process working
directory is the parent of the scripts directory, and that parent is supplied
as `PYTHONPATH`. Stdout and stderr are piped. On Unix the `uv` process starts a
new process group so termination covers its Python descendant.

A successful `spawn` returns the trusted generated worker ID. Registration
owns the child, socket, session task, both output tasks, and sender. Failures
after process creation kill and reap the process group and remove the socket.
A second `spawn` call on the same facade returns an error.

### 4. Connect and announce readiness

`uv` executes the script. Its `RunScript(MyWorker)` receives the socket path and
worker ID as the first two positional arguments, creates the subclass, connects
the Unix socket, and sends a `ready` envelope. Remaining `--key=value`
arguments become `Worker.extra`.

The Manager accepts repeated socket connections for the same live process. An
output reader belongs to the process lifetime, not a socket connection, so a
reconnect does not duplicate output capture.

### 5. Route messages

Incoming worker envelopes are validated, then their `x_wid` and `x_spath` are
overwritten with trusted Manager values. The current global handler runs first;
the worker-specific handler captured at spawn runs second. These synchronous
callbacks execute in the protocol task and should return promptly.

`InboundEnvelope::reply` queues a best-effort response to the originating
worker. `reply_async` waits for the bounded route and returns an error if the
worker is no longer registered. `Worker::send_message` targets one worker;
`Manager::broadcast` returns a result for every registered worker.

### 6. Execute Python business logic

An `execute` envelope stores its `data` and calls `Worker.execute`. A dictionary
result becomes `done.data`; `None` becomes an empty object. Any exception, or a
non-dictionary result, becomes an `error` envelope. The worker continues its
receive loop after sending the error. `retry` repeats the most recent execute
payload; retry before execute returns an error envelope.

A message without `x_op` is passed once to `Worker.handle_envelope`. The method
can reply with `Worker.send` or emit a structured `Worker.log` envelope.

### 7. Terminate and clean up

`Worker::terminate` sends `terminate`. When the send succeeds, Rust allows two
seconds for graceful handling, then removes the worker and force-stops the
process group. `Manager::terminate_all` broadcasts the same operation, waits
two seconds, and removes every worker. Watchdog cleanup uses the same removal
path when `uv` exits.

Removal aborts the protocol task, kills and reaps the process group, removes the
socket, then permits each output reader up to 250 ms to drain final bytes.
Manager drop is synchronous: it closes the output dispatcher, aborts all tasks,
and immediately kills/reaps all registered groups. Final buffered output is
therefore best effort.

## Protocol reference

### Frame format

Each socket frame consists of:

1. an unsigned 64-bit little-endian payload length;
2. exactly that many UTF-8 JSON bytes.

The JSON value must contain exactly two fields, both objects:

```json
{
  "meta": {
    "x_op": "execute",
    "x_wid": "my_worker_25092026-1200_A1b2",
    "x_spath": "/tmp/runpy/rp_my_worker_25092026-1200_A1b2.sock",
    "correlation_id": 42
  },
  "data": {
    "task": "parse"
  }
}
```

Application code owns all `data` fields and non-`x_` metadata. Every metadata
key beginning with `x_` is reserved by Runpy. Public custom-envelope builders
reject such keys. At each outbound transport edge, Runpy stamps `x_wid` and
`x_spath`; incoming values supplied by a peer are not trusted.

### Reserved metadata

| Key | Meaning |
| --- | --- |
| `x_wid` | Manager-generated identity for the running worker. |
| `x_spath` | Manager-bound socket path for that worker. |
| `x_op` | Optional lifecycle operation; absence denotes a custom envelope. |

Reserved fields must be strings. Unknown `x_` fields and unknown operations
are protocol errors.

### Operations

| Operation | Direction | Behavior |
| --- | --- | --- |
| `ready` | Python → Rust | Announces that the worker connected. |
| `execute` | Rust → Python | Calls `Worker.execute(data)`. |
| `retry` | Rust → Python | Repeats the last execute payload. |
| `terminate` | Rust → Python | Stops the Python receive loop and closes its socket. |
| `done` | Python → Rust | Carries the direct dictionary execution result. |
| `error` | Python → Rust | Carries a `data.message` execution or dispatch failure. |
| `log` | Python → Rust | Carries developer-selected structured log data. |

A known operation sent in the wrong direction closes that socket connection.
A custom envelope has no `x_op` and is valid in either direction. Structured
`log` envelopes are distinct from process stdout/stderr: they use normal
message routing and have a reply-capable callback context.

## Worker process output

Runpy captures output inherited by `uv`, Python, and their descendants unless a
process redirects its descriptors. It emits newline-free records through
Scribbler with trusted attribution:

```text
[worker:<worker-id>][stdout] <line>
[worker:<worker-id>][stderr] <line>
```

Stdout uses `info`; stderr uses `warning`. Both pipes are drained regardless of
the configured log filter. Stderr is diagnostic, not proof that a worker
failed, and Runpy never terminates a worker based on captured text.

Applications can observe the raw record:

```rust
use runpy::{Manager, WorkerOutputStream};

let mut manager = Manager::new("worker");
manager.on_worker_output(|output| match output.stream {
    WorkerOutputStream::Stdout => println!("{}: {}", output.worker_id, output.line),
    WorkerOutputStream::Stderr => eprintln!("{}: {}", output.worker_id, output.line),
});
```

The latest observer replaces the previous one. It runs on the dedicated output
dispatcher thread and must return promptly. A callback panic is caught and
logged so later records continue to dispatch. Applications can send policy
decisions to their own async channel rather than blocking the observer.

Capture is bounded and best effort:

- a shared queue holds 256 records;
- a full queue drops records instead of backpressuring the worker;
- the next accepted record reports prior loss in `dropped_lines_before` when
  possible;
- reaching 16 KiB without a newline emits the bounded record with
  `truncated = true`; later bytes continue in subsequent records;
- byte order is preserved within one stream, but stdout, stderr, and socket
  envelopes have no total ordering;
- invalid UTF-8 is decoded lossily and control characters other than tab are
  escaped;
- an unterminated final fragment is emitted at EOF;
- piped streams are not TTYs, so programs may change color or progress output.

Runpy forces `PYTHONUNBUFFERED=1` after builder environment values to keep
Python output prompt. This favors observability over buffered write throughput.

## Python SDK execution model

### RunScript bootstrap

A managed script ends with:

```python
if __name__ == "__main__":
    RunScript(MyWorker)
```

`RunScript` requires a `Worker` subclass. It reads the socket and trusted worker
ID from `sys.argv[1]` and `sys.argv[2]`. Later arguments matching
`--key=value` populate `Worker.extra`; other arguments are ignored. Missing
required arguments, an invalid class, or initialization failure prints a
configuration message and exits with status 1.

### Worker hooks

Override `execute(data)` for managed work and return a dictionary or `None`.
Override `handle_envelope(envelope)` for custom envelopes. Use `send(data,
meta=...)` for a custom response. Public builders copy the supplied objects and
reject application metadata in the `x_` namespace.

`run()` owns the receive loop. Malformed frames, invalid JSON, invalid reserved
metadata, and wrong-direction operations are protocol violations: Python prints
a diagnostic, marks the worker closed, and closes the socket.

### Structured log fallback

`Worker.log(data, level=..., meta=...)` normally sends an `x_op="log"` envelope.
Only when that socket operation raises `OSError`, it makes one best-effort,
flushed stdout write:

```text
[runpy-log-fallback][level=<level>] <data repr>
```

The fallback has no Python-supplied worker identity; Rust adds trusted output
attribution. A fallback `OSError` or the `ValueError` raised by closed stdout is
suppressed, and the original socket error is re-raised. Serialization,
metadata, and other programmer errors do not use the fallback. A kernel can
report a send error after accepting the complete frame, so the Manager may
observe both the structured envelope and fallback line.

## Operations and observability

### Scribbler settings

`Manager::logger()` returns the shared `Arc<Scribbler>`. It writes to stderr and
reads these variables at Manager construction:

| Variable | Values | Effect |
| --- | --- | --- |
| `ENVIRONMENT` | `development`, `dev` | Enables every severity. |
| `LOG` | `0`–`5`, `off`, `error`, `warning`, `info`, `debug`, `verbose` | Selects the maximum visible level; default is `info`. |
| `NO_COLOR` | any value | Disables ANSI color output. |

Severity order is `Error`, `Warning`, `Info`, `Debug`, `Verbose`; `Off`
disables logging. Component-tagged and untagged methods share the same filter.

### Watchdog reports

The shared watchdog checks child exit state every five seconds and removes dead
worker process groups. `report()` snapshots all registered workers;
`report_worker(id)` returns one report or `None` when the ID is absent.

A `WorkerReport` contains the trusted worker name, `uv` parent PID, state,
resident memory, and CPU percentage. Current reports produce `Healthy` when the
process is observable and `Dead` otherwise; `Frozen` is defined but not
currently detected. Linux reads resident memory from `/proc/<pid>/status` in
KiB. Other platforms report `memory_kb = None`. `cpu_percent` is currently
always `None` because CPU sampling is not implemented.

The watchdog reports process state; application-level retry, alerting, and
termination policy remains the caller's responsibility.

## Development and validation

### Repository layout

```text
runpy/
├── manager/                    # Rust runpy crate
│   ├── src/
│   │   ├── lib.rs             # Public Manager API and crate docs
│   │   ├── manager.rs         # Worker builder and remote handle
│   │   ├── protocol.rs        # Envelope transport and ControlPlane
│   │   ├── integrity.rs       # uv/script validation
│   │   ├── scribbler.rs       # Shared logger
│   │   └── watchdog.rs        # Process monitoring
│   └── tests/                 # Rust integration tests
├── worker/                     # Python runpyrs package and tests
├── examples/playground/        # End-to-end Rust/Python example
├── docs/                       # Installation and canonical overview
├── Cargo.toml                  # Rust workspace
└── pyproject.toml              # Python uv workspace and Ruff settings
```

The [Rust playground](../examples/playground/src/main.rs) and its
[Python worker](../examples/playground/worker/my_script.py) demonstrate global
and per-worker handlers, custom envelopes, structured logging, execution,
watchdog reporting, and shutdown.

Enable the repository hook once per clone:

```bash
git config --local core.hooksPath .githooks
```

Run Rust validation from the repository root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked -- --test-threads=1
cargo test --doc -p runpy --locked
cargo doc -p runpy --no-deps
```

Run Python validation from the repository root:

```bash
uv run --frozen --python 3.10 ruff format --check worker/src/runpyrs worker/tests examples/playground/worker/my_script.py
uv run --frozen --python 3.10 ruff check worker/src/runpyrs worker/tests examples/playground/worker/my_script.py
uv run --frozen --python 3.10 --package runpyrs --extra dev pytest worker/tests
```

Exercise the complete system with:

```bash
cargo run -p playground
```
