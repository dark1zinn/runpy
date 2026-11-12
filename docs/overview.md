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

`Manager` is the composition root, created with `Manager::builder(scripts)`.
It owns the integrity checker, logger, and one shared `ControlPlane`. Dropping
it synchronously shuts down registered workers, removes their sockets, and
aborts protocol/output tasks. A Manager also exposes the global message
handler, output observer, logger, watchdog, broadcast, and all-worker
termination APIs.

### WorkerBuilder and Worker

`Manager::worker("name")` returns a `WorkerBuilder`: immutable configuration
for environment variables, `--key=value` arguments, working directory, stdin,
and a per-worker message handler. Setters consume and return the builder, and
it is cheap to clone, so one template can launch many workers.
`WorkerBuilder::spawn` consumes it and returns a running `Worker` handle for
messaging and termination. Configuration and running state are separate types,
so a running worker cannot be reconfigured and a builder cannot be messaged.
Both hold weak references to Manager-owned services: after the Manager is
dropped they return `RunpyError::ManagerClosed`.

### IntegrityChecker

The private `IntegrityChecker` runs `uv --version` under the startup timeout
(a hung `uv` is killed and reaped, and cancelling `build` kills it too) and
verifies that the scripts path is a directory. `build` and
`Manager::check_integrity` run these checks; spawns do not repeat the `uv`
check. Each `WorkerBuilder::spawn` requires the supplied name to be a single
path component and resolves `<scripts-directory>/<name>.py` afresh, so scripts
added or removed after `build` are seen; nothing is indexed or cached.
Filesystem checks run on Tokio's blocking pool, so no check stalls a runtime
thread. Symlinked scripts are followed: the scripts directory is trusted.

### Socket directory and identities

Each Manager creates its own socket directory, `runpy-<random>`, with mode
`0700` in the platform temporary directory or in `socket_parent`. The name is
unpredictable and created atomically; Runpy then verifies that the directory
is owned by the current user and private, and rejects a path that is not
UTF-8 or would make socket paths exceed the Unix limit (about 100 bytes).
`Manager::socket_dir()` returns it, and dropping the Manager removes it after
its workers stop. Runpy never adopts or deletes a directory it did not create.

A worker ID is the script name plus a 128-bit random suffix, so it is unique
across Managers and restarts. Its socket file has a short, independent random
name, so long script names cannot exceed the socket path limit. Runpy never
unlinks an existing path: if a socket name is taken it picks another, up to
eight attempts. Registering an ID that is already in use fails and stops only
the new process.

These measures keep other users and other Managers away from a worker's
socket. They are not a sandbox: worker scripts and other processes of the same
user are trusted.

### ControlPlane, Mailer, and Watchdog

The private `ControlPlane` owns the worker registry and the lifecycle tasks for
each process. A protocol session serves the worker's single connection. The
Mailer resolves replies to the originating worker's bounded outbound route.
The single shared Watchdog polls registered child processes and removes process
groups whose `uv` parent has exited.

### Scribbler

`Scribbler` is the Manager-owned stderr logger. It provides severity filtering,
optional component tags, color control, and convenience output. Worker stdout
and stderr reach it only when `mirror_worker_output` is enabled, and are never
interpreted as protocol messages.

### Output dispatcher

Each worker has independent asynchronous stdout and stderr readers. They apply
the per-stream byte budget and submit records without waiting to a bounded
queue. One detached standard thread mirrors records to Scribbler when enabled
and invokes the current user observer, keeping terminal writes and callbacks
off Tokio runtime workers.

## End-to-end lifecycle

### 1. Build a Manager

```rust
use runpy::Manager;
use std::time::Duration;

let manager = Manager::builder("worker")
    .uv_path("/opt/runpy/bin/uv") // default: `uv` from PATH
    .send_timeout(Duration::from_secs(5))
    .build()
    .await?;
```

`build` validates every setting before creating resources, then checks that
`uv --version` succeeds and that the scripts directory exists. Failures are
typed: `InvalidConfig` (also returned outside a Tokio runtime),
`UvUnavailable`, and `ScriptsDirectory`. `Manager::check_integrity` repeats
the runtime checks later. The Watchdog monitor starts during `build`.

| Setter | Default | Meaning |
| --- | --- | --- |
| `socket_parent` | temp dir | Where the private `runpy-*` socket directory is created. |
| `uv_path` | `uv` | Executable that launches workers. |
| `max_frame_bytes` | 8 MiB | Maximum JSON body per frame, both directions. |
| `frame_timeout` | 30 s | Time to finish a frame after its first byte. |
| `outbound_capacity` | 64 | Admitted frames per worker awaiting the writer. |
| `queued_bytes` | 16 MiB | Manager-wide encoded bytes awaiting a write; must hold one maximum frame. |
| `send_timeout` | 5 s | Deadline for outbound admission plus the write. |
| `startup_timeout` | 30 s | Deadline for the `uv --version` check. |
| `shutdown_grace` | 2 s | Time a worker gets to exit after termination starts. |
| `max_workers` | 3 | Worker processes, across all scripts, that may exist at once. |
| `max_pending_requests` | 64 | Submitted requests, across all workers, not yet consumed. |
| `mirror_worker_output` | off | Copy worker stdout/stderr into the Manager's log. |
| `worker_output_rate` | 1 MiB/s | Bytes per second each worker stream may deliver; `None` for no limit. |

`max_workers` counts managed workers, not OS processes: each worker is a `uv`
process plus its Python descendants. A worker holds a slot from before `uv`
starts until its process group has been stopped and reaped, so starting and
stopping workers count too. `spawn` waits for a free slot within the startup
timeout (the same deadline that then covers waiting for `ready`) and fails with
`Timeout(Admission)` without launching anything. A failed or cancelled start
releases its slot once cleanup finishes.

### 2. Configure a WorkerBuilder

```rust
let builder = manager
    .worker("my_worker")
    .env("API_BASE", "https://example.com")
    .arg("mode", "fast");
```

The script name omits `.py`. Repeated environment or argument keys replace the
prior value. Runpy later forces `PYTHONUNBUFFERED=1` and its frame-limit
variables, even if the builder sets different values. `cwd(dir)` overrides the
working directory; `stdin(StdinMode::Inherit)` lets a worker read the
Manager's stdin, which is otherwise the null device.

### 3. Spawn the process

`WorkerBuilder::spawn` does not recheck `uv`. It requires the name to be a
single path component, canonicalizes runtime paths, verifies that
`<scripts-directory>/<name>.py` is a file, creates a unique identity and
socket in the Manager's socket directory, and starts:

```text
uv run --no-project [--locked] --script <script.py> <socket-path> <worker-id> [--key=value ...]
```

`--locked` is present only when `<script.py>.lock` exists. The process working
directory defaults to the parent of the scripts directory, and that parent is
supplied as `PYTHONPATH` unless the builder overrides it. Stdin is the null
device by default; stdout and stderr are piped. On Unix the `uv` process starts
a new process group so termination covers its Python descendant.

A successful `spawn` returns a `Worker` whose `id()` is the trusted generated
worker ID. A bad script name fails with `InvalidScript`. Registration owns the
child, socket, session task, both output tasks, and sender. Failures after
process creation kill and reap the process group and remove the socket.

### 4. Connect and announce readiness

`uv` executes the script. Its `RunScript(MyWorker)` receives the socket path and
worker ID as the first two positional arguments, creates the subclass, and
connects the Unix socket. `Worker.run()` then sends a `ready` envelope with
`{"protocol_version": 1, "capabilities": [...]}` before receiving anything, so
work never reaches a worker that is still constructing. Remaining
`--key=value` arguments become `Worker.extra`.

The Manager requires `ready` to be the first envelope and to carry protocol
version 1; an older or newer SDK, a missing version, a repeated `ready`, or any
other envelope first is a protocol violation. Rust and Python must therefore
be upgraded together. `ready` is lifecycle state, not a message: it is not
passed to `on_message` handlers. `WorkerBuilder::spawn` returns only after
`ready` arrives, so work can be sent immediately; `Worker::capabilities()`
lists what the worker advertised. If the process exits first, `spawn` fails
with `WorkerExited` at once; if `ready` does not arrive within the startup
timeout, the process group is stopped and `spawn` fails with
`Timeout(Startup)`. Cancelling `spawn` while it waits stops the process too.

Each worker gets exactly one socket connection. When that connection closes,
fails, or violates the protocol, the session ends and the Manager removes the
worker: it stops the process group, fails every pending send, and removes the
socket. There is no reconnect and nothing is replayed; create a replacement
worker to continue. Output readers belong to the process lifetime and stop with
it.

### 5. Route messages

Incoming worker envelopes are validated, then their `x_wid` and `x_spath` are
overwritten with trusted Manager values. The current global handler runs first;
the worker-specific handler captured at spawn runs second. These synchronous
callbacks execute in the protocol task and should return promptly. Both receive
the same shared envelope (`InboundEnvelope::envelope` is an `Arc<Envelope>`),
so dispatching to two handlers does not copy the JSON. A panic in either
handler is caught and logged; the other handler and later envelopes are still
dispatched. Runpy cannot interrupt a handler that blocks, so a blocking handler
stalls that worker's inbound dispatch.

Every outbound send is bounded. The envelope is validated, trust-stamped, and
encoded first, so direction and frame-size errors return to the caller
immediately. Admission then reserves a slot on the worker's route (64 frames)
and bytes from a Manager-wide budget (16 MiB) shared by all workers. The
writer releases both when the frame is written or discarded, so queued memory
never exceeds the budget and no detached task ever waits on a full queue.

- `Worker::send_message` and `InboundEnvelope::reply_async` wait for capacity
  and then for the write. Both share one send deadline (5 seconds). Success
  means the frame was completely written to the socket, not that the worker
  processed it.
- `Worker::try_send_message` and `InboundEnvelope::reply` admit immediately
  or fail with `RunpyError::QueueFull`. The returned `SendReceipt` reports the
  write outcome; dropping it does not cancel the send.
- `Manager::broadcast` returns each registered worker's write outcome.

Admitted frames are written in admission order. A deadline produces
`RunpyError::Timeout` with a stage: `Admission` (nothing was queued), `Queued`
(admitted, discarded unwritten), or `Write` (interrupted mid-frame). A `Write`
timeout leaves delivery unknown, so Runpy closes that connection instead of
risking a corrupted stream; it never resends a frame.

### 6. Execute Python business logic

`Worker::execute(data, deadline)` (or `Worker::submit`, which returns a
`Request` handle) sends an `execute` envelope carrying a Manager-generated
request ID in `x_req`. Each worker has a request dispatcher: requests wait in
order and only one is written to the worker at a time; the next is sent only
after the active one's `done` or `error` arrives. Different workers run
requests concurrently. Submitted, unconsumed requests are bounded
Manager-wide by `max_pending_requests` (64); waiting for that capacity counts
against the deadline and expires as `Timeout(Admission)`.

The deadline covers the whole request. A request that expires while queued
fails with `Timeout(Queued)` and is never sent. One that expires while running
fails with `Timeout(Response)`, but the worker stays occupied until it answers;
the late result is discarded. Dropping a `Request` does not cancel it either.
Runpy never retries or replays a request: if the worker exits or the
connection closes, the running request fails with `OutcomeUnknown` and queued
ones with `WorkerExited`. Resubmit explicitly when the work is idempotent.

On the Python side, `Worker.execute` receives `data`. A dictionary result
becomes `done.data`; `None` becomes an empty object. An `Exception`, a
non-dictionary result, or a result that cannot be encoded becomes an `error`
envelope whose data is `{"kind": "<exception type>", "message": "<text>"}`;
an exception with an empty message reports its type name as the message. Both
carry the request's `x_req`, and Rust returns the error as
`RunpyError::Execution`. The worker continues its receive loop after sending
the error. `SystemExit`, `KeyboardInterrupt`, and other non-`Exception` signals
are not converted: they end the worker. A `done` or `error` with an unknown or
missing request ID while a request is active is a protocol violation. An
`error` without `x_req` (for example from a failing `handle_envelope`) is
unsolicited: it completes nothing and only reaches handlers. Request results
also reach handlers after the request completes. Raw `execute` envelopes
cannot be sent through `send_message` or `reply`.

A message without `x_op` is passed once to `Worker.handle_envelope`. The method
can reply with `Worker.send` or emit a structured `Worker.log` envelope.

#### Cancellation

`Request::canceller()` returns a `Canceller` that can stop the request while
another task awaits `Request::result`. A queued request is cancelled locally:
it never runs and its result is `Cancelled`. A running request can be
cancelled only on a worker that advertised `cancel` (the Python
`AsyncWorker`); otherwise `cancel` returns `Unsupported`. Runpy sends `cancel`
and waits, within `send_timeout`, for the worker to confirm with `cancelled`:
`Cancellation::Cancelled` is reported only after the worker stopped the
request, and its result is `RunpyError::Cancelled`. If the request finished
first, the result is kept and `cancel` returns `AlreadyFinished`. If the
worker does not confirm in time (cancellation is cooperative), `cancel`
returns `Timeout(Response)` and the worker stays occupied until the request
ends. A deadline alone never sends `cancel`, and a cancelled request is never
retried.

```rust
let request = worker.submit(data, Duration::from_secs(60)).await?;
let canceller = request.canceller();
tokio::spawn(async move { canceller.cancel().await });
match request.result().await {
    Err(RunpyError::Cancelled { .. }) => println!("stopped"),
    other => println!("{other:?}"),
}
```

### 7. Terminate and clean up

`Worker::terminate` starts a two-second shutdown grace. It immediately refuses
new sends to that worker, then tries to deliver `terminate` within the grace.
The worker is removed as soon as its process exits, and at the latest when the
grace ends, even if `terminate` is stuck behind a saturated queue or could not
be written. It returns the worker's `WorkerExit`. `Manager::terminate_all`
does the same for every worker under one shared deadline, delivering the
requests concurrently so one stuck worker cannot delay the others, and returns
each exit keyed by worker ID. `Manager::broadcast` likewise sends to all
workers concurrently.

Watchdog cleanup and session end use the same removal path. Removal happens
once: concurrent watchdog, session, termination, and shutdown attempts are
idempotent, and the first records the reason (`Exited`, `Terminated`,
`Disconnected`, `ProtocolViolation`, `StartupTimeout`, or `ManagerShutdown`).
A worker whose termination was requested is recorded as terminated even if it
exits or disconnects first.

The outcome is retained as a `WorkerExit` with the worker ID, the reason, and
the `uv` process's exit code or terminating signal. `Worker::wait()` returns
it to any number of waiters, including ones that start after the worker
stopped and after the Manager was dropped (Manager drop records
`ManagerShutdown`).

Removal aborts the protocol task, kills and reaps the process group on a
blocking thread outside the async runtime workers, removes the socket, then
permits each output reader up to 250 ms to drain final bytes. Termination is
therefore bounded by the grace plus kill-and-reap and the output drain; a
process stuck in an uninterruptible kernel state can still delay its reap.

Manager drop is synchronous: it closes the output dispatcher, aborts all tasks,
and immediately kills and reaps all registered groups on the dropping thread,
so it can block briefly and does not give workers a grace period. Final
buffered output is therefore best effort; call `terminate_all` first for a
graceful shutdown.

## Worker pools

`Manager::pool(template, PoolConfig)` creates a pool that runs independent
requests for one `WorkerBuilder` template (script, environment, arguments,
handler) across separate worker processes. Every call creates a distinct pool,
even for the same script and configuration. Pools use the ordinary correlated
request protocol; there is no second wire format.

```rust
use runpy::{Data, Manager, PoolConfig};
use serde_json::json;
use std::time::Duration;

let pool = manager
    .pool(manager.worker("parse"), PoolConfig::default().max_workers(2))
    .await?;
let first = pool.submit(Data::from_iter([("text".into(), json!("a"))]), Duration::from_secs(30)).await?;
let second = pool.submit(Data::from_iter([("text".into(), json!("b"))]), Duration::from_secs(30)).await?;
// Both run at once, on two processes; each result matches its own request.
let (first, second) = tokio::join!(first.result(), second.result());
```

Pools are cold and, by default, do not reuse workers:

- Creating a pool starts no process. Requests wait in a FIFO queue and each
  starts its own worker when a pool slot is free. A pool runs at most
  `max_workers` workers (default: the Manager's `max_workers`), and every
  pool and direct `spawn` share the Manager-wide worker cap.
- Without reuse, a worker runs exactly one request and is terminated after
  the request's terminal result. Its pool slot frees only once its process
  group has been stopped and reaped, so no state carries between requests
  and requests must not rely on affinity.
- `submit` waits within the request deadline for pool queue capacity
  (`queue_capacity`, default 64) and the Manager-wide `max_pending_requests`;
  `try_submit` fails with `QueueFull` instead of waiting. The pending budget
  covers queued requests in every pool. Futures an application creates
  outside Runpy while waiting for admission are not bounded by it.
- The deadline covers queueing, worker startup, and execution. A request that
  expires before reaching a worker fails with `Timeout(Queued)` and never
  runs; a failed worker start returns that start's error. A request that times
  out while running keeps its worker and pool slot busy until the worker
  answers. Requests are never replayed.
- `WorkerPool::close` rejects new requests, fails queued ones with
  `PoolClosed`, terminates running and idle workers within the shutdown grace
  (interrupted requests fail with `OutcomeUnknown`), and returns their exits.
  The Manager owns every pool: dropping all handles keeps serving submitted
  requests, and dropping the Manager stops pool tasks and kills their workers.

### Opt-in worker reuse

`PoolConfig::reuse_workers(true)` keeps a worker after its request and hands
it the next queued request before starting a new process. A worker is parked
for reuse only after `done` or a reported execution error; a worker that
exited, broke the protocol, or lost its connection is retired. A request whose
caller timed out still occupies its worker until it answers, so a worker never
runs two requests. An idle worker is retired after `idle_timeout` (default
30 s); taking it for new work cancels that timer, so a stale timer never stops
a busy worker.

Reuse keeps everything a process accumulated: imports, module globals,
caches, open files, and leaked memory. Nothing is reset between requests, and
any request may land on any worker. Use reuse to amortize startup, and keep
results independent of what a worker served before. A per-process cache is
fine when a miss gives the same answer:

```python
from functools import lru_cache

@lru_cache(maxsize=1024)
def load_model(name):  # warm after the first request on this process
    ...

class Parser(Worker):
    def execute(self, data):
        return {"tokens": load_model(data["model"]).parse(data["text"])}
```

### Warm minima and fair scheduling

`PoolConfig::min_workers(n)` keeps `n` warm workers. It requires reuse, and
`min_workers <= max_workers <=` the Manager's `max_workers`; the sum of all
open pools' minima must also fit the Manager's `max_workers`. Invalid
configurations are rejected before anything starts. Creating such a pool
takes `n` Manager-wide slots out of circulation (waiting up to the startup
timeout for running workers to free them), starts the minimum, and returns
only when all of them are ready; if any fails, the started ones are stopped
and the reservation is returned. Reserved slots are never used by direct
`spawn` calls or other pools, and minimum workers never idle out. `close`
returns the reservation.

A minimum worker that exits, or fails a request, gets exactly one replacement
start. If that start fails, the pool closes (queued requests fail with
`PoolClosed`) instead of restarting in a loop; call `close` to release its
reservation. Workers above the minimum are not replaced; new ones start only
for queued requests. Failed requests are never replayed: whether to resubmit
is the application's decision.

Workers above a pool's minimum obtain Manager-wide slots from a shared
scheduler. Requests stay FIFO within a pool, and when several pools wait for
capacity the scheduler hands each freed slot to the next waiting pool in
round-robin order, so a busy pool cannot starve another. While pools wait and
no slot is free, the scheduler retires an idle worker above another pool's
minimum. Running requests and reserved minima are never preempted, and
requests have no affinity to particular workers.

## Protocol reference

### Frame format

Each socket frame consists of:

1. an unsigned 64-bit little-endian payload length;
2. exactly that many UTF-8 JSON bytes.

Both peers bound every frame. The default maximum JSON body is 8 MiB
(`DEFAULT_MAX_FRAME_BYTES`); a zero-length body or a declared length above the
limit closes the connection before any body is allocated. Outbound envelopes
are serialized into a single buffer that stops growing at the limit, so an
oversized envelope is rejected without writing anything and the connection
stays usable. Once a frame's first byte arrives, the rest of the frame must
complete within the frame timeout (default 30 seconds,
`DEFAULT_FRAME_TIMEOUT`); an idle connection waiting for the first byte has no
deadline. A stalled frame closes the connection.

Configure both values with `ManagerBuilder::max_frame_bytes` and `frame_timeout`. The
Manager passes them to every worker as `RUNPY_MAX_FRAME_BYTES` and
`RUNPY_FRAME_TIMEOUT_MS` after applying user environment variables, so both
peers always enforce the same limits. Keep bulk data in files or external
storage and pass references in envelopes.

On the Manager, each connection is full duplex. A dedicated reader task owns
the read half and completes every frame before handing the decoded envelope to
the session, so a frame split across arbitrary socket reads is reassembled
exactly, even while the Manager is sending to the same worker. A separate writer
task owns the write half: a write blocked on a busy worker never stops inbound
dispatch, so large frames can flow in both directions at once. If either task
fails, the whole connection is closed; a partially written frame is never
retried.

The JSON value must contain exactly two fields, both objects:

```json
{
  "meta": {
    "x_op": "execute",
    "x_req": "17",
    "x_wid": "my_worker-3f2a9c1e0d7b4e6a8c5f1b2d3e4a5b6c",
    "x_spath": "/tmp/runpy-Xk3p9Q/w8f3c2a1b0e9d7c6a.sock",
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

### Strict JSON profile

Both peers apply the same JSON rules in both directions, because RFC 8259
leaves duplicate names undefined and lets implementations limit nesting:

- Any object, at any level, with a repeated key (after decoding escapes) is
  rejected. Distinct Unicode spellings, such as precomposed and combining
  forms, are different keys.
- Containers may nest at most 64 levels, counting the envelope itself (so
  `meta` and `data` are level 2). Deeper bodies are rejected by a linear,
  string-aware scan before any recursive parsing.
- NaN and infinite numbers are rejected.

An inbound violation closes the connection. An outbound envelope that is too
deep fails before anything is written: Rust returns
`InvalidEnvelope(EnvelopeError::TooDeep)`, Python raises `ValueError`, and the
connection stays usable.

### Reserved metadata

| Key | Meaning |
| --- | --- |
| `x_wid` | Manager-generated identity for the running worker. |
| `x_spath` | Manager-bound socket path for that worker. |
| `x_op` | Optional lifecycle operation; absence denotes a custom envelope. |
| `x_req` | Manager-generated request ID; only on `execute`, `cancel`, `done`, `error`, and `cancelled`. |

Reserved fields must be strings. Unknown `x_` fields and unknown operations
are protocol errors.

### Operations

| Operation | Direction | Behavior |
| --- | --- | --- |
| `ready` | Python → Rust | First envelope: `protocol_version` and `capabilities`. Consumed by Runpy, not dispatched. |
| `execute` | Rust → Python | Calls `Worker.execute(data)` for request `x_req`. |
| `cancel` | Rust → Python | Cancels request `x_req`; sent only to workers advertising `cancel`. |
| `terminate` | Rust → Python | Stops the Python receive loop and closes its socket. |
| `done` | Python → Rust | Carries the direct dictionary execution result. |
| `error` | Python → Rust | Carries `data.kind` (exception type) and `data.message` for an execution or dispatch failure. |
| `cancelled` | Python → Rust | Answers `cancel` for `x_req`: `{"status": "cancelled"}` ends the request; `{"status": "already_finished"}` only acknowledges it. |
| `log` | Python → Rust | Carries developer-selected structured log data. |

A known operation sent in the wrong direction is a protocol violation: the
Manager closes the connection and removes the worker.
A custom envelope has no `x_op` and is valid in either direction. Structured
`log` envelopes are distinct from process stdout/stderr: they use normal
message routing and have a reply-capable callback context.

## Worker process output

Runpy captures output inherited by `uv`, Python, and their descendants unless a
process redirects its descriptors, and delivers newline-free records with
trusted attribution to the `on_worker_output` handler. The host decides where
output goes:

```rust
use runpy::WorkerOutputStream;

manager.on_worker_output(|output| match output.stream {
    WorkerOutputStream::Stdout => println!("{}: {}", output.worker_id, output.line),
    WorkerOutputStream::Stderr => eprintln!("{}: {}", output.worker_id, output.line),
});
```

Worker output can contain anything a script or a library it imports prints,
including secrets, so Runpy does not copy it into the Manager's own log by
default. `ManagerBuilder::mirror_worker_output(true)` enables that copy:

```text
[worker:<worker-id>][stdout] <line>
[worker:<worker-id>][stderr] <line>
```

Mirrored stdout uses `info`; stderr uses `warning`. Both pipes are drained
whether or not output is mirrored or observed. Stderr is diagnostic, not proof
that a worker failed, and Runpy never terminates a worker based on captured
text.

The latest observer replaces the previous one. It runs on the dedicated output
dispatcher thread and must return promptly. A callback panic is caught and
logged so later records continue to dispatch. Applications can send policy
decisions to their own async channel rather than blocking the observer.

Capture is bounded and best effort:

- each worker stream may deliver `worker_output_rate` bytes per second
  (default 1 MiB/s, `None` for no limit), with up to one second of budget as
  burst; records beyond it are dropped before mirroring and before the
  handler, so a chatty worker cannot flood the host;
- a shared queue holds 256 records, and a full queue drops records instead
  of backpressuring the worker;
- the next delivered record reports either kind of prior loss in
  `dropped_lines_before` (mirrored as `[dropped N prior record(s)]`);
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
`--key=value` populate `Worker.extra`; other arguments are ignored.

`RunScript` returns normally (exit status 0) after `terminate` or when the
Manager closes the connection. Missing required arguments, an invalid class,
a construction or connection failure, a protocol violation, or a socket
failure writes one diagnostic to stderr and exits with status 1. The
diagnostic is ordinary worker output: it reaches the `on_worker_output`
handler (and the host log with `mirror_worker_output(true)`) on a best-effort
basis and is dropped like any other record when the stream is over its rate
budget or the output queue is full, so hosts must not rely on receiving it.
The exit status is always reported. A closed stderr does not change it.
`SystemExit` and `KeyboardInterrupt` from worker code propagate unchanged.

### Worker hooks

Override `execute(data)` for managed work and return a dictionary or `None`.
Override `handle_envelope(envelope)` for custom envelopes. Use `send(data,
meta=...)` for a custom response. Public builders copy the supplied objects and
reject application metadata in the `x_` namespace.

`run()` owns the receive loop. Malformed frames, invalid JSON, non-finite
numbers, invalid reserved metadata, and wrong-direction operations are protocol
violations: `run()` raises `ValueError` (a stalled frame raises
`TimeoutError`, a socket failure `OSError`). It returns normally after
`terminate`, a Manager close, or `close()` from another thread, and always
closes the connection first.

`send` and `log` may be called from any thread: a lock writes each frame
atomically. Results and messages are encoded before any byte is written, so a
non-finite number or oversized body raises `ValueError` and leaves the
connection usable; an `execute` result that cannot be encoded becomes an
`error` envelope. A failed socket write may leave a partial frame on the wire,
so it closes the connection and later sends raise `ConnectionError`. The
constructor closes its socket if connecting fails, and `run()` closes it if
sending `ready` fails. `close()`
is idempotent, and `Worker` is a context manager.

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

Diagnostics are best effort. Each line is written under the stderr lock, and a
closed or broken stderr is ignored instead of panicking. Runpy never logs valid
application envelopes. When a worker sends a body that is not valid JSON, the
diagnostic includes the parse error, the body length, and at most the first
1024 bytes, decoded lossily with control characters escaped. That excerpt can
still contain application data, so treat Runpy logs as potentially sensitive.

### Watchdog reports

The shared watchdog checks child exit state every five seconds and removes dead
worker process groups. Every second it also samples the resources of each
registered worker's process group on a blocking thread. `report()` returns the
latest snapshot for all registered workers; `report_worker(id)` returns one
report or `None` when the ID is absent.

| Field | Meaning |
| --- | --- |
| `worker_name`, `pid` | Trusted worker ID and the `uv` parent PID, which is also the process group ID. |
| `state` | `Running`, `Stopped` (every live member stopped, for example by `SIGSTOP`), `Exited` (from Runpy's own child handle, not PID existence), or `Unknown` (not yet sampled, or no member observable). |
| `memory_bytes` | Sum of resident memory of all live group members, in bytes. Shared pages are counted once per member, so the sum can exceed real usage. |
| `cpu_percent` | Sum of members' CPU over the last interval, in percent of one core (can exceed 100). `None` until the same members were sampled twice. |
| `process_count` | Live members found: `uv`, Python, and their descendants. |
| `sampled_at` | When the metrics were taken; reports are at most about a second old. |

Membership is the worker's process group (`getpgid`), so a descendant that
moves to its own group or session is not counted, and processes in other
groups are never included. Zombies and threads are skipped. Metrics that
cannot be read (sampling races, permissions) are reported as `None`, never as
zero. Sampling refreshes the process table once per second, which costs a few
milliseconds with typical process counts; it runs only while workers exist.
sysinfo opens each process's files only for the duration of a refresh, so
sampling holds no descriptor per host process between refreshes. If sysinfo
cannot initialize on a host, every report is `Unknown` with no metrics.

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
├── examples/performance/       # Real-uv performance runner
├── tests/protocol_vectors.json # Wire vectors shared by both suites
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

The hook runs only quick checks (formatting, Clippy, Ruff) and only for the
languages whose inputs are staged. It exports the index with
`git checkout-index` into a private snapshot under the Git directory and
checks that, so it validates exactly the commit: unstaged edits and untracked
files never affect the result, deletions and renames are seen, and unmerged
entries are rejected. It never stashes, resets, or writes to the working tree
or index. Build caches (`target/`, a uv environment under the Git directory)
are reused; source files are not. Behavior tests run in CI, not in the hook;
`tests/hooks` proves these guarantees against disposable clones.

Run Rust validation from the repository root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo rustdoc -p runpy --locked -- -D warnings -D missing-docs
```

`cargo test` includes `manager/tests/end_to_end.rs`, which runs real `uv` and
the Python SDK from this checkout: each test stages PEP 723 scripts whose only
dependency is the local `worker/` package by absolute path, and asserts that
`runpyrs` was imported from it. `uv` must be on `PATH`; the first run builds
the package into uv's cache. The playground is likewise a repository
development example: its script resolves `runpyrs` from `../../../worker`.
Applications install pinned Git revisions instead (see the
[installation guide](installation.md)).

Run Python validation from the repository root:

```bash
uv run --frozen ruff format --check worker/src/runpyrs worker/tests tests scripts examples/playground/worker/my_script.py
uv run --frozen ruff check worker/src/runpyrs worker/tests tests scripts examples/playground/worker/my_script.py
uv run --frozen mypy
uv run --frozen --all-packages pytest worker/tests tests/docs tests/hooks tests/release --doctest-modules worker/src/runpyrs
# Against the minimum Python without replacing .venv:
uv run --frozen --all-packages --isolated --python 3.10 pytest worker/tests
# Build both packages and use them from outside the checkout:
PACKAGE_SMOKE_ALLOW_DIRTY=1 scripts/package-smoke.sh
```

mypy runs in strict mode (configured in `pyproject.toml`) over the SDK and
the playground worker. `tests/docs` checks that local Markdown links and
heading anchors resolve and that Python examples compile.

### Continuous integration

`.github/workflows/quality.yml` holds every correctness gate and is called by
`ci.yml` on pull requests and pushes to `main`. No gate is path-filtered.
The required status check is **CI success**, which always runs and fails
unless all gates succeeded, so skipped or cancelled gates cannot pass.

| Gate | Platforms |
| --- | --- |
| Rust fmt, Clippy, tests (parallel), docs with warnings denied | Linux: Rust 1.95.0 and stable; macOS: stable |
| Real uv integration and playground | Linux and macOS × Python 3.10 and 3.14 |
| Ruff, mypy strict, pytest, doctests, Markdown, hook tests | Linux × Python 3.10–3.14 |
| `scripts/package-smoke.sh` | Linux |
| actionlint | Linux |
| cargo-deny licenses, bans, sources; suppression validity | Linux |

The declared minimum Rust is `rust-version = "1.95"`. The development shell
job (`dev-shell.yml`), the scheduled benchmarks, and the advisory audit are
separate and not required.

### Supply chain

Following GitHub's
[security hardening guide](https://docs.github.com/actions/security-guides/security-hardening-for-github-actions):

- Every third-party action is pinned to a full commit SHA resolved from the
  upstream tag, with the version in a trailing comment. Tools downloaded in
  CI (actionlint, cargo-deny) are checked against release SHA-256 sums.
- Workflows default to `contents: read`; only release creation gets
  `contents: write`. Pull requests run with `pull_request`, never
  `pull_request_target`, so untrusted code receives no secrets.
- Dependabot proposes weekly grouped updates for Cargo, uv (`pyproject.toml`
  with `uv.lock`), and the pinned actions.
- `deny.toml` allows only Apache-2.0, MIT, BSD-2/3-Clause, ISC, Unicode-3.0,
  and Apache-2.0 WITH LLVM-exception licenses (multi-licensed crates such as
  `r-efi` qualify through MIT), denies yanked crates, wildcard versions, and
  any registry or Git source other than crates.io. The required gate checks
  licenses, bans, and sources; `audit.yml` checks RustSec advisories daily and
  when a lockfile changes.
- `audit.yml` runs pip-audit on the hashed export of `uv.lock` (all locked
  development and build tooling; `runpyrs` itself has no runtime
  dependencies).

Suppressions live only in `deny.toml` (`[advisories] ignore`) and
`audit-suppressions.toml`. Each needs the advisory ID and a reason ending in
`(expires YYYY-MM-DD)`; `scripts/check-suppressions.py` fails on bare IDs,
missing reasons, and expired entries. Nothing is ignored by default.

To remediate a finding, update the affected package and its lock, then rerun
the audit:

```bash
cargo update -p <crate>            # Rust; may need related crates together
uv lock --upgrade-package <name>   # Python
cargo deny --locked check
uv export --frozen --all-packages --group dev --no-emit-workspace --format requirements-txt -o /tmp/req.txt
uv run --frozen pip-audit --disable-pip --require-hashes -r /tmp/req.txt
```

Raise the minimum in the manifest when an old version is vulnerable. Suppress
only when no fixed version exists and the code path is unreachable, with a
short expiry.

### Releases

Releases are source-only: a GitHub release points at a tag, carries no build
artifacts, and nothing is published to crates.io or PyPI, so no registry
tokens exist. Consumers install from the tag (see the
[installation guide](installation.md)).

| Tag | Kind | Manifest version |
| --- | --- | --- |
| `vX.Y.Z` | Release | `X.Y.Z` |
| `vX.Y.Z-dev`, `vX.Y.Z-dev.N` | Pre-release | `X.Y.Z` (the same base) |

The root `pyproject.toml`, `manager/Cargo.toml`, and `worker/pyproject.toml`
must all declare `X.Y.Z`; pre-releases share the base version by policy
instead of encoding `-dev` in the manifests. To release:

1. Set the three manifest versions to `X.Y.Z` (refresh `Cargo.lock` and
   `uv.lock`), record breaking changes in the [migration guide](migration.md),
   and merge to `main`.
2. Tag a commit on `main` and push the tag:

   ```bash
   git tag v0.3.0-dev.1 <commit-on-main>
   git push origin v0.3.0-dev.1
   ```

`release.yml` runs for every `v*` tag. Its `verify` job runs
`scripts/release-check.py`, which rejects malformed tags, any manifest that
does not match, and commits not reachable from `origin/main` (the tag need not
be the tip). The full quality gate then runs on the exact tag commit. Only if
both succeed does the `release` job, the only one with `contents: write`,
create the release with generated notes and a link to the migration guide at
that tag. Rerunning the workflow updates the existing release instead of
duplicating it. `tests/release` exercises the check against disposable
repositories and pins the job dependencies that keep a failed gate from
publishing.

`tests/protocol_vectors.json` holds wire vectors that both languages must
agree on: frame bodies (Unicode, reserved fields, direction, malformed JSON,
non-finite numbers, depth, duplicate keys) and complete byte streams
(malformed lengths, truncation). Each side checks the direction it receives.
Property tests (proptest in Rust, Hypothesis in Python, both with
deterministic seeds) generate bounded envelopes and arbitrary write
segmentation and require exact round trips. Resource tests re-run the test
binary in a child process, cycle 100 worker starts, requests, deaths, and
stops plus saturated pools, and require sockets, workers, slots, child
processes, and file descriptors to return to their baseline. These suites
reduce risk; they do not prove the protocol robust.

Exercise the complete system with:

```bash
cargo run -p playground
```

### Performance measurements

Two tools measure performance; neither is a functional test, and functional
tests never assert latency. Concurrency and bounds are checked with barriers
and counters instead.

```bash
# Criterion: transport through the public API and the local SDK, with a uv
# shim that runs python3 directly (no dependency resolution).
LOG=off cargo bench -p runpy --bench transport

# Real uv: startup and steady-state requests, reported as JSON.
LOG=off cargo run --release -p performance -- run --output report.json
cargo run --release -p performance -- compare \
  --baseline old-1.json old-2.json old-3.json \
  --current report-1.json report-2.json report-3.json
```

Criterion covers 1 KiB, 64 KiB, and 1 MiB request round trips on one reused
worker, a spawn-request-terminate cycle, and fixed 12-request batches on
reusable pools of widths 1 to 3. The `performance` runner stages a PEP 723
echo worker on this checkout's SDK and reports, separately, the first spawn
(which includes uv resolving the script environment), later cold spawns,
one-shot requests, reused-worker latency and throughput per payload size, and
pool batches at widths 1 to 3: median, p95, and p99 latency, requests and MiB
per second, plus the runner's peak RSS (`VmHWM` on Linux) and peak open
descriptors. Payloads come from a fixed seed. `compare` takes each metric's
median across runs and flags medians more than 20% slower; it never fails.

Limits: measurements are only comparable on the same machine class,
toolchain, Python, and uv version; the first spawn depends on uv's cache
state; worker RSS is not included. `ENVIRONMENT=development` forces verbose
logging and distorts results: the runner clears it and defaults `LOG=off`,
but Criterion runs need `ENVIRONMENT=` set by hand. The scheduled **Benchmarks** workflow runs the
runner three times and the Criterion suite, compares against the latest
successful run on `main`, and keeps reports as workflow artifacts for 90 days.
They are never attached to releases.
