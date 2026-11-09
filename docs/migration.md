# Migration guide

This guide lists breaking changes to the Rust `runpy` crate and the Python
`runpyrs` SDK, with the replacement for each removed or changed API. Rust and
Python must be upgraded together because the wire protocol changes too.

## Rust: construction and worker handles

| Before | After |
| --- | --- |
| `Manager::new(dir)` | `Manager::builder(dir).build().await?` |
| `Manager::with_uv_path(dir, uv)` | `Manager::builder(dir).uv_path(uv).build().await?` |
| `Manager::with_limits(dir, uv, ProtocolLimits::new(..))` | `.max_frame_bytes(..)` and `.frame_timeout(..)` on the builder |
| Construction logs integrity failures | `build` returns `UvUnavailable` / `ScriptsDirectory` |
| `manager.check_integrity()` | `manager.check_integrity().await` |
| `let mut worker = manager.worker("x"); worker.env(..);` | `let builder = manager.worker("x").env(..);` (setters consume and return) |
| `worker.spawn().await -> Result<String, String>` | `builder.spawn().await -> Result<Worker, RunpyError>`; read the ID with `worker.id()` |
| `on_message` / `on_worker_output` / `terminate_all` on `&mut Manager` | on `&Manager` |

```rust
// Before
let mut manager = Manager::new("worker");
let mut worker = manager.worker("my_worker");
worker.env("MODE", "fast");
let worker_id = worker.spawn().await?;

// After
let manager = Manager::builder("worker").build().await?;
let worker = manager.worker("my_worker").env("MODE", "fast").spawn().await?;
let worker_id = worker.id();
```

New builder options: `cwd(dir)` and `stdin(StdinMode::Inherit)`. Worker stdin
is now the null device by default instead of the Manager's stdin.

## Rust: sending

- Errors are the typed `RunpyError` instead of `String`; branch on variants
  such as `QueueFull`, `Timeout(stage)`, `WorkerExited`, or `ManagerClosed`.
- `send_message` and `reply_async` now resolve after the frame is completely
  written, bounded by the send deadline.
- `InboundEnvelope::reply` returns `Result<SendReceipt, RunpyError>` and fails
  with `QueueFull` instead of spawning a background send. Ignore the receipt
  for fire-and-forget replies or `.wait().await` it for the write outcome.
- `Worker::try_send_message` admits without waiting.
- `InboundEnvelope::envelope` is `Arc<Envelope>`; method calls are unchanged,
  and `Envelope::clone(&inbound.envelope)` copies it when needed.

## Rust: lifecycle

- A worker's socket accepts exactly one connection. A closed or invalid
  connection removes the worker and stops its process group; reconnecting is
  no longer supported.

## Python SDK

- Error envelopes carry `{"kind": "<exception type>", "message": "<text>"}`
  instead of a prefixed `message` only.
- `Worker.run()` raises protocol (`ValueError`) and socket (`OSError`)
  failures instead of printing and returning. `RunScript` reports them on
  stderr and exits with status 1.
- Frames above `RUNPY_MAX_FRAME_BYTES` raise `FrameTooLargeError`; NaN and
  infinite numbers raise `ValueError` in both directions.
