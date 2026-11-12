# Migration guide

This guide lists the breaking changes in Runpy 0.3.0 (since `v0.2.0-dev`) to
the Rust `runpy` crate and the Python `runpyrs` SDK, with the replacement for
each removed or changed API. Rust and Python must be upgraded together because
the wire protocol changes too.

## Rust toolchain

`runpy` requires Rust 1.95 or newer (`rust-version = "1.95"`, previously
1.93), because its `sysinfo` 0.40 and `parking_lot` 0.13 dependencies do.
Run `rustup update stable`, or pin `1.95.0` in your toolchain file.

## Rust: worker output

Changed after `v0.3.0-dev`:

- Worker stdout and stderr are no longer copied into the Manager's log by
  default; they reach only the `on_worker_output` handler. To keep the old
  `[worker:<id>][<stream>] <line>` log lines, build with
  `.mirror_worker_output(true)`, or log from your own handler.
- Each worker stream delivers at most 1 MiB/s by default; records beyond that
  are dropped and counted in the next record's `dropped_lines_before`. Use
  `.worker_output_rate(Some(bytes_per_second))` to change the limit or
  `.worker_output_rate(None)` to remove it.

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

- At most `max_workers` (default 3) workers exist at once across all
  scripts. Additional `spawn` calls wait for a slot within the startup timeout
  and then fail with `Timeout(Admission)`; raise the limit with
  `ManagerBuilder::max_workers` if you ran more workers before.
- A worker's socket accepts exactly one connection. A closed or invalid
  connection removes the worker and stops its process group; reconnecting is
  no longer supported.
- `spawn` returns only after the worker's versioned `ready`. `ready` is no
  longer passed to `on_message` handlers.
- `Envelope::execute` and `Envelope::retry` are removed. Run work with
  `Worker::execute(data, deadline)` or `Worker::submit` and read the
  correlated result instead of matching `done` in a handler:

  ```rust
  // Before
  manager.on_message(|inbound| {
      if inbound.envelope.meta().get("x_op") == Some(&"ready".into()) {
          let _ = inbound.reply(Envelope::execute(data));
      }
  });
  // After
  let worker = manager.worker("my_worker").spawn().await?;
  let output = worker.execute(data, Duration::from_secs(30)).await?;
  ```
- There is no implicit retry: requests are never replayed. Resubmit a new
  request explicitly after `OutcomeUnknown` when the work is idempotent.
- `Worker::terminate` returns `Result<WorkerExit, RunpyError>` (whether the
  `terminate` frame was written is no longer reported) and
  `Manager::terminate_all` returns each `WorkerExit`. `Worker::wait` observes
  the retained exit.
- `spawn` fails with `Timeout(Startup)` when `ready` does not arrive within
  `startup_timeout`.

- `ProcessState` is now `Running`, `Stopped`, `Exited`, or `Unknown` (the
  former `Healthy`, `Frozen`, and `Dead` are removed). `WorkerReport` replaces
  `memory_kb` with `memory_bytes` for the whole process group and adds
  sampled `cpu_percent`, `process_count`, and `sampled_at`.

## Python SDK

- Error envelopes carry `{"kind": "<exception type>", "message": "<text>"}`
  instead of a prefixed `message` only.
- `Worker.run()` raises protocol (`ValueError`) and socket (`OSError`)
  failures instead of printing and returning. `RunScript` reports them on
  stderr and exits with status 1.
- Frames above `RUNPY_MAX_FRAME_BYTES` raise `FrameTooLargeError`; NaN and
  infinite numbers raise `ValueError` in both directions.
- `ready` is sent by `Worker.run()` instead of the constructor and carries
  `{"protocol_version": 1, "capabilities": [...]}`. The Rust Manager rejects
  any other version: upgrade `runpy` and `runpyrs` together.
- `retry` is no longer an operation and `Worker` no longer stores the last
  payload. `execute` envelopes carry `x_req`, and `done`/`error` echo it.
  `ready` advertises the `requests` capability.
- The protocol adds `cancel` (Manager to worker) and `cancelled` (worker to
  Manager), both carrying `x_req`. `RunpyOperation` includes them. The sync
  `Worker` treats `cancel` as a protocol violation; the Manager never sends it
  to workers that do not advertise the `cancel` capability.
- `RunpyError` gains `Cancelled { request_id }`; exhaustive matches must
  handle it.
- Both peers enforce a strict JSON profile: duplicate object keys at any level
  and nesting deeper than 64 containers (counting the envelope) are rejected
  in both directions. `EnvelopeError` gains `TooDeep`.
